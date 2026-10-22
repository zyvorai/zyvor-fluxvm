import { describe, expect, it } from 'vitest';
import { launchProblem, launchRequest, leaseProgress, nodeState, powerActions, tenantIdProblem, vmKey, vmState } from './fleet';

describe('fleet helpers', () => {
  it('keys a vm by node and id', () => expect(vmKey({ node: 'mac-1', id: 'abc' })).toBe('mac-1/abc'));
  it('reads every status shape', () => {
    expect(vmState({ node: 'n', id: 'i', name: 'x', status: 'Running' })).toBe('running');
    expect(vmState({ node: 'n', id: 'i', name: 'x', status: { stopped: {} } })).toBe('stopped');
    expect(vmState({ node: 'n', id: 'i', name: 'x', state: 'paused' })).toBe('paused');
    expect(vmState({ node: 'n', id: 'i', name: 'x' })).toBe('unknown');
  });
  it('offers sensible power actions', () => {
    expect(powerActions('running')).toEqual(['stop', 'restart', 'pause']);
    expect(powerActions('stopped')).toEqual(['start']);
    expect(powerActions('creating')).toEqual([]);
  });
  it('ranks node state: unhealthy beats scrub beats drain beats cordon', () => {
    expect(nodeState({ name: 'a', healthy: false, cordoned: true }, { draining: true }).text).toBe('Not responding');
    expect(nodeState({ name: 'a' }, { scrub_pending: true, draining: true }).text).toBe('Needs scrub');
    expect(nodeState({ name: 'a' }, { draining: true }).text).toBe('Draining');
    expect(nodeState({ name: 'a', cordoned: true }).text).toBe('Cordoned');
    expect(nodeState({ name: 'a' }, {}, { id: 'l', node: 'a', tenant: 'acme', state: 'active', started_at: '', min_end: '' }).text).toBe('Leased to acme');
    expect(nodeState({ name: 'a', vm_count: 0 }).text).toBe('Available');
  });
  it('tracks the 24 hour lease minimum', () => {
    const l = { started_at: '2026-10-01T00:00:00Z', min_end: '2026-10-02T00:00:00Z' };
    expect(leaseProgress(l, Date.parse('2026-10-01T12:00:00Z'))).toEqual({ p: 0.5, releasable: false });
    expect(leaseProgress(l, Date.parse('2026-10-03T00:00:00Z'))).toEqual({ p: 1, releasable: true });
    expect(leaseProgress({ started_at: 'x', min_end: 'y' }).releasable).toBe(true);
  });
  it('validates tenant ids', () => {
    expect(tenantIdProblem('acme-1')).toBe('');
    expect(tenantIdProblem('Acme')).not.toBe('');
    expect(tenantIdProblem('-x')).not.toBe('');
    expect(tenantIdProblem('')).not.toBe('');
  });
  const draft = { name: 'web-1', image: '/img/macos.img', guest: 'macos', vcpus: '4', memoryGib: '8', node: '', ttl: '', networks: [] };
  it('validates a launch', () => {
    expect(launchProblem(draft)).toBe('');
    expect(launchProblem({ ...draft, name: '' })).not.toBe('');
    expect(launchProblem({ ...draft, vcpus: '0' })).not.toBe('');
    expect(launchProblem({ ...draft, memoryGib: '0.5' })).not.toBe('');
    expect(launchProblem({ ...draft, image: ' ' })).not.toBe('');
  });
  it('builds the launch body central expects', () => {
    expect(launchRequest({ ...draft, node: 'mac-2', ttl: '3600', networks: ['net-a'], diskGib: 80 })).toEqual({
      name: 'web-1', backend: 'vz', image: '/img/macos.img', vcpus: 4, memory_mib: 8192,
      apple: { guest_os: 'macos', networks: [{ name: 'net-a' }] }, disk_size_gib: 80, node: 'mac-2', ttl_seconds: 3600,
    });
    expect(launchRequest(draft)).not.toHaveProperty('node');
  });
});
