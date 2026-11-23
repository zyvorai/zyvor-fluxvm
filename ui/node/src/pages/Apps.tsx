// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Copy, Minus, Plus, Play, ScrollText, SquareTerminal, Trash2 } from 'lucide-react';
import type { Notify } from '../App';
import { Button, Card, EmptyState, Field, Hero, Pill, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import { type App, type Apps, type RunForm, appCommands, appTone, appsPath, appsSummary, clampCopies, deleteAppPath, emptyRunForm, runBody, runCommand, runProblem, scalePath } from '../lib/apps';
import { type KubeClusterItem, kubeProblem, shortImage } from '../lib/kube';
import { PodWindow, type PodTab } from './PodWindow';

/** Images that run on a cluster like `docker run`: start one, watch it, read its logs, scale it, remove it. */
export function AppsPage({ notify }: { notify: Notify }) {
  const clusters = usePoll(async () => (await api.get<{ items: KubeClusterItem[] }>('/v1/kube')).items ?? [], 8000);
  const items = clusters.data ?? [];
  const [picked, setPicked] = useState('');
  const current = items.find((c) => c.name === picked)?.name ?? items[0]?.name ?? '';
  const poll = usePoll(async () => (current ? await api.get<Apps>(appsPath(current)) : null), 4000, [current]);
  const [form, setForm] = useState<RunForm>(emptyRunForm);
  const [busy, setBusy] = useState('');
  const [confirm, setConfirm] = useState('');
  const [open, setOpen] = useState<{ app: App; tab: PodTab } | null>(null);
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  const d = poll.data;
  const problem = form.image.trim() ? runProblem(form) : null;

  const run = async () => {
    const p = runProblem(form);
    if (p) return notify(p, 'bad');
    setBusy('run');
    try {
      const r = await api.post<{ name: string; result: string }>(appsPath(current), runBody(form));
      notify(`${r.name} ${r.result}. It is starting; the list below shows when it is ready.`);
      setForm(emptyRunForm);
      await poll.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    }
    setBusy('');
  };
  const scale = async (a: App, n: number) => {
    setBusy(`${a.namespace}/${a.name}scale`);
    try {
      await api.post(scalePath(current, a), { replicas: clampCopies(n) });
      notify(n === 0 ? `${a.name} stopped` : `${a.name} scaled to ${n}`);
      await poll.refresh();
    } catch (e) {
      notify(`${a.name}: ${(e as Error).message}`, 'bad');
    }
    setBusy('');
  };
  const remove = async (a: App) => {
    setBusy(`${a.namespace}/${a.name}delete`);
    try {
      await api.post(deleteAppPath(current, a), {});
      notify(`${a.name} removed`);
      await poll.refresh();
    } catch (e) {
      notify(`${a.name}: ${(e as Error).message}`, 'bad');
    }
    setBusy('');
    setConfirm('');
  };

  return (
    <>
      <Hero eyebrow="Apps" title="Run an image," accent="like docker run." lede="Start a container image on a cluster, see whether it is healthy in plain words, read its logs, scale it and remove it. The same as fluxctl kube run." />
      {clusters.loading && !items.length ? (
        <Skeleton h={120} />
      ) : items.length === 0 ? (
        <Card>
          <EmptyState title="No cluster to run apps on" body="Create one with fluxctl kube up (or fluxctl up). Then start an app with fluxctl kube run nginx, or from here." />
          <Button size="sm" variant="ghost" onClick={() => void copy('fluxctl kube up')}><Copy size={14} />Copy as fluxctl</Button>
        </Card>
      ) : (
        <>
          {items.length > 1 ? <Tabs value={current} onChange={(v) => setPicked(v)} tabs={items.map((c) => ({ value: c.name, label: c.name }))} /> : null}
          <Card title="Run an app">
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(160px, 1fr))', gap: 12 }}>
              <Field label="Image" hint="For example nginx or ghcr.io/you/app:1.0">{(id) => <input id={id} className="fx-input" value={form.image} onChange={(e) => setForm({ ...form, image: e.target.value })} placeholder="nginx" />}</Field>
              <Field label="Name" hint="Default: from the image">{(id) => <input id={id} className="fx-input" value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} placeholder="web" />}</Field>
              <Field label="Port" hint="What it listens on">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={form.port} onChange={(e) => setForm({ ...form, port: e.target.value })} placeholder="80" />}</Field>
              <Field label="Copies">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={form.replicas} onChange={(e) => setForm({ ...form, replicas: e.target.value })} />}</Field>
            </div>
            {problem ? <p className="fx-muted" role="status">{problem}</p> : null}
            <div style={{ display: 'flex', gap: 8, marginTop: 12 }}>
              <Button variant="primary" busy={busy === 'run'} disabled={!form.image.trim() || !!problem || !!busy} onClick={() => void run()}><Play size={14} />Run</Button>
              <Button variant="ghost" onClick={() => void copy(runCommand(current, form))}><Copy size={14} />Copy as fluxctl</Button>
            </div>
          </Card>
          {poll.error && !d ? (
            <Card><EmptyState title={kubeProblem(poll.error.message).title} body={kubeProblem(poll.error.message).body} /></Card>
          ) : !d ? (
            <Skeleton h={160} />
          ) : d.error ? (
            <Card>
              <EmptyState title="Could not read this cluster's apps" body={`${d.error.message} Next, run: ${d.error.next}`} />
              <Button size="sm" variant="ghost" onClick={() => void copy(d.error!.next)}><Copy size={14} />Copy as fluxctl</Button>
            </Card>
          ) : d.apps.length === 0 ? (
            <Card><EmptyState title="No apps yet" body="Type an image above and press Run, or use fluxctl kube run nginx. Databases and tools you use every day are under fluxctl kube add." /></Card>
          ) : (
            <Card title={appsSummary(d.apps)}>
              <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>Image</th><th>Status</th><th>Port</th><th /></tr></thead><tbody>
                {d.apps.map((a) => {
                  const key = `${a.namespace}/${a.name}`;
                  const cmd = appCommands(current, a);
                  const first = a.pods[0];
                  return (
                    <tr key={key}>
                      <td className="vm-name">{a.name}<div className="fx-muted">{a.namespace}</div></td>
                      <td className="fx-mono fx-muted">{shortImage(a.image)}</td>
                      <td>
                        <Pill tone={appTone(a.state)}>{a.state === 'running' ? `${a.ready}/${a.desired} running` : a.state === 'starting' ? `starting ${a.ready}/${a.desired}` : a.state}</Pill>
                        {a.state === 'problem' || a.state === 'stopped' ? <div className="fx-muted" style={{ maxWidth: 420 }}>{a.summary}{a.next ? ` Next: ${a.next}` : ''}{a.code ? ` (fluxctl explain ${a.code})` : ''}</div> : null}
                      </td>
                      <td className="fx-mono">{a.port ?? '—'}</td>
                      <td><span className="fx-row-actions">
                        <Button size="sm" variant="ghost" aria-label={`Logs of ${a.name}`} disabled={!first} onClick={() => setOpen({ app: a, tab: 'logs' })}><ScrollText size={14} />Logs</Button>
                        <Button size="sm" variant="ghost" aria-label={`Shell in ${a.name}`} disabled={!first} onClick={() => setOpen({ app: a, tab: 'shell' })}><SquareTerminal size={14} />Shell</Button>
                        <Button size="sm" variant="ghost" aria-label={`Fewer copies of ${a.name}`} disabled={!!busy || a.desired <= 0} busy={busy === key + 'scale'} onClick={() => void scale(a, a.desired - 1)}><Minus size={14} /></Button>
                        <Button size="sm" variant="ghost" aria-label={`More copies of ${a.name}`} disabled={!!busy} onClick={() => void scale(a, a.desired + 1)}><Plus size={14} /></Button>
                        {confirm === key ? (
                          <Button size="sm" variant="danger" aria-label={`Confirm removing ${a.name}`} busy={busy === key + 'delete'} onClick={() => void remove(a)}>Remove {a.name}?</Button>
                        ) : (
                          <Button size="sm" variant="ghost" aria-label={`Remove ${a.name}`} disabled={!!busy} onClick={() => setConfirm(key)}><Trash2 size={14} />Remove</Button>
                        )}
                        <Button size="sm" variant="ghost" aria-label={`Copy fluxctl logs command for ${a.name}`} title={cmd.logs} onClick={() => void copy(cmd.logs)}><Copy size={14} />Copy as fluxctl</Button>
                      </span></td>
                    </tr>
                  );
                })}
              </tbody></table></div>
            </Card>
          )}
          {open && open.app.pods[0] ? <PodWindow cluster={current} pod={open.app.pods[0]} tab={open.tab} onClose={() => setOpen(null)} /> : null}
        </>
      )}
    </>
  );
}
