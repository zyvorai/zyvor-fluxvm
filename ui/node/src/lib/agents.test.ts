// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { agentTone, agentWord, needsAttention, repairSummary, repairable, restartCommand, runningText, type AgentInfo, type AgentsReport } from './agents';

const a = (label: string, health: AgentInfo['health'], extra: Partial<AgentInfo> = {}): AgentInfo => ({ label, name: label, loaded: health !== 'not_loaded', state: null, pid: null, last_exit_code: null, program: '/x', program_exists: true, plist: '/p', log: null, health, what_to_do: null, ...extra });
const report = (agents: AgentInfo[]): AgentsReport => ({ schema: 1, agents, headline: 'x' });

describe('agents helpers', () => {
  it('uses the CLI words and a tone per health', () => {
    expect(agentWord('not_loaded')).toBe('NOT LOADED');
    expect(agentWord('crashing')).toBe('FAILING');
    expect(agentTone('ok')).toBe('ok');
    expect(agentTone('crashing')).toBe('bad');
    expect(agentTone('stale_program')).toBe('warn');
  });
  it('offers repair only for installed-but-not-loaded services', () => {
    const r = report([a('dev.zyvor.fluxvm', 'ok'), a('dev.zyvor.fluxvm.kube', 'not_loaded'), a('dev.zyvor.fluxvm.usbd', 'stale_program'), a('dev.zyvor.fluxvm.docker-engine', 'crashing')]);
    expect(repairable(r).map((x) => x.label)).toEqual(['dev.zyvor.fluxvm.kube']);
    expect(needsAttention(r)).toHaveLength(3);
  });
  it('builds the restart command with the short name the CLI accepts', () => {
    expect(restartCommand(a('dev.zyvor.fluxvm', 'ok'))).toBe('fluxctl agents restart daemon');
    expect(restartCommand(a('dev.zyvor.fluxvm.kube', 'ok'))).toBe('fluxctl agents restart kube');
  });
  it('says what a row is doing', () => {
    expect(runningText(a('x', 'ok', { pid: 7 }))).toBe('running (pid 7)');
    expect(runningText(a('x', 'crashing', { loaded: true }))).toBe('loaded, not running');
    expect(runningText(a('x', 'not_loaded'))).toBe('not loaded');
  });
  it('summarises a repair, including a failure', () => {
    expect(repairSummary([])).toMatch(/Nothing to repair/);
    expect(repairSummary([{ label: 'a', ok: true, detail: 'loaded' }])).toBe('Loaded 1 service.');
    expect(repairSummary([{ label: 'a', ok: false, detail: 'boom' }])).toBe('Could not load a: boom');
  });
});
