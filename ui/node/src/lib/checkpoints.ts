// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// Checkpoints are the daemon's snapshots (`/v1/vms/{id}/snapshot|snapshots|restore`) in plain words. The same names and the same
// safety net as `fluxctl mac checkpoint`: a restore saves the current state first, as `auto-before-restore-<time>`.

export interface Snap {
  tag: string;
  created_at?: string | null;
  size_bytes?: number;
  memory?: boolean;
  has_memory?: boolean | null;
}

/** Saved by the daemon itself as the safety net before a restore (the newest three are kept). */
export const isAuto = (tag: string): boolean => tag.startsWith('auto-');

export type SnapKind = 'memory' | 'disk' | 'unknown';

export function snapKind(s: Snap): SnapKind {
  const m = s.has_memory ?? s.memory;
  if (m === true) return 'memory';
  if (m === false) return 'disk';
  return 'unknown';
}

export function kindLabel(s: Snap): string {
  const k = snapKind(s);
  return k === 'memory' ? 'with memory' : k === 'disk' ? 'disk only' : '';
}

/** `ckpt-YYYYMMDD-HHMMSS` in UTC, the CLI's default name. */
export function defaultTag(now: Date): string {
  const p = (n: number) => String(n).padStart(2, '0');
  return `ckpt-${now.getUTCFullYear()}${p(now.getUTCMonth() + 1)}${p(now.getUTCDate())}-${p(now.getUTCHours())}${p(now.getUTCMinutes())}${p(now.getUTCSeconds())}`;
}

/** The daemon's tag rule, so the button can be disabled with a reason instead of failing. */
export function tagProblem(tag: string): string | null {
  const t = tag.trim();
  if (!t) return 'Give the checkpoint a name.';
  if (t.length > 128 || t.startsWith('.') || !/^[A-Za-z0-9._-]+$/.test(t)) {
    return "Use letters, digits, '.', '_' or '-' (no spaces), not starting with '.'.";
  }
  return null;
}

/** The body of `POST /v1/vms/{id}/snapshot`. A stopped machine can only save its disk; a running one saves memory unless asked not to. */
export function saveBody(tag: string, running: boolean, diskOnly: boolean): { tag: string; memory: boolean } {
  return { tag: tag.trim(), memory: running && !diskOnly };
}

/** What the restore confirmation says. */
export function restoreText(s: Snap, running: boolean): string {
  const how = snapKind(s) === 'memory' ? 'The machine resumes right where it was when you took it.' : 'The machine starts again from the saved disk (a macOS boot takes minutes).';
  const now = running ? 'It is stopped first, and what it is doing now is saved as an automatic checkpoint, so you can undo this.' : 'What it holds now is saved as an automatic checkpoint first, so you can undo this.';
  return `${how} ${now}`;
}

/** The toast after `POST /v1/vms/{id}/restore`: says how to roll back when the daemon took a safety checkpoint. */
export function restoredMessage(reply: unknown, tag: string): string {
  const r = (reply ?? {}) as { safety_checkpoint?: string };
  return r.safety_checkpoint
    ? `Restored ${tag}. Your previous state is saved as ${r.safety_checkpoint}: restore that one to go back.`
    : `Restored ${tag}.`;
}

/** CLI equivalent, for the "Copy as fluxctl" hint. */
export const cliSave = (name: string, tag: string, diskOnly: boolean): string => `fluxctl mac checkpoint ${name} ${tag}${diskOnly ? ' --disk-only' : ''}`;
export const cliRestore = (name: string, tag: string): string => `fluxctl mac checkpoint restore ${name} ${tag}`;
