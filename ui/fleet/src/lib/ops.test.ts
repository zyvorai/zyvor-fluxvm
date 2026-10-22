import { describe, expect, it } from 'vitest';
import { fmtBytes, idleText, loadTone, pct, resizeProblem, tokenBody, tokenProblem, tokenState, usesText, volumeProblem } from './ops';

describe('capacity', () => {
  it('clamps percentages and survives an empty fleet', () => {
    expect(pct(0, 0)).toBe(0);
    expect(pct(3, 4)).toBe(75);
    expect(pct(9, 4)).toBe(100);
    expect(pct(-1, 4)).toBe(0);
  });
  it('turns warm then red', () => {
    expect([loadTone(10), loadTone(75), loadTone(89), loadTone(90)]).toEqual(['ok', 'warn', 'warn', 'bad']);
  });
});

describe('formatting', () => {
  it('formats bytes', () => {
    expect(fmtBytes(undefined)).toBe('—');
    expect(fmtBytes(512)).toBe('512 B');
    expect(fmtBytes(1536)).toBe('1.5 KiB');
    expect(fmtBytes(5 * 1024 ** 3)).toBe('5.0 GiB');
  });
  it('says how idle a tunnel is', () => {
    expect(idleText(2)).toBe('active');
    expect(idleText(30)).toBe('30s idle');
    expect(idleText(600)).toBe('10 min idle');
    expect(idleText(7200)).toBe('2 h idle');
  });
});

describe('volumes', () => {
  it('checks create input', () => {
    expect(volumeProblem('data', '20')).toBe('');
    expect(volumeProblem('', '20')).toMatch(/name/);
    expect(volumeProblem('a b', '20')).toMatch(/Name/);
    expect(volumeProblem('data', '0')).toMatch(/Size/);
    expect(volumeProblem('data', '1.5')).toMatch(/Size/);
  });
  it('only grows a detached volume', () => {
    expect(resizeProblem(20, '40', false)).toBe('');
    expect(resizeProblem(20, '20', false)).toMatch(/larger/);
    expect(resizeProblem(20, '10', false)).toMatch(/larger/);
    expect(resizeProblem(20, '40', true)).toMatch(/Detach/);
    expect(resizeProblem(20, 'x', false)).toMatch(/whole/);
  });
});

describe('join tokens', () => {
  const t = { id: 'a', created: 0, uses: 0, revoked: false };
  it('builds a body from the form', () => {
    expect(tokenBody({ hours: '24', uses: '', pattern: ' mac-* ', note: ' ci ' })).toEqual({ ttl_secs: 86400, name_pattern: 'mac-*', note: 'ci' });
    expect(tokenBody({ hours: '', uses: '5', pattern: '', note: '' })).toEqual({ max_uses: 5, note: '' });
  });
  it('rejects zero and fractions, accepts empty', () => {
    expect(tokenProblem({ hours: '', uses: '', pattern: '', note: '' })).toBe('');
    expect(tokenProblem({ hours: '0', uses: '', pattern: '', note: '' })).toMatch(/hours/);
    expect(tokenProblem({ hours: '', uses: '1.5', pattern: '', note: '' })).toMatch(/Uses/);
  });
  it('reports the first reason a token is no good', () => {
    expect(tokenState(t, 100)).toBe('active');
    expect(tokenState({ ...t, revoked: true, expires: 1 }, 100)).toBe('revoked');
    expect(tokenState({ ...t, expires: 100 }, 100)).toBe('expired');
    expect(tokenState({ ...t, max_uses: 2, uses: 2 }, 100)).toBe('used up');
  });
  it('shows uses against the limit', () => {
    expect(usesText({ ...t, uses: 3 })).toBe('3');
    expect(usesText({ ...t, uses: 3, max_uses: 5 })).toBe('3 / 5');
  });
});
