export const fmtMiB = (mib: number): string => (mib >= 1024 ? `${+(mib / 1024).toFixed(1)} GiB` : `${mib} MiB`);

export function ago(ts: number | string | Date, now = Date.now()): string {
  const t = ts instanceof Date ? ts.getTime() : typeof ts === 'number' ? ts : Date.parse(ts);
  if (!Number.isFinite(t)) return '—';
  const s = Math.max(0, Math.round((now - t) / 1000));
  if (s < 5) return 'just now';
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

/** Status → pill tone. */
export function statusTone(status: string): 'ok' | 'warn' | 'bad' | 'info' | 'idle' {
  switch (status) {
    case 'running':
      return 'ok';
    case 'creating':
    case 'starting':
    case 'stopping':
    case 'migrating':
      return 'info';
    case 'paused':
    case 'hibernated':
      return 'warn';
    case 'failed':
      return 'bad';
    default:
      return 'idle';
  }
}
