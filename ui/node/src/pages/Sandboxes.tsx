import { useState } from 'react';
import { Plus, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Segmented, Sheet, Skeleton, Stat, Tabs, api, left, navigate, statusTone, usePoll, type Vm } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { type ExecResult, execFailure, execTone, fmtUnix } from '../lib/exec';
import { rowsOf } from '../lib/generic';

interface Density { active_sandboxes?: number; paused_sandboxes?: number; warm_slots_ready?: number; warm_slots_configured?: number; host_pressure_level?: string | null }
type Profile = 'tiny' | 'small' | 'standard';

export function SandboxesPage({ notify }: { notify: Notify }) {
  const list = usePoll(async () => (await api.get<{ items: Vm[] }>('/v1/sandboxes')).items ?? [], 4000);
  const dens = usePoll(() => api.get<Density>('/v1/sandboxes/density').catch(() => null), 8000);
  const [create, setCreate] = useState(false);
  const [name, setName] = useState('');
  const [template, setTemplate] = useState('');
  const [profile, setProfile] = useState<Profile>('small');
  const [ttl, setTtl] = useState('3600');
  const [busy, setBusy] = useState('');
  const templates = usePoll(async () => (await api.get<{ items: { name: string }[] }>('/v1/templates').catch(() => ({ items: [] }))).items, 60000);
  const rows = list.data ?? [];
  const d = dens.data;
  const make = async () => {
    setBusy('create');
    try {
      const vm = await api.post<Vm>('/v1/sandboxes', { ...(name.trim() ? { name: name.trim() } : {}), ...(template ? { template } : {}), profile, ...(Number(ttl) > 0 ? { ttl_seconds: Number(ttl) } : {}) });
      notify('Sandbox created');
      setCreate(false);
      await list.refresh();
      if (vm?.id) navigate(`/sandboxes/${vm.id}`);
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const remove = async (v: Vm) => {
    setBusy(v.id);
    try {
      await api.del(`/v1/vms/${v.id}`);
      notify(`${v.name} deleted`);
      await list.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  return (
    <>
      <Hero eyebrow="Sandboxes" title="Disposable machines," accent="ready in a blink." lede="Short-lived, isolated machines for agents and experiments, with a warm pool so they start instantly.">
        <Button variant="primary" size="lg" onClick={() => setCreate(true)}><Plus size={18} />New sandbox</Button>
      </Hero>
      <div className="fx-grid cols-4">
        {dens.loading && !d ? [0, 1, 2, 3].map((i) => <Skeleton key={i} h={104} />) : (
          <>
            <Stat label="Active" value={d?.active_sandboxes ?? rows.filter((r) => r.status === 'running').length} tone="ok" />
            <Stat label="Paused" value={d?.paused_sandboxes ?? 0} tone="warn" />
            <Stat label="Warm and ready" value={d?.warm_slots_ready ?? 0} tone="info" hint={d?.warm_slots_configured != null ? `of ${d.warm_slots_configured} configured` : undefined} />
            <Stat label="Host memory pressure" value={d?.host_pressure_level ?? 'normal'} tone={d?.host_pressure_level === 'critical' ? 'bad' : d?.host_pressure_level === 'warn' ? 'warn' : 'idle'} />
          </>
        )}
      </div>
      <div className="fx-section">
        <Card>
          {list.loading && !rows.length ? <Skeleton h={140} /> : rows.length === 0 ? <EmptyState title="No sandboxes" body="Create one to run untrusted code or let an agent loose, then throw it away." action={<Button variant="primary" onClick={() => setCreate(true)}>Create a sandbox</Button>} /> : (
            <div className="fx-table-wrap">
              <table className="fx-table">
                <thead><tr><th>Name</th><th>State</th><th>Backend</th><th>Expires</th><th /></tr></thead>
                <tbody>
                  {rows.map((v) => (
                    <tr key={v.id}>
                      <td><a className="vm-name" href={`#/sandboxes/${v.id}`}>{v.name}</a></td>
                      <td><Pill tone={statusTone(v.status)}>{v.status}</Pill></td>
                      <td className="fx-muted">{v.backend}</td>
                      <td className="fx-muted">{left((v as unknown as { expires_at?: string }).expires_at)}</td>
                      <td><span className="fx-row-actions"><Button size="sm" variant="ghost" busy={busy === v.id} aria-label={`Delete ${v.name}`} onClick={() => remove(v)}><Trash2 size={14} /></Button></span></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Card>
      </div>
      <Sheet open={create} title="New sandbox" onClose={() => setCreate(false)}>
        <Field label="Name" hint="Optional; a name is generated otherwise.">{(i) => <input id={i} className="fx-input" autoFocus value={name} onChange={(e) => setName(e.target.value)} />}</Field>
        <Field label="Template">{(i) => (
          <select id={i} className="fx-input" value={template} onChange={(e) => setTemplate(e.target.value)}>
            <option value="">Default</option>
            {(templates.data ?? []).map((t) => <option key={t.name} value={t.name}>{t.name}</option>)}
          </select>
        )}</Field>
        <Field label="Size">{() => <Segmented label="Size" value={profile} onChange={setProfile} options={[{ value: 'tiny', label: 'Tiny · 1 / 512 MiB' }, { value: 'small', label: 'Small · 1 / 1 GiB' }, { value: 'standard', label: 'Standard · 2 / 2 GiB' }]} />}</Field>
        <Field label="Time to live (seconds)" hint="It is deleted automatically afterwards. 0 keeps it.">{(i) => <input id={i} className="fx-input" inputMode="numeric" value={ttl} onChange={(e) => setTtl(e.target.value.replace(/\D/g, ''))} />}</Field>
        <Button variant="primary" busy={busy === 'create'} onClick={make}>Create sandbox</Button>
      </Sheet>
    </>
  );
}

/* ---------------------------------------------------------------- detail */
type STab = 'run' | 'changes' | 'grants' | 'logs';
export function SandboxDetail({ id, notify }: { id: string; notify: Notify }) {
  const [tab, setTab] = useState<STab>('run');
  const vm = usePoll(async () => (await api.get<{ items: Vm[] }>('/v1/sandboxes')).items.find((v) => v.id === id) ?? null, 4000, [id]);
  const v = vm.data;
  if (vm.loading && !v) return <Skeleton h={160} />;
  if (!v) return <><a className="fx-back" href="#/sandboxes">‹ Sandboxes</a><Card><p className="fx-muted">This sandbox no longer exists.</p></Card></>;
  return (
    <>
      <a className="fx-back" href="#/sandboxes">‹ Sandboxes</a>
      <div className="fx-detail-head"><h1>{v.name} <Pill tone={statusTone(v.status)}>{v.status}</Pill></h1></div>
      <Tabs value={tab} onChange={setTab} tabs={[{ value: 'run', label: 'Run' }, { value: 'changes', label: 'Changes' }, { value: 'grants', label: 'Access grants' }, { value: 'logs', label: 'Logs' }]} />
      {tab === 'run' && <RunTab id={id} notify={notify} />}
      {tab === 'changes' && <ChangesTab id={id} notify={notify} />}
      {tab === 'grants' && <GrantsTab id={id} notify={notify} />}
      {tab === 'logs' && <SandboxLogs id={id} />}
    </>
  );
}

function RunTab({ id }: { id: string; notify: Notify }) {
  const [cmd, setCmd] = useState('ls -la');
  const [res, setRes] = useState<ExecResult | null>(null);
  const [err, setErr] = useState('');
  const [busy, setBusy] = useState(false);
  const run = async () => {
    setBusy(true);
    setErr('');
    setRes(null);
    try {
      const r = await api.post<ExecResult>(`/v1/sandboxes/${id}/process`, { command: cmd, timeout_seconds: 30 });
      const bad = execFailure(r);
      bad ? setErr(bad) : setRes(r);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Card title="Run in the sandbox">
      <Field label="Command">{(i) => <input id={i} className="fx-input fx-mono" value={cmd} onChange={(e) => setCmd(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && !busy && run()} />}</Field>
      <Button variant="primary" busy={busy} disabled={!cmd.trim()} onClick={run}>Run</Button>
      {err ? <div className="fx-error" role="alert" style={{ marginTop: 12 }}>{err}</div> : null}
      {res ? <div style={{ marginTop: 16 }}><Pill tone={execTone(res)}>exit {res.exit_code ?? '?'}</Pill>{res.stdout ? <pre className="fx-code" style={{ marginTop: 10 }}>{res.stdout}</pre> : null}{res.stderr ? <pre className="fx-code fx-code-err" style={{ marginTop: 10 }}>{res.stderr}</pre> : null}</div> : null}
    </Card>
  );
}

interface Changeset { id: string; state: string; command?: string; created_at?: number; changes?: { added?: unknown[]; modified?: unknown[]; deleted?: unknown[] }; error?: string | null }
const csTone = (s: string): 'ok' | 'warn' | 'bad' | 'info' | 'idle' => (s === 'applied' ? 'ok' : s === 'approved' ? 'info' : s === 'pending' ? 'warn' : s === 'failed' ? 'bad' : 'idle');

function ChangesTab({ id, notify }: { id: string; notify: Notify }) {
  const sets = usePoll(async () => (await api.get<{ items: Changeset[] }>(`/v1/sandboxes/${id}/changesets`).catch(() => ({ items: [] }))).items, 4000, [id]);
  const [cmd, setCmd] = useState('');
  const [busy, setBusy] = useState('');
  const act = async (key: string, fn: () => Promise<unknown>, ok: string) => {
    setBusy(key);
    try {
      await fn();
      notify(ok);
      await sets.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const n = (a?: unknown[]) => a?.length ?? 0;
  return (
    <div className="fx-grid">
      <Card title="Try a change safely">
        <p className="fx-muted">Runs the command on a copy and records what it would change. Nothing touches the sandbox until you apply it.</p>
        <Field label="Command">{(i) => <input id={i} className="fx-input fx-mono" value={cmd} onChange={(e) => setCmd(e.target.value)} placeholder="npm install" />}</Field>
        <Button variant="primary" busy={busy === 'spec'} disabled={!cmd.trim()} onClick={() => act('spec', () => api.post(`/v1/sandboxes/${id}/speculate`, { command: cmd.trim() }), 'Changeset recorded')}>Speculate</Button>
      </Card>
      <Card title="Changesets">
        {(sets.data ?? []).length === 0 ? <p className="fx-muted">No changesets yet.</p> : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th>Command</th><th>State</th><th>Changes</th><th>Created</th><th /></tr></thead>
            <tbody>{(sets.data ?? []).map((c) => (
              <tr key={c.id}>
                <td className="fx-mono">{c.command}</td>
                <td><Pill tone={csTone(c.state)}>{c.state}</Pill></td>
                <td className="fx-muted">+{n(c.changes?.added)} ~{n(c.changes?.modified)} −{n(c.changes?.deleted)}</td>
                <td className="fx-muted">{fmtUnix(c.created_at)}</td>
                <td><span className="fx-row-actions">
                  {c.state === 'pending' ? <><Button size="sm" variant="primary" busy={busy === c.id + 'a'} onClick={() => act(c.id + 'a', () => api.post(`/v1/sandboxes/${id}/changesets/${c.id}/approve`), 'Approved')}>Approve</Button><Button size="sm" busy={busy === c.id + 'r'} onClick={() => act(c.id + 'r', () => api.post(`/v1/sandboxes/${id}/changesets/${c.id}/reject`), 'Rejected')}>Reject</Button></> : null}
                  {c.state === 'approved' ? <Button size="sm" variant="primary" busy={busy === c.id + 'p'} onClick={() => act(c.id + 'p', () => api.post(`/v1/sandboxes/${id}/changesets/${c.id}/apply`), 'Applied')}>Apply</Button> : null}
                </span></td>
              </tr>))}</tbody></table></div>
        )}
      </Card>
    </div>
  );
}

function GrantsTab({ id, notify }: { id: string; notify: Notify }) {
  const grants = usePoll(async () => (await api.get<{ grants: Record<string, unknown>[] }>(`/v1/sandboxes/${id}/grants`).catch(() => ({ grants: [] }))).grants, 6000, [id]);
  const [open, setOpen] = useState(false);
  const [ref, setRef] = useState('');
  const [value, setValue] = useState('');
  const [hosts, setHosts] = useState('');
  const [ttl, setTtl] = useState('600');
  const [busy, setBusy] = useState('');
  const act = async (key: string, fn: () => Promise<unknown>, ok: string) => {
    setBusy(key);
    try {
      await fn();
      notify(ok);
      await grants.refresh();
      return true;
    } catch (e) {
      notify((e as Error).message, 'bad');
      return false;
    } finally {
      setBusy('');
    }
  };
  const list = grants.data ?? [];
  return (
    <>
      <Card title="Access grants" action={<Button size="sm" variant="primary" onClick={() => setOpen(true)}><Plus size={14} />New grant</Button>}>
        <p className="fx-muted">Let the sandbox call specific hosts with a secret it never sees. Grants expire on their own.</p>
        {list.length === 0 ? <p className="fx-muted">No grants.</p> : (
          <div className="fx-table-wrap"><table className="fx-table">
            <thead><tr><th>Secret</th><th>Hosts</th><th>Expires</th><th /></tr></thead>
            <tbody>{list.map((g) => (
              <tr key={String(g.id)}>
                <td>{String(g.secret_ref)}</td>
                <td className="fx-mono">{(g.hosts as string[] | undefined)?.join(', ')}</td>
                <td className="fx-muted">{left(g.expires_at as string | undefined)}</td>
                <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label="Revoke grant" busy={busy === String(g.id)} onClick={() => act(String(g.id), () => api.del(`/v1/sandboxes/${id}/grants/${g.id}`), 'Grant revoked')}><Trash2 size={14} /></Button></span></td>
              </tr>))}</tbody></table></div>
        )}
      </Card>
      <Sheet open={open} title="New access grant" onClose={() => setOpen(false)}>
        <Field label="Secret name">{(i) => <input id={i} className="fx-input" autoFocus value={ref} onChange={(e) => setRef(e.target.value)} placeholder="github" />}</Field>
        <Field label="Secret value" hint="Write-only. Leave empty to read FLUXVM_SECRET_<NAME> from the host.">{(i) => <input id={i} className="fx-input" type="password" autoComplete="off" value={value} onChange={(e) => setValue(e.target.value)} />}</Field>
        <Field label="Hosts" hint="Comma separated.">{(i) => <input id={i} className="fx-input" value={hosts} onChange={(e) => setHosts(e.target.value)} placeholder="api.github.com" />}</Field>
        <Field label="Lifetime (seconds)">{(i) => <input id={i} className="fx-input" inputMode="numeric" value={ttl} onChange={(e) => setTtl(e.target.value.replace(/\D/g, ''))} />}</Field>
        <Button variant="primary" busy={busy === 'new'} disabled={!ref.trim() || !hosts.trim()} onClick={async () => (await act('new', () => api.post(`/v1/sandboxes/${id}/grants`, { secret_ref: ref.trim(), ...(value ? { value } : {}), hosts: hosts.split(',').map((h) => h.trim()).filter(Boolean), ttl_seconds: Number(ttl) || 600 }), 'Grant created')) && (setOpen(false), setValue(''))}>Create grant</Button>
      </Sheet>
    </>
  );
}

function SandboxLogs({ id }: { id: string }) {
  const logs = usePoll(() => api.get<{ log?: string }>(`/v1/sandboxes/${id}/logs?lines=300`).catch((e: Error) => ({ log: `Could not read logs: ${e.message}` })), 3000, [id]);
  return <Card title="Logs"><pre className="fx-code" style={{ minHeight: 260 }}>{logs.data?.log || 'No output yet.'}</pre></Card>;
}

export { rowsOf };
