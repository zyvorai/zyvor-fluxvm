import { useState } from 'react';
import { Camera, HardDrive, Plus, RotateCcw, Send, Trash2 } from 'lucide-react';
import { Button, Card, Field, KV, Meter, Pill, Segmented, Sheet, api, ago, fmtMiB, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { EgressEditor } from './EgressEditor';
import { GenericTable, rowsOf } from '../lib/generic';

type Tone = 'ok' | 'warn' | 'bad' | 'info' | 'idle';
const errMsg = (e: unknown) => (e as Error).message ?? String(e);

/** Small helper: run an API call with busy state + toast + refresh. */
function useAction(notify: Notify, refresh: () => Promise<unknown> | void) {
  const [busy, setBusy] = useState('');
  const run = async (key: string, fn: () => Promise<unknown>, ok: string) => {
    setBusy(key);
    try {
      await fn();
      notify(ok);
      await refresh();
      return true;
    } catch (e) {
      notify(errMsg(e), 'bad');
      return false;
    } finally {
      setBusy('');
    }
  };
  return { busy, run };
}

/* ------------------------------------------------------------------ Network */
export function NetworkTab({ id, notify }: { id: string; notify: Notify }) {
  const [view, setView] = useState<'flows' | 'drops' | 'stats'>('flows');
  const data = usePoll(async () => {
    const path = view === 'drops' ? 'drops?limit=100' : view;
    return api.get<unknown>(`/v1/vms/${id}/network/${path}`).catch((e: Error) => ({ __error: e.message }));
  }, 5000, [id, view]);
  const d = data.data as { __error?: string } | null;
  return (
    <div className="fx-grid">
      <EgressEditor vmId={id} notify={notify} />
      <Card title="Traffic" action={<Segmented label="Traffic view" value={view} onChange={setView} options={[{ value: 'flows', label: 'Flows' }, { value: 'drops', label: 'Drops' }, { value: 'stats', label: 'Counters' }]} />}>
        {d?.__error ? <p className="fx-muted">Not available for this machine: {d.__error}</p> : view === 'stats' ? <StatsKV data={data.data} /> : <GenericTable rows={rowsOf(data.data)} empty={view === 'drops' ? 'No dropped packets. The datapath has nothing to report.' : 'No flows recorded yet.'} />}
      </Card>
    </div>
  );
}

function StatsKV({ data }: { data: unknown }) {
  const o = (data && typeof data === 'object' ? data : {}) as Record<string, unknown>;
  const rows = Object.entries(o).filter(([, v]) => typeof v !== 'object').map(([k, v]) => [k.replace(/_/g, ' '), typeof v === 'number' ? v.toLocaleString() : String(v)] as [string, string]);
  return rows.length ? <KV rows={rows} /> : <p className="fx-muted">No counters yet.</p>;
}

/* ------------------------------------------------------------------ Storage */
interface Disk {
  name: string;
  size_gib?: number;
  size_bytes?: number;
  path?: string;
  format?: string;
  [k: string]: unknown;
}
export function StorageTab({ id, notify }: { id: string; notify: Notify }) {
  const disks = usePoll(async () => rowsOf(await api.get<unknown>(`/v1/vms/${id}/disks`), 'disks') as Disk[], 6000, [id]);
  const { busy, run } = useAction(notify, () => disks.refresh());
  const [add, setAdd] = useState(false);
  const [name, setName] = useState('');
  const [size, setSize] = useState('10');
  const [resize, setResize] = useState<Disk | null>(null);
  const [newSize, setNewSize] = useState('');
  const [del, setDel] = useState<Disk | null>(null);
  const list = disks.data ?? [];
  const sizeOf = (d: Disk) => (d.size_gib != null ? `${d.size_gib} GiB` : d.size_bytes != null ? `${(d.size_bytes / 2 ** 30).toFixed(1)} GiB` : '—');
  return (
    <>
      <Card title="Disks" action={<Button size="sm" variant="primary" onClick={() => { setName(''); setAdd(true); }}><Plus size={14} />Add disk</Button>}>
        {list.length === 0 ? <p className="fx-muted">{disks.loading ? 'Loading…' : 'No disks reported.'}</p> : (
          <div className="fx-table-wrap">
            <table className="fx-table">
              <thead><tr><th>Name</th><th>Size</th><th>Path</th><th /></tr></thead>
              <tbody>
                {list.map((d) => (
                  <tr key={d.name}>
                    <td><HardDrive size={14} style={{ verticalAlign: -2, marginRight: 8 }} />{d.name}{d.name === 'root' ? <Pill tone="info" dot={false} raw>boot</Pill> : null}</td>
                    <td>{sizeOf(d)}</td>
                    <td className="fx-mono fx-muted">{d.path ?? '—'}</td>
                    <td><span className="fx-row-actions"><Button size="sm" onClick={() => { setResize(d); setNewSize(String(d.size_gib ?? '')); }}>Resize</Button>{d.name !== 'root' ? <Button size="sm" variant="ghost" aria-label={`Delete ${d.name}`} onClick={() => setDel(d)}><Trash2 size={14} /></Button> : null}</span></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <Sheet open={add} title="Add disk" onClose={() => setAdd(false)}>
        <Field label="Name">{(i) => <input id={i} className="fx-input" autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="data" />}</Field>
        <Field label="Size (GiB)">{(i) => <input id={i} className="fx-input" type="number" min={1} value={size} onChange={(e) => setSize(e.target.value)} />}</Field>
        <Button variant="primary" busy={busy === 'add'} disabled={!name.trim() || !(Number(size) >= 1)} onClick={async () => (await run('add', () => api.post(`/v1/vms/${id}/disks`, { name: name.trim(), size_gib: Number(size) }), `Disk ${name} added`)) && setAdd(false)}>Add disk</Button>
      </Sheet>
      <Sheet open={!!resize} title={`Resize ${resize?.name ?? ''}`} onClose={() => setResize(null)}>
        <p className="fx-muted">Disks can only grow. The guest still has to extend its partition and filesystem.</p>
        <Field label="New size (GiB)">{(i) => <input id={i} className="fx-input" type="number" min={1} value={newSize} onChange={(e) => setNewSize(e.target.value)} />}</Field>
        <Button variant="primary" busy={busy === 'resize'} disabled={!(Number(newSize) >= 1)} onClick={async () => resize && (await run('resize', () => api.patch(`/v1/vms/${id}/disks/${resize.name}`, { size_gib: Number(newSize) }), `${resize.name} resized`)) && setResize(null)}>Resize</Button>
      </Sheet>
      <Sheet open={!!del} title="Delete disk" onClose={() => setDel(null)}>
        <p>Delete <b>{del?.name}</b>? The disk file is removed.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => del && (await run('del', () => api.del(`/v1/vms/${id}/disks/${del.name}`), `${del.name} deleted`)) && setDel(null)}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}

/* ------------------------------------------------------------------ Protect */
interface Snap { tag: string; created_at?: string | null; size_bytes?: number; memory?: boolean; has_memory?: boolean }
export function ProtectTab({ id, name, running, notify }: { id: string; name: string; running: boolean; notify: Notify }) {
  const snaps = usePoll(async () => rowsOf(await api.get<unknown>(`/v1/vms/${id}/snapshots`), 'snapshots') as unknown as Snap[], 6000, [id]);
  const backups = usePoll(async () => rowsOf(await api.get<unknown>('/v1/backups'), 'backups').filter((b) => String(b.name ?? '').startsWith(name)), 10000, [id, name]);
  const branches = usePoll(async () => rowsOf(await api.get<unknown>(`/v1/vms/${id}/branches`).catch(() => ({})), 'branches'), 10000, [id]);
  const { busy, run } = useAction(notify, async () => { await Promise.all([snaps.refresh(), backups.refresh(), branches.refresh()]); });
  const [tag, setTag] = useState('');
  const [sheet, setSheet] = useState<'snap' | 'branch' | 'backup' | null>(null);
  const [confirm, setConfirm] = useState<{ kind: 'restore' | 'delete'; tag: string } | null>(null);
  const [branchName, setBranchName] = useState('');
  const stamp = () => new Date().toISOString().slice(0, 16).replace(/[-:T]/g, '');
  return (
    <div className="fx-grid">
      <Card title="Snapshots" action={<Button size="sm" variant="primary" onClick={() => { setTag(`snap-${stamp()}`); setSheet('snap'); }}><Camera size={14} />Take snapshot</Button>}>
        {(snaps.data ?? []).length === 0 ? <p className="fx-muted">No snapshots yet. A snapshot of a running machine includes its memory, so restoring resumes exactly where it was.</p> : (
          <div className="fx-table-wrap">
            <table className="fx-table">
              <thead><tr><th>Tag</th><th>Taken</th><th>Size</th><th /></tr></thead>
              <tbody>
                {(snaps.data ?? []).map((s) => (
                  <tr key={s.tag}>
                    <td>{s.tag}{s.has_memory || s.memory ? <Pill tone="info" dot={false} raw>with memory</Pill> : null}</td>
                    <td className="fx-muted">{s.created_at ? ago(s.created_at) : '—'}</td>
                    <td className="fx-muted">{s.size_bytes != null ? fmtMiB(Math.round(s.size_bytes / 1048576)) : '—'}</td>
                    <td><span className="fx-row-actions"><Button size="sm" onClick={() => setConfirm({ kind: 'restore', tag: s.tag })}><RotateCcw size={14} />Restore</Button><Button size="sm" variant="ghost" aria-label={`Delete ${s.tag}`} onClick={() => setConfirm({ kind: 'delete', tag: s.tag })}><Trash2 size={14} /></Button></span></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <Card title="Backups" action={<Button size="sm" onClick={() => setSheet('backup')}>Back up now</Button>}>
        <GenericTable rows={backups.data ?? []} empty="No backups for this machine." />
      </Card>
      <Card title="Branches" action={<Button size="sm" disabled={!running} onClick={() => { setBranchName(''); setSheet('branch'); }}>New branch</Button>}>
        <GenericTable rows={branches.data ?? []} empty={running ? 'No branches. A branch captures this VM as a named, restartable fork point.' : 'Branches are created from a running machine.'} />
      </Card>

      <Sheet open={sheet === 'snap'} title="Take snapshot" onClose={() => setSheet(null)}>
        <Field label="Tag">{(i) => <input id={i} className="fx-input" autoFocus value={tag} onChange={(e) => setTag(e.target.value)} />}</Field>
        <Button variant="primary" busy={busy === 'snap'} disabled={!tag.trim()} onClick={async () => (await run('snap', () => api.post(`/v1/vms/${id}/snapshot`, { tag: tag.trim() }), 'Snapshot taken')) && setSheet(null)}>Take snapshot</Button>
      </Sheet>
      <Sheet open={sheet === 'backup'} title="Back up" onClose={() => setSheet(null)}>
        <p className="fx-muted">Writes a standalone copy of this machine's disk under the node's backup directory. The guest's filesystems are quiesced first when its agent answers.</p>
        <Button variant="primary" busy={busy === 'backup'} onClick={async () => (await run('backup', () => api.post(`/v1/vms/${id}/backup`, { quiesce: 'auto' }), 'Backup started')) && setSheet(null)}>Back up now</Button>
      </Sheet>
      <Sheet open={sheet === 'branch'} title="New branch" onClose={() => setSheet(null)}>
        <Field label="Branch name">{(i) => <input id={i} className="fx-input" autoFocus value={branchName} onChange={(e) => setBranchName(e.target.value)} />}</Field>
        <Button variant="primary" busy={busy === 'branch'} disabled={!branchName.trim()} onClick={async () => (await run('branch', () => api.post(`/v1/vms/${id}/branches`, { name: branchName.trim() }), 'Branch created')) && setSheet(null)}><Send size={14} />Create branch</Button>
      </Sheet>
      <Sheet open={!!confirm} title={confirm?.kind === 'restore' ? 'Restore snapshot' : 'Delete snapshot'} onClose={() => setConfirm(null)}>
        <p>{confirm?.kind === 'restore' ? <>Restore <b>{confirm.tag}</b>? Changes since the snapshot are lost.</> : <>Delete snapshot <b>{confirm?.tag}</b>?</>}</p>
        <div className="fx-actions">
          <Button variant={confirm?.kind === 'delete' ? 'danger' : 'primary'} busy={busy === 'confirm'} onClick={async () => { if (!confirm) return; const ok = await run('confirm', () => (confirm.kind === 'restore' ? api.post(`/v1/vms/${id}/restore`, { tag: confirm.tag }) : api.del(`/v1/vms/${id}/snapshots/${encodeURIComponent(confirm.tag)}`)), confirm.kind === 'restore' ? 'Restored' : 'Snapshot deleted'); if (ok) setConfirm(null); }}>{confirm?.kind === 'restore' ? 'Restore' : 'Delete'}</Button>
          <Button onClick={() => setConfirm(null)}>Cancel</Button>
        </div>
      </Sheet>
    </div>
  );
}

/* ---------------------------------------------------------------- Migration */
interface Mig {
  phase?: string;
  status?: string;
  ram_transferred?: number | null;
  ram_total?: number | null;
  total_time_ms?: number | null;
  downtime_ms?: number | null;
  error?: string | null;
}
const phaseTone = (p?: string): Tone => (p === 'completed' ? 'ok' : p === 'failed' ? 'bad' : p === 'cancelled' ? 'warn' : p === 'active' || p === 'postcopy-active' || p === 'setup' ? 'info' : 'idle');
export const migrationPct = (m?: Mig | null): number => (m?.ram_total ? Math.min(100, ((m.ram_transferred ?? 0) / m.ram_total) * 100) : 0);

export function MigrationTab({ id, running, notify }: { id: string; running: boolean; notify: Notify }) {
  const st = usePoll(async () => api.get<Mig>(`/v1/vms/${id}/migration/status`).catch(() => null), 2000, [id]);
  const { busy, run } = useAction(notify, () => st.refresh());
  const [dest, setDest] = useState('');
  const [mode, setMode] = useState<'pre-copy' | 'post-copy'>('pre-copy');
  const [bw, setBw] = useState('');
  const [down, setDown] = useState('');
  const m = st.data;
  const active = m?.phase === 'active' || m?.phase === 'setup' || m?.phase === 'postcopy-active';
  const pct = migrationPct(m);
  return (
    <div className="fx-grid cols-2">
      <Card title="Live migration" action={m?.phase && m.phase !== 'none' ? <Pill tone={phaseTone(m.phase)}>{m.phase}</Pill> : null}>
        {m && m.phase && m.phase !== 'none' ? (
          <>
            <div className="fx-stat-num">{pct.toFixed(0)}%</div>
            <Meter value={pct} label="Migration progress" tone={phaseTone(m.phase) === 'idle' ? 'info' : (phaseTone(m.phase) as 'ok')} />
            <KV rows={[['Transferred', m.ram_transferred != null ? fmtMiB(Math.round(m.ram_transferred / 1048576)) : '—'], ['Total', m.ram_total != null ? fmtMiB(Math.round(m.ram_total / 1048576)) : '—'], ['Elapsed', m.total_time_ms != null ? `${(m.total_time_ms / 1000).toFixed(1)} s` : '—'], ['Downtime', m.downtime_ms != null ? `${m.downtime_ms} ms` : '—']]} />
            {m.error ? <div className="fx-error" role="alert" style={{ marginTop: 12 }}>{m.error}</div> : null}
            {active ? (
              <div className="fx-actions" style={{ marginTop: 16 }}>
                {m.phase === 'active' ? <Button onClick={() => run('post', () => api.post(`/v1/vms/${id}/migration/postcopy`), 'Switched to post-copy')} busy={busy === 'post'}>Switch to post-copy</Button> : null}
                <Button variant="danger" busy={busy === 'cancel'} onClick={() => run('cancel', () => api.post(`/v1/vms/${id}/migration/cancel`), 'Migration cancelled')}>Cancel</Button>
              </div>
            ) : null}
          </>
        ) : <p className="fx-muted">No migration in progress.</p>}
      </Card>
      <Card title="Move to another node">
        {!running ? <p className="fx-muted">Live migration needs a running machine.</p> : (
          <>
            <Field label="Destination" hint="The receiver address on the target node, e.g. tcp:10.0.0.12:4444">{(i) => <input id={i} className="fx-input" value={dest} onChange={(e) => setDest(e.target.value)} placeholder="tcp:host:port" />}</Field>
            <Field label="Mode">{() => <Segmented label="Migration mode" value={mode} onChange={setMode} options={[{ value: 'pre-copy', label: 'Pre-copy' }, { value: 'post-copy', label: 'Post-copy' }]} />}</Field>
            <Field label="Bandwidth limit (Mbps)" hint="Optional.">{(i) => <input id={i} className="fx-input" inputMode="numeric" value={bw} onChange={(e) => setBw(e.target.value.replace(/\D/g, ''))} />}</Field>
            <Field label="Max downtime (ms)" hint="Optional.">{(i) => <input id={i} className="fx-input" inputMode="numeric" value={down} onChange={(e) => setDown(e.target.value.replace(/\D/g, ''))} />}</Field>
            <Button variant="primary" busy={busy === 'start'} disabled={!dest.trim() || active} onClick={() => run('start', () => api.post(`/v1/vms/${id}/migration/start`, { destination: dest.trim(), mode, ...(bw ? { bandwidth_mbps: Number(bw) } : {}), ...(down ? { max_downtime_ms: Number(down) } : {}) }), 'Migration started')}>Start migration</Button>
          </>
        )}
      </Card>
    </div>
  );
}
