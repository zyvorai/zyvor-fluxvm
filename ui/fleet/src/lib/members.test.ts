import { describe, expect, it } from 'vitest';
import { assignableRoles, canEdit, canManage, isFull, isSelf, memberProblem, roleName } from './members';

const token = {};
const owner = { email: 'o@x.com', role: 'owner' };
const admin = { email: 'a@x.com', role: 'admin' };
const member = { email: 'm@x.com', role: 'member' };
const op = { email: 'op@x.com', operator: true };

describe('member roles', () => {
  it('lets a token, the operator and an owner do everything', () => {
    for (const v of [token, op, owner]) {
      expect(isFull(v)).toBe(true);
      expect(canManage(v)).toBe(true);
      expect(assignableRoles(v).map((r) => r.value)).toEqual(['viewer', 'member', 'admin', 'owner']);
    }
  });
  it('lets an admin manage everyone but owners, and never hand out owner', () => {
    expect(canManage(admin)).toBe(true);
    expect(isFull(admin)).toBe(false);
    expect(assignableRoles(admin).map((r) => r.value)).toEqual(['viewer', 'member', 'admin']);
    expect(canEdit(admin, { email: 'b@x.com', role: 'member' })).toBe(true);
    expect(canEdit(admin, { email: 'o2@x.com', role: 'owner' })).toBe(false);
    expect(canEdit(owner, { email: 'o2@x.com', role: 'owner' })).toBe(true);
  });
  it('lets a member and a viewer only read', () => {
    expect(canManage(member)).toBe(false);
    expect(canManage({ email: 'v@x.com', role: 'viewer' })).toBe(false);
    expect(canEdit(member, { email: 'b@x.com', role: 'viewer' })).toBe(false);
  });
  it('names roles and recognises yourself case-insensitively', () => {
    expect(roleName('admin')).toBe('Admin');
    expect(roleName('custom')).toBe('custom');
    expect(isSelf({ email: 'Me@X.com' }, { email: 'me@x.com', role: 'member' })).toBe(true);
    expect(isSelf(token, { email: 'me@x.com', role: 'member' })).toBe(false);
  });
});

describe('add a member', () => {
  it('checks the address, duplicates and the role', () => {
    expect(memberProblem('', 'member', owner, [])).toMatch(/email/);
    expect(memberProblem('nope', 'member', owner, [])).toMatch(/not an email/);
    expect(memberProblem('a b@x.com', 'member', owner, [])).toMatch(/not an email/);
    expect(memberProblem('Alex@Example.com', 'member', owner, ['alex@example.com'])).toMatch(/already/);
    expect(memberProblem('alex@example.com', 'owner', admin, [])).toMatch(/cannot give/);
    expect(memberProblem('alex@example.com', 'owner', owner, [])).toBe('');
    expect(memberProblem(' alex@example.com ', 'member', token, ['bob@example.com'])).toBe('');
  });
});
