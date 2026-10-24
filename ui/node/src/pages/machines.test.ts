import { describe, expect, it } from 'vitest';
import { actionsFor, portsOf, stacksOf } from './Machines';

describe('actionsFor', () => {
  it('offers the right lifecycle verbs per state', () => {
    expect(actionsFor('running')).toEqual(['pause', 'restart', 'stop']);
    expect(actionsFor('paused')).toEqual(['resume', 'stop']);
    expect(actionsFor('stopped')).toEqual(['start']);
    expect(actionsFor('failed')).toEqual(['start']);
    expect(actionsFor('creating')).toEqual([]);
  });
});

import { buildRequest, emptyDraft, validate } from './CreateMachine';
import { pushPoint } from './MachineDetail';

describe('create wizard', () => {
  const base = { ...emptyDraft(), name: 'web-01', image: 'ubuntu-24.04' };
  it('builds a NAT request with forwards and cloud-init', () => {
    const r = buildRequest({ ...base, forwards: [{ host_port: '8080', guest_port: '80' }, { host_port: '', guest_port: '' }], user: 'ubuntu', ssh: 'ssh-ed25519 AAA\n\n' });
    expect(r).toMatchObject({ name: 'web-01', backend: 'auto', image: 'ubuntu-24.04', vcpus: 2, memory_mib: 4096, disk_size_gib: 20 });
    expect(r.network).toEqual({ mode: 'user', forwards: [{ host_port: 8080, guest_port: 80, protocol: 'tcp' }] });
    expect(r.cloud_init).toEqual({ user: 'ubuntu', ssh_authorized_keys: ['ssh-ed25519 AAA'] });
  });
  it('omits cloud-init when nothing is set and supports none/bridged', () => {
    expect(buildRequest({ ...base, net: 'none' })).toMatchObject({ network: { mode: 'none' } });
    expect(buildRequest({ ...base, net: 'none' }).cloud_init).toBeUndefined();
    expect(buildRequest({ ...base, net: 'tap', bridge: 'br0' }).network).toEqual({ mode: 'tap', bridge: 'br0' });
  });
  it('validates each step', () => {
    expect(validate({ ...base, name: 'bad name!' }, 0)).toMatch(/Name/);
    expect(validate({ ...base, image: '' }, 0)).toMatch(/image/i);
    expect(validate(base, 0)).toBe('');
    expect(validate({ ...base, vcpus: 0 }, 1)).toMatch(/Size/);
    expect(validate({ ...base, forwards: [{ host_port: '99999', guest_port: '80' }] }, 2)).toMatch(/ports/);
  });
});

describe('pushPoint', () => {
  it('keeps a bounded window', () => {
    expect(pushPoint([1, 2, 3], 4, 3)).toEqual([2, 3, 4]);
  });
});

import { migrationPct } from './tabs';
describe('migrationPct', () => {
  it('computes a bounded percentage', () => {
    expect(migrationPct({ ram_transferred: 50, ram_total: 200 })).toBe(25);
    expect(migrationPct({ ram_transferred: 500, ram_total: 200 })).toBe(100);
    expect(migrationPct({})).toBe(0);
    expect(migrationPct(null)).toBe(0);
  });
});

const vm = (id: string, extra: object = {}) => ({ id, name: id, status: 'running', backend: 'x', request: { vcpus: 1, memory_mib: 1 }, ...extra }) as never;

describe('portsOf', () => {
  it('lists forwards, marking guest-only ones', () => {
    const v = vm('a', { request: { vcpus: 1, memory_mib: 1, network: { forwards: [{ host_port: 8080, guest_port: 80 }, { host_port: 22, guest_port: 22, guests: true }] } } });
    expect(portsOf(v)).toBe('8080→80 guests:22→22');
    expect(portsOf(vm('b'))).toBe('');
  });
});

describe('stacksOf', () => {
  it('groups by the stack label, sorted, ignoring unlabelled VMs', () => {
    const s = stacksOf([vm('1', { labels: { 'fluxvm.stack': 'web' } }), vm('2'), vm('3', { labels: { 'fluxvm.stack': 'db' } }), vm('4', { labels: { 'fluxvm.stack': 'web' } })]);
    expect(s.map((x) => [x.name, x.vms.length])).toEqual([['db', 1], ['web', 2]]);
  });
});

describe('stacksOf', () => {
  const vm = (name: string, labels?: Record<string, string>) => ({ id: name, name, labels }) as never;
  it('groups by stack label and by Kubernetes cluster', () => {
    const groups = stacksOf([vm('a', { 'fluxvm.stack': 's1' }), vm('b', { 'fluxvm.stack': 's1' }), vm('k', { 'fluxvm.kube': 'k' }), vm('k-w1', { 'fluxvm.kube': 'k' }), vm('old', { 'fluxvm.kube': '1' }), vm('plain')]);
    expect(groups.map((g) => [g.name, g.vms.map((v) => v.name)])).toEqual([
      ['kube k', ['k', 'k-w1']],
      ['kube old', ['old']],
      ['s1', ['a', 'b']],
    ]);
  });
});
