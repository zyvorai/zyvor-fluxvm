// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useMemo, useState } from 'react';
import { Activity, Copy, Pause, Play, RotateCw, ScrollText, Skull, Square } from 'lucide-react';
import type { Notify } from '../App';
import { ContainerWindow } from './ContainerWindow';
import { CopyCmd } from './Images';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import { type DockerAction, type DockerContainer, type DockerContext, type DockerStats, type DockerView, actionDone, actionLabel, actionsFor, builtinNote, FULL_ENGINE_COMMAND, statsText, contextCommand, contextLabel, contextTone, ageSince, containerTone, filterContainers, fmtSize, portsText, summarize } from '../lib/docker';

/** The `fluxvm` docker CLI context: whether `docker --context fluxvm ps` works and whether `docker` uses it by default. Read-only; the command is copied. */
function DockerContextCard({ notify }: { notify: Notify }) {
  const ctx = usePoll(async () => await api.get<DockerContext>('/v1/docker/context'), 10000);
  const c = ctx.data;
  if (!c) return ctx.error && !/admin/i.test(ctx.error.message) ? <Card title="Docker context"><p className="fx-muted">Could not read the docker context: {ctx.error.message}. An older FluxVM daemon does not have it; run <code className="fx-mono">fluxctl docker context status</code> instead.</p></Card> : null;
  const act = contextCommand(c);
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  return (
    <Card title="Docker context" action={<CopyCmd notify={notify} cmd="fluxctl docker context status" />}>
      <p className="fx-muted">
        <Pill tone={contextTone(c.state)}>{contextLabel(c.state)}</Pill> {c.summary}
      </p>
      {c.docker_cli_hint ? <p className="fx-muted">{c.docker_cli_hint}</p> : null}
      {c.state === 'ready' || c.state === 'current' ? <p className="fx-muted">Try <code className="fx-mono">{c.try}</code></p> : null}
      {act ? (
        <div className="fx-actions">
          <Button onClick={() => void copy(act.command)}><Copy size={14} /> {act.label}</Button>
          <span className="fx-muted">Studio only shows this. Run <code className="fx-mono">{act.command}</code> in a terminal on this machine (the button copies it).</span>
        </div>
      ) : null}
    </Card>
  );
}

const ACTION_ICON: Record<DockerAction, typeof Play> = { start: Play, stop: Square, restart: RotateCw, kill: Skull, pause: Pause, unpause: Play };

/** The Docker engine `fluxctl docker` runs in its own VM: containers and images, a container's log, and start, stop and restart. */
export function DockerPage({ notify }: { notify: Notify }) {
  const view = usePoll(async () => await api.get<DockerView>('/v1/docker'), 5000);
  const d = view.data;
  const [tab, setTab] = useState<'containers' | 'images'>('containers');
  const [group, setGroup] = useState<'all' | 'running' | 'stopped'>('all');
  const [q, setQ] = useState('');
  const [open, setOpen] = useState<DockerContainer | null>(null);
  const [busy, setBusy] = useState('');
  const [stats, setStats] = useState<Record<string, string>>({});
  const now = Date.now();
  const act = async (c: DockerContainer, action: DockerAction) => {
    setBusy(c.id + action);
    try {
      const r = await api.post<{ changed: boolean }>(`/v1/docker/containers/${encodeURIComponent(c.id)}/${action}`);
      notify(actionDone(c.name, action, r.changed));
      await view.refresh();
    } catch (e) {
      notify(`${c.name}: ${(e as Error).message}`, 'bad');
    } finally {
      setBusy('');
    }
  };
  const readStats = async (c: DockerContainer) => {
    setBusy(c.id + 'stats');
    try {
      const r = await api.get<DockerStats>(`/v1/docker/containers/${encodeURIComponent(c.id)}/stats`);
      setStats((m) => ({ ...m, [c.id]: statsText(r) || 'No reading yet' }));
    } catch (e) {
      notify(`${c.name}: ${(e as Error).message}`, 'bad');
    } finally {
      setBusy('');
    }
  };
  const shown = useMemo(() => filterContainers(d?.containers ?? [], q, group), [d, q, group]);
  const sum = useMemo(() => summarize(d?.containers ?? []), [d]);
  const forbidden = view.error?.message && /admin/i.test(view.error.message);

  return (
    <>
      <Hero eyebrow="Docker" title="The engine on this host," accent="containers included." lede="The Docker engine fluxctl docker runs in its own VM. Start, stop, restart, pause or kill a container, read its log and its resource use; use the docker CLI for the rest." />
      {forbidden ? null : <DockerContextCard notify={notify} />}
      {view.loading && !d ? (
        <Skeleton h={160} />
      ) : forbidden ? (
        <Card><EmptyState title="Admin only" body="The containers belong to the whole host, so only an administrator can see them." /></Card>
      ) : view.error && !d ? (
        <Card><EmptyState title="The daemon could not read the engine" body={view.error.message} /></Card>
      ) : d && !d.engine.reachable ? (
        <Card><EmptyState title={d.engine.vm ? `The engine is ${d.engine.vm.status}` : 'No Docker engine'} body={d.engine.reason ?? 'The engine did not answer.'} /></Card>
      ) : d ? (
        <>
          <Card title="Engine">
            <p className="fx-muted">
              <Pill tone="ok">reachable</Pill>{' '}
              Docker <span className="fx-mono">{d.engine.info?.version ?? '?'}</span> (API {d.engine.info?.api ?? '?'}) on {d.engine.info?.os ?? 'linux'}/{d.engine.info?.arch ?? '?'}, kernel <span className="fx-mono">{d.engine.info?.kernel ?? '?'}</span>
              {d.engine.vm ? <> · machine <b>{d.engine.vm.name}</b> ({d.engine.vm.status})</> : null}
              {' '}· socket <span className="fx-mono">{d.engine.socket}</span>
            </p>
          </Card>
          <Card title="Built-in engine">
            <p className="fx-muted">Unsupported by the built-in engine (the daemon's own docker.sock): {builtinNote()}</p>
            <CopyCmd notify={notify} cmd={FULL_ENGINE_COMMAND} />
          </Card>
          <Tabs value={tab} onChange={setTab} tabs={[{ value: 'containers', label: `Containers · ${d.containers.length}` }, { value: 'images', label: `Images · ${d.images.length}` }]} />
          {tab === 'containers' ? (
            <Card title="Containers">
              <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', marginBottom: 12 }}>
                <Pill tone="ok">{sum.running} running</Pill>
                {sum.stopped ? <Pill tone="idle">{sum.stopped} stopped</Pill> : null}
                {sum.failing ? <Pill tone="bad">{sum.failing} failing</Pill> : null}
                <select className="fx-input" style={{ width: 'auto' }} aria-label="Show" value={group} onChange={(e) => setGroup(e.target.value as typeof group)}>
                  <option value="all">All containers</option><option value="running">Running</option><option value="stopped">Not running</option>
                </select>
                <input className="fx-input" style={{ flex: 1, minWidth: 180 }} aria-label="Filter containers" placeholder="Filter by name, image, id or status" value={q} onChange={(e) => setQ(e.target.value)} />
              </div>
              {shown.length === 0 ? (
                <EmptyState title={d.containers.length ? 'No container matches' : 'No containers'} body={d.containers.length ? 'Change the filter to see more.' : 'Run one with docker run; it shows here.'} />
              ) : (
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>Image</th><th>State</th><th>Status</th><th>Ports</th><th>Age</th><th /></tr></thead><tbody>
                  {shown.map((c) => (
                    <tr key={c.id}>
                      <td className="vm-name">{c.name}<div className="fx-muted fx-mono">{c.id}</div>{stats[c.id] ? <div className="fx-muted">{stats[c.id]}</div> : null}</td>
                      <td className="fx-mono">{c.image}</td>
                      <td><Pill tone={containerTone(c)}>{c.state}</Pill></td>
                      <td className="fx-muted">{c.status}</td>
                      <td className="fx-mono">{portsText(c.ports) || '—'}</td>
                      <td className="fx-muted">{ageSince(c.created, now)}</td>
                      <td><span className="fx-row-actions">
                        {actionsFor(c.state).map((a) => {
                          const Icon = ACTION_ICON[a];
                          return <Button key={a} size="sm" variant="ghost" aria-label={`${actionLabel(a)} ${c.name}`} busy={busy === c.id + a} disabled={!!busy} onClick={() => void act(c, a)}><Icon size={14} />{actionLabel(a)}</Button>;
                        })}
                        {c.state === 'running' ? <Button size="sm" variant="ghost" aria-label={`Stats of ${c.name}`} busy={busy === c.id + 'stats'} disabled={!!busy} onClick={() => void readStats(c)}><Activity size={14} />Stats</Button> : null}
                        <Button size="sm" variant="ghost" aria-label={`Logs of ${c.name}`} onClick={() => setOpen(c)}><ScrollText size={14} />Logs</Button>
                      </span></td>
                    </tr>
                  ))}
                </tbody></table></div>
              )}
            </Card>
          ) : (
            <Card title="Images">
              {d.images.length === 0 ? <EmptyState title="No images" body="Pull one with docker pull; it shows here." /> : (
                <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Image</th><th>Id</th><th>Size</th><th>Containers</th><th>Age</th></tr></thead><tbody>
                  {d.images.map((i) => (
                    <tr key={i.id}>
                      <td className="vm-name">{i.tags.length ? i.tags.map((t) => <div key={t} className="fx-mono">{t}</div>) : <span className="fx-muted">untagged</span>}</td>
                      <td className="fx-mono fx-muted">{i.id}</td>
                      <td>{fmtSize(i.size)}</td>
                      <td className="fx-muted">{i.containers > 0 ? i.containers : '—'}</td>
                      <td className="fx-muted">{ageSince(i.created, now)}</td>
                    </tr>
                  ))}
                </tbody></table></div>
              )}
            </Card>
          )}
        </>
      ) : null}
      {open ? <ContainerWindow container={open} onClose={() => setOpen(null)} /> : null}
    </>
  );
}
