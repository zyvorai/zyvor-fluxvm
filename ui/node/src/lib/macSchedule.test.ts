// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type MacSchedule, EMPTY_FORM, addCommand, explainScheduleError, formProblem, nextText, statusText, statusTone, toBody } from './macSchedule';

const s = (x: Partial<MacSchedule> = {}): MacSchedule => ({
  name: 'nightly', cron: '0 3 * * *', image: 'img:1', cmd: [], ttl: null, notify: false, webhook: null, enabled: true,
  next_run_unix: null, last_run_unix: null, last_status: null, last_detail: null, run_now: false, ...x,
});
const ok = { ...EMPTY_FORM, name: 'nightly', image: 'macos-base:15' };

describe('mac schedules', () => {
  it('checks the form and says what to fix', () => {
    expect(formProblem(ok)).toBeNull();
    expect(formProblem({ ...ok, name: 'bad name' })).toMatch(/Name/);
    expect(formProblem({ ...ok, cron: '* * *' })).toMatch(/5 fields/);
    expect(formProblem({ ...ok, cron: '@daily' })).toBeNull();
    expect(formProblem({ ...ok, image: ' ' })).toMatch(/Image/);
    expect(formProblem({ ...ok, ttl: 'soon' })).toMatch(/Time limit/);
    expect(formProblem({ ...ok, webhook: 'ftp://x' })).toMatch(/Webhook/);
  });
  it('builds the request body', () => {
    expect(toBody({ ...ok, cmd: './build.sh  --fast', ttl: ' 2h ' })).toEqual({ name: 'nightly', cron: '0 3 * * *', image: 'macos-base:15', cmd: ['./build.sh', '--fast'], ttl: '2h', notify: true, webhook: null });
  });
  it('writes the fluxctl command without the webhook secret', () => {
    const c = addCommand({ ...ok, ttl: '1h', cmd: './b.sh', webhook: 'https://hooks.example.com/SECRET' });
    expect(c).toBe("fluxctl mac schedule add nightly '0 3 * * *' macos-base:15 --ttl 1h --notify --notify-webhook URL -- ./b.sh");
    expect(c).not.toContain('SECRET');
  });
  it('tones and words', () => {
    expect(statusTone(s({ enabled: false }))).toBe('idle');
    expect(statusTone(s({ last_status: 'failed' }))).toBe('bad');
    expect(statusTone(s({ last_status: 'running' }))).toBe('warn');
    expect(statusTone(s({ last_status: 'ok' }))).toBe('ok');
    expect(statusText(s({ run_now: true }))).toBe('waiting for the next tick');
    expect(statusText(s())).toBe('not run yet');
    expect(statusText(s({ enabled: false }))).toBe('off');
  });
  it('says when the next run is', () => {
    const now = 1_000_000_000;
    expect(nextText(null, now)).toBe('—');
    expect(nextText(1_000_000 + 30, now)).toBe('now');
    expect(nextText(1_000_000 + 600, now)).toBe('in 10m');
    expect(nextText(1_000_000 + 7200, now)).toBe('in 2h');
    expect(nextText(1_000_000 + 3 * 86400, now)).toBe('in 3d');
  });
  it('explains errors', () => {
    expect(explainScheduleError('admin role required')).toMatch(/administrator/);
    expect(explainScheduleError('boom')).toMatch(/mac doctor/);
  });
});
