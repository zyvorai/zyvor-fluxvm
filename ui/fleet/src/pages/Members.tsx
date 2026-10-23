import { useEffect, useState } from 'react';
import { Plus } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Sheet, Skeleton, ago, api, apiBase, getToken, usePoll } from '@fluxvm/ui-core';
import type { Ctx, Notify } from '../App';
import { useAct } from '../lib/act';
import { type Member, type Viewer, assignableRoles, canEdit, canManage, isSelf, memberProblem, roleName } from '../lib/members';

const enc = encodeURIComponent;

/** Who is signed in through SSO, if anyone. A token holder (a token in session storage) has no SSO user and is not asked: the
 *  session route would answer 401 and log a console error. Plain fetch otherwise, so a 401 cannot sign the page out. */
function useViewer(): Viewer | null {
  const [v, setV] = useState<Viewer | null>(() => (getToken() ? {} : null));
  useEffect(() => {
    if (getToken()) return;
    let live = true;
    fetch(`${apiBase()}/auth/session`, { cache: 'no-store', credentials: 'same-origin' })
      .then(async (r) => (r.ok ? ((await r.json()) as Viewer) : {}))
      .catch(() => ({}))
      .then((x) => live && setV(x ?? {}));
    return () => {
      live = false;
    };
  }, []);
  return v;
}

/** The members of one tenant ('me' for a tenant's own page, or a tenant id for the operator): roles, add, remove. */
export function MemberList({ owner, notify, compact }: { owner: string; notify: Notify; compact?: boolean }) {
  const base = `/fleet/tenants/${enc(owner)}/members`;
  const viewer = useViewer();
  const list = usePoll(async () => {
    const r = await api.get<{ items?: Member[]; sso?: boolean }>(base);
    return { items: r.items ?? [], sso: !!r.sso };
  }, 30000, [base]);
  const { busy, run } = useAct(notify);
  const [add, setAdd] = useState<{ email: string; role: string } | null>(null);
  const [touched, setTouched] = useState(false);
  const [del, setDel] = useState<Member | null>(null);
  const v = viewer ?? {};
  const items = list.data?.items ?? [];
  const manage = !!viewer && canManage(v);
  const problem = add ? memberProblem(add.email, add.role, v, items.map((m) => m.email)) : '';
  const path = (m: Member) => `${base}/${enc(m.email)}`;

  return (
    <div>
      {manage ? <div className="fx-actions" style={{ marginBottom: 12 }}><Button variant="primary" size={compact ? 'sm' : 'md'} onClick={() => (setAdd({ email: '', role: 'member' }), setTouched(false))}><Plus size={16} />Add member</Button></div> : null}
      {list.data && !list.data.sso ? <p className="fx-muted" role="status">Single sign-on is not set up on this central yet, so members cannot sign in until the operator configures it. You can add them now.</p> : null}
      {list.loading && !list.data ? <Skeleton h={100} /> : list.error && !list.data ? <p className="fx-error" role="alert">The members could not be loaded: {list.error.message}</p> : items.length === 0 ? (
        <EmptyState title="Just the token so far" body="Add people by their work email. They sign in with the company's single sign-on and get the role you pick: viewer, member, admin or owner." />
      ) : (
        <div className="fx-table-wrap"><table className="fx-table">
          <thead><tr><th>Person</th><th>Role</th>{compact ? null : <th>Added</th>}<th>Last sign-in</th><th /></tr></thead>
          <tbody>
            {items.map((m) => {
              const self = viewer ? isSelf(v, m) : false;
              const editable = viewer ? canEdit(v, m) : false;
              return (
                <tr key={m.email}>
                  <td><div className="vm-name">{m.email}</div><div className="fx-muted">{self ? 'You' : m.added_by ? `Added by ${m.added_by}` : ''}</div></td>
                  <td>{editable ? (
                    <select className="fx-input" aria-label={`Role of ${m.email}`} value={m.role} disabled={busy === 'role' + m.email}
                      onChange={(e) => void run('role' + m.email, () => api.patch(path(m), { role: e.target.value }), `${m.email} is now ${roleName(e.target.value).toLowerCase()}`, list.refresh)}>
                      {assignableRoles(v).some((r) => r.value === m.role) ? null : <option value={m.role}>{roleName(m.role)}</option>}
                      {assignableRoles(v).map((r) => <option key={r.value} value={r.value}>{r.label}</option>)}
                    </select>
                  ) : <Pill tone={m.role === 'owner' || m.role === 'admin' ? 'info' : 'idle'} dot={false}>{roleName(m.role)}</Pill>}</td>
                  {compact ? null : <td className="fx-muted">{m.added_at ? ago(m.added_at) : '—'}</td>}
                  <td className="fx-muted">{m.last_sign_in ? ago(m.last_sign_in) : 'Not yet'}</td>
                  <td>{editable ? <Button size="sm" variant="danger" aria-label={`${self ? 'Leave' : 'Remove'} ${m.email}`} onClick={() => setDel(m)}>{self ? 'Leave' : 'Remove'}</Button> : null}</td>
                </tr>
              );
            })}
          </tbody>
        </table></div>
      )}
      <Sheet open={!!add} title="Add a member" onClose={() => setAdd(null)}>
        {add ? (
          <>
            <p className="fx-muted">They sign in with single sign-on using this email. Nothing is sent to them: tell them the console address.</p>
            <Field label="Work email" hint="It has to match the email your identity provider has verified for them.">{(id) => <input id={id} className="fx-input" type="email" maxLength={254} placeholder="alex@example.com" autoComplete="off" spellCheck={false} value={add.email} onChange={(e) => setAdd({ ...add, email: e.target.value })} />}</Field>
            <fieldset className="fx-roles"><legend>Role</legend>
              {assignableRoles(v).map((r) => (
                <label key={r.value} className="fx-check"><input type="radio" name="member-role" checked={add.role === r.value} onChange={() => setAdd({ ...add, role: r.value })} /> <b>{r.label}</b> <span className="fx-muted">{r.text}</span></label>
              ))}
            </fieldset>
            {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
            <div className="fx-actions"><Button variant="primary" busy={busy === 'add'} onClick={async () => {
              setTouched(true);
              if (problem) return;
              if (await run('add', () => api.post(base, { email: add.email.trim(), role: add.role }), `${add.email.trim()} can now sign in as ${roleName(add.role).toLowerCase()}`, list.refresh)) setAdd(null);
            }}>Add member</Button><Button onClick={() => setAdd(null)}>Cancel</Button></div>
          </>
        ) : null}
      </Sheet>
      <Sheet open={!!del} title={del && isSelf(v, del) ? 'Leave the team' : `Remove ${del?.email ?? ''}`} onClose={() => setDel(null)}>
        {del ? (
          <>
            <p>{isSelf(v, del)
              ? 'You are signed out right away and cannot sign in to this team again unless someone adds you back.'
              : `${del.email} is signed out right away and can no longer sign in. Their past activity stays in the log.`}</p>
            <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => { if (await run('del', () => api.del(path(del)), `${del.email} removed`, list.refresh)) setDel(null); }}>{isSelf(v, del) ? 'Leave team' : 'Remove member'}</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
          </>
        ) : null}
      </Sheet>
    </div>
  );
}

/** A tenant's own team: the people who sign in to this console and what each may do. */
export function MembersPage({ notify }: Ctx) {
  return (
    <>
      <Hero eyebrow="Members" title="Your team," accent="and what each may do." lede="The people who sign in to this console with single sign-on, and the role each has." />
      <Card><MemberList owner="me" notify={notify} /></Card>
      <p className="fx-muted" style={{ marginTop: 14 }}>Viewers only read and get no live screens. Members run VMs and networks. Admins also manage hosts, keys, members, budget and webhooks. Owners can do everything, including managing other owners.</p>
    </>
  );
}
