// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { ShieldCheck, Trash2 } from 'lucide-react';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { Button, Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import {
  LS_COMMAND, ageWords, bytesText, canBackUp, createCommand, everyWords, explainBackupError, roomWords, scheduleCommand, stateTone, stateWord,
  type BackupRow, type BackupsView,
} from '../lib/macBackups';

/** Backups of the Mac's instances and named volumes: the same files `fluxctl mac backup` writes. Restore stays a copied command. */
export function BackupsCard({ notify }: { notify: Notify }) {
  const view = usePoll(() => api.get<BackupsView>('/v1/mac/backups'), 5000);
  const d = view.data;
  const [pick, setPick] = useState('');
  const [every, setEvery] = useState('24h');
  const [busy, setBusy] = useState('');
  const [ask, setAsk] = useState('');
  const [open, setOpen] = useState('');

  if (!d) {
    return view.loading ? <Skeleton h={120} /> : (
      <Card title="Backups" action={<CopyCmd notify={notify} cmd={LS_COMMAND} />}>
        <EmptyState title="Backups are not available" body={`${view.error?.message ?? 'No answer'}. An older FluxVM daemon has no backups API: update it, or run ${LS_COMMAND} on the Mac.`} />
      </Card>
    );
  }
  const room = roomWords(d);
  const targets = [
    ...d.instances.map((i) => ({ key: `instance:${i.name}`, kind: 'instance' as const, name: i.name, label: `instance ${i.name}${canBackUp(i.status) ? '' : ` (${i.status}: stop it first)`}`, ok: canBackUp(i.status) })),
    ...d.volumes.map((v) => ({ key: `volume:${v}`, kind: 'volume' as const, name: v, label: `volume ${v}`, ok: true })),
  ];
  const target = targets.find((t) => t.key === pick);

  const run = async (key: string, what: string, f: () => Promise<unknown>) => {
    setBusy(key);
    try {
      await f();
      notify(what);
      await view.refresh();
    } catch (e) {
      notify(explainBackupError(key, (e as Error).message), 'bad');
    } finally {
      setBusy('');
    }
  };

  const remove = (b: BackupRow) => {
    if (ask !== b.id) return setAsk(b.id);
    setAsk('');
    void run(b.id, `Removed ${b.id}`, () => api.del(`/v1/mac/backups/${encodeURIComponent(b.id)}`));
  };

  return (
    <Card title="Backups" action={<CopyCmd notify={notify} cmd={LS_COMMAND} />}>
      <p><Pill tone={room.tone}>{room.tone === 'bad' ? 'disk full' : 'disk'}</Pill> {room.text}. <span className="fx-muted">Kept in <span className="fx-mono">{d.root}</span>.</span></p>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', margin: '8px 0 12px' }}>
        <select className="fx-input" aria-label="What to back up" value={pick} onChange={(e) => setPick(e.target.value)}>
          <option value="">Back up…</option>
          {targets.map((t) => <option key={t.key} value={t.key} disabled={!t.ok}>{t.label}</option>)}
        </select>
        <Button size="sm" disabled={!target || busy !== ''} onClick={() => target && void run('create', `Backing up ${target.name} in the background`, () => api.post('/v1/mac/backups', { kind: target.kind, name: target.name }))}>Back up now</Button>
        <input className="fx-input" aria-label="Every" style={{ width: 70 }} value={every} onChange={(e) => setEvery(e.target.value)} />
        <Button size="sm" variant="ghost" disabled={!target || busy !== ''} onClick={() => target && void run('schedule', `${target.name} will be backed up every ${every}`, () => api.post('/v1/mac/backup-schedules', { kind: target.kind, name: target.name, every }))}>Schedule</Button>
        {target ? <><CopyCmd notify={notify} cmd={createCommand(target.kind, target.name)} /><CopyCmd notify={notify} cmd={scheduleCommand(target.kind, target.name, every)} /></> : null}
      </div>
      {d.backups.length === 0 ? (
        <EmptyState title="No backups yet" body="Stop an instance and back it up, or back up a volume. A backup is a compressed archive with a checksum; the oldest are removed after the newest 7 of each." action={<CopyCmd notify={notify} cmd="fluxctl mac backup create NAME" />} />
      ) : (
        <ul className="fx-feed">
          {d.backups.map((b) => (
            <li key={b.id}>
              <Pill tone={stateTone(b.state)}>{stateWord(b.state)}</Pill>
              <span>
                <b>{b.kind ?? ''} {b.name ?? b.id}</b> <span className="fx-muted fx-mono">{b.id}</span>
                <br /><span className="fx-muted">{b.manifest ? `${bytesText(b.manifest.archive_bytes)} · ${ageWords(b.age_seconds)}${b.manifest.automatic ? ' · automatic safety net' : ''}${b.manifest.note ? ` · ${b.manifest.note}` : ''}` : (b.error ?? '')}</span>
                {open === b.id && b.manifest ? (
                  <><br /><span className="fx-muted">Holds: {b.manifest.captured.join('; ')}.</span><br /><span className="fx-muted">Never holds: {b.manifest.excluded.join('; ')}.</span><br /><span className="fx-muted fx-mono">sha256 {b.manifest.sha256}</span></>
                ) : null}
              </span>
              {b.manifest ? <Button size="sm" variant="ghost" onClick={() => setOpen(open === b.id ? '' : b.id)}>{open === b.id ? 'Less' : 'Details'}</Button> : null}
              {b.state === 'ok' ? <Button size="sm" variant="ghost" disabled={busy === b.id} title="Check the archive against its checksum" onClick={() => void run(b.id, `${b.id}: checksum matches`, () => api.post(`/v1/mac/backups/${encodeURIComponent(b.id)}/verify`))}><ShieldCheck size={14} /> Verify</Button> : null}
              {b.state === 'ok' ? <CopyCmd notify={notify} cmd={b.restore} /> : null}
              <Button size="sm" variant="ghost" disabled={busy === b.id} aria-label={ask === b.id ? 'Confirm delete' : 'Delete backup'} onClick={() => remove(b)}><Trash2 size={14} />{ask === b.id ? ' Really?' : ''}</Button>
            </li>
          ))}
        </ul>
      )}
      {d.schedules.length ? (
        <ul className="fx-feed">
          {d.schedules.map((s) => (
            <li key={s.kind + s.name}>
              <Pill tone={s.last_error ? 'bad' : 'ok'}>{s.last_error ? 'last run failed' : 'scheduled'}</Pill>
              <span>{s.kind} <b>{s.name}</b> every {everyWords(s.every_secs)}, keep {s.keep}{s.last_error ? <><br /><span className="fx-muted">{s.last_error}</span></> : null}</span>
              <Button size="sm" variant="ghost" onClick={() => void run(s.name, `Stopped the schedule of ${s.name}`, () => api.post('/v1/mac/backup-schedules', { kind: s.kind, name: s.name, off: true }))}>Stop</Button>
            </li>
          ))}
        </ul>
      ) : null}
      <p className="fx-muted">Never in a backup: {d.excluded.join('; ')}. Restoring is a terminal command (copy it from a backup): it never overwrites an existing instance without <code className="fx-mono">--replace</code>, and saves what it replaces first.</p>
    </Card>
  );
}
