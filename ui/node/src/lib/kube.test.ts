import { describe, expect, it } from 'vitest';
import { type KubeEvent, type KubePod, type Workloads, ageOf, eventTone, filterEvents, filterPods, fmtCpu, fmtMem, inNamespace, kubeProblem, namespacesOf, namespacesOfAll, namespacesOfEvents, podTone, portText, readyTone, shortImage, summarize, usageTone } from './kube';

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

describe('usage formatting', () => {
  it('shows millicores and cores, MiB and GiB', () => {
    expect([fmtCpu(undefined), fmtCpu(0), fmtCpu(250), fmtCpu(999), fmtCpu(1000), fmtCpu(1500), fmtCpu(12000)]).toEqual(['—', '0m', '250m', '999m', '1.00', '1.50', '12.0']);
    expect([fmtMem(undefined), fmtMem(12), fmtMem(1023), fmtMem(1024), fmtMem(2560)]).toEqual(['—', '12 MiB', '1023 MiB', '1.0 GiB', '2.5 GiB']);
  });
  it('turns amber at 75 % and red at 90 %', () => {
    expect([usageTone(null), usageTone(undefined), usageTone(10), usageTone(74.9), usageTone(75), usageTone(89.9), usageTone(90), usageTone(100)]).toEqual(['idle', 'idle', 'ok', 'ok', 'warn', 'warn', 'bad', 'bad']);
  });
});

describe('workloads', () => {
  const w = (name: string, ns: string, desired: number, ready: number) => ({ namespace: ns, name, desired, ready, updated: ready, available: ready, images: [], created: null });
  it('colours ready of wanted', () => {
    expect([readyTone(w('a', 'x', 3, 3)), readyTone(w('a', 'x', 3, 1)), readyTone(w('a', 'x', 3, 0)), readyTone(w('a', 'x', 0, 0)), readyTone(w('a', 'x', 1, 2))]).toEqual(['ok', 'warn', 'bad', 'idle', 'ok']);
  });
  it('writes ports the way a Service lists them', () => {
    expect(portText({ name: 'http', protocol: 'TCP', port: 80, target_port: 8080, node_port: 30080 })).toBe('80→8080/TCP:30080');
    expect(portText({ name: null, protocol: 'UDP', port: 53, target_port: 53, node_port: null })).toBe('53/UDP');
    expect(portText({ name: null, protocol: 'TCP', port: 443, target_port: 'https', node_port: null })).toBe('443→https/TCP');
    expect(portText({ name: null, protocol: 'TCP', port: 80, target_port: null, node_port: null })).toBe('80/TCP');
  });
  it('shortens image references', () => {
    expect(shortImage('docker.io/library/nginx:1.29-alpine')).toBe('nginx:1.29-alpine');
    expect(shortImage('quay.io/cilium/cilium:v1.20.3')).toBe('cilium:v1.20.3');
    expect(shortImage('ghcr.io/o/app@sha256:0123456789abcdef0123')).toBe('app@sha256:01234567');
    expect(shortImage('busybox')).toBe('busybox');
  });
  it('filters by namespace and lists namespaces from every kind', () => {
    const all: Workloads = { cluster: 'c', deployments: [w('a', 'default', 1, 1)], statefulsets: [w('db', 'data', 1, 1)], daemonsets: [w('cilium', 'kube-system', 1, 1)], services: [{ namespace: 'default', name: 's', type: 'ClusterIP', cluster_ip: null, external: [], ports: [], selector: [], created: null }] };
    expect(namespacesOfAll(all)).toEqual(['data', 'default', 'kube-system']);
    expect(inNamespace(all.deployments, '').length).toBe(1);
    expect(inNamespace(all.deployments, 'data')).toEqual([]);
  });
});

describe('events', () => {
  const ev = (type: string, reason: string, object: string, ns: string | null, message = ''): KubeEvent => ({ namespace: ns, type, reason, message, object, kind: 'Pod', name: object, count: 1, first: null, last: null, component: 'kubelet' });
  const events = [ev('Warning', 'BackOff', 'Pod/api-1', 'default', 'Back-off restarting failed container'), ev('Normal', 'Pulled', 'Pod/web-1', 'default', 'image pulled'), ev('Warning', 'FailedMount', 'Pod/db-0', 'data', 'volume x missing')];
  it('filters by type, namespace and text, keeping the order', () => {
    const f = { warningsOnly: false, namespace: '', text: '' };
    expect(filterEvents(events, f).length).toBe(3);
    expect(filterEvents(events, { ...f, warningsOnly: true }).map((e) => e.reason)).toEqual(['BackOff', 'FailedMount']);
    expect(filterEvents(events, { ...f, namespace: 'data' }).map((e) => e.reason)).toEqual(['FailedMount']);
    expect(filterEvents(events, { ...f, text: ' BACK-OFF ' }).map((e) => e.reason)).toEqual(['BackOff']);
    expect(filterEvents(events, { ...f, text: 'kubelet' }).length).toBe(3);
    expect(filterEvents(events, { ...f, text: 'zzz' })).toEqual([]);
  });
  it('colours warnings and lists namespaces', () => {
    expect([eventTone('Warning'), eventTone('Normal')]).toEqual(['warn', 'idle']);
    expect(namespacesOfEvents([...events, ev('Normal', 'X', 'Node/n', null)])).toEqual(['data', 'default']);
  });
});
