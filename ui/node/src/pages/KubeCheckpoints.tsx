import { useState } from 'react';
import { Camera, Moon, RotateCcw, Sun, Trash2 } from 'lucide-react';
import { Button, Card, Field, Pill, Sheet, api, ago, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { type KubeCheckpoint, type KubeSnapshot, checkpointLabelNow, checkpointLabelOk, checkpointsOf } from '../lib/kube';
import { rowsOf } from '../lib/generic';

const KIND: Record<KubeCheckpoint['kind'], string> = { sleep: 'asleep', auto: 'automatic', try: 'before a try', manual: '' };

/** A single-node cluster's checkpoints: snapshots of its server machine tagged `kube-<label>`, the same ones `fluxctl kube checkpoint` makes. */
export function KubeCheckpoints({ cluster, workers, notify }: { cluster: string; workers: boolean; notify: Notify }) {
  const vm = usePoll(async () => (await api.get<{ items: { id: string; name: string; status: string }[] }>('/v1/vms')).items?.find((v) => v.name === cluster) ?? null, 8000, [cluster]);
  const id = vm.data?.id;
  const snaps = usePoll(async () => (id ? (rowsOf(await api.get<unknown>(`/v1/vms/${id}/snapshots`), 'snapshots') as unknown as KubeSnapshot[]) : []), 6000, [id]);
  const list = checkpointsOf(snaps.data ?? []);
  const [sheet, setSheet] = useState(false);
  const [label, setLabel] = useState('');
  const [confirm, setConfirm] = useState<{ kind: 'restore' | 'delete'; c: KubeCheckpoint } | null>(null);
  const [busy, setBusy] = useState('');
  const run = async (key: string, fn: () => Promise<unknown>, ok: string) => {
    setBusy(key);
    try {
      await fn();
      notify(ok);
      await snaps.refresh();
      return true;
    } catch (e) {
      notify((e as Error).message, 'bad');
      return false;
    } finally {
      setBusy('');
    }
  };
  if (workers) return <Card title="Checkpoints"><p className="fx-muted">This cluster has worker nodes. A checkpoint is a snapshot of one machine, which cannot put nodes on a private network back consistently. Checkpoints work on a cluster made without workers.</p></Card>;
  const running = vm.data?.status === 'running';
  const asleep = !running && list.some((c) => c.kind === 'sleep');
  const sleepNow = () => run('sleep', async () => {
    await api.post(`/v1/vms/${id}/snapshot`, { tag: 'kube-sleep' });
    await api.post(`/v1/vms/${id}/stop`, {});
    await vm.refresh();
  }, `${cluster} is asleep`);
  const wakeNow = () => run('wake', async () => {
    await api.post(`/v1/vms/${id}/restore`, { tag: 'kube-sleep' });
    await api.del(`/v1/vms/${id}/snapshots/${encodeURIComponent('kube-sleep')}`);
    await vm.refresh();
  }, `${cluster} is awake`);
  const lastTry = list.find((c) => c.kind === 'try');
  const undoTry = () => run('undo', async () => {
    if (!lastTry) return;
    await api.post(`/v1/vms/${id}/restore`, { tag: lastTry.tag });
    await api.del(`/v1/vms/${id}/snapshots/${encodeURIComponent(lastTry.tag)}`);
  }, 'Cluster put back to before the last try');
  const dup = list.some((c) => c.label === label);
  return (
    <Card title="Checkpoints" action={<span className="fx-row-actions">
      {asleep ? <Button size="sm" busy={busy === 'wake'} onClick={wakeNow}><Sun size={14} />Wake</Button> : <Button size="sm" busy={busy === 'sleep'} disabled={!running || list.some((c) => c.kind === 'sleep')} onClick={sleepNow}><Moon size={14} />Sleep</Button>}
      <Button size="sm" variant="primary" disabled={!running} onClick={() => { setLabel(checkpointLabelNow(new Date())); setSheet(true); }}><Camera size={14} />Save checkpoint</Button></span>}>
      {lastTry ? (
        <div className="kx-undo">
          <span>A <code>fluxctl kube try</code> saved the cluster {lastTry.created_at ? ago(lastTry.created_at) : 'earlier'}. Undo puts it back as it was, running pods included.</span>
          <Button size="sm" busy={busy === 'undo'} onClick={undoTry}><RotateCcw size={14} />Undo it</Button>
        </div>
      ) : null}
      {list.length > 1 ? (
        <ol className="kx-timeline" aria-label="Checkpoints, oldest to newest">
          {[...list].reverse().map((c) => (
            <li key={c.tag}>
              <button type="button" className={`kx-tl-dot kx-tl-${c.kind}`} title={`Go back to ${c.label}`} onClick={() => setConfirm({ kind: 'restore', c })}><span className="kx-tl-label">{c.label}</span><span className="kx-tl-when">{c.created_at ? ago(c.created_at) : ''}</span></button>
            </li>
          ))}
        </ol>
      ) : null}
      {list.length === 0 ? <p className="fx-muted">No checkpoints. A checkpoint saves the cluster with its memory in about a second, so Restore brings back every object and running pod exactly as they were. {running ? '' : 'Start the cluster to save one.'}</p> : (
        <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Checkpoint</th><th>Saved</th><th /></tr></thead><tbody>
          {list.map((c) => (
            <tr key={c.tag}>
              <td className="vm-name">{c.label}{KIND[c.kind] ? <Pill tone="info" dot={false} raw>{KIND[c.kind]}</Pill> : null}</td>
              <td className="fx-muted">{c.created_at ? ago(c.created_at) : '—'}</td>
              <td><span className="fx-row-actions">
                <Button size="sm" onClick={() => setConfirm({ kind: 'restore', c })}><RotateCcw size={14} />Restore</Button>
                <Button size="sm" variant="ghost" aria-label={`Delete ${c.label}`} onClick={() => setConfirm({ kind: 'delete', c })}><Trash2 size={14} /></Button>
              </span></td>
            </tr>
          ))}
        </tbody></table></div>
      )}
      <Sheet open={sheet} title="Save checkpoint" onClose={() => setSheet(false)}>
        <Field label="Label">{(i) => <input id={i} className="fx-input" autoFocus value={label} onChange={(e) => setLabel(e.target.value)} />}</Field>
        <p className="fx-muted">{!label ? '' : !checkpointLabelOk(label) ? 'Use 1 to 40 lowercase letters, digits or dashes, not starting or ending with a dash.' : dup ? 'A checkpoint with this label exists. Delete it first or pick another label.' : ''}</p>
        <Button variant="primary" busy={busy === 'save'} disabled={!checkpointLabelOk(label) || dup} onClick={async () => (await run('save', () => api.post(`/v1/vms/${id}/snapshot`, { tag: `kube-${label}` }), `Checkpoint ${label} saved`)) && setSheet(false)}>Save checkpoint</Button>
      </Sheet>
      <Sheet open={!!confirm} title={confirm?.kind === 'restore' ? 'Restore checkpoint' : 'Delete checkpoint'} onClose={() => setConfirm(null)}>
        <p>{confirm?.kind === 'restore' ? <>Restore <b>{confirm.c.label}</b>? The cluster goes back to how it was then; everything since is lost. It takes about 15 seconds. If kubectl was connected, run <code>fluxctl kube up</code> or let <code>kube supervise</code> reopen the port.</> : <>Delete checkpoint <b>{confirm?.c.label}</b>?</>}</p>
        <div className="fx-actions">
          <Button variant={confirm?.kind === 'delete' ? 'danger' : 'primary'} busy={busy === 'confirm'} onClick={async () => { if (!confirm) return; const { kind, c } = confirm; const ok = await run('confirm', () => (kind === 'restore' ? api.post(`/v1/vms/${id}/restore`, { tag: c.tag }) : api.del(`/v1/vms/${id}/snapshots/${encodeURIComponent(c.tag)}`)), kind === 'restore' ? `Restored ${c.label}` : `Checkpoint ${c.label} deleted`); if (ok) setConfirm(null); }}>{confirm?.kind === 'restore' ? 'Restore' : 'Delete'}</Button>
          <Button onClick={() => setConfirm(null)}>Cancel</Button>
        </div>
      </Sheet>
    </Card>
  );
}
