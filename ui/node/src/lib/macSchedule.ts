// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for the Schedules card of the Mac page: the shape of `GET /v1/mac/schedules`, a check of the add form before it is
// sent (the daemon checks again), and the `fluxctl mac schedule` command for every action.
import type { Tone } from './kube';

export type MacSchedule = {
  name: string;
  cron: string;
  image: string;
  cmd: string[];
  ttl: string | null;
  notify: boolean;
  /** Only the host of the webhook, never the URL. */
  webhook: string | null;
  enabled: boolean;
  next_run_unix: number | null;
  last_run_unix: number | null;
  /** `ok`, `failed` or `running`. */
  last_status: string | null;
  last_detail: string | null;
  run_now: boolean;
};

export type MacScheduleList = {
  path: string | null;
  schedules: MacSchedule[];
  note: string;
  commands: { add: string; list: string; install: string };
};

export type ScheduleForm = { name: string; cron: string; image: string; cmd: string; ttl: string; notify: boolean; webhook: string };

export const EMPTY_FORM: ScheduleForm = { name: '', cron: '0 3 * * *', image: '', cmd: '', ttl: '', notify: true, webhook: '' };

export const PRESETS: { label: string; cron: string }[] = [
  { label: 'Every 15 minutes', cron: '*/15 * * * *' },
  { label: 'Hourly', cron: '@hourly' },
  { label: 'Every night at 3', cron: '0 3 * * *' },
  { label: 'Weekdays at 9', cron: '0 9 * * mon-fri' },
  { label: 'Weekly', cron: '@weekly' },
];

export const LIST_COMMAND = 'fluxctl mac schedule ls';
export const INSTALL_COMMAND = 'fluxctl mac schedule install';
export const NOTIFY_TEST_COMMAND = 'fluxctl mac notify test';

const NAME = /^[A-Za-z0-9_][A-Za-z0-9_.-]{0,39}$/;
const ALIASES = ['@yearly', '@annually', '@monthly', '@weekly', '@daily', '@midnight', '@hourly'];
const DURATION = /^[1-9][0-9]*[smhd]?$/;

/** What is wrong with the form, in words, or null. The daemon checks the cron expression properly. */
export const formProblem = (f: ScheduleForm): string | null => {
  if (!NAME.test(f.name.trim())) return 'Name: use letters, digits, - _ . (up to 40), for example nightly-build.';
  const cron = f.cron.trim();
  if (!ALIASES.includes(cron) && cron.split(/\s+/).length !== 5) return 'When: a cron expression has 5 fields (minute hour day month weekday), for example 0 3 * * *. Pick a preset if unsure.';
  if (!f.image.trim()) return 'Image: say which image to run (fluxctl mac images lists them).';
  if (f.ttl.trim() && !DURATION.test(f.ttl.trim())) return 'Time limit: use a duration such as 30m or 2h.';
  if (f.webhook.trim() && !/^https?:\/\/\S+$/.test(f.webhook.trim())) return 'Webhook: must be an http:// or https:// address.';
  return null;
};

/** The request body for `POST /v1/mac/schedules`. The command is split on spaces; use the CLI when you need shell quoting. */
export const toBody = (f: ScheduleForm) => ({
  name: f.name.trim(),
  cron: f.cron.trim(),
  image: f.image.trim(),
  cmd: f.cmd.trim() ? f.cmd.trim().split(/\s+/) : [],
  ttl: f.ttl.trim() || null,
  notify: f.notify,
  webhook: f.webhook.trim() || null,
});

const sh = (s: string) => (/^[A-Za-z0-9_@%+=:,./-]+$/.test(s) ? s : `'${s.replace(/'/g, `'\\''`)}'`);

/** The same schedule as a fluxctl command. A webhook is left as a placeholder: Studio never puts a secret in a command. */
export const addCommand = (f: ScheduleForm): string =>
  [
    'fluxctl mac schedule add', sh(f.name.trim() || 'NAME'), sh(f.cron.trim() || '* * * * *'), sh(f.image.trim() || 'IMAGE'),
    f.ttl.trim() ? `--ttl ${sh(f.ttl.trim())}` : '', f.notify ? '--notify' : '', f.webhook.trim() ? '--notify-webhook URL' : '',
    f.cmd.trim() ? `-- ${f.cmd.trim()}` : '',
  ].filter(Boolean).join(' ');

export const runNowCommand = (name: string) => `fluxctl mac schedule run-now ${name}`;
export const rmCommand = (name: string) => `fluxctl mac schedule rm ${name}`;

export const statusTone = (s: MacSchedule): Tone => {
  if (!s.enabled) return 'idle';
  switch (s.last_status) {
    case 'failed': return 'bad';
    case 'running': return 'warn';
    default: return 'ok';
  }
};

export const statusText = (s: MacSchedule): string => {
  if (!s.enabled) return 'off';
  if (s.run_now) return 'waiting for the next tick';
  return s.last_status ?? 'not run yet';
};

/** `in 5m`, `in 2h`, `in 3d`, `now`, or an em dash. */
export const nextText = (unix: number | null, nowMs: number): string => {
  if (unix == null) return '—';
  const s = Math.round(unix - nowMs / 1000);
  if (s <= 60) return 'now';
  if (s < 3600) return `in ${Math.round(s / 60)}m`;
  if (s < 86400) return `in ${Math.round(s / 3600)}h`;
  return `in ${Math.round(s / 86400)}d`;
};

/** Plain words for a failed call to /v1/mac/schedules. */
export const explainScheduleError = (message: string): string => {
  if (/admin/i.test(message)) return 'Only an administrator can change schedules. Sign in with an admin token, or run the copied fluxctl command on the Mac.';
  if (/no home directory/i.test(message)) return `${message} Run ${LIST_COMMAND} on the Mac instead.`;
  if (/cron|name|ttl|webhook|image|schedule/i.test(message)) return message;
  return `${message}. If it keeps failing, run fluxctl mac doctor on the Mac.`;
};
