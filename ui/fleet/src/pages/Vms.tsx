import { useState } from 'react';
import { Plus } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Segmented, Sheet, Skeleton, Tabs, ago, api, fmtMiB, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { VmRun } from './VmRun';
import { VmScreen } from './VmScreen';
import { VmSnapshots } from './VmSnapshots';
import { type FleetNode, type FleetVm, type LaunchDraft, launchProblem, launchRequest, powerActions, vmKey, vmState, vmTone } from '../lib/fleet';

type Row = Record<string, unknown>;
type Filter = 'all' | 'running' | 'other';
type Tab = 'overview' | 'screen' | 'snapshots' | 'run';
const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
const enc = encodeURIComponent;

export function VmsPage({ role, notify }: Ctx) {
  const list = usePoll(async () => {
    const r = await api.get<{ items?: FleetVm[]; unreachable_nodes?: { node: string }[] }>('/fleet/vms');
    return { items: r.items ?? [], unreachable: r.unreachable_nodes ?? [] };
  }, 5000);
  const images = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/images').catch(() => ({ items: [] })), 'items'), 60000);
  const overlays = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/overlays').catch(() => ({ items: [] })), 'items'), 60000);
  const nodes = usePoll(async () => (role === 'operator' ? (rowsOf(await api.get<unknown>('/fleet/nodes').catch(() => ({ items: [] })), 'items') as unknown as FleetNode[]) : []), 30000, [role]);
  const { busy, run } = useAct(notify);
  const [q, setQ] = useState(() => new URLSearchParams(location.hash.split('?')[1] ?? '').get('q') ?? '');
  const [filter, setFilter] = useState<Filter>('all');
  const [sel, setSel] = useState<FleetVm | null>(null);
  const [logs, setLogs] = useState('');
  const [tab, setTab] = useState<Tab>('overview');
  const [launch, setLaunch] = useState(false);
  const [touched, setTouched] = useState(false);
  const [d, setD] = useState<LaunchDraft>({ name: '', image: '', guest: 'macos', vcpus: '4', memoryGib: '8', node: '', ttl: '', networks: [] });
  const [del, setDel] = useState<FleetVm | null>(null);
  const all = list.data?.items ?? [];
  const shown = all.filter((v) => {
    const s = vmState(v);
    if (filter === 'running' && s !== 'running') return false;
    if (filter === 'other' && s === 'running') return false;
    const t = q.trim().toLowerCase();
    return !t || [v.name, v.node, v.id].some((x) => String(x ?? '').toLowerCase().includes(t));
  });
  const imgs = images.data ?? [];
  const problem = launchProblem(d);
  const pickImage = (path: string) => {
    const i = imgs.find((x) => x.image === path);
    setD({ ...d, image: path, guest: String(i?.guest_os ?? d.guest), diskGib: typeof i?.disk_size_gib === 'number' ? i.disk_size_gib : undefined });
  };
  const open = async (v: FleetVm) => {
    setSel(v);
    setTab('overview');
    setLogs('');
    try { setLogs(await api.get<string>(`/fleet/vms/${enc(v.node)}/${enc(v.id)}/logs`, { text: true })); } catch (e) { setLogs((e as Error).message); }
  };
  const power = (v: FleetVm, a: string) => run(a + vmKey(v), () => api.post(`/fleet/vms/${enc(v.node)}/${enc(v.id)}/${a}`), `${v.name}: ${a}`, list.refresh);
  return (
    <>
      <Hero eyebrow="Virtual machines" title="Every VM," accent="on every host." lede={role === 'operator' ? 'Everything tenants have launched through this central.' : 'Your VMs, on the hosts you lease and on shared hosts.'}>
        <Button variant="primary" size="lg" onClick={() => setLaunch(true)}><Plus size={18} />Launch VM</Button>
      </Hero>
      {list.data?.unreachable.length ? <p className="fx-error" role="status">Not listed because they did not answer: {list.data.unreachable.map((u) => u.node).join(', ')}.</p> : null}
      <div className="fx-toolbar">
        <input className="fx-input" aria-label="Search" placeholder="Search by name, host or id" value={q} onChange={(e) => setQ(e.target.value)} />
        <Segmented<Filter> label="Filter" value={filter} onChange={setFilter} options={[{ value: 'all', label: 'All' }, { value: 'running', label: 'Running' }, { value: 'other', label: 'Not running' }]} />
      </div>
      <Card>
        {list.loading && !all.length ? <Skeleton h={160} /> : all.length === 0 ? <EmptyState title="No virtual machines yet" body="Launch one from an image in the catalog." action={<Button variant="primary" onClick={() => setLaunch(true)}>Launch a VM</Button>} /> : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th>Name</th><th>State</th><th>Host</th><th>Size</th>{role === 'operator' ? <th>Tenant</th> : null}<th>Created</th></tr></thead>
            <tbody>
              {shown.length === 0 ? <tr><td colSpan={6} className="fx-muted">Nothing matches.</td></tr> : shown.map((v) => (
                <tr key={vmKey(v)} className="link" tabIndex={0} onClick={() => void open(v)} onKeyDown={(e) => e.key === 'Enter' && void open(v)}>
                  <td className="vm-name">{v.name}</td>
                  <td><Pill tone={vmTone(vmState(v))}>{cap(vmState(v))}</Pill></td>
                  <td className="fx-muted">{v.node}</td>
                  <td className="fx-muted">{v.request?.vcpus ?? '–'} vCPUs, {v.request?.memory_mib ? fmtMiB(v.request.memory_mib) : '–'}</td>
                  {role === 'operator' ? <td className="fx-muted">{v.request?.tenant ?? 'none'}</td> : null}
                  <td className="fx-muted">{v.created_at ? ago(v.created_at) : '—'}</td>
                </tr>
              ))}
            </tbody>
          </table></div>
        )}
      </Card>
      <Sheet open={!!sel} title={sel?.name ?? ''} onClose={() => setSel(null)}>
        {sel ? (
          <>
            <p className="fx-muted">on {sel.node} · <span className="fx-mono">{sel.id.slice(0, 8)}</span></p>
            {(() => {
              const state = vmState(sel);
              const live = state === 'running';
              const tabs: { value: Tab; label: string }[] = [{ value: 'overview', label: 'Overview' }, ...(live || state === 'paused' ? [{ value: 'screen' as const, label: 'Screen' }] : []), { value: 'snapshots', label: 'Snapshots' }, ...(live ? [{ value: 'run' as const, label: 'Run' }] : [])];
              const here = tabs.some((t) => t.value === tab) ? tab : 'overview';
              const macos = sel.request?.apple?.guest_os === 'macos';
              return (
                <>
                  <Tabs<Tab> value={here} onChange={setTab} tabs={tabs} />
                  {here === 'overview' ? (
                    <>
                      <div className="fx-actions">
                        {powerActions(state).map((a) => <Button key={a} busy={busy === a + vmKey(sel)} onClick={() => power(sel, a)}>{cap(a)}</Button>)}
                        <Button variant="danger" onClick={() => (setDel(sel), setSel(null))}>Delete</Button>
                      </div>
                      <h3>Console log</h3>
                      {logs ? <pre className="fx-code">{logs.slice(-6000)}</pre> : <Skeleton h={80} />}
                    </>
                  ) : here === 'screen' ? <VmScreen key={vmKey(sel)} vm={sel} canControl={macos && live} notify={notify} />
                    : here === 'snapshots' ? <VmSnapshots key={vmKey(sel)} vm={sel} stopped={state === 'stopped'} macos={macos} taken={all.map((x) => x.name)} notify={notify} onCloned={() => void list.refresh()} />
                    : <VmRun key={vmKey(sel)} vm={sel} />}
                </>
              );
            })()}
          </>
        ) : null}
      </Sheet>
      <Sheet open={!!del} title={`Delete ${del?.name ?? ''}`} onClose={() => setDel(null)}>
        <p>Delete <b>{del?.name}</b> and its disk? This cannot be undone.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => { if (del && (await run('del', () => api.del(`/fleet/vms/${enc(del.node)}/${enc(del.id)}`), `${del.name} deleted`, list.refresh))) setDel(null); }}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={launch} title="Launch a VM" onClose={() => setLaunch(false)}>
        <Field label="Image">{(id) => <select id={id} className="fx-input" value={d.image} onChange={(e) => pickImage(e.target.value)}><option value="">Choose…</option>{imgs.map((i) => <option key={String(i.image)} value={String(i.image)}>{String(i.display_name ?? i.name)} ({String(i.guest_os ?? '')})</option>)}</select>}</Field>
        <Field label="Name">{(id) => <input id={id} className="fx-input" value={d.name} onChange={(e) => setD({ ...d, name: e.target.value })} />}</Field>
        <div className="fx-grid cols-2">
          <Field label="vCPUs">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={d.vcpus} onChange={(e) => setD({ ...d, vcpus: e.target.value })} />}</Field>
          <Field label="Memory (GiB)">{(id) => <input id={id} className="fx-input" inputMode="decimal" value={d.memoryGib} onChange={(e) => setD({ ...d, memoryGib: e.target.value })} />}</Field>
        </div>
        {role === 'operator' ? <Field label="Host" hint="Empty lets central choose.">{(id) => <select id={id} className="fx-input" value={d.node} onChange={(e) => setD({ ...d, node: e.target.value })}><option value="">Any</option>{(nodes.data ?? []).map((n) => <option key={n.name}>{n.name}</option>)}</select>}</Field> : null}
        <Field label="Delete automatically">{(id) => <select id={id} className="fx-input" value={d.ttl} onChange={(e) => setD({ ...d, ttl: e.target.value })}><option value="">Never</option><option value="3600">After 1 hour</option><option value="28800">After 8 hours</option><option value="86400">After 24 hours</option><option value="604800">After 7 days</option></select>}</Field>
        {(overlays.data ?? []).length ? (
          <div>{(overlays.data ?? []).map((o) => { const n = String(o.name); return <label key={n} className="fx-check"><input type="checkbox" checked={d.networks.includes(n)} onChange={(e) => setD({ ...d, networks: e.target.checked ? [...d.networks, n] : d.networks.filter((x) => x !== n) })} /> {n} <span className="fx-muted">{String(o.subnet ?? '')}</span></label>; })}</div>
        ) : null}
        {touched && problem ? <p className="fx-error" role="alert">{problem}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'launch'} onClick={async () => {
          setTouched(true);
          if (problem) return;
          let at = '';
          const ok = await run('launch', async () => { const r = await api.post<Row>('/fleet/vms', launchRequest(d)); at = `${String(r.node ?? '')}`; }, undefined, list.refresh);
          if (ok) (notify(`Launched ${d.name}${at ? ' on ' + at : ''}`), setLaunch(false), setTouched(false), setD({ ...d, name: '' }));
        }}>Launch VM</Button><Button onClick={() => setLaunch(false)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
