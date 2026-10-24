import { useMemo, useState } from 'react';
import { Globe, Server } from 'lucide-react';
import { Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { type GEdge, type GNode, type GraphFilter, type Link, NODE_H, NODE_W, archRoom, compact, edgeGeometry, edgeWidth, filterGraph, layoutOf, linksOf, namespacesOfGraph, neighbourhood } from '../lib/graph';
import { ageOf } from '../lib/kube';

type Flows = {
  cluster: string;
  nodes: GNode[];
  edges: GEdge[];
  agents: number;
  errors: { node: string; error: string }[];
  flows_seen: number;
  flows_counted: number;
  window: { from: string; to: string };
  truncated: boolean;
};

const PAD = 28;
const enc = encodeURIComponent;

/** Who talks to whom: Cilium Hubble flows as a service map, calls running left to right. */
export function KubeGraph({ cluster }: { cluster: string }) {
  const poll = usePoll(async () => await api.get<Flows>(`/v1/kube/${enc(cluster)}/flows`), 10_000, [cluster]);
  const [filter, setFilter] = useState<GraphFilter>({ showSystem: false, droppedOnly: false, namespace: '', text: '' });
  const [picked, setPicked] = useState('');
  const [hover, setHover] = useState('');
  const d = poll.data;

  const shown = useMemo(() => (d ? filterGraph(d, filter) : { nodes: [], edges: [] }), [d, filter]);
  const links = useMemo(() => linksOf(shown.edges), [shown]);
  const layout = useMemo(() => layoutOf(shown.nodes, links), [shown, links]);
  const maxFlows = Math.max(1, ...links.map((l) => l.flows));
  const focus = picked || hover;
  const near = useMemo(() => (focus ? neighbourhood(focus, links) : null), [focus, links]);
  const pair = useMemo(() => new Set(links.map((l) => `${l.from}\u0000${l.to}`)), [links]);
  const sel = picked ? layout.byId.get(picked) : undefined;
  const room = archRoom(layout, links);

  if (poll.error && !d)
    return <Card><EmptyState title={/no running Cilium agent/i.test(poll.error.message) ? 'Traffic needs Cilium' : 'Could not read traffic'} body={poll.error.message} /></Card>;
  if (!d) return <Skeleton h={220} />;

  const seconds = d.window.from && d.window.to ? Math.max(1, Math.round((Date.parse(d.window.to) - Date.parse(d.window.from)) / 1000)) : 0;
  const set = (p: Partial<GraphFilter>) => (setFilter({ ...filter, ...p }), setPicked(''));

  return (
    <div className="kg-layout">
      <Card title="Traffic">
        <div className="kg-bar">
          <label><input type="checkbox" checked={filter.showSystem} onChange={(e) => set({ showSystem: e.target.checked })} /> system pods</label>
          <label><input type="checkbox" checked={filter.droppedOnly} onChange={(e) => set({ droppedOnly: e.target.checked })} /> dropped only</label>
          <select className="fx-input" aria-label="Namespace" value={filter.namespace} onChange={(e) => set({ namespace: e.target.value })}>
            <option value="">All namespaces</option>
            {namespacesOfGraph(d).map((x) => <option key={x}>{x}</option>)}
          </select>
          <input className="fx-input" aria-label="Filter traffic" placeholder="Filter by name or port" value={filter.text} onChange={(e) => set({ text: e.target.value })} />
          <span className="kg-grow" />
          <Pill tone="info" raw>{compact(d.flows_counted)} flows{seconds ? ` in ${seconds < 120 ? seconds + 's' : Math.round(seconds / 60) + 'm'}` : ''}</Pill>
        </div>
        {layout.nodes.length === 0 ? (
          <p className="fx-muted">{d.edges.length ? 'Nothing matches these filters.' : 'No traffic recorded yet. Cilium keeps the most recent few thousand flows on each node.'}</p>
        ) : (
          <div className="kg-scroll">
            <svg className="kg-svg" width={layout.width + PAD * 2} height={layout.height + PAD * 2 + room} viewBox={`${-PAD} ${-PAD - room} ${layout.width + PAD * 2} ${layout.height + PAD * 2 + room}`} role="img" aria-label={`Traffic between ${layout.nodes.length} workloads`}>
              <defs>
                <marker id="kg-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerUnits="userSpaceOnUse" markerWidth="11" markerHeight="11" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" className="kg-arrowhead" /></marker>
                <marker id="kg-arrow-bad" viewBox="0 0 10 10" refX="9" refY="5" markerUnits="userSpaceOnUse" markerWidth="11" markerHeight="11" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" className="kg-arrowhead bad" /></marker>
              </defs>
              <g>
                {links.map((l) => {
                  const a = layout.byId.get(l.from)!;
                  const b = layout.byId.get(l.to)!;
                  const lane = pair.has(`${l.to}\u0000${l.from}`) ? (l.from < l.to ? -1 : 1) : 0;
                  const dim = near && !near.links.has(l);
                  const bad = l.dropped > 0;
                  const geo = edgeGeometry(a, b, lane);
                  return (
                    <g key={`${l.from}>${l.to}`} className={`kg-edge${bad ? ' bad' : ''}${dim ? ' dim' : ''}`}>
                      <path d={geo.d} strokeWidth={edgeWidth(l.flows, maxFlows)} markerEnd={`url(#${bad ? 'kg-arrow-bad' : 'kg-arrow'})`} fill="none" />
                      <path d={geo.d} className="kg-hit" fill="none" strokeWidth={14}><title>{`${a.label} → ${b.label}\n${l.ports.join(', ')}\n${l.flows} flows${l.connections ? `, ${l.connections} new connections` : ''}${bad ? `, ${l.dropped} dropped` : ''}`}</title></path>
                      <text x={geo.label.x} y={geo.label.y} className="kg-count" textAnchor="middle">{compact(l.flows)}</text>
                    </g>
                  );
                })}
              </g>
              <g>
                {layout.nodes.map((n) => (
                  <g key={n.id} className={`kg-node ${n.kind}${picked === n.id ? ' is-picked' : ''}${near && !near.nodes.has(n.id) ? ' dim' : ''}`} transform={`translate(${n.x},${n.y})`} tabIndex={0} role="button" aria-label={`${n.label}${n.namespace ? ' in ' + n.namespace : ''}`} aria-pressed={picked === n.id}
                    onClick={() => setPicked(picked === n.id ? '' : n.id)} onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && (e.preventDefault(), setPicked(picked === n.id ? '' : n.id))} onMouseEnter={() => setHover(n.id)} onMouseLeave={() => setHover('')}>
                    <rect width={NODE_W} height={NODE_H} rx={10} />
                    <text x={14} y={20} className="kg-label">{clip(n.label, 22)}</text>
                    <text x={14} y={36} className="kg-sub">{n.kind === 'world' ? 'outside the cluster' : n.kind === 'host' ? 'node network' : `${n.namespace}${n.pods > 1 ? ` · ${n.pods} pods` : ''}`}</text>
                  </g>
                ))}
              </g>
            </svg>
          </div>
        )}
        <div className="kg-legend">
          <span><i className="kg-sw ok" /> allowed</span>
          <span><i className="kg-sw bad" /> some packets dropped</span>
          <span className="fx-muted">Line weight is how many flows; the number is the count. Click a workload for its calls.</span>
        </div>
        {d.errors.length ? <p className="fx-muted">{d.errors.length} agent{d.errors.length > 1 ? 's' : ''} did not answer: {d.errors.map((e) => `${e.node}: ${e.error}`).join('; ')}</p> : null}
        {d.truncated ? <p className="fx-muted">Showing the busiest 400 connections.</p> : null}
      </Card>
      <Card title={sel ? sel.label : 'Details'}>
        {sel ? <NodePanel node={sel} links={links} byId={layout.byId} /> : <Summary d={d} shown={shown.nodes.length} links={links.length} seconds={seconds} />}
      </Card>
    </div>
  );
}

const clip = (s: string, n: number) => (s.length > n ? s.slice(0, n - 1) + '…' : s);

function Summary({ d, shown, links, seconds }: { d: Flows; shown: number; links: number; seconds: number }) {
  return (
    <div className="kg-panel">
      <p className="fx-muted">Showing {shown} workload{shown === 1 ? '' : 's'} and {links} connection{links === 1 ? '' : 's'}.</p>
      <dl>
        <dt>Flows counted</dt><dd>{d.flows_counted.toLocaleString()} of {d.flows_seen.toLocaleString()} seen</dd>
        <dt>Window</dt><dd>{seconds ? `${seconds < 120 ? seconds + ' seconds' : Math.round(seconds / 60) + ' minutes'}` : '—'}</dd>
        <dt>Cilium agents</dt><dd>{d.agents}</dd>
      </dl>
      <p className="fx-muted">Each packet event counts once, on the sender’s node; replies are not counted. The window is however long Cilium’s per-node buffer (a few thousand flows) covers, so a quiet cluster shows a longer one. Select a workload to see what it calls and what calls it.</p>
    </div>
  );
}

function NodePanel({ node, links, byId }: { node: GNode; links: Link[]; byId: Map<string, GNode> }) {
  const now = Date.now();
  const out = links.filter((l) => l.from === node.id);
  const inc = links.filter((l) => l.to === node.id);
  const row = (l: Link, other: string) => (
    <tr key={`${l.from}>${l.to}`}>
      <td className="vm-name">{byId.get(other)?.label ?? other}</td>
      <td className="fx-mono">{l.ports.join(' ')}</td>
      <td>{compact(l.flows)}</td>
      <td>{l.dropped ? <Pill tone="bad">{l.dropped}</Pill> : <span className="fx-muted">0</span>}</td>
      <td className="fx-muted">{l.last ? ageOf(l.last, now) : '—'}</td>
    </tr>
  );
  const table = (title: string, ls: Link[], other: (l: Link) => string) => (
    <>
      <h3 className="kg-h">{title}</h3>
      {ls.length ? (
        <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>{title === 'Calls' ? 'To' : 'From'}</th><th>Ports</th><th>Flows</th><th>Dropped</th><th>Last</th></tr></thead><tbody>{ls.map((l) => row(l, other(l)))}</tbody></table></div>
      ) : <p className="fx-muted">None in this window.</p>}
    </>
  );
  return (
    <div className="kg-panel">
      <p className="fx-muted">{node.kind === 'world' ? <><Globe size={13} /> Traffic to or from outside the cluster.</> : node.kind === 'host' ? <><Server size={13} /> The node’s own network, including the Kubernetes API server.</> : `${node.namespace} · ${node.pods} pod${node.pods === 1 ? '' : 's'}`}</p>
      {table('Calls', out, (l) => l.to)}
      {table('Called by', inc, (l) => l.from)}
    </div>
  );
}
