// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { copyFor, lightWord, needs, tone, type Fix, type HealthItem } from './health';

const fix = (o: Partial<Fix> = {}): Fix => ({ command: null, steps: null, safe: false, action: null, needs_sudo: false, needs_restart: false, needs_login: false, changes_security: false, ...o });
const item = (f: Fix | null, explain: string | null = 'LOW_DISK'): HealthItem => ({ id: 'a/b/1', machine: 'a', source: 'doctor', severity: 'warn', message: 'm', cause: null, fix: f, explain });

describe('health helpers', () => {
  it('maps severity to a tone and a word', () => {
    expect([tone('ok'), tone('warn'), tone('fail')]).toEqual(['ok', 'warn', 'bad']);
    expect(lightWord('fail')).toBe('Red');
  });
  it('copies fluxctl fix for a safe fix, the command otherwise, explain as a last resort', () => {
    expect(copyFor(item(fix({ safe: true, command: 'launchctl kickstart x', action: { kind: 'start_service' } })))).toBe('fluxctl fix');
    expect(copyFor(item(fix({ command: 'sudo sysctl -w net.ipv4.ip_forward=1', needs_sudo: true })))).toBe('sudo sysctl -w net.ipv4.ip_forward=1');
    expect(copyFor(item(fix({ steps: 'do it by hand' })))).toBe('fluxctl explain LOW_DISK');
    expect(copyFor(item(null))).toBeNull();
  });
  it('lists what a fix needs', () => {
    expect(needs(fix({ needs_sudo: true, needs_restart: true }))).toEqual(['Needs sudo', 'Needs a restart']);
    expect(needs(fix({ changes_security: true }))[0]).toMatch(/never changed/);
    expect(needs(null)).toEqual([]);
  });
});
