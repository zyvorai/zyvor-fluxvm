import { useMemo, useState } from 'react';
import { Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { type EventFilter, type KubeEvent, ageOf, eventTone, filterEvents, kubeProblem, namespacesOfEvents } from '../lib/kube';

const enc = encodeURIComponent;

type Events = { cluster: string; events: KubeEvent[]; warnings: number; truncated: boolean };

/** What has happened in a cluster, newest first: the quickest way to see why a pod is not starting. */
export function KubeEvents({ cluster }: { cluster: string }) {
  const poll = usePoll(async () => await api.get<Events>(`/v1/kube/${enc(cluster)}/events`), 8000, [cluster]);
  const [filter, setFilter] = useState<EventFilter>({ warningsOnly: false, namespace: '', text: '' });
  const d = poll.data;
  const shown = useMemo(() => (d ? filterEvents(d.events, filter) : []), [d, filter]);
  if (poll.error && !d) {
    const p = kubeProblem(poll.error.message);
    return <Card><EmptyState title={p.title} body={p.body} /></Card>;
  }
  if (!d) return <Skeleton h={200} />;
  const now = Date.now();
  return (
    <Card title="Events">
      <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap', marginBottom: 12 }}>
        {d.warnings ? <Pill tone="warn">{d.warnings} warning{d.warnings === 1 ? '' : 's'}</Pill> : <Pill tone="ok">no warnings</Pill>}
        <label style={{ display: 'inline-flex', gap: 6, alignItems: 'center', fontSize: 13 }}><input type="checkbox" checked={filter.warningsOnly} onChange={(e) => setFilter({ ...filter, warningsOnly: e.target.checked })} /> warnings only</label>
        <select className="fx-input" aria-label="Namespace" value={filter.namespace} onChange={(e) => setFilter({ ...filter, namespace: e.target.value })} style={{ maxWidth: 220 }}>
          <option value="">All namespaces</option>
          {namespacesOfEvents(d.events).map((x) => <option key={x} value={x}>{x}</option>)}
        </select>
        <input className="fx-input" aria-label="Filter events" placeholder="Filter by reason, object or message" value={filter.text} onChange={(e) => setFilter({ ...filter, text: e.target.value })} style={{ maxWidth: 320 }} />
      </div>
      {shown.length === 0 ? (
        <p className="fx-muted">{d.events.length ? 'No event matches this filter.' : 'The cluster has recorded no events (Kubernetes keeps them for about an hour).'}</p>
      ) : (
        <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Type</th><th>Reason</th><th>Object</th><th>Message</th><th>Count</th><th>Last</th><th>From</th></tr></thead><tbody>
          {shown.map((e, i) => (
            <tr key={`${e.object}/${e.reason}/${e.last}/${i}`}>
              <td><Pill tone={eventTone(e.type)}>{e.type}</Pill></td>
              <td className="vm-name">{e.reason ?? '—'}</td>
              <td className="fx-mono">{e.namespace ? <span className="fx-muted">{e.namespace}/</span> : null}{e.object}</td>
              <td style={{ whiteSpace: 'normal', minWidth: 260 }}>{e.message ?? '—'}</td>
              <td className="fx-muted">{e.count}</td>
              <td className="fx-muted">{ageOf(e.last, now)}</td>
              <td className="fx-muted">{e.component ?? '—'}</td>
            </tr>
          ))}
        </tbody></table></div>
      )}
      {d.truncated ? <p className="fx-muted">Showing the newest events of the first 500 the cluster returned.</p> : null}
    </Card>
  );
}
