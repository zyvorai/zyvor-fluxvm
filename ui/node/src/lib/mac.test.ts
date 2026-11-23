// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type MacInstance, actionCommand, actionDone, actionLabel, actionsFor, explainError, filterInstances, memoryText, portsText, sizeText, statusTone, summarize } from './mac';

const i = (name: string, status: string, extra: Partial<MacInstance> = {}): MacInstance => ({
  id: name + '-id', name, image: 'xcode:16', status, ip: '192.168.64.5', cpus: 4, memory_mib: 8192, created: null, ports: [], restart_policy: 'no', project: null, service: null, error: null, labels: {}, ...extra,
});

describe('mac instances', () => {
  it('tones and counts by state', () => {
    expect(statusTone('running')).toBe('ok');
    expect(statusTone('failed')).toBe('bad');
    expect(statusTone('creating')).toBe('warn');
    expect(statusTone('stopped')).toBe('idle');
    expect(summarize([i('a', 'running'), i('b', 'stopped'), i('c', 'paused'), i('d', 'failed')])).toEqual({ running: 1, stopped: 2, failed: 1 });
  });
  it('writes ports, memory and sizes', () => {
    expect(portsText([{ host: 2222, guest: 22 }, { host: 8080, guest: 80 }])).toBe('2222→22, 8080→80');
    expect(portsText([])).toBe('');
    expect(memoryText(8192)).toBe('8 GiB');
    expect(memoryText(1536)).toBe('1.5 GiB');
    expect(memoryText(512)).toBe('512 MiB');
    expect(memoryText(null)).toBe('—');
    expect(sizeText(64_000_000_000)).toBe('64.0 GB');
    expect(sizeText(null)).toBe('—');
  });
  it('filters by every word', () => {
    const l = [i('web', 'running', { project: 'shop' }), i('ci', 'stopped')];
    expect(filterInstances(l, 'shop run').map((x) => x.name)).toEqual(['web']);
    expect(filterInstances(l, '').length).toBe(2);
    expect(filterInstances(l, 'nothing')).toEqual([]);
  });
});

describe('mac actions', () => {
  it('offers what the state allows', () => {
    expect(actionsFor('running')).toEqual(['restart', 'stop', 'kill', 'rm']);
    expect(actionsFor('stopped')).toEqual(['start', 'rm']);
    expect(actionsFor('failed')).toEqual(['start', 'rm']);
    expect(actionsFor('paused')).not.toContain('start');
  });
  it('shows the fluxctl command of every action', () => {
    for (const a of ['start', 'stop', 'restart', 'kill', 'rm'] as const) {
      expect(actionCommand(a, 'ci')).toBe(`fluxctl mac ${a} ci`);
      expect(actionDone('ci', a)).toContain('ci');
    }
    expect(actionLabel('rm')).toBe('Remove');
    expect(actionLabel('stop')).toBe('Stop');
  });
  it('says what to do next in plain English', () => {
    expect(explainError('ci', 'this Mac already runs 2 macOS guests')).toMatch(/Stop or remove one/);
    expect(explainError('ci', 'login session is locked')).toMatch(/Unlock the Mac/);
    expect(explainError('ci', 'admin role required')).toMatch(/administrator/);
    expect(explainError('ci', 'boom')).toMatch(/fluxctl mac doctor/);
  });
});
