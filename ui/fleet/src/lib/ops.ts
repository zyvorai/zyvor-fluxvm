// Helpers for the fleet Volumes, Tunnels, Pools and Enrollment pages.

export interface Capacity {
  nodes_total: number;
  nodes_healthy: number;
  nodes_cordoned: number;
  nodes_schedulable: number;
  vm_count: number;
  vcpus_total: number;
  vcpus_used: number;
  vcpus_free: number;
  memory_mib_total: number;
  memory_mib_used: number;
  memory_mib_free: number;
}

export interface Volume { id: string; name: string; tenant?: string | null; node: string; size_gib: number; attached_to?: string | null; creating?: boolean; format?: string; created_at?: string }

export interface Tunnel { id: string; tenant?: string | null; user?: string | null; node: string; vm_name: string; port: number; opened_at?: string; idle_seconds?: number; bytes_sent?: number; bytes_received?: number }

export interface JoinToken { id: string; created: number; expires?: number | null; max_uses?: number | null; uses: number; name_pattern?: string | null; revoked: boolean; note?: string }

export interface EnrolledNode { name: string; node_id: string; token_id: string; generation: number; revoked: boolean; enrolled_at: number }

/** Whole-number percentage, clamped to 0..100; 0 when there is nothing to divide by. */
export const pct = (used: number, total: number): number => (total > 0 ? Math.max(0, Math.min(100, Math.round((used / total) * 100))) : 0);

/** Meter tone: a fleet running hot should say so before it refuses a launch. */
export const loadTone = (p: number): 'ok' | 'warn' | 'bad' => (p >= 90 ? 'bad' : p >= 75 ? 'warn' : 'ok');

export function fmtBytes(n: number | undefined): string {
  if (n == null || !Number.isFinite(n)) return '—';
  const u = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) (v /= 1024, i++);
  return `${i === 0 ? v : v.toFixed(v < 10 ? 1 : 0)} ${u[i]}`;
}

export function idleText(seconds: number | undefined): string {
  if (seconds == null) return '—';
  if (seconds < 5) return 'active';
  if (seconds < 90) return `${seconds}s idle`;
  if (seconds < 5400) return `${Math.round(seconds / 60)} min idle`;
  return `${Math.round(seconds / 3600)} h idle`;
}

export function volumeProblem(name: string, size: string): string {
  const n = name.trim();
  if (!n) return 'Give the volume a name.';
  if (n.length > 64 || /[\s/\\]/.test(n)) return 'Name: up to 64 characters, no spaces or slashes.';
  if (!/^\d+$/.test(size.trim()) || Number(size) < 1) return 'Size is a whole number of GiB, at least 1.';
  return '';
}

/** A volume only grows, and only while detached; the file system inside the guest is not grown for you. */
export function resizeProblem(current: number, next: string, attached: boolean): string {
  if (attached) return 'Detach the volume first: only a detached volume can grow.';
  if (!/^\d+$/.test(next.trim())) return 'Size is a whole number of GiB.';
  if (Number(next) <= current) return `Must be larger than ${current} GiB; volumes only grow.`;
  return '';
}

export interface TokenDraft { hours: string; uses: string; pattern: string; note: string }

export function tokenProblem(d: TokenDraft): string {
  if (d.hours.trim() && (!/^\d+$/.test(d.hours.trim()) || Number(d.hours) < 1)) return 'Lifetime is a whole number of hours, or empty for none.';
  if (d.uses.trim() && (!/^\d+$/.test(d.uses.trim()) || Number(d.uses) < 1)) return 'Uses is a whole number, or empty for unlimited.';
  return '';
}

export const tokenBody = (d: TokenDraft) => ({
  ...(d.hours.trim() ? { ttl_secs: Number(d.hours) * 3600 } : {}),
  ...(d.uses.trim() ? { max_uses: Number(d.uses) } : {}),
  ...(d.pattern.trim() ? { name_pattern: d.pattern.trim() } : {}),
  note: d.note.trim(),
});

export type TokenState = 'active' | 'revoked' | 'expired' | 'used up';

export function tokenState(t: JoinToken, nowSec: number): TokenState {
  if (t.revoked) return 'revoked';
  if (t.expires != null && t.expires <= nowSec) return 'expired';
  if (t.max_uses != null && t.uses >= t.max_uses) return 'used up';
  return 'active';
}

export const usesText = (t: JoinToken): string => `${t.uses}${t.max_uses != null ? ` / ${t.max_uses}` : ''}`;
