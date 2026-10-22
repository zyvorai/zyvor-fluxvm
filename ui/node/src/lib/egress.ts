// Egress policy helpers for user-mode (slirp) guests. Mirrors the daemon's rules (IPv4 CIDRs, tcp/udp/any,
// optional port, at most 256 rules) so the UI can refuse bad input before the API does.
export type EgressMode = 'loopback' | 'allowlist' | 'audit' | 'open';
export type Proto = 'any' | 'tcp' | 'udp';
export interface EgressRule {
  cidr: string;
  proto?: Proto;
  port?: number | null;
  note?: string | null;
}
export interface EgressPolicy {
  mode: EgressMode;
  rules: EgressRule[];
}
export interface EgressView {
  policy: EgressPolicy;
  source?: string;
  running?: boolean;
  programmed?: { mode?: string } | null;
}
export interface ObservedFlow {
  dst: string;
  proto: string;
  port: number;
  packets: number;
  bytes: number;
  audit: boolean;
  last_ns?: number;
}

export const MAX_RULES = 256;

export const MODES: { value: EgressMode; label: string; blurb: string; tone: 'ok' | 'warn' | 'bad' | 'info' }[] = [
  { value: 'loopback', label: 'Isolated', blurb: 'The guest reaches nothing outside the host.', tone: 'ok' },
  { value: 'allowlist', label: 'Allow-list', blurb: 'Only the destinations below are reachable.', tone: 'info' },
  { value: 'audit', label: 'Learn', blurb: 'Everything is allowed and recorded, so you can build a list from real traffic.', tone: 'warn' },
  { value: 'open', label: 'Open', blurb: 'Unrestricted outbound access.', tone: 'bad' },
];

export const modeInfo = (m: EgressMode) => MODES.find((x) => x.value === m) ?? MODES[0];

const OCTET = '(25[0-5]|2[0-4]\\d|1\\d\\d|[1-9]?\\d)';
const CIDR_RE = new RegExp(`^${OCTET}\\.${OCTET}\\.${OCTET}\\.${OCTET}(?:/(\\d|[12]\\d|3[0-2]))?$`);

/** `10.0.0.0/16`, `1.2.3.4` (→ /32). Returns the normalised CIDR or null. IPv4 only. */
export function normalizeCidr(input: string): string | null {
  const s = input.trim();
  if (!CIDR_RE.test(s)) return null;
  return s.includes('/') ? s : `${s}/32`;
}

export function validateRule(r: EgressRule): string {
  if (!normalizeCidr(r.cidr)) return 'Enter an IPv4 address or CIDR, such as 10.0.0.0/16.';
  if (r.port != null && !(Number.isInteger(r.port) && r.port >= 1 && r.port <= 65535)) return 'Port must be 1–65535.';
  if (r.port != null && (r.proto ?? 'any') === 'any') return 'Pick tcp or udp when you set a port.';
  return '';
}

export const ruleKey = (r: EgressRule): string => `${normalizeCidr(r.cidr) ?? r.cidr}|${r.proto ?? 'any'}|${r.port ?? ''}`;

/** Adds a rule unless an identical one exists; returns the new list or an error. */
export function addRule(rules: EgressRule[], r: EgressRule): { rules: EgressRule[]; error: string } {
  const bad = validateRule(r);
  if (bad) return { rules, error: bad };
  if (rules.length >= MAX_RULES) return { rules, error: `At most ${MAX_RULES} rules.` };
  const clean: EgressRule = { cidr: normalizeCidr(r.cidr)!, proto: r.proto ?? 'any', ...(r.port != null ? { port: r.port } : {}), ...(r.note?.trim() ? { note: r.note.trim() } : {}) };
  if (rules.some((x) => ruleKey(x) === ruleKey(clean))) return { rules, error: 'That rule is already in the list.' };
  return { rules: [...rules, clean], error: '' };
}

export const describeRule = (r: EgressRule): string => `${r.cidr}${r.proto && r.proto !== 'any' ? ` ${r.proto}` : ''}${r.port ? `/${r.port}` : ''}`;

/** Port-folding in the datapath allows at most 4 distinct (proto, port) pairs under one prefix; warn early. */
export function crowdedPrefixes(rules: EgressRule[], max = 4): string[] {
  const by = new Map<string, Set<string>>();
  for (const r of rules) {
    const k = normalizeCidr(r.cidr) ?? r.cidr;
    if (!by.has(k)) by.set(k, new Set());
    by.get(k)!.add(`${r.proto ?? 'any'}/${r.port ?? '*'}`);
  }
  return [...by].filter(([, s]) => s.size > max).map(([k]) => k);
}

export const fmtBytes = (n: number): string => (n >= 1 << 30 ? `${(n / (1 << 30)).toFixed(1)} GiB` : n >= 1 << 20 ? `${(n / (1 << 20)).toFixed(1)} MiB` : n >= 1024 ? `${(n / 1024).toFixed(1)} KiB` : `${n} B`);

/** Quick starting points; rules for a platform/mirror allow-list are environment-specific, so none are baked in. */
export const PRESETS: { id: string; label: string; blurb: string; policy: EgressPolicy }[] = [
  { id: 'isolated', label: 'Isolated', blurb: 'No outbound access.', policy: { mode: 'loopback', rules: [] } },
  { id: 'learn', label: 'Learn', blurb: 'Allow and record, then review what the guest needed.', policy: { mode: 'audit', rules: [] } },
  { id: 'web', label: 'Web only', blurb: 'Any host on tcp/80 and tcp/443.', policy: { mode: 'allowlist', rules: [{ cidr: '0.0.0.0/0', proto: 'tcp', port: 443, note: 'HTTPS' }, { cidr: '0.0.0.0/0', proto: 'tcp', port: 80, note: 'HTTP' }, { cidr: '0.0.0.0/0', proto: 'udp', port: 53, note: 'DNS' }] } },
  { id: 'open', label: 'Open', blurb: 'No restrictions.', policy: { mode: 'open', rules: [] } },
];

export const samePolicy = (a: EgressPolicy, b: EgressPolicy): boolean => a.mode === b.mode && a.rules.length === b.rules.length && a.rules.every((r, i) => ruleKey(r) === ruleKey(b.rules[i]));
