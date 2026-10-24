import { describe, expect, it } from 'vitest';
import { type ConnectVm, connectRoutes, displayKind, forwardsOf, hasDisplayTab, loginOf, shq, webUrl } from './connect';

const vm = (over: Partial<ConnectVm> = {}): ConnectVm => ({ id: '5be2b2f0-aaaa', name: 'web-1', status: 'running', backend: 'qemu', guest_ip: '192.168.64.10', request: { cloud_init: { user: 'ubuntu' } }, ...over });
const by = (r: ReturnType<typeof connectRoutes>, key: string) => r.find((x) => x.key === key)!;

describe('shq', () => {
  it('leaves plain words alone and quotes the rest', () => {
    expect(shq('ubuntu')).toBe('ubuntu');
    expect(shq('web-1')).toBe('web-1');
    expect(shq('a b')).toBe("'a b'");
    expect(shq("it's")).toBe("'it'\\''s'");
    expect(shq('$(id)')).toBe("'$(id)'");
    expect(shq('')).toBe("''");
  });
});

describe('connectRoutes', () => {
  it('offers the CLI and OpenSSH routes for a running machine with an address', () => {
    const r = connectRoutes(vm(), 'mac.local');
    expect(by(r, 'cli').command).toBe('fluxctl ssh 5be2b2f0-aaaa -l ubuntu');
    expect(by(r, 'ssh').command).toBe('ssh ubuntu@192.168.64.10');
    expect(by(r, 'cli').ready && by(r, 'ssh').ready).toBe(true);
    expect(by(r, 'fleet').command).toBe('fluxctl fleet ssh web-1 -l ubuntu');
  });
  it('omits -l for root and says what is missing when stopped or without an address', () => {
    const root = connectRoutes(vm({ request: {} }), 'h');
    expect(by(root, 'cli').command).toBe('fluxctl ssh 5be2b2f0-aaaa');
    expect(by(root, 'ssh').command).toBe('ssh root@192.168.64.10');
    const stopped = connectRoutes(vm({ status: 'stopped' }), 'h');
    expect(by(stopped, 'ssh')).toMatchObject({ command: '', ready: false });
    expect(by(stopped, 'ssh').note).toMatch(/Start the machine/);
    const booting = connectRoutes(vm({ guest_ip: null }), 'h');
    expect(by(booting, 'ssh').note).toMatch(/No address/);
    expect(by(booting, 'cli').ready).toBe(false);
  });
  it('adds a route per published port 22, and the CLI works through it without an address', () => {
    const v = vm({ guest_ip: null, request: { cloud_init: { user: 'dev' }, network: { mode: 'user', forwards: [{ host_port: 2222, guest_port: 22 }, { host_port: 8080, guest_port: 80 }] } } });
    const r = connectRoutes(v, 'mac.local');
    expect(by(r, 'fwd-2222').command).toBe('ssh -p 2222 dev@mac.local');
    expect(r.some((x) => x.key === 'fwd-8080')).toBe(false);
    expect(by(r, 'cli')).toMatchObject({ ready: true, command: 'fluxctl ssh 5be2b2f0-aaaa -l dev' });
  });
  it('quotes a hostile user, name or host instead of passing it through', () => {
    const r = connectRoutes(vm({ name: 'a b', request: { cloud_init: { user: 'x;rm -rf /' }, network: { forwards: [{ host_port: 22, guest_port: 22 }] } } }), '$(host)');
    expect(by(r, 'cli').command).toContain("-l 'x;rm -rf /'");
    expect(by(r, 'fleet').command).toBe("fluxctl fleet ssh 'a b' -l 'x;rm -rf /'");
    expect(by(r, 'fwd-22').command).toBe("ssh -p 22 'x;rm -rf /'@'$(host)'");
  });
  it('never puts a non-address in the ssh command', () => {
    expect(by(connectRoutes(vm({ guest_ip: '1.2.3.4; reboot' }), 'h'), 'ssh').command).toBe('');
    expect(by(connectRoutes(vm({ guest_ip: 'fe80::1' }), 'h'), 'ssh').command).toBe('ssh ubuntu@fe80::1');
  });
});

describe('helpers', () => {
  it('reads forwards defensively', () => {
    expect(forwardsOf(vm())).toEqual([]);
    expect(forwardsOf(vm({ request: { network: { forwards: 'x' } } }))).toEqual([]);
    expect(forwardsOf(vm({ request: { network: { forwards: [{ host_port: 1, guest_port: 2 }, { host_port: 'a' }, null] } } }))).toEqual([{ host_port: 1, guest_port: 2 }]);
  });
  it('picks the login and decides about the display', () => {
    expect(loginOf(vm())).toBe('ubuntu');
    expect(loginOf(vm({ request: { cloud_init: { user: '  ' } } }))).toBe('root');
    expect(displayKind({ backend: 'vz', status: 'running' })).toBe('screenshot');
    expect(displayKind({ backend: 'qemu', status: 'running' })).toBe('vnc');
    expect(displayKind({ backend: 'vz', status: 'stopped' })).toBeNull();
    expect(displayKind({ backend: 'qemu', status: 'paused' })).toBeNull();
    expect(displayKind({ backend: 'firecracker', status: 'running' })).toBeNull();
    expect(hasDisplayTab({ backend: 'qemu' }) && hasDisplayTab({ backend: 'vz' })).toBe(true);
    expect(hasDisplayTab({ backend: 'firecracker' })).toBe(false);
  });
  it('links web-looking published ports only', () => {
    expect(webUrl({ host_port: 8080, guest_port: 80 }, 'mac.local')).toBe('http://mac.local:8080/');
    expect(webUrl({ host_port: 8443, guest_port: 443 }, 'mac.local')).toBe('https://mac.local:8443/');
    expect(webUrl({ host_port: 2222, guest_port: 22 }, 'mac.local')).toBeNull();
  });
});
