import { useState } from 'react';
import { Cpu } from 'lucide-react';
import { Button, Card, EmptyState, Hero, KV, Pill, Sheet, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { GenericTable, flatten, rowsOf } from '../lib/generic';

type Tab = 'overview' | 'gpus' | 'apple' | 'pool';
type Row = Record<string, unknown>;
const str = (v: unknown): string => (typeof v === 'string' ? v : '');

function Facts({ path, empty }: { path: string; empty: string }) {
  const d = usePoll(() => api.get<unknown>(path).catch(() => null), 10000);
  const rows = flatten(d.data);
  if (d.loading && !d.data) return <Skeleton h={100} />;
  return rows.length ? <KV rows={rows} /> : <p className="fx-muted">{empty}</p>;
}

function GpusTab({ notify }: { notify: Notify }) {
  const gpus = usePoll(async () => rowsOf(await api.get<unknown>('/v1/host/gpus').catch(() => ({ items: [] })), 'items'), 8000);
  const health = usePoll(async () => rowsOf(await api.get<unknown>('/v1/host/gpus/health').catch(() => ({ items: [] })), 'items'), 15000);
  const mdev = usePoll(() => api.get<unknown>('/v1/host/mdev').catch(() => null), 15000);
  const { busy, run } = useAct(notify);
  const [act, setAct] = useState<{ gpu: Row; kind: 'bind' | 'release' } | null>(null);
  const [restore, setRestore] = useState(true);
  const [pre, setPre] = useState<unknown>(null);
  const rows = gpus.data ?? [];
  const open = async (gpu: Row, kind: 'bind' | 'release') => {
    setAct({ gpu, kind });
    setPre(null);
    if (kind === 'bind') setPre(await api.get<unknown>('/v1/host/gpus/preflight').catch((e: Error) => ({ error: e.message })));
  };
  const go = async () => {
    if (!act) return;
    const bdf = str(act.gpu.bdf);
    const ok = await run('gpu', () => api.post(`/v1/host/gpus/${act.kind}`, act.kind === 'bind' ? { bdf } : { bdf, restore_driver: restore }), act.kind === 'bind' ? `${bdf} bound to vfio-pci` : `${bdf} released`, gpus.refresh);
    if (ok) setAct(null);
  };
  return (
    <>
      <Card title="GPUs for passthrough">
        {gpus.loading && !rows.length ? <Skeleton h={100} /> : rows.length === 0 ? <EmptyState title="No GPUs found" body="Passthrough needs a GPU, an enabled IOMMU and a Linux host. Nothing here is hidden: this node reports none." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Device</th><th>Vendor</th><th>Driver</th><th>IOMMU group</th><th>State</th><th /></tr></thead><tbody>
            {rows.map((g) => {
              const bdf = str(g.bdf);
              const bound = g.group_bound_to_vfio === true;
              return (
                <tr key={bdf}>
                  <td className="fx-mono">{bdf}</td>
                  <td>{str(g.vendor) || '—'}</td>
                  <td className="fx-muted">{str(g.driver) || '—'}</td>
                  <td className="fx-muted">{g.iommu_group != null ? `${g.iommu_group} (${Array.isArray(g.iommu_members) ? g.iommu_members.length : 1})` : 'none'}</td>
                  <td>{g.group_held === true ? <Pill tone="warn">in use</Pill> : bound ? <Pill tone="ok">vfio ready</Pill> : <Pill tone="idle">host driver</Pill>}</td>
                  <td><span className="fx-row-actions">{bound ? <Button size="sm" disabled={g.group_held === true} title={g.group_held === true ? 'A machine holds this group' : undefined} onClick={() => open(g, 'release')}>Release</Button> : <Button size="sm" disabled={g.iommu_group == null} title={g.iommu_group == null ? 'No IOMMU group: enable IOMMU in firmware and the kernel' : undefined} onClick={() => open(g, 'bind')}>Bind to vfio</Button>}</span></td>
                </tr>
              );
            })}
          </tbody></table></div>
        )}
      </Card>
      <div className="fx-section"><Card title="Health"><GenericTable rows={health.data ?? []} empty="No GPU health data on this host." /></Card></div>
      <div className="fx-section"><Card title="Mediated devices"><MdevBlock data={mdev.data} /></Card></div>
      <Sheet open={!!act} title={act?.kind === 'bind' ? 'Bind to vfio-pci' : 'Release GPU'} onClose={() => setAct(null)}>
        {act?.kind === 'bind' ? (
          <>
            <p>Binding <b className="fx-mono">{str(act.gpu.bdf)}</b> moves its whole IOMMU group off the host driver. The host loses that GPU, including any display on it, until you release it.</p>
            {pre ? <details><summary>Preflight</summary><pre className="fx-code">{JSON.stringify(pre, null, 2)}</pre></details> : <Skeleton h={20} />}
          </>
        ) : (
          <label className="fx-check"><input type="checkbox" checked={restore} onChange={(e) => setRestore(e.target.checked)} /> Re-bind the previous host driver</label>
        )}
        <div className="fx-actions"><Button variant={act?.kind === 'bind' ? 'danger' : 'primary'} busy={busy === 'gpu'} onClick={go}>{act?.kind === 'bind' ? 'Bind GPU' : 'Release'}</Button><Button onClick={() => setAct(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}

function MdevBlock({ data }: { data: unknown }) {
  const rows = flatten(data);
  return rows.length ? <KV rows={rows} /> : <p className="fx-muted">No mediated-device types on this host.</p>;
}

interface PoolVm { id: string; name?: string; status?: unknown }

/** The Mac VM pool: pre-created macOS machines a launch can check out, then hand back. */
function MacPoolTab({ notify }: { notify: Notify }) {
  const status = usePoll(() => api.get<unknown>('/v1/macos-pool/status').catch(() => null), 8000);
  const list = usePoll(async () => (await api.get<PoolVm[]>('/v1/macos-pool').catch(() => [])) ?? [], 8000);
  const { busy, run } = useAct(notify);
  const rows = Array.isArray(list.data) ? list.data : [];
  const after = async () => { await list.refresh(); await status.refresh(); };
  return (
    <>
      <div className="fx-grid cols-2">
        <Card title="Pool" action={<Button size="sm" variant="primary" busy={busy === 'checkout'} onClick={() => run('checkout', () => api.post('/v1/macos-pool/checkout'), 'Checked out a pool machine', after)}>Check out</Button>}>
          {status.loading && !status.data ? <Skeleton h={80} /> : flatten(status.data).length ? <KV rows={flatten(status.data)} /> : <p className="fx-muted">The pool reports nothing.</p>}
        </Card>
        <Card title="Machines">
          {list.loading && !rows.length ? <Skeleton h={80} /> : rows.length === 0 ? <EmptyState title="The pool is empty" body="Enroll a macOS machine into the pool, or check one out when it has some." /> : (
            <div className="fx-table-wrap"><table className="fx-table"><tbody>
              {rows.map((v) => (
                <tr key={v.id}>
                  <td className="vm-name">{v.name ?? v.id.slice(0, 8)}</td>
                  <td><span className="fx-row-actions">
                    <Button size="sm" variant="ghost" busy={busy === 'enroll' + v.id} onClick={() => run('enroll' + v.id, () => api.post(`/v1/macos-pool/${v.id}/enroll`), 'Enrolled', after)}>Enroll</Button>
                    <Button size="sm" variant="ghost" busy={busy === 'return' + v.id} onClick={() => run('return' + v.id, () => api.post(`/v1/macos-pool/${v.id}/return`), 'Returned to the pool', after)}>Return</Button>
                  </span></td>
                </tr>
              ))}
            </tbody></table></div>
          )}
        </Card>
      </div>
    </>
  );
}

export function HostPage({ notify, os }: { notify: Notify; os: 'linux' | 'macos' | 'unknown' }) {
  const [tab, setTab] = useState<Tab>('overview');
  const tabs: { value: Tab; label: string }[] = [{ value: 'overview', label: 'Overview' }, ...(os !== 'macos' ? [{ value: 'gpus' as Tab, label: 'GPUs' }] : []), ...(os !== 'linux' ? [{ value: 'apple' as Tab, label: 'Apple host' }, { value: 'pool' as Tab, label: 'Mac pool' }] : [])];
  const t = tabs.some((x) => x.value === tab) ? tab : 'overview';
  return (
    <>
      <Hero eyebrow="Host" title="The machine under" accent="your machines." lede="Memory, pressure and topology, plus GPU passthrough on Linux and Virtualization.framework details on a Mac." />
      <Tabs<Tab> value={t} onChange={setTab} tabs={tabs} />
      <div className="fx-section">
        {t === 'overview' ? (
          <div className="fx-grid cols-2">
            <Card title="Memory"><Facts path="/v1/host/memory" empty="Memory is not reported on this host." /></Card>
            <Card title="Pressure"><Facts path="/v1/host/pressure" empty="Pressure stall information is not available (Linux only)." /></Card>
            <Card title="Topology"><Facts path="/v1/host/topology" empty="No topology reported." /></Card>
            <Card title="Confidential computing"><Facts path="/v1/host/confidential" empty="Not reported." /></Card>
            <Card title="Hibernation"><Facts path="/v1/host/hibernation" empty="Hibernation facts are not reported on this host." /></Card>
          </div>
        ) : t === 'gpus' ? <GpusTab notify={notify} /> : t === 'pool' ? <MacPoolTab notify={notify} /> : (
          <div className="fx-grid cols-2">
            <Card title={<><Cpu size={16} /> Virtual networks</>}><VzNets /></Card>
            <Card title="Local model server"><Facts path="/v1/host/infer" empty="No host model server answering." /></Card>
          </div>
        )}
      </div>
    </>
  );
}

function VzNets() {
  const n = usePoll(async () => rowsOf(await api.get<unknown>('/v1/vznets').catch(() => ({ items: [] })), 'items'), 15000);
  return <GenericTable rows={n.data ?? []} empty="No vmnet networks." />;
}
