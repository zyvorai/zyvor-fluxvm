// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { checkTone, checkWord, needsAttention, notChecked, type SetupCheck, type SetupReport } from './doctor';

const c = (o: Partial<SetupCheck>): SetupCheck => ({ area: 'a', title: 'A', light: 'ok', message: 'm', next: null, command: null, safe_fix: false, checked: true, ...o });
const report = (checks: SetupCheck[]): SetupReport => ({ schema: 1, light: 'ok', headline: 'h', checks, next: null, offer_support_bundle: false, support_bundle_command: 'fluxctl support-bundle' });

describe('doctor report helpers', () => {
  it('maps a line to a tone and a word, and never colours an unchecked one', () => {
    expect(checkTone(c({}))).toBe('ok');
    expect(checkTone(c({ light: 'warn' }))).toBe('warn');
    expect(checkTone(c({ light: 'fail' }))).toBe('bad');
    expect(checkTone(c({ light: 'fail', checked: false }))).toBe('ok');
    expect(checkWord(c({ light: 'warn' }))).toBe('Yellow');
    expect(checkWord(c({ checked: false }))).toBe('Not checked');
  });
  it('lists what needs attention, red first', () => {
    const r = report([c({ title: 'ok' }), c({ title: 'y', light: 'warn' }), c({ title: 'r', light: 'fail' }), c({ title: 'u', light: 'warn', checked: false })]);
    expect(needsAttention(r).map((x) => x.title)).toEqual(['r', 'y']);
  });
  it('groups the unchecked lines by the command that checks them', () => {
    const r = report([c({ title: 'Docker', checked: false, command: 'fluxctl doctor' }), c({ title: 'Registry', checked: false, command: 'fluxctl doctor' }), c({ title: 'Fleet', checked: false, command: 'fluxctl doctor -f inventory.yaml' })]);
    expect(notChecked(r)).toEqual([{ titles: ['Docker', 'Registry'], command: 'fluxctl doctor' }, { titles: ['Fleet'], command: 'fluxctl doctor -f inventory.yaml' }]);
  });
});
