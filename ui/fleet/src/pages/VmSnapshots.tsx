import { useState } from 'react';
import { Button, Card, EmptyState, Field, Pill, Skeleton, ago, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { type FleetVm, vmKey } from '../lib/fleet';
import { type Snapshot, cloneProblem, fmtBytes, snapshotProblem, suggestCloneName } from '../lib/vmtools';

const enc = encodeURIComponent;

/** A VM's snapshots (take, restore, delete) and clones (a new VM on the same Mac from a snapshot or from the stopped VM's disk). */
export function VmSnapshots({ vm, stopped, macos, taken, notify, onCloned }: { vm: FleetVm; stopped: boolean; macos: boolean; taken: string[]; notify: Notify; onCloned: () => void }) {
  const base = `/fleet/vms/${enc(vm.node)}/${enc(vm.id)}`;
  const list = usePoll(async () => {
    const r = await api.get<{ items?: Snapshot[]; limit?: number }>(`${base}/snapshots`);
    return { items: r.items ?? [], limit: r.limit };
  }, 30000, [base]);
  const { busy, run } = useAct(notify);
  const items = list.data?.items ?? [];
  const limit = list.data?.limit;
  const names = items.map((s) => s.name);
  const [take, setTake] = useState<{ name: string; memory: boolean } | null>(null);
  const [touched, setTouched] = useState(false);
  const [confirm, setConfirm] = useState<{ kind: 'restore' | 'delete'; snap: Snapshot } | null>(null);
  const [clone, setClone] = useState<{ name: string; source: string } | null>(null);
  const takeProblem = take ? snapshotProblem(take.name, names, limit) : '';
  const cloneIssue = clone ? cloneProblem(clone.name, taken, clone.source, stopped) : '';
  const full = limit !== undefined && items.length >= limit;

  return (
    <div>
      <div className="fx-actions" style={{ marginBottom: 12 }}>
        <Button variant="primary" size="sm" disabled={!list.data || full} title={full ? snapshotProblem('x', names, limit) : undefined} onClick={() => (setTake({ name: '', memory: false }), setTouched(false))}>Take snapshot</Button>
        <Button size="sm" disabled={!stopped && !items.length} title={!stopped && !items.length ? 'Stop the VM or take a snapshot first.' : undefined} onClick={() => setClone({ name: suggestCloneName(vm.name, taken), source: stopped ? '' : items[0]?.name ?? '' })}>Clone VM…</Button>
      </div>
      {take ? (
        <Card title="Take a snapshot">
          <Field label="Name">{(id) => <input id={id} className="fx-input" value={take.name} autoComplete="off" spellCheck={false} onChange={(e) => setTake({ ...take, name: e.target.value })} />}</Field>
          <label className="fx-check"><input type="checkbox" checked={take.memory} disabled={!(!stopped)} onChange={(e) => setTake({ ...take, memory: e.target.checked })} /> Include memory <span className="fx-muted">(needs a running or paused VM; restoring resumes where it was)</span></label>
          {touched && takeProblem ? <p className="fx-error" role="alert">{takeProblem}</p> : null}
          <div className="fx-actions"><Button variant="primary" busy={busy === 'take'} onClick={async () => {
            setTouched(true);
            if (takeProblem) return;
            if (await run('take', () => api.post(`${base}/snapshots`, { name: take.name, include_memory: take.memory }), `Snapshot ${take.name} taken`, list.refresh)) setTake(null);
          }}>Take snapshot</Button><Button onClick={() => setTake(null)}>Cancel</Button></div>
        </Card>
      ) : null}
      {clone ? (
        <Card title={`New VM from ${vm.name}`}>
          <p className="fx-muted">It runs on {vm.node} with the same size, settings and networks, and counts against the quota.</p>
          <Field label="Name of the new VM">{(id) => <input id={id} className="fx-input" value={clone.name} autoComplete="off" spellCheck={false} onChange={(e) => setClone({ ...clone, name: e.target.value })} />}</Field>
          <Field label="Copy from" hint="A snapshot can be cloned while the VM runs. Its memory is not copied: the new VM starts from the disk.">{(id) => (
            <select id={id} className="fx-input" value={clone.source} onChange={(e) => setClone({ ...clone, source: e.target.value })}>
              <option value="" disabled={!stopped}>{stopped ? 'Its disk, as it is now' : 'Its disk now (stop the VM first)'}</option>
              {items.map((s) => <option key={s.name} value={s.name}>Snapshot {s.name}{s.created_at ? ` (${ago(s.created_at)})` : ''}</option>)}
            </select>
          )}</Field>
          {macos ? <p className="fx-muted">Apple&rsquo;s licence allows two macOS VMs per Mac, so this works only if {vm.node} has a free slot.</p> : null}
          {cloneIssue ? <p className="fx-error" role="alert">{cloneIssue}</p> : null}
          <div className="fx-actions"><Button variant="primary" busy={busy === 'clone'} disabled={!!cloneIssue} onClick={async () => {
            if (await run('clone', () => api.post(`${base}/clone`, { name: clone.name, snapshot: clone.source || undefined }), `${clone.name} created from ${clone.source ? `snapshot ${clone.source}` : vm.name}`, async () => { onCloned(); await list.refresh(); })) setClone(null);
          }}>Create VM</Button><Button onClick={() => setClone(null)}>Cancel</Button></div>
        </Card>
      ) : null}
      {confirm ? (
        <Card title={confirm.kind === 'restore' ? `Restore ${confirm.snap.name}?` : `Delete ${confirm.snap.name}?`}>
          <p>{confirm.kind === 'restore'
            ? `${vm.name} is stopped, its disk replaced by this snapshot${confirm.snap.memory ? ' and its memory resumed' : ''}, then started. Changes since the snapshot are lost.`
            : 'The snapshot is removed and cannot be recovered.'}</p>
          <div className="fx-actions"><Button variant={confirm.kind === 'delete' ? 'danger' : 'primary'} busy={busy === confirm.kind} onClick={async () => {
            const s = confirm.snap;
            const ok = confirm.kind === 'restore'
              ? await run('restore', () => api.post(`${base}/snapshots/${enc(s.name)}/restore`), `${vm.name} restored to ${s.name}`, async () => { onCloned(); await list.refresh(); })
              : await run('delete', () => api.del(`${base}/snapshots/${enc(s.name)}`), `Snapshot ${s.name} deleted`, list.refresh);
            if (ok) setConfirm(null);
          }}>{confirm.kind === 'restore' ? 'Restore' : 'Delete'}</Button><Button onClick={() => setConfirm(null)}>Cancel</Button></div>
        </Card>
      ) : null}
      {list.loading && !list.data ? <Skeleton h={80} /> : list.error && !list.data ? <p className="fx-error" role="alert">The snapshots could not be loaded: {list.error.message}</p> : items.length === 0 ? (
        <EmptyState title="No snapshots yet" body="A snapshot keeps this VM's disk as it is now, and its memory too if you like, so you can come back to this moment or start new VMs from it." />
      ) : (
        <div className="fx-snaps">
          {items.map((s) => (
            <div className="fx-snap" key={vmKey({ node: vm.node, id: vm.id }) + s.name}>
              <div className="fx-snap-head"><b>{s.name}</b><Pill tone={s.memory ? 'info' : 'idle'} dot={false}>{s.memory ? 'Disk + memory' : 'Disk'}</Pill></div>
              <div className="fx-muted">{s.created_at ? ago(s.created_at) : '—'} · {fmtBytes(s.size_bytes)}</div>
              <div className="fx-actions">
                <Button size="sm" onClick={() => setConfirm({ kind: 'restore', snap: s })}>Restore…</Button>
                <Button size="sm" onClick={() => setClone({ name: suggestCloneName(vm.name, taken), source: s.name })}>New VM from this…</Button>
                <Button size="sm" variant="danger" aria-label={`Delete snapshot ${s.name}`} onClick={() => setConfirm({ kind: 'delete', snap: s })}>Delete…</Button>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
