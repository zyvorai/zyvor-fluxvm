import { describe, expect, it } from 'vitest';
import { baseName, execFailure, execTone, fromBase64, isAbsPath, toBase64 } from './exec';

describe('exec helpers', () => {
  it('treats result:error as a failure even on HTTP 200', () => {
    expect(execFailure({ result: 'error', message: 'agent offline' })).toBe('agent offline');
    expect(execFailure({ result: 'error' })).toMatch(/agent/);
    expect(execFailure({ result: 'exec', exit_code: 1 })).toBe('');
  });
  it('tones by exit code', () => {
    expect(execTone({ exit_code: 0 })).toBe('ok');
    expect(execTone({ exit_code: 2 })).toBe('bad');
    expect(execTone({})).toBe('idle');
  });
  it('round-trips bytes through base64, including non-ASCII and large inputs', () => {
    const bytes = new TextEncoder().encode('héllo — 世界');
    expect(new TextDecoder().decode(fromBase64(toBase64(bytes)))).toBe('héllo — 世界');
    const big = new Uint8Array(100_000).map((_, i) => i % 251);
    expect(fromBase64(toBase64(big))).toEqual(big);
  });
  it('validates absolute paths like the daemon', () => {
    expect(isAbsPath('/workspace/a.txt')).toBe(true);
    for (const bad of ['relative', '/a/../b', '/a\nb', '/a\0b', '']) expect(isAbsPath(bad)).toBe(false);
  });
  it('derives a download name', () => {
    expect(baseName('/var/log/syslog')).toBe('syslog');
    expect(baseName('/tmp/dir/')).toBe('dir');
    expect(baseName('/')).toBe('download');
  });
});
