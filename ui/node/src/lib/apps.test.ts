// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { type App, appCommands, appTone, appsPath, appsSummary, clampCopies, deleteAppPath, emptyRunForm, runBody, runCommand, runProblem, scalePath } from './apps';

const app = (over: Partial<App> = {}): App => ({ namespace: 'default', name: 'web', image: 'nginx', desired: 1, ready: 1, port: 80, state: 'running', summary: '1 of 1 running', code: null, next: null, pods: [], created: null, ...over });

describe('apps', () => {
  it('colours by state', () => {
    expect(appTone('running')).toBe('ok');
    expect(appTone('starting')).toBe('warn');
    expect(appTone('problem')).toBe('bad');
    expect(appTone('stopped')).toBe('idle');
  });

  it('escapes every part of a path', () => {
    expect(appsPath('a b')).toBe('/v1/kube/a%20b/apps');
    expect(deleteAppPath('dev', app({ namespace: 'shop' }))).toBe('/v1/kube/dev/apps/shop/web/delete');
    expect(scalePath('dev', app())).toBe('/v1/kube/dev/workloads/default/deployments/web/scale');
  });

  it('clamps copies', () => {
    expect(clampCopies(-3)).toBe(0);
    expect(clampCopies(500)).toBe(100);
    expect(clampCopies(NaN)).toBe(0);
  });

  it('writes the fluxctl command for each action', () => {
    const c = appCommands('dev', app());
    expect(c.logs).toBe('fluxctl kube logs web -f --name dev');
    expect(c.shell).toBe('fluxctl kube shell web --cluster dev');
    expect(c.scale(3)).toBe('fluxctl kube scale web 3 --cluster dev');
    expect(c.rm).toBe('fluxctl kube rm web --cluster dev');
    expect(appCommands('dev', app({ namespace: 'shop' })).expose).toBe('fluxctl kube expose web -n shop --cluster dev');
  });

  it('checks the run form with a sentence and builds the body', () => {
    expect(runProblem(emptyRunForm)).toMatch(/image/);
    expect(runProblem({ ...emptyRunForm, image: 'nginx' })).toBeNull();
    expect(runProblem({ ...emptyRunForm, image: 'nginx', name: 'Bad Name' })).toMatch(/lowercase/);
    expect(runProblem({ ...emptyRunForm, image: 'nginx', port: '70000' })).toMatch(/port/);
    expect(runProblem({ ...emptyRunForm, image: 'nginx', replicas: '101' })).toMatch(/Copies/);
    expect(runBody({ ...emptyRunForm, image: ' nginx ' })).toEqual({ image: 'nginx' });
    expect(runBody({ image: 'nginx', name: 'web', port: '8080', replicas: '2', namespace: 'shop' })).toEqual({ image: 'nginx', name: 'web', port: 8080, replicas: 2, namespace: 'shop' });
    expect(runCommand('dev', { ...emptyRunForm, image: 'nginx', port: '8080' })).toBe('fluxctl kube run nginx --port 8080 --cluster dev');
  });

  it('summarises the list', () => {
    expect(appsSummary([])).toBe('No apps yet');
    expect(appsSummary([app(), app({ state: 'problem' })])).toBe('2 apps, 1 with a problem');
    expect(appsSummary([app()])).toBe('1 app');
  });
});
