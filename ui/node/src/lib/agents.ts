// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/agents` (the same document as `fluxctl agents list --json`) and the helpers the Home page's
// "Background services" card needs.

export type AgentHealth = 'ok' | 'not_loaded' | 'crashing' | 'stale_program';
export type AgentTone = 'ok' | 'warn' | 'bad' | 'idle';

export interface AgentInfo {
  label: string;
  name: string;
  loaded: boolean;
  state: string | null;
  pid: number | null;
  last_exit_code: number | null;
  program: string | null;
  program_exists: boolean;
  plist: string;
  log: string | null;
  health: AgentHealth;
  what_to_do: string | null;
}

export interface AgentsReport {
  schema: number;
  agents: AgentInfo[];
  headline: string;
}

export interface RepairResult {
  label: string;
  ok: boolean;
  detail: string;
}

export const AGENTS_COMMAND = 'fluxctl agents';
export const AGENTS_REPAIR_COMMAND = 'fluxctl agents repair';

/** The same words as the CLI table. */
export const agentWord = (h: AgentHealth): string => ({ ok: 'OK', not_loaded: 'NOT LOADED', crashing: 'FAILING', stale_program: 'STALE' })[h];
export const agentTone = (h: AgentHealth): AgentTone => (h === 'ok' ? 'ok' : h === 'crashing' ? 'bad' : 'warn');

/** What the row says it is doing. */
export const runningText = (a: AgentInfo): string => (a.pid != null ? `running (pid ${a.pid})` : a.loaded ? 'loaded, not running' : 'not loaded');

/** Agents that the Repair button can fix: installed, the program exists, but launchd does not have them loaded. */
export const repairable = (r: AgentsReport): AgentInfo[] => r.agents.filter((a) => a.health === 'not_loaded');

export const needsAttention = (r: AgentsReport): AgentInfo[] => r.agents.filter((a) => a.health !== 'ok');

/** `fluxctl agents restart NAME` for one row: the label after `dev.zyvor.fluxvm.`, or `daemon` for the daemon itself. */
export const restartCommand = (a: AgentInfo): string => `fluxctl agents restart ${a.label === 'dev.zyvor.fluxvm' ? 'daemon' : a.label.replace(/^dev\.zyvor\.fluxvm\./, '')}`;

/** One line for the result of a repair. */
export function repairSummary(rs: RepairResult[]): string {
  if (rs.length === 0) return 'Nothing to repair: every installed service is loaded.';
  const bad = rs.filter((r) => !r.ok);
  return bad.length ? `Could not load ${bad.map((r) => r.label).join(', ')}: ${bad[0].detail}` : `Loaded ${rs.length} service${rs.length === 1 ? '' : 's'}.`;
}

/** `GET /v1/agents/{label}/log`. */
export interface AgentLog {
  label: string;
  text: string;
}

/** `fluxctl agents logs NAME`, with the same short name as `restartCommand`. */
export const logsCommand = (a: AgentInfo): string => `fluxctl agents logs ${a.label === 'dev.zyvor.fluxvm' ? 'daemon' : a.label.replace(/^dev\.zyvor\.fluxvm[.-]/, '')}`;
