import { describe, expect, it } from 'vitest';
import { parseSse } from './sse';
import { parseHash } from './router';
import { filterCommands } from './palette';
import { detectOs } from './capabilities';
import { ago, fmtMiB, left, statusTone } from './format';

describe('parseSse', () => {
  it('parses a JSON event', () => {
    expect(parseSse('event: vm.started\ndata: {"id":"a"}')).toEqual({ event: 'vm.started', data: { id: 'a' } });
  });
  it('joins multi-line data and falls back to text', () => {
    expect(parseSse('data: hello\ndata: world')).toEqual({ event: 'message', data: 'hello\nworld' });
  });
  it('ignores comments and empty blocks', () => {
    expect(parseSse(': keep-alive')).toBeNull();
  });
});

describe('parseHash', () => {
  it('splits path and query', () => {
    const r = parseHash('#/machines?q=web&x=1');
    expect(r.path).toBe('/machines');
    expect(r.query.get('q')).toBe('web');
    expect(r.segs).toEqual(['machines']);
  });
  it('treats empty as root', () => {
    expect(parseHash('').path).toBe('/');
    expect(parseHash('#/').path).toBe('/');
  });
  it('keeps nested segments', () => {
    expect(parseHash('#/machines/abc/network').segs).toEqual(['machines', 'abc', 'network']);
  });
});

describe('filterCommands', () => {
  const cmds = [
    { id: '1', label: 'Go to Machines', group: 'Navigate', run: () => {} },
    { id: '2', label: 'web-01', group: 'Machines', hint: 'running', run: () => {} },
  ];
  it('matches label, group and hint case-insensitively', () => {
    expect(filterCommands(cmds, 'WEB').map((c) => c.id)).toEqual(['2']);
    expect(filterCommands(cmds, 'running').map((c) => c.id)).toEqual(['2']);
    expect(filterCommands(cmds, 'navigate').map((c) => c.id)).toEqual(['1']);
  });
  it('returns everything for an empty query', () => {
    expect(filterCommands(cmds, '  ')).toHaveLength(2);
  });
});

describe('detectOs', () => {
  it('recognises Apple and Linux hypervisors', () => {
    expect(detectOs({ backends: ['vz'] })).toBe('macos');
    expect(detectOs({ backends: ['qemu', 'kvm'] })).toBe('linux');
    expect(detectOs({})).toBe('unknown');
  });
});

describe('format', () => {
  it('formats memory', () => {
    expect(fmtMiB(512)).toBe('512 MiB');
    expect(fmtMiB(2048)).toBe('2 GiB');
  });
  it('formats relative time', () => {
    const now = 1_000_000;
    expect(ago(now - 2000, now)).toBe('just now');
    expect(ago(now - 90_000, now)).toBe('1m ago');
    expect(ago('nope', now)).toBe('—');
  });
  it('counts down to a deadline', () => {
    const now = 1_000_000;
    expect(left(now + 90_000, now)).toBe('1m left');
    expect(left(now - 1, now)).toBe('expired');
    expect(left(null, now)).toBe('never');
    expect(left('nope', now)).toBe('—');
  });
  it('maps status to tone', () => {
    expect(statusTone('running')).toBe('ok');
    expect(statusTone('failed')).toBe('bad');
    expect(statusTone('paused')).toBe('warn');
    expect(statusTone('whatever')).toBe('idle');
  });
});
