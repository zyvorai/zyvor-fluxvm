// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The shape of `GET /v1/k3s/backups` (schema 1): the k3s clusters' backups, the same data as `fluxctl kube backups --json`.

import type { Tone } from './health';

export type BackupKind = 'manual' | 'auto' | 'pre_upgrade' | 'pre_restore';

export interface BackupEntry {
  name: string;
  path: string;
  kind: BackupKind;
  bytes: number;
  created_epoch: number;
  age_secs: number;
  has_checksum: boolean;
}

export interface BackupSchedule {
  enabled: boolean;
  every_hours: number;
  keep: number;
  keep_pre_upgrade: number;
}

export interface ClusterBackups {
  name: string;
  schedule: BackupSchedule | null;
  backups: BackupEntry[];
  total_bytes: number;
  newest_age_secs: number | null;
  disk_free_bytes: number | null;
  commands: { backup: string; list: string; verify: string; restore_latest: string };
}

export interface BackupListing {
  schema: number;
  clusters: ClusterBackups[];
}

export interface VerifyCheck {
  name: string;
  ok: boolean;
  detail: string;
}

export interface VerifyReport {
  file: string;
  restorable: boolean;
  datastore: string | null;
  sha256: string | null;
  checks: VerifyCheck[];
}

const KIND_WORD: Record<BackupKind, string> = { manual: 'Manual', auto: 'Scheduled', pre_upgrade: 'Before upgrade', pre_restore: 'Before restore' };
export const kindWord = (k: BackupKind): string => KIND_WORD[k] ?? k;

export function sizeWord(bytes: number): string {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(1)} GiB`;
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(1)} MiB`;
  return `${Math.max(1, Math.ceil(bytes / 1024))} KiB`;
}

export function ageWord(secs: number): string {
  if (secs < 90) return 'just now';
  if (secs < 5400) return `${Math.round(secs / 60)} min ago`;
  if (secs < 36 * 3600) return `${Math.round(secs / 3600)} h ago`;
  return `${Math.round(secs / 86400)} days ago`;
}

/** How the newest backup looks against the schedule: the tone and the sentence for the card's header. */
export function freshness(c: ClusterBackups): { tone: Tone; text: string } {
  if (c.newest_age_secs === null) return { tone: 'warn', text: 'No backup yet' };
  const every = c.schedule?.enabled ? c.schedule.every_hours * 3600 : 7 * 86400;
  if (c.newest_age_secs > every * 2 + 3600) return { tone: 'bad', text: `Last backup ${ageWord(c.newest_age_secs)}, overdue` };
  return { tone: 'ok', text: `Last backup ${ageWord(c.newest_age_secs)}` };
}

export function scheduleWord(s: BackupSchedule | null): string {
  if (!s) return 'Automatic backups are not set up for this cluster.';
  if (!s.enabled) return 'Automatic backups are off.';
  const every = s.every_hours % 24 === 0 ? (s.every_hours === 24 ? 'day' : `${s.every_hours / 24} days`) : `${s.every_hours} hours`;
  return `A backup is taken every ${every}; the newest ${s.keep} are kept (and ${s.keep_pre_upgrade} before upgrades).`;
}

/** Two-step confirm: the restore button only works once the backup's name is typed again. */
export const restoreConfirmed = (typed: string, name: string): boolean => typed.trim() === name;

/** The terminal command for a restore of this backup. */
export function restoreCommand(clusterName: string, name: string): string {
  return `fluxctl kube restore ${name}${clusterName === 'default' ? '' : ` --k3s-cluster ${clusterName}`} --yes`;
}

export const backupBase = (cluster: string): string => `/v1/k3s/${encodeURIComponent(cluster)}/backups`;
