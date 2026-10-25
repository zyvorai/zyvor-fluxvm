import { useMemo, useState } from 'react';
import { Minus, Plus, RotateCw } from 'lucide-react';
import type { Notify } from '../App';
import { Button, Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { kubeAct } from '../lib/kubeAct';
import { type KubeService, type Replicated, type WorkloadKind, type Workloads, actionPath, ageOf, canScale, clampReplicas, inNamespace, kubeProblem, namespacesOfAll, portText, readyTone, shortImage } from '../lib/kube';

const enc = encodeURIComponent;

/** What runs in a cluster: Deployments, StatefulSets, DaemonSets and the Services that expose them. */
export function KubeWorkloads({ cluster, notify }: { cluster: string; notify: Notify }) {
  const poll = usePoll(async () => await api.get<Workloads>(`/v1/kube/${enc(cluster)}/workloads`), 8000, [cluster]);
  const [ns, setNs] = useState('');
  const [busy, setBusy] = useState('');
  const d = poll.data;
  const act = async (w: Replicated, kind: WorkloadKind, action: 'scale' | 'restart', replicas?: number) => {
    setBusy(`${w.namespace}/${w.name}${action}`);
    const path = actionPath(cluster, { workload: { ns: w.namespace, kind, name: w.name }, action });
    if (await kubeAct(path, w.name, action === 'scale' ? 'scaled' : 'restarted', notify, replicas != null ? { replicas } : undefined, replicas)) await poll.refresh();
    setBusy('');
  };
  const now = Date.now();
  const namespaces = useMemo(() => (d ? namespacesOfAll(d) : []), [d]);
  if (poll.error && !d) {
    const p = kubeProblem(poll.error.message);
    return <Card><EmptyState title={p.title} body={p.body} /></Card>;
  }
  if (!d) return <Skeleton h={200} />;
  const pick = <T extends { namespace: string }>(l: T[]) => inNamespace(l, ns);
  return (
    <>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginBottom: 12 }}>
        <select className="fx-input" aria-label="Namespace" value={ns} onChange={(e) => setNs(e.target.value)} style={{ maxWidth: 240 }}>
          <option value="">All namespaces</option>
          {namespaces.map((x) => <option key={x} value={x}>{x}</option>)}
        </select>
      </div>
      <ReplicatedTable title="Deployments" kind="deployments" rows={pick(d.deployments)} now={now} busy={busy} act={act} />
      <ReplicatedTable title="StatefulSets" kind="statefulsets" rows={pick(d.statefulsets)} now={now} busy={busy} act={act} />
      <ReplicatedTable title="DaemonSets" kind="daemonsets" rows={pick(d.daemonsets)} now={now} busy={busy} act={act} />
      <ServicesTable rows={pick(d.services)} now={now} />
    </>
  );
}

type Act = (w: Replicated, kind: WorkloadKind, action: 'scale' | 'restart', replicas?: number) => Promise<void>;

function ReplicatedTable({ title, kind, rows, now, busy, act }: { title: string; kind: WorkloadKind; rows: Replicated[]; now: number; busy: string; act: Act }) {
  if (!rows.length) return null;
  return (
    <Card title={title}>
      <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Name</th><th>Ready</th><th>Up to date</th><th>Images</th><th>Age</th><th /></tr></thead><tbody>
        {rows.map((w) => {
          const id = `${w.namespace}/${w.name}`;
          return (
          <tr key={`${w.namespace}/${w.name}`}>
            <td className="fx-muted">{w.namespace}</td>
            <td className="vm-name">{w.name}</td>
            <td><Pill tone={readyTone(w)}>{w.ready}/{w.desired}</Pill></td>
            <td className="fx-muted">{w.updated}</td>
            <td className="fx-mono fx-muted">{w.images.map(shortImage).join(', ') || '—'}</td>
            <td className="fx-muted">{ageOf(w.created, now)}</td>
            <td><span className="fx-row-actions">
              {canScale(kind) ? (
                <>
                  <Button size="sm" variant="ghost" aria-label={`Fewer replicas of ${w.name}`} disabled={!!busy || w.desired <= 0} busy={busy === id + 'scale'} onClick={() => void act(w, kind, 'scale', clampReplicas(w.desired - 1))}><Minus size={14} /></Button>
                  <Button size="sm" variant="ghost" aria-label={`More replicas of ${w.name}`} disabled={!!busy} onClick={() => void act(w, kind, 'scale', clampReplicas(w.desired + 1))}><Plus size={14} /></Button>
                </>
              ) : null}
              <Button size="sm" variant="ghost" aria-label={`Restart ${w.name}`} disabled={!!busy} busy={busy === id + 'restart'} onClick={() => void act(w, kind, 'restart')}><RotateCw size={14} />Restart</Button>
            </span></td>
          </tr>
          );
        })}
      </tbody></table></div>
    </Card>
  );
}

function ServicesTable({ rows, now }: { rows: KubeService[]; now: number }) {
  if (!rows.length) return null;
  return (
    <Card title="Services">
      <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Name</th><th>Type</th><th>Cluster IP</th><th>Ports</th><th>Selects</th><th>Age</th></tr></thead><tbody>
        {rows.map((s) => (
          <tr key={`${s.namespace}/${s.name}`}>
            <td className="fx-muted">{s.namespace}</td>
            <td className="vm-name">{s.name}</td>
            <td><Pill tone={s.type === 'ClusterIP' ? 'idle' : 'info'}>{s.type}</Pill></td>
            <td className="fx-mono">{s.cluster_ip ?? '—'}</td>
            <td className="fx-mono">{s.ports.map(portText).join(', ') || '—'}{s.external.length ? <span className="fx-muted"> · external {s.external.join(', ')}</span> : null}</td>
            <td className="fx-mono fx-muted">{s.selector.join(', ') || '—'}</td>
            <td className="fx-muted">{ageOf(s.created, now)}</td>
          </tr>
        ))}
      </tbody></table></div>
    </Card>
  );
}
