// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

// The Images security view: one verdict badge per cached OCI image, from `GET /v1/oci/security`.

export type Badge = 'blocked' | 'risk' | 'unsigned' | 'unscanned' | 'ok' | 'error';
export type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'idle';

export interface Finding { id: string; package: string; version: string; severity: string; fixed?: string | null }
export interface DataStatus { present: boolean; age_days?: number | null; advisories: number; source: string }
export interface Assessment {
  reference: string;
  manifest_digest: string;
  signature: string;
  signer?: string | null;
  packages?: number | null;
  scan?: { findings: Finding[]; counts: Record<string, number>; advisory: DataStatus } | null;
  decision?: { allowed: boolean; violations: { code: string; message: string }[]; warnings: string[] } | null;
  badge: Badge;
  summary: string;
  error?: string | null;
}
export interface SecurityView {
  items: Assessment[];
  policy: { present: boolean; path: string; error?: string | null };
  advisory: DataStatus;
  overrides: { unix: number; reference: string; actor: string }[];
}

export const BADGE_WORD: Record<Badge, string> = {
  blocked: 'Blocked by policy',
  risk: 'Known problems',
  unsigned: 'Not verified',
  unscanned: 'Not scanned',
  ok: 'Looks good',
  error: 'Could not check',
};

export const BADGE_TONE: Record<Badge, Tone> = { blocked: 'bad', risk: 'bad', unsigned: 'warn', unscanned: 'info', ok: 'ok', error: 'idle' };

export const badgeWord = (b: string): string => BADGE_WORD[b as Badge] ?? 'Unknown';
export const badgeTone = (b: string): Tone => BADGE_TONE[b as Badge] ?? 'idle';

/** "advisory data is 12 days old", or the plain statement that there is none (never "clean"). */
export function advisoryLine(a: DataStatus | undefined | null): string {
  if (!a || !a.present) return 'There is no advisory data on this machine, so nothing has been checked for known problems.';
  const n = a.age_days;
  const age = n == null ? 'has no date' : n === 0 ? 'is from today' : n === 1 ? 'is 1 day old' : `is ${n} days old`;
  return `Advisory data ${age} (${a.advisories} advisories).`;
}

export function signatureWord(sig: string): string {
  switch (sig) {
    case 'trusted': return 'Signed by a trusted key';
    case 'unsigned': return 'No signature';
    case 'untrusted-signer': return 'Signed by an unknown key';
    case 'invalid': return 'Signature does not match';
    default: return 'Not checked yet';
  }
}

/** "1 critical, 2 high" in severity order. */
export function countsLine(counts: Record<string, number> | undefined): string {
  const order = ['critical', 'high', 'medium', 'low', 'unknown'];
  const parts = order.filter((s) => counts?.[s]).map((s) => `${counts![s]} ${s}`);
  return parts.length ? parts.join(', ') : 'none';
}

const q = (a: string) => (/^[\w./:@=,-]+$/.test(a) ? a : `'${a.replace(/'/g, `'\\''`)}'`);

/** The command that fixes or explains a badge, to copy into a terminal. */
export function nextCommand(a: Assessment): string {
  const ref = q(a.reference);
  switch (a.badge) {
    case 'blocked': return `fluxctl image policy check ${ref}`;
    case 'risk': return `fluxctl image scan ${ref}`;
    case 'unsigned': return `fluxctl image verify ${ref}`;
    case 'unscanned': return 'fluxctl image scan --update --source FILE_OR_URL';
    default: return `fluxctl image sbom ${ref}`;
  }
}

export const sbomCommand = (a: Assessment, format = 'spdx'): string => `fluxctl image sbom ${q(a.reference)} --sbom-format ${format}`;
export const verifyCommand = (a: Assessment): string => `fluxctl image verify ${q(a.reference)}`;
export const scanCommand = (a: Assessment): string => `fluxctl image scan ${q(a.reference)}`;
export const POLICY_INIT_COMMAND = 'fluxctl image policy init';
export const UPDATE_COMMAND = 'fluxctl image scan --update --source FILE_OR_URL';

/** The route that returns an image's SBOM. */
export const sbomPath = (digest: string, format: 'spdx' | 'cyclonedx' = 'spdx'): string => `/v1/oci/images/${encodeURIComponent(digest)}/sbom?format=${format}`;

/** Worst problems first, then unsigned, so what needs attention is on top. */
export function sortAssessments(items: Assessment[]): Assessment[] {
  const rank: Record<string, number> = { blocked: 0, risk: 1, unsigned: 2, unscanned: 3, error: 4, ok: 5 };
  return [...items].sort((a, b) => (rank[a.badge] ?? 9) - (rank[b.badge] ?? 9) || a.reference.localeCompare(b.reference));
}
