// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { upgradeDetails, upgradeTone, upgradeWord, type UpgradeStatus } from './upgrade';

const st = (o: Partial<UpgradeStatus>): UpgradeStatus => ({
  schema: 1, state: 'up_to_date', current: '0.4.21', latest: null, tag: null, update_available: false, source: 's', looked_for: 'a vX.Y.Z release', newest: [], line: 'l', maintainer_next: null, user_next: 'Nothing to do.', ...o,
});

describe('upgrade status helpers', () => {
  it('only an available update is coloured', () => {
    expect(upgradeTone(st({ state: 'update_available' }))).toBe('warn');
    expect(upgradeTone(st({ state: 'no_installable_release' }))).toBe('ok');
    expect(upgradeWord(st({ state: 'unreachable' }))).toBe('Not checked');
  });
  it('says what was looked for, what exists and the next step for both people', () => {
    const d = upgradeDetails(st({ state: 'no_installable_release', newest: [{ tag: 'mac-v0.1.0', installable: false, note: 'no .tar.gz package for macos-arm64' }], maintainer_next: 'Publish a vX.Y.Z release.', user_next: 'Re-run the installer.' }));
    expect(d[0]).toContain('Looked for');
    expect(d[1]).toBe('Newest releases: mac-v0.1.0 (no .tar.gz package for macos-arm64).');
    expect(d[2]).toBe('For the maintainer: Publish a vX.Y.Z release.');
    expect(d[3]).toBe('For you: Re-run the installer.');
  });
  it('an up to date install has nothing to explain', () => {
    expect(upgradeDetails(st({}))).toEqual([]);
  });
});
