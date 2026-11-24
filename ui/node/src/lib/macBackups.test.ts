// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { ageWords, bytesText, canBackUp, createCommand, everyWords, roomWords, scheduleCommand, stateTone, stateWord, explainBackupError } from './macBackups';

describe('backup commands', () => {
  it('match the CLI', () => {
    expect(createCommand('instance', 'ci')).toBe('fluxctl mac backup create ci');
    expect(createCommand('volume', 'cache', 3)).toBe('fluxctl mac backup create --volume cache --keep 3');
    expect(createCommand('instance', 'ci', 7)).toBe('fluxctl mac backup create ci');
    expect(scheduleCommand('instance', 'ci', '24h')).toBe('fluxctl mac backup schedule ci --every 24h');
    expect(scheduleCommand('volume', 'v', '6h', 2)).toBe('fluxctl mac backup schedule --volume v --every 6h --keep 2');
  });
});

describe('backup words', () => {
  it('words sizes, ages and intervals', () => {
    expect(bytesText(1536 * 1024 ** 2)).toBe('1.5 GiB');
    expect(bytesText(null)).toBe('—');
    expect(ageWords(10)).toBe('just now');
    expect(ageWords(7200)).toBe('2h ago');
    expect(ageWords(3 * 86400)).toBe('3d ago');
    expect(everyWords(86400)).toBe('1d');
    expect(everyWords(5400)).toBe('90m');
  });
  it('tones the state', () => {
    expect(stateTone('ok')).toBe('ok');
    expect(stateTone('running')).toBe('warn');
    expect(stateTone('damaged')).toBe('bad');
    expect(stateWord('running')).toBe('being written');
  });
  it('says how much room a backup has, keeping the reserve', () => {
    const gib = 1024 ** 3;
    expect(roomWords({ free_bytes: 1 * gib, reserve_bytes: 2 * gib })).toMatchObject({ tone: 'bad' });
    expect(roomWords({ free_bytes: 10 * gib, reserve_bytes: 2 * gib })).toMatchObject({ tone: 'warn', text: '10.0 GiB free, 8.0 GiB usable for backups' });
    expect(roomWords({ free_bytes: 200 * gib, reserve_bytes: 2 * gib }).tone).toBe('ok');
    expect(roomWords({ free_bytes: null, reserve_bytes: 2 * gib }).tone).toBe('idle');
  });
  it('only a stopped instance can be backed up', () => {
    expect(canBackUp('stopped')).toBe(true);
    expect(canBackUp('running')).toBe(false);
  });
  it('words an admin refusal', () => {
    expect(explainBackupError('ci', '403 admin role required')).toContain('administrator');
    expect(explainBackupError('ci', 'boom')).toBe('ci: boom');
  });
});
