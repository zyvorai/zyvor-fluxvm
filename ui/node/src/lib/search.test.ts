// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { ACTIONS, actionItems, appItems, catalogItems, emptyMessage, isSubsequence, loadProblem, loadRecent, machineItems, matchText, moveSel, ociItems, parseQuery, pushRecent, rank, saveRecent, scoreItem, shellWord, volumeItems } from './search';

const items = [
  ...machineItems([
    { id: 'aaaa1111', name: 'web-1', status: 'running', guest_ip: '10.0.0.5', labels: { env: 'dev', team: 'blue' } },
    { id: 'bbbb2222', name: 'db', status: 'stopped', labels: { env: 'prod' } },
  ]),
  ...appItems('dev', [{ namespace: 'default', name: 'web', image: 'nginx:1.27', port: 8080, summary: '2 of 2 running' }]),
  ...catalogItems([{ name: 'debian-13', format: 'qcow2', distro: 'debian', version: '13' }]),
  ...ociItems([{ reference: 'docker.io/library/alpine:3.22', digest: 'sha256:abcdef0123456789' }]),
  ...volumeItems([{ name: 'pg-data', size_gib: 50, attached_to: ['db'] }]),
];
const titles = (q: string, recent: string[] = []) => rank(items, q, recent).map((r) => r.item.title);

describe('matchText', () => {
  it('orders exact, prefix, word start, inside, loose, none', () => {
    expect(matchText('web', 'web')).toBe(100);
    expect(matchText('we', 'web-1')).toBe(80);
    expect(matchText('1', 'web-1')).toBe(60);
    expect(matchText('eb', 'web-1')).toBe(40);
    expect(matchText('wb1', 'web-1')).toBe(15);
    expect(matchText('zzz', 'web-1')).toBe(0);
  });
  it('is not case sensitive and ignores empty input', () => {
    expect(matchText('WEB', 'web')).toBe(100);
    expect(matchText('', 'web')).toBe(0);
  });
  it('does not use loose matching for one or two letters', () => {
    expect(matchText('wb', 'web-1')).toBe(0);
    expect(isSubsequence('abc', 'a-b-c')).toBe(true);
    expect(isSubsequence('acb', 'abc')).toBe(false);
  });
});

describe('rank', () => {
  it('puts an exact name before a prefix before a looser hit', () => {
    expect(titles('web').slice(0, 2)).toEqual(['web', 'web-1']);
  });
  it('finds a machine by label', () => {
    expect(titles('env=dev')).toEqual(['web-1']);
    expect(titles('team=blue')).toEqual(['web-1']);
  });
  it('finds an app by image reference', () => {
    expect(titles('nginx')).toEqual(['web']);
  });
  it('finds an app by port, with or without the colon', () => {
    expect(titles('8080')).toEqual(['web']);
    expect(titles(':8080')).toEqual(['web']);
    expect(titles('9999')).toEqual([]);
  });
  it('finds a volume by the machine using it, and a machine by address', () => {
    expect(titles('pg')[0]).toBe('pg-data');
    expect(titles('10.0.0.5')).toEqual(['web-1']);
  });
  it('requires every word to match', () => {
    expect(titles('web blue')).toEqual(['web-1']);
    expect(titles('web prod')).toEqual([]);
  });
  it('finds an OCI image by the last part of its reference', () => {
    expect(titles('alpine')).toEqual(['docker.io/library/alpine:3.22']);
  });
  it('lifts a recent pick over an equal match, but not over a far better one', () => {
    const two = [...machineItems([{ id: '1', name: 'api-a' }, { id: '2', name: 'api-b' }])];
    expect(rank(two, 'api', ['machine:2']).map((r) => r.item.title)).toEqual(['api-b', 'api-a']);
    expect(rank(two, 'api-a', ['machine:2']).map((r) => r.item.title)[0]).toBe('api-a');
  });
  it('is stable for equal scores and honours the limit', () => {
    const many = machineItems(Array.from({ length: 10 }, (_, i) => ({ id: String(i), name: `node-${i}` })));
    expect(rank(many, 'node', [], 3).map((r) => r.item.title)).toEqual(['node-0', 'node-1', 'node-2']);
  });
  it('shows recents first when nothing is typed', () => {
    const r = rank(items, '', ['volume:pg-data', 'machine:bbbb2222']);
    expect(r.slice(0, 2).map((x) => x.item.title)).toEqual(['pg-data', 'db']);
    expect(r[0].recent).toBe(true);
    expect(r.filter((x) => x.item.title === 'db')).toHaveLength(1);
  });
  it('ignores a recent id that no longer exists', () => {
    expect(rank(items, '', ['machine:gone']).every((r) => !r.recent)).toBe(true);
  });
});

describe('scoreItem', () => {
  it('is 0 for no match and positive otherwise', () => {
    expect(scoreItem(items[0], 'nothing-like-it')).toBe(0);
    expect(scoreItem(items[0], 'web')).toBeGreaterThan(0);
  });
});

describe('parseQuery', () => {
  it('reads a leading > as actions', () => {
    expect(parseQuery('> create')).toEqual({ mode: 'actions', text: 'create' });
    expect(parseQuery('  >')).toEqual({ mode: 'actions', text: '' });
    expect(parseQuery(' web ')).toEqual({ mode: 'search', text: 'web' });
    expect(parseQuery('a>b')).toEqual({ mode: 'search', text: 'a>b' });
  });
});

describe('actions', () => {
  it('each one names its fluxctl command, or says it is Studio only', () => {
    for (const a of ACTIONS) expect(a.studioOnly ? a.cmd === '' : a.cmd.length > 0).toBe(true);
    const a = actionItems();
    expect(rank(a, 'volume', []).map((r) => r.item.title)).toEqual(['Create a volume']);
    expect(a.find((x) => x.title === 'Sign out')?.sub).toBe('Studio only');
  });
  it('finds an action by the command you already know', () => {
    expect(rank(actionItems(), 'oci pull', []).map((r) => r.item.title)).toEqual(['Pull an image']);
  });
});

describe('recent picks', () => {
  it('keeps newest first, without repeats, capped', () => {
    expect(pushRecent(['a', 'b', 'c'], 'b')).toEqual(['b', 'a', 'c']);
    expect(pushRecent(['a', 'b'], 'c', 2)).toEqual(['c', 'a']);
  });
  it('survives a broken or missing store', () => {
    expect(loadRecent({ getItem: () => 'not json' })).toEqual([]);
    expect(loadRecent({ getItem: () => '{"a":1}' })).toEqual([]);
    expect(loadRecent({ getItem: () => null })).toEqual([]);
    expect(loadRecent(undefined as never)).toEqual([]);
    expect(() => saveRecent(['a'], { setItem: () => { throw new Error('full'); } })).not.toThrow();
  });
  it('round-trips', () => {
    let saved = '';
    saveRecent(['x', 'y'], { setItem: (_k, v) => { saved = v; } });
    expect(loadRecent({ getItem: () => saved })).toEqual(['x', 'y']);
  });
});

describe('moveSel', () => {
  it('stops at both ends and copes with an empty list', () => {
    expect(moveSel(0, -1, 5)).toBe(0);
    expect(moveSel(4, 1, 5)).toBe(4);
    expect(moveSel(2, 1, 5)).toBe(3);
    expect(moveSel(0, 1, 0)).toBe(0);
  });
});

describe('words for empty and error states', () => {
  it('says what to try', () => {
    expect(emptyMessage('zzz', 'search', [])).toContain('Nothing matches “zzz”');
    expect(emptyMessage('zzz', 'actions', [])).toContain('Clear the >');
    expect(emptyMessage('', 'search', [])).toContain('Create a machine');
    expect(emptyMessage('x', 'search', ['Volumes'])).toContain('Volumes could not be read');
  });
  it('turns a failure into a next step', () => {
    expect(loadProblem('Volumes', new Error('GET /v1/volumes: 403 Forbidden'))).toContain('Sign in again');
    expect(loadProblem('Apps', new Error('404 not found'))).toContain('older release');
    expect(loadProblem('Images', new TypeError('Failed to fetch'))).toContain('Check that FluxVM is running');
    expect(loadProblem('Images', 'odd')).toBe('Images: odd');
  });
});

describe('items and commands', () => {
  it('builds routes that open the thing', () => {
    expect(items.find((i) => i.title === 'web-1')?.to).toBe('/machines/aaaa1111');
    expect(items.find((i) => i.kind === 'app')?.to).toBe('/apps?cluster=dev&focus=web');
    expect(items.find((i) => i.title === 'pg-data')?.to).toBe('/volumes?focus=pg-data');
  });
  it('gives the command that shows the same thing', () => {
    expect(items.find((i) => i.title === 'web-1')?.cmd).toBe('fluxctl get web-1');
    expect(items.find((i) => i.kind === 'app')?.cmd).toBe('fluxctl kube apps --cluster dev');
  });
  it('quotes a name only when a shell needs it', () => {
    expect(shellWord('web-1')).toBe('web-1');
    expect(shellWord("my vm's")).toBe("'my vm'\\''s'");
  });
});
