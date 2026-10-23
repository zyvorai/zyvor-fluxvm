// Who may sign in to a tenant through single sign-on, and what each role may do. Central enforces all of it; these helpers only
// keep the form from offering what it would refuse.

export type MemberRole = 'viewer' | 'member' | 'admin' | 'owner';

export interface Member {
  email: string;
  role: MemberRole | string;
  added_at?: string;
  added_by?: string;
  last_sign_in?: string;
}

export const ROLES: { value: MemberRole; label: string; text: string }[] = [
  { value: 'viewer', label: 'Viewer', text: 'Sees the tenant and its graphs, changes nothing, and does not get live screens or console logs.' },
  { value: 'member', label: 'Member', text: 'Launches and manages VMs and networks.' },
  { value: 'admin', label: 'Admin', text: 'Also leases Macs and manages keys, members, budget and webhooks.' },
  { value: 'owner', label: 'Owner', text: 'Everything, including other owners.' },
];

export const roleName = (r: string): string => ROLES.find((x) => x.value === r)?.label ?? r;

/** Who is looking: an operator or a token holder ("full" access), or someone signed in through SSO with a role. */
export interface Viewer {
  email?: string;
  role?: string;
  operator?: boolean;
}

/** The operator, a token (no SSO user) and an owner can hand out and change every role, including owner. */
export const isFull = (v: Viewer): boolean => !v.email || !!v.operator || v.role === 'owner';

/** Changing members needs an admin (or more); a viewer or member only reads the list. */
export const canManage = (v: Viewer): boolean => !v.email || !!v.operator || v.role === 'admin' || v.role === 'owner';

export const assignableRoles = (v: Viewer): typeof ROLES => (isFull(v) ? ROLES : ROLES.filter((r) => r.value !== 'owner'));

/** An admin may not touch an owner. */
export const canEdit = (v: Viewer, m: Member): boolean => canManage(v) && (isFull(v) || m.role !== 'owner');

export function memberProblem(email: string, role: string, v: Viewer, existing: string[]): string {
  const e = email.trim().toLowerCase();
  if (!e) return 'Give the work email.';
  if (e.length > 254 || !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(e)) return 'That is not an email address.';
  if (existing.some((x) => x.toLowerCase() === e)) return 'That person is already a member.';
  if (!assignableRoles(v).some((r) => r.value === role)) return 'You cannot give that role.';
  return '';
}

export const isSelf = (v: Viewer, m: Member): boolean => !!v.email && v.email.toLowerCase() === m.email.toLowerCase();
