import { describe, expect, it } from 'vitest';
import { type Branch, childrenOf, isKept, planSummary, pruneBody, pruneProblem, startBody, startProblem, volumeGrowProblem, volumeNameProblem } from './branches';

const b = (id: string, parent?: string, labels?: Record<string, string>): Branch => ({ id, name: id, source_vm: 'v', source_name: 'v', parent_branch: parent ?? null, created_at: '', labels, vcpus: 2, memory_mib: 2048, state_bytes: 1 });

describe('start', () => {
  it('bounds the count and checks the prefix', () => {
    expect(startProblem({ count: '3', prefix: 'worker' })).toBe('');
    expect(startProblem({ count: '0', prefix: '' })).toMatch(/Count/);
    expect(startProblem({ count: '300', prefix: '' })).toMatch(/Count/);
    expect(startProblem({ count: '2', prefix: 'Bad Name' })).toMatch(/prefix/);
  });
  it('omits an empty prefix', () => {
    expect(startBody({ count: '2', prefix: '' })).toEqual({ count: 2 });
    expect(startBody({ count: '2', prefix: ' w ' })).toEqual({ count: 2, name_prefix: 'w' });
  });
  it('summarises a plan, or why it is refused', () => {
    const ok = { ok: true, count: 3, parallelism: 3, refusals: [], predicted: { extra_rss_mib_low: 600, extra_rss_mib_high: 6000, host_available_mib: 9000 } };
    expect(planSummary(ok)).toBe('3 machines will add about 600–6000 MiB of memory, 9000 MiB free now.');
    expect(planSummary({ ...ok, count: 1, predicted: { extra_rss_mib_low: 5, extra_rss_mib_high: 5 } })).toBe('1 machine will add about 5 MiB of memory.');
    expect(planSummary({ ...ok, ok: false, refusals: [{ code: 'quota', reason: 'over quota.' }] })).toBe('over quota.');
  });
});

describe('prune', () => {
  it('refuses a prune with no limit', () => expect(pruneProblem({ keepLast: '', olderDays: '', prefix: '' })).toMatch(/at least one/));
  it('converts days to seconds and carries the dry-run flag', () => {
    expect(pruneBody({ keepLast: '5', olderDays: '7', prefix: ' ci- ' }, true)).toEqual({ dry_run: true, keep_last: 5, older_than_secs: 604800, name_prefix: 'ci-' });
    expect(pruneBody({ keepLast: '', olderDays: '1', prefix: '' }, false)).toEqual({ dry_run: false, older_than_secs: 86400 });
  });
  it('validates numbers', () => {
    expect(pruneProblem({ keepLast: 'x', olderDays: '', prefix: '' })).toMatch(/Keep/);
    expect(pruneProblem({ keepLast: '', olderDays: '0', prefix: '' })).toMatch(/Older/);
  });
});

describe('branches', () => {
  it('knows which are protected and who the children are', () => {
    const list = [b('a'), b('b', 'a'), b('c', 'a', { 'fluxvm.dev/keep': 'true' })];
    expect(isKept(list[2])).toBe(true);
    expect(isKept(list[0])).toBe(false);
    expect(childrenOf(list, 'a').map((x) => x.id)).toEqual(['b', 'c']);
  });
});

describe('volumes', () => {
  it('uses the daemon name rule', () => {
    expect(volumeNameProblem('data-1', '10')).toBe('');
    expect(volumeNameProblem('Data', '10')).toMatch(/Name/);
    expect(volumeNameProblem('-x', '10')).toMatch(/Name/);
    expect(volumeNameProblem('x'.repeat(64), '10')).toMatch(/Name/);
    expect(volumeNameProblem('data', '16385')).toMatch(/Size/);
  });
  it('grows only a detached volume', () => {
    expect(volumeGrowProblem(10, '20', false)).toBe('');
    expect(volumeGrowProblem(10, '10', false)).toMatch(/larger/);
    expect(volumeGrowProblem(10, '20', true)).toMatch(/Stop/);
  });
});
