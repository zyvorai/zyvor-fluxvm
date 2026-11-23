// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { ageWord, backupBase, freshness, kindWord, restoreCommand, restoreConfirmed, scheduleWord, sizeWord, type ClusterBackups } from './backups';

const cluster = (o: Partial<ClusterBackups> = {}): ClusterBackups => ({
  name: 'default', schedule: { enabled: true, every_hours: 24, keep: 7, keep_pre_upgrade: 3 }, backups: [], total_bytes: 0,
  newest_age_secs: 3600, disk_free_bytes: null, commands: { backup: 'fluxctl kube backup', list: '', verify: '', restore_latest: '' }, ...o,
});

describe('backup helpers', () => {
  it('words sizes, ages and kinds', () => {
    expect(sizeWord(100)).toBe('1 KiB');
    expect(sizeWord(5 * 1048576)).toBe('5.0 MiB');
    expect(ageWord(30)).toBe('just now');
    expect(ageWord(3600)).toBe('60 min ago');
    expect(ageWord(7200)).toBe('2 h ago');
    expect(ageWord(3 * 86400)).toBe('3 days ago');
    expect(kindWord('pre_upgrade')).toBe('Before upgrade');
  });
  it('judges freshness against the schedule', () => {
    expect(freshness(cluster()).tone).toBe('ok');
    expect(freshness(cluster({ newest_age_secs: null })).text).toBe('No backup yet');
    expect(freshness(cluster({ newest_age_secs: 3 * 86400 })).tone).toBe('bad');
    expect(freshness(cluster({ schedule: null, newest_age_secs: 3 * 86400 })).tone).toBe('ok');
  });
  it('explains the schedule', () => {
    expect(scheduleWord(null)).toMatch(/not set up/);
    expect(scheduleWord({ enabled: false, every_hours: 24, keep: 7, keep_pre_upgrade: 3 })).toMatch(/off/);
    expect(scheduleWord({ enabled: true, every_hours: 24, keep: 7, keep_pre_upgrade: 3 })).toContain('every day');
    expect(scheduleWord({ enabled: true, every_hours: 6, keep: 7, keep_pre_upgrade: 3 })).toContain('every 6 hours');
  });
  it('needs the name typed again before a restore', () => {
    expect(restoreConfirmed('', 'a.tar.gz')).toBe(false);
    expect(restoreConfirmed('a.tar.gz ', 'a.tar.gz')).toBe(true);
    expect(restoreConfirmed('b.tar.gz', 'a.tar.gz')).toBe(false);
  });
  it('builds the copyable restore command and the API base', () => {
    expect(restoreCommand('default', 'x.tar.gz')).toBe('fluxctl kube restore x.tar.gz --yes');
    expect(restoreCommand('lab', 'x.tar.gz')).toBe('fluxctl kube restore x.tar.gz --k3s-cluster lab --yes');
    expect(backupBase('a b')).toBe('/v1/k3s/a%20b/backups');
  });
});
