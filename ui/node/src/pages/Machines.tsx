import { useMemo, useState, type ReactNode } from 'react';
import { Play, Pause, Plus, RotateCw, Square, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Pill, Segmented, Sheet, Skeleton, api, fmtMiB, statusTone, usePoll, type Vm } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { CreateMachine } from './CreateMachine';

type Filter = 'all' | 'running' | 'stopped' | 'failed';
type Op = 'start' | 'stop' | 'restart' | 'pause' | 'resume';

export const actionsFor = (status: string): Op[] => {
  if (status === 'running') return ['pause', 'restart', 'stop'];
  if (status === 'paused') return ['resume', 'stop'];
  if (status === 'stopped' || status === 'failed') return ['start'];
  return [];
};
/** Published ports as the legacy console showed them: `8080→80`, `guests:` when the forward is for other guests. */
export const portsOf = (vm: Vm): string => {
  const fwd = ((vm.request?.network as { forwards?: { host_port: number; guest_port: number; guests?: boolean }[] } | undefined)?.forwards) ?? [];
  return fwd.map((f) => `${f.guests ? 'guests:' : ''}${f.host_port}→${f.guest_port}`).join(' ');
};

/** VMs sharing the `fluxvm.stack` label, by stack name. */
export const stacksOf = (vms: Vm[]): { name: string; vms: Vm[] }[] => {
  const m = new Map<string, Vm[]>();
  for (const v of vms) { const n = v.labels?.['fluxvm.stack']; if (n) m.set(n, [...(m.get(n) ?? []), v]); }
  return [...m.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([name, list]) => ({ name, vms: list }));
};
const ICON: Record<Op, ReactNode> = { start: <Play size={14} />, stop: <Square size={14} />, restart: <RotateCw size={14} />, pause: <Pause size={14} />, resume: <Play size={14} /> };

export function Machines({ vms, setVms, notify, q: initialQ }: { vms: Vm[]; setVms: (v: Vm[]) => void; notify: Notify; q: string }) {
  const [q, setQ] = useState(initialQ);
  const [filter, setFilter] = useState<Filter>('all');
  const [confirm, setConfirm] = useState<Vm | null>(null);
  const [busy, setBusy] = useState<string>('');
  const [creating, setCreating] = useState(false);
  const list = usePoll(async () => {
    const r = await api.get<{ items: Vm[] }>('/v1/vms');
    setVms(r.items ?? []);
    return r.items ?? [];
  }, 4000);

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    return vms
      .filter((v) => (filter === 'all' ? true : filter === 'stopped' ? v.status === 'stopped' || v.status === 'paused' : v.status === filter))
      .filter((v) => !needle || `${v.name} ${v.id} ${v.guest_ip ?? ''} ${v.backend}`.toLowerCase().includes(needle))
      .sort((a, b) => a.name.localeCompare(b.name));
  }, [vms, q, filter]);

  const run = async (vm: Vm, op: Op | 'delete') => {
    setBusy(vm.id + op);
    try {
      if (op === 'delete') await api.del(`/v1/vms/${vm.id}`);
      else await api.post(`/v1/vms/${vm.id}/${op}`);
      notify(`${vm.name}: ${op} requested`);
      await list.refresh();
    } catch (e) {
      notify(`${vm.name}: ${(e as Error).message}`, 'bad');
    } finally {
      setBusy('');
      setConfirm(null);
    }
  };

  const stackAll = async (name: string, op: 'start' | 'stop', members: Vm[]) => {
    const want = members.filter((v) => (op === 'start' ? v.status === 'stopped' || v.status === 'failed' : v.status === 'running'));
    setBusy(`stack:${name}${op}`);
    let failed = 0;
    for (const v of want) { try { await api.post(`/v1/vms/${v.id}/${op}`); } catch (e) { failed++; notify(`${v.name}: ${(e as Error).message}`, 'bad'); } }
    notify(`Stack ${name}: ${op} requested for ${want.length - failed} of ${want.length}`, failed ? 'bad' : undefined);
    setBusy('');
    await list.refresh();
  };
  const stacks = useMemo(() => stacksOf(vms), [vms]);

  return (
    <>
      <Hero eyebrow="Machines" title="Every machine," accent="one glance.">
        <Button variant="primary" size="lg" onClick={() => setCreating(true)}><Plus size={18} />New machine</Button>
      </Hero>
      <div className="fx-toolbar">
        <input className="fx-input" placeholder="Search name, id, IP…" value={q} onChange={(e) => setQ(e.target.value)} aria-label="Search machines" />
        <Segmented label="Filter by state" value={filter} onChange={setFilter} options={[{ value: 'all', label: 'All' }, { value: 'running', label: 'Running' }, { value: 'stopped', label: 'Stopped' }, { value: 'failed', label: 'Failed' }]} />
      </div>
      {stacks.length ? (
        <div className="fx-toolbar" aria-label="Stacks">
          {stacks.map((st) => (
            <span key={st.name} className="fx-row-actions">
              <b>Stack <code>{st.name}</code></b> <span className="fx-muted">{st.vms.length} VM{st.vms.length === 1 ? '' : 's'}</span>
              <Button size="sm" busy={busy === `stack:${st.name}start`} onClick={() => void stackAll(st.name, 'start', st.vms)} aria-label={`Start all in stack ${st.name}`}>Start all</Button>
              <Button size="sm" busy={busy === `stack:${st.name}stop`} onClick={() => void stackAll(st.name, 'stop', st.vms)} aria-label={`Stop all in stack ${st.name}`}>Stop all</Button>
            </span>
          ))}
        </div>
      ) : null}
      <Card>
        {list.loading && vms.length === 0 ? <Skeleton h={160} /> : shown.length === 0 ? (
          <EmptyState title={vms.length ? 'No machines match' : 'No machines yet'} body={vms.length ? 'Try a different search or filter.' : 'Pick an image, a size and a network — your first machine is a minute away.'} action={vms.length ? undefined : <Button variant="primary" onClick={() => setCreating(true)}>Create your first machine</Button>} />
        ) : (
          <div className="fx-table-wrap">
            <table className="fx-table">
              <thead><tr><th>Name</th><th>State</th><th>Backend</th><th>IP</th><th>Ports</th><th>Size</th><th /></tr></thead>
              <tbody>
                {shown.map((vm) => (
                  <tr key={vm.id}>
                    <td><a className="vm-name" href={`#/machines/${vm.id}`}>{vm.name}</a>{vm.labels?.['fluxvm.stack'] ? <span className="vm-stack">{vm.labels['fluxvm.stack']}</span> : null}{vm.error ? <div className="vm-err">{vm.error}</div> : null}</td>
                    <td><Pill tone={statusTone(vm.status)}>{vm.status}</Pill></td>
                    <td className="fx-muted">{vm.request?.apple?.init_config ? 'container' : vm.backend}</td>
                    <td className="fx-mono">{vm.guest_ip || '—'}</td>
                    <td className="fx-mono">{portsOf(vm) || '—'}</td>
                    <td className="fx-muted">{vm.request ? `${vm.request.vcpus} vCPU · ${fmtMiB(vm.request.memory_mib)}` : '—'}</td>
                    <td>
                      <span className="fx-row-actions">
                        {actionsFor(vm.status).map((op) => (
                          <Button key={op} size="sm" variant={op === 'start' || op === 'resume' ? 'primary' : 'secondary'} busy={busy === vm.id + op} onClick={() => run(vm, op)} aria-label={`${op} ${vm.name}`}>{ICON[op]}{op}</Button>
                        ))}
                        <Button size="sm" variant="ghost" onClick={() => setConfirm(vm)} aria-label={`Delete ${vm.name}`}><Trash2 size={14} /></Button>
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <CreateMachine open={creating} onClose={() => setCreating(false)} notify={notify} onCreated={() => void list.refresh()} />
      <Sheet open={!!confirm} title="Delete machine" onClose={() => setConfirm(null)}>
        <p>Delete <b>{confirm?.name}</b>? Its disk is removed and this cannot be undone.</p>
        <div className="fx-hero-actions" style={{ justifyContent: 'flex-start' }}>
          <Button variant="danger" busy={!!confirm && busy === confirm.id + 'delete'} onClick={() => confirm && run(confirm, 'delete')}>Delete</Button>
          <Button onClick={() => setConfirm(null)}>Cancel</Button>
        </div>
      </Sheet>
    </>
  );
}
