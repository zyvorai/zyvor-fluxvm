import { useState, type ReactNode } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, KV, Pill, Segmented, Sheet, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { GenericTable, flatten, rowsOf } from '../lib/generic';
import { fluxctlFor } from '../lib/images';
import {
  type BackendHealth, type GroupDraft, type SecurityGroup, type ServiceSpec, type VnetDraft, type VnetStatus,
  CNP_TEMPLATE, SERVICE_REQUIRED, SERVICE_TEMPLATE, cnpName, describePolicy, groupBody, groupProblem, parseObject, protoName, serviceHealth, vnetBody, vnetProblem,
} from '../lib/network';
import { CopyCmd } from './Images';

function Confirm({ what, onConfirm, onClose, busy }: { what: string | null; onConfirm: () => void; onClose: () => void; busy: boolean }) {
  return (
    <Sheet open={!!what} title={`Delete ${what ?? ''}`} onClose={onClose}>
      <p>Delete <b>{what}</b>? This cannot be undone.</p>
      <div className="fx-actions"><Button variant="danger" busy={busy} onClick={onConfirm}>Delete</Button><Button onClick={onClose}>Cancel</Button></div>
    </Sheet>
  );
}

const Table = ({ head, children }: { head: string[]; children: ReactNode }) => (
  <div className="fx-table-wrap"><table className="fx-table"><thead><tr>{head.map((h) => <th key={h || 'x'}>{h}</th>)}</tr></thead><tbody>{children}</tbody></table></div>
);

const input = (v: string, set: (s: string) => void, id: string, ph = '') => <input id={id} className="fx-input" value={v} placeholder={ph} onChange={(e) => set(e.target.value)} />;

// ---- Virtual networks ----

const NO_VNET: VnetDraft = { vni: '', cidr: '', gateway: '', uplink: '', snat: '' };

export function VnetsTab({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/network/vnets')) as unknown as VnetStatus[], 8000);
  const { busy, run } = useAct(notify);
  const [draft, setDraft] = useState<VnetDraft | null>(null);
  const [prev, setPrev] = useState<VnetStatus | undefined>();
  const [del, setDel] = useState<number | null>(null);
  const rows = list.data ?? [];
  const problem = draft ? vnetProblem(draft) : '';
  const edit = (v: VnetStatus) => {
    setPrev(v);
    setDraft({ vni: String(v.config.vni), cidr: v.config.cidr, gateway: v.config.gateway, uplink: v.config.uplink ?? '', snat: v.config.snat_address ?? '' });
  };
  const save = async () => {
    if (!draft || problem) return;
    const body = vnetBody(draft, prev?.config);
    if (await run('save', () => api.put(`/v1/network/vnets/${body.vni}`, body), `Network ${body.vni} saved`, list.refresh)) setDraft(null);
  };
  return (
    <>
      <Card title="Virtual networks" action={<Button size="sm" onClick={() => (setPrev(undefined), setDraft(NO_VNET))}><Plus size={14} /> New network</Button>}>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? (
          <EmptyState title="No virtual networks" body="A virtual network is a Geneve overlay with its own CIDR, gateway and optional SNAT and floating IPs." />
        ) : (
          <Table head={['VNI', 'Network', 'Gateway', 'Uplink / SNAT', 'Guests', '']}>
            {rows.map((v) => (
              <tr key={v.config.vni}>
                <td className="fx-mono">{v.config.vni}</td>
                <td className="fx-mono">{v.config.cidr}</td>
                <td className="fx-mono">{v.config.gateway}</td>
                <td className="fx-muted">{v.config.uplink ? `${v.config.uplink}${v.config.snat_address ? ` → ${v.config.snat_address}` : ''}` : 'no outbound'}</td>
                <td className="fx-muted">{v.local_endpoints?.length ?? 0} local · {v.remote_endpoints ?? 0} remote</td>
                <td><span className="fx-row-actions">
                  <Button size="sm" variant="ghost" onClick={() => edit(v)}>Edit</Button>
                  <Button size="sm" variant="ghost" aria-label={`Delete network ${v.config.vni}`} onClick={() => setDel(v.config.vni)}><Trash2 size={14} /></Button>
                </span></td>
              </tr>
            ))}
          </Table>
        )}
      </Card>
      <Sheet open={!!draft} title={prev ? `Network ${prev.config.vni}` : 'New virtual network'} onClose={() => setDraft(null)}>
        {draft ? (
          <>
            <Field label="VNI" hint="Fixed once created.">{(id) => (prev ? <input id={id} className="fx-input" value={draft.vni} disabled /> : input(draft.vni, (vni) => setDraft({ ...draft, vni }), id, '100'))}</Field>
            <Field label="Network">{(id) => input(draft.cidr, (cidr) => setDraft({ ...draft, cidr }), id, '10.20.0.0/24')}</Field>
            <Field label="Gateway">{(id) => input(draft.gateway, (gateway) => setDraft({ ...draft, gateway }), id, '10.20.0.1')}</Field>
            <Field label="Uplink interface" hint="Needed for SNAT and floating IPs.">{(id) => input(draft.uplink, (uplink) => setDraft({ ...draft, uplink }), id, 'eth0')}</Field>
            <Field label="SNAT address" hint="Without one, guests on this network cannot reach out.">{(id) => input(draft.snat, (snat) => setDraft({ ...draft, snat }), id, '203.0.113.5')}</Field>
            {draft.vni && problem ? <p className="fx-error">{problem}</p> : null}
            <div className="fx-actions"><Button variant="primary" busy={busy === 'save'} disabled={!!problem} onClick={save}>Save</Button><Button onClick={() => setDraft(null)}>Cancel</Button></div>
          </>
        ) : null}
      </Sheet>
      <Confirm what={del == null ? null : `network ${del}`} busy={busy === 'del'} onClose={() => setDel(null)} onConfirm={async () => {
        if (del != null && (await run('del', () => api.del(`/v1/network/vnets/${del}`), `Network ${del} deleted`, list.refresh))) setDel(null);
      }} />
    </>
  );
}

// ---- Security groups ----

const NO_GROUP: GroupDraft = { name: '', labels: '', cidrs: '', ports: '', defaultAllow: false, priority: '', description: '' };

export function GroupsTab({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/network/groups')) as unknown as SecurityGroup[], 8000);
  const { busy, run } = useAct(notify);
  const [draft, setDraft] = useState<GroupDraft | null>(null);
  const [del, setDel] = useState<string | null>(null);
  const rows = list.data ?? [];
  const problem = draft ? groupProblem(draft) : '';
  const save = async () => {
    if (!draft || problem) return;
    if (await run('save', () => api.post('/v1/network/groups', groupBody(draft)), `${draft.name.trim()} saved`, list.refresh)) setDraft(null);
  };
  return (
    <>
      <Card title="Security groups" action={<Button size="sm" onClick={() => setDraft(NO_GROUP)}><Plus size={14} /> New group</Button>}>
        <p className="fx-muted" style={{ marginTop: 0 }}>A machine joins a group by name or by matching a label. Lower priority wins a tie.</p>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? (
          <EmptyState title="No security groups" body="Create a group to give several machines the same network policy." />
        ) : (
          <Table head={['Group', 'Labels', 'Policy', 'Priority', '']}>
            {rows.map((g) => (
              <tr key={g.name}>
                <td className="vm-name">{g.name}{g.description ? <div className="fx-muted">{g.description}</div> : null}</td>
                <td className="fx-muted">{g.labels?.length ? g.labels.join(', ') : '—'}</td>
                <td className="fx-muted">{describePolicy(g.policy)}</td>
                <td className="fx-muted">{g.priority ?? 0}</td>
                <td><span className="fx-row-actions">
                  <CopyCmd notify={notify} cmd={fluxctlFor('group get', g.name)} />
                  <Button size="sm" variant="ghost" aria-label={`Delete ${g.name}`} onClick={() => setDel(g.name)}><Trash2 size={14} /></Button>
                </span></td>
              </tr>
            ))}
          </Table>
        )}
      </Card>
      <Sheet open={!!draft} title="New security group" onClose={() => setDraft(null)}>
        {draft ? (
          <>
            <Field label="Name">{(id) => input(draft.name, (name) => setDraft({ ...draft, name }), id, 'web')}</Field>
            <Field label="Labels" hint="key=value, separated by spaces or commas.">{(id) => input(draft.labels, (labels) => setDraft({ ...draft, labels }), id, 'role=web')}</Field>
            <Field label="Allowed destinations" hint="CIDRs. Once any CIDR or port is listed, everything else is denied.">{(id) => input(draft.cidrs, (cidrs) => setDraft({ ...draft, cidrs }), id, '10.0.0.0/16')}</Field>
            <Field label="Allowed ports">{(id) => input(draft.ports, (ports) => setDraft({ ...draft, ports }), id, 'tcp/443 udp/53')}</Field>
            <Field label="Priority">{(id) => input(draft.priority, (priority) => setDraft({ ...draft, priority }), id, '0')}</Field>
            <Field label="Description">{(id) => input(draft.description, (description) => setDraft({ ...draft, description }), id)}</Field>
            <label className="fx-check"><input type="checkbox" checked={draft.defaultAllow} onChange={(e) => setDraft({ ...draft, defaultAllow: e.target.checked })} /> Allow everything when no CIDR or port is listed</label>
            {draft.name && problem ? <p className="fx-error">{problem}</p> : null}
            <div className="fx-actions"><Button variant="primary" busy={busy === 'save'} disabled={!draft.name.trim() || !!problem} onClick={save}>Save</Button><Button onClick={() => setDraft(null)}>Cancel</Button></div>
          </>
        ) : null}
      </Sheet>
      <Confirm what={del} busy={busy === 'del'} onClose={() => setDel(null)} onConfirm={async () => {
        if (del && (await run('del', () => api.del(`/v1/network/groups/${encodeURIComponent(del)}`), `${del} deleted`, list.refresh))) setDel(null);
      }} />
    </>
  );
}

// ---- Services ----

const HEALTH_TONE = { healthy: 'ok', degraded: 'warn', down: 'bad', unknown: 'idle' } as const;

export function ServicesTab({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/network/services')) as unknown as ServiceSpec[], 8000);
  const health = usePoll(async () => rowsOf(await api.get<unknown>('/v1/network/services/health').catch(() => null), 'backends') as unknown as BackendHealth[], 8000);
  const host = usePoll(() => api.get<{ xdp_acceleration?: boolean; map_tier?: string; north_south_interfaces?: string[] }>('/v1/network/services/status').catch(() => null), 15000);
  const { busy, run } = useAct(notify);
  const [del, setDel] = useState<string | null>(null);
  const [apply, setApply] = useState<string | null>(null);
  const rows = list.data ?? [];
  const report = health.data ?? [];
  const parsed = apply == null ? null : parseObject(apply, SERVICE_REQUIRED);
  const h = host.data;
  return (
    <>
      <Card title="Load-balanced services" action={
        <span className="fx-row-actions">
          <Button size="sm" onClick={() => setApply(SERVICE_TEMPLATE)}><Plus size={14} /> Apply service</Button>
          <Button size="sm" busy={busy === 'health'} onClick={() => run('health', () => api.post('/v1/network/services/health/reconcile'), 'Health probes run', health.refresh)}>Probe now</Button>
          <Button size="sm" busy={busy === 'gc'} onClick={() => run('gc', () => api.post('/v1/network/services/conntrack/gc'), 'Connection table cleaned')}>Clean connections</Button>
        </span>
      }>
        {h ? <p className="fx-muted" style={{ marginTop: 0 }}>{h.xdp_acceleration ? 'XDP acceleration on' : 'XDP acceleration off'}{h.map_tier ? ` · ${h.map_tier} maps` : ''}{h.north_south_interfaces?.length ? ` · north-south on ${h.north_south_interfaces.join(', ')}` : ''}</p> : null}
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? (
          <EmptyState title="No services" body="A service gives a virtual IP and port to a set of backends, balanced in the datapath." />
        ) : (
          <Table head={['Service', 'Address', 'Mode', 'Backends', 'Health', '']}>
            {rows.map((s) => {
              const hl = serviceHealth(s.name, report);
              return (
                <tr key={s.name}>
                  <td className="vm-name">{s.name}</td>
                  <td className="fx-mono">{s.vip}:{s.port}/{s.protocol}</td>
                  <td className="fx-muted">{[s.mode, s.exposure, s.algorithm].filter(Boolean).join(' · ') || '—'}{s.advertise ? ' · advertised' : ''}</td>
                  <td className="fx-muted">{(s.backends ?? []).filter((b) => b.enabled !== false && (b.state ?? 'ready') === 'ready').length} / {s.backends?.length ?? 0} ready</td>
                  <td><Pill tone={HEALTH_TONE[hl]}>{hl === 'unknown' ? 'no probes' : hl}</Pill></td>
                  <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Delete ${s.name}`} onClick={() => setDel(s.name)}><Trash2 size={14} /></Button></span></td>
                </tr>
              );
            })}
          </Table>
        )}
      </Card>
      <Sheet open={apply != null} title="Apply a service" onClose={() => setApply(null)}>
        <p className="fx-muted">Paste a service as JSON. It is created, or replaced if the name exists. Address family, ports and backends are checked by the daemon.</p>
        <textarea className="fx-input fx-mono" rows={14} aria-label="Service JSON" spellCheck={false} value={apply ?? ''} onChange={(e) => setApply(e.target.value)} />
        {parsed && !parsed.ok ? <p className="fx-error" role="alert">{parsed.error}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'apply'} disabled={!parsed?.ok} onClick={async () => {
          if (parsed?.ok && (await run('apply', () => api.post('/v1/network/services', parsed.value), `${String(parsed.value.name)} applied`, list.refresh))) setApply(null);
        }}>Apply</Button><Button onClick={() => setApply(null)}>Cancel</Button></div>
      </Sheet>
      {report.some((b) => !b.healthy) ? (
        <div className="fx-section"><Card title="Failing backends"><GenericTable rows={report.filter((b) => !b.healthy) as unknown as Record<string, unknown>[]} empty="" /></Card></div>
      ) : null}
      <Confirm what={del} busy={busy === 'del'} onClose={() => setDel(null)} onConfirm={async () => {
        if (del && (await run('del', () => api.del(`/v1/network/services/${encodeURIComponent(del)}`), `${del} deleted`, list.refresh))) setDel(null);
      }} />
    </>
  );
}

// ---- Flows ----

type FlowRow = { service?: string; source?: string; destination?: string; destination_port?: number; protocol?: number; verdict?: string; reason?: string; packets?: number; bytes?: number };

export function FlowsTab() {
  const [limit, setLimit] = useState('256');
  const [only, setOnly] = useState<'all' | 'drops'>('all');
  const flows = usePoll(async () => rowsOf(await api.get<unknown>(`/v1/network/services/flows?limit=${limit}`)) as FlowRow[], 4000, [limit]);
  const all = flows.data ?? [];
  const rows = only === 'drops' ? all.filter((f) => f.verdict && f.verdict !== 'allow') : all;
  return (
    <Card title="Service flows" action={
      <span className="fx-row-actions">
        <Segmented label="Verdict filter" value={only} onChange={setOnly} options={[{ value: 'all', label: 'All' }, { value: 'drops', label: 'Drops' }]} />
        <Segmented label="Rows" value={limit} onChange={setLimit} options={[{ value: '64', label: '64' }, { value: '256', label: '256' }, { value: '1024', label: '1024' }]} />
      </span>
    }>
      {flows.loading && !all.length ? <Skeleton h={120} /> : rows.length === 0 ? (
        <EmptyState title={only === 'drops' ? 'No drops' : 'No flows yet'} body="Allowed flows are sampled per service; drops are always recorded." />
      ) : (
        <Table head={['Service', 'Source', 'Destination', 'Verdict', 'Packets', 'Bytes']}>
          {rows.slice(0, 200).map((f, i) => (
            <tr key={i}>
              <td className="vm-name">{f.service || '—'}</td>
              <td className="fx-mono">{f.source}</td>
              <td className="fx-mono">{f.destination}:{f.destination_port}/{protoName(f.protocol)}</td>
              <td><Pill tone={f.verdict === 'allow' ? 'ok' : 'bad'}>{f.verdict || '—'}</Pill>{f.reason ? <span className="fx-muted"> {f.reason}</span> : null}</td>
              <td className="fx-muted">{(f.packets ?? 0).toLocaleString()}</td>
              <td className="fx-muted">{(f.bytes ?? 0).toLocaleString()}</td>
            </tr>
          ))}
        </Table>
      )}
    </Card>
  );
}

// ---- Cilium policies ----

export function PoliciesTab({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/network/cnp')), 8000);
  const { busy, run } = useAct(notify);
  const [apply, setApply] = useState<string | null>(null);
  const [del, setDel] = useState<string | null>(null);
  const rows = list.data ?? [];
  const parsed = apply == null ? null : parseObject(apply, ['metadata', 'spec']);
  const name = parsed?.ok ? cnpName(parsed.value) : '';
  return (
    <>
      <Card title="Cilium network policies" action={<Button size="sm" onClick={() => setApply(CNP_TEMPLATE)}><Plus size={14} /> Apply policy</Button>}>
        {list.loading && !rows.length ? <Skeleton h={100} /> : rows.length === 0 ? (
          <EmptyState title="No Cilium policies" body="Policies in CiliumNetworkPolicy form are compiled to the same datapath as security groups." />
        ) : (
          <Table head={['Policy', 'Namespace', 'Selects', '']}>
            {rows.map((p, i) => {
              const n = cnpName(p);
              const meta = (p.metadata ?? {}) as { namespace?: string };
              return (
                <tr key={n || i}>
                  <td className="vm-name">{n || '—'}</td>
                  <td className="fx-muted">{meta.namespace || 'default'}</td>
                  <td className="fx-mono fx-muted">{JSON.stringify((p.spec as { endpointSelector?: unknown } | undefined)?.endpointSelector ?? {}).slice(0, 60)}</td>
                  <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Delete ${n}`} onClick={() => setDel(n)}><Trash2 size={14} /></Button></span></td>
                </tr>
              );
            })}
          </Table>
        )}
      </Card>
      <Sheet open={apply != null} title="Apply a policy" onClose={() => setApply(null)}>
        <p className="fx-muted">Paste a CiliumNetworkPolicy as JSON. A policy with the same name is replaced.</p>
        <textarea className="fx-input fx-mono" rows={14} aria-label="Policy JSON" spellCheck={false} value={apply ?? ''} onChange={(e) => setApply(e.target.value)} />
        {parsed && !parsed.ok ? <p className="fx-error" role="alert">{parsed.error}</p> : parsed?.ok && !name ? <p className="fx-error" role="alert">metadata.name is required.</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'apply'} disabled={!parsed?.ok || !name} onClick={async () => {
          if (parsed?.ok && (await run('apply', () => api.post('/v1/network/cnp', parsed.value), `${name} applied`, list.refresh))) setApply(null);
        }}>Apply</Button><Button onClick={() => setApply(null)}>Cancel</Button></div>
      </Sheet>
      <Confirm what={del} busy={busy === 'del'} onClose={() => setDel(null)} onConfirm={async () => {
        if (del && (await run('del', () => api.del(`/v1/network/cnp/${encodeURIComponent(del)}`), `${del} deleted`, list.refresh))) setDel(null);
      }} />
    </>
  );
}

// ---- Inspect: identities, endpoints, address pool, health ----

function Facts({ path, empty }: { path: string; empty: string }) {
  const d = usePoll(() => api.get<unknown>(path).catch(() => null), 10000);
  const rows = flatten(d.data);
  if (d.loading && !d.data) return <Skeleton h={80} />;
  return rows.length ? <KV rows={rows} /> : <p className="fx-muted">{empty}</p>;
}

function Rows({ path, empty }: { path: string; empty: string }) {
  const d = usePoll(async () => rowsOf(await api.get<unknown>(path).catch(() => null)), 10000);
  if (d.loading && !d.data) return <Skeleton h={80} />;
  return <GenericTable rows={d.data ?? []} empty={empty} />;
}

export function InspectTab() {
  return (
    <div className="fx-grid cols-2">
      <Card title="Datapath health"><Facts path="/v1/network/health" empty="The datapath reports no health data." /></Card>
      <Card title="Address pool"><Facts path="/v1/network/ipam" empty="No address pool reported." /></Card>
      <Card title="Identities"><Rows path="/v1/network/identities" empty="No identities allocated." /></Card>
      <Card title="Endpoints"><Rows path="/v1/network/endpoints" empty="No endpoints." /></Card>
      <Card title="IP cache"><Rows path="/v1/network/ipcache" empty="The IP cache is empty." /></Card>
      <Card title="DNS"><Rows path="/v1/netobs/dns" empty="No DNS queries seen." /></Card>
    </div>
  );
}
