// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/audit`: what was changed, by whom, when and how it ended, newest first. The same records as
// `fluxctl audit ls --json`.

import type { Tone } from './health';

export interface AuditRecord {
  ts: string;
  action: string;
  actor: string;
  target: string;
  result: 'ok' | 'failed' | 'denied' | string;
  source: 'cli' | 'api' | string;
  detail?: Record<string, string>;
}

export interface AuditPage {
  items: AuditRecord[];
  total: number;
  offset: number;
  next_offset?: number;
  /** Plain-English note that the oldest records were removed to keep the log within its size cap. */
  notice?: string;
}

export interface AuditFilters {
  since: string; // '' | '1h' | '24h' | '7d' | '30d'
  action: string;
  target: string;
  actor: string;
  result: string; // '' | 'ok' | 'failed' | 'denied'
}

export const EMPTY_FILTERS: AuditFilters = { since: '', action: '', target: '', actor: '', result: '' };

export const SINCE_CHOICES: { value: string; label: string; ms: number }[] = [
  { value: '', label: 'Any time', ms: 0 },
  { value: '1h', label: 'Last hour', ms: 3600e3 },
  { value: '24h', label: 'Last 24 hours', ms: 86400e3 },
  { value: '7d', label: 'Last 7 days', ms: 7 * 86400e3 },
  { value: '30d', label: 'Last 30 days', ms: 30 * 86400e3 },
];

/** The query string of a request: only the filters that are set, URL-encoded; `now` makes `since` testable. */
export function auditQuery(f: AuditFilters, page: { limit?: number; offset?: number } = {}, now: Date = new Date()): string {
  const p = new URLSearchParams();
  const since = SINCE_CHOICES.find((s) => s.value === f.since);
  if (since && since.ms > 0) p.set('since', new Date(now.getTime() - since.ms).toISOString());
  if (f.action.trim()) p.set('action', f.action.trim());
  if (f.target.trim()) p.set('target', f.target.trim());
  if (f.actor.trim()) p.set('actor', f.actor.trim());
  if (f.result) p.set('result', f.result);
  if (page.limit) p.set('limit', String(page.limit));
  if (page.offset) p.set('offset', String(page.offset));
  return p.toString();
}

export const auditPath = (f: AuditFilters, page: { limit?: number; offset?: number } = {}, now?: Date): string => {
  const q = auditQuery(f, page, now);
  return q ? `/v1/audit?${q}` : '/v1/audit';
};

export function resultTone(r: string): Tone {
  return r === 'ok' ? 'ok' : r === 'denied' ? 'warn' : 'bad';
}

export function resultWord(r: string): string {
  return r === 'ok' ? 'Worked' : r === 'denied' ? 'Refused' : r === 'failed' ? 'Failed' : r;
}

const VERBS: Record<string, string> = {
  run: 'started', rm: 'removed', exec: 'ran a command in', create: 'created', delete: 'deleted', start: 'started', stop: 'stopped',
  restart: 'restarted', pull: 'pulled', push: 'pushed', backup: 'backed up', restore: 'restored', kill: 'killed', up: 'brought up', down: 'took down',
};

/** `mac.run` on `web` as a sentence: "started web (Mac)". Falls back to the raw action for names it does not know. */
export function actionWords(action: string, target: string): string {
  const [group, ...rest] = action.split('.');
  const verb = rest.join('.');
  const word = VERBS[verb] ?? VERBS[verb.split('-')[0]];
  const area = group === 'mac' ? ' (Mac)' : group === 'kube' ? ' (Kubernetes)' : group === 'mac-fleet' ? ' (Mac fleet)' : '';
  if (!word) return target ? `${action} ${target}` : action;
  return `${word} ${target || 'something'}${area}`;
}

/** The command that shows the same list in a terminal. */
export function lsCommand(f: AuditFilters): string {
  const parts = ['fluxctl audit ls'];
  if (f.since) parts.push(`--since ${f.since}`);
  if (f.action.trim()) parts.push(`--action ${shellWord(f.action.trim())}`);
  if (f.target.trim()) parts.push(`--target ${shellWord(f.target.trim())}`);
  if (f.actor.trim()) parts.push(`--actor ${shellWord(f.actor.trim())}`);
  if (f.result) parts.push(`--result ${f.result}`);
  return parts.join(' ');
}

const shellWord = (s: string): string => (/^[\w.:/-]+$/.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`);

/** One JSON object per line, oldest first: the format of `fluxctl audit export`. */
export function toJsonl(records: AuditRecord[]): string {
  return [...records].reverse().map((r) => JSON.stringify(r)).join('\n') + (records.length ? '\n' : '');
}

/** What to say when the list is empty: no records yet, or the filters hide them. */
export function emptyWords(f: AuditFilters, total: number): string {
  const filtered = Object.values(f).some((v) => v.trim() !== '');
  if (filtered || total > 0) return 'Nothing matches these filters. Clear them to see everything.';
  return 'No changes recorded yet. Starting, stopping or removing a machine, backing up a cluster or changing the fleet shows up here.';
}
