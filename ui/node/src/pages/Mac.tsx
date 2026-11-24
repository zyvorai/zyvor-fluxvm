// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useMemo, useState } from 'react';
import { Copy, Play, RotateCw, ScrollText, Search, Skull, Square, Trash2 } from 'lucide-react';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { BackupsCard } from './MacBackups';
import { MacSchedules } from './MacSchedules';
import { navigate, Button, Card, EmptyState, Field, Hero, Pill, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import { copyFor, lightWord, tone, type ClusterHealth } from '../lib/health';
import {
  COMPOSE_COMMAND, DOCTOR_COMMAND, IMAGES_COMMAND, LOGINS_COMMAND, PS_COMMAND, SUPERVISE_INSTALL_COMMAND, UNSUPPORTED_NOTE,
  actionCommand, actionDone, healthAdvice, healthCommand, healthText, healthTone, composeCommands, gateNote, actionLabel, actionsFor, ageText, attachCommand, execCommand, explainError, filterInstances, inspectCommand, loginCommand, logsCommand, memoryText, planEdit, portsText, shareCommand, sizeText, statusTone, summarize, supervisorNote, editFormFor, updateAnswerText, RESTART_POLICIES,
  type EditForm, type MacAction, type MacInstance, type MacUpdateAnswer, type MacView,
} from '../lib/mac';

const ICON: Record<MacAction, typeof Play> = { start: Play, stop: Square, restart: RotateCw, kill: Skull, rm: Trash2 };
/** Actions that lose something (a running guest's memory, or the disk) ask a second time. */
const ASKS: MacAction[] = ['kill', 'rm'];

const copyText = (notify: Notify, cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));

/** This Mac's health in plain words, from the same report as Home: slots, memory, disk, session, thermal. Read-only. */
function HostHealth({ notify }: { notify: Notify }) {
  const h = usePoll(() => api.get<ClusterHealth>('/v1/cluster/health'), 15000);
  const d = h.data;
  const m = d?.machines.find((x) => x.local) ?? d?.machines[0];
  if (!d || !m) {
    return (
      <Card title="Host health" action={<CopyCmd notify={notify} cmd={DOCTOR_COMMAND} />}>
        <p className="fx-muted">{h.loading ? 'Checking this Mac.' : `The health report is not available (${h.error?.message ?? 'no answer'}). Run ${DOCTOR_COMMAND} on the Mac instead.`}</p>
      </Card>
    );
  }
  return (
    <Card title="Host health" action={<><Pill tone={tone(m.light)}>{lightWord(m.light)}</Pill> <CopyCmd notify={notify} cmd="fluxctl cluster status" /></>}>
      <p>{m.headline}</p>
      <p className="fx-muted">{m.capacity.summary}</p>
      {m.items.length === 0 ? <p className="fx-muted">{m.checks_passed} checks passed. Nothing to do.</p> : (
        <ul className="fx-feed">
          {m.items.map((i) => {
            const cmd = copyFor(i);
            return (
              <li key={i.id}>
                <Pill tone={tone(i.severity)}>{lightWord(i.severity)}</Pill>
                <span>
                  {i.message}
                  {i.cause ? <div className="fx-muted">Why: {i.cause}</div> : null}
                  {i.fix ? <div className="fx-muted">Fix: {i.fix.command ? <code className="fx-mono">{i.fix.command}</code> : i.fix.steps}</div> : null}
                </span>
                {cmd ? <CopyCmd notify={notify} cmd={cmd} /> : null}
              </li>
            );
          })}
        </ul>
      )}
      <p className="fx-muted">Studio only shows this. <code className="fx-mono">{DOCTOR_COMMAND}</code> checks the Mac's own settings in more detail.</p>
    </Card>
  );
}

/** Edit one instance like `docker update`: the same fields as `fluxctl mac update`, and the command for exactly what you changed. */
function EditPanel({ inst, notify }: { inst: MacInstance; notify: Notify }) {
  const [form, setForm] = useState<EditForm>(() => editFormFor(inst));
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<{ text: string; tone: 'ok' | 'warn' | 'bad' } | null>(null);
  const plan = useMemo(() => planEdit(inst, form), [inst, form]);
  const set = (p: Partial<EditForm>) => setForm((f) => ({ ...f, ...p }));
  const live = inst.status === 'running' || inst.status === 'paused';
  const save = async () => {
    setBusy(true);
    setResult(null);
    try {
      const a = await api.patch<MacUpdateAnswer>(`/v1/mac/instances/${encodeURIComponent(inst.id)}`, plan.body);
      setResult({ text: updateAnswerText(inst.name, a), tone: a.pending_restart.length ? 'warn' : 'ok' });
      notify(a.pending_restart.length ? `${inst.name}: saved, the new size waits for a restart` : `${inst.name} updated`);
      if (!a.pending_restart.length) setForm((f) => ({ ...f, restartNow: false }));
    } catch (e) {
      setResult({ text: explainError(inst.name, (e as Error).message), tone: 'bad' });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div>
      <p className="fx-muted">Change what you need; only the changes are sent. CPUs and memory are read when a macOS VM boots, so {live ? `${inst.name} is running and needs a restart for them` : `${inst.name} is stopped, so they apply now and are used at the next start`}.</p>
      <Field label="CPUs">{(i) => <input id={i} className="fx-input" inputMode="numeric" value={form.cpus} onChange={(e) => set({ cpus: e.target.value.replace(/\D/g, '') })} />}</Field>
      <Field label="Memory" hint="8g, 512m or MiB.">{(i) => <input id={i} className="fx-input" value={form.memory} onChange={(e) => set({ memory: e.target.value })} />}</Field>
      <Field label="Restart policy" hint="Applied by fluxctl mac supervise.">{(i) => <select id={i} className="fx-input" value={form.restart} onChange={(e) => set({ restart: e.target.value })}>{RESTART_POLICIES.map((p) => <option key={p} value={p}>{p}</option>)}</select>}</Field>
      <Field label="Labels" hint="One KEY=VALUE per line. Delete a line to remove that label.">{(i) => <textarea id={i} className="fx-input fx-mono" rows={4} value={form.labels} onChange={(e) => set({ labels: e.target.value })} />}</Field>
      <p className="fx-muted">Environment variables are not stored on a macOS VM. Pass them per command: <code className="fx-mono">fluxctl mac exec {inst.name} -e KEY=VALUE -- CMD</code>.</p>
      {plan.needsRestart ? (
        <>
          <label className="fx-check"><input type="checkbox" checked={form.restartNow} onChange={(e) => set({ restartNow: e.target.checked })} /> Restart now to apply the new size ({inst.name} stops for a moment; same name and disk)</label>
          {form.restartNow ? <label className="fx-check"><input type="checkbox" checked={form.checkpoint} onChange={(e) => set({ checkpoint: e.target.checked })} /> Save a checkpoint first, so you can go back</label> : <p className="fx-muted">Without this, the labels and policy are saved but the new size is not applied.</p>}
        </>
      ) : null}
      {plan.error ? <p className="fx-muted">{plan.error}</p> : <p>Will change: {plan.changes.join('; ')}.</p>}
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <Button disabled={!!plan.error} busy={busy} onClick={() => void save()}>{plan.needsRestart && form.restartNow ? 'Save and restart' : 'Save'}</Button>
        <Button variant="ghost" aria-label="Copy as fluxctl" disabled={!!plan.error} onClick={() => void copyText(notify, plan.command)}><Copy size={14} /> Copy as fluxctl</Button>
        {plan.error ? null : <code className="fx-mono">{plan.command}</code>}
      </div>
      {result ? <p role="status"><Pill tone={result.tone}>{result.tone === 'ok' ? 'saved' : result.tone === 'warn' ? 'needs a restart' : 'not saved'}</Pill> {result.text}</p> : null}
    </div>
  );
}

/** One instance: its inspect record and the tail of its console log, with the command for each. */
function Detail({ inst, notify, onClose }: { inst: MacInstance; notify: Notify; onClose: () => void }) {
  const [tab, setTab] = useState<'inspect' | 'edit' | 'logs'>('inspect');
  const [lines, setLines] = useState(300);
  const rec = usePoll(() => api.get<unknown>(`/v1/vms/${encodeURIComponent(inst.id)}`), 5000, [inst.id]);
  const logs = usePoll(() => api.get<string>(`/v1/vms/${encodeURIComponent(inst.id)}/logs?lines=${lines}`, { text: true }), tab === 'logs' && inst.status === 'running' ? 3000 : 3_600_000, [inst.id, lines, tab]);
  const facts: [string, string][] = [
    ['Name', inst.name], ['Id', inst.id], ['Image', inst.image ?? '—'], ['State', inst.status], ['Address', inst.ip ?? 'none yet'],
    ['CPUs', inst.cpus == null ? '—' : String(inst.cpus)], ['Memory', memoryText(inst.memory_mib)], ['Ports', portsText(inst.ports) || 'none published'],
    ['Restart policy', inst.restart_policy], ['Health', healthText(inst.health) || 'no probe has run yet'], ['Project', inst.project ?? '—'], ['Created', inst.created ?? '—'],
  ];
  return (
    <Card title={<>{inst.name} <span className="fx-muted">details</span></>} action={<Button size="sm" variant="ghost" onClick={onClose}>Close</Button>}>
      {inst.error ? <p><Pill tone="bad">error</Pill> {inst.error}. Run <code className="fx-mono">{DOCTOR_COMMAND}</code> on the Mac, then <code className="fx-mono">{actionCommand('start', inst.name)}</code>.</p> : null}
      <Tabs value={tab} onChange={setTab} tabs={[{ value: 'inspect', label: 'Inspect' }, { value: 'edit', label: 'Edit' }, { value: 'logs', label: 'Logs' }]} />
      {tab === 'edit' ? <EditPanel key={inst.id} inst={inst} notify={notify} /> : tab === 'inspect' ? (
        <>
          <div className="fx-table-wrap"><table className="fx-table"><tbody>{facts.map(([k, v]) => <tr key={k}><td className="fx-muted">{k}</td><td className="fx-mono">{v}</td></tr>)}</tbody></table></div>
          {Object.keys(inst.labels).length ? <p className="fx-muted">Labels: {Object.entries(inst.labels).map(([k, v]) => `${k}=${v}`).join(', ')}</p> : null}
          <details>
            <summary>The daemon's full record</summary>
            <pre className="fx-mono" style={{ maxHeight: 320, overflow: 'auto' }}>{rec.error ? `Could not read the record: ${rec.error.message}` : JSON.stringify(rec.data ?? {}, null, 2)}</pre>
          </details>
          {healthAdvice(inst) ? <p><Pill tone="bad">unhealthy</Pill> {healthAdvice(inst)}</p> : null}
          <p><CopyCmd notify={notify} cmd={healthCommand(inst.name)} /> <code className="fx-mono">{healthCommand(inst.name)}</code> <span className="fx-muted">probe its health now</span></p>
          <p><CopyCmd notify={notify} cmd={inspectCommand(inst.name)} /> <code className="fx-mono">{inspectCommand(inst.name)}</code></p>
          <p><CopyCmd notify={notify} cmd={execCommand(inst.name)} /> <code className="fx-mono">{execCommand(inst.name)}</code> <span className="fx-muted">a shell in the guest</span></p>
          {inst.status === 'stopped' ? <p><CopyCmd notify={notify} cmd={shareCommand(inst.name)} /> <code className="fx-mono">{shareCommand(inst.name)}</code> <span className="fx-muted">one file for another Mac; logins and secrets are left out, nothing is uploaded (add --serve for a one-time local link)</span></p> : null}
          <p><CopyCmd notify={notify} cmd={attachCommand(inst.name)} /> <code className="fx-mono">{attachCommand(inst.name)}</code> <span className="fx-muted">follow its console</span></p>
        </>
      ) : (
        <>
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginBottom: 8 }}>
            <select className="fx-input" style={{ width: 'auto' }} aria-label="Lines" value={lines} onChange={(e) => setLines(Number(e.target.value))}>{[100, 300, 1000, 3000].map((n) => <option key={n} value={n}>last {n}</option>)}</select>
            <span className="fx-muted">{inst.status === 'running' ? 'Refreshes every 3 seconds.' : 'Not running, so the log does not change.'} This is the VM's console output.</span>
            <CopyCmd notify={notify} cmd={logsCommand(inst.name)} />
          </div>
          <pre className="fx-mono" role="log" style={{ maxHeight: 360, overflow: 'auto', whiteSpace: 'pre-wrap' }}>
            {logs.error && !logs.data ? `Could not read the log: ${logs.error.message}. The log file appears once the VM has started at least once; try Start, or run ${logsCommand(inst.name)}.` : logs.data || (logs.loading ? 'Loading…' : 'No log output yet.')}
          </pre>
        </>
      )}
    </Card>
  );
}

/** `fluxctl mac` instances: macOS guests of this Mac, with the same start, stop, restart, kill and remove the CLI does. */
export function MacPage({ notify }: { notify: Notify }) {
  const view = usePoll(() => api.get<MacView>('/v1/mac'), 5000);
  const d = view.data;
  const [tab, setTab] = useState<'instances' | 'images' | 'projects' | 'login' | 'schedules'>('instances');
  const [q, setQ] = useState('');
  const [open, setOpen] = useState<MacInstance | null>(null);
  const [busy, setBusy] = useState('');
  const [ask, setAsk] = useState('');
  const now = Date.now();
  const shown = useMemo(() => filterInstances(d?.instances ?? [], q), [d, q]);
  const sum = useMemo(() => summarize(d?.instances ?? []), [d]);
  const forbidden = !!view.error && /admin/i.test(view.error.message);

  const act = async (i: MacInstance, a: MacAction) => {
    const key = i.id + a;
    if (ASKS.includes(a) && ask !== key) {
      setAsk(key);
      return;
    }
    setAsk('');
    setBusy(key);
    try {
      await api.post(`/v1/mac/instances/${encodeURIComponent(i.id)}/${a}`);
      notify(actionDone(i.name, a));
      if (a === 'rm' && open?.id === i.id) setOpen(null);
      await view.refresh();
    } catch (e) {
      notify(explainError(i.name, (e as Error).message), 'bad');
    } finally {
      setBusy('');
    }
  };

  return (
    <>
      <Hero eyebrow="Mac" title="macOS instances" accent="on this Mac." lede="The macOS guests fluxctl mac runs: start, stop, restart, kill or remove one, read its log and inspect it. Every button has the fluxctl command beside it." />
      {forbidden ? (
        <Card><EmptyState title="Admin only" body="Instances belong to the whole Mac, so only an administrator can see them. Sign in with an admin token, or run fluxctl mac ps -a in a terminal." action={<CopyCmd notify={notify} cmd={PS_COMMAND} />} /></Card>
      ) : view.loading && !d ? (
        <Skeleton h={160} />
      ) : !d ? (
        <Card><EmptyState title="The daemon could not list the instances" body={`${view.error?.message ?? 'No answer'}. An older FluxVM daemon has no Mac page data: update it, or run ${PS_COMMAND} on the Mac.`} action={<Button onClick={() => void copyText(notify, PS_COMMAND)}><Copy size={14} /> Copy as fluxctl</Button>} /></Card>
      ) : (
        <>
          <HostHealth notify={notify} />
          <BackupsCard notify={notify} />
          {!d.supported ? <Card title="Not a Mac"><p className="fx-muted">{UNSUPPORTED_NOTE}</p></Card> : null}
          <Tabs value={tab} onChange={setTab} tabs={[
            { value: 'instances', label: `Instances · ${d.instances.length}` },
            { value: 'images', label: `Images · ${d.images.items.length}` },
            { value: 'projects', label: `Compose · ${d.projects.length}` },
            { value: 'login', label: `Registry login · ${d.logins.items.length}` },
            { value: 'schedules', label: 'Schedules' },
          ]} />
          {tab === 'instances' ? (
            <Card title="Instances" action={<CopyCmd notify={notify} cmd={PS_COMMAND} />}>
              <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', marginBottom: 12 }}>
                <Pill tone={d.macos_running >= d.macos_limit ? 'warn' : 'ok'}>{d.macos_running} of {d.macos_limit} macOS VMs running</Pill>
                {sum.stopped ? <Pill tone="idle">{sum.stopped} stopped</Pill> : null}
                {sum.failed ? <Pill tone="bad">{sum.failed} failed</Pill> : null}
                {sum.unhealthy ? <Pill tone="bad">{sum.unhealthy} unhealthy</Pill> : null}
                <span className="fx-muted">Apple allows two macOS VMs at once on one Mac.</span>
                <span style={{ position: 'relative', flex: 1, minWidth: 180, display: 'inline-flex', alignItems: 'center' }}>
                  <Search size={14} style={{ position: 'absolute', left: 8, opacity: 0.6 }} />
                  <input className="fx-input" style={{ flex: 1, paddingLeft: 28 }} aria-label="Filter instances" placeholder="Filter by name, image, address, state, health or team=ios" value={q} onChange={(e) => setQ(e.target.value)} />
                </span>
              </div>
              {supervisorNote(d.instances, d.supervisor) ? <p><Pill tone="warn">not restarted</Pill> {supervisorNote(d.instances, d.supervisor)} <CopyCmd notify={notify} cmd={SUPERVISE_INSTALL_COMMAND} /></p> : null}
              {shown.length === 0 ? (
                <EmptyState title={d.instances.length ? 'No instance matches' : 'No instances yet'} body={d.instances.length ? 'Change the filter to see more.' : 'Start one with fluxctl mac run IMAGE; it shows here. See Images for what is available.'} action={d.instances.length ? undefined : <CopyCmd notify={notify} cmd="fluxctl mac run IMAGE" />} />
              ) : (
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>Image</th><th>State</th><th>CPU / memory</th><th>Ports</th><th>Age</th><th /></tr></thead><tbody>
                  {shown.map((i) => (
                    <tr key={i.id}>
                      <td className="vm-name">{i.name}<div className="fx-muted fx-mono">{i.ip ?? 'no address yet'}</div></td>
                      <td className="fx-mono">{i.image ?? '—'}</td>
                      <td><Pill tone={statusTone(i.status)}>{i.status}</Pill>{i.health ? <div><Pill tone={healthTone(i.health)}>{i.health.status}</Pill></div> : null}{i.restart_policy !== 'no' ? <div className="fx-muted" title="fluxctl mac run --restart">restart: {i.restart_policy}</div> : null}</td>
                      <td className="fx-muted">{i.cpus ?? '—'} CPU · {memoryText(i.memory_mib)}</td>
                      <td className="fx-mono">{portsText(i.ports) || '—'}</td>
                      <td className="fx-muted">{ageText(i.created, now)}</td>
                      <td><span className="fx-row-actions">
                        {actionsFor(i.status).map((a) => {
                          const Icon = ICON[a];
                          const key = i.id + a;
                          const asking = ask === key;
                          return (
                            <span key={a} style={{ display: 'inline-flex', alignItems: 'center' }}>
                              <Button size="sm" variant="ghost" aria-label={`${actionLabel(a)} ${i.name}`} title={actionCommand(a, i.name)} busy={busy === key} disabled={!!busy} onClick={() => void act(i, a)}>
                                <Icon size={14} />{asking ? `Sure? ${actionLabel(a)}` : actionLabel(a)}
                              </Button>
                              <CopyCmd notify={notify} cmd={actionCommand(a, i.name)} />
                            </span>
                          );
                        })}
                        <Button size="sm" variant="ghost" aria-label={`Logs and inspect of ${i.name}`} onClick={() => setOpen(i)}><ScrollText size={14} />Logs</Button>
                      </span>
                        {ASKS.some((a) => ask === i.id + a) ? <div className="fx-muted">{ask.endsWith('rm') ? 'This deletes the disk.' : 'This ends the VM at once with no clean shutdown.'} Press again to confirm, or run <code className="fx-mono">{actionCommand(ask.endsWith('rm') ? 'rm' : 'kill', i.name)}</code>.</div> : null}
                      </td>
                    </tr>
                  ))}
                </tbody></table></div>
              )}
              <p className="fx-muted">Studio runs these through the daemon, exactly as fluxctl does. Use the copy button next to a button to run the same thing in a terminal.</p>
            </Card>
          ) : null}
          {tab === 'images' ? (
            <Card title="Images" action={<CopyCmd notify={notify} cmd={IMAGES_COMMAND} />}>
              {d.images.note ? <p className="fx-muted">{d.images.note}</p> : null}
              {d.images.items.length === 0 ? (
                <EmptyState title="No images" body={`Pull or build one: fluxctl mac pull REFERENCE. Looked in ${d.images.path ?? 'no known folder'}.`} action={<CopyCmd notify={notify} cmd="fluxctl mac pull REFERENCE" />} />
              ) : (
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Image</th><th>Source</th><th>Disk size</th><th>Added</th></tr></thead><tbody>
                  {d.images.items.map((m) => (
                    <tr key={m.reference}>
                      <td className="vm-name fx-mono">{m.reference}{m.digest ? <div className="fx-muted">{m.digest.slice(0, 19)}</div> : null}</td>
                      <td className="fx-muted">{m.source || 'local'}</td>
                      <td>{m.present ? sizeText(m.size_bytes) : <Pill tone="bad">files missing</Pill>}</td>
                      <td className="fx-muted">{m.added_unix ? ageText(new Date(m.added_unix * 1000).toISOString(), now) + ' ago' : '—'}</td>
                    </tr>
                  ))}
                </tbody></table></div>
              )}
              <p className="fx-muted">Read from {d.images.path ?? 'the image store'}. A file marked missing means its folder was deleted: run <code className="fx-mono">fluxctl mac rmi REFERENCE</code> and pull it again. Sizes are the disk's apparent size.</p>
            </Card>
          ) : null}
          {tab === 'projects' ? (
            <Card title="Compose projects" action={<CopyCmd notify={notify} cmd={COMPOSE_COMMAND} />}>
              {d.projects.length === 0 ? (
                <EmptyState title="No compose projects" body="Bring one up with fluxctl mac compose up; its services show here, read-only." action={<CopyCmd notify={notify} cmd="fluxctl mac compose up" />} />
              ) : d.projects.map((p) => (
                <div key={p.name} style={{ marginBottom: 12 }}>
                  <p><b>{p.name}</b> <Pill tone={p.running === p.total ? 'ok' : p.running ? 'warn' : 'idle'}>{p.running} of {p.total} running</Pill> <CopyCmd notify={notify} cmd={`fluxctl mac compose ps -p ${p.name}`} /> {composeCommands(p.name).map((c) => <span key={c.label} title={c.label}><CopyCmd notify={notify} cmd={c.cmd} /></span>)}</p>
                  <ul className="fx-feed">{p.services.map((s) => <li key={s.instance}><Pill tone={statusTone(s.status)}>{s.status}</Pill>{s.health ? <Pill tone={healthTone(s.health)}>{s.health.status}</Pill> : null}<span>{s.service ?? s.instance} <span className="fx-muted fx-mono">({s.instance})</span></span></li>)}</ul>
                  {gateNote(p) ? <p className="fx-muted">{gateNote(p)}</p> : null}
                </div>
              ))}
              <p className="fx-muted">Studio only lists projects. Run <code className="fx-mono">fluxctl mac compose up</code> or <code className="fx-mono">down</code> in the project's folder.</p>
            </Card>
          ) : null}
          {tab === 'login' ? (
            <Card title="Registry login" action={<CopyCmd notify={notify} cmd={LOGINS_COMMAND} />}>
              {d.logins.problem ? <p><Pill tone="bad">needs attention</Pill> {d.logins.problem}</p> : null}
              {d.logins.items.length === 0 && !d.logins.problem ? (
                <EmptyState title="Not logged in to any registry" body="Public images pull without a login. For a private registry run fluxctl mac login REGISTRY; it asks for the password in the terminal, never here." action={<CopyCmd notify={notify} cmd={loginCommand()} />} />
              ) : (
                <ul className="fx-feed">{d.logins.items.map((l) => <li key={l.registry}><Pill tone="ok">logged in</Pill><span><b>{l.registry}</b> as <span className="fx-mono">{l.username}</span></span><CopyCmd notify={notify} cmd={loginCommand(l.registry)} /></li>)}</ul>
              )}
              <p className="fx-muted">Passwords are never shown or typed in Studio. The login file is {d.logins.path ?? 'not known to the daemon'}; it is the one of the account the daemon runs as, so if you use fluxctl as another user, run <code className="fx-mono">{LOGINS_COMMAND}</code> there.</p>
            </Card>
          ) : null}
          {tab === 'schedules' ? <MacSchedules notify={notify} /> : null}
          {open ? <Detail inst={d.instances.find((x) => x.id === open.id) ?? open} notify={notify} onClose={() => setOpen(null)} /> : null}
        </>
      )}
    </>
  );
}

/** Home: a short card for the macOS instances, with a link to the page. Shown only on a Mac the daemon can list; quiet otherwise. */
export function MacHomeCard({ notify }: { notify: Notify }) {
  const view = usePoll(() => api.get<MacView>('/v1/mac'), 10000);
  const d = view.data;
  if (!d || !d.supported) return null;
  const sum = summarize(d.instances);
  return (
    <div className="fx-section">
      <Card title="Mac instances" action={<><CopyCmd notify={notify} cmd={PS_COMMAND} /> <Button size="sm" onClick={() => navigate('/mac')}>Open</Button></>}>
        <p>
          <Pill tone={d.macos_running >= d.macos_limit ? 'warn' : 'ok'}>{d.macos_running} of {d.macos_limit} macOS VMs running</Pill>{' '}
          {sum.failed ? <Pill tone="bad">{sum.failed} failed</Pill> : null}
        </p>
        <p className="fx-muted">
          {d.instances.length === 0 ? 'No instances yet. Start one with fluxctl mac run IMAGE.' : `${d.instances.length} instance${d.instances.length === 1 ? '' : 's'}, ${d.images.items.length} image${d.images.items.length === 1 ? '' : 's'}, ${d.projects.length} compose project${d.projects.length === 1 ? '' : 's'}.`}
          {sum.failed ? ' A failed instance: open the page for its log, then Start it again.' : ''}
        </p>
      </Card>
    </div>
  );
}
