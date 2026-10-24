// Layout for the traffic graph: who talks to whom, left to right. Pure and DOM-free so it can be tested.

export type GNode = { id: string; label: string; namespace: string; kind: 'workload' | 'world' | 'host' | 'system'; pods: number };
export type GEdge = { from: string; to: string; proto: string; port: number | null; flows: number; connections?: number; dropped: number; last: string | null };

export const NODE_W = 188;
export const NODE_H = 46;
export const COL_GAP = 150;
export const ROW_GAP = 22;

export type Placed = GNode & { x: number; y: number; rank: number };

export type Layout = { nodes: Placed[]; width: number; height: number; byId: Map<string, Placed> };

/** Edges between the same two nodes, one per protocol and port, merged for drawing: the graph shows pairs, the panel lists ports. */
export type Link = { from: string; to: string; flows: number; connections: number; dropped: number; ports: string[]; last: string | null };

export const linksOf = (edges: GEdge[]): Link[] => {
  const m = new Map<string, Link>();
  for (const e of edges) {
    if (e.from === e.to) continue;
    const key = `${e.from}\u0000${e.to}`;
    const l = m.get(key) ?? { from: e.from, to: e.to, flows: 0, connections: 0, dropped: 0, ports: [], last: null };
    l.flows += e.flows;
    l.connections += e.connections ?? 0;
    l.dropped += e.dropped;
    const port = `${e.proto}${e.port != null ? '/' + e.port : ''}`;
    if (!l.ports.includes(port)) l.ports.push(port);
    if (e.last && (!l.last || e.last > l.last)) l.last = e.last;
    m.set(key, l);
  }
  return [...m.values()].sort((a, b) => b.flows - a.flows);
};

/** Ranks by longest path from the sources, ignoring edges that would close a cycle (found by depth-first search). */
export const ranksOf = (ids: string[], links: Pick<Link, 'from' | 'to'>[]): Map<string, number> => {
  const out = new Map<string, string[]>(ids.map((i) => [i, []]));
  for (const l of links) if (out.has(l.from) && out.has(l.to)) out.get(l.from)!.push(l.to);
  // break cycles: an edge to a node still on the DFS stack is a back edge
  const state = new Map<string, 0 | 1 | 2>();
  const back = new Set<string>();
  const visit = (n: string) => {
    state.set(n, 1);
    for (const t of out.get(n)!) {
      if (state.get(t) === 1) back.add(`${n}\u0000${t}`);
      else if (!state.get(t)) visit(t);
    }
    state.set(n, 2);
  };
  const hasIn = new Set(links.filter((l) => l.from !== l.to && out.has(l.from) && out.has(l.to)).map((l) => l.to));
  for (const n of ids.filter((i) => !hasIn.has(i)).concat(ids)) if (!state.get(n)) visit(n);
  const rank = new Map<string, number>(ids.map((i) => [i, 0]));
  const dag = ids.map((n) => [n, out.get(n)!.filter((t) => t !== n && !back.has(`${n}\u0000${t}`))] as const);
  for (let pass = 0; pass < ids.length; pass++) {
    let changed = false;
    for (const [n, ts] of dag) for (const t of ts) if (rank.get(t)! < rank.get(n)! + 1) (rank.set(t, rank.get(n)! + 1), (changed = true));
    if (!changed) break;
  }
  return rank;
};

const KIND_ORDER = { world: 0, host: 1, workload: 2, system: 3 } as const;

/** Columns by rank; within a column, nodes are ordered by the average position of their neighbours to cut crossings. */
export const layoutOf = (nodes: GNode[], links: Link[]): Layout => {
  const ids = nodes.map((n) => n.id);
  const rank = ranksOf(ids, links);
  const cols: GNode[][] = [];
  for (const n of [...nodes].sort((a, b) => KIND_ORDER[a.kind] - KIND_ORDER[b.kind] || a.namespace.localeCompare(b.namespace) || a.label.localeCompare(b.label))) (cols[rank.get(n.id)!] ??= []).push(n);
  const pos = new Map<string, number>();
  const settle = () => cols.forEach((c) => c?.forEach((n, i) => pos.set(n.id, i)));
  settle();
  const neighbours = (id: string) => links.flatMap((l) => (l.from === id ? [l.to] : l.to === id ? [l.from] : []));
  for (let sweep = 0; sweep < 4; sweep++) {
    for (const c of sweep % 2 ? [...cols].reverse() : cols) {
      if (!c) continue;
      const score = (n: GNode) => {
        const ps = neighbours(n.id).map((x) => pos.get(x)!).filter((v) => v !== undefined);
        return ps.length ? ps.reduce((a, b) => a + b, 0) / ps.length : pos.get(n.id)!;
      };
      c.sort((a, b) => score(a) - score(b) || a.label.localeCompare(b.label));
      c.forEach((n, i) => pos.set(n.id, i));
    }
  }
  const tallest = Math.max(1, ...cols.map((c) => c?.length ?? 0));
  const height = tallest * (NODE_H + ROW_GAP) - ROW_GAP;
  const placed: Placed[] = [];
  cols.forEach((c, r) => {
    if (!c) return;
    const colH = c.length * (NODE_H + ROW_GAP) - ROW_GAP;
    c.forEach((n, i) => placed.push({ ...n, rank: r, x: r * (NODE_W + COL_GAP), y: (height - colH) / 2 + i * (NODE_H + ROW_GAP) }));
  });
  const width = Math.max(0, cols.filter(Boolean).length - 1) * (NODE_W + COL_GAP) + NODE_W;
  return { nodes: placed, width, height: Math.max(height, NODE_H), byId: new Map(placed.map((p) => [p.id, p])) };
};

/** How far above its end nodes a link that skips columns rises, so it clears the nodes it passes. */
export const HOP = NODE_H / 2 + ROW_GAP + 10;

/** The curve from one node to another, as an SVG path, and the point where its count is written. `lane` shifts it sideways so a
 * pair talking both ways does not draw over itself; a link that skips a column arches over the ones between. */
export const edgeGeometry = (a: Placed, b: Placed, lane = 0): { d: string; label: { x: number; y: number } } => {
  const forward = b.x > a.x;
  const same = b.x === a.x;
  const x1 = forward || same ? a.x + NODE_W : a.x;
  const x2 = forward ? b.x : same ? b.x + NODE_W : b.x + NODE_W;
  const y1 = a.y + NODE_H / 2 + lane * 6;
  const y2 = b.y + NODE_H / 2 + lane * 6;
  if (same) {
    const bow = 60 + Math.abs(a.y - b.y) * 0.1;
    return { d: `M ${x1} ${y1} C ${x1 + bow} ${y1}, ${x2 + bow} ${y2}, ${x2} ${y2}`, label: { x: x1 + bow * 0.75, y: (y1 + y2) / 2 - 5 } };
  }
  const dx = Math.max(40, Math.abs(x2 - x1) / 2);
  const sign = forward ? 1 : -1;
  const skips = Math.abs(b.rank - a.rank) > 1;
  if (skips) {
    const top = Math.min(a.y, b.y) - HOP + lane * 6;
    const xm = (x1 + x2) / 2;
    const k = dx / 2;
    return {
      d: `M ${x1} ${y1} C ${x1 + sign * k} ${y1}, ${x1 + sign * k} ${top}, ${xm} ${top} S ${x2 - sign * k} ${y2}, ${x2} ${y2}`,
      label: { x: xm, y: top - 5 },
    };
  }
  // a point 40% of the way along the cubic, clear of the arrowhead at the far end
  const t = 0.4;
  const c1x = x1 + sign * dx;
  const c2x = x2 - sign * dx;
  const m = 1 - t;
  const lx = m ** 3 * x1 + 3 * m * m * t * c1x + 3 * m * t * t * c2x + t ** 3 * x2;
  const ly = m ** 3 * y1 + 3 * m * m * t * y1 + 3 * m * t * t * y2 + t ** 3 * y2;
  return { d: `M ${x1} ${y1} C ${c1x} ${y1}, ${c2x} ${y2}, ${x2} ${y2}`, label: { x: lx, y: ly - 6 } };
};

export const edgePath = (a: Placed, b: Placed, lane = 0): string => edgeGeometry(a, b, lane).d;

/** Stroke width from a flow count, on a log scale so one busy pair does not flatten the rest: 1.5 to 7 px. */
export const edgeWidth = (flows: number, max: number): number => (max <= 1 ? 2 : 1.5 + (5.5 * Math.log(1 + flows)) / Math.log(1 + max));

/** `1.2k`, `3.4M`. */
export const compact = (n: number): string => (n < 1000 ? String(n) : n < 1e6 ? `${(n / 1e3).toFixed(n < 1e4 ? 1 : 0)}k` : `${(n / 1e6).toFixed(1)}M`);

/** Nodes within one step of `id` (and the links between them), for highlighting on hover or selection. */
export const neighbourhood = (id: string, links: Link[]): { nodes: Set<string>; links: Set<Link> } => {
  const ls = new Set(links.filter((l) => l.from === id || l.to === id));
  const ns = new Set<string>([id]);
  for (const l of ls) (ns.add(l.from), ns.add(l.to));
  return { nodes: ns, links: ls };
};

export type GraphData = { nodes: GNode[]; edges: GEdge[] };

export type GraphFilter = { showSystem: boolean; droppedOnly: boolean; namespace: string; text: string };

/** What the filters leave: edges that pass, and only the nodes they touch. `text` matches node names and ports. */
export const filterGraph = (data: GraphData, f: GraphFilter): GraphData => {
  const byId = new Map(data.nodes.map((n) => [n.id, n]));
  const t = f.text.trim().toLowerCase();
  const nodeOk = (n: GNode | undefined): n is GNode => !!n && (f.showSystem || n.kind !== 'system') && (!f.namespace || n.namespace === f.namespace || n.namespace === '');
  const edges = data.edges.filter((e) => {
    const a = byId.get(e.from);
    const b = byId.get(e.to);
    if (!nodeOk(a) || !nodeOk(b)) return false;
    if (f.droppedOnly && !e.dropped) return false;
    if (!t) return true;
    const port = `${e.proto}/${e.port ?? ''}`.toLowerCase();
    return `${a.label} ${b.label} ${port}`.toLowerCase().includes(t);
  });
  const used = new Set(edges.flatMap((e) => [e.from, e.to]));
  return { nodes: data.nodes.filter((n) => used.has(n.id)), edges };
};

export const namespacesOfGraph = (data: GraphData): string[] => [...new Set(data.nodes.map((n) => n.namespace).filter(Boolean))].sort();

/** Extra room above the nodes for links that skip a column and arch over the ones between. */
export const archRoom = (layout: Layout, links: Pick<Link, 'from' | 'to'>[]): number =>
  links.some((l) => Math.abs((layout.byId.get(l.to)?.rank ?? 0) - (layout.byId.get(l.from)?.rank ?? 0)) > 1) ? HOP + 18 : 0;
