// Shapes and helpers for the Network pages. Only the fields the UI reads; the daemon returns more.

export interface VnetConfig {
  vni: number;
  cidr: string;
  gateway: string;
  uplink?: string | null;
  snat_address?: string | null;
  floating_ips?: { address: string; ip: string }[];
  endpoints?: { ip: string; mac: string; node_ip: string }[];
}

export interface VnetStatus {
  config: VnetConfig;
  local_endpoints: string[];
  remote_endpoints: number;
  gateway_mac: string;
}

export interface SecurityGroup {
  name: string;
  labels?: string[];
  identity?: number;
  priority?: number;
  description?: string;
  policy?: { default_allow?: boolean; allow_cidrs?: string[]; deny_cidrs?: string[]; allow_ports?: string[]; allow_icmp?: boolean };
}

export interface ServiceBackend { address: string; port: number; weight?: number; enabled?: boolean; state?: 'ready' | 'draining' | 'unhealthy' }

export interface ServiceSpec {
  name: string;
  vip: string;
  port: number;
  protocol: string;
  algorithm?: string;
  mode?: string;
  exposure?: string;
  backends?: ServiceBackend[];
  advertise?: boolean;
}

export interface BackendHealth { service: string; backend_index: number; address: string; port: number; healthy: boolean; last_error?: string | null }

const IPV4 = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/;
export const isIpv4 = (s: string): boolean => {
  const m = IPV4.exec(s.trim());
  return !!m && m.slice(1).every((o) => Number(o) <= 255);
};

/** `a.b.c.d/n` with n in 0..32. */
export const isIpv4Cidr = (s: string): boolean => {
  const [ip, len, ...rest] = s.trim().split('/');
  return rest.length === 0 && len !== undefined && /^\d{1,2}$/.test(len) && Number(len) <= 32 && isIpv4(ip);
};

const ipNum = (s: string): number => s.split('.').reduce((a, o) => a * 256 + Number(o), 0);

/** Is `ip` inside `cidr`? Both IPv4. */
export function inCidr(ip: string, cidr: string): boolean {
  const [net, len] = cidr.split('/');
  const bits = Number(len);
  const size = 2 ** (32 - bits);
  return Math.floor(ipNum(ip) / size) === Math.floor(ipNum(net) / size);
}

export interface VnetDraft { vni: string; cidr: string; gateway: string; uplink: string; snat: string }

/** First problem with a virtual-network form, or '' when it can be sent. */
export function vnetProblem(d: VnetDraft): string {
  const vni = Number(d.vni);
  if (!/^\d+$/.test(d.vni.trim()) || vni < 1 || vni > 16777215) return 'VNI is a number from 1 to 16777215.';
  if (!isIpv4Cidr(d.cidr)) return 'Network must be an IPv4 CIDR such as 10.20.0.0/24.';
  if (!isIpv4(d.gateway)) return 'Gateway must be an IPv4 address.';
  if (!inCidr(d.gateway.trim(), d.cidr.trim())) return 'Gateway must be inside the network.';
  if (d.snat.trim() && !isIpv4(d.snat)) return 'SNAT address must be an IPv4 address.';
  if (d.snat.trim() && !d.uplink.trim()) return 'SNAT needs an uplink interface.';
  return '';
}

export const vnetBody = (d: VnetDraft, prev?: VnetConfig): VnetConfig => ({
  ...prev,
  vni: Number(d.vni),
  cidr: d.cidr.trim(),
  gateway: d.gateway.trim(),
  uplink: d.uplink.trim() || null,
  snat_address: d.snat.trim() || null,
  floating_ips: prev?.floating_ips ?? [],
  endpoints: prev?.endpoints ?? [],
});

export interface GroupDraft { name: string; labels: string; cidrs: string; ports: string; defaultAllow: boolean; priority: string; description: string }

const split = (s: string): string[] => s.split(/[\s,]+/).filter(Boolean);

export function groupProblem(d: GroupDraft): string {
  if (!/^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/.test(d.name.trim())) return 'Name is lowercase letters, digits and dashes.';
  if (split(d.labels).some((l) => !/^[^=\s]+=[^=\s]+$/.test(l))) return 'Labels are key=value.';
  const bad = split(d.cidrs).find((c) => !isIpv4Cidr(c) && !c.includes(':'));
  if (bad) return `${bad} is not a CIDR.`;
  const port = split(d.ports).find((p) => !/^(tcp|udp)\/\d{1,5}$/.test(p) || Number(p.split('/')[1]) > 65535);
  if (port) return `${port} should look like tcp/443.`;
  if (d.priority.trim() && !/^\d+$/.test(d.priority.trim())) return 'Priority is a whole number.';
  return '';
}

export const groupBody = (d: GroupDraft): SecurityGroup => ({
  name: d.name.trim(),
  labels: split(d.labels),
  priority: d.priority.trim() ? Number(d.priority) : 0,
  description: d.description.trim(),
  policy: { default_allow: d.defaultAllow, allow_cidrs: split(d.cidrs), allow_ports: split(d.ports) },
});

/** One line saying what a group lets through. */
export function describePolicy(p: SecurityGroup['policy']): string {
  if (!p) return 'no policy';
  const cidrs = p.allow_cidrs?.length ?? 0;
  const ports = p.allow_ports?.length ?? 0;
  const deny = p.deny_cidrs?.length ?? 0;
  const parts: string[] = [];
  // An allowlist makes unmatched traffic denied whatever default_allow says.
  if (cidrs || ports) parts.push(`allows ${[cidrs && `${cidrs} CIDR${cidrs > 1 ? 's' : ''}`, ports && `${ports} port${ports > 1 ? 's' : ''}`].filter(Boolean).join(' + ')}, denies the rest`);
  else parts.push(p.default_allow ? 'allows everything' : 'denies everything');
  if (deny) parts.push(`${deny} deny CIDR${deny > 1 ? 's' : ''}`);
  return parts.join('; ');
}

export type Health = 'healthy' | 'degraded' | 'down' | 'unknown';

/** Service health from the health report; a service without probes is `unknown`, not healthy. */
export function serviceHealth(name: string, report: BackendHealth[]): Health {
  const mine = report.filter((b) => b.service === name);
  if (!mine.length) return 'unknown';
  const up = mine.filter((b) => b.healthy).length;
  return up === mine.length ? 'healthy' : up === 0 ? 'down' : 'degraded';
}

/** Protocol number as the datapath reports it. */
export const protoName = (n: unknown): string => (n === 6 ? 'tcp' : n === 17 ? 'udp' : n === 1 ? 'icmp' : n === 58 ? 'icmpv6' : String(n ?? '—'));

export type JsonResult<T> = { ok: true; value: T } | { ok: false; error: string };

/** Parse a pasted JSON object and make sure the fields the daemon needs are there, before a request is wasted. */
export function parseObject(text: string, required: string[]): JsonResult<Record<string, unknown>> {
  let v: unknown;
  try {
    v = JSON.parse(text);
  } catch (e) {
    return { ok: false, error: `Not valid JSON: ${(e as Error).message}` };
  }
  if (!v || typeof v !== 'object' || Array.isArray(v)) return { ok: false, error: 'Expected a JSON object.' };
  const missing = required.filter((k) => (v as Record<string, unknown>)[k] == null);
  return missing.length ? { ok: false, error: `Missing: ${missing.join(', ')}.` } : { ok: true, value: v as Record<string, unknown> };
}

/** A service needs these to be accepted; the daemon checks the rest (VIP family, backend ports and so on). */
export const SERVICE_REQUIRED = ['name', 'vip', 'port', 'protocol'];

export const SERVICE_TEMPLATE = JSON.stringify(
  { name: 'api', vip: '10.96.0.10', port: 443, protocol: 'tcp', mode: 'nat', exposure: 'east-west', backends: [{ address: '10.20.0.5', port: 8443 }] },
  null,
  2,
);

export const CNP_TEMPLATE = JSON.stringify(
  { metadata: { name: 'allow-dns' }, spec: { endpointSelector: { matchLabels: {} }, egress: [{ toPorts: [{ ports: [{ port: '53', protocol: 'UDP' }] }] }] } },
  null,
  2,
);

/** Name of a Cilium policy from its metadata, for the confirm text. */
export const cnpName = (p: Record<string, unknown>): string => {
  const m = p.metadata;
  return m && typeof m === 'object' && typeof (m as { name?: unknown }).name === 'string' ? (m as { name: string }).name : '';
};
