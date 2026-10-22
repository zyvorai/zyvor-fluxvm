import { describe, expect, it } from 'vitest';
import { MAX_RULES, addRule, crowdedPrefixes, describeRule, fmtBytes, normalizeCidr, samePolicy, validateRule } from './egress';

describe('normalizeCidr', () => {
  it('accepts IPv4 and CIDR, defaulting to /32', () => {
    expect(normalizeCidr('10.0.0.0/16')).toBe('10.0.0.0/16');
    expect(normalizeCidr(' 1.2.3.4 ')).toBe('1.2.3.4/32');
  });
  it('rejects junk, IPv6 and out-of-range', () => {
    for (const bad of ['', 'abc', '256.1.1.1', '10.0.0.0/33', '::1', '10.0.0']) expect(normalizeCidr(bad)).toBeNull();
  });
});

describe('validateRule', () => {
  it('requires tcp/udp with a port', () => {
    expect(validateRule({ cidr: '10.0.0.0/8', port: 443 })).toMatch(/tcp or udp/);
    expect(validateRule({ cidr: '10.0.0.0/8', proto: 'tcp', port: 443 })).toBe('');
    expect(validateRule({ cidr: '10.0.0.0/8', proto: 'tcp', port: 70000 })).toMatch(/Port/);
    expect(validateRule({ cidr: 'nope' })).toMatch(/IPv4/);
  });
});

describe('addRule', () => {
  it('normalises, dedupes and caps', () => {
    const a = addRule([], { cidr: '1.2.3.4', proto: 'tcp', port: 443, note: ' api ' });
    expect(a.rules).toEqual([{ cidr: '1.2.3.4/32', proto: 'tcp', port: 443, note: 'api' }]);
    expect(addRule(a.rules, { cidr: '1.2.3.4/32', proto: 'tcp', port: 443 }).error).toMatch(/already/);
    const full = Array.from({ length: MAX_RULES }, (_, i) => ({ cidr: `10.${i >> 8}.${i & 255}.0/24` }));
    expect(addRule(full, { cidr: '9.9.9.9' }).error).toMatch(/At most/);
  });
});

describe('helpers', () => {
  it('describes rules', () => {
    expect(describeRule({ cidr: '10.0.0.0/16', proto: 'tcp', port: 5151 })).toBe('10.0.0.0/16 tcp/5151');
    expect(describeRule({ cidr: '1.1.1.1/32', proto: 'any' })).toBe('1.1.1.1/32');
  });
  it('flags prefixes over the datapath port-fold limit', () => {
    const rules = [80, 443, 8080, 8443, 9000].map((port) => ({ cidr: '10.0.0.0/8', proto: 'tcp' as const, port }));
    expect(crowdedPrefixes(rules)).toEqual(['10.0.0.0/8']);
    expect(crowdedPrefixes(rules.slice(0, 4))).toEqual([]);
  });
  it('compares policies by content', () => {
    expect(samePolicy({ mode: 'allowlist', rules: [{ cidr: '1.1.1.1' }] }, { mode: 'allowlist', rules: [{ cidr: '1.1.1.1/32' }] })).toBe(true);
    expect(samePolicy({ mode: 'audit', rules: [] }, { mode: 'open', rules: [] })).toBe(false);
  });
  it('formats bytes', () => {
    expect(fmtBytes(512)).toBe('512 B');
    expect(fmtBytes(2048)).toBe('2.0 KiB');
    expect(fmtBytes(5 * (1 << 20))).toBe('5.0 MiB');
  });
});
