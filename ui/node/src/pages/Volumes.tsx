import { useState } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Sheet, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { type DaemonVolume, volumeGrowProblem, volumeNameProblem } from '../lib/branches';
import { fmtBytes } from '../lib/egress';
import { rowsOf } from '../lib/generic';

const enc = encodeURIComponent;

export function VolumesPage({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/volumes')) as unknown as DaemonVolume[], 8000);
  const { busy, run } = useAct(notify);
  const [create, setCreate] = useState(false);
  const [name, setName] = useState('');
  const [size, setSize] = useState('20');
  const [touched, setTouched] = useState(false);
  const [grow, setGrow] = useState<DaemonVolume | null>(null);
  const [next, setNext] = useState('');
  const [del, setDel] = useState<DaemonVolume | null>(null);
  const rows = list.data ?? [];
  const problem = volumeNameProblem(name, size);
  const growErr = grow ? volumeGrowProblem(grow.size_gib, next, grow.attached_to.length > 0) : '';
  return (
    <>
      <Hero eyebrow="Volumes" title="Disks that outlive" accent="any one machine." lede="Sparse disk files on this host. Attach one when you create a machine; it stays when the machine goes.">
        <Button variant="primary" size="lg" onClick={() => setCreate(true)}><Plus size={18} />Create volume</Button>
      </Hero>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? <EmptyState title="No volumes" body="Create one, then attach it when you create a machine." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>Size</th><th>On disk</th><th>Used by</th><th /></tr></thead><tbody>
            {rows.map((v) => (
              <tr key={v.name}>
                <td className="vm-name">{v.name}</td>
                <td className="fx-muted">{v.size_gib} GiB</td>
                <td className="fx-muted">{fmtBytes(v.allocated_bytes)}</td>
                <td>{v.attached_to.length ? v.attached_to.map((a) => <Pill key={a} tone="info">{a}</Pill>) : <Pill tone="idle">unused</Pill>}</td>
                <td><span className="fx-row-actions">
                  <Button size="sm" variant="ghost" onClick={() => (setGrow(v), setNext(String(v.size_gib + 10)))}>Grow</Button>
                  <Button size="sm" variant="ghost" aria-label={`Delete ${v.name}`} onClick={() => setDel(v)}><Trash2 size={14} /></Button>
                </span></td>
              </tr>
            ))}
          </tbody></table></div>
        )}
      </Card>
      <Sheet open={create} title="Create a volume" onClose={() => setCreate(false)}>
        <Field label="Name">{(id) => <input id={id} className="fx-input" value={name} onChange={(e) => setName(e.target.value)} placeholder="build-cache" autoComplete="off" />}</Field>
        <Field label="Size (GiB)" hint="Sparse: it only uses disk as you write to it.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={size} onChange={(e) => setSize(e.target.value)} />}</Field>
        {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'create'} onClick={async () => {
          setTouched(true);
          if (problem) return;
          if (await run('create', () => api.post('/v1/volumes', { name: name.trim(), size_gib: Number(size) }), `${name.trim()} created`, list.refresh)) (setCreate(false), setName(''), setTouched(false));
        }}>Create volume</Button><Button onClick={() => setCreate(false)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!grow} title={`Grow ${grow?.name ?? ''}`} onClose={() => setGrow(null)}>
        <p className="fx-muted">Now {grow?.size_gib} GiB. Grow the file system inside the guest afterwards (<code>resize2fs</code> or <code>xfs_growfs</code>).</p>
        <Field label="New size (GiB)">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={next} onChange={(e) => setNext(e.target.value)} />}</Field>
        {growErr ? <p className="fx-error" role="alert">{growErr}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'grow'} disabled={!!growErr} onClick={async () => {
          if (grow && (await run('grow', () => api.post(`/v1/volumes/${enc(grow.name)}/resize`, { size_gib: Number(next) }), `${grow.name} is now ${next} GiB`, list.refresh))) setGrow(null);
        }}>Grow</Button><Button onClick={() => setGrow(null)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!del} title={`Delete ${del?.name ?? ''}`} onClose={() => setDel(null)}>
        <p>Delete <b>{del?.name}</b> and the data on it? This cannot be undone.{del?.attached_to.length ? ` It is used by ${del.attached_to.join(', ')}; the daemon refuses until that is gone.` : ''}</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => {
          if (del && (await run('del', () => api.del(`/v1/volumes/${enc(del.name)}`), `${del.name} deleted`, list.refresh))) setDel(null);
        }}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
