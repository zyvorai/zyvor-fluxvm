import { describe, expect, it } from 'vitest';
import { type KubePod, ageOf, filterPods, kubeProblem, namespacesOf, podTone, summarize } from './kube';

const pod = (name: string, status: string, over: Partial<KubePod> = {}): KubePod => ({
  namespace: 'default', name, status, phase: null, ready: '1/1', restarts: 0, node: 'n1', pod_ip: '10.42.0.5', created: null, ...over,
});

describe('podTone', () => {
  it('colours by what kubectl would print', () => {
    expect(podTone(pod('a', 'Running'))).toBe('ok');
    expect(podTone(pod('a', 'Running', { ready: '1/2' }))).toBe('warn');
    expect(podTone(pod('a', 'ContainerCreating'))).toBe('warn');
    expect(podTone(pod('a', 'Init:0/6'))).toBe('warn');
    expect(podTone(pod('a', 'CrashLoopBackOff'))).toBe('bad');
    expect(podTone(pod('a', 'OOMKilled'))).toBe('bad');
    expect(podTone(pod('a', 'Succeeded'))).toBe('idle');
    expect(podTone(pod('a', 'Terminating'))).toBe('warn');
  });
});

describe('summarize and filter', () => {
  const pods = [pod('web', 'Running'), pod('db', 'CrashLoopBackOff'), pod('job', 'Succeeded'), pod('new', 'Pending'), pod('dns', 'Running', { namespace: 'kube-system', node: 'n2', pod_ip: '10.42.1.3' })];
  it('counts each group', () => {
    expect(summarize(pods)).toEqual({ total: 5, healthy: 2, waiting: 1, failing: 1, done: 1 });
    expect(summarize([])).toEqual({ total: 0, healthy: 0, waiting: 0, failing: 0, done: 0 });
  });
  it('lists namespaces once, sorted', () => {
    expect(namespacesOf(pods)).toEqual(['default', 'kube-system']);
  });
  it('puts failing pods first and filters by namespace and text', () => {
    expect(filterPods(pods, '', '').map((p) => p.name)).toEqual(['db', 'new', 'web', 'dns', 'job']);
    expect(filterPods(pods, 'kube-system', '').map((p) => p.name)).toEqual(['dns']);
    expect(filterPods(pods, '', '10.42.1').map((p) => p.name)).toEqual(['dns']);
    expect(filterPods(pods, '', 'N2').map((p) => p.name)).toEqual(['dns']);
    expect(filterPods(pods, '', 'crashloop').map((p) => p.name)).toEqual(['db']);
    expect(filterPods(pods, 'nope', '')).toEqual([]);
  });
});

describe('ageOf', () => {
  const now = Date.parse('2026-10-24T12:00:00Z');
  it('uses the first unit that is not zero', () => {
    expect(ageOf('2026-10-24T11:59:15Z', now)).toBe('45s');
    expect(ageOf('2026-10-24T11:48:00Z', now)).toBe('12m');
    expect(ageOf('2026-10-24T09:00:00Z', now)).toBe('3h');
    expect(ageOf('2026-10-22T00:00:00Z', now)).toBe('2d');
    expect(ageOf('2026-10-24T12:00:30Z', now)).toBe('0s');
  });
  it('shows a dash when there is no time', () => {
    expect(ageOf(null, now)).toBe('—');
    expect(ageOf('not a date', now)).toBe('—');
  });
});

describe('kubeProblem', () => {
  it('explains the common refusals', () => {
    expect(kubeProblem('no kubeconfig at /Users/x/.kube/fluxvm-a.yaml for the daemon’s user').title).toMatch(/kubeconfig/);
    expect(kubeProblem('the server VM a is stopped').body).toMatch(/Start it from Machines/);
    expect(kubeProblem('the server VM has no address yet').title).toMatch(/no address/);
    expect(kubeProblem('boom').title).toBe('Could not read the cluster');
  });
});
