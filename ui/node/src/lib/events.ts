// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/events?source=...`: one chronological feed merged from VM, Mac, Kubernetes, container, backup and
// audit events. The same lines as `fluxctl events --json`.

import type { Tone } from './health';

export interface FeedEvent {
  ts: string;
  source: string;
  event: string;
  summary: string;
  level: 'info' | 'warn' | 'error' | string;
  target?: string;
  vm_id?: string;
  fields?: Record<string, string>;
}

export interface FeedReply {
  items: FeedEvent[];
  sources?: string[];
  /** One line per cluster that could not be read, each saying what to do next. */
  notes?: string[];
}

export const SOURCES: { value: string; label: string; about: string }[] = [
  { value: 'vm', label: 'VMs', about: 'Starts, stops, crashes and refusals' },
  { value: 'mac', label: 'Mac', about: 'Mac VM instances' },
  { value: 'kube', label: 'Kubernetes', about: 'Clusters and nodes' },
  { value: 'docker', label: 'Containers', about: 'Container events' },
  { value: 'backup', label: 'Backups', about: 'Backup and schedule runs' },
  { value: 'k8s', label: 'Cluster events', about: 'Live events from the Kubernetes API of running clusters' },
  { value: 'audit', label: 'Audit', about: 'Who changed what (admin)' },
];

export const SINCE_CHOICES: { value: string; label: string }[] = [
  { value: '', label: 'Any time' },
  { value: '15m', label: 'Last 15 minutes' },
  { value: '1h', label: 'Last hour' },
  { value: '24h', label: 'Last 24 hours' },
  { value: '7d', label: 'Last 7 days' },
];

export const LEVEL_CHOICES: { value: string; label: string }[] = [
  { value: '', label: 'Everything' },
  { value: 'warn', label: 'Warnings and errors' },
  { value: 'error', label: 'Errors only' },
];

export interface EventFilters {
  /** Sources switched on; empty means all of them. */
  sources: string[];
  since: string;
  level: string;
  /** Event-name prefix, e.g. `mac.run`. */
  event: string;
  /** Only the Kubernetes events of this cluster (used by the k8s source). */
  cluster?: string;
}

export const EMPTY_FILTERS: EventFilters = { sources: [], since: '', level: '', event: '', cluster: '' };

/** The request for the feed: only the filters that are set. `source` is always sent, as it asks for the merged feed. */
export function eventsPath(f: EventFilters, limit = 200, sinceOverride?: string): string {
  const p = new URLSearchParams();
  p.set('source', f.sources.length ? f.sources.join(',') : 'all');
  const since = sinceOverride ?? f.since;
  if (since) p.set('since', since);
  if (f.level) p.set('level', f.level);
  if (f.event.trim()) p.set('event', f.event.trim());
  if (f.cluster?.trim() && wantsK8s(f)) p.set('cluster', f.cluster.trim());
  p.set('limit', String(limit));
  return `/v1/events?${p.toString()}`;
}

/** Does this selection include the Kubernetes API events (named, or everything)? */
export function wantsK8s(f: EventFilters): boolean {
  return f.sources.length === 0 || f.sources.includes('k8s');
}

const shellWord = (s: string): string => (/^[\w.:/,-]+$/.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`);

/** The command that shows the same feed in a terminal. */
export function eventsCommand(f: EventFilters, follow = false): string {
  const parts = ['fluxctl events'];
  if (f.sources.length) parts.push(`--source ${f.sources.join(',')}`);
  if (f.since) parts.push(`--since ${f.since}`);
  if (f.level) parts.push(`--level ${f.level}`);
  if (f.event.trim()) parts.push(`--event ${shellWord(f.event.trim())}`);
  if (f.cluster?.trim() && wantsK8s(f)) parts.push(`--cluster ${shellWord(f.cluster.trim())}`);
  if (follow) parts.push('--follow');
  return parts.join(' ');
}

export function levelTone(l: string): Tone {
  return l === 'error' ? 'bad' : l === 'warn' ? 'warn' : 'ok';
}

export function sourceLabel(s: string): string {
  return SOURCES.find((x) => x.value === s)?.label ?? s;
}

const key = (e: FeedEvent): string => `${e.ts}|${e.event}|${e.summary}`;

/** Newest first. Adds `incoming` to `current`, drops lines already shown and keeps at most `cap`. */
export function mergeNew(current: FeedEvent[], incoming: FeedEvent[], cap = 500): FeedEvent[] {
  const seen = new Set(current.map(key));
  const fresh = incoming.filter((e) => !seen.has(key(e)));
  if (!fresh.length) return current;
  return [...fresh, ...current].sort((a, b) => (a.ts < b.ts ? 1 : a.ts > b.ts ? -1 : 0)).slice(0, cap);
}

/** What to say when the daemon refuses or cannot be reached, and what to do next. */
export function errorWords(message: string): { title: string; body: string } {
  if (/admin|403|forbidden|audit/i.test(message) && /admin|403|forbidden/i.test(message)) {
    return { title: 'That source needs an admin token', body: 'The audit log names every person who changed something, so only an admin token reads it. Switch the Audit source off, or sign in with an admin token. (fluxctl explain EVENTS_NOT_ALLOWED)' };
  }
  if (/not an event source|is not a time|not a level/i.test(message)) {
    return { title: 'That filter was not understood', body: `${message} Clear the filters and try again. (fluxctl explain EVENTS_BAD_SOURCE)` };
  }
  return { title: 'Could not read the events', body: `${message}. Check the daemon is running, or run the command below on the machine instead.` };
}

/** What to say when the list is empty. */
export function emptyWords(f: EventFilters): string {
  const filtered = f.sources.length > 0 || f.since !== '' || f.level !== '' || f.event.trim() !== '' || !!f.cluster?.trim();
  return filtered
    ? 'Nothing matches these filters. Widen the time range or switch more sources on.'
    : 'No events yet. They appear as machines start and stop, as backups run and as changes are made; switch Live on to wait for the next one.';
}

/** The notes of a reply, one per cluster that was left out, each with its next step (never an error: the rest of the feed is shown). */
export function feedNotes(r: Pick<FeedReply, 'notes'>): string[] {
  return (r.notes ?? []).filter((n) => n.trim() !== '');
}

/** The newest timestamp in a list, to ask only for what came after it. */
export function newestTs(items: FeedEvent[]): string | undefined {
  return items.reduce<string | undefined>((m, e) => (m === undefined || e.ts > m ? e.ts : m), undefined);
}
