// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Pure helpers for the Mac fleet page: how GET /v1/mac-fleet is read, toned and turned into fluxctl commands to copy.
import type { Tone } from './kube';

export type FleetState = 'ready' | 'full' | 'cordoned' | 'draining' | 'unreachable' | 'unknown';

export type FleetCommands = { cordon: string; uncordon: string; drain: string; doctor: string };

export type FleetHost = {
  name: string;
  host: string;
  state: FleetState;
  cordoned: boolean;
  macos_guests: number;
  macos_limit: number;
  thermal: 'nominal' | 'throttled' | 'unknown';
  thermal_limit_pct: number | null;
  disk_free_gib: number | null;
  disk_low: boolean;
  on_ac: boolean | null;
  error: string | null;
  /** What to do about this Mac, when something is wrong. */
  next: string | null;
  commands: FleetCommands;
};

export type FleetSummary = { macs: number; ready: number; cordoned: number; unreachable: number; slots_used: number; slots_total: number };

export type FleetView = {
  status: 'live' | 'stale' | 'no_snapshot' | 'empty';
  message: string;
  taken_unix: number | null;
  age_secs: number | null;
  hosts: FleetHost[];
  summary: FleetSummary;
  refresh_command: string;
  add_command: string;
};

export const stateTone = (s: FleetState): Tone =>
  s === 'ready' ? 'ok' : s === 'full' ? 'info' : s === 'cordoned' || s === 'draining' ? 'warn' : s === 'unreachable' ? 'bad' : 'idle';

export const stateLabel = (s: FleetState): string =>
  ({ ready: 'ready', full: 'full', cordoned: 'cordoned', draining: 'draining', unreachable: 'unreachable', unknown: 'not checked' })[s];

/** `1/2`, or a dash when the Mac could not be asked. */
export const slotsText = (h: Pick<FleetHost, 'state' | 'macos_guests' | 'macos_limit'>): string =>
  h.state === 'unreachable' || h.state === 'unknown' ? '—' : `${h.macos_guests}/${h.macos_limit}`;

export const thermalText = (h: Pick<FleetHost, 'state' | 'thermal' | 'thermal_limit_pct'>): string =>
  h.state === 'unreachable' || h.state === 'unknown' ? '—' : h.thermal === 'throttled' ? `throttled${h.thermal_limit_pct != null ? ` ${h.thermal_limit_pct}%` : ''}` : h.thermal;

export const diskText = (h: Pick<FleetHost, 'state' | 'disk_free_gib' | 'disk_low'>): string =>
  h.state === 'unreachable' || h.state === 'unknown' || h.disk_free_gib == null ? '—' : `${h.disk_free_gib} GiB free${h.disk_low ? ' (low)' : ''}`;

/** The one action worth offering for a Mac: copy the command, never run it from the browser. */
export const primaryAction = (h: Pick<FleetHost, 'state' | 'commands'>): { label: string; command: string } | null => {
  switch (h.state) {
    case 'cordoned':
      return { label: 'Uncordon', command: h.commands.uncordon };
    case 'draining':
      return { label: 'Wait for empty', command: h.commands.drain };
    case 'unreachable':
      return { label: 'Diagnose', command: h.commands.doctor };
    case 'ready':
    case 'full':
      return { label: 'Cordon', command: h.commands.cordon };
    default:
      return null;
  }
};

export const ageText = (secs: number | null): string => {
  if (secs == null) return 'never';
  if (secs < 90) return `${secs}s ago`;
  if (secs < 5400) return `${Math.round(secs / 60)}m ago`;
  return `${Math.round(secs / 3600)}h ago`;
};

/** Pill tone for the whole answer: stale or missing data is a warning, not an error. */
export const viewTone = (v: Pick<FleetView, 'status' | 'summary'>): Tone =>
  v.status !== 'live' ? 'warn' : v.summary.unreachable > 0 ? 'bad' : 'ok';
