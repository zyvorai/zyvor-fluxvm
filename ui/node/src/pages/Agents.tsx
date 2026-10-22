import { useEffect, useState } from 'react';
import { ArrowUp, Folder, FileText, Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, KV, Pill, Segmented, Sheet, Skeleton, Tabs, ago, api, left, navigate, streamEvents, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { baseName, download, fromBase64, isAbsPath } from '../lib/exec';
import { TerminalView } from './TerminalView';

interface Checkpoint { tag: string; memory?: boolean; created_at?: string }
interface Session {
  id: string;
  agent: string;
  state: string;
  created_at?: string;
  expires_at?: string | null;
  checkpoints?: Checkpoint[];
  policy?: { preset?: string | null; offline?: boolean };
  workspace?: { path?: string; git?: string | null };
  capabilities?: { pty?: boolean; snapshot?: boolean; fork?: boolean; memory_checkpoint?: boolean };
  revision?: number;
}
const stateTone = (s: string): 'ok' | 'warn' | 'bad' | 'idle' => (s === 'running' ? 'ok' : s === 'expired' ? 'warn' : s === 'gone' ? 'bad' : 'idle');
type Preset = 'offline' | 'developer-safe' | 'open';

export function AgentsPage({ notify }: { notify: Notify }) {
  const list = usePoll(async () => (await api.get<{ items: Session[] }>('/v1/agents/sessions')).items ?? [], 4000);
  const [open, setOpen] = useState(false);
  const [agent, setAgent] = useState('custom');
  const [preset, setPreset] = useState<Preset>('developer-safe');
  const [git, setGit] = useState('');
  const [ttl, setTtl] = useState('3600');
  const [busy, setBusy] = useState('');
  const rows = [...(list.data ?? [])].sort((a, b) => (b.created_at ?? '').localeCompare(a.created_at ?? ''));
  const make = async () => {
    setBusy('create');
    try {
      const s = await api.post<Session>('/v1/agents/sessions', { agent: agent.trim() || 'custom', policy: { preset }, ...(git.trim() ? { workspace: { git: git.trim() } } : {}), ...(Number(ttl) > 0 ? { ttl_seconds: Number(ttl) } : {}) });
      notify('Session started');
      setOpen(false);
      await list.refresh();
      navigate(`/agents/${s.id}`);
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const remove = async (s: Session) => {
    setBusy(s.id);
    try {
      await api.del(`/v1/agents/sessions/${s.id}`);
      notify('Session deleted');
      await list.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  return (
    <>
      <Hero eyebrow="Agent sessions" title="Give agents a machine," accent="keep the keys." lede="Confined workspaces with approvals, checkpoints and a full activity ledger.">
        <Button variant="primary" size="lg" onClick={() => setOpen(true)}><Plus size={18} />New session</Button>
      </Hero>
      <Card>
        {list.loading && !rows.length ? <Skeleton h={140} /> : rows.length === 0 ? <EmptyState title="No agent sessions" body="Start one to give an agent an isolated workspace with a policy." action={<Button variant="primary" onClick={() => setOpen(true)}>Start a session</Button>} /> : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th>Agent</th><th>State</th><th>Policy</th><th>Started</th><th>Expires</th><th /></tr></thead>
            <tbody>{rows.map((s) => (
              <tr key={s.id}>
                <td><a className="vm-name" href={`#/agents/${s.id}`}>{s.agent}</a><div className="fx-muted fx-mono" style={{ fontSize: 12 }}>{s.id.slice(0, 8)}</div></td>
                <td><Pill tone={stateTone(s.state)}>{s.state}</Pill></td>
                <td className="fx-muted">{s.policy?.preset ?? (s.policy?.offline ? 'offline' : 'custom')}</td>
                <td className="fx-muted">{s.created_at ? ago(s.created_at) : '—'}</td>
                <td className="fx-muted">{left(s.expires_at)}</td>
                <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label="Delete session" busy={busy === s.id} onClick={() => remove(s)}><Trash2 size={14} /></Button></span></td>
              </tr>))}</tbody></table></div>
        )}
      </Card>
      <Sheet open={open} title="New agent session" onClose={() => setOpen(false)}>
        <Field label="Agent" hint="A label, e.g. claude-code.">{(i) => <input id={i} className="fx-input" autoFocus value={agent} onChange={(e) => setAgent(e.target.value)} />}</Field>
        <Field label="Policy">{() => <Segmented label="Policy preset" value={preset} onChange={setPreset} options={[{ value: 'offline', label: 'Offline' }, { value: 'developer-safe', label: 'Developer-safe' }, { value: 'open', label: 'Open' }]} />}</Field>
        <p className="fx-muted" style={{ marginTop: -8 }}>{preset === 'offline' ? 'No network access.' : preset === 'open' ? 'Unrestricted network. Use only for trusted work.' : 'Allow-listed network and guarded commands.'}</p>
        <Field label="Git repository" hint="Optional: cloned into /workspace.">{(i) => <input id={i} className="fx-input" value={git} onChange={(e) => setGit(e.target.value)} placeholder="https://github.com/owner/repo" />}</Field>
        <Field label="Time to live (seconds)">{(i) => <input id={i} className="fx-input" inputMode="numeric" value={ttl} onChange={(e) => setTtl(e.target.value.replace(/\D/g, ''))} />}</Field>
        <Button variant="primary" busy={busy === 'create'} onClick={make}>Start session</Button>
      </Sheet>
    </>
  );
}

/* ---------------------------------------------------------------- detail */
type ATab = 'terminal' | 'activity' | 'approvals' | 'jobs' | 'files' | 'checkpoints';
export function AgentDetail({ id, notify }: { id: string; notify: Notify }) {
  const [tab, setTab] = useState<ATab>('terminal');
  const s = usePoll(() => api.get<Session>(`/v1/agents/sessions/${id}`), 4000, [id]);
  const v = s.data;
  if (s.error && !v) return <><a className="fx-back" href="#/agents">‹ Agent sessions</a><Card><p className="fx-error">{s.error.message}</p></Card></>;
  if (!v) return <Skeleton h={160} />;
  const live = v.state === 'running';
  return (
    <>
      <a className="fx-back" href="#/agents">‹ Agent sessions</a>
      <div className="fx-detail-head"><h1>{v.agent} <Pill tone={stateTone(v.state)}>{v.state}</Pill></h1><span className="fx-muted fx-mono">{v.id}</span></div>
      <Tabs value={tab} onChange={setTab} tabs={[{ value: 'terminal', label: 'Terminal' }, { value: 'activity', label: 'Activity' }, { value: 'approvals', label: 'Approvals' }, { value: 'jobs', label: 'Jobs' }, { value: 'files', label: 'Files' }, { value: 'checkpoints', label: 'Checkpoints' }]} />
      {tab === 'terminal' && <Card title="Terminal">{live ? <TerminalView connect={async (c, r) => `/console/pty/${(await api.post<{ ticket: string }>(`/v1/agents/sessions/${id}/pty-ticket`)).ticket}?cols=${c}&rows=${r}`} resize="nested" hint="A shell in the session's workspace." /> : <p className="fx-muted">The session is not running.</p>}</Card>}
      {tab === 'activity' && <Activity id={id} />}
      {tab === 'approvals' && <Approvals id={id} notify={notify} />}
      {tab === 'jobs' && <Jobs id={id} live={live} notify={notify} />}
      {tab === 'files' && <Files id={id} live={live} notify={notify} />}
      {tab === 'checkpoints' && <Checkpoints id={id} session={v} notify={notify} onChange={() => void s.refresh()} />}
    </>
  );
}

interface Entry { ts?: string; kind?: string; detail?: unknown; event?: string }
function Activity({ id }: { id: string }) {
  const [items, setItems] = useState<Entry[]>([]);
  const [live, setLive] = useState(false);
  useEffect(() => {
    const ac = new AbortController();
    api.get<{ ledger: Entry[] }>(`/v1/agents/sessions/${id}/events`).then((r) => setItems([...(r.ledger ?? [])].reverse().slice(0, 200))).catch(() => undefined);
    streamEvents(`/v1/agents/sessions/${id}/events/stream`, (m) => {
      setLive(true);
      if (m.event === 'ledger' && m.data && typeof m.data === 'object') setItems((x) => [m.data as Entry, ...x].slice(0, 200));
    }, ac.signal).finally(() => setLive(false));
    return () => ac.abort();
  }, [id]);
  return (
    <Card title="Ledger" action={<span className={`fx-live ${live ? '' : 'is-off'}`}><i />{live ? 'Live' : 'Offline'}</span>}>
      {items.length === 0 ? <p className="fx-muted">Nothing has happened yet. Every command, file write and approval is recorded here.</p> : (
        <ul className="fx-feed">{items.map((e, i) => <li key={(e.ts ?? '') + i}><Pill tone={String(e.kind).includes('refused') || String(e.kind).includes('denied') ? 'bad' : 'info'} dot={false} raw>{e.kind ?? e.event}</Pill><span className="fx-mono fx-muted">{JSON.stringify(e.detail ?? {}).slice(0, 140)}</span><time>{e.ts ? ago(e.ts) : ''}</time></li>)}</ul>
      )}
    </Card>
  );
}

interface Approval { id: string; kind: string; summary: string; matched?: string; state: string; requested_at?: string; expires_at?: string }
function Approvals({ id, notify }: { id: string; notify: Notify }) {
  const list = usePoll(async () => (await api.get<{ items: Approval[] }>(`/v1/agents/sessions/${id}/approvals`)).items ?? [], 3000, [id]);
  const [busy, setBusy] = useState('');
  const decide = async (a: Approval, verb: 'approve' | 'deny') => {
    setBusy(a.id + verb);
    try {
      await api.post(`/v1/agents/sessions/${id}/approvals/${a.id}/${verb}`, {});
      notify(verb === 'approve' ? 'Approved' : 'Denied');
      await list.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const rows = list.data ?? [];
  const tone = (s: string) => (s === 'pending' ? 'warn' : s === 'approved' || s === 'consumed' ? 'ok' : s === 'denied' ? 'bad' : 'idle') as 'warn' | 'ok' | 'bad' | 'idle';
  return (
    <Card title="Approvals">
      {rows.length === 0 ? <p className="fx-muted">No approvals requested. When the agent runs a command that matches an “ask” rule, it waits here for you.</p> : (
        <div className="fx-table-wrap"><table className="fx-table">
          <thead><tr><th>Request</th><th>Rule</th><th>State</th><th>Asked</th><th /></tr></thead>
          <tbody>{rows.map((a) => (
            <tr key={a.id}>
              <td><span className="fx-mono">{a.summary}</span><div className="fx-muted" style={{ fontSize: 12 }}>{a.kind}</div></td>
              <td className="fx-mono fx-muted">{a.matched ?? '—'}</td>
              <td><Pill tone={tone(a.state)}>{a.state}</Pill></td>
              <td className="fx-muted">{a.requested_at ? ago(a.requested_at) : '—'}</td>
              <td>{a.state === 'pending' ? <span className="fx-row-actions"><Button size="sm" variant="primary" busy={busy === a.id + 'approve'} onClick={() => decide(a, 'approve')}>Approve</Button><Button size="sm" busy={busy === a.id + 'deny'} onClick={() => decide(a, 'deny')}>Deny</Button></span> : null}</td>
            </tr>))}</tbody></table></div>
      )}
    </Card>
  );
}

interface Job { job: string; command: string; state: string; exit_code?: number | null; started_at?: string }
function Jobs({ id, live, notify }: { id: string; live: boolean; notify: Notify }) {
  const list = usePoll(async () => (await api.get<{ items: Job[] }>(`/v1/agents/sessions/${id}/jobs`)).items ?? [], 3000, [id]);
  const [cmd, setCmd] = useState('');
  const [sel, setSel] = useState<string | null>(null);
  const [busy, setBusy] = useState('');
  const start = async () => {
    setBusy('start');
    try {
      const r = await api.post<{ job: string }>(`/v1/agents/sessions/${id}/jobs`, { command: cmd.trim() });
      setCmd('');
      setSel(r.job);
      await list.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const kill = async (j: Job) => {
    try {
      await api.del(`/v1/agents/sessions/${id}/jobs/${j.job}`);
      notify('Job signalled');
      await list.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    }
  };
  const tone = (s: string) => (s === 'running' ? 'info' : s === 'exited' ? 'ok' : 'warn') as 'info' | 'ok' | 'warn';
  return (
    <div className="fx-grid">
      <Card title="Background jobs">
        <div className="fx-rule-form" style={{ gridTemplateColumns: '1fr auto' }}>
          <input className="fx-input fx-mono" aria-label="Command" placeholder="npm run dev" disabled={!live} value={cmd} onChange={(e) => setCmd(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && cmd.trim() && start()} />
          <Button variant="primary" busy={busy === 'start'} disabled={!live || !cmd.trim()} onClick={start}>Start job</Button>
        </div>
        {(list.data ?? []).length === 0 ? <p className="fx-muted" style={{ marginTop: 12 }}>No jobs.</p> : (
          <div className="fx-table-wrap" style={{ marginTop: 12 }}><table className="fx-table">
            <thead><tr><th>Command</th><th>State</th><th>Exit</th><th /></tr></thead>
            <tbody>{(list.data ?? []).map((j) => (
              <tr key={j.job} className="is-clickable" onClick={() => setSel(j.job)}>
                <td className="fx-mono">{j.command}</td><td><Pill tone={tone(j.state)}>{j.state}</Pill></td><td className="fx-muted">{j.exit_code ?? '—'}</td>
                <td><span className="fx-row-actions">{j.state === 'running' ? <Button size="sm" onClick={(e) => { e.stopPropagation(); void kill(j); }}>Stop</Button> : null}</span></td>
              </tr>))}</tbody></table></div>
        )}
      </Card>
      {sel ? <JobOutput id={id} job={sel} /> : null}
    </div>
  );
}

function JobOutput({ id, job }: { id: string; job: string }) {
  const [out, setOut] = useState('');
  const [err, setErr] = useState('');
  const [state, setState] = useState('');
  useEffect(() => {
    let off = 0;
    let eoff = 0;
    let stop = false;
    setOut('');
    setErr('');
    const tick = async () => {
      while (!stop) {
        try {
          const r = await api.get<{ output?: string; next_offset?: number; stderr?: string; stderr_next_offset?: number; state?: string; eof?: boolean; more?: boolean }>(`/v1/agents/sessions/${id}/jobs/${job}?offset=${off}&stderr_offset=${eoff}&wait_ms=5000`);
          if (r.output) setOut((o) => o + r.output);
          if (r.stderr) setErr((o) => o + r.stderr);
          off = r.next_offset ?? off;
          eoff = r.stderr_next_offset ?? eoff;
          setState(r.state ?? '');
          if (r.eof || (r.state && r.state !== 'running' && !r.more)) return;
        } catch {
          return;
        }
      }
    };
    void tick();
    return () => { stop = true; };
  }, [id, job]);
  return <Card title={`Output ${state ? `· ${state}` : ''}`}><pre className="fx-code" style={{ minHeight: 160 }}>{out || 'Waiting for output…'}</pre>{err ? <pre className="fx-code fx-code-err" style={{ marginTop: 10 }}>{err}</pre> : null}</Card>;
}

interface FEntry { name: string; type: string; size?: number; mtime?: number | null }
function Files({ id, live, notify }: { id: string; live: boolean; notify: Notify }) {
  const [path, setPath] = useState('/workspace');
  const [text, setText] = useState('/workspace');
  const list = usePoll(async () => api.get<{ entries?: FEntry[] }>(`/v1/agents/sessions/${id}/files?path=${encodeURIComponent(path)}`).catch((e: Error) => ({ entries: [] as FEntry[], error: e.message })), 8000, [id, path]);
  const data = list.data as { entries?: FEntry[]; error?: string } | null;
  const go = (p: string) => { setPath(p); setText(p); };
  const join = (n: string) => (path === '/' ? '' : path.replace(/\/$/, '')) + '/' + n;
  const up = () => go(path.replace(/\/[^/]+\/?$/, '') || '/');
  const open = async (e: FEntry) => {
    if (e.type === 'dir') return go(join(e.name));
    try {
      const r = await api.get<{ content_base64: string }>(`/v1/agents/sessions/${id}/files/content?path=${encodeURIComponent(join(e.name))}`);
      download(baseName(e.name), fromBase64(r.content_base64));
    } catch (x) {
      notify((x as Error).message, 'bad');
    }
  };
  if (!live) return <Card title="Files"><p className="fx-muted">The session is not running.</p></Card>;
  return (
    <Card title="Files" action={<Button size="sm" onClick={up}><ArrowUp size={14} />Up</Button>}>
      <input className="fx-input fx-mono" aria-label="Path" value={text} onChange={(e) => setText(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && isAbsPath(text) && go(text)} style={{ marginBottom: 12 }} />
      {data?.error ? <p className="fx-muted">{data.error}</p> : (data?.entries ?? []).length === 0 ? <p className="fx-muted">Empty.</p> : (
        <div className="fx-table-wrap"><table className="fx-table"><tbody>{(data?.entries ?? []).map((e) => (
          <tr key={e.name} className="is-clickable" onClick={() => open(e)}>
            <td>{e.type === 'dir' ? <Folder size={14} style={{ verticalAlign: -2, marginRight: 8 }} /> : <FileText size={14} style={{ verticalAlign: -2, marginRight: 8 }} />}{e.name}</td>
            <td className="fx-muted">{e.type === 'dir' ? '' : e.size != null ? `${e.size.toLocaleString()} B` : ''}</td>
          </tr>))}</tbody></table></div>
      )}
    </Card>
  );
}

function Checkpoints({ id, session, notify, onChange }: { id: string; session: Session; notify: Notify; onChange: () => void }) {
  const [tag, setTag] = useState('');
  const [memory, setMemory] = useState(true);
  const [busy, setBusy] = useState('');
  const act = async (key: string, fn: () => Promise<unknown>, ok: string) => {
    setBusy(key);
    try {
      await fn();
      notify(ok);
      onChange();
      return true;
    } catch (e) {
      notify((e as Error).message, 'bad');
      return false;
    } finally {
      setBusy('');
    }
  };
  const cps = session.checkpoints ?? [];
  return (
    <div className="fx-grid">
      <Card title="Take a checkpoint">
        <p className="fx-muted">Save the whole session so you can return to it, or fork copies from it.</p>
        <Field label="Tag" hint="Optional; one is generated otherwise.">{(i) => <input id={i} className="fx-input" value={tag} onChange={(e) => setTag(e.target.value)} />}</Field>
        <label className="fx-check"><input type="checkbox" checked={memory} onChange={(e) => setMemory(e.target.checked)} /> Include memory (resume exactly where it was)</label>
        <div style={{ marginTop: 14 }}><Button variant="primary" busy={busy === 'cp'} onClick={async () => (await act('cp', () => api.post(`/v1/agents/sessions/${id}/checkpoint`, { ...(tag.trim() ? { tag: tag.trim() } : {}), memory }), 'Checkpoint saved')) && setTag('')}>Checkpoint</Button></div>
      </Card>
      <Card title="Checkpoints" action={<Button size="sm" busy={busy === 'fork'} onClick={() => act('fork', () => api.post(`/v1/agents/sessions/${id}/fork`, { count: 1 }), 'Forked a copy')}>Fork session</Button>}>
        {cps.length === 0 ? <p className="fx-muted">No checkpoints yet.</p> : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th>Tag</th><th>Memory</th><th>Taken</th><th /></tr></thead>
            <tbody>{cps.map((c) => (
              <tr key={c.tag}><td>{c.tag}</td><td className="fx-muted">{c.memory ? 'yes' : 'no'}</td><td className="fx-muted">{c.created_at ? ago(c.created_at) : '—'}</td>
                <td><span className="fx-row-actions"><Button size="sm" busy={busy === c.tag} onClick={() => act(c.tag, () => api.post(`/v1/agents/sessions/${id}/restore`, { tag: c.tag }), `Restored ${c.tag}`)}>Restore</Button></span></td></tr>))}</tbody></table></div>
        )}
      </Card>
      <Card title="Session"><KV rows={[['Workspace', session.workspace?.path], ['Repository', session.workspace?.git], ['Policy', session.policy?.preset ?? (session.policy?.offline ? 'offline' : 'custom')], ['Expires', left(session.expires_at)], ['Terminal', session.capabilities?.pty ? 'available' : 'unavailable'], ['Memory checkpoints', session.capabilities?.memory_checkpoint ? 'supported' : 'not supported']]} /></Card>
    </div>
  );
}
