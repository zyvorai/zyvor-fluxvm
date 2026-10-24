import { useEffect, useMemo, useState } from 'react';
import { ScrollText, SquareTerminal } from 'lucide-react';
import { PodWindow, type PodTab } from './PodWindow';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import { type KubeClusterItem, type KubeDetail, type KubePod, ageOf, filterPods, kubeProblem, namespacesOf, podTone, summarize } from '../lib/kube';

const enc = encodeURIComponent;

export function KubernetesPage() {
  const clusters = usePoll(async () => (await api.get<{ items: KubeClusterItem[] }>('/v1/kube')).items ?? [], 8000);
  const items = clusters.data ?? [];
  const [picked, setPicked] = useState('');
  const current = items.find((c) => c.name === picked)?.name ?? items[0]?.name ?? '';
  const detail = usePoll(async () => (current ? await api.get<KubeDetail>(`/v1/kube/${enc(current)}`) : null), 5000, [current]);
  const [ns, setNs] = useState('');
  const [q, setQ] = useState('');
  const [now, setNow] = useState(Date.now());
  const [open, setOpen] = useState<{ pod: KubePod; tab: PodTab } | null>(null);
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
      <Hero eyebrow="Kubernetes" title="Clusters on this host," accent="pods included." lede="k3s clusters made with fluxctl kube. Read-only: this page shows what is running; use kubectl to change it." />
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
          {problem ? (
            <Card><EmptyState title={problem.title} body={problem.body} /></Card>
          ) : !d ? (
            <Skeleton h={160} />
          ) : (
            <>
              <Card title="Nodes">
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Node</th><th>Status</th><th>Roles</th><th>Version</th><th>Address</th><th>CPU</th><th>Memory</th><th>Age</th></tr></thead><tbody>
                  {d.nodes.map((n) => (
                    <tr key={n.name}>
                      <td className="vm-name">{n.name}</td>
                      <td><Pill tone={n.ready ? 'ok' : 'bad'}>{n.ready ? 'Ready' : 'NotReady'}</Pill></td>
                      <td className="fx-muted">{n.roles.join(', ')}</td>
                      <td className="fx-muted fx-mono">{n.version ?? '—'}</td>
                      <td className="fx-mono">{n.internal_ip ?? '—'}</td>
                      <td className="fx-muted">{n.cpu ?? '—'}</td>
                      <td className="fx-muted">{n.memory_mib != null ? `${(n.memory_mib / 1024).toFixed(1)} GiB` : '—'}</td>
                      <td className="fx-muted">{ageOf(n.created, now)}</td>
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
                  <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Pod</th><th>Ready</th><th>Status</th><th>Restarts</th><th>Node</th><th>Pod IP</th><th>Age</th><th /></tr></thead><tbody>
                    {pods.map((p) => (
                      <tr key={`${p.namespace}/${p.name}`}>
                        <td className="fx-muted">{p.namespace}</td>
                        <td className="vm-name"><button className="fx-link" onClick={() => setOpen({ pod: p, tab: 'logs' })} title="Open the logs">{p.name}</button></td>
                        <td className="fx-mono">{p.ready}</td>
                        <td><Pill tone={podTone(p)}>{p.status}</Pill></td>
                        <td className="fx-muted">{p.restarts}</td>
                        <td className="fx-muted">{p.node ?? '—'}</td>
                        <td className="fx-mono">{p.pod_ip ?? '—'}</td>
                        <td className="fx-muted">{ageOf(p.created, now)}</td>
                        <td><span className="fx-row-actions">
                          <Button size="sm" variant="ghost" onClick={() => setOpen({ pod: p, tab: 'logs' })}><ScrollText size={14} />Logs</Button>
                          <Button size="sm" variant="ghost" onClick={() => setOpen({ pod: p, tab: 'shell' })} disabled={p.phase !== 'Running'}><SquareTerminal size={14} />Shell</Button>
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
      {open ? <PodWindow cluster={current} pod={open.pod} tab={open.tab} onClose={() => setOpen(null)} /> : null}
    </>
  );
}
