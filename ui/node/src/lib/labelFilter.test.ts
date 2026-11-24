// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { filterFlags, labelsMatch, parseQuery } from './labelFilter';

describe('label search', () => {
  it('splits label terms from words', () => {
    expect(parseQuery('Web team=ios label:tier label=env=dev')).toEqual({
      words: ['web'],
      labels: [{ key: 'team', value: 'ios' }, { key: 'tier', value: null }, { key: 'env', value: 'dev' }],
    });
    expect(parseQuery('   ')).toEqual({ words: [], labels: [] });
    expect(parseQuery('=x').words).toEqual(['=x']);
  });
  it('needs every label term to match', () => {
    const l = { team: 'ios', tier: 'ci' };
    expect(labelsMatch(l, parseQuery('team=ios').labels)).toBe(true);
    expect(labelsMatch(l, parseQuery('label:tier').labels)).toBe(true);
    expect(labelsMatch(l, parseQuery('team=android').labels)).toBe(false);
    expect(labelsMatch(l, parseQuery('team=ios gpu').labels)).toBe(true);
    expect(labelsMatch(l, parseQuery('label:gpu').labels)).toBe(false);
    expect(labelsMatch(undefined, parseQuery('team').labels)).toBe(true);
    expect(labelsMatch(undefined, parseQuery('label:team').labels)).toBe(false);
  });
  it('shows the fluxctl flags the box means', () => {
    expect(filterFlags('web team=ios label:tier')).toBe('--filter label=team=ios --filter label=tier');
    expect(filterFlags('web')).toBe('');
  });
});
