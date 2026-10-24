import { describe, expect, it } from 'vitest';
import { highlight, levelCounts, levelOf, linesOf, matches, parseAnsi, renderLine, shortTime, splitTimestamp } from './logs';

describe('timestamps', () => {
  it('splits the time kubectl prefixes', () => {
    expect(splitTimestamp('2026-10-24T12:00:01.123456789Z hello world')).toEqual({ ts: '2026-10-24T12:00:01.123456789Z', rest: 'hello world' });
    expect(splitTimestamp('no time here')).toEqual({ ts: '', rest: 'no time here' });
    expect(shortTime('2026-10-24T12:00:01.123456789Z')).toBe('12:00:01.123');
    expect(shortTime('2026-10-24T12:00:01Z')).toBe('12:00:01');
  });
});

describe('levelOf', () => {
  it('reads logfmt, JSON, brackets, bare words and klog', () => {
    expect(levelOf('time=x level=error msg="boom"')).toBe('error');
    expect(levelOf('{"level":"warn","msg":"slow"}')).toBe('warn');
    expect(levelOf('[INFO] started')).toBe('info');
    expect(levelOf('ERROR failed to connect')).toBe('error');
    expect(levelOf('WARNING: disk almost full')).toBe('warn');
    expect(levelOf('level=debug msg=x')).toBe('debug');
    expect(levelOf('panic: nil pointer')).toBe('fatal');
    expect(levelOf('E1024 13:55:24.730510 2374 kubelet.go:5] failed')).toBe('error');
    expect(levelOf('W1024 13:55:24.730510 2374 x.go:1] careful')).toBe('warn');
    expect(levelOf('I1024 13:55:24.730510 2374 x.go:1] fine')).toBe('info');
    expect(levelOf('GET /healthz 200')).toBe('');
  });
  it('ignores colour escapes', () => {
    expect(levelOf('\x1b[31mERROR\x1b[0m bad')).toBe('error');
  });
});

describe('parseAnsi', () => {
  it('applies colours and resets', () => {
    expect(parseAnsi('a\x1b[31mred\x1b[0mb')).toEqual([
      { text: 'a', cls: '' },
      { text: 'red', cls: 'fg-red' },
      { text: 'b', cls: '' },
    ]);
    expect(parseAnsi('\x1b[1;32mok\x1b[0m')[0]).toEqual({ text: 'ok', cls: 'fg-green b' });
    expect(parseAnsi('\x1b[91mx')[0].cls).toBe('fg-bright-red');
    expect(parseAnsi('\x1b[44;37mx')[0].cls).toBe('fg-white bg-blue');
  });
  it('drops other escapes and skips extended colours', () => {
    expect(parseAnsi('\x1b[2Kclear\x1b[1Gme').map((s) => s.text).join('')).toBe('clearme');
    expect(parseAnsi('\x1b[38;5;196mx\x1b[0m')[0]).toEqual({ text: 'x', cls: '' });
  });
});

describe('highlight', () => {
  const kinds = (t: string) => highlight(t).filter((s) => s.cls).map((s) => `${s.cls}:${s.text}`);
  it('finds logfmt keys, strings, numbers and addresses', () => {
    expect(kinds('msg="hello there" took=12ms addr=10.42.0.5:8080')).toEqual(['key:msg', 'str:"hello there"', 'key:took', 'num:12ms', 'key:addr', 'ip:10.42.0.5:8080']);
  });
  it('colours http verbs and status codes by class', () => {
    expect(kinds('GET /x 200')).toEqual(['method:GET', 'code-ok:200']);
    expect(kinds('POST /x 404')).toEqual(['method:POST', 'code-warn:404']);
    expect(kinds('GET /x 503')).toEqual(['method:GET', 'code-error:503']);
  });
  it('finds JSON keys and URLs and never loses text', () => {
    expect(kinds('{"a": true, "b": null}')).toContain('key:"a"');
    expect(kinds('see https://example.com/x?y=1 now')).toContain('url:https://example.com/x?y=1');
    for (const t of ['', 'plain words only', 'a=b c="d e" 1.2.3.4 GET 500 {"k":"v"}', '日本語 ✓']) expect(highlight(t).map((s) => s.text).join('')).toBe(t);
  });
});

describe('renderLine and filters', () => {
  it('keeps a line’s own colours, otherwise highlights', () => {
    const own = renderLine(1, '2026-10-24T12:00:00Z \x1b[32mgreen\x1b[0m');
    expect(own.segs[0]).toEqual({ text: 'green', cls: 'fg-green' });
    expect(own.ts).toBe('2026-10-24T12:00:00Z');
    const plain = renderLine(2, 'level=warn msg="x"');
    expect(plain.level).toBe('warn');
    expect(plain.segs.some((s) => s.cls === 'key')).toBe(true);
  });
  it('splits text into lines', () => {
    expect(linesOf('a\nb\r\nc\n')).toEqual(['a', 'b', 'c']);
    expect(linesOf('')).toEqual([]);
    expect(linesOf(Array.from({ length: 6000 }, (_, i) => String(i)).join('\n')).length).toBe(5000);
  });
  it('filters by words and excludes with a dash', () => {
    expect(matches('GET /health 200', '')).toBe(true);
    expect(matches('GET /health 200', 'get HEALTH')).toBe(true);
    expect(matches('GET /health 200', 'post')).toBe(false);
    expect(matches('GET /health 200', 'get -health')).toBe(false);
    expect(matches('GET /x 200', 'get -health')).toBe(true);
    expect(matches('a-b', '-')).toBe(true); // a lone dash is just a dash
  });
  it('counts levels', () => {
    const ls = ['level=error a', 'level=error b', 'INFO c', 'plain'].map((l, i) => renderLine(i, l));
    expect(levelCounts(ls)).toEqual({ error: 2, info: 1 });
  });
});
