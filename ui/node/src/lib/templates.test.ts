// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { filterCheatsheet, filterTemplates, kindWord, needsWord, runPath, type CheatGroup, type Template } from './templates';

const t = (name: string, kind: Template['kind'], summary = ''): Template => ({ name, kind, title: name.toUpperCase(), summary, after: '', run: `fluxctl templates run ${name}`, show: '' });

describe('template helpers', () => {
  const all = [t('redis', 'compose', 'Redis 7'), t('whoami', 'k3s', 'Two pods')];
  it('filters by name, title, summary and kind', () => {
    expect(filterTemplates(all, '')).toHaveLength(2);
    expect(filterTemplates(all, 'REDIS').map((x) => x.name)).toEqual(['redis']);
    expect(filterTemplates(all, 'pods').map((x) => x.name)).toEqual(['whoami']);
    expect(filterTemplates(all, 'k3s').map((x) => x.name)).toEqual(['whoami']);
    expect(filterTemplates(all, 'zzz')).toEqual([]);
  });
  it('says what each kind needs', () => {
    expect(needsWord('compose')).toMatch(/Docker/);
    expect(needsWord('k3s')).toMatch(/kube up/);
    expect(kindWord('compose')).toBe('Compose');
  });
  it('builds the run path from a safe name', () => {
    expect(runPath('redis')).toBe('/v1/stack-templates/redis/run');
    expect(runPath('a/b')).toBe('/v1/stack-templates/a%2Fb/run');
  });
});

describe('cheatsheet search', () => {
  const groups: CheatGroup[] = [
    { group: 'A', commands: [{ command: 'fluxctl doctor', does: 'check this host' }, { command: 'fluxctl status', does: 'what is running' }] },
    { group: 'B', commands: [{ command: 'fluxctl upgrade', does: 'new release' }] },
  ];
  it('keeps matching commands and drops empty groups', () => {
    expect(filterCheatsheet(groups, '')).toHaveLength(2);
    const r = filterCheatsheet(groups, 'host');
    expect(r).toHaveLength(1);
    expect(r[0].commands.map((c) => c.command)).toEqual(['fluxctl doctor']);
    expect(filterCheatsheet(groups, 'nothing')).toEqual([]);
  });
});
