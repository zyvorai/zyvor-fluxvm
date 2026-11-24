// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `POST /v1/self-test` and `GET /v1/self-test` (the same document as `fluxctl self-test --json`) and the
// small helpers the Home page's Self-test card needs.

export type SelfTestStatus = 'pass' | 'warn' | 'fail' | 'skip';
export type SelfTestTone = 'ok' | 'warn' | 'bad' | 'idle';

export interface SelfTestCheck {
  id: string;
  name: string;
  status: SelfTestStatus;
  detail: string;
  what_to_do: string | null;
  explain: string | null;
}

export interface SelfTestReport {
  schema: number;
  ts: number;
  status: SelfTestStatus;
  passed: number;
  warned: number;
  failed: number;
  skipped: number;
  headline: string;
  checks: SelfTestCheck[];
}

export const SELF_TEST_COMMAND = 'fluxctl self-test';
export const SELF_TEST_FIX_COMMAND = 'fluxctl self-test --fix';

export const selfTestWord = (s: SelfTestStatus): string => s.toUpperCase();
export const selfTestTone = (s: SelfTestStatus): SelfTestTone => (s === 'pass' ? 'ok' : s === 'warn' ? 'warn' : s === 'fail' ? 'bad' : 'idle');

/** Checks that need the person to do something, worst first. */
export function todo(r: SelfTestReport): SelfTestCheck[] {
  const rank = (s: SelfTestStatus) => (s === 'fail' ? 0 : 1);
  return r.checks.filter((c) => (c.status === 'fail' || c.status === 'warn') && c.what_to_do).sort((a, b) => rank(a.status) - rank(b.status));
}

/** `fluxctl explain CODE` for a check that has a code. */
export const explainCommand = (c: SelfTestCheck): string | null => (c.explain ? `fluxctl explain ${c.explain}` : null);
