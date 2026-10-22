import { describe, expect, it } from 'vitest';
import { curlFor, filterOps, operationsOf, runnable } from './explorer';

const doc = { paths: { '/v1/vms': { get: { summary: 'List' }, post: { summary: 'Create' } }, '/v1/vms/{id}': { delete: { summary: 'Delete' } }, '/v1/events/stream': { get: {} } } };

describe('explorer', () => {
  it('lists operations in path then method order', () => {
    expect(operationsOf(doc).map((o) => o.method + ' ' + o.path)).toEqual(['GET /v1/events/stream', 'GET /v1/vms', 'POST /v1/vms', 'DELETE /v1/vms/{id}']);
  });
  it('tolerates junk', () => {
    expect(operationsOf(null)).toEqual([]);
    expect(operationsOf({ paths: 3 })).toEqual([]);
  });
  it('filters by any part', () => {
    expect(filterOps(operationsOf(doc), 'create')).toHaveLength(1);
    expect(filterOps(operationsOf(doc), '')).toHaveLength(4);
  });
  it('only runs plain reads', () => {
    const ops = operationsOf(doc);
    expect(ops.filter(runnable).map((o) => o.path)).toEqual(['/v1/vms']);
  });
  it('never inlines the token in curl', () => {
    const c = curlFor({ method: 'POST', path: '/v1/pools', summary: '', tag: '' }, 'http://h:7788');
    expect(c).toContain('$FLUXVM_TOKEN');
    expect(c).toContain('-X POST');
  });
});
