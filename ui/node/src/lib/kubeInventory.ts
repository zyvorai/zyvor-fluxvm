// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import type { Tone } from './kube';

/** One Secret as the daemon reports it: names only. A value never reaches the browser. */
export type SecretRow = { namespace: string; name: string; type: string; keys: string[]; created: string | null };

export type SecretsAnswer = {
  schema: number;
  cluster: string;
  secrets?: SecretRow[];
  fluxctl: string;
  set?: string;
  truncated?: boolean;
  error?: { message: string; next: string };
};

export type CertState = 'ok' | 'expiring' | 'expired' | 'missing' | 'invalid';

export type CertRow = {
  namespace: string;
  secret: string;
  used_by: string[];
  hosts: string[];
  not_after: number | null;
  days_left: number | null;
  state: CertState;
  summary: string;
  fix: string;
};

export type CertsAnswer = {
  schema: number;
  cluster: string;
  certs?: CertRow[];
  problems?: number;
  warn_days?: number;
  fluxctl: string;
  error?: { message: string; next: string };
};

export const certTone = (s: CertState): Tone => (s === 'ok' ? 'ok' : s === 'expiring' ? 'warn' : 'bad');

/** Words for the state, the way `fluxctl kube certs` prints it. */
export const certLabel = (s: CertState): string => ({ ok: 'Valid', expiring: 'Expiring', expired: 'Expired', missing: 'Missing', invalid: 'Unreadable' })[s];

/** `2026-12-01`, or a dash for a certificate that could not be read. */
export const endDate = (unix: number | null): string => (unix == null ? '—' : new Date(unix * 1000).toISOString().slice(0, 10));

/** One sentence for the top of the page. */
export const certsHeadline = (rows: CertRow[]): string => {
  if (!rows.length) return 'No Ingress or Gateway in this cluster serves a certificate from a Secret.';
  const bad = rows.filter((r) => r.state !== 'ok').length;
  const n = `${rows.length} certificate${rows.length === 1 ? '' : 's'}`;
  return bad ? `${n}, ${bad} need${bad === 1 ? 's' : ''} attention.` : `${n}, all valid.`;
};

/** Secrets worth a look first are not ranked; the filter matches name, namespace, type and key names. */
export const filterSecrets = (rows: SecretRow[], namespace: string, text: string): SecretRow[] => {
  const q = text.trim().toLowerCase();
  return rows.filter((r) => (!namespace || r.namespace === namespace) && (!q || `${r.namespace}/${r.name} ${r.type} ${r.keys.join(' ')}`.toLowerCase().includes(q)));
};

export const namespacesOfSecrets = (rows: SecretRow[]): string[] => [...new Set(rows.map((r) => r.namespace))].sort();

const word = (s: string): string => (/^[A-Za-z0-9._-]+$/.test(s) ? s : "'" + s.replace(/'/g, "'\\''") + "'");

/** `fluxctl kube secrets set` for a Secret: the value is asked for at a hidden prompt, so it is not part of the command. */
export const setSecretCommand = (cluster: string, namespace: string, name: string, key: string): string => `fluxctl kube secrets set --cluster ${word(cluster)} -n ${word(namespace)} ${word(name)} ${word(key)}`;

export const eventsCommand = (cluster: string, warningsOnly: boolean, namespace: string): string =>
  `fluxctl kube events --cluster ${word(cluster)}${namespace ? ` -n ${word(namespace)}` : ''}${warningsOnly ? ' --warnings' : ''}`;

export const topCommand = (cluster: string, what: 'pods' | 'nodes'): string => `fluxctl kube top ${what} --cluster ${word(cluster)}`;
