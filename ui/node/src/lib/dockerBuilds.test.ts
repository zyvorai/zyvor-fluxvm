// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type DockerBuild, type DockerBuildsView, builderNote, buildLabel, buildTone, durationText, errorLine, shortImage, sizeText, summarizeBuilds, tagsText } from './dockerBuilds';

const b = (state: string, extra: Partial<DockerBuild> = {}): DockerBuild => ({
  id: 'x', started_unix: 1, finished_unix: 2, duration_ms: 1000, state, tags: ['app:1'], dockerfile: 'Macfile', build_arg_names: [], target: null, platform: null,
  no_cache: false, context_bytes: 10, image_id: null, error: null, builder: 'fluxctl mac build', fluxctl: 'fluxctl mac build -t app:1 .', ...extra,
});

describe('build state', () => {
  it('maps states to tones and words', () => {
    expect(buildTone('succeeded')).toBe('ok');
    expect(buildTone('failed')).toBe('bad');
    expect(buildTone('running')).toBe('info');
    expect(buildTone('cancelled')).toBe('idle');
    expect(buildLabel('succeeded')).toBe('built');
    expect(buildLabel('cancelled')).toBe('stopped');
  });
  it('counts built, failed and running apart', () => {
    expect(summarizeBuilds([b('succeeded'), b('failed'), b('failed'), b('running'), b('cancelled')])).toEqual({ built: 1, failed: 2, running: 1 });
  });
});

describe('text', () => {
  it('writes durations, ids, names and sizes', () => {
    expect(durationText(null)).toBe('—');
    expect(durationText(42000)).toBe('42s');
    expect(durationText(75000)).toBe('1m 15s');
    expect(durationText(120000)).toBe('2m 00s');
    expect(shortImage('sha256:0123456789abcdef')).toBe('0123456789ab');
    expect(shortImage(null)).toBe('—');
    expect(tagsText(b('failed', { tags: [] }))).toBe('(untagged)');
    expect(tagsText(b('failed', { tags: ['a:1', 'a:latest'] }))).toBe('a:1, a:latest');
    expect(sizeText(2048)).toBe('2 KiB');
    expect(sizeText(5 * (1 << 20))).toBe('5.0 MiB');
  });
  it('shows the last line of a failure, the one that says what to do', () => {
    expect(errorLine(b('failed', { error: 'step 2\nthe disk is full. Free space, then build again.' }))).toBe('the disk is full. Free space, then build again.');
    expect(errorLine(b('failed'))).toBe('');
  });
});

describe('builder note', () => {
  const v = (p: Partial<DockerBuildsView>): DockerBuildsView => ({ enabled: true, available: true, reason: null, builds: [], try: 'docker build -t NAME:TAG .', fluxctl: 'fluxctl mac build -t NAME:TAG .', ...p });
  it('says where builds go, or why they cannot', () => {
    expect(builderNote(v({ builder: 'fluxctl mac build' }))).toContain('forwarded to fluxctl mac build');
    expect(builderNote(v({ available: false, reason: 'this host is not a Mac' }))).toBe('this host is not a Mac');
    expect(builderNote(v({ enabled: false, available: false, reason: 'The socket is off' }))).toBe('The socket is off');
  });
});
