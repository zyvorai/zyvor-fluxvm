import { useState } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Sheet, Skeleton, ago, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { SecretSheet } from './Secret';

export function KeysPage({ notify }: Ctx) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/tenants/me/keys'), 'items'), 15000);
  const { busy, run } = useAct(notify);
  const [create, setCreate] = useState(false);
  const [name, setName] = useState('');
  const [days, setDays] = useState('90');
  const [ro, setRo] = useState(false);
  const [del, setDel] = useState<string | null>(null);
  const [secret, setSecret] = useState<string | null>(null);
  const rows = list.data ?? [];
  const bad = !name.trim() || name.trim().length > 64;
  return (
    <>
      <Hero eyebrow="API keys" title="Named keys for" accent="your tools." lede="Give CI and scripts their own key, so you can see what each one did and revoke it alone.">
        <Button variant="primary" size="lg" onClick={() => setCreate(true)}><Plus size={18} />Create key</Button>
      </Hero>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={100} /> : rows.length === 0 ? <EmptyState title="No API keys" body="Create one for each tool that talks to FluxVM." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>Access</th><th>Expires</th><th>Created</th><th /></tr></thead><tbody>
            {rows.map((k) => {
              const n = String(k.name);
              return (
                <tr key={n}>
                  <td className="vm-name">{n}</td>
                  <td>{k.read_only ? <Pill tone="info">read-only</Pill> : <Pill tone="idle">full</Pill>}</td>
                  <td className="fx-muted">{typeof k.expires_at === 'string' ? ago(k.expires_at) : 'never'}</td>
                  <td className="fx-muted">{typeof k.created_at === 'string' ? ago(k.created_at) : '—'}</td>
                  <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Revoke ${n}`} onClick={() => setDel(n)}><Trash2 size={14} /></Button></span></td>
                </tr>
              );
            })}
          </tbody></table></div>
        )}
      </Card>
      <Sheet open={create} title="Create an API key" onClose={() => setCreate(false)}>
        <Field label="Name" hint="Who or what uses it. It shows in the activity log.">{(id) => <input id={id} className="fx-input" value={name} onChange={(e) => setName(e.target.value)} placeholder="ci-github" maxLength={64} autoComplete="off" />}</Field>
        <Field label="Expires">{(id) => <select id={id} className="fx-input" value={days} onChange={(e) => setDays(e.target.value)}><option value="30">In 30 days</option><option value="90">In 90 days</option><option value="365">In a year</option><option value="">Never</option></select>}</Field>
        <label className="fx-check"><input type="checkbox" checked={ro} onChange={(e) => setRo(e.target.checked)} /> Read-only: it can look, not change anything.</label>
        <div className="fx-actions"><Button variant="primary" busy={busy === 'create'} disabled={bad} onClick={async () => {
          const ok = await run('create', async () => { const r = await api.post<{ token: string }>('/fleet/tenants/me/keys', { name: name.trim(), read_only: ro, ...(days ? { expires_in_days: Number(days) } : {}) }); setSecret(r.token); }, undefined, list.refresh);
          if (ok) (setCreate(false), setName(''));
        }}>Create key</Button><Button onClick={() => setCreate(false)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!del} title={`Revoke ${del ?? ''}`} onClose={() => setDel(null)}>
        <p>Revoke <b>{del}</b>? Anything using it stops working immediately.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => { if (del && (await run('del', () => api.del(`/fleet/tenants/me/keys/${encodeURIComponent(del)}`), `${del} revoked`, list.refresh))) setDel(null); }}>Revoke</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
      <SecretSheet title="Your new key" intro="This is the only time it is shown. Store it somewhere safe." secret={secret} notify={notify} onClose={() => setSecret(null)} />
    </>
  );
}
