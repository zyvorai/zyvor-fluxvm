// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type CertRow, type SecretRow, certLabel, certTone, certsHeadline, endDate, eventsCommand, filterSecrets, namespacesOfSecrets, setSecretCommand, topCommand } from './kubeInventory';

const secret = (name: string, over: Partial<SecretRow> = {}): SecretRow => ({ namespace: 'shop', name, type: 'Opaque', keys: ['password'], created: null, ...over });
const cert = (state: CertRow['state']): CertRow => ({ namespace: 'shop', secret: 's', used_by: ['ingress web'], hosts: [], not_after: null, days_left: null, state, summary: '', fix: '' });

describe('certificates', () => {
  it('colours and words every state', () => {
    expect(certTone('ok')).toBe('ok');
    expect(certTone('expiring')).toBe('warn');
    for (const s of ['expired', 'missing', 'invalid'] as const) expect(certTone(s)).toBe('bad');
    expect(certLabel('invalid')).toBe('Unreadable');
    expect(certLabel('ok')).toBe('Valid');
  });
  it('says the number that need attention', () => {
    expect(certsHeadline([])).toMatch(/No Ingress or Gateway/);
    expect(certsHeadline([cert('ok')])).toBe('1 certificate, all valid.');
    expect(certsHeadline([cert('ok'), cert('expired')])).toBe('2 certificates, 1 needs attention.');
    expect(certsHeadline([cert('missing'), cert('expiring')])).toBe('2 certificates, 2 need attention.');
  });
  it('formats the end date in UTC', () => {
    expect(endDate(1_800_000_000)).toBe('2027-01-15');
    expect(endDate(null)).toBe('—');
  });
});

describe('secrets', () => {
  const rows = [secret('db'), secret('api', { namespace: 'edge', keys: ['token', 'id'] }), secret('web-tls', { type: 'kubernetes.io/tls', keys: ['tls.crt', 'tls.key'] })];
  it('filters by namespace and by name, type or key', () => {
    expect(filterSecrets(rows, '', '').length).toBe(3);
    expect(filterSecrets(rows, 'edge', '').map((r) => r.name)).toEqual(['api']);
    expect(filterSecrets(rows, '', 'tls').map((r) => r.name)).toEqual(['web-tls']);
    expect(filterSecrets(rows, '', ' TOKEN ').map((r) => r.name)).toEqual(['api']);
    expect(filterSecrets(rows, 'shop', 'nothing')).toEqual([]);
  });
  it('lists namespaces once, sorted', () => {
    expect(namespacesOfSecrets(rows)).toEqual(['edge', 'shop']);
  });
});

describe('fluxctl commands', () => {
  it('never contains a value and quotes odd names', () => {
    expect(setSecretCommand('dev', 'shop', 'db', 'password')).toBe('fluxctl kube secrets set --cluster dev -n shop db password');
    expect(setSecretCommand("it's", 'shop', 'db', 'k')).toContain("'it'\\''s'");
  });
  it('builds the events and top commands', () => {
    expect(eventsCommand('dev', false, '')).toBe('fluxctl kube events --cluster dev');
    expect(eventsCommand('dev', true, 'shop')).toBe('fluxctl kube events --cluster dev -n shop --warnings');
    expect(topCommand('dev', 'nodes')).toBe('fluxctl kube top nodes --cluster dev');
  });
});
