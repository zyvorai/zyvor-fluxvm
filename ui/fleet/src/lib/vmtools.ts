// Pure helpers for a VM's snapshots, clones and commands: what the node accepts, so the forms can refuse first.
// The screen helpers moved to @fluxvm/ui-core (the node Studio shows the same display); they are re-exported here.
export { type InputAction, type KeyLike, keyAction, mergeTypes, screenPoint, scrollNotches, typable } from '@fluxvm/ui-core';

export interface Snapshot {
  name: string;
  created_at?: string;
  size_bytes?: number;
  memory?: boolean;
}


export const SNAPSHOT_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;
export const CLONE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$/;
const NAME_RULE = 'Letters, digits, dot, dash and underscore, starting with a letter or digit.';

export function snapshotProblem(name: string, existing: string[], limit?: number): string {
  if (limit !== undefined && limit <= 0) return 'Snapshots are turned off for this VM.';
  if (limit !== undefined && existing.length >= limit) return `This VM keeps at most ${limit} snapshots. Delete one first.`;
  if (!name.trim()) return 'Give the snapshot a name.';
  if (!SNAPSHOT_NAME.test(name)) return `${NAME_RULE} Up to 64 characters.`;
  if (existing.includes(name)) return 'This VM already has a snapshot with that name.';
  return '';
}

/** `source` is a snapshot name, or '' for the VM's own disk, which is only a stable copy while the VM is stopped. */
export function cloneProblem(name: string, taken: string[], source: string, stopped: boolean): string {
  if (!name.trim()) return 'Give the new VM a name.';
  if (!CLONE_NAME.test(name)) return `${NAME_RULE} Up to 63 characters.`;
  if (taken.includes(name)) return 'A VM with that name already exists.';
  if (!source && !stopped) return 'Stop the VM first, or copy one of its snapshots.';
  return '';
}

export function suggestCloneName(name: string, taken: string[]): string {
  let n = `${name}-copy`;
  for (let i = 2; taken.includes(n); i++) n = `${name}-copy-${i}`;
  return n;
}

export const fmtBytes = (n?: number): string => {
  if (!n) return '–';
  if (n >= 2 ** 30) return `${(n / 2 ** 30).toFixed(n >= 100 * 2 ** 30 ? 0 : 1).replace(/\.0$/, '')} GiB`;
  return `${Math.max(1, Math.round(n / 2 ** 20))} MiB`;
};


export interface ExecResult {
  stdout?: string;
  stderr?: string;
  exit_code?: number;
  exit_status?: number;
}

export const exitOf = (r: ExecResult): number | '?' => r.exit_code ?? r.exit_status ?? '?';
