import { useMemo, useState } from 'react';
import { Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { type KubeService, type Replicated, type Workloads, ageOf, inNamespace, kubeProblem, namespacesOfAll, portText, readyTone, shortImage } from '../lib/kube';

const enc = encodeURIComponent;

/** What runs in a cluster: Deployments, StatefulSets, DaemonSets and the Services that expose them. */
export function KubeWorkloads({ cluster }: { cluster: string }) {
  const poll = usePoll(async () => await api.get<Workloads>(`/v1/kube/${enc(cluster)}/workloads`), 8000, [cluster]);
  const [ns, setNs] = useState('');
  const d = poll.data;
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
      <ReplicatedTable title="Deployments" rows={pick(d.deployments)} now={now} />
      <ReplicatedTable title="StatefulSets" rows={pick(d.statefulsets)} now={now} />
      <ReplicatedTable title="DaemonSets" rows={pick(d.daemonsets)} now={now} />
      <ServicesTable rows={pick(d.services)} now={now} />
    </>
  );
}

function ReplicatedTable({ title, rows, now }: { title: string; rows: Replicated[]; now: number }) {
  if (!rows.length) return null;
  return (
    <Card title={title}>
      <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Name</th><th>Ready</th><th>Up to date</th><th>Images</th><th>Age</th></tr></thead><tbody>
        {rows.map((w) => (
          <tr key={`${w.namespace}/${w.name}`}>
            <td className="fx-muted">{w.namespace}</td>
            <td className="vm-name">{w.name}</td>
            <td><Pill tone={readyTone(w)}>{w.ready}/{w.desired}</Pill></td>
            <td className="fx-muted">{w.updated}</td>
            <td className="fx-mono fx-muted">{w.images.map(shortImage).join(', ') || '—'}</td>
            <td className="fx-muted">{ageOf(w.created, now)}</td>
          </tr>
        ))}
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
