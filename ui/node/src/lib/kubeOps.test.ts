// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { describe, expect, it } from 'vitest';
import { confirmed, explainCommand, failureText, kindWord, nextStep, nodeAddPath, nodeRemovePath, opPath, opTone, opWord, upgradePath, upgradePlanPath, type KubeOp } from './kubeOps';

const op = (o: Partial<KubeOp> = {}): KubeOp => ({ id: 'op-1', cluster: 'default', kind: 'upgrade', state: 'failed', command: 'fluxctl kube upgrade', lines: [], error: null, explain: null, started_secs_ago: 3, ...o });

describe('cluster operation helpers', () => {
  it('builds encoded paths', () => {
    expect(upgradePlanPath('a b', 'v1.31')).toBe('/v1/k3s/a%20b/upgrade/plan?channel=v1.31');
    expect(upgradePath('lab')).toBe('/v1/k3s/lab/upgrade');
    expect(nodeAddPath('lab')).toBe('/v1/k3s/lab/nodes/add');
    expect(nodeRemovePath('lab', 'lab-a1')).toBe('/v1/k3s/lab/nodes/lab-a1/remove');
    expect(opPath('op-1')).toBe('/v1/k3s-ops/op-1');
  });
  it('words state and kind with a tone', () => {
    expect(kindWord('node-add')).toBe('Adding a node');
    expect(opWord(op({ state: 'running' }))).toBe('Upgrade is running');
    expect(opWord(op({ state: 'done', kind: 'node-add' }))).toBe('Adding a node finished');
    expect(opTone('done')).toBe('ok');
    expect(opTone('failed')).toBe('bad');
    expect(opTone('running')).toBe('warn');
  });
  it('separates the failure from the explain line and offers the explain command', () => {
    const e = op({ error: 'Upgrade refused: a node is not Ready\n  more: fluxctl explain KUBE_UPGRADE_REFUSED', explain: 'KUBE_UPGRADE_REFUSED' });
    expect(failureText(e)).toBe('Upgrade refused: a node is not Ready');
    expect(explainCommand(e)).toBe('fluxctl explain KUBE_UPGRADE_REFUSED');
    expect(explainCommand(op())).toBeNull();
    expect(failureText(op())).toBe('');
  });
  it('says what to do next per failure', () => {
    expect(nextStep(op({ explain: 'KUBE_UPGRADE_REFUSED' }))).toMatch(/Nothing was changed/);
    expect(nextStep(op({ explain: 'KUBE_OP_BUSY' }))).toMatch(/Wait/);
    expect(nextStep(op({ explain: 'KUBE_NODE_UNSUPPORTED', kind: 'node-remove' }))).toMatch(/terminal/);
    expect(nextStep(op())).toMatch(/backup is kept/);
    expect(nextStep(op({ kind: 'node-add' }))).toMatch(/cluster status/);
    expect(nextStep(op({ state: 'done' }))).toBeNull();
  });
  it('needs the name typed again', () => {
    expect(confirmed(' lab ', 'lab')).toBe(true);
    expect(confirmed('la', 'lab')).toBe(false);
  });
});
