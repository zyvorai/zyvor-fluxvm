// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/alerts`: what needs attention now, built from warn/error events, failed backups and schedule
// runs, unhealthy Mac instances and the cluster health model. The same document as `fluxctl alerts --json`.

import type { Tone } from './health';

export interface Alert {
  /** Stable id: what `snooze` takes. */
  key: string;
  level: 'error' | 'warn' | string;
  source: string;
  summary: string;
  /** What to do next, in plain English. */
  next_step: string;
  /** A code `fluxctl explain` knows. */
  explain?: string;
  target?: string;
  last_seen: string;
  first_seen: string;
  count: number;
  snoozed_until?: string;
}

export interface AlertsReply {
  since: string;
  items: Alert[];
  /** Counts of alerts that are not snoozed. */
  errors: number;
  warnings: number;
  snoozed: number;
  notes?: string[];
}

export const SNOOZE_HOURS = 24;
/** The windows the page offers; the first one is what `fluxctl alerts` uses by default. */
export const WINDOWS = ['24h', '7d', '30d'] as const;
/** Sent on `window` after a snooze, so the bell does not wait for its next poll. */
export const ALERTS_CHANGED = 'fluxvm:alerts-changed';

export function alertsPath(opts: { since?: string; includeSnoozed?: boolean } = {}): string {
  const p = new URLSearchParams();
  if (opts.since) p.set('since', opts.since);
  if (opts.includeSnoozed) p.set('include_snoozed', '1');
  const q = p.toString();
  return `/v1/alerts${q ? `?${q}` : ''}`;
}

export const snoozeBody = (key: string, hours = SNOOZE_HOURS): { key: string; hours: number } => ({ key, hours });

export function levelTone(l: string): Tone {
  return l === 'error' ? 'bad' : l === 'warn' ? 'warn' : 'ok';
}

/** The number on the bell, and how loud it is: red when anything is an error, yellow for warnings only, none when quiet. */
export function badge(r: Pick<AlertsReply, 'errors' | 'warnings'> | null | undefined): { count: number; tone: Tone | null; label: string } {
  const e = r?.errors ?? 0;
  const w = r?.warnings ?? 0;
  const count = e + w;
  if (count === 0) return { count: 0, tone: null, label: 'Nothing needs attention' };
  const parts = [e ? `${e} ${e === 1 ? 'error' : 'errors'}` : '', w ? `${w} ${w === 1 ? 'warning' : 'warnings'}` : ''].filter(Boolean);
  return { count, tone: e ? 'bad' : 'warn', label: `${parts.join(' and ')} ${count === 1 ? 'needs' : 'need'} attention` };
}

/** The command that shows the same list in a terminal. */
export function alertsCommand(since?: string): string {
  return since && since !== '24h' ? `fluxctl alerts --since ${since}` : 'fluxctl alerts';
}

export const snoozeCommand = (key: string): string => `fluxctl alerts snooze ${/^[\w.:/,@=+-]+$/.test(key) ? key : `'${key.replace(/'/g, `'\\''`)}'`}`;

/** Is this alert hidden right now? */
export const isSnoozed = (a: Alert, now = Date.now()): boolean => !!a.snoozed_until && Date.parse(a.snoozed_until) > now;

/** What to say when the list is empty. */
export function emptyWords(r: Pick<AlertsReply, 'snoozed' | 'since'>): string {
  const more = r.snoozed > 0 ? ` ${r.snoozed} ${r.snoozed === 1 ? 'alert is' : 'alerts are'} snoozed; show them to bring one back.` : '';
  return `Nothing needs attention: no warnings or errors in the last ${r.since}, no failed backups or schedules, no unhealthy Mac instances and no health problems.${more}`;
}

/** What to say when a snooze or "bring back" is refused, with the command that does it instead. */
export function changeFailed(message: string, command: string): string {
  const why = /admin|403|forbidden/i.test(message) ? 'it needs an admin token. Sign in with one' : `${message}. Try again`;
  return `Not saved: ${why}, or run \`${command}\` on the machine.`;
}

export const unsnoozeCommand = (key: string): string => snoozeCommand(key).replace(' snooze ', ' unsnooze ');

/** What to say when the daemon refuses or cannot be reached, and what to do next. */
export function errorWords(message: string): { title: string; body: string } {
  if (/tenant|403|forbidden/i.test(message)) {
    return { title: 'Alerts are for the whole host', body: 'A tenant token sees only its own machines. Sign in with an admin or read-only token, or run `fluxctl events --source vm` for your own machines.' };
  }
  return { title: 'Could not read the alerts', body: `${message}. Check the daemon is running, or run \`fluxctl alerts\` on the machine instead.` };
}

/** "2 hours ago" style, for the list. */
export function ago(ts: string, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - Date.parse(ts)) / 1000));
  if (Number.isNaN(s)) return '';
  if (s < 90) return 'just now';
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 129600) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}
