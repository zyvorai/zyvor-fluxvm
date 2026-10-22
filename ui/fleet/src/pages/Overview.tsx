import { Button, Card, Hero, Meter, Pill, Skeleton, Stat, ago, api, navigate, statusTone, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { GenericTable, rowsOf } from '../lib/generic';
import { type FleetNode, type FleetVm, type Lease, vmState } from '../lib/fleet';
import { type Capacity, loadTone, pct } from '../lib/ops';

export function Overview({ role }: Ctx) {
  const vms = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/vms'), 'items') as unknown as FleetVm[], 6000);
  const leases = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/leases'), 'items') as unknown as Lease[], 15000);
  const nodes = usePoll(async () => (role === 'operator' ? (rowsOf(await api.get<unknown>('/fleet/nodes'), 'items') as unknown as FleetNode[]) : []), 8000, [role]);
  const tenants = usePoll(async () => (role === 'operator' ? rowsOf(await api.get<unknown>('/fleet/tenants').catch(() => ({ items: [] })), 'items') : []), 30000, [role]);
  const events = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/events?limit=8').catch(() => ({ items: [] })), 'items'), 8000);
  const cap = usePoll(() => (role === 'operator' ? api.get<Capacity>('/fleet/capacity').catch(() => null) : Promise.resolve(null)), 10000, [role]);
  const v = vms.data ?? [];
  const running = v.filter((x) => vmState(x) === 'running').length;
  const failed = v.filter((x) => vmState(x) === 'failed');
  const n = nodes.data ?? [];
  const down = n.filter((x) => x.healthy === false || x.cordoned);
  const active = (leases.data ?? []).filter((l) => l.state === 'active').length;
  const loading = vms.loading && !vms.data;
  return (
    <>
      <Hero eyebrow="FluxVM Cloud" title={role === 'operator' ? 'Your whole fleet,' : 'Your machines,'} accent={role === 'operator' ? 'one calm view.' : 'on your own hosts.'} lede={role === 'operator' ? 'Hosts, tenants and every VM they run. Press ⌘K to jump anywhere.' : 'Lease a host, launch VMs, and keep an eye on what they cost.'}>
        <Button variant="primary" size="lg" onClick={() => navigate('/vms')}>Virtual machines</Button>
      </Hero>
      <div className="fx-grid cols-4">
        {loading ? [0, 1, 2, 3].map((i) => <Skeleton key={i} h={104} />) : role === 'operator' ? (
          <>
            <Stat label="Hosts" value={n.length} tone="info" hint={down.length ? `${down.length} need attention` : 'all healthy'} />
            <Stat label="VMs running" value={running} tone="ok" hint={`of ${v.length}`} />
            <Stat label="Tenants" value={(tenants.data ?? []).length} tone="idle" />
            <Stat label="Failed VMs" value={failed.length} tone={failed.length ? 'bad' : 'idle'} />
          </>
        ) : (
          <>
            <Stat label="Hosts leased" value={active} tone="info" />
            <Stat label="VMs running" value={running} tone="ok" hint={`of ${v.length}`} />
            <Stat label="Stopped" value={v.filter((x) => vmState(x) === 'stopped').length} tone="idle" />
            <Stat label="Failed" value={failed.length} tone={failed.length ? 'bad' : 'idle'} />
          </>
        )}
      </div>
      {role === 'operator' && cap.data ? (
        <div className="fx-section">
          <Card title="Capacity" action={<span className="fx-muted">{cap.data.nodes_schedulable} of {cap.data.nodes_total} hosts can take new VMs</span>}>
            <div className="fx-grid cols-2">
              <Meter label={`vCPUs · ${cap.data.vcpus_used} of ${cap.data.vcpus_total} in use`} value={pct(cap.data.vcpus_used, cap.data.vcpus_total)} tone={loadTone(pct(cap.data.vcpus_used, cap.data.vcpus_total))} />
              <Meter label={`Memory · ${Math.round(cap.data.memory_mib_used / 1024)} of ${Math.round(cap.data.memory_mib_total / 1024)} GiB in use`} value={pct(cap.data.memory_mib_used, cap.data.memory_mib_total)} tone={loadTone(pct(cap.data.memory_mib_used, cap.data.memory_mib_total))} />
            </div>
          </Card>
        </div>
      ) : null}
      {role === 'operator' && down.length ? (
        <div className="fx-section">
          <Card title="Needs attention">
            <div className="fx-table-wrap"><table className="fx-table"><tbody>{down.map((x) => <tr key={x.name} className="link" onClick={() => navigate('/hosts')}><td className="vm-name">{x.name}</td><td><Pill tone={x.healthy === false ? 'bad' : 'warn'}>{x.healthy === false ? 'Not responding' : 'Cordoned'}</Pill></td></tr>)}</tbody></table></div>
          </Card>
        </div>
      ) : null}
      {failed.length ? (
        <div className="fx-section">
          <Card title="Failed VMs">
            <div className="fx-table-wrap"><table className="fx-table"><tbody>{failed.map((x) => <tr key={x.node + x.id} className="link" onClick={() => navigate('/vms?q=' + encodeURIComponent(x.name))}><td className="vm-name">{x.name}</td><td className="fx-muted">{x.node}</td><td><Pill tone={statusTone('failed')}>failed</Pill></td></tr>)}</tbody></table></div>
          </Card>
        </div>
      ) : null}
      <div className="fx-section">
        <Card title="Recent activity" action={<Button size="sm" variant="ghost" onClick={() => navigate('/activity')}>See all</Button>}>
          {events.loading && !events.data ? <Skeleton h={100} /> : <GenericTable rows={(events.data ?? []).slice(0, 8).map((e) => ({ ...e, ...(typeof e.at === 'string' ? { at: ago(e.at) } : {}) }))} empty="Nothing has happened yet." />}
        </Card>
      </div>
    </>
  );
}
