export type Role = 'operator' | 'tenant';
export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'idle';

export interface FleetVm {
  node: string;
  id: string;
  name: string;
  status?: unknown;
  state?: string;
  created_at?: string;
  request?: { vcpus?: number; memory_mib?: number; tenant?: string; image?: string; apple?: { guest_os?: string } };
}

export interface FleetNode {
  name: string;
  fluxvm_url?: string;
  vcpus_total?: number;
  memory_mib_total?: number;
  vm_count?: number;
  cordoned?: boolean;
  healthy?: boolean;
  outdated?: boolean;
  apple?: { macos_guests?: number } | null;
}

export interface Drain {
  draining?: boolean;
  scrub_pending?: boolean;
}

export interface Lease {
  id: string;
  node: string;
  tenant?: string;
  state: string;
  started_at: string;
  min_end: string;
}

export const vmKey = (v: Pick<FleetVm, 'node' | 'id'>): string => `${v.node}/${v.id}`;

/** A bare string, a `{state: …}` enum, or `state` on the record: the daemon has used all three. */
export function vmState(v: FleetVm): string {
  const s = typeof v.status === 'string' ? v.status : v.state || (v.status && typeof v.status === 'object' ? Object.keys(v.status as object)[0] : '') || 'unknown';
  return s.toLowerCase();
}

export const vmTone = (state: string): Tone =>
  ({ running: 'ok', creating: 'warn', starting: 'warn', paused: 'warn', stopped: 'idle', failed: 'bad' } as Record<string, Tone>)[state] ?? 'idle';

/** Power actions that make sense from a state, matching what central accepts at `/fleet/vms/{node}/{id}/{action}`. */
export function powerActions(state: string): string[] {
  if (state === 'stopped' || state === 'failed') return ['start'];
  if (state === 'paused') return ['resume', 'stop'];
  if (state === 'running') return ['stop', 'restart', 'pause'];
  return [];
}

export function nodeState(n: FleetNode, d: Drain = {}, lease?: Lease | null): { text: string; tone: Tone } {
  if (n.healthy === false) return { text: 'Not responding', tone: 'bad' };
  if (d.scrub_pending) return { text: 'Needs scrub', tone: 'warn' };
  if (d.draining) return { text: 'Draining', tone: 'warn' };
  if (n.cordoned) return { text: 'Cordoned', tone: 'warn' };
  if (lease) return { text: lease.tenant ? `Leased to ${lease.tenant}` : 'Leased', tone: 'ok' };
  return (n.vm_count ?? 0) > 0 ? { text: 'In use', tone: 'ok' } : { text: 'Available', tone: 'idle' };
}

/** How far through the minimum lease period (Apple's licence requires 24 h) a lease is. */
export function leaseProgress(l: Pick<Lease, 'started_at' | 'min_end'>, now = Date.now()): { p: number; releasable: boolean } {
  const start = Date.parse(l.started_at);
  const end = Date.parse(l.min_end);
  if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) return { p: 1, releasable: true };
  return { p: Math.min(1, Math.max(0, (now - start) / (end - start))), releasable: now >= end };
}

/** Tenant ids appear in URLs and network names. */
export const tenantIdProblem = (id: string): string => (/^[a-z0-9][a-z0-9-]{0,62}$/.test(id) ? '' : 'Lowercase letters, digits and dashes, starting with a letter or digit, up to 63 characters.');

export interface LaunchDraft {
  name: string;
  image: string;
  guest: string;
  vcpus: string;
  memoryGib: string;
  node: string;
  ttl: string;
  networks: string[];
  diskGib?: number;
}

export function launchProblem(d: LaunchDraft): string {
  if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$/.test(d.name.trim())) return 'Name: letters, digits, dot, dash and underscore.';
  if (!d.image.trim()) return 'Choose an image.';
  const c = Number(d.vcpus);
  if (!Number.isInteger(c) || c < 1 || c > 64) return 'vCPUs: a whole number from 1 to 64.';
  const m = Number(d.memoryGib);
  if (!(m >= 1) || m > 1024) return 'Memory: 1 GiB or more.';
  return '';
}

export function launchRequest(d: LaunchDraft): Record<string, unknown> {
  const req: Record<string, unknown> = {
    name: d.name.trim(),
    backend: 'vz',
    image: d.image.trim(),
    vcpus: Number(d.vcpus),
    memory_mib: Math.round(Number(d.memoryGib) * 1024),
    apple: { guest_os: d.guest, ...(d.networks.length ? { networks: d.networks.map((name) => ({ name })) } : {}) },
  };
  if (d.diskGib) req.disk_size_gib = d.diskGib;
  if (d.node) req.node = d.node;
  if (Number(d.ttl) > 0) req.ttl_seconds = Number(d.ttl);
  return req;
}

export const joinedList = (xs: string[]): string => (xs.length ? xs.join(', ') : 'none');
