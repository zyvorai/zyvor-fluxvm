import { useState } from 'react';
import { Plus, RefreshCw, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Sheet, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { tenantIdProblem } from '../lib/fleet';
import { MemberList } from './Members';
import { SecretSheet } from './Secret';

const enc = encodeURIComponent;
type Row = Record<string, unknown>;
const QUOTA: [string, string][] = [['max_leased_hosts', 'Hosts'], ['max_vms', 'VMs'], ['max_vcpus', 'vCPUs'], ['max_volume_gib', 'Volume storage (GiB)']];

export function TenantsPage({ notify }: Ctx) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/tenants'), 'items'), 10000);
  const { busy, run } = useAct(notify);
  const [create, setCreate] = useState(false);
  const [id, setId] = useState('');
  const [name, setName] = useState('');
  const [quota, setQuota] = useState<Record<string, string>>({ max_leased_hosts: '2' });
  const [edit, setEdit] = useState<Row | null>(null);
  const [del, setDel] = useState<string | null>(null);
  const [people, setPeople] = useState<string | null>(null);
  const [secret, setSecret] = useState<{ title: string; token: string } | null>(null);
  const rows = list.data ?? [];
  const body = (q: Record<string, string>) => Object.fromEntries(Object.entries(q).filter(([, v]) => v.trim() !== '').map(([k, v]) => [k, Number(v)]));
  const quotaFields = (q: Record<string, string>, set: (q: Record<string, string>) => void) => (
    <div className="fx-grid cols-2">{QUOTA.map(([k, label]) => <Field key={k} label={label}>{(fid) => <input id={fid} className="fx-input" inputMode="numeric" placeholder="No limit" value={q[k] ?? ''} onChange={(e) => set({ ...q, [k]: e.target.value })} />}</Field>)}</div>
  );
  const quotaBad = (q: Record<string, string>) => Object.values(q).some((v) => v.trim() !== '' && !(Number.isInteger(Number(v)) && Number(v) >= 0));
  return (
    <>
      <Hero eyebrow="Tenants" title="Who gets to" accent="use the fleet." lede="Each tenant has its own token, quota, keys and bill.">
        <Button variant="primary" size="lg" onClick={() => setCreate(true)}><Plus size={18} />Create tenant</Button>
      </Hero>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? <EmptyState title="No tenants yet" body="Until a tenant exists, any caller without a tenant token acts as the operator." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Tenant</th><th>Name</th><th>Quota</th><th /></tr></thead><tbody>
            {rows.map((t) => {
              const tid = String(t.id);
              const q = (t.quota ?? {}) as Row;
              return (
                <tr key={tid}>
                  <td className="vm-name">{tid}</td>
                  <td className="fx-muted">{String(t.display_name ?? '—')}</td>
                  <td className="fx-muted">{QUOTA.map(([k, l]) => (q[k] != null ? `${l}: ${String(q[k])}` : null)).filter(Boolean).join(' · ') || 'no limits'}</td>
                  <td><span className="fx-row-actions">
                    <Button size="sm" variant="ghost" aria-label={`Members of ${tid}`} onClick={() => setPeople(tid)}>Members</Button>
                    <Button size="sm" variant="ghost" onClick={() => setEdit({ id: tid, q: Object.fromEntries(QUOTA.map(([k]) => [k, q[k] != null ? String(q[k]) : ''])) })}>Quota</Button>
                    <Button size="sm" variant="ghost" aria-label={`Rotate token for ${tid}`} title="Rotate token" busy={busy === 'rot' + tid} onClick={() => run('rot' + tid, async () => { const r = await api.post<{ token: string }>(`/fleet/tenants/${enc(tid)}/rotate-token`); setSecret({ title: `New token for ${tid}`, token: r.token }); })}><RefreshCw size={14} /></Button>
                    <Button size="sm" variant="ghost" aria-label={`Delete ${tid}`} onClick={() => setDel(tid)}><Trash2 size={14} /></Button>
                  </span></td>
                </tr>
              );
            })}
          </tbody></table></div>
        )}
      </Card>
      <Sheet open={create} title="Create a tenant" onClose={() => setCreate(false)}>
        <Field label="ID" hint="Lowercase. Used in URLs and network names.">{(f) => <input id={f} className="fx-input" value={id} onChange={(e) => setId(e.target.value)} placeholder="acme" autoComplete="off" />}</Field>
        <Field label="Display name">{(f) => <input id={f} className="fx-input" value={name} onChange={(e) => setName(e.target.value)} placeholder="Acme Inc." />}</Field>
        <b>Quota</b>
        {quotaFields(quota, setQuota)}
        {id && tenantIdProblem(id) ? <p className="fx-error" role="alert">{tenantIdProblem(id)}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'create'} disabled={!!tenantIdProblem(id) || quotaBad(quota)} onClick={async () => {
          const ok = await run('create', async () => { const r = await api.post<{ token: string }>('/fleet/tenants', { id, ...(name.trim() ? { display_name: name.trim() } : {}), quota: body(quota) }); setSecret({ title: `Token for ${id}`, token: r.token }); }, `${id} created`, list.refresh);
          if (ok) (setCreate(false), setId(''), setName(''));
        }}>Create tenant</Button><Button onClick={() => setCreate(false)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!edit} title={`Quota for ${String(edit?.id ?? '')}`} onClose={() => setEdit(null)}>
        {edit ? quotaFields(edit.q as Record<string, string>, (q) => setEdit({ ...edit, q })) : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'quota'} disabled={!!edit && quotaBad(edit.q as Record<string, string>)} onClick={async () => { if (edit && (await run('quota', () => api.patch(`/fleet/tenants/${enc(String(edit.id))}`, { quota: body(edit.q as Record<string, string>) }), 'Quota saved', list.refresh))) setEdit(null); }}>Save</Button><Button onClick={() => setEdit(null)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!people} title={`Members of ${people ?? ''}`} onClose={() => setPeople(null)}>
        {people ? <MemberList key={people} owner={people} notify={notify} compact /> : null}
      </Sheet>
      <Sheet open={!!del} title={`Delete ${del ?? ''}`} onClose={() => setDel(null)}>
        <p>Delete tenant <b>{del}</b>? Its token and keys stop working.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => { if (del && (await run('del', () => api.del(`/fleet/tenants/${enc(del)}`), `${del} deleted`, list.refresh))) setDel(null); }}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
      <SecretSheet title={secret?.title ?? ''} intro="This is the only time it is shown. Store it somewhere safe." secret={secret?.token ?? null} notify={notify} onClose={() => setSecret(null)} />
    </>
  );
}
