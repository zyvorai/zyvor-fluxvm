import { useState } from 'react';
import { Button, Card, EmptyState, Field, Hero, Meter, Pill, Sheet, Skeleton, api, fmtMiB, left, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { type Drain, type FleetNode, type FleetVm, type Lease, leaseProgress, nodeState } from '../lib/fleet';

type Row = Record<string, unknown>;
const DRAIN: [string, string, string][] = [
  ['cordon', 'Leave them running', 'Only stop new VMs. Wait for them to finish, then uncordon.'],
  ['evict-ephemeral', 'Delete temporary VMs', 'VMs with an automatic deletion time or labelled fluxvm.dev/ephemeral=true. Others keep running.'],
  ['reschedule', 'Move what can be moved', 'Reschedulable VMs are recreated on another host from their spec. Disks do not move.'],
];

export function HostsPage({ role, notify }: Ctx) {
  const nodes = usePoll(async () => {
    const list = role === 'operator' ? (rowsOf(await api.get<unknown>('/fleet/nodes'), 'items') as unknown as FleetNode[]) : [];
    const drains: Record<string, Drain> = {};
    await Promise.all(list.map(async (n) => { drains[n.name] = await api.get<Drain>(`/fleet/nodes/${encodeURIComponent(n.name)}/drain`).catch(() => ({})); }));
    return { list, drains };
  }, 8000, [role]);
  const leases = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/leases'), 'items') as unknown as Lease[], 10000);
  const vms = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/vms'), 'items') as unknown as FleetVm[], 15000);
  const tenants = usePoll(async () => (role === 'operator' ? rowsOf(await api.get<unknown>('/fleet/tenants').catch(() => ({ items: [] })), 'items') : []), 60000, [role]);
  const { busy, run } = useAct(notify);
  const [drain, setDrain] = useState<string | null>(null);
  const [mode, setMode] = useState('cordon');
  const [force, setForce] = useState(false);
  const [lease, setLease] = useState(false);
  const [tenant, setTenant] = useState('');
  const [hours, setHours] = useState('');
  const [size, setSize] = useState('');
  const refresh = async () => { await Promise.all([nodes.refresh(), leases.refresh()]); };
  const active = (leases.data ?? []).filter((l) => l.state === 'active');
  const leaseOf = (name: string) => active.find((l) => l.node === name) ?? null;
  const rows: { n: FleetNode; d: Drain; l: Lease | null }[] = role === 'operator'
    ? (nodes.data?.list ?? []).map((n) => ({ n, d: nodes.data?.drains[n.name] ?? {}, l: leaseOf(n.name) }))
    : active.map((l) => ({ n: { name: l.node }, d: {}, l }));
  const countOn = (name: string) => (vms.data ?? []).filter((v) => v.node === name).length;
  const leaseBody = () => {
    const b: Row = {};
    if (hours) b.hours = Number(hours);
    if (role === 'operator') b.tenant = tenant;
    else if (size === 'm') Object.assign(b, { min_vcpus: 8, min_memory_mib: 24576 });
    else if (size === 'l') Object.assign(b, { min_vcpus: 16, min_memory_mib: 49152 });
    return b;
  };
  const drainTarget = rows.find((r) => r.n.name === drain);
  return (
    <>
      <Hero eyebrow="Hosts" title={role === 'operator' ? 'Every host,' : 'Hosts that are'} accent={role === 'operator' ? 'at a glance.' : 'yours alone.'} lede={role === 'operator' ? 'Cordon, drain and scrub hosts, or lease one to a tenant.' : 'A leased host runs only your VMs. Leases last at least 24 hours.'}>
        <Button variant="primary" size="lg" onClick={() => setLease(true)}>{role === 'operator' ? 'Lease a host to a tenant' : 'Lease a host'}</Button>
      </Hero>
      {nodes.loading && !rows.length ? <Skeleton h={160} /> : rows.length === 0 ? (
        <Card><EmptyState title={role === 'operator' ? 'No hosts have registered' : 'You have no hosts yet'} body={role === 'operator' ? 'Run fluxvm-agent node on each host with --central pointing here.' : 'Lease a host to run VMs on it.'} /></Card>
      ) : (
        <div className="fx-grid cols-2">
          {rows.map(({ n, d, l }) => {
            const st = nodeState(n, d, l);
            const lp = l ? leaseProgress(l) : null;
            const cnt = n.vm_count ?? countOn(n.name);
            return (
              <Card key={n.name} title={<>{n.name} <Pill tone={st.tone} raw>{st.text}</Pill></>}>
                <p className="fx-muted" style={{ margin: '0 0 12px' }}>
                  {n.vcpus_total ? `${n.vcpus_total} vCPUs` : ''}{n.memory_mib_total ? ` · ${fmtMiB(n.memory_mib_total)}` : ''} · {cnt} VM{cnt === 1 ? '' : 's'}{n.outdated ? ' · agent out of date' : ''}
                </p>
                {lp && l ? <Meter value={Math.round(lp.p * 100)} tone="info" label={lp.releasable ? 'Minimum lease period over' : `Minimum period ends in ${left(l.min_end)}`} /> : null}
                <div className="fx-actions" style={{ marginTop: 14 }}>
                  {role === 'operator' ? (
                    <>
                      {n.cordoned ? <Button size="sm" busy={busy === 'u' + n.name} onClick={() => run('u' + n.name, () => api.post(`/fleet/nodes/${encodeURIComponent(n.name)}/uncordon`), `${n.name} uncordoned`, refresh)}>Uncordon</Button> : <Button size="sm" busy={busy === 'c' + n.name} onClick={() => run('c' + n.name, () => api.post(`/fleet/nodes/${encodeURIComponent(n.name)}/cordon`), `${n.name} cordoned`, refresh)}>Cordon</Button>}
                      <Button size="sm" onClick={() => (setDrain(n.name), setMode('cordon'), setForce(false))}>Drain</Button>
                      {d.scrub_pending ? <Button size="sm" variant="primary" busy={busy === 's' + n.name} onClick={() => run('s' + n.name, () => api.post(`/fleet/nodes/${encodeURIComponent(n.name)}/scrub`), `${n.name} scrubbed`, refresh)}>Scrub</Button> : null}
                    </>
                  ) : null}
                  {l ? <Button size="sm" variant="danger" disabled={!lp?.releasable} title={lp?.releasable ? undefined : 'Not before the 24-hour minimum is over'} busy={busy === 'r' + l.id} onClick={() => run('r' + l.id, () => api.post(`/fleet/leases/${encodeURIComponent(l.id)}/release`), `${n.name} released`, refresh)}>Release</Button> : null}
                </div>
              </Card>
            );
          })}
        </div>
      )}
      <Sheet open={!!drain} title={`Drain ${drain ?? ''}`} onClose={() => setDrain(null)}>
        <p>Draining cordons the host first, so nothing new is placed on it. Choose what happens to the VMs on it.</p>
        <div className="fx-presets" style={{ gridTemplateColumns: '1fr' }}>
          {DRAIN.map(([v, t, s]) => <button key={v} className="fx-preset" aria-pressed={mode === v} onClick={() => setMode(v)}><b>{t}</b><small>{s}</small></button>)}
        </div>
        {drainTarget?.l ? <label className="fx-check"><input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} /> It is leased to {drainTarget.l.tenant}. Drain anyway. Tell them first.</label> : null}
        <div className="fx-actions"><Button variant="danger" busy={busy === 'drain'} disabled={!!drainTarget?.l && !force} onClick={async () => { if (drain && (await run('drain', () => api.post(`/fleet/nodes/${encodeURIComponent(drain)}/drain?mode=${encodeURIComponent(mode)}${force ? '&force=true' : ''}`), `${drain} draining`, refresh))) setDrain(null); }}>Drain host</Button><Button onClick={() => setDrain(null)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={lease} title={role === 'operator' ? 'Lease a host to a tenant' : 'Lease a host'} onClose={() => setLease(false)}>
        {role === 'operator' ? (
          <Field label="Tenant">{(id) => <select id={id} className="fx-input" value={tenant} onChange={(e) => setTenant(e.target.value)}><option value="">Choose…</option>{(tenants.data ?? []).map((t) => <option key={String(t.id)}>{String(t.id)}</option>)}</select>}</Field>
        ) : (
          <Field label="Size">{(id) => <select id={id} className="fx-input" value={size} onChange={(e) => setSize(e.target.value)}><option value="">Any host</option><option value="m">8 vCPUs, 24 GiB or more</option><option value="l">16 vCPUs, 48 GiB or more</option></select>}</Field>
        )}
        <Field label="Hours" hint="Empty = open-ended after the 24-hour minimum.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={hours} onChange={(e) => setHours(e.target.value)} placeholder="24" />}</Field>
        {hours && !(Number(hours) >= 24) ? <p className="fx-error" role="alert">Leases last at least 24 hours.</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'lease'} disabled={(role === 'operator' && !tenant) || (!!hours && !(Number(hours) >= 24))} onClick={async () => { if (await run('lease', () => api.post('/fleet/leases', leaseBody()), 'Host leased', refresh)) setLease(false); }}>Lease host</Button><Button onClick={() => setLease(false)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
