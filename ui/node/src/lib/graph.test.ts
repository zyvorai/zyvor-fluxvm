import { describe, expect, it } from 'vitest';
import { type GEdge, type GNode, NODE_H, NODE_W, compact, edgeGeometry, edgePath, edgeWidth, layoutOf, linksOf, neighbourhood, ranksOf } from './graph';

const n = (id: string, kind: GNode['kind'] = 'workload', ns = 'default'): GNode => ({ id, label: id, namespace: ns, kind, pods: 1 });
const e = (from: string, to: string, over: Partial<GEdge> = {}): GEdge => ({ from, to, proto: 'TCP', port: 80, flows: 1, dropped: 0, last: null, ...over });

describe('linksOf', () => {
  it('merges ports between one pair, sums counts and drops self edges', () => {
    const l = linksOf([e('a', 'b', { flows: 3, port: 80 }), e('a', 'b', { flows: 2, dropped: 1, port: 443, last: '2026-10-24T10:00:00Z' }), e('a', 'a'), e('b', 'a', { flows: 9, proto: 'UDP', port: 53 })]);
    expect(l.map((x) => [x.from, x.to, x.flows, x.dropped, x.ports])).toEqual([
      ['b', 'a', 9, 0, ['UDP/53']],
      ['a', 'b', 5, 1, ['TCP/80', 'TCP/443']],
    ]);
    expect(l[1].last).toBe('2026-10-24T10:00:00Z');
  });
  it('keeps a port-less protocol readable', () => {
    expect(linksOf([e('a', 'b', { proto: 'ICMPv4', port: null })])[0].ports).toEqual(['ICMPv4']);
  });
});

describe('ranksOf', () => {
  it('puts sources first and follows the longest path', () => {
    const r = ranksOf(['web', 'api', 'db', 'cache'], [{ from: 'web', to: 'api' }, { from: 'api', to: 'db' }, { from: 'api', to: 'cache' }, { from: 'web', to: 'db' }]);
    expect([r.get('web'), r.get('api'), r.get('db'), r.get('cache')]).toEqual([0, 1, 2, 2]);
  });
  it('survives cycles and self edges', () => {
    const r = ranksOf(['a', 'b', 'c'], [{ from: 'a', to: 'b' }, { from: 'b', to: 'c' }, { from: 'c', to: 'a' }, { from: 'b', to: 'b' }]);
    expect(new Set([...r.values()]).size).toBe(3);
    expect(Math.min(...r.values())).toBe(0);
  });
  it('ignores edges to nodes that are not shown', () => {
    expect(ranksOf(['a'], [{ from: 'a', to: 'ghost' }]).get('a')).toBe(0);
  });
});

describe('layoutOf', () => {
  const nodes = [n('world', 'world'), n('web'), n('api'), n('db'), n('lonely')];
  const links = linksOf([e('world', 'web'), e('web', 'api'), e('api', 'db')]);
  const l = layoutOf(nodes, links);
  it('places every node once, in columns by rank', () => {
    expect(l.nodes.length).toBe(5);
    expect(l.byId.get('world')!.x).toBeLessThan(l.byId.get('web')!.x);
    expect(l.byId.get('web')!.x).toBeLessThan(l.byId.get('api')!.x);
    expect(l.byId.get('api')!.x).toBeLessThan(l.byId.get('db')!.x);
    expect(l.byId.get('lonely')!.rank).toBe(0);
  });
  it('never overlaps two nodes of one column', () => {
    const col0 = l.nodes.filter((p) => p.rank === 0).sort((a, b) => a.y - b.y);
    for (let i = 1; i < col0.length; i++) expect(col0[i].y - col0[i - 1].y).toBeGreaterThanOrEqual(NODE_H);
    expect(l.height).toBeGreaterThanOrEqual(NODE_H);
    expect(l.width).toBeGreaterThanOrEqual(NODE_W);
  });
  it('is deterministic and handles an empty graph', () => {
    expect(layoutOf(nodes, links).nodes.map((p) => [p.id, p.x, p.y])).toEqual(l.nodes.map((p) => [p.id, p.x, p.y]));
    const empty = layoutOf([], []);
    expect(empty.nodes).toEqual([]);
    expect(empty.width).toBe(0 + NODE_W);
  });
});

describe('drawing helpers', () => {
  const a = layoutOf([n('a'), n('b')], linksOf([e('a', 'b')]));
  it('draws forward, backward and same-column curves from a node edge to a node edge', () => {
    const pa = a.byId.get('a')!;
    const pb = a.byId.get('b')!;
    expect(edgePath(pa, pb)).toMatch(/^M \d+(\.\d+)? \d+(\.\d+)? C /);
    expect(edgePath(pa, pb)).toContain(`M ${pa.x + NODE_W} `);
    expect(edgePath(pb, pa)).toContain(`M ${pb.x} `);
    expect(edgePath(pa, pb, 1)).not.toBe(edgePath(pa, pb, 0));
    const s1 = { ...pa, x: 0, y: 0 };
    const s2 = { ...pa, x: 0, y: 200 };
    expect(edgePath(s1, s2)).toContain(`M ${NODE_W} `);
  });
  it('arches a link that skips a column over the nodes between, and labels it clear of both ends', () => {
    const chain = layoutOf([n('a'), n('b'), n('c')], linksOf([e('a', 'b'), e('b', 'c'), e('a', 'c')]));
    const [pa, pb, pc] = ['a', 'b', 'c'].map((i) => chain.byId.get(i)!);
    expect(pc.rank - pa.rank).toBe(2);
    const direct = edgeGeometry(pa, pb);
    const skip = edgeGeometry(pa, pc);
    expect(skip.d).toContain(' S ');
    expect(direct.d).not.toContain(' S ');
    expect(skip.label.y).toBeLessThan(Math.min(pa.y, pc.y)); // the label sits above the arch
    expect(direct.label.x).toBeGreaterThan(pa.x + NODE_W);
    expect(direct.label.x).toBeLessThan(pb.x);
    expect(edgePath(pa, pc)).toBe(skip.d);
    // every number in a path is finite
    for (const g of [direct, skip, edgeGeometry(pb, pa), edgeGeometry({ ...pa, x: 0, y: 0 }, { ...pa, x: 0, y: 90 })]) expect(g.d.match(/-?\d+(\.\d+)?/g)!.every((v) => Number.isFinite(Number(v)))).toBe(true);
  });
  it('scales widths on a log scale between 1.5 and 7', () => {
    expect(edgeWidth(1, 1)).toBe(2);
    expect(edgeWidth(0, 1000)).toBeCloseTo(1.5);
    expect(edgeWidth(1000, 1000)).toBeCloseTo(7);
    expect(edgeWidth(10, 1000)).toBeGreaterThan(edgeWidth(5, 1000));
  });
  it('abbreviates counts', () => {
    expect([compact(7), compact(999), compact(1200), compact(15400), compact(3_400_000)]).toEqual(['7', '999', '1.2k', '15k', '3.4M']);
  });
  it('finds a node’s neighbourhood', () => {
    const ls = linksOf([e('a', 'b'), e('b', 'c'), e('d', 'e')]);
    const h = neighbourhood('b', ls);
    expect([...h.nodes].sort()).toEqual(['a', 'b', 'c']);
    expect(h.links.size).toBe(2);
  });
});

import { HOP, archRoom, filterGraph, namespacesOfGraph } from './graph';

describe('filterGraph', () => {
  const data = {
    nodes: [n('web'), n('api'), n('world', 'world', ''), n('dns', 'system', 'kube-system'), n('other', 'workload', 'staging')],
    edges: [e('world', 'web', { port: 443 }), e('web', 'api', { port: 8080, dropped: 2 }), e('api', 'dns', { proto: 'UDP', port: 53 }), e('other', 'dns')],
  };
  const f = { showSystem: false, droppedOnly: false, namespace: '', text: '' };
  it('hides system workloads and what only they connect', () => {
    const r = filterGraph(data, f);
    expect(r.edges.map((x) => `${x.from}>${x.to}`)).toEqual(['world>web', 'web>api']);
    expect(r.nodes.map((x) => x.id).sort()).toEqual(['api', 'web', 'world']);
    expect(filterGraph(data, { ...f, showSystem: true }).edges.length).toBe(4);
  });
  it('keeps only dropped traffic on request', () => {
    expect(filterGraph(data, { ...f, droppedOnly: true }).edges.map((x) => `${x.from}>${x.to}`)).toEqual(['web>api']);
  });
  it('narrows to a namespace but keeps the internet and host nodes', () => {
    const r = filterGraph(data, { ...f, showSystem: true, namespace: 'default' });
    expect(r.edges.map((x) => `${x.from}>${x.to}`)).toEqual(['world>web', 'web>api']);
    expect(filterGraph(data, { ...f, namespace: 'staging' }).edges).toEqual([]);
  });
  it('searches names and ports', () => {
    expect(filterGraph(data, { ...f, text: '8080' }).edges.length).toBe(1);
    expect(filterGraph(data, { ...f, text: 'WORLD' }).edges.length).toBe(1);
    expect(filterGraph(data, { ...f, text: 'tcp/443' }).edges.length).toBe(1);
    expect(filterGraph(data, { ...f, text: 'nothing' }).nodes).toEqual([]);
  });
  it('lists the namespaces that have workloads', () => {
    expect(namespacesOfGraph(data)).toEqual(['default', 'kube-system', 'staging']);
  });
});

describe('archRoom', () => {
  it('reserves room only when a link skips a column', () => {
    const chain = layoutOf([n('a'), n('b'), n('c')], linksOf([e('a', 'b'), e('b', 'c')]));
    expect(archRoom(chain, linksOf([e('a', 'b'), e('b', 'c')]))).toBe(0);
    expect(archRoom(chain, linksOf([e('a', 'c')]))).toBe(HOP + 18);
    expect(archRoom(chain, [])).toBe(0);
  });
});
