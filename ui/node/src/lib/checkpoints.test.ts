// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { cliRestore, cliSave, defaultTag, isAuto, kindLabel, restoreText, restoredMessage, saveBody, snapKind, tagProblem } from './checkpoints';

describe('checkpoints', () => {
  it('tells memory checkpoints from disk-only ones and marks automatic ones', () => {
    expect(snapKind({ tag: 'a', has_memory: true })).toBe('memory');
    expect(snapKind({ tag: 'a', has_memory: false })).toBe('disk');
    expect(snapKind({ tag: 'a', memory: true })).toBe('memory');
    expect(snapKind({ tag: 'a' })).toBe('unknown');
    expect(kindLabel({ tag: 'a', has_memory: false })).toBe('disk only');
    expect(kindLabel({ tag: 'a' })).toBe('');
    expect(isAuto('auto-before-restore-20261123-101500')).toBe(true);
    expect(isAuto('my-auto')).toBe(false);
  });

  it('names a checkpoint like the CLI and checks the name like the daemon', () => {
    expect(defaultTag(new Date(Date.UTC(2026, 10, 3, 4, 5, 6)))).toBe('ckpt-20261103-040506');
    expect(tagProblem('before-upgrade_1.2')).toBeNull();
    expect(tagProblem('  ')).toMatch(/name/);
    for (const bad of ['a b', 'a/b', '.hidden', 'x'.repeat(129)]) expect(tagProblem(bad)).toMatch(/letters/);
  });

  it('saves memory only for a running machine and only when asked', () => {
    expect(saveBody(' w ', true, false)).toEqual({ tag: 'w', memory: true });
    expect(saveBody('w', true, true)).toEqual({ tag: 'w', memory: false });
    expect(saveBody('w', false, false)).toEqual({ tag: 'w', memory: false });
  });

  it('explains a restore and how to roll it back', () => {
    expect(restoreText({ tag: 'w', has_memory: true }, true)).toMatch(/resumes right where it was.*undo/s);
    expect(restoreText({ tag: 'w', has_memory: false }, false)).toMatch(/boot takes minutes.*undo/s);
    expect(restoredMessage({ safety_checkpoint: 'auto-before-restore-1' }, 'w')).toBe('Restored w. Your previous state is saved as auto-before-restore-1: restore that one to go back.');
    expect(restoredMessage({}, 'w')).toBe('Restored w.');
    expect(restoredMessage(null, 'w')).toBe('Restored w.');
  });

  it('shows the matching fluxctl commands', () => {
    expect(cliSave('ci', 'warm', false)).toBe('fluxctl mac checkpoint ci warm');
    expect(cliSave('ci', 'warm', true)).toBe('fluxctl mac checkpoint ci warm --disk-only');
    expect(cliRestore('ci', 'warm')).toBe('fluxctl mac checkpoint restore ci warm');
  });
});
