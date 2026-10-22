import { describe, expect, it } from 'vitest';
import { describePolicy, groupBody, groupProblem, inCidr, isIpv4, isIpv4Cidr, protoName, serviceHealth, vnetBody, vnetProblem } from './network';

const vnet = { vni: '100', cidr: '10.20.0.0/24', gateway: '10.20.0.1', uplink: '', snat: '' };
const group = { name: 'web', labels: 'role=web', cidrs: '10.0.0.0/16', ports: 'tcp/443', defaultAllow: false, priority: '', description: '' };

describe('addresses', () => {
  it('accepts IPv4 only', () => {
    expect(isIpv4('10.0.0.1')).toBe(true);
    expect(isIpv4('10.0.0.256')).toBe(false);
    expect(isIpv4('10.0.0')).toBe(false);
    expect(isIpv4Cidr('10.20.0.0/24')).toBe(true);
    expect(isIpv4Cidr('10.20.0.0/33')).toBe(false);
    expect(isIpv4Cidr('10.20.0.0')).toBe(false);
  });
  it('tests membership by prefix', () => {
    expect(inCidr('10.20.0.1', '10.20.0.0/24')).toBe(true);
    expect(inCidr('10.20.1.1', '10.20.0.0/24')).toBe(false);
    expect(inCidr('192.168.5.5', '0.0.0.0/0')).toBe(true);
  });
});

describe('virtual network form', () => {
  it('passes a plain network', () => expect(vnetProblem(vnet)).toBe(''));
  it('rejects a gateway outside the network', () => expect(vnetProblem({ ...vnet, gateway: '10.21.0.1' })).toMatch(/inside/));
  it('needs an uplink for SNAT', () => {
    expect(vnetProblem({ ...vnet, snat: '203.0.113.5' })).toMatch(/uplink/);
    expect(vnetProblem({ ...vnet, snat: '203.0.113.5', uplink: 'eth0' })).toBe('');
  });
  it('bounds the VNI', () => {
    expect(vnetProblem({ ...vnet, vni: '0' })).toMatch(/VNI/);
    expect(vnetProblem({ ...vnet, vni: '16777216' })).toMatch(/VNI/);
    expect(vnetProblem({ ...vnet, vni: 'x' })).toMatch(/VNI/);
  });
  it('keeps floating IPs and endpoints it was not asked to change', () => {
    const prev = { vni: 100, cidr: '10.20.0.0/24', gateway: '10.20.0.1', floating_ips: [{ address: '1.2.3.4', ip: '10.20.0.5' }], endpoints: [{ ip: '10.20.0.5', mac: 'aa', node_ip: '1.1.1.1' }] };
    const body = vnetBody({ ...vnet, uplink: 'eth0' }, prev);
    expect(body.floating_ips).toEqual(prev.floating_ips);
    expect(body.endpoints).toEqual(prev.endpoints);
    expect(body.uplink).toBe('eth0');
    expect(body.snat_address).toBeNull();
  });
});

describe('security group form', () => {
  it('passes a plain group', () => expect(groupProblem(group)).toBe(''));
  it('checks the name, labels, CIDRs and ports', () => {
    expect(groupProblem({ ...group, name: 'Web Tier' })).toMatch(/Name/);
    expect(groupProblem({ ...group, labels: 'role' })).toMatch(/Labels/);
    expect(groupProblem({ ...group, cidrs: '10.0.0.0/99' })).toMatch(/not a CIDR/);
    expect(groupProblem({ ...group, ports: '443' })).toMatch(/tcp\/443/);
    expect(groupProblem({ ...group, ports: 'tcp/70000' })).toMatch(/tcp\/443/);
  });
  it('splits lists on spaces and commas', () => {
    const b = groupBody({ ...group, labels: 'a=b, c=d', ports: 'tcp/80 udp/53', priority: '5' });
    expect(b.labels).toEqual(['a=b', 'c=d']);
    expect(b.policy?.allow_ports).toEqual(['tcp/80', 'udp/53']);
    expect(b.priority).toBe(5);
  });
  it('says an allowlist denies the rest whatever the default is', () => {
    expect(describePolicy({ default_allow: true, allow_cidrs: ['10.0.0.0/8'] })).toMatch(/denies the rest/);
    expect(describePolicy({ default_allow: true })).toBe('allows everything');
    expect(describePolicy({})).toBe('denies everything');
    expect(describePolicy({ allow_ports: ['tcp/1'], deny_cidrs: ['1.0.0.0/8'] })).toBe('allows 1 port, denies the rest; 1 deny CIDR');
  });
});

describe('service health', () => {
  const b = (service: string, healthy: boolean) => ({ service, backend_index: 0, address: '1.1.1.1', port: 80, healthy });
  it('is unknown without probes, not healthy', () => expect(serviceHealth('api', [b('other', true)])).toBe('unknown'));
  it('grades by how many backends answer', () => {
    expect(serviceHealth('api', [b('api', true), b('api', true)])).toBe('healthy');
    expect(serviceHealth('api', [b('api', true), b('api', false)])).toBe('degraded');
    expect(serviceHealth('api', [b('api', false)])).toBe('down');
  });
});

it('names protocols', () => {
  expect(protoName(6)).toBe('tcp');
  expect(protoName(17)).toBe('udp');
  expect(protoName(99)).toBe('99');
  expect(protoName(undefined)).toBe('—');
});
