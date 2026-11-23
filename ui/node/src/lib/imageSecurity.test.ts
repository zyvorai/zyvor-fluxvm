// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type Assessment, advisoryLine, badgeTone, badgeWord, countsLine, nextCommand, sbomCommand, sbomPath, signatureWord, sortAssessments } from './imageSecurity';

const a = (badge: Assessment['badge'], reference = 'ghcr.io/acme/app:1'): Assessment => ({ reference, manifest_digest: 'sha256:abc', signature: 'unchecked', badge, summary: '' });

describe('image security helpers', () => {
  it('never calls missing advisory data clean', () => {
    expect(advisoryLine(undefined)).toMatch(/nothing has been checked/);
    expect(advisoryLine({ present: false, advisories: 0, source: '' })).toMatch(/no advisory data/);
    expect(advisoryLine({ present: true, age_days: 12, advisories: 3, source: 's' })).toBe('Advisory data is 12 days old (3 advisories).');
    expect(advisoryLine({ present: true, age_days: 0, advisories: 1, source: 's' })).toMatch(/from today/);
    expect(advisoryLine({ present: true, age_days: 1, advisories: 1, source: 's' })).toMatch(/1 day old/);
  });
  it('maps badges to plain words and tones', () => {
    expect(badgeWord('blocked')).toBe('Blocked by policy');
    expect(badgeTone('ok')).toBe('ok');
    expect(badgeTone('risk')).toBe('bad');
    expect(badgeWord('nonsense')).toBe('Unknown');
  });
  it('describes signatures', () => {
    expect(signatureWord('trusted')).toMatch(/trusted/);
    expect(signatureWord('unchecked')).toBe('Not checked yet');
  });
  it('orders counts by severity', () => {
    expect(countsLine({ high: 2, critical: 1 })).toBe('1 critical, 2 high');
    expect(countsLine({})).toBe('none');
  });
  it('offers the command that matches the badge, quoting odd references', () => {
    expect(nextCommand(a('unsigned'))).toBe('fluxctl image verify ghcr.io/acme/app:1');
    expect(nextCommand(a('blocked'))).toBe('fluxctl image policy check ghcr.io/acme/app:1');
    expect(nextCommand(a('unscanned'))).toMatch(/--update/);
    expect(sbomCommand(a('ok', 'x y:1'), 'cyclonedx')).toBe("fluxctl image sbom 'x y:1' --sbom-format cyclonedx");
  });
  it('builds the sbom route from a digest', () => {
    expect(sbomPath('sha256:abc')).toBe('/v1/oci/images/sha256%3Aabc/sbom?format=spdx');
  });
  it('puts problems first', () => {
    expect(sortAssessments([a('ok', 'a'), a('blocked', 'z'), a('unsigned', 'b')]).map((x) => x.badge)).toEqual(['blocked', 'unsigned', 'ok']);
  });
});
