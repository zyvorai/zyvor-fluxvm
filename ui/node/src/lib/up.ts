// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/up/plan` (schema 1): what `fluxctl up` checks and does, the same document as
// `fluxctl up --dry-run --json` prints under `plan`.

import type { Tone } from './health';

export type StepState = 'done' | 'todo' | 'warn' | 'blocked';

export interface UpStep {
  id: string;
  title: string;
  state: StepState;
  detail: string;
  code: string | null;
  fix: string | null;
}

export interface UpPlan {
  schema: number;
  command: string;
  host: string;
  headline: string;
  nothing_to_do: boolean;
  can_run: boolean;
  steps: UpStep[];
  next_commands: { command: string; why: string }[];
}

export const stepTone = (s: StepState): Tone => (s === 'done' ? 'ok' : s === 'blocked' ? 'bad' : 'warn');
export const stepWord = (s: StepState): string => (s === 'done' ? 'Done' : s === 'todo' ? 'To do' : s === 'warn' ? 'Look' : 'Blocked');

/** The command to copy: the plan's own, with `--dry-run` when asked to look first. */
export const upCommand = (p: Pick<UpPlan, 'command'>, dryRun = false): string => (dryRun ? `${p.command} --dry-run` : p.command);

/** The line under the title: what the person should do about this plan, in words. */
export function upAdvice(p: UpPlan): string {
  if (!p.can_run) return 'Fix the blocked steps first; each says what to do. Then run the command below.';
  if (p.nothing_to_do) return 'Everything is already set up. Running the command again changes nothing.';
  return 'Run the command below in a terminal on this machine. It asks once, then does the steps marked To do.';
}

/** The steps that still need the person or `fluxctl up`, worst first. */
export function openSteps(p: UpPlan): UpStep[] {
  const rank: Record<StepState, number> = { blocked: 0, todo: 1, warn: 2, done: 3 };
  return p.steps.filter((s) => s.state !== 'done').sort((a, b) => rank[a.state] - rank[b.state]);
}
