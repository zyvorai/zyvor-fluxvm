'use strict';
// FluxVM terminal page: tabs and split panes of xterm.js terminals, each wired to an agent session's
// pty WebSocket. The pure helpers at the top (themes, layout tree, URL, etc.) are exported for node
// tests; the page code at the bottom only runs in a browser.

const TOKEN_KEY = 'fluxvm.token';
const THEME_KEY = 'fluxvm.terminal.theme';
const SIZE_KEY = 'fluxvm.terminal.size';

// ANSI palettes (16 colours + foreground, background, cursor, selection). Hand-written values.
// `chrome` styles the window around the terminal (tab bar, title bar, accent); themes without it
// get defaults derived from their palette.
const THEMES = {
  // The look of the user's ~/.wezterm.lua: Tokyo Night Storm, neon-blue accent, deep-space chrome.
  'Neon Storm': {
    dark: true,
    foreground: '#c0caf5', background: '#24283b', cursor: '#82aaff', cursorAccent: '#24283b', selectionBackground: '#364a82',
    black: '#1d202f', red: '#f7768e', green: '#9ece6a', yellow: '#e0af68', blue: '#7aa2f7', magenta: '#bb9af7', cyan: '#7dcfff', white: '#a9b1d6',
    brightBlack: '#414868', brightRed: '#f7768e', brightGreen: '#9ece6a', brightYellow: '#e0af68', brightBlue: '#7aa2f7',
    brightMagenta: '#bb9af7', brightCyan: '#7dcfff', brightWhite: '#c0caf5',
    chrome: {
      titlebar: '#16161e', titlebarInactive: '#1a1b26', tabBarBg: '#16161e',
      tabActiveBg: '#82aaff', tabActiveFg: '#16161e', tabInactiveBg: '#2a2b3b', tabInactiveFg: '#b4befe',
      tabHoverBg: '#414868', tabHoverFg: '#c0caf5', newTabFg: '#82aaff', accent: '#82aaff',
      gradient: ['#16161e', '#2a2b3b', '#16161e'], gradientAngle: 135, opacity: 0.94,
      paneDim: { saturation: 0.8, brightness: 0.7 },
    },
  },
  'macOS Pro': {
    dark: true,
    foreground: '#f2f2f2', background: '#141414', cursor: '#b0b0b0', cursorAccent: '#141414', selectionBackground: '#4d5970',
    black: '#000000', red: '#c91b00', green: '#00c200', yellow: '#c7c400', blue: '#4a6bff', magenta: '#c930c7', cyan: '#00c5c7', white: '#c7c7c7',
    brightBlack: '#676767', brightRed: '#ff6d67', brightGreen: '#5ff967', brightYellow: '#fefb67', brightBlue: '#6871ff',
    brightMagenta: '#ff76ff', brightCyan: '#5ffdff', brightWhite: '#feffff',
  },
  'macOS Basic': {
    dark: false,
    foreground: '#1d1d1f', background: '#fbfbfb', cursor: '#1d1d1f', cursorAccent: '#fbfbfb', selectionBackground: '#b3d7ff',
    black: '#000000', red: '#c23621', green: '#25bc24', yellow: '#adad27', blue: '#492ee1', magenta: '#d338d3', cyan: '#33bbc8', white: '#cbcccd',
    brightBlack: '#818383', brightRed: '#fc391f', brightGreen: '#31e722', brightYellow: '#eaec23', brightBlue: '#5833ff',
    brightMagenta: '#f935f8', brightCyan: '#14f0f0', brightWhite: '#e9ebeb',
  },
  'Tokyo Night': {
    dark: true,
    foreground: '#c0caf5', background: '#1a1b26', cursor: '#c0caf5', cursorAccent: '#1a1b26', selectionBackground: '#33467c',
    black: '#15161e', red: '#f7768e', green: '#9ece6a', yellow: '#e0af68', blue: '#7aa2f7', magenta: '#bb9af7', cyan: '#7dcfff', white: '#a9b1d6',
    brightBlack: '#414868', brightRed: '#f7768e', brightGreen: '#9ece6a', brightYellow: '#e0af68', brightBlue: '#7aa2f7',
    brightMagenta: '#bb9af7', brightCyan: '#7dcfff', brightWhite: '#c0caf5',
  },
  'Dracula': {
    dark: true,
    foreground: '#f8f8f2', background: '#282a36', cursor: '#f8f8f2', cursorAccent: '#282a36', selectionBackground: '#44475a',
    black: '#21222c', red: '#ff5555', green: '#50fa7b', yellow: '#f1fa8c', blue: '#bd93f9', magenta: '#ff79c6', cyan: '#8be9fd', white: '#f8f8f2',
    brightBlack: '#6272a4', brightRed: '#ff6e6e', brightGreen: '#69ff94', brightYellow: '#ffffa5', brightBlue: '#d6acff',
    brightMagenta: '#ff92df', brightCyan: '#a4ffff', brightWhite: '#ffffff',
  },
  'Solarized Dark': {
    dark: true,
    foreground: '#93a1a1', background: '#002b36', cursor: '#93a1a1', cursorAccent: '#002b36', selectionBackground: '#073642',
    black: '#073642', red: '#dc322f', green: '#859900', yellow: '#b58900', blue: '#268bd2', magenta: '#d33682', cyan: '#2aa198', white: '#eee8d5',
    brightBlack: '#586e75', brightRed: '#cb4b16', brightGreen: '#586e75', brightYellow: '#657b83', brightBlue: '#839496',
    brightMagenta: '#6c71c4', brightCyan: '#93a1a1', brightWhite: '#fdf6e3',
  },
};
const DEFAULT_DARK = 'Neon Storm';
const DEFAULT_LIGHT = 'macOS Basic';
// Nerd Font names first (the user's WezTerm font list), then the system monospace fonts.
const FONT_STACK = '"Hack Nerd Font Mono", "MesloLGS Nerd Font Mono", "FiraCode Nerd Font", "0xProto Nerd Font", ' +
  '"SF Mono", Menlo, "JetBrains Mono", Consolas, monospace';

/** The theme name to use: a saved choice that exists, else by colour scheme. */
function pickTheme(saved, prefersDark) {
  if (saved && Object.prototype.hasOwnProperty.call(THEMES, saved)) return saved;
  return prefersDark ? DEFAULT_DARK : DEFAULT_LIGHT;
}

/** WCAG contrast ratio of two #rrggbb colours. */
function contrast(a, b) {
  const lum = (hex) => {
    const n = parseInt(hex.slice(1), 16);
    return [n >> 16, (n >> 8) & 255, n & 255]
      .map((v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); })
      .reduce((s, v, i) => s + v * [0.2126, 0.7152, 0.0722][i], 0);
  };
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/** Window chrome colours for a theme: its own `chrome`, or defaults taken from its palette. */
function chromeFor(theme) {
  if (theme.chrome) return theme.chrome;
  return {
    titlebar: theme.background, titlebarInactive: theme.background, tabBarBg: theme.background,
    tabActiveBg: theme.blue, tabActiveFg: theme.background, tabInactiveBg: theme.black, tabInactiveFg: theme.foreground,
    tabHoverBg: theme.brightBlack, tabHoverFg: theme.foreground, newTabFg: theme.blue, accent: theme.blue,
    gradient: null, gradientAngle: 135, opacity: 1, paneDim: { saturation: 0.8, brightness: 0.7 },
  };
}

/** `ws://host/console/pty/<ticket>?...` for the page's own origin. */
function ptyUrl(loc, ticket, cols, rows) {
  const proto = loc.protocol === 'https:' ? 'wss:' : 'ws:';
  const q = new URLSearchParams({
    cols: String(cols), rows: String(rows), term: 'xterm-256color', colorterm: 'truecolor', term_program: 'FluxVM',
  });
  return `${proto}//${loc.host}/console/pty/${encodeURIComponent(ticket)}?${q}`;
}

/** The exit code from a close reason like `exit=3`, else null. */
function exitCode(reason) {
  const m = /^exit=(-?\d+)$/.exec(reason || '');
  return m ? Number(m[1]) : null;
}

/** A font size kept inside what the page supports. */
function clampSize(n) {
  n = Math.round(Number(n));
  if (!Number.isFinite(n)) return 16;
  return Math.min(32, Math.max(9, n));
}

// ---- layout tree: a leaf is {leaf: id}; a split is {dir: 'h'|'v', a, b, ratio} ('h' puts a left of b) ----

/** Ids of every pane, left/top first. */
function leaves(t) {
  return t.leaf !== undefined ? [t.leaf] : [...leaves(t.a), ...leaves(t.b)];
}

/** Splits leaf `id` in `dir`; the new pane `newId` goes after it. Unknown ids leave the tree unchanged. */
function splitLeaf(t, id, dir, newId) {
  if (t.leaf !== undefined) {
    return t.leaf === id ? { dir, a: t, b: { leaf: newId }, ratio: 0.5 } : t;
  }
  return { ...t, a: splitLeaf(t.a, id, dir, newId), b: splitLeaf(t.b, id, dir, newId) };
}

/** Removes leaf `id`; its sibling takes the space. `null` when it was the last pane. */
function removeLeaf(t, id) {
  if (t.leaf !== undefined) return t.leaf === id ? null : t;
  const a = removeLeaf(t.a, id), b = removeLeaf(t.b, id);
  if (a === null) return b;
  if (b === null) return a;
  return { ...t, a, b };
}

/** Pixel (or cell) rectangles for every pane inside `rect` {x,y,w,h}. */
function layoutRects(t, rect, out = {}) {
  if (t.leaf !== undefined) { out[t.leaf] = rect; return out; }
  const r = t.ratio;
  if (t.dir === 'h') {
    const w = rect.w * r;
    layoutRects(t.a, { x: rect.x, y: rect.y, w, h: rect.h }, out);
    layoutRects(t.b, { x: rect.x + w, y: rect.y, w: rect.w - w, h: rect.h }, out);
  } else {
    const h = rect.h * r;
    layoutRects(t.a, { x: rect.x, y: rect.y, w: rect.w, h }, out);
    layoutRects(t.b, { x: rect.x, y: rect.y + h, w: rect.w, h: rect.h - h }, out);
  }
  return out;
}

/** The pane next to `from` in `direction` ('left'|'right'|'up'|'down'), the nearest that overlaps it. */
function neighbour(rects, from, direction) {
  const me = rects[from];
  if (!me) return null;
  let best = null, bestDist = Infinity;
  for (const [id, r] of Object.entries(rects)) {
    if (id === String(from)) continue;
    const overlapY = Math.min(me.y + me.h, r.y + r.h) - Math.max(me.y, r.y) > 1e-9;
    const overlapX = Math.min(me.x + me.w, r.x + r.w) - Math.max(me.x, r.x) > 1e-9;
    let dist = null;
    if (direction === 'right' && overlapY && r.x >= me.x + me.w - 1e-9) dist = r.x - (me.x + me.w);
    if (direction === 'left' && overlapY && r.x + r.w <= me.x + 1e-9) dist = me.x - (r.x + r.w);
    if (direction === 'down' && overlapX && r.y >= me.y + me.h - 1e-9) dist = r.y - (me.y + me.h);
    if (direction === 'up' && overlapX && r.y + r.h <= me.y + 1e-9) dist = me.y - (r.y + r.h);
    if (dist !== null && dist < bestDist) { best = id; bestDist = dist; }
  }
  return best;
}

/** Tab title: the shell's own title (OSC 0/2), trimmed, else a fallback. */
function tabTitle(title, fallback, max) {
  const t = String(title || '').replace(/\s+/g, ' ').trim() || fallback;
  return t.length > max ? t.slice(0, Math.max(1, max - 1)) + '…' : t;
}

/** HH:MM:SS for the status line. */
function clock(d) {
  const p = (n) => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** What a key press means on this page, or null to leave it to the shell. `mac` picks ⌘ over Ctrl+Shift. */
function shortcut(e, mac) {
  const k = String(e.key || '').toLowerCase();
  const meta = mac ? e.metaKey : e.ctrlKey && e.shiftKey;
  if (mac ? e.metaKey && e.ctrlKey : e.ctrlKey && e.shiftKey && e.altKey) {
    const dir = { h: 'left', j: 'down', k: 'up', l: 'right' }[k];
    if (dir) return { action: 'focus', dir };
  }
  if (e.ctrlKey && e.shiftKey && e.altKey) {
    if (e.key === '"' || e.key === "'") return { action: 'split', dir: 'v' };
    if (e.key === '%' || e.key === '5') return { action: 'split', dir: 'h' };
  }
  if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey) {
    if (k === 'x') return { action: 'copy-mode' };
    if (e.key === ' ') return { action: 'quick-select' };
  }
  if (meta && !e.altKey && !(mac && e.ctrlKey)) {
    if (e.key === 'ArrowUp') return { action: 'prompt', dir: -1 };
    if (e.key === 'ArrowDown') return { action: 'prompt', dir: 1 };
    if (k === 'f' && !e.shiftKey) return { action: 'search' };
    if (k === 'p' && e.shiftKey) return { action: 'palette' };
  }
  if (meta && !e.altKey && !(mac && e.ctrlKey)) {
    if (k === 't') return { action: 'new-tab' };
    if (k === 'w') return { action: 'close' };
    if (/^[1-9]$/.test(k)) return { action: 'tab', index: Number(k) - 1 };
    if (e.key === '=' || e.key === '+') return { action: 'font', delta: 1 };
    if (e.key === '-') return { action: 'font', delta: -1 };
    if (e.key === '0') return { action: 'font', delta: 0 };
    if (k === 'd') return { action: 'split', dir: e.shiftKey ? 'v' : 'h' };
    if (k === 'k') return { action: 'clear' };
  }
  return null;
}

// ---- quick select: what is worth copying on screen, and the letters that pick it ----

// In priority order: when two patterns match the same text the earlier one wins.
const QUICK_PATTERNS = [
  ['url', /\bhttps?:\/\/[^\s<>"'`)\]]+/gi],
  ['uuid', /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/gi],
  ['ipv4', /\b(?:\d{1,3}\.){3}\d{1,3}(?::\d+)?\b/g],
  ['path', /(?<![\w.@+~\/-])(?:~|\.{1,2})?\/[\w.@+~-]+(?:\/[\w.@+~-]+)*\/?/g],
  ['hash', /\b(?=[0-9a-f]*\d)(?=[0-9a-f]*[a-f])[0-9a-f]{7,64}\b/gi],
  ['number', /\b\d{4,}\b/g],
];
const HINT_KEYS = 'asdfqwerzxcvjklmiuopghtybn';

/** Matches in `lines` (array of strings, one per screen row): [{row, col, len, text, kind}] in reading order. */
function quickMatches(lines) {
  const out = [];
  lines.forEach((line, row) => {
    const taken = [];
    for (const [kind, re] of QUICK_PATTERNS) {
      re.lastIndex = 0;
      let m;
      while ((m = re.exec(line)) !== null) {
        let text = m[0];
        if (kind === 'url') text = text.replace(/[.,;:!?]+$/, '');
        if (!text) { re.lastIndex++; continue; }
        const col = m.index, end = col + text.length;
        // a path that is only "/" or part of a longer match found earlier is not a new match
        if (kind === 'path' && text.length < 2) continue;
        if (taken.some(([a, b]) => col < b && end > a)) continue;
        taken.push([col, end]);
        out.push({ row, col, len: text.length, text, kind });
      }
    }
  });
  return out.sort((a, b) => a.row - b.row || a.col - b.col);
}

/** `n` distinct prefix-free labels: one letter each when they fit, else two letters each (at most 676). */
function hintLabels(n) {
  const k = HINT_KEYS.length;
  if (n <= k) return [...HINT_KEYS].slice(0, n);
  const out = [];
  for (const a of HINT_KEYS) for (const b of HINT_KEYS) { if (out.length < n) out.push(a + b); }
  return out;
}

// ---- copy mode: vi-like motions over the buffer (lines are strings, rows are absolute) ----

const charClass = (c) => (c === undefined || /\s/.test(c) ? 0 : /\w/.test(c) ? 1 : 2);

/** The state after one key. `st` is {row, col, mode: null|'char'|'line', anchor}; `lines` the whole buffer. */
function copyMove(st, key, lines) {
  const last = Math.max(0, lines.length - 1);
  const s = { ...st };
  const len = (r) => Math.max(0, lines[r].length);
  const clampCol = () => { s.col = Math.max(0, Math.min(s.col, Math.max(0, len(s.row) - 1))); };
  switch (key) {
    case 'h': case 'ArrowLeft': s.col = Math.max(0, s.col - 1); break;
    case 'l': case 'ArrowRight': s.col += 1; clampCol(); break;
    case 'j': case 'ArrowDown': s.row = Math.min(last, s.row + 1); clampCol(); break;
    case 'k': case 'ArrowUp': s.row = Math.max(0, s.row - 1); clampCol(); break;
    case '0': s.col = 0; break;
    case '$': s.col = Math.max(0, len(s.row) - 1); break;
    case 'g': s.row = 0; s.col = 0; break;
    case 'G': s.row = last; s.col = 0; break;
    case 'w': {
      let { row, col } = s;
      const cls = charClass(lines[row][col]);
      while (col < len(row) && cls !== 0 && charClass(lines[row][col]) === cls) col++;
      for (;;) {
        while (col < len(row) && charClass(lines[row][col]) === 0) col++;
        if (col < len(row) || row === last) break;
        row++; col = 0;
        if (len(row) === 0) break;   // like vi, an empty line is a stop
      }
      s.row = row; s.col = col; clampCol();
      break;
    }
    case 'b': {
      let { row, col } = s;
      const step = () => {
        if (col > 0) { col--; return true; }
        if (row === 0) return false;
        row--; col = Math.max(0, len(row) - 1);
        return true;
      };
      if (!step()) break;
      while (charClass(lines[row][col]) === 0 && step()) { /* skip space */ }
      const cls = charClass(lines[row][col]);
      while (col > 0 && charClass(lines[row][col - 1]) === cls) col--;
      s.row = row; s.col = col;
      break;
    }
    case 'v': s.mode = s.mode === 'char' ? null : 'char'; s.anchor = { row: s.row, col: s.col }; break;
    case 'V': s.mode = s.mode === 'line' ? null : 'line'; s.anchor = { row: s.row, col: s.col }; break;
    default: break;
  }
  return s;
}

/** The text a yank takes: the selection between anchor and cursor, or '' outside visual mode. */
function copyText(st, lines) {
  if (!st.mode || !st.anchor) return '';
  const [a, b] = [st.anchor, { row: st.row, col: st.col }]
    .sort((x, y) => x.row - y.row || x.col - y.col);
  if (st.mode === 'line') return lines.slice(a.row, b.row + 1).join('\n');
  if (a.row === b.row) return lines[a.row].slice(a.col, b.col + 1);
  return [lines[a.row].slice(a.col), ...lines.slice(a.row + 1, b.row), lines[b.row].slice(0, b.col + 1)].join('\n');
}

/** The prompt line to jump to from `current`: nearest mark above (dir -1) or below (dir 1), else null. */
function promptJump(marks, current, dir) {
  const ok = marks.filter((l) => Number.isInteger(l) && l >= 0).sort((a, b) => a - b);
  if (dir < 0) { const c = ok.filter((l) => l < current); return c.length ? c[c.length - 1] : null; }
  const c = ok.filter((l) => l > current);
  return c.length ? c[0] : null;
}

// ---- command palette: every action by name, narrowed by what was typed ----

const ACTIONS = [
  { title: 'New tab', keys: '⌘T', run: { action: 'new-tab' } },
  { title: 'Close pane', keys: '⌘W', run: { action: 'close' } },
  { title: 'Split pane right', keys: '⌘D', run: { action: 'split', dir: 'h' } },
  { title: 'Split pane down', keys: '⌘⇧D', run: { action: 'split', dir: 'v' } },
  { title: 'Search scrollback', keys: '⌘F', run: { action: 'search' } },
  { title: 'Copy mode', keys: '⌃⇧X', run: { action: 'copy-mode' } },
  { title: 'Quick select', keys: '⌃⇧Space', run: { action: 'quick-select' } },
  { title: 'Previous prompt', keys: '⌘↑', run: { action: 'prompt', dir: -1 } },
  { title: 'Next prompt', keys: '⌘↓', run: { action: 'prompt', dir: 1 } },
  { title: 'Clear scrollback', keys: '⌘K', run: { action: 'clear' } },
  { title: 'Larger text', keys: '⌘+', run: { action: 'font', delta: 1 } },
  { title: 'Smaller text', keys: '⌘−', run: { action: 'font', delta: -1 } },
  { title: 'Reset text size', keys: '⌘0', run: { action: 'font', delta: 0 } },
  ...Object.keys(THEMES).map((name) => ({ title: 'Theme: ' + name, keys: '', run: { action: 'theme', name } })),
];

/** Actions whose title contains every word of `query` (case-insensitive); all of them for an empty query. */
function filterActions(items, query) {
  const words = String(query || '').toLowerCase().split(/\s+/).filter(Boolean);
  return items.filter((a) => words.every((w) => a.title.toLowerCase().includes(w)));
}

if (typeof module !== 'undefined' && module.exports) {
  module.exports = {
    THEMES, FONT_STACK, pickTheme, contrast, chromeFor, ptyUrl, exitCode, clampSize, DEFAULT_DARK, DEFAULT_LIGHT,
    leaves, splitLeaf, removeLeaf, layoutRects, neighbour, tabTitle, clock, shortcut,
    quickMatches, hintLabels, copyMove, copyText, promptJump, ACTIONS, filterActions,
  };
}

if (typeof window !== 'undefined' && typeof document !== 'undefined' && document.getElementById('stage')) {
  (function page() {
    const $ = (id) => document.getElementById(id);
    const store = {
      get(k) { try { return localStorage.getItem(k); } catch { return null; } },
      set(k, v) { try { localStorage.setItem(k, v); } catch { /* private mode */ } },
    };
    const token = () => sessionStorage.getItem(TOKEN_KEY) || '';
    const sessionId = new URLSearchParams(location.search).get('session') || '';
    const mac = /Mac/i.test(navigator.platform || navigator.userAgent || '');

    async function api(method, path, body) {
      const headers = {};
      if (token()) headers.Authorization = 'Bearer ' + token();
      if (body !== undefined) headers['Content-Type'] = 'application/json';
      const r = await fetch(path, { method, headers, cache: 'no-store', body: body === undefined ? undefined : JSON.stringify(body) });
      if (!r.ok) {
        let msg = r.status + ' ' + r.statusText;
        try { const j = await r.json(); msg = j.error || j.message || msg; } catch { /* no body */ }
        const err = new Error(msg); err.status = r.status; throw err;
      }
      return r.json();
    }

    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    let themeName = pickTheme(store.get(THEME_KEY), mq.matches);
    let fontSize = clampSize(store.get(SIZE_KEY) || 16);

    // ---- state: tabs, each with a layout tree of panes ----
    let tabs = [];            // {id, tree, panes: Map(id -> Pane), focus, title}
    let active = null;        // the visible tab
    let nextId = 1;
    const newId = () => nextId++;

    const show = (which) => { for (const id of ['login', 'picker', 'workspace']) $(id).hidden = id !== which; };
    const status = (text) => { $('left-status').textContent = text; };

    // ---- theme / chrome ----
    function applyTheme() {
      const th = THEMES[themeName], c = chromeFor(th);
      const root = document.documentElement.style;
      document.documentElement.dataset.scheme = th.dark ? 'dark' : 'light';
      const set = (k, v) => root.setProperty(k, v);
      set('--bg', th.background); set('--fg', th.foreground); set('--accent', c.accent);
      set('--titlebar', c.titlebar); set('--tabbar', c.tabBarBg);
      set('--tab-active-bg', c.tabActiveBg); set('--tab-active-fg', c.tabActiveFg);
      set('--tab-bg', c.tabInactiveBg); set('--tab-fg', c.tabInactiveFg);
      set('--tab-hover-bg', c.tabHoverBg); set('--tab-hover-fg', c.tabHoverFg); set('--new-tab-fg', c.newTabFg);
      set('--pane-dim', `saturate(${c.paneDim.saturation}) brightness(${c.paneDim.brightness})`);
      set('--opacity', String(c.opacity));
      set('--window-bg', c.gradient
        ? `linear-gradient(${c.gradientAngle}deg, ${c.gradient.join(', ')})`
        : th.background);
      for (const t of tabs) for (const p of t.panes.values()) p.term.options.theme = xtermTheme(th);
    }
    // With a gradient behind the terminal the cell background must not paint over it.
    const xtermTheme = (th) => ({ ...th, background: chromeFor(th).gradient ? 'rgba(0,0,0,0)' : th.background });

    function setSize(n) {
      fontSize = n === 0 ? 16 : clampSize(n);
      store.set(SIZE_KEY, String(fontSize));
      for (const t of tabs) for (const p of t.panes.values()) { p.term.options.fontSize = fontSize; }
      fitAll();
    }

    // ---- one pane: an xterm.js terminal and its WebSocket ----
    function makePane(tab) {
      const th = THEMES[themeName];
      const el = document.createElement('div');
      el.className = 'pane';
      const inner = document.createElement('div');
      inner.className = 'pane-inner';
      el.appendChild(inner);
      const term = new Terminal({
        fontFamily: FONT_STACK, fontSize, scrollback: 15000, macOptionIsMeta: true, allowProposedApi: true,
        allowTransparency: true, theme: xtermTheme(th), rightClickSelectsWord: true,
        cursorStyle: 'bar', cursorWidth: 2, cursorBlink: true, drawBoldTextInBrightColors: true,
        lineHeight: 1.0, letterSpacing: 0,
      });
      const fit = new FitAddon.FitAddon();
      term.loadAddon(fit);
      term.loadAddon(new Unicode11Addon.Unicode11Addon());
      term.unicode.activeVersion = '11';
      term.loadAddon(new WebLinksAddon.WebLinksAddon());
      term.loadAddon(new ImageAddon.ImageAddon());
      term.loadAddon(new ClipboardAddon.ClipboardAddon());
      const search = new SearchAddon.SearchAddon();
      term.loadAddon(search);
      const pane = { id: newId(), el, inner, term, fit, search, ws: null, tab, closing: false, exited: null, marks: [], lastExit: null };
      term.open(inner);
      try {
        const gl = new WebglAddon.WebglAddon();
        gl.onContextLoss(() => gl.dispose());
        term.loadAddon(gl);
        window.__terminalRenderer = 'WebGL';
      } catch { window.__terminalRenderer = 'DOM'; }
      el.addEventListener('mousedown', () => focusPane(tab, pane.id));
      term.onBell(() => { el.classList.add('bell'); setTimeout(() => el.classList.remove('bell'), 220); });
      term.onTitleChange((t) => { pane.title = t; renderTabs(); });
      // OSC 133 prompt marks: A = a prompt starts (remember its line), D;<n> = a command finished.
      term.parser.registerOscHandler(133, (data) => {
        const [kind, arg] = data.split(';');
        if (kind === 'A') {
          const m = term.registerMarker(0);
          if (m) {
            pane.marks.push(m);
            m.onDispose(() => { const i = pane.marks.indexOf(m); if (i >= 0) pane.marks.splice(i, 1); });
            const d = term.registerDecoration({ marker: m, x: 0, width: 1, layer: 'top' });
            if (d) d.onRender((node) => node.classList.add('prompt-mark'));
          }
        } else if (kind === 'D' && arg !== undefined && /^-?\d+$/.test(arg)) {
          pane.lastExit = Number(arg);
          updateStatus();
        }
        return true;
      });
      term.attachCustomKeyEventHandler((e) => {
        if (e.type !== 'keydown') return true;
        if (modal && modal.pane === pane) { e.preventDefault(); modalKey(e); return false; }
        const s = shortcut(e, mac);
        if (!s) return true;
        e.preventDefault();
        runAction(s);
        return false;
      });
      return pane;
    }

    async function connectPane(pane) {
      const { term } = pane;
      let ticket;
      try {
        ticket = (await api('POST', `/v1/agents/sessions/${encodeURIComponent(sessionId)}/pty-ticket`, {})).ticket;
      } catch (e) {
        if (e.status === 401) { needToken(); return; }
        term.write(`\r\n\x1b[31m${e.message}\x1b[0m\r\n`);
        pane.exited = 'error';
        renderTabs();
        return;
      }
      const ws = new WebSocket(ptyUrl(location, ticket, term.cols, term.rows));
      pane.ws = ws;
      ws.binaryType = 'arraybuffer';
      const enc = new TextEncoder();
      ws.onopen = () => updateStatus();
      ws.onmessage = (e) => term.write(typeof e.data === 'string' ? e.data : new Uint8Array(e.data));
      ws.onclose = (e) => {
        if (pane.closing) return;
        const code = exitCode(e.reason);
        pane.exited = code === null ? 'closed' : String(code);
        term.write(`\r\n\x1b[2m[${code === null ? 'connection closed' : 'exited ' + code}] press Enter to close\x1b[0m\r\n`);
        renderTabs();
      };
      term.onData((d) => {
        if (pane.exited !== null) { if (d === '\r') closePane(pane.tab, pane.id); return; }
        if (ws.readyState === 1) ws.send(enc.encode(d));
      });
      term.onBinary((d) => { if (ws.readyState === 1) ws.send(Uint8Array.from(d, (c) => c.charCodeAt(0))); });
      term.onResize(({ cols, rows }) => {
        if (ws.readyState === 1) ws.send(JSON.stringify({ resize: { cols, rows } }));
        updateStatus();
      });
    }

    // ---- tabs and layout ----
    function newTab() {
      const tab = { id: newId(), tree: null, panes: new Map(), focus: null };
      const pane = makePane(tab);
      tab.panes.set(pane.id, pane);
      tab.tree = { leaf: pane.id };
      tab.focus = pane.id;
      tabs.push(tab);
      switchTab(tab);
      connectPane(pane);
      return tab;
    }

    function switchTab(tab) {
      active = tab;
      renderLayout();
      renderTabs();
      requestAnimationFrame(() => { fitAll(); focusPane(tab, tab.focus); });
    }

    function renderLayout() {
      const stage = $('panes');
      stage.textContent = '';
      for (const t of tabs) for (const p of t.panes.values()) if (t !== active) p.el.remove();
      const build = (node) => {
        if (node.leaf !== undefined) return active.panes.get(node.leaf).el;
        const box = document.createElement('div');
        box.className = 'split ' + (node.dir === 'h' ? 'row' : 'col');
        const a = build(node.a), b = build(node.b);
        a.style.flex = `${node.ratio} 1 0`; b.style.flex = `${1 - node.ratio} 1 0`;
        box.append(a, b);
        return box;
      };
      stage.appendChild(build(active.tree));
      for (const p of active.panes.values()) p.el.classList.toggle('inactive', p.id !== active.focus);
    }

    function focusPane(tab, id) {
      if (!tab.panes.has(id)) return;
      tab.focus = id;
      for (const p of tab.panes.values()) p.el.classList.toggle('inactive', p.id !== id);
      const p = tab.panes.get(id);
      window.__fluxTerm = p.term;
      window.__fluxMarks = () => p.marks.map((m) => m.line);
      p.term.focus();
      updateStatus();
    }

    function split(dir) {
      const tab = active, cur = tab.panes.get(tab.focus);
      const pane = makePane(tab);
      tab.panes.set(pane.id, pane);
      tab.tree = splitLeaf(tab.tree, cur.id, dir, pane.id);
      renderLayout();
      requestAnimationFrame(() => { fitAll(); focusPane(tab, pane.id); });
      connectPane(pane);
    }

    function closePane(tab, id) {
      const pane = tab.panes.get(id);
      if (!pane) return;
      pane.closing = true;
      try { pane.ws && pane.ws.close(); } catch { /* closed */ }
      pane.term.dispose();
      pane.el.remove();
      tab.panes.delete(id);
      tab.tree = removeLeaf(tab.tree, id);
      if (tab.tree === null) {
        tabs = tabs.filter((t) => t !== tab);
        if (!tabs.length) { newTab(); return; }
        switchTab(tabs[Math.max(0, tabs.length - 1)]);
        return;
      }
      if (tab.focus === id) tab.focus = leaves(tab.tree)[0];
      if (tab === active) { renderLayout(); requestAnimationFrame(() => { fitAll(); focusPane(tab, tab.focus); }); }
      renderTabs();
    }

    function fitAll() {
      if (!active) return;
      for (const p of active.panes.values()) { try { p.fit.fit(); } catch { /* hidden */ } }
    }

    function runAction(a) {
      if (a.action === 'new-tab') newTab();
      else if (a.action === 'close') closePane(active, active.focus);
      else if (a.action === 'tab') { if (tabs[a.index]) switchTab(tabs[a.index]); }
      else if (a.action === 'split') split(a.dir);
      else if (a.action === 'font') setSize(a.delta === 0 ? 0 : fontSize + a.delta);
      else if (a.action === 'clear') { const p = active.panes.get(active.focus); p.term.clear(); }
      else if (a.action === 'theme') { themeName = a.name; sel.value = a.name; store.set(THEME_KEY, a.name); applyTheme(); }
      else if (a.action === 'search') openSearch();
      else if (a.action === 'copy-mode') startCopyMode(active.panes.get(active.focus));
      else if (a.action === 'quick-select') startQuickSelect(active.panes.get(active.focus));
      else if (a.action === 'palette') openPalette();
      else if (a.action === 'prompt') {
        const p = active.panes.get(active.focus);
        const to = promptJump(p.marks.map((m) => m.line), p.term.buffer.active.viewportY, a.dir);
        if (to !== null) p.term.scrollToLine(to);
      }
      else if (a.action === 'focus') {
        const host = $('panes').getBoundingClientRect();
        const rects = {};
        for (const p of active.panes.values()) {
          const r = p.el.getBoundingClientRect();
          rects[p.id] = { x: r.x - host.x, y: r.y - host.y, w: r.width, h: r.height };
        }
        const to = neighbour(rects, active.focus, a.dir);
        if (to !== null) focusPane(active, Number(to));
      }
    }

    // ---- modes that take the keyboard: copy mode and quick select ----
    let modal = null;   // {type: 'copy'|'quick', pane, ...}
    const bufferLines = (term) => {
      const b = term.buffer.active, out = [];
      for (let i = 0; i < b.length; i++) { const l = b.getLine(i); out.push(l ? l.translateToString(true) : ''); }
      return out;
    };
    function copyToClipboard(text) {
      const done = () => status(`copied ${text.length} characters`);
      const fallback = () => {
        const ta = document.createElement('textarea');
        ta.value = text; ta.style.position = 'fixed'; ta.style.opacity = '0';
        document.body.appendChild(ta); ta.select();
        try { document.execCommand('copy'); done(); } catch { status('copy was refused by the browser'); }
        ta.remove();
      };
      if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(text).then(done, fallback);
      else fallback();
    }
    function endModal() {
      if (!modal) return;
      const { pane } = modal;
      pane.term.clearSelection();
      if (modal.layer) modal.layer.remove();
      modal = null;
      updateStatus();
      pane.term.focus();
    }
    function modalKey(e) {
      if (e.key === 'Escape' || (e.key === 'c' && e.ctrlKey)) { endModal(); return; }
      if (['Shift', 'Control', 'Alt', 'Meta'].includes(e.key)) return;
      if (modal.type === 'copy') copyKey(e);
      else quickKey(e);
    }

    function startCopyMode(pane) {
      if (!pane || modal) return;
      const b = pane.term.buffer.active;
      modal = { type: 'copy', pane, st: { row: b.baseY + b.cursorY, col: b.cursorX, mode: null, anchor: null } };
      drawCopy();
    }
    function drawCopy() {
      const { pane, st } = modal, term = pane.term;
      if (!st.mode) term.select(st.col, st.row, 1);
      else {
        const [a, c] = [st.anchor, { row: st.row, col: st.col }].sort((x, y) => x.row - y.row || x.col - y.col);
        if (st.mode === 'line') term.selectLines(a.row, c.row);
        else term.select(a.col, a.row, (c.row - a.row) * term.cols + (c.col - a.col) + 1);
      }
      const top = term.buffer.active.viewportY;
      if (st.row < top || st.row >= top + term.rows) term.scrollToLine(Math.max(0, st.row - Math.floor(term.rows / 2)));
      status(`COPY MODE${st.mode ? ' · ' + (st.mode === 'line' ? 'LINE' : 'VISUAL') : ''} · hjkl w b 0 $ g G · v V · y copy · Esc`);
    }
    function copyKey(e) {
      const lines = bufferLines(modal.pane.term);
      if (e.key === 'y' || e.key === 'Enter') {
        const text = copyText(modal.st, lines);
        endModal();
        if (text) copyToClipboard(text);
        return;
      }
      if (e.key === '/') { const p = modal.pane; endModal(); openSearch(p); return; }
      const key = e.key.length === 1 || e.key.startsWith('Arrow') ? e.key : '';
      if (key) { modal.st = copyMove(modal.st, key, lines); drawCopy(); }
    }

    function startQuickSelect(pane) {
      if (!pane || modal) return;
      const term = pane.term, b = term.buffer.active;
      const rows = [];
      for (let r = 0; r < term.rows; r++) { const l = b.getLine(b.viewportY + r); rows.push(l ? l.translateToString(true) : ''); }
      const found = quickMatches(rows);
      if (!found.length) { status('quick select: nothing to pick on screen'); return; }
      const labels = hintLabels(found.length);
      const layer = document.createElement('div');
      layer.className = 'quick-layer';
      const screen = pane.inner.querySelector('.xterm-screen') || pane.inner;
      const sr = screen.getBoundingClientRect(), pr = pane.el.getBoundingClientRect();
      const cw = sr.width / term.cols, ch = sr.height / term.rows;
      found.slice(0, labels.length).forEach((m, i) => {
        const box = document.createElement('span');
        box.className = 'quick-hit';
        box.style.left = (sr.left - pr.left + m.col * cw) + 'px';
        box.style.top = (sr.top - pr.top + m.row * ch) + 'px';
        box.style.width = (m.len * cw) + 'px'; box.style.height = ch + 'px';
        const tag = document.createElement('b');
        tag.textContent = labels[i];
        box.appendChild(tag);
        layer.appendChild(box);
        m.label = labels[i];
      });
      pane.el.appendChild(layer);
      modal = { type: 'quick', pane, layer, found: found.slice(0, labels.length), typed: '' };
      status(`QUICK SELECT · ${modal.found.length} matches · type a label to copy · Shift opens a URL · Esc`);
    }
    function quickKey(e) {
      if (e.key.length !== 1) return;
      const typed = modal.typed + e.key.toLowerCase();
      const hit = modal.found.find((m) => m.label === typed);
      if (hit) {
        const open = e.shiftKey && hit.kind === 'url';
        endModal();
        if (open) window.open(hit.text, '_blank', 'noopener,noreferrer');
        else copyToClipboard(hit.text);
        return;
      }
      if (modal.found.some((m) => m.label.startsWith(typed))) {
        modal.typed = typed;
        for (const node of modal.layer.children) {
          const lab = node.firstChild.textContent;
          node.hidden = !lab.startsWith(typed);
        }
      } else endModal();
    }

    // ---- search bar (per active pane) and command palette ----
    function openSearch(p) {
      const pane = p || active.panes.get(active.focus);
      $('searchbar').hidden = false;
      $('searchbar').dataset.pane = String(pane.id);
      $('searchq').focus();
      $('searchq').select();
    }
    function closeSearch() {
      $('searchbar').hidden = true;
      const id = Number($('searchbar').dataset.pane);
      const pane = active && active.panes.get(id);
      if (pane) { pane.search.clearDecorations(); pane.term.focus(); }
    }
    const searchOpts = {
      incremental: false,
      decorations: {
        matchBackground: '#3b4261', matchBorder: '#7aa2f7', matchOverviewRuler: '#7aa2f7',
        activeMatchBackground: '#e0af68', activeMatchBorder: '#e0af68', activeMatchColorOverviewRuler: '#e0af68',
      },
    };
    function searchStep(back) {
      const pane = active.panes.get(Number($('searchbar').dataset.pane));
      const q = $('searchq').value;
      if (!pane || !q) return;
      const found = back ? pane.search.findPrevious(q, searchOpts) : pane.search.findNext(q, searchOpts);
      $('searchcount').textContent = found ? '' : 'no match';
    }
    $('searchq').addEventListener('keydown', (e) => {
      if (e.key === 'Escape') closeSearch();
      else if (e.key === 'Enter') { e.preventDefault(); searchStep(e.shiftKey); }
    });
    $('searchq').addEventListener('input', () => searchStep(false));
    $('searchprev').addEventListener('click', () => searchStep(true));
    $('searchnext').addEventListener('click', () => searchStep(false));
    $('searchclose').addEventListener('click', closeSearch);

    let paletteSel = 0, paletteItems = [];
    function renderPalette() {
      paletteItems = filterActions(ACTIONS, $('paletteq').value);
      paletteSel = Math.min(paletteSel, Math.max(0, paletteItems.length - 1));
      const box = $('paletteitems');
      box.textContent = '';
      paletteItems.forEach((a, i) => {
        const row = document.createElement('div');
        row.className = 'palette-row' + (i === paletteSel ? ' sel' : '');
        const t = document.createElement('span'); t.textContent = a.title;
        const k = document.createElement('kbd'); k.textContent = a.keys;
        row.append(t, k);
        row.addEventListener('click', () => runPalette(a));
        box.appendChild(row);
      });
    }
    function openPalette() { $('palette').hidden = false; $('paletteq').value = ''; paletteSel = 0; renderPalette(); $('paletteq').focus(); }
    function closePalette() { $('palette').hidden = true; if (active) focusPane(active, active.focus); }
    function runPalette(a) { closePalette(); runAction(a.run); }
    $('paletteq').addEventListener('input', () => { paletteSel = 0; renderPalette(); });
    $('paletteq').addEventListener('keydown', (e) => {
      if (e.key === 'Escape') closePalette();
      else if (e.key === 'ArrowDown') { e.preventDefault(); paletteSel = Math.min(paletteItems.length - 1, paletteSel + 1); renderPalette(); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); paletteSel = Math.max(0, paletteSel - 1); renderPalette(); }
      else if (e.key === 'Enter' && paletteItems[paletteSel]) { e.preventDefault(); runPalette(paletteItems[paletteSel]); }
    });
    // The shortcuts also work while focus is in the page chrome (search box, palette), not only in a pane.
    document.addEventListener('keydown', (e) => {
      if (e.target && e.target.closest && e.target.closest('.xterm')) return;
      const s = shortcut(e, mac);
      if (s && (s.action === 'palette' || s.action === 'search')) { e.preventDefault(); runAction(s); }
    });

    // ---- tab bar (bottom) and status ----
    function renderTabs() {
      const bar = $('tabs');
      bar.textContent = '';
      tabs.forEach((t, i) => {
        const p = t.panes.get(t.focus);
        const b = document.createElement('button');
        b.className = 'tab' + (t === active ? ' active' : '');
        const done = p && p.exited !== null;
        const idx = document.createElement('span'); idx.className = 'idx'; idx.textContent = String(i + 1);
        const dot = document.createElement('span'); dot.className = 'dot'; dot.textContent = t === active ? '●' : '○';
        const title = document.createElement('span'); title.className = 'title';
        title.textContent = tabTitle(p && p.title, sessionId.slice(0, 8), 24);
        b.append(idx, dot, title);
        if (done) b.classList.add('done');
        b.addEventListener('click', () => switchTab(t));
        bar.appendChild(b);
      });
      const plus = document.createElement('button');
      plus.className = 'newtab'; plus.textContent = '+'; plus.title = 'New tab'; plus.setAttribute('aria-label', 'New tab');
      plus.addEventListener('click', () => newTab());
      bar.appendChild(plus);
    }

    function updateStatus() {
      if (!active) return;
      const p = active.panes.get(active.focus);
      if (modal) return;
      const exit = p && p.lastExit !== null && p.lastExit !== 0 ? ` · last exit ${p.lastExit}` : '';
      if (p) status(`session ${sessionId.slice(0, 8)} · ${p.term.cols}×${p.term.rows} · ${window.__terminalRenderer || ''}${exit}`);
    }
    setInterval(() => { $('clock').textContent = '🕒 ' + clock(new Date()); }, 1000);
    $('clock').textContent = '🕒 ' + clock(new Date());

    // ---- controls ----
    const sel = $('theme');
    for (const name of Object.keys(THEMES)) {
      const o = document.createElement('option'); o.value = name; o.textContent = name; sel.appendChild(o);
    }
    sel.value = themeName;
    sel.addEventListener('change', () => { themeName = sel.value; store.set(THEME_KEY, themeName); applyTheme(); });
    $('smaller').addEventListener('click', () => setSize(fontSize - 1));
    $('larger').addEventListener('click', () => setSize(fontSize + 1));
    new ResizeObserver(() => fitAll()).observe($('panes'));

    function needToken() { show('login'); status('sign in to open a terminal'); }
    $('signin').addEventListener('click', () => {
      sessionStorage.setItem(TOKEN_KEY, $('token').value.trim());
      $('token').value = '';
      start();
    });
    $('token').addEventListener('keydown', (e) => { if (e.key === 'Enter') $('signin').click(); });

    async function pick() {
      show('picker');
      status('choose a session');
      $('pickerr').textContent = '';
      let list;
      try { list = await api('GET', '/v1/agents/sessions'); } catch (e) {
        if (e.status === 401) return needToken();
        $('pickerr').textContent = e.message;
        return;
      }
      const items = (list.items || list || []).filter((s) => s.state === 'running');
      const box = $('sessions');
      box.textContent = '';
      if (!items.length) { box.textContent = 'No running sessions. Create one with `fluxctl agent run`.'; return; }
      for (const s of items) {
        const b = document.createElement('button');
        const policy = (s.policy && s.policy.preset) || 'open';
        b.innerHTML = '<b></b><small></small>';
        b.firstChild.textContent = `${s.agent} · ${s.id.slice(0, 8)}`;
        b.lastChild.textContent = `${s.backend} · policy ${policy}` + (s.policy && s.policy.process ? ' · has command rules (terminal refused)' : '');
        b.addEventListener('click', () => { location.search = '?session=' + encodeURIComponent(s.id); });
        box.appendChild(b);
      }
    }

    function start() {
      $('title').textContent = sessionId ? 'session ' + sessionId : '';
      applyTheme();
      if (!sessionId) { pick(); return; }
      show('workspace');
      if (!tabs.length) newTab();
    }
    start();
  })();
}
