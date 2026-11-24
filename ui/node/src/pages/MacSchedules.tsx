// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Play, Trash2 } from 'lucide-react';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { Button, Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import {
  EMPTY_FORM, INSTALL_COMMAND, LIST_COMMAND, NOTIFY_TEST_COMMAND, PRESETS, addCommand, explainScheduleError, formProblem, nextText, rmCommand, runNowCommand, statusText, statusTone, toBody,
  type MacScheduleList, type ScheduleForm,
} from '../lib/macSchedule';

const enc = encodeURIComponent;

/** Schedules: run a Mac image on a cron expression. The same list and file as `fluxctl mac schedule`. */
export function MacSchedules({ notify }: { notify: Notify }) {
  const list = usePoll(() => api.get<MacScheduleList>('/v1/mac/schedules'), 10000);
  const [form, setForm] = useState<ScheduleForm>(EMPTY_FORM);
  const [busy, setBusy] = useState('');
  const [ask, setAsk] = useState('');
  const d = list.data;
  const problem = formProblem(form);
  const now = Date.now();

  const call = async (key: string, fn: () => Promise<unknown>, done: string) => {
    setBusy(key);
    try {
      await fn();
      notify(done);
      await list.refresh();
    } catch (e) {
      notify(explainScheduleError((e as Error).message), 'bad');
    } finally {
      setBusy('');
    }
  };
  const set = <K extends keyof ScheduleForm>(k: K, v: ScheduleForm[K]) => setForm((f) => ({ ...f, [k]: v }));

  if (list.loading && !d) return <Skeleton h={120} />;
  if (!d) {
    return (
      <Card title="Schedules">
        <EmptyState title="The daemon could not list schedules" body={`${list.error ? explainScheduleError(list.error.message) : 'No answer'} An older FluxVM daemon has no schedules: update it, or run ${LIST_COMMAND} on the Mac.`} action={<CopyCmd notify={notify} cmd={LIST_COMMAND} />} />
      </Card>
    );
  }
  return (
    <>
      <Card title="Schedules" action={<CopyCmd notify={notify} cmd={LIST_COMMAND} />}>
        {d.schedules.length === 0 ? (
          <EmptyState title="No schedules" body="Add one below, or run fluxctl mac schedule add NAME '0 3 * * *' IMAGE -- CMD." action={<CopyCmd notify={notify} cmd={d.commands.add} />} />
        ) : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Name</th><th>When</th><th>Runs</th><th>Next</th><th>Last</th><th /></tr></thead><tbody>
            {d.schedules.map((s) => (
              <tr key={s.name}>
                <td className="vm-name">{s.name}<div className="fx-muted">{s.notify ? 'banner' : ''}{s.notify && s.webhook ? ' + ' : ''}{s.webhook ? `webhook ${s.webhook}` : ''}</div></td>
                <td className="fx-mono">{s.cron}</td>
                <td className="fx-mono">{s.image}{s.cmd.length ? ` -- ${s.cmd.join(' ')}` : ''}{s.ttl ? <div className="fx-muted">removed after {s.ttl}</div> : null}</td>
                <td className="fx-muted">{s.enabled ? nextText(s.next_run_unix, now) : '—'}</td>
                <td><Pill tone={statusTone(s)}>{statusText(s)}</Pill>{s.last_detail ? <div className="fx-muted">{s.last_detail.slice(0, 140)}</div> : null}</td>
                <td><span className="fx-row-actions">
                  <Button size="sm" variant="ghost" aria-label={`Run ${s.name} now`} busy={busy === s.name + 'run'} disabled={!!busy || !s.enabled} onClick={() => void call(s.name + 'run', () => api.post(`/v1/mac/schedules/${enc(s.name)}/run-now`), `${s.name} will run at the next tick (within about 30 seconds)`)}><Play size={14} />Run now</Button>
                  <CopyCmd notify={notify} cmd={runNowCommand(s.name)} />
                  <Button size="sm" variant="ghost" aria-label={`${s.enabled ? 'Turn off' : 'Turn on'} ${s.name}`} disabled={!!busy} onClick={() => void call(s.name + 'en', () => api.post(`/v1/mac/schedules/${enc(s.name)}/${s.enabled ? 'disable' : 'enable'}`), `${s.name} is ${s.enabled ? 'off' : 'on'}`)}>{s.enabled ? 'Turn off' : 'Turn on'}</Button>
                  <Button size="sm" variant="ghost" aria-label={`Remove ${s.name}`} disabled={!!busy} onClick={() => { if (ask !== s.name) { setAsk(s.name); return; } setAsk(''); void call(s.name + 'rm', () => api.del(`/v1/mac/schedules/${enc(s.name)}`), `${s.name} removed`); }}><Trash2 size={14} />{ask === s.name ? 'Sure? Remove' : 'Remove'}</Button>
                  <CopyCmd notify={notify} cmd={rmCommand(s.name)} />
                </span></td>
              </tr>
            ))}
          </tbody></table></div>
        )}
        <p className="fx-muted">{d.note} <CopyCmd notify={notify} cmd={INSTALL_COMMAND} /> Times are this Mac's local time. A banner needs notifications allowed for Script Editor: <code className="fx-mono">{NOTIFY_TEST_COMMAND}</code> checks.</p>
      </Card>
      <Card title="Add a schedule" action={<CopyCmd notify={notify} cmd={addCommand(form)} />}>
        <div style={{ display: 'grid', gap: 8, gridTemplateColumns: 'repeat(auto-fit, minmax(200px, 1fr))' }}>
          <label>Name<input className="fx-input" aria-label="Schedule name" value={form.name} onChange={(e) => set('name', e.target.value)} placeholder="nightly-build" /></label>
          <label>Image<input className="fx-input" aria-label="Image" value={form.image} onChange={(e) => set('image', e.target.value)} placeholder="macos-base:15" /></label>
          <label>When (cron)<input className="fx-input fx-mono" aria-label="Cron expression" value={form.cron} onChange={(e) => set('cron', e.target.value)} /></label>
          <label>Command (optional)<input className="fx-input fx-mono" aria-label="Command" value={form.cmd} onChange={(e) => set('cmd', e.target.value)} placeholder="./build.sh" /></label>
          <label>Time limit (optional)<input className="fx-input" aria-label="Time limit" value={form.ttl} onChange={(e) => set('ttl', e.target.value)} placeholder="2h" /></label>
          <label>Webhook (optional)<input className="fx-input" type="password" autoComplete="off" aria-label="Webhook URL" value={form.webhook} onChange={(e) => set('webhook', e.target.value)} placeholder="https://hooks.example.com/..." /></label>
        </div>
        <p style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
          {PRESETS.map((p) => <Button key={p.cron} size="sm" variant="ghost" onClick={() => set('cron', p.cron)}>{p.label}</Button>)}
          <label><input type="checkbox" checked={form.notify} onChange={(e) => set('notify', e.target.checked)} /> macOS banner when it ends</label>
        </p>
        {problem && form.name ? <p><Pill tone="warn">check</Pill> {problem}</p> : null}
        <Button disabled={!!problem || !!busy} busy={busy === 'add'} onClick={() => void call('add', async () => { await api.post('/v1/mac/schedules', toBody(form)); setForm(EMPTY_FORM); }, `${form.name} added`)}>Add schedule</Button>
        <p className="fx-muted">Without a command an instance is started and removed after the time limit. With one, the VM is removed when it ends. The webhook is stored on the Mac and never shown again; the copied command uses a placeholder for it.</p>
      </Card>
    </>
  );
}
