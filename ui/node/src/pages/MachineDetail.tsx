import { type ReactNode, useEffect, useMemo, useRef, useState } from 'react';
import { ChevronLeft, Copy, Pause, Play, RotateCw, Square, Trash2 } from 'lucide-react';
import { Button, Card, Field, KV, Meter, Pill, Sheet, Skeleton, Sparkline, Tabs, ago, api, fmtMiB, navigate, statusTone, streamEvents, usePoll, type Vm } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { actionsFor } from './Machines';
import { ConsoleTab } from './ConsoleTab';
import { hasDisplayTab } from '../lib/connect';
import { ConnectTab } from './ConnectTab';
import { DisplayTab } from './DisplayTab';
import { GuestTab } from './GuestTab';
import { MigrationTab, NetworkTab, ProtectTab, StorageTab } from './tabs';

type Tab = 'overview' | 'console' | 'connect' | 'display' | 'guest' | 'network' | 'storage' | 'protect' | 'migration' | 'logs' | 'events';
interface Metrics {
  cpu_usage_percent?: number;
  memory_usage_bytes?: number;
  disk_read_bytes?: number;
  disk_write_bytes?: number;
}
interface Ev {
  at: number;
  name: string;
  detail: string;
}

const MAX_POINTS = 60;
export const pushPoint = (arr: number[], v: number, max = MAX_POINTS): number[] => [...arr, v].slice(-max);

export function MachineDetail({ id, notify }: { id: string; notify: Notify }) {
  const [tab, setTab] = useState<Tab>('overview');
  const vm = usePoll(() => api.get<Vm>(`/v1/vms/${id}`), 3000, [id]);
  const [busy, setBusy] = useState('');
  const [clone, setClone] = useState(false);
  const [cloneName, setCloneName] = useState('');
  const [del, setDel] = useState(false);

  const v = vm.data;
  const act = async (op: string) => {
    setBusy(op);
    try {
      await api.post(`/v1/vms/${id}/${op}`);
      notify(`${v?.name ?? 'Machine'}: ${op} requested`);
      await vm.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const doClone = async () => {
    setBusy('clone');
    try {
      const c = await api.post<Vm>(`/v1/vms/${id}/clone`, { name: cloneName.trim() });
      notify(`Cloned to ${cloneName}`);
      setClone(false);
      if (c?.id) navigate(`/machines/${c.id}`);
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const doDelete = async () => {
    setBusy('delete');
    try {
      await api.del(`/v1/vms/${id}`);
      notify(`${v?.name} deleted`);
      navigate('/machines');
    } catch (e) {
      notify((e as Error).message, 'bad');
      setBusy('');
    }
  };

  if (vm.error && !v) return <><a className="fx-back" href="#/machines"><ChevronLeft size={16} />Machines</a><Card><p className="fx-error">{vm.error.message}</p></Card></>;
  if (!v) return <Skeleton h={200} />;
  const icon = { start: <Play size={14} />, resume: <Play size={14} />, stop: <Square size={14} />, restart: <RotateCw size={14} />, pause: <Pause size={14} /> } as Record<string, ReactNode>;

  return (
    <>
      <a className="fx-back" href="#/machines"><ChevronLeft size={16} />Machines</a>
      <div className="fx-detail-head">
        <h1>{v.name} <Pill tone={statusTone(v.status)}>{v.status}</Pill></h1>
        <div className="fx-actions">
          {actionsFor(v.status).map((op) => (
            <Button key={op} variant={op === 'start' || op === 'resume' ? 'primary' : 'secondary'} busy={busy === op} onClick={() => act(op)}>{icon[op]}{op}</Button>
          ))}
          <Button onClick={() => { setCloneName(`${v.name}-clone`); setClone(true); }}><Copy size={14} />Clone</Button>
          <Button variant="ghost" onClick={() => setDel(true)} aria-label="Delete machine"><Trash2 size={14} /></Button>
        </div>
      </div>
      {v.error ? <div className="fx-error" role="alert">{v.error}</div> : null}
      <Tabs value={tab} onChange={setTab} tabs={[{ value: 'overview', label: 'Overview' }, { value: 'console', label: 'Console' }, { value: 'connect', label: 'Connect' }, ...(hasDisplayTab(v) ? [{ value: 'display' as const, label: 'Display' }] : []), { value: 'guest', label: 'Guest' }, { value: 'network', label: 'Network' }, { value: 'storage', label: 'Storage' }, { value: 'protect', label: 'Protect' }, { value: 'migration', label: 'Migration' }, { value: 'logs', label: 'Logs' }, { value: 'events', label: 'Events' }]} />
      {tab === 'overview' && <Overview vm={v} />}
      {tab === 'console' && <ConsoleTab id={id} running={v.status === 'running'} />}
      {tab === 'connect' && <ConnectTab vm={v} notify={notify} goConsole={() => setTab('console')} />}
      {tab === 'display' && <DisplayTab vm={v} notify={notify} />}
      {tab === 'guest' && <GuestTab id={id} running={v.status === 'running'} notify={notify} />}
      {tab === 'network' && <NetworkTab id={id} notify={notify} />}
      {tab === 'storage' && <StorageTab id={id} notify={notify} />}
      {tab === 'protect' && <ProtectTab id={id} name={v.name} running={v.status === 'running'} notify={notify} />}
      {tab === 'migration' && <MigrationTab id={id} running={v.status === 'running'} notify={notify} />}
      {tab === 'logs' && <Logs id={id} />}
      {tab === 'events' && <Events id={id} name={v.name} />}

      <Sheet open={clone} title="Clone machine" onClose={() => setClone(false)}>
        <p className="fx-muted">Copies the stopped VM's disk into a new machine with a fresh MAC address.</p>
        <Field label="New name">{(i) => <input id={i} className="fx-input" autoFocus value={cloneName} onChange={(e) => setCloneName(e.target.value)} />}</Field>
        <Button variant="primary" busy={busy === 'clone'} disabled={!cloneName.trim()} onClick={doClone}>Clone</Button>
      </Sheet>
      <Sheet open={del} title="Delete machine" onClose={() => setDel(false)}>
        <p>Delete <b>{v.name}</b>? Its disk is removed and this cannot be undone.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'delete'} onClick={doDelete}>Delete</Button><Button onClick={() => setDel(false)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}

function Overview({ vm }: { vm: Vm }) {
  const running = vm.status === 'running';
  const cpu = useRef<number[]>([]);
  const mem = useRef<number[]>([]);
  const stats = usePoll(async () => (running ? api.get<Metrics>(`/v1/vms/${vm.id}/stats`).catch(() => null) : null), 3000, [vm.id, running]);
  const m = stats.data;
  useMemo(() => {
    if (m?.cpu_usage_percent != null) cpu.current = pushPoint(cpu.current, m.cpu_usage_percent);
    if (m?.memory_usage_bytes != null) mem.current = pushPoint(mem.current, m.memory_usage_bytes / 1048576);
  }, [m]);
  const memLimit = vm.request?.memory_mib ?? 0;
  const memNow = m?.memory_usage_bytes != null ? m.memory_usage_bytes / 1048576 : 0;
  return (
    <div className="fx-grid cols-2">
      <Card title="CPU">
        <div className="fx-stat-num">{m?.cpu_usage_percent != null ? `${m.cpu_usage_percent.toFixed(1)}%` : '—'}</div>
        <Sparkline values={cpu.current} max={100} />
        <div className="fx-stat-label">{running ? `${vm.request?.vcpus} vCPU` : 'Start the machine to see live usage'}</div>
      </Card>
      <Card title="Memory">
        <div className="fx-stat-num">{m?.memory_usage_bytes != null ? fmtMiB(Math.round(memNow)) : '—'}</div>
        <Sparkline values={mem.current} max={memLimit || undefined} tone="ok" />
        {memLimit ? <Meter value={(memNow / memLimit) * 100} label="Memory used" /> : null}
        <div className="fx-stat-label">of {fmtMiB(memLimit)}</div>
      </Card>
      <Card title="Details" className="fx-span">
        <KV rows={[['ID', <span className="fx-mono">{vm.id}</span>], ['Backend', vm.backend], ['Guest IP', <span className="fx-mono">{vm.guest_ip || '—'}</span>], ['vCPUs', vm.request?.vcpus], ['Memory', fmtMiB(memLimit)], ['Disk I/O', m ? `${fmtMiB(Math.round((m.disk_read_bytes ?? 0) / 1048576))} read · ${fmtMiB(Math.round((m.disk_write_bytes ?? 0) / 1048576))} written` : '—']]} />
      </Card>
      <Card title="Labels" className="fx-span">
        {vm.labels && Object.keys(vm.labels).length ? <KV rows={Object.entries(vm.labels)} /> : <p className="fx-muted">No labels.</p>}
      </Card>
    </div>
  );
}

function Logs({ id }: { id: string }) {
  const [follow, setFollow] = useState(true);
  const box = useRef<HTMLPreElement>(null);
  const logs = usePoll(() => api.get<string>(`/v1/vms/${id}/logs?lines=300`, { text: true }).catch((e) => `Could not read logs: ${(e as Error).message}`), follow ? 2500 : 3_600_000, [id, follow]);
  useEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [logs.data, follow]);
  return (
    <Card title="Console log" action={<Button size="sm" variant={follow ? 'primary' : 'secondary'} onClick={() => setFollow(!follow)}>{follow ? 'Following' : 'Paused'}</Button>}>
      <pre className="fx-code" ref={box} style={{ minHeight: 280 }}>{logs.data || 'No output yet.'}</pre>
    </Card>
  );
}

function Events({ id, name }: { id: string; name: string }) {
  const [events, setEvents] = useState<Ev[]>([]);
  useEffect(() => {
    const ac = new AbortController();
    streamEvents('/v1/events/stream', (m) => {
      const d = JSON.stringify(m.data);
      if (!d.includes(id) && !d.includes(name)) return;
      setEvents((e) => [{ at: Date.now(), name: m.event, detail: d.length > 160 ? d.slice(0, 160) + '…' : d }, ...e].slice(0, 100));
    }, ac.signal);
    return () => ac.abort();
  }, [id, name]);
  return (
    <Card title="Live events">
      {events.length === 0 ? <p className="fx-muted">Listening… events for this machine appear here as they happen.</p> : (
        <ul className="fx-feed">{events.map((e, i) => <li key={e.at + '-' + i}><Pill tone="info" dot={false} raw>{e.name}</Pill><span className="fx-mono fx-muted">{e.detail}</span><time>{ago(e.at)}</time></li>)}</ul>
      )}
    </Card>
  );
}
