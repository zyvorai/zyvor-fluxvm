// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { EMPTY_FILTERS, actionWords, auditPath, emptyWords, lsCommand, resultTone, resultWord, toJsonl, type AuditRecord } from './audit';

const now = new Date('2026-11-23T12:00:00Z');
const rec = (o: Partial<AuditRecord> = {}): AuditRecord => ({ ts: '2026-11-23T11:00:00Z', action: 'mac.run', actor: 'sam', target: 'web', result: 'ok', source: 'cli', ...o });

describe('audit query', () => {
  it('sends only the filters that are set', () => {
    expect(auditPath(EMPTY_FILTERS, {}, now)).toBe('/v1/audit');
    const p = auditPath({ since: '24h', action: 'mac.', target: 'my vm', actor: '', result: 'failed' }, { limit: 50, offset: 100 }, now);
    expect(p).toContain('since=2026-11-22T12%3A00%3A00.000Z');
    expect(p).toContain('action=mac.');
    expect(p).toContain('target=my+vm');
    expect(p).toContain('result=failed');
    expect(p).toContain('limit=50');
    expect(p).toContain('offset=100');
    expect(p).not.toContain('actor');
  });
  it('shows the same filters as a terminal command, quoted safely', () => {
    expect(lsCommand(EMPTY_FILTERS)).toBe('fluxctl audit ls');
    expect(lsCommand({ ...EMPTY_FILTERS, since: '7d', target: "it's here", result: 'denied' })).toBe(`fluxctl audit ls --since 7d --target 'it'\\''s here' --result denied`);
  });
});

describe('audit words', () => {
  it('words results and tones', () => {
    expect(resultWord('denied')).toBe('Refused');
    expect(resultTone('ok')).toBe('ok');
    expect(resultTone('denied')).toBe('warn');
    expect(resultTone('failed')).toBe('bad');
  });
  it('turns an action into a sentence and falls back to the raw name', () => {
    expect(actionWords('mac.run', 'web')).toBe('started web (Mac)');
    expect(actionWords('mac.exec', 'web')).toBe('ran a command in web (Mac)');
    expect(actionWords('vm.delete', 'abc')).toBe('deleted abc');
    expect(actionWords('k3s.backups-restore', 'default')).toBe('k3s.backups-restore default');
  });
  it('exports oldest first, one per line', () => {
    const out = toJsonl([rec({ target: 'new' }), rec({ target: 'old' })]);
    const lines = out.trim().split('\n').map((l) => JSON.parse(l) as AuditRecord);
    expect(lines.map((l) => l.target)).toEqual(['old', 'new']);
    expect(toJsonl([])).toBe('');
  });
  it('explains an empty list', () => {
    expect(emptyWords(EMPTY_FILTERS, 0)).toMatch(/No changes recorded yet/);
    expect(emptyWords({ ...EMPTY_FILTERS, result: 'failed' }, 0)).toMatch(/Clear them/);
  });
});
