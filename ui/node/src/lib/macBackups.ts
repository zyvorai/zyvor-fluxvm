// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/mac/backups` (the same backups as `fluxctl mac backup ls --json`) and the words and commands the Backups card
// shows. Pure functions, no network.

import type { Tone } from './kube';

export type BackupState = 'ok' | 'running' | 'failed' | 'damaged';

export type BackupRow = {
  id: string;
  state: BackupState;
  kind: 'instance' | 'volume' | null;
  name: string | null;
  age_seconds: number | null;
  error: string | null;
  restore: string;
  verify: string;
  manifest: null | {
    created_unix: number;
    automatic: boolean;
    note: string | null;
    sha256: string;
    archive_bytes: number;
    source_bytes: number;
    captured: string[];
    excluded: string[];
  };
};

export type BackupSchedule = { kind: 'instance' | 'volume'; name: string; every_secs: number; keep: number; enabled: boolean; last_error: string | null };

export type BackupsView = {
  schema: number;
  supported: boolean;
  root: string;
  /** Free bytes on the disk that holds the backups; null when unknown. */
  free_bytes: number | null;
  reserve_bytes: number;
  default_keep: number;
  backups: BackupRow[];
  schedules: BackupSchedule[];
  instances: { name: string; status: string }[];
  volumes: string[];
  excluded: string[];
};

export const LS_COMMAND = 'fluxctl mac backup ls';

export const createCommand = (kind: 'instance' | 'volume', name: string, keep?: number): string =>
  `fluxctl mac backup create ${kind === 'volume' ? '--volume ' : ''}${name}${keep && keep !== 7 ? ` --keep ${keep}` : ''}`;

export const scheduleCommand = (kind: 'instance' | 'volume', name: string, every: string, keep = 7): string =>
  `fluxctl mac backup schedule ${kind === 'volume' ? '--volume ' : ''}${name} --every ${every}${keep !== 7 ? ` --keep ${keep}` : ''}`;

export const stateTone = (s: BackupState): Tone => (s === 'ok' ? 'ok' : s === 'running' ? 'warn' : 'bad');

export const stateWord = (s: BackupState): string => (s === 'ok' ? 'good' : s === 'running' ? 'being written' : s === 'failed' ? 'failed' : 'damaged');

export const bytesText = (b: number | null | undefined): string => {
  if (b == null || !Number.isFinite(b)) return '—';
  const K = 1024;
  if (b >= K ** 3) return `${(b / K ** 3).toFixed(1)} GiB`;
  if (b >= K ** 2) return `${(b / K ** 2).toFixed(1)} MiB`;
  if (b >= K) return `${(b / K).toFixed(1)} KiB`;
  return `${b} B`;
};

export const ageWords = (secs: number | null): string => {
  if (secs == null) return '—';
  if (secs < 45) return 'just now';
  if (secs < 3600) return `${Math.max(1, Math.round(secs / 60))}m ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ago`;
  return `${Math.floor(secs / 86400)}d ago`;
};

export const everyWords = (secs: number): string => (secs % 86400 === 0 ? `${secs / 86400}d` : secs % 3600 === 0 ? `${secs / 3600}h` : `${Math.round(secs / 60)}m`);

/** A one-line verdict on the backups disk: the reserve is kept free for macOS, so what is left for a backup is free minus the reserve. */
export const roomWords = (v: Pick<BackupsView, 'free_bytes' | 'reserve_bytes'>): { tone: Tone; text: string } => {
  if (v.free_bytes == null) return { tone: 'idle', text: 'free space unknown' };
  const usable = v.free_bytes - v.reserve_bytes;
  if (usable <= 0) return { tone: 'bad', text: `${bytesText(v.free_bytes)} free: below the ${bytesText(v.reserve_bytes)} kept free for macOS, so no backup can be taken until you free room` };
  return { tone: usable < 20 * 1024 ** 3 ? 'warn' : 'ok', text: `${bytesText(v.free_bytes)} free, ${bytesText(usable)} usable for backups` };
};

/** Instances can only be backed up while stopped. */
export const canBackUp = (status: string): boolean => status === 'stopped' || status === 'failed';

export const explainBackupError = (name: string, message: string): string => (/admin/i.test(message) ? 'Only an administrator can manage backups.' : `${name}: ${message}`);
