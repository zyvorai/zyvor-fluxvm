// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type FleetHost, ageText, diskText, primaryAction, slotsText, stateTone, thermalText, viewTone } from './fleet';

const cmds = { cordon: 'fluxctl mac-fleet cordon m1', uncordon: 'fluxctl mac-fleet uncordon m1', drain: 'fluxctl mac-fleet drain m1 --wait', doctor: 'fluxctl mac-fleet doctor m1' };
const h = (o: Partial<FleetHost>): FleetHost => ({ name: 'm1', host: 'a@m1', state: 'ready', cordoned: false, macos_guests: 1, macos_limit: 2, thermal: 'nominal', thermal_limit_pct: null, disk_free_gib: 120, disk_low: false, on_ac: true, error: null, next: null, commands: cmds, ...o });

describe('fleet rows', () => {
  it('shows guests against the limit and dashes for a Mac that did not answer', () => {
    expect(slotsText(h({}))).toBe('1/2');
    expect(slotsText(h({ state: 'unreachable' }))).toBe('—');
    expect(thermalText(h({ thermal: 'throttled', thermal_limit_pct: 70 }))).toBe('throttled 70%');
    expect(diskText(h({ disk_free_gib: 30, disk_low: true }))).toBe('30 GiB free (low)');
  });
  it('tones the states', () => {
    expect([stateTone('ready'), stateTone('full'), stateTone('draining'), stateTone('unreachable'), stateTone('unknown')]).toEqual(['ok', 'info', 'warn', 'bad', 'idle']);
  });
});

describe('copy as fluxctl', () => {
  it('offers the command that fits the state, never an action run from the browser', () => {
    expect(primaryAction(h({}))?.command).toBe(cmds.cordon);
    expect(primaryAction(h({ state: 'cordoned' }))?.command).toBe(cmds.uncordon);
    expect(primaryAction(h({ state: 'draining' }))?.command).toBe(cmds.drain);
    expect(primaryAction(h({ state: 'unreachable' }))?.command).toBe(cmds.doctor);
    expect(primaryAction(h({ state: 'unknown' }))).toBeNull();
  });
});

describe('freshness', () => {
  it('writes ages and warns on old data', () => {
    expect([ageText(null), ageText(30), ageText(600), ageText(7200)]).toEqual(['never', '30s ago', '10m ago', '2h ago']);
    const s = { macs: 1, ready: 1, cordoned: 0, unreachable: 0, slots_used: 0, slots_total: 2 };
    expect(viewTone({ status: 'stale', summary: s })).toBe('warn');
    expect(viewTone({ status: 'live', summary: s })).toBe('ok');
    expect(viewTone({ status: 'live', summary: { ...s, unreachable: 1 } })).toBe('bad');
  });
});
