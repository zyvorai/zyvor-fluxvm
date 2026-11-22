// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { BUNDLE_COMMAND, BUNDLE_NOTE, bundleFailure, bundleFileName, bundleSummary } from './bundle';

describe('support bundle helpers', () => {
  it('takes the file name from Content-Disposition', () => {
    expect(bundleFileName('attachment; filename="fluxvm-support-20261122-101500.tar.gz"')).toBe('fluxvm-support-20261122-101500.tar.gz');
  });
  it('makes a timestamped name when there is no header, and never trusts odd characters', () => {
    expect(bundleFileName(null, new Date(Date.UTC(2026, 10, 22, 9, 5, 7)))).toBe('fluxvm-support-20261122-090507.tar.gz');
    expect(bundleFileName('attachment; filename="../../etc/passwd"', new Date(Date.UTC(2026, 0, 2, 3, 4, 5)))).toMatch(/^[A-Za-z0-9._-]+$/);
  });
  it('summarises the download with the review note and what was skipped', () => {
    expect(bundleSummary('b.tar.gz', 2048, 0)).toBe(`Saved b.tar.gz (2 KiB). ${BUNDLE_NOTE}`);
    expect(bundleSummary('b.tar.gz', 3 * 1048576, 2)).toContain('2 parts were skipped; MANIFEST.txt inside says which and why.');
    expect(bundleSummary('b.tar.gz', 100, 1)).toContain('1 part was skipped');
  });
  it('says what to do when the download fails', () => {
    expect(bundleFailure(500, 'no bundle was written: ...')).toBe('no bundle was written: ...');
    expect(bundleFailure(403, null)).toContain('admin token');
    expect(bundleFailure(502, null)).toContain(BUNDLE_COMMAND);
  });
});
