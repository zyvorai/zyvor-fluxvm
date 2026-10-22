import { useState } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Sheet, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { type Volume, resizeProblem, volumeProblem } from '../lib/ops';

const enc = encodeURIComponent;

export function VolumesPage({ role, notify }: Ctx) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/volumes'), 'items') as unknown as Volume[], 10000);
  const { busy, run } = useAct(notify);
  const [create, setCreate] = useState(false);
  const [name, setName] = useState('');
  const [size, setSize] = useState('20');
  const [touched, setTouched] = useState(false);
  const [grow, setGrow] = useState<Volume | null>(null);
  const [next, setNext] = useState('');
  const [del, setDel] = useState<Volume | null>(null);
  const rows = list.data ?? [];
  const problem = volumeProblem(name, size);
  const growProblem = grow ? resizeProblem(grow.size_gib, next, !!grow.attached_to) : '';
  return (
    <>
      <Hero eyebrow="Volumes" title="Disks that outlive" accent="the machine." lede="Create a volume on a host, attach it when you launch a VM, and keep it after the VM is gone.">
        <Button variant="primary" size="lg" onClick={() => setCreate(true)}><Plus size={18} />Create volume</Button>
      </Hero>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? <EmptyState title="No volumes" body="Create one, then pick it when you launch a VM." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th>{role === 'operator' ? <th>Tenant</th> : null}<th>Host</th><th>Size</th><th>State</th><th /></tr></thead><tbody>
            {rows.map((v) => (
              <tr key={v.id}>
                <td className="vm-name">{v.name}</td>
                {role === 'operator' ? <td className="fx-muted">{v.tenant || 'operator'}</td> : null}
                <td className="fx-muted">{v.node}</td>
                <td className="fx-muted">{v.size_gib} GiB</td>
                <td>{v.creating ? <Pill tone="warn">creating</Pill> : v.attached_to ? <Pill tone="info">attached</Pill> : <Pill tone="idle">detached</Pill>}</td>
                <td><span className="fx-row-actions">
                  <Button size="sm" variant="ghost" disabled={!!v.creating} onClick={() => (setGrow(v), setNext(String(v.size_gib + 10)))}>Grow</Button>
                  <Button size="sm" variant="ghost" aria-label={`Delete ${v.name}`} onClick={() => setDel(v)}><Trash2 size={14} /></Button>
                </span></td>
              </tr>
            ))}
          </tbody></table></div>
        )}
      </Card>
      <Sheet open={create} title="Create a volume" onClose={() => setCreate(false)}>
        <Field label="Name">{(id) => <input id={id} className="fx-input" value={name} onChange={(e) => setName(e.target.value)} placeholder="build-cache" autoComplete="off" />}</Field>
        <Field label="Size (GiB)" hint="It is placed on a host you can use, preferring one you lease.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={size} onChange={(e) => setSize(e.target.value)} />}</Field>
        {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'create'} onClick={async () => {
          setTouched(true);
          if (problem) return;
          if (await run('create', () => api.post('/fleet/volumes', { name: name.trim(), size_gib: Number(size) }), `${name.trim()} created`, list.refresh)) (setCreate(false), setName(''), setTouched(false));
        }}>Create volume</Button><Button onClick={() => setCreate(false)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!grow} title={`Grow ${grow?.name ?? ''}`} onClose={() => setGrow(null)}>
        <p className="fx-muted">Now {grow?.size_gib} GiB. The file system inside the guest is not grown for you: run <code>resize2fs</code> or <code>xfs_growfs</code> after attaching it again.</p>
        <Field label="New size (GiB)">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={next} onChange={(e) => setNext(e.target.value)} />}</Field>
        {growProblem ? <p className="fx-error" role="alert">{growProblem}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'grow'} disabled={!!growProblem} onClick={async () => {
          if (grow && (await run('grow', () => api.post(`/fleet/volumes/${enc(grow.name)}/resize`, { size_gib: Number(next) }), `${grow.name} is now ${next} GiB`, list.refresh))) setGrow(null);
        }}>Grow</Button><Button onClick={() => setGrow(null)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!del} title={`Delete ${del?.name ?? ''}`} onClose={() => setDel(null)}>
        <p>Delete <b>{del?.name}</b> and the data on it? This cannot be undone.{del?.attached_to ? ' It is attached to a VM; the delete is refused until you detach it.' : ''}</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => {
          if (del && (await run('del', () => api.del(`/fleet/volumes/${enc(del.name)}`), `${del.name} deleted`, list.refresh))) setDel(null);
        }}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
