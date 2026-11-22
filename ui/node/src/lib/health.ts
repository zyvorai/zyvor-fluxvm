// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/cluster/health` (schema 1): the same document `fluxctl cluster status --json` prints.

export type Severity = 'ok' | 'warn' | 'fail';
export type Tone = 'ok' | 'warn' | 'bad';

export interface Fix {
  command: string | null;
  steps: string | null;
  safe: boolean;
  action: { kind: string } | null;
  needs_sudo: boolean;
  needs_restart: boolean;
  needs_login: boolean;
  changes_security: boolean;
}

export interface HealthItem {
  id: string;
  machine: string;
  source: string;
  severity: Severity;
  message: string;
  cause: string | null;
  fix: Fix | null;
  explain: string | null;
}

export interface Capacity {
  room_for_vms: number | null;
  macos_in_use: number | null;
  macos_limit: number | null;
  disk_free_gib: number | null;
  summary: string;
}

export interface MachineHealth {
  name: string;
  platform: string;
  local: boolean;
  light: Severity;
  headline: string;
  version: string | null;
  update: { current: string; wanted: string } | null;
  capacity: Capacity;
  last_backup: string;
  checks_passed: number;
  items: HealthItem[];
}

export interface ClusterHealth {
  schema: number;
  light: Severity;
  headline: string;
  machines: MachineHealth[];
  items: HealthItem[];
  capacity: Capacity;
  next_action: { text: string; command: string | null; safe: boolean; machine: string | null };
}

export const tone = (s: Severity): Tone => (s === 'ok' ? 'ok' : s === 'warn' ? 'warn' : 'bad');
export const lightWord = (s: Severity): string => (s === 'ok' ? 'Green' : s === 'warn' ? 'Yellow' : 'Red');

/** What to put on the clipboard for an item's fix: `fluxctl fix` for a safe one, else the exact command (with the machine to run it on). */
export function copyFor(i: HealthItem): string | null {
  const f = i.fix;
  if (!f) return null;
  if (f.safe) return 'fluxctl fix';
  if (f.command) return f.command;
  return `fluxctl explain ${i.explain ?? ''}`.trim();
}

/** A one-line reminder of what the fix needs, in words. */
export function needs(f: Fix | null): string[] {
  if (!f) return [];
  const n: string[] = [];
  if (f.safe) n.push('Safe to apply automatically');
  if (f.needs_sudo) n.push('Needs sudo');
  if (f.needs_restart) n.push('Needs a restart');
  if (f.needs_login) n.push('Needs someone logged in');
  if (f.changes_security) n.push('Security setting: never changed for you');
  return n;
}
