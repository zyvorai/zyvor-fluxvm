import { useEffect, useMemo, useState } from 'react';
import { Ban, CircleCheck, ScrollText, SquareTerminal, Trash2 } from 'lucide-react';
import type { Notify } from '../App';
import { kubeAct } from '../lib/kubeAct';
import { KubeEvents } from './KubeEvents';
import { KubeGraph } from './KubeGraph';
import { KubeWorkloads } from './KubeWorkloads';
import { PodWindow, type PodTab } from './PodWindow';
import { Button, Card, EmptyState, Hero, Meter, Pill, Sheet, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import { type KubeClusterItem, type KubeDetail, type KubePod, actionPath, ageOf, filterPods, fmtCpu, fmtMem, kubeProblem, namespacesOf, podTone, summarize } from '../lib/kube';

const enc = encodeURIComponent;

export function KubernetesPage({ notify }: { notify: Notify }) {
  const clusters = usePoll(async () => (await api.get<{ items: KubeClusterItem[] }>('/v1/kube')).items ?? [], 8000);
  const items = clusters.data ?? [];
  const [picked, setPicked] = useState('');
  const current = items.find((c) => c.name === picked)?.name ?? items[0]?.name ?? '';
  const detail = usePoll(async () => (current ? await api.get<KubeDetail>(`/v1/kube/${enc(current)}`) : null), 5000, [current]);
  const [view, setView] = useState<'overview' | 'workloads' | 'events' | 'traffic'>('overview');
  const [ns, setNs] = useState('');
  const [q, setQ] = useState('');
  const [now, setNow] = useState(Date.now());
  const [open, setOpen] = useState<{ pod: KubePod; tab: PodTab } | null>(null);
  const [confirm, setConfirm] = useState<KubePod | null>(null);
  const [busy, setBusy] = useState('');
  const act = async (key: string, path: string, subject: string, verb: 'deleted' | 'cordoned' | 'uncordoned') => {
    setBusy(key);
    if (await kubeAct(path, subject, verb, notify)) await detail.refresh();
    setBusy('');
    setConfirm(null);
  };
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 5000);
    return () => clearInterval(t);
  }, []);

  const d = detail.data;
  const pods = useMemo(() => filterPods(d?.pods ?? [], ns, q), [d, ns, q]);
  const sum = useMemo(() => summarize(d?.pods ?? []), [d]);
  const problem = detail.error && !d ? kubeProblem(detail.error.message) : null;

  return (
    <>
      <Hero eyebrow="Kubernetes" title="Clusters on this host," accent="pods included." lede="k3s clusters made with fluxctl kube. Scale, restart, cordon and delete from here; use kubectl for the rest." />
      {clusters.loading && !items.length ? (
        <Skeleton h={120} />
      ) : items.length === 0 ? (
        <Card><EmptyState title="No Kubernetes clusters" body="Create one with fluxctl kube up. Its machines appear here and under Machines." /></Card>
      ) : (
        <>
          <Tabs
            value={current}
            onChange={(v) => (setPicked(v), setNs(''), setQ(''))}
            tabs={items.map((c) => ({ value: c.name, label: `${c.name} · ${c.nodes.filter((n) => n.status === 'running').length}/${c.nodes.length} machines up` }))}
          />
          <Tabs value={view} onChange={setView} tabs={[{ value: 'overview', label: 'Nodes and pods' }, { value: 'workloads', label: 'Workloads' }, { value: 'events', label: 'Events' }, { value: 'traffic', label: 'Traffic' }]} />
          {problem ? (
            <Card><EmptyState title={problem.title} body={problem.body} /></Card>
          ) : view === 'traffic' ? (
            <KubeGraph cluster={current} />
          ) : view === 'workloads' ? (
            <KubeWorkloads cluster={current} notify={notify} />
          ) : view === 'events' ? (
            <KubeEvents cluster={current} />
          ) : !d ? (
            <Skeleton h={160} />
          ) : (
            <>
              <Card title="Nodes">
                {d.metrics === false ? <p className="fx-muted">No CPU or memory numbers: the cluster's metrics-server is not answering (it may still be starting).</p> : null}
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Node</th><th>Status</th><th>Roles</th><th>Version</th><th>Address</th><th>CPU</th><th>Memory</th><th>Age</th><th /></tr></thead><tbody>
                  {d.nodes.map((n) => (
                    <tr key={n.name}>
                      <td className="vm-name">{n.name}</td>
                      <td><Pill tone={n.ready ? 'ok' : 'bad'}>{n.ready ? 'Ready' : 'NotReady'}</Pill>{n.unschedulable ? <> <Pill tone="warn">Cordoned</Pill></> : null}</td>
                      <td className="fx-muted">{n.roles.join(', ')}</td>
                      <td className="fx-muted fx-mono">{n.version ?? '—'}</td>
                      <td className="fx-mono">{n.internal_ip ?? '—'}</td>
                      <td>{n.cpu_percent != null ? <span className="kd-use"><Meter value={n.cpu_percent} label={`CPU of ${n.name}`} /><span className="fx-mono">{n.cpu_percent.toFixed(0)}%</span><span className="fx-muted"> of {n.cpu} cores</span></span> : <span className="fx-muted">{n.cpu ? `${n.cpu} cores` : '—'}</span>}</td>
                      <td>{n.mem_percent != null ? <span className="kd-use"><Meter value={n.mem_percent} label={`Memory of ${n.name}`} /><span className="fx-mono">{n.mem_percent.toFixed(0)}%</span><span className="fx-muted"> of {fmtMem(n.memory_mib ?? undefined)}</span></span> : <span className="fx-muted">{n.memory_mib != null ? fmtMem(n.memory_mib) : '—'}</span>}</td>
                      <td className="fx-muted">{ageOf(n.created, now)}</td>
                      <td><span className="fx-row-actions">
                        <Button size="sm" variant="ghost" aria-label={`${n.unschedulable ? 'Uncordon' : 'Cordon'} ${n.name}`} disabled={!!busy} busy={busy === `node/${n.name}`} onClick={() => void act(`node/${n.name}`, actionPath(current, { node: n.name, action: n.unschedulable ? 'uncordon' : 'cordon' }), n.name, n.unschedulable ? 'uncordoned' : 'cordoned')}>
                          {n.unschedulable ? <CircleCheck size={14} /> : <Ban size={14} />}{n.unschedulable ? 'Uncordon' : 'Cordon'}
                        </Button>
                      </span></td>
                    </tr>
                  ))}
                </tbody></table></div>
              </Card>
              <Card title="Pods">
                <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', marginBottom: 12 }}>
                  <Pill tone="ok">{sum.healthy} running</Pill>
                  {sum.waiting ? <Pill tone="warn">{sum.waiting} starting</Pill> : null}
                  {sum.failing ? <Pill tone="bad">{sum.failing} failing</Pill> : null}
                  {sum.done ? <Pill tone="idle">{sum.done} finished</Pill> : null}
                  <select className="fx-input" aria-label="Namespace" value={ns} onChange={(e) => setNs(e.target.value)} style={{ maxWidth: 220 }}>
                    <option value="">All namespaces</option>
                    {namespacesOf(d.pods).map((x) => <option key={x} value={x}>{x}</option>)}
                  </select>
                  <input className="fx-input" aria-label="Filter pods" placeholder="Filter by name, node, address or status" value={q} onChange={(e) => setQ(e.target.value)} style={{ maxWidth: 320 }} />
                </div>
                {pods.length === 0 ? (
                  <p className="fx-muted">{d.pods.length ? 'No pod matches this filter.' : 'This cluster has no pods yet.'}</p>
                ) : (
                  <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Pod</th><th>Ready</th><th>Status</th><th>Restarts</th><th>CPU</th><th>Memory</th><th>Node</th><th>Pod IP</th><th>Age</th><th /></tr></thead><tbody>
                    {pods.map((p) => (
                      <tr key={`${p.namespace}/${p.name}`}>
                        <td className="fx-muted">{p.namespace}</td>
                        <td className="vm-name"><button className="fx-link" onClick={() => setOpen({ pod: p, tab: 'logs' })} title="Open the logs">{p.name}</button></td>
                        <td className="fx-mono">{p.ready}</td>
                        <td><Pill tone={podTone(p)}>{p.status}</Pill></td>
                        <td className="fx-muted">{p.restarts}</td>
                        <td className="fx-mono fx-muted">{fmtCpu(p.cpu_m)}</td>
                        <td className="fx-mono fx-muted">{fmtMem(p.mem_mib)}</td>
                        <td className="fx-muted">{p.node ?? '—'}</td>
                        <td className="fx-mono">{p.pod_ip ?? '—'}</td>
                        <td className="fx-muted">{ageOf(p.created, now)}</td>
                        <td><span className="fx-row-actions">
                          <Button size="sm" variant="ghost" onClick={() => setOpen({ pod: p, tab: 'logs' })}><ScrollText size={14} />Logs</Button>
                          <Button size="sm" variant="ghost" onClick={() => setOpen({ pod: p, tab: 'shell' })} disabled={p.phase !== 'Running'}><SquareTerminal size={14} />Shell</Button>
                          <Button size="sm" variant="ghost" aria-label={`Delete ${p.name}`} disabled={!!busy} onClick={() => setConfirm(p)}><Trash2 size={14} />Delete</Button>
                        </span></td>
                      </tr>
                    ))}
                  </tbody></table></div>
                )}
                {d.truncated ? <p className="fx-muted">Showing the first 500 pods.</p> : null}
              </Card>
            </>
          )}
        </>
      )}
      <Sheet open={!!confirm} title="Delete pod" onClose={() => setConfirm(null)}>
        <p>Delete <b>{confirm?.name}</b>? Its controller, if it has one, starts a new pod; a pod made by hand is gone for good.</p>
        <div style={{ display: 'flex', gap: 8, justifyContent: 'flex-end', marginTop: 16 }}>
          <Button variant="ghost" onClick={() => setConfirm(null)}>Cancel</Button>
          <Button variant="danger" busy={!!confirm && busy === `pod/${confirm.namespace}/${confirm.name}`} onClick={() => confirm && void act(`pod/${confirm.namespace}/${confirm.name}`, actionPath(current, { pod: { ns: confirm.namespace, name: confirm.name } }), confirm.name, 'deleted')}>Delete</Button>
        </div>
      </Sheet>
      {open ? <PodWindow cluster={current} pod={open.pod} tab={open.tab} onClose={() => setOpen(null)} /> : null}
    </>
  );
}
