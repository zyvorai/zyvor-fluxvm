// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { explainCommand, selfTestTone, selfTestWord, todo, type SelfTestCheck, type SelfTestReport } from './selfTest';

const c = (id: string, status: SelfTestCheck['status'], what: string | null = null, explain: string | null = null): SelfTestCheck => ({ id, name: id, status, detail: '', what_to_do: what, explain });
const report = (checks: SelfTestCheck[]): SelfTestReport => ({ schema: 1, ts: 1, status: 'fail', passed: 1, warned: 1, failed: 1, skipped: 0, headline: 'x', checks });

describe('self-test helpers', () => {
  it('uses the same words as the CLI table and a tone per status', () => {
    expect(selfTestWord('pass')).toBe('PASS');
    expect(selfTestTone('pass')).toBe('ok');
    expect(selfTestTone('warn')).toBe('warn');
    expect(selfTestTone('fail')).toBe('bad');
    expect(selfTestTone('skip')).toBe('idle');
  });
  it('lists only what needs doing, failures first', () => {
    const r = report([c('a', 'pass'), c('b', 'warn', 'do b'), c('c', 'fail', 'do c'), c('d', 'skip')]);
    expect(todo(r).map((x) => x.id)).toEqual(['c', 'b']);
  });
  it('builds the explain command only when there is a code', () => {
    expect(explainCommand(c('a', 'fail', 'w', 'DOCKER_SOCKET_DOWN'))).toBe('fluxctl explain DOCKER_SOCKET_DOWN');
    expect(explainCommand(c('a', 'fail', 'w'))).toBeNull();
  });
});
