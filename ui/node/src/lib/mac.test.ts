// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type MacInstance, actionCommand, actionDone, actionLabel, actionsFor, attachCommand, execCommand, explainError, filterInstances, memoryText, portsText, sizeText, statusTone, summarize, composeCommands, shareCommand, shareServeCommand, editFormFor, memoryField, parseLabelLines, parseMemory, planEdit, updateAnswerText, updateCommand, supervisorNote, unrestarted, healthAdvice, healthText, healthTone } from './mac';

const i = (name: string, status: string, extra: Partial<MacInstance> = {}): MacInstance => ({
  id: name + '-id', name, image: 'xcode:16', status, ip: '192.168.64.5', cpus: 4, memory_mib: 8192, created: null, ports: [], restart_policy: 'no', project: null, service: null, error: null, labels: {}, ...extra,
});

describe('mac instances', () => {
  it('tones and counts by state', () => {
    expect(statusTone('running')).toBe('ok');
    expect(statusTone('failed')).toBe('bad');
    expect(statusTone('creating')).toBe('warn');
    expect(statusTone('stopped')).toBe('idle');
    expect(summarize([i('a', 'running'), i('b', 'stopped'), i('c', 'paused'), i('d', 'failed')])).toEqual({ running: 1, stopped: 2, failed: 1, unhealthy: 0 });
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
  it('shows the fluxctl command for a shell and for attach', () => {
    expect(execCommand('ci')).toBe('fluxctl mac exec -it ci');
    expect(attachCommand('ci')).toBe('fluxctl mac attach ci');
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

describe('composeCommands', () => {
  it('spells watch, scale and the fleet plan as fluxctl commands for the project', () => {
    const c = composeCommands('shop');
    expect(c.map((x) => x.label)).toEqual(['Watch', 'Scale', 'Fleet plan']);
    expect(c[0].cmd).toBe('fluxctl mac compose -p shop watch');
    expect(c[1].cmd).toContain('--scale SERVICE=2');
    expect(c[2].cmd).toContain('--dry-run --fleet');
    expect(c.every((x) => x.cmd.startsWith('fluxctl mac compose'))).toBe(true);
  });
});

describe('share commands', () => {
  it('name the instance in the command', () => {
    expect(shareCommand('build-1')).toBe('fluxctl share build-1');
    expect(shareServeCommand('build-1')).toBe('fluxctl share build-1 --serve');
  });
});

describe('edit (fluxctl mac update)', () => {
  const ci = i('ci', 'stopped', { restart_policy: 'always', labels: { team: 'ios', old: '1' } });

  it('reads and writes memory the way fluxctl does', () => {
    expect(memoryField(8192)).toBe('8g');
    expect(memoryField(1536)).toBe('1536m');
    expect(parseMemory('16g')).toBe(16384);
    expect(parseMemory(' 16 GiB ')).toBe(16384);
    expect(parseMemory('512m')).toBe(512);
    expect(parseMemory('4096')).toBe(4096);
    expect(parseMemory('lots')).toBeNull();
    expect(parseMemory('0')).toBeNull();
  });

  it('parses label lines and names the bad one', () => {
    expect(parseLabelLines('a=b\n\n c=d=e ')).toEqual({ labels: { a: 'b', c: 'd=e' }, bad: null });
    expect(parseLabelLines('a=b\nnovalue').bad).toBe('novalue');
    expect(parseLabelLines('a b=c').bad).toBe('a b=c');
  });

  it('starts from the instance, so nothing is sent until you change something', () => {
    const p = planEdit(ci, editFormFor(ci));
    expect(p.body).toEqual({});
    expect(p.error).toMatch(/Nothing is different/);
  });

  it('sends only what changed, with the fluxctl command for it', () => {
    const f = { ...editFormFor(ci), cpus: '8', memory: '16g', restart: 'unless-stopped', labels: 'team=android\nnew=x y' };
    const p = planEdit(ci, f);
    expect(p.error).toBeNull();
    expect(p.body).toEqual({ cpus: 8, memory_mib: 16384, restart: 'unless-stopped', labels: { team: 'android', new: 'x y' }, rm_labels: ['old'] });
    expect(p.command).toBe("fluxctl mac update ci --cpus 8 --memory 16g --restart unless-stopped --label team=android --label 'new=x y' --rm-label old");
    expect(p.needsRestart).toBe(false);
  });

  it('a running instance needs a restart for the size, and restart now adds it with a checkpoint by default', () => {
    const run = i('ci', 'running');
    const f = { ...editFormFor(run), cpus: '8' };
    const p = planEdit(run, f);
    expect(p.needsRestart).toBe(true);
    expect(p.body.restart_now).toBeUndefined();
    const now = planEdit(run, { ...f, restartNow: true });
    expect(now.body).toEqual({ cpus: 8, restart_now: true });
    expect(now.command).toBe('fluxctl mac update ci --cpus 8 --restart-now');
    const skip = planEdit(run, { ...f, restartNow: true, checkpoint: false });
    expect(skip.body.checkpoint).toBe(false);
    expect(skip.command).toMatch(/--restart-now --no-checkpoint$/);
    // Labels alone never need a restart.
    expect(planEdit(run, { ...editFormFor(run), labels: 'a=b' }).needsRestart).toBe(false);
  });

  it('says what is wrong in plain words', () => {
    expect(planEdit(ci, { ...editFormFor(ci), cpus: '0' }).error).toMatch(/CPUs/);
    expect(planEdit(ci, { ...editFormFor(ci), memory: 'lots' }).error).toMatch(/8g, 512m/);
    expect(planEdit(ci, { ...editFormFor(ci), memory: '64m' }).error).toMatch(/512 MiB/);
    expect(planEdit(ci, { ...editFormFor(ci), labels: 'oops' }).error).toMatch(/KEY=VALUE/);
  });

  it('quotes a name or label with spaces', () => {
    expect(updateCommand("o'neil", { cpus: 2 })).toBe("fluxctl mac update 'o'\\''neil' --cpus 2");
  });

  it('describes the daemon answer, pending restarts and checkpoints included', () => {
    const base = { ok: true, name: 'ci', applied: [], pending_restart: [], restarted: false, checkpoint: null, explain: null, next: null, fluxctl: '' };
    expect(updateAnswerText('ci', { ...base, applied: ['cpus 4 -> 8'], restarted: true, checkpoint: 'auto-before-update-5' })).toMatch(/applied cpus 4 -> 8.*checkpoint restore ci auto-before-update-5.*restarted/);
    const pend = updateAnswerText('ci', { ...base, ok: false, pending_restart: [{ change: 'cpus 4 -> 8' }], next: 'Add restart now.' });
    expect(pend).toMatch(/Not applied, needs a restart: cpus 4 -> 8\..*Add restart now/);
    expect(updateAnswerText('ci', base)).toMatch(/nothing to change/);
  });
});

describe('restart policies and the supervisor', () => {
  const sup = { installed: false, label: 'x', install_command: 'fluxctl mac supervise --install', status_command: 'fluxctl mac supervise --status' };
  const list = [i('a', 'stopped', { restart_policy: 'always' }), i('b', 'running'), i('c', 'failed', { restart_policy: 'on-failure' })];
  it('says nothing when the restarter is installed or no policy is set', () => {
    expect(supervisorNote(list, { ...sup, installed: true })).toBe('');
    expect(supervisorNote([i('b', 'running')], sup)).toBe('');
    expect(supervisorNote(list, undefined)).toBe('');
  });
  it('names the problem and the command when a policy has no restarter', () => {
    expect(unrestarted(list, sup).map((x) => x.name)).toEqual(['a', 'c']);
    const n = supervisorNote(list, sup);
    expect(n).toContain('2 instances have a restart policy');
    expect(n).toContain('fluxctl mac supervise --install');
    expect(supervisorNote([list[0]], sup)).toContain('1 instance has');
  });
});

describe('health', () => {
  const h = (status: 'starting' | 'healthy' | 'unhealthy', n = 0) => ({ status, failing_streak: n, last_check_unix: 1 });
  it('has a tone and a sentence for every state, and nothing before the first probe', () => {
    expect(healthTone(h('healthy'))).toBe('ok');
    expect(healthTone(h('unhealthy', 3))).toBe('bad');
    expect(healthTone(h('starting', 1))).toBe('warn');
    expect(healthTone(null)).toBe('idle');
    expect(healthText(null)).toBe('');
    expect(healthText(h('healthy'))).toBe('healthy');
    expect(healthText(h('unhealthy', 3))).toBe('unhealthy (3 probes failed in a row)');
    expect(healthText(h('starting', 1))).toBe('starting (1 probe failed so far)');
  });
  it('says what to do only when unhealthy', () => {
    expect(healthAdvice(i('a', 'running', { health: h('healthy') }))).toBe('');
    const a = healthAdvice(i('a', 'running', { health: h('unhealthy', 3) }));
    expect(a).toContain('fluxctl mac health a');
    expect(a).toContain('fluxctl mac exec -it a');
  });
  it('is part of the free-text filter and the summary', () => {
    const list = [i('a', 'running', { health: h('unhealthy', 3) }), i('b', 'running')];
    expect(filterInstances(list, 'unhealthy').map((x) => x.name)).toEqual(['a']);
    expect(summarize(list).unhealthy).toBe(1);
  });
});
