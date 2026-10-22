// Helpers for the Branches and Volumes pages.

export interface Branch {
  id: string;
  name: string;
  source_vm: string;
  source_name: string;
  parent_branch?: string | null;
  created_at: string;
  labels?: Record<string, string>;
  vcpus: number;
  memory_mib: number;
  state_bytes: number;
}

export interface PrunePolicy { keep_last?: number; older_than_secs?: number; name_prefix?: string }
export interface PruneReport { dry_run: boolean; selected: { id: string; name: string; reason: string; state_bytes: number }[]; deleted: string[]; failed: [string, string][]; protected: number; selected_state_bytes: number }
export interface ForkPlan { ok: boolean; count: number; parallelism: number; refusals: { code: string; reason: string }[]; predicted: { extra_rss_mib_low: number; extra_rss_mib_high: number; host_available_mib?: number | null; est_total_ms?: number | null } }

export interface DaemonVolume { name: string; size_gib: number; allocated_bytes: number; attached_to: string[]; format?: string }

const whole = (s: string): boolean => /^\d+$/.test(s.trim());

export interface StartDraft { count: string; prefix: string }

export function startProblem(d: StartDraft): string {
  if (!whole(d.count) || Number(d.count) < 1 || Number(d.count) > 256) return 'Count is a whole number from 1 to 256.';
  if (d.prefix.trim() && !/^[a-z0-9][a-z0-9-]{0,40}$/.test(d.prefix.trim())) return 'Name prefix: lowercase letters, digits and dashes.';
  return '';
}

export const startBody = (d: StartDraft) => ({ count: Number(d.count), ...(d.prefix.trim() ? { name_prefix: d.prefix.trim() } : {}) });

export interface PruneDraft { keepLast: string; olderDays: string; prefix: string }

/** A prune with no limit would select everything; refuse it before asking the daemon. */
export function pruneProblem(d: PruneDraft): string {
  if (!d.keepLast.trim() && !d.olderDays.trim() && !d.prefix.trim()) return 'Set at least one limit: keep the newest N, older than N days, or a name prefix.';
  if (d.keepLast.trim() && !whole(d.keepLast)) return 'Keep is a whole number.';
  if (d.olderDays.trim() && (!whole(d.olderDays) || Number(d.olderDays) < 1)) return 'Older than is a whole number of days.';
  return '';
}

export const pruneBody = (d: PruneDraft, dryRun: boolean) => ({
  dry_run: dryRun,
  ...(d.keepLast.trim() ? { keep_last: Number(d.keepLast) } : {}),
  ...(d.olderDays.trim() ? { older_than_secs: Number(d.olderDays) * 86400 } : {}),
  ...(d.prefix.trim() ? { name_prefix: d.prefix.trim() } : {}),
});

/** Is a branch protected from pruning by its label? */
export const isKept = (b: Branch): boolean => b.labels?.['fluxvm.dev/keep'] === 'true';

/** Branch names by id, so a lineage column can say a name instead of a UUID. */
export const nameById = (bs: Branch[]): Map<string, string> => new Map(bs.map((b) => [b.id, b.name]));

/** The children of `id` in a list, for the lineage view. */
export const childrenOf = (bs: Branch[], id: string): Branch[] => bs.filter((b) => b.parent_branch === id);

/** One sentence from a fork plan, for the confirm step. */
export function planSummary(p: ForkPlan): string {
  if (!p.ok) return p.refusals.map((r) => r.reason).join(' ') || 'The daemon would refuse this start.';
  const { extra_rss_mib_low: lo, extra_rss_mib_high: hi, host_available_mib: avail } = p.predicted;
  const mem = lo === hi ? `${lo} MiB` : `${lo}–${hi} MiB`;
  const tail = avail != null ? `, ${avail} MiB free now` : '';
  return `${p.count} ${p.count === 1 ? 'machine' : 'machines'} will add about ${mem} of memory${tail}.`;
}

export function volumeNameProblem(name: string, size: string): string {
  const n = name.trim();
  if (!/^[a-z0-9][a-z0-9-]{0,62}$/.test(n)) return 'Name: 1–63 lowercase letters, digits and dashes, starting with a letter or digit.';
  if (!whole(size) || Number(size) < 1 || Number(size) > 16384) return 'Size is a whole number of GiB, 1 to 16384.';
  return '';
}

export function volumeGrowProblem(current: number, next: string, attached: boolean): string {
  if (attached) return 'Stop or detach the VM using it first.';
  if (!whole(next) || Number(next) > 16384) return 'Size is a whole number of GiB, up to 16384.';
  if (Number(next) <= current) return `Must be larger than ${current} GiB; volumes only grow.`;
  return '';
}
