// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for Studio's global search (Cmd/Ctrl-K): turn what the daemon lists into searchable items, rank them
// against what the person typed, remember recent picks, and say in plain words when nothing is found. The same sets
// and the same order of preference as `fluxctl find QUERY`.

export type Kind = 'machine' | 'image' | 'app' | 'volume' | 'page' | 'action';

export interface Item {
  /** Stable across reloads, so a recent pick can be found again: `machine:ID`, `app:cluster/ns/name`. */
  id: string;
  kind: Kind;
  title: string;
  /** One quiet line under the title: status, image, size. */
  sub: string;
  /** Other words that find it: labels (`key=value`), an address, an image reference, a namespace. */
  terms: string[];
  /** Ports it listens on; typing `8080` or `:8080` finds it. */
  ports: number[];
  /** Hash route to open. */
  to: string;
  /** The fluxctl command that shows the same thing (for an action: what it does). Empty when there is none. */
  cmd: string;
}

export const KIND_LABEL: Record<Kind, string> = { machine: 'Machine', image: 'Image', app: 'App', volume: 'Volume', page: 'Page', action: 'Action' };

/** A tie between two kinds goes to the one people open more often. */
const KIND_WEIGHT: Record<Kind, number> = { machine: 5, app: 4, image: 3, volume: 2, page: 1, action: 0 };

export type Mode = { mode: 'search' | 'actions'; text: string };

/** A leading `>` means actions, as in an editor's palette. */
export function parseQuery(raw: string): Mode {
  const t = raw.trimStart();
  return t.startsWith('>') ? { mode: 'actions', text: t.slice(1).trim() } : { mode: 'search', text: raw.trim() };
}

const lower = (s: string) => s.toLowerCase();

/** Letters of `needle` appear in `hay` in order (a typo-tolerant last resort, only for 3+ letters). */
export function isSubsequence(needle: string, hay: string): boolean {
  let i = 0;
  for (const ch of hay) if (ch === needle[i] && ++i === needle.length) return true;
  return needle.length === 0;
}

/** Start of a word: after a space, dash, dot, slash, colon or underscore, or the start. */
const wordStart = (hay: string, needle: string): boolean => {
  let at = hay.indexOf(needle);
  while (at >= 0) {
    if (at === 0 || /[\s\-._/:@=]/.test(hay[at - 1])) return true;
    at = hay.indexOf(needle, at + 1);
  }
  return false;
};

/** How well one word matches one text: 100 exact, 80 prefix, 60 word start, 40 inside, 15 loosely, 0 not at all. */
export function matchText(term: string, text: string): number {
  const t = lower(term);
  const h = lower(text);
  if (!t || !h) return 0;
  if (h === t) return 100;
  if (h.startsWith(t)) return 80;
  if (wordStart(h, t)) return 60;
  if (h.includes(t)) return 40;
  if (t.length >= 3 && isSubsequence(t, h)) return 15;
  return 0;
}

const portOf = (term: string): number | null => (/^:?\d{1,5}$/.test(term) ? Number(term.replace(':', '')) : null);

/** 0 means no match. Every word typed must match something; the title counts double, other words once. */
export function scoreItem(item: Item, text: string): number {
  const words = text.split(/\s+/).filter(Boolean);
  if (!words.length) return 1;
  let total = 0;
  for (const w of words) {
    const port = portOf(w);
    let best = matchText(w, item.title) * 2;
    for (const term of item.terms) best = Math.max(best, matchText(w, term) * (term.includes('=') ? 1.5 : 1));
    best = Math.max(best, matchText(w, item.sub) * 0.5);
    if (port !== null && item.ports.includes(port)) best = Math.max(best, w.startsWith(':') ? 190 : 150);
    if (best === 0) return 0;
    total += best;
  }
  return total / words.length;
}

export interface Ranked { item: Item; score: number; recent: boolean }

/**
 * The ranked list. With no text, recent picks come first (newest first) and then pages. With text, score decides, a
 * recent pick gets a small lift, and the kind breaks ties; the order is stable for equal scores.
 */
export function rank(items: Item[], text: string, recent: string[], limit = 40): Ranked[] {
  const rec = new Map(recent.map((id, i) => [id, i]));
  if (!text.trim()) {
    const picked = recent.map((id) => items.find((i) => i.id === id)).filter((i): i is Item => !!i);
    const rest = items.filter((i) => !rec.has(i.id));
    return [...picked.map((item) => ({ item, score: 1, recent: true })), ...rest.map((item) => ({ item, score: 1, recent: false }))].slice(0, limit);
  }
  return items
    .map((item, n) => ({ item, n, score: scoreItem(item, text) + (rec.has(item.id) ? 8 : 0), recent: rec.has(item.id) }))
    .filter((r) => r.score > 0)
    .sort((a, b) => b.score - a.score || KIND_WEIGHT[b.item.kind] - KIND_WEIGHT[a.item.kind] || a.n - b.n)
    .slice(0, limit)
    .map(({ item, score, recent: r }) => ({ item, score, recent: r }));
}

export const RECENT_MAX = 6;

/** Newest first, no repeats, at most `RECENT_MAX`. */
export function pushRecent(list: string[], id: string, max = RECENT_MAX): string[] {
  return [id, ...list.filter((x) => x !== id)].slice(0, max);
}

const RECENT_KEY = 'fluxvm.search.recent';

export function loadRecent(store: Pick<Storage, 'getItem'> | undefined = globalThis.localStorage): string[] {
  try {
    const v = JSON.parse(store?.getItem(RECENT_KEY) ?? '[]');
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string').slice(0, RECENT_MAX) : [];
  } catch {
    return [];
  }
}

export function saveRecent(list: string[], store: Pick<Storage, 'setItem'> | undefined = globalThis.localStorage): void {
  try {
    store?.setItem(RECENT_KEY, JSON.stringify(list.slice(0, RECENT_MAX)));
  } catch {
    // a full or blocked store only means recents are not remembered
  }
}

/** Arrow-key movement that stops at both ends. */
export const moveSel = (sel: number, delta: 1 | -1, count: number): number => (count <= 0 ? 0 : Math.min(count - 1, Math.max(0, sel + delta)));

// ---- items from what the daemon lists -----------------------------------------------------------------------------

const str = (v: unknown): string => (typeof v === 'string' ? v : typeof v === 'number' ? String(v) : '');
const enc = encodeURIComponent;

export type Row = Record<string, unknown>;

export function machineItems(vms: { id: string; name: string; status?: string; backend?: string; guest_ip?: string | null; labels?: Record<string, string> }[]): Item[] {
  return vms.map((v) => ({
    id: `machine:${v.id}`,
    kind: 'machine',
    title: v.name,
    sub: [v.status, v.guest_ip, v.backend].filter(Boolean).join(' · '),
    terms: [v.id, v.guest_ip ?? '', ...Object.entries(v.labels ?? {}).map(([k, val]) => `${k}=${val}`)].filter(Boolean),
    ports: [],
    to: `/machines/${enc(v.id)}`,
    cmd: `fluxctl get ${shellWord(v.name)}`,
  }));
}

export function catalogItems(rows: Row[]): Item[] {
  return rows.filter((r) => str(r.name)).map((r) => ({
    id: `image:catalog:${str(r.name)}`,
    kind: 'image',
    title: str(r.name),
    sub: ['catalog', str(r.format), [str(r.distro), str(r.version)].filter(Boolean).join(' ')].filter(Boolean).join(' · '),
    terms: [str(r.distro), str(r.version)].filter(Boolean),
    ports: [],
    to: `/images?tab=catalog&focus=${enc(str(r.name))}`,
    cmd: 'fluxctl catalog list',
  }));
}

export function ociItems(rows: Row[]): Item[] {
  return rows.map((r) => ({ ref: str(r.reference) || str(r.image) || str(r.name), digest: str(r.digest) })).filter((r) => r.ref).map((r) => ({
    id: `image:oci:${r.ref}`,
    kind: 'image' as const,
    title: r.ref,
    sub: ['OCI image', r.digest.replace('sha256:', '').slice(0, 12)].filter(Boolean).join(' · '),
    terms: r.digest ? [r.digest.replace('sha256:', '')] : [],
    ports: [],
    to: `/images?tab=oci&focus=${enc(r.ref)}`,
    cmd: 'fluxctl oci ls',
  }));
}

export function volumeItems(rows: Row[]): Item[] {
  return rows.filter((r) => str(r.name)).map((r) => {
    const used = Array.isArray(r.attached_to) ? (r.attached_to as unknown[]).map(str).filter(Boolean) : [];
    return {
      id: `volume:${str(r.name)}`,
      kind: 'volume' as const,
      title: str(r.name),
      sub: [`${str(r.size_gib)} GiB`, used.length ? `used by ${used.join(', ')}` : 'unused'].join(' · '),
      terms: used,
      ports: [],
      to: `/volumes?focus=${enc(str(r.name))}`,
      cmd: '',
    };
  });
}

export interface AppLike { namespace: string; name: string; image?: string; port?: number | null; summary?: string; state?: string }

export function appItems(cluster: string, apps: AppLike[]): Item[] {
  return apps.map((a) => ({
    id: `app:${cluster}/${a.namespace}/${a.name}`,
    kind: 'app',
    title: a.name,
    sub: [a.summary || a.state, a.image, a.port ? `port ${a.port}` : ''].filter(Boolean).join(' · '),
    terms: [a.image ?? '', a.namespace, cluster].filter(Boolean),
    ports: a.port ? [a.port] : [],
    to: `/apps?cluster=${enc(cluster)}&focus=${enc(a.name)}`,
    cmd: `fluxctl kube apps --cluster ${shellWord(cluster)}`,
  }));
}

export function pageItems(pages: { to: string; label: string; desc?: string }[]): Item[] {
  return pages.map((p) => ({ id: `page:${p.to}`, kind: 'page', title: p.label, sub: p.desc ?? '', terms: ['go to', p.to.replace('/', '')], ports: [], to: p.to, cmd: '' }));
}

/** Quote a word for a shell only when it needs it (names are usually plain). */
export function shellWord(s: string): string {
  return /^[\w./:@=,-]+$/.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`;
}

// ---- actions (`>`) ------------------------------------------------------------------------------------------------

export interface ActionDef { id: string; label: string; cmd: string; to?: string; /** Needs no fluxctl: a Studio-only choice. */ studioOnly?: boolean }

/** Every action says the fluxctl command that does the same, so nothing is a Studio-only trick. */
export const ACTIONS: ActionDef[] = [
  { id: 'create-machine', label: 'Create a machine', cmd: 'fluxctl create NAME', to: '/machines' },
  { id: 'pull-image', label: 'Pull an image', cmd: 'fluxctl oci pull IMAGE', to: '/images?tab=oci' },
  { id: 'create-volume', label: 'Create a volume', cmd: 'POST /v1/volumes {"name": "NAME", "size_gib": 20}', to: '/volumes' },
  { id: 'run-app', label: 'Run an app on the cluster', cmd: 'fluxctl kube run IMAGE --port 8080', to: '/apps' },
  { id: 'check-health', label: 'Check that everything is OK', cmd: 'fluxctl cluster status', to: '/' },
  { id: 'doctor', label: 'Check this host is ready', cmd: 'fluxctl doctor', to: '/host' },
  { id: 'audit', label: 'See who changed what', cmd: 'fluxctl audit ls --since 24h', to: '/audit' },
  { id: 'usage', label: 'See usage and cost', cmd: 'fluxctl usage --by vm', to: '/usage' },
  { id: 'theme', label: 'Toggle light / dark theme', cmd: '', studioOnly: true },
  { id: 'signout', label: 'Sign out', cmd: '', studioOnly: true },
];

export const actionItems = (defs: ActionDef[] = ACTIONS): Item[] =>
  defs.map((a) => ({ id: `action:${a.id}`, kind: 'action', title: a.label, sub: a.studioOnly ? 'Studio only' : a.cmd, terms: a.cmd ? [a.cmd] : [], ports: [], to: a.to ?? '', cmd: a.cmd }));

// ---- words for the empty and error states -------------------------------------------------------------------------

export function emptyMessage(text: string, mode: Mode['mode'], failed: string[]): string {
  const base = mode === 'actions' ? `No action matches “${text}”. Clear the > to search machines, images, apps and volumes.` : text ? `Nothing matches “${text}”. Try part of a name, a label like env=dev, an image, or a port number.` : 'Nothing to show yet. Create a machine, pull an image or run an app, and it will show up here.';
  return failed.length ? `${base} (${failed.join(', ')} could not be read, so they were not searched.)` : base;
}

/** What went wrong reading one set, and what to do. */
export function loadProblem(what: string, e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/401|403|unauthor|forbidden/i.test(m)) return `${what}: your token is not allowed to read this. Sign in again as an admin.`;
  if (/404|not found/i.test(m)) return `${what}: this daemon does not have it (it may be an older release).`;
  if (/failed to fetch|network|ECONN|timeout/i.test(m)) return `${what}: the daemon did not answer. Check that FluxVM is running, then press Cmd/Ctrl-K again.`;
  return `${what}: ${m.slice(0, 100)}`;
}

/** The part of a hash route's query that names the thing to scroll to. */
export function focusOf(query: URLSearchParams): string {
  return query.get('focus') ?? '';
}
