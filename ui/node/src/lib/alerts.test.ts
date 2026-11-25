// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { ago, alertsCommand, alertsPath, badge, changeFailed, emptyWords, errorWords, isSnoozed, levelTone, snoozeBody, snoozeCommand, unsnoozeCommand, type Alert } from './alerts';

const al = (o: Partial<Alert> = {}): Alert => ({ key: 'vm.oom:web', level: 'error', source: 'vm', summary: 'web ran out of memory', next_step: 'Run `fluxctl events`.', last_seen: '2026-11-24T10:00:00Z', first_seen: '2026-11-24T09:00:00Z', count: 1, ...o });

describe('alerts request', () => {
  it('sends only what is set', () => {
    expect(alertsPath()).toBe('/v1/alerts');
    expect(alertsPath({ since: '7d', includeSnoozed: true })).toBe('/v1/alerts?since=7d&include_snoozed=1');
  });
  it('snoozes for 24 hours unless told otherwise', () => {
    expect(snoozeBody('k')).toEqual({ key: 'k', hours: 24 });
    expect(snoozeBody('k', 2).hours).toBe(2);
  });
  it('shows the terminal command, quoting a key that needs it', () => {
    expect(alertsCommand()).toBe('fluxctl alerts');
    expect(alertsCommand('24h')).toBe('fluxctl alerts');
    expect(alertsCommand('7d')).toBe('fluxctl alerts --since 7d');
    expect(snoozeCommand('mac.schedule.run:nightly')).toBe('fluxctl alerts snooze mac.schedule.run:nightly');
    expect(snoozeCommand("a b's")).toBe(`fluxctl alerts snooze 'a b'\\''s'`);
  });
});

describe('the bell', () => {
  it('is quiet with nothing to do, red for an error, yellow for warnings only', () => {
    expect(badge(null)).toEqual({ count: 0, tone: null, label: 'Nothing needs attention' });
    expect(badge({ errors: 0, warnings: 0 }).tone).toBeNull();
    expect(badge({ errors: 1, warnings: 2 })).toEqual({ count: 3, tone: 'bad', label: '1 error and 2 warnings need attention' });
    expect(badge({ errors: 0, warnings: 1 })).toEqual({ count: 1, tone: 'warn', label: '1 warning needs attention' });
  });
  it('maps levels to tones', () => {
    expect(levelTone('error')).toBe('bad');
    expect(levelTone('warn')).toBe('warn');
  });
});

describe('wording', () => {
  it('knows a snoozed alert only until its time is up', () => {
    const now = Date.parse('2026-11-24T12:00:00Z');
    expect(isSnoozed(al(), now)).toBe(false);
    expect(isSnoozed(al({ snoozed_until: '2026-11-25T12:00:00Z' }), now)).toBe(true);
    expect(isSnoozed(al({ snoozed_until: '2026-11-24T11:00:00Z' }), now)).toBe(false);
  });
  it('says what an empty list means and mentions snoozed ones', () => {
    expect(emptyWords({ since: '24h', snoozed: 0 })).toContain('last 24h');
    expect(emptyWords({ since: '24h', snoozed: 2 })).toContain('2 alerts are snoozed');
  });
  it('explains a refusal and an outage with the next step', () => {
    expect(errorWords('403 forbidden: tenant token').title).toContain('whole host');
    expect(errorWords('network down').body).toContain('fluxctl alerts');
  });
  it('says why a snooze was not saved and which command does it', () => {
    expect(unsnoozeCommand('vm.oom:web')).toBe('fluxctl alerts unsnooze vm.oom:web');
    expect(changeFailed('admin role required', snoozeCommand('k'))).toBe('Not saved: it needs an admin token. Sign in with one, or run `fluxctl alerts snooze k` on the machine.');
    expect(changeFailed('disk full', unsnoozeCommand('k'))).toContain('disk full. Try again, or run `fluxctl alerts unsnooze k`');
  });
  it('says how long ago in short words', () => {
    const now = Date.parse('2026-11-24T12:00:00Z');
    expect(ago('2026-11-24T11:59:30Z', now)).toBe('just now');
    expect(ago('2026-11-24T11:30:00Z', now)).toBe('30 min ago');
    expect(ago('2026-11-24T09:00:00Z', now)).toBe('3 h ago');
    expect(ago('2026-11-20T12:00:00Z', now)).toBe('4 d ago');
  });
});
