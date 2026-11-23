// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/doctor` (schema 1): the same list `fluxctl doctor --json` prints.

import type { Severity, Tone } from './health';

export interface SetupCheck {
  area: string;
  title: string;
  light: Severity;
  message: string;
  next: string | null;
  command: string | null;
  safe_fix: boolean;
  /** False when the daemon could not look (it cannot see your home folder or other machines). */
  checked: boolean;
}

export interface SetupReport {
  schema: number;
  light: Severity;
  headline: string;
  checks: SetupCheck[];
  next: string | null;
  offer_support_bundle: boolean;
  support_bundle_command: string;
}

export const checkTone = (c: Pick<SetupCheck, 'light' | 'checked'>): Tone => (!c.checked || c.light === 'ok' ? 'ok' : c.light === 'warn' ? 'warn' : 'bad');
export const checkWord = (c: Pick<SetupCheck, 'light' | 'checked'>): string => (!c.checked ? 'Not checked' : c.light === 'ok' ? 'Green' : c.light === 'warn' ? 'Yellow' : 'Red');

/** The lines that need a person, worst first; green and not-checked lines are left out. */
export const needsAttention = (r: SetupReport): SetupCheck[] =>
  r.checks.filter((c) => c.checked && c.light !== 'ok').sort((a, b) => (a.light === b.light ? 0 : a.light === 'fail' ? -1 : 1));

/** Lines the daemon could not check, grouped by the one command that does. */
export const notChecked = (r: SetupReport): { titles: string[]; command: string }[] => {
  const by = new Map<string, string[]>();
  for (const c of r.checks) if (!c.checked && c.command) by.set(c.command, [...(by.get(c.command) ?? []), c.title]);
  return [...by].map(([command, titles]) => ({ command, titles }));
};
