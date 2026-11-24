// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { EMPTY_FILTERS, SOURCES, emptyWords, errorWords, eventsCommand, eventsPath, feedNotes, levelTone, mergeNew, newestTs, sourceLabel, wantsK8s, type FeedEvent } from './events';

const ev = (o: Partial<FeedEvent> = {}): FeedEvent => ({ ts: '2026-11-24T10:00:00Z', source: 'vm', event: 'vm.start', summary: 'VM web started', level: 'info', ...o });

describe('events query', () => {
  it('always names the source and sends only the filters that are set', () => {
    expect(eventsPath(EMPTY_FILTERS)).toBe('/v1/events?source=all&limit=200');
    const p = eventsPath({ sources: ['mac', 'kube'], since: '1h', level: 'warn', event: 'mac.run' }, 50);
    expect(p).toBe('/v1/events?source=mac%2Ckube&since=1h&level=warn&event=mac.run&limit=50');
  });
  it('lets the follower ask only for what is newer', () => {
    expect(eventsPath({ ...EMPTY_FILTERS, since: '1h' }, 100, '2026-11-24T10:00:00Z')).toContain('since=2026-11-24T10%3A00%3A00Z');
  });
  it('shows the same filters as a terminal command, quoted safely', () => {
    expect(eventsCommand(EMPTY_FILTERS)).toBe('fluxctl events');
    expect(eventsCommand({ sources: ['mac'], since: '7d', level: 'error', event: "it's" }, true)).toBe(`fluxctl events --source mac --since 7d --level error --event 'it'\\''s' --follow`);
  });
});

describe('cluster events', () => {
  it('has a k8s source toggle and labels it', () => {
    expect(SOURCES.map((s) => s.value)).toContain('k8s');
    expect(sourceLabel('k8s')).toBe('Cluster events');
  });
  it('sends the cluster only when the k8s source is in play', () => {
    expect(eventsPath({ ...EMPTY_FILTERS, sources: ['k8s'], cluster: 'dev' })).toBe('/v1/events?source=k8s&cluster=dev&limit=200');
    expect(eventsPath({ ...EMPTY_FILTERS, cluster: 'dev' })).toContain('cluster=dev');
    expect(eventsPath({ ...EMPTY_FILTERS, sources: ['vm'], cluster: 'dev' })).not.toContain('cluster');
    expect(wantsK8s({ ...EMPTY_FILTERS, sources: ['vm'] })).toBe(false);
    expect(eventsCommand({ ...EMPTY_FILTERS, sources: ['k8s'], cluster: 'dev', level: 'warn' })).toBe('fluxctl events --source k8s --level warn --cluster dev');
    expect(eventsCommand({ ...EMPTY_FILTERS, sources: ['vm'], cluster: 'dev' })).toBe('fluxctl events --source vm');
  });
  it('shows the notes of clusters that were left out, and none when there are none', () => {
    expect(feedNotes({})).toEqual([]);
    expect(feedNotes({ notes: ['Kubernetes events of `lab` are left out. more: fluxctl explain K8S_EVENTS_UNREACHABLE', ' '] })).toHaveLength(1);
    expect(emptyWords({ ...EMPTY_FILTERS, cluster: 'dev' })).toMatch(/Widen/);
  });
});

describe('events words', () => {
  it('tones and labels', () => {
    expect(levelTone('error')).toBe('bad');
    expect(levelTone('warn')).toBe('warn');
    expect(levelTone('info')).toBe('ok');
    expect(sourceLabel('docker')).toBe('Containers');
    expect(sourceLabel('other')).toBe('other');
  });
  it('explains an empty list and a refusal with the next step', () => {
    expect(emptyWords(EMPTY_FILTERS)).toMatch(/Live/);
    expect(emptyWords({ ...EMPTY_FILTERS, level: 'error' })).toMatch(/Widen/);
    expect(errorWords('403 forbidden: admin token needed').title).toMatch(/admin token/);
    expect(errorWords('`x` is not an event source. Pick from: vm').body).toMatch(/explain EVENTS_BAD_SOURCE/);
    expect(errorWords('network down').body).toMatch(/daemon is running/);
  });
});

describe('following', () => {
  it('puts new lines on top, newest first, without repeats', () => {
    const a = ev({ ts: '2026-11-24T10:00:00Z', summary: 'a' });
    const b = ev({ ts: '2026-11-24T10:00:05Z', summary: 'b' });
    const shown = mergeNew([a], [a, b]);
    expect(shown.map((e) => e.summary)).toEqual(['b', 'a']);
    expect(mergeNew(shown, [b])).toBe(shown);
  });
  it('keeps at most the cap and finds the newest time', () => {
    const many = Array.from({ length: 10 }, (_, i) => ev({ ts: `2026-11-24T10:00:0${i}Z`, summary: `e${i}` }));
    expect(mergeNew([], many, 3).map((e) => e.summary)).toEqual(['e9', 'e8', 'e7']);
    expect(newestTs(many)).toBe('2026-11-24T10:00:09Z');
    expect(newestTs([])).toBeUndefined();
  });
});
