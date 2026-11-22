// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { openSteps, stepTone, stepWord, upAdvice, upCommand, type StepState, type UpPlan } from './up';

const step = (id: string, state: StepState) => ({ id, title: id, state, detail: 'd', code: null, fix: null });
const plan = (o: Partial<UpPlan> = {}): UpPlan => ({ schema: 1, command: 'fluxctl up', host: 'box', headline: 'h', nothing_to_do: false, can_run: true, steps: [], next_commands: [], ...o });

describe('up plan helpers', () => {
  it('maps a step state to a tone and a word', () => {
    expect((['done', 'todo', 'warn', 'blocked'] as StepState[]).map(stepTone)).toEqual(['ok', 'warn', 'warn', 'bad']);
    expect(stepWord('todo')).toBe('To do');
    expect(stepWord('blocked')).toBe('Blocked');
  });
  it('builds the command to copy', () => {
    expect(upCommand(plan())).toBe('fluxctl up');
    expect(upCommand(plan(), true)).toBe('fluxctl up --dry-run');
  });
  it('advises by what the plan says', () => {
    expect(upAdvice(plan({ can_run: false }))).toMatch(/blocked/);
    expect(upAdvice(plan({ nothing_to_do: true }))).toMatch(/changes nothing/);
    expect(upAdvice(plan())).toMatch(/asks once/);
  });
  it('lists only open steps, blocked first', () => {
    const p = plan({ steps: [step('a', 'done'), step('b', 'warn'), step('c', 'blocked'), step('d', 'todo')] });
    expect(openSteps(p).map((s) => s.id)).toEqual(['c', 'd', 'b']);
  });
});
