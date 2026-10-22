import { useState } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Sheet, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { joinedList } from '../lib/fleet';

const enc = encodeURIComponent;

export function NetworksPage({ role, notify }: Ctx) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/overlays'), 'items'), 8000);
  const nodes = usePoll(async () => (role === 'operator' ? rowsOf(await api.get<unknown>('/fleet/nodes').catch(() => ({ items: [] })), 'items') : []), 60000, [role]);
  const { busy, run } = useAct(notify);
  const [create, setCreate] = useState(false);
  const [name, setName] = useState('');
  const [per, setPer] = useState('30');
  const [mtu, setMtu] = useState('1400');
  const [picked, setPicked] = useState<string[]>([]);
  const [del, setDel] = useState<string | null>(null);
  const rows = list.data ?? [];
  const problem = !/^[a-z0-9][a-z0-9-]{0,30}$/.test(name) ? 'Name: lowercase letters, digits and dashes.' : !(Number(per) >= 2 && Number(per) <= 126) ? 'VMs per host: 2 to 126.' : !(Number(mtu) >= 1280 && Number(mtu) <= 9000) ? 'MTU: 1280 to 9000.' : '';
  const [touched, setTouched] = useState(false);
  return (
    <>
      <Hero eyebrow="Networks" title="One private switch," accent="across hosts." lede="VMs on a private network reach each other by IP wherever they run. Traffic between hosts is encrypted.">
        <Button variant="primary" size="lg" onClick={() => setCreate(true)}><Plus size={18} />Create network</Button>
      </Hero>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? <EmptyState title="No private networks" body="Create one, then pick it when you launch a VM." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>Subnet</th><th>Hosts</th><th>MTU</th><th /></tr></thead><tbody>
            {rows.map((o) => {
              const n = String(o.name);
              const members = o.members && typeof o.members === 'object' ? Object.keys(o.members as object) : [];
              return (
                <tr key={n}>
                  <td className="vm-name">{n}</td>
                  <td className="fx-mono fx-muted">{String(o.subnet ?? '—')}</td>
                  <td className="fx-muted">{joinedList(members)}</td>
                  <td className="fx-muted">{String(o.mtu ?? '—')}</td>
                  <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Delete ${n}`} onClick={() => setDel(n)}><Trash2 size={14} /></Button></span></td>
                </tr>
              );
            })}
          </tbody></table></div>
        )}
      </Card>
      <Sheet open={create} title="Create a private network" onClose={() => setCreate(false)}>
        <Field label="Name">{(id) => <input id={id} className="fx-input" value={name} onChange={(e) => setName(e.target.value)} />}</Field>
        <div className="fx-grid cols-2">
          <Field label="VMs per host">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={per} onChange={(e) => setPer(e.target.value)} />}</Field>
          <Field label="MTU" hint="Raise only on a jumbo-frame network.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={mtu} onChange={(e) => setMtu(e.target.value)} />}</Field>
        </div>
        {(nodes.data ?? []).length ? <div><b>Hosts</b>{(nodes.data ?? []).map((n) => { const h = String(n.name); return <label key={h} className="fx-check"><input type="checkbox" checked={picked.includes(h)} onChange={(e) => setPicked(e.target.checked ? [...picked, h] : picked.filter((x) => x !== h))} /> {h}</label>; })}</div> : null}
        {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'create'} onClick={async () => {
          setTouched(true);
          if (problem) return;
          if (await run('create', () => api.post('/fleet/overlays', { name, hosts_per_node: Number(per), mtu: Number(mtu), ...(picked.length ? { nodes: picked } : {}) }), `${name} created`, list.refresh)) (setCreate(false), setName(''), setTouched(false));
        }}>Create network</Button><Button onClick={() => setCreate(false)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!del} title={`Delete ${del ?? ''}`} onClose={() => setDel(null)}>
        <p>Delete <b>{del}</b>? VMs on it lose the connection.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => { if (del && (await run('del', () => api.del(`/fleet/overlays/${enc(del)}`), `${del} deleted`, list.refresh))) setDel(null); }}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
