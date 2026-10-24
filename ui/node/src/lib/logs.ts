// Turning raw container log text into coloured segments, in the palette of Terminal.app.
// Pure and DOM-free so it can be tested: the page maps a segment's `cls` to a CSS class.

export type Seg = { text: string; cls: string };

/** Terminal.app's colour names, in ANSI order. */
const ANSI = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];

export type Level = 'fatal' | 'error' | 'warn' | 'info' | 'debug' | 'trace' | '';

/** A line from `kubectl logs --timestamps`: `2026-10-24T12:00:00.123456789Z message`. */
export const splitTimestamp = (line: string): { ts: string; rest: string } => {
  const m = /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z)\s?(.*)$/s.exec(line);
  return m ? { ts: m[1], rest: m[2] } : { ts: '', rest: line };
};

/** `12:00:00.123` from a Kubernetes timestamp; the date is the same for a tail of a running container. */
export const shortTime = (ts: string): string => {
  const m = /T(\d{2}:\d{2}:\d{2})(?:\.(\d{1,3}))?/.exec(ts);
  return m ? m[1] + (m[2] ? '.' + m[2].padEnd(3, '0') : '') : ts;
};

/** The severity a line announces: logfmt, JSON, bracketed or bare words, and klog's `E1024 ...` prefix. */
export const levelOf = (text: string): Level => {
  const plain = text.replace(/\x1b\[[0-9;]*m/g, '');
  const m =
    /(?:\blevel["']?\s*[=:]\s*["']?|\[|\b)(fatal|panic|critical|crit|error|err|warn(?:ing)?|info|debug|trace)(?:["'\]]|\b)/i.exec(plain) ??
    null;
  if (m) {
    const w = m[1].toLowerCase();
    if (w === 'fatal' || w === 'panic' || w === 'critical' || w === 'crit') return 'fatal';
    if (w === 'error' || w === 'err') return 'error';
    if (w.startsWith('warn')) return 'warn';
    return w as Level;
  }
  const k = /^([EWIF])\d{4}\s/.exec(plain); // klog: E1024 13:55:24.730510 ...
  if (k) return ({ E: 'error', W: 'warn', I: 'info', F: 'fatal' } as const)[k[1] as 'E' | 'W' | 'I' | 'F'];
  return '';
};

type Sgr = { fg: string; bg: string; bold: boolean; dim: boolean; underline: boolean };
const reset = (): Sgr => ({ fg: '', bg: '', bold: false, dim: false, underline: false });

const clsOf = (s: Sgr): string =>
  [s.fg && 'fg-' + s.fg, s.bg && 'bg-' + s.bg, s.bold && 'b', s.dim && 'dim', s.underline && 'u'].filter(Boolean).join(' ');

/** Applies the colours of ANSI "select graphic rendition" escapes and drops every other escape sequence. */
export const parseAnsi = (text: string): Seg[] => {
  const out: Seg[] = [];
  let st = reset();
  let last = 0;
  const re = /\x1b\[([0-9;?]*)([A-Za-z])/g;
  const push = (t: string) => t && out.push({ text: t, cls: clsOf(st) });
  for (let m = re.exec(text); m; m = re.exec(text)) {
    push(text.slice(last, m.index));
    last = m.index + m[0].length;
    if (m[2] !== 'm') continue;
    const codes = (m[1] || '0').split(';').map((c) => Number(c) || 0);
    for (let i = 0; i < codes.length; i++) {
      const c = codes[i];
      if (c === 0) st = reset();
      else if (c === 1) st.bold = true;
      else if (c === 2) st.dim = true;
      else if (c === 4) st.underline = true;
      else if (c === 22) (st.bold = false), (st.dim = false);
      else if (c === 24) st.underline = false;
      else if (c >= 30 && c <= 37) st.fg = ANSI[c - 30];
      else if (c >= 90 && c <= 97) st.fg = 'bright-' + ANSI[c - 90];
      else if (c === 39) st.fg = '';
      else if (c >= 40 && c <= 47) st.bg = ANSI[c - 40];
      else if (c >= 100 && c <= 107) st.bg = 'bright-' + ANSI[c - 100];
      else if (c === 49) st.bg = '';
      else if (c === 38 || c === 48) i += codes[i + 1] === 5 ? 2 : codes[i + 1] === 2 ? 4 : 0; // 256/truecolour: skip it
    }
  }
  push(text.slice(last));
  return out;
};

const TOKENS: [RegExp, (m: RegExpExecArray) => string][] = [
  [/"(?:[^"\\]|\\.)*"(?=\s*:)/y, () => 'key'], // JSON key
  [/"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'/y, () => 'str'],
  [/\b[A-Za-z_][\w.-]*(?==)/y, () => 'key'], // logfmt key
  [/\bhttps?:\/\/[^\s"')\]]+/y, () => 'url'],
  [/\b\d{1,3}(?:\.\d{1,3}){3}(?::\d+)?\b/y, () => 'ip'],
  [/\b(?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\b/y, () => 'method'],
  [/\b[1-5]\d\d\b(?=[\s"',}]|$)/y, (m) => ({ 2: 'ok', 3: 'info', 4: 'warn', 5: 'error' })[m[0][0] as '2'] ? 'code-' + ({ 2: 'ok', 3: 'info', 4: 'warn', 5: 'error' } as Record<string, string>)[m[0][0]] : 'num'],
  [/\b\d+(?:\.\d+)?(?:ms|µs|us|ns|s|m|h|%|[KMGT]i?B|B)?\b/y, () => 'num'],
  [/\b(?:true|false|null|nil)\b/y, () => 'lit'],
];

/** Highlights the interesting parts of a plain run: keys, strings, URLs, addresses, HTTP verbs and status codes, numbers. */
export const highlight = (text: string): Seg[] => {
  const out: Seg[] = [];
  let plain = '';
  const flush = () => plain && (out.push({ text: plain, cls: '' }), (plain = ''));
  let i = 0;
  while (i < text.length) {
    let hit: { len: number; cls: string } | null = null;
    if (/[\w"'h]/.test(text[i])) {
      for (const [re, cls] of TOKENS) {
        re.lastIndex = i;
        const m = re.exec(text);
        if (m && m[0].length) {
          hit = { len: m[0].length, cls: cls(m) };
          break;
        }
      }
    }
    if (hit && (i === 0 || !/[\w]/.test(text[i - 1]))) {
      flush();
      out.push({ text: text.slice(i, i + hit.len), cls: hit.cls });
      i += hit.len;
    } else {
      plain += text[i++];
    }
  }
  flush();
  return out;
};

export type LogLine = { n: number; ts: string; level: Level; segs: Seg[]; raw: string };

/** One display line: its time, its level, and its text as coloured segments. A line that carries its own colours keeps them. */
export const renderLine = (n: number, line: string): LogLine => {
  const { ts, rest } = splitTimestamp(line);
  const hasAnsi = /\x1b\[[0-9;]*m/.test(rest);
  const segs = hasAnsi ? parseAnsi(rest) : highlight(rest.replace(/\x1b\[[0-9;?]*[A-Za-z]/g, ''));
  return { n, ts, level: levelOf(rest), segs, raw: line };
};

export const MAX_LINES = 5000;

/** Splits the text the daemon returned into lines, drops the trailing empty one and caps the count. */
export const linesOf = (text: string): string[] => {
  const lines = text.split(/\r?\n/);
  if (lines.length && lines[lines.length - 1] === '') lines.pop();
  return lines.length > MAX_LINES ? lines.slice(-MAX_LINES) : lines;
};

/** The filter box: plain text, case-insensitive, over the raw line; `-word` excludes. */
export const matches = (raw: string, filter: string): boolean => {
  const terms = filter.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (!terms.length) return true;
  const hay = raw.toLowerCase();
  return terms.every((t) => (t.startsWith('-') && t.length > 1 ? !hay.includes(t.slice(1)) : hay.includes(t)));
};

export const LEVEL_ORDER: Level[] = ['fatal', 'error', 'warn', 'info', 'debug', 'trace'];

/** Counts per level over the lines shown, for the little legend. */
export const levelCounts = (lines: LogLine[]): Record<string, number> => {
  const c: Record<string, number> = {};
  for (const l of lines) if (l.level) c[l.level] = (c[l.level] ?? 0) + 1;
  return c;
};
