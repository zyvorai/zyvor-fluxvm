// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Copy, DatabaseBackup, ShieldCheck } from 'lucide-react';
import { Button, Card, Pill, Sheet, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { CopyCmd } from './Images';
import {
  backupBase, freshness, kindWord, restoreCommand, restoreConfirmed, scheduleWord, sizeWord, ageWord,
  type BackupEntry, type BackupListing, type ClusterBackups, type VerifyReport,
} from '../lib/backups';

function Cluster({ c, notify, reload }: { c: ClusterBackups; notify: Notify; reload: () => void }) {
  const { busy, run } = useAct(notify);
  const [pick, setPick] = useState<BackupEntry | null>(null);
  const [typed, setTyped] = useState('');
  const [step, setStep] = useState<1 | 2>(1);
  const [report, setReport] = useState<VerifyReport | null>(null);
  const f = freshness(c);
  const close = () => { setPick(null); setTyped(''); setStep(1); };
  const backupNow = () => void run('now', () => api.post(backupBase(c.name), {}), 'Backup taken and checked', async () => reload());
  const verify = (b: BackupEntry) => void run('verify:' + b.name, async () => {
    const r = await api.post<VerifyReport>(`${backupBase(c.name)}/${encodeURIComponent(b.name)}/verify`, {});
    setReport(r);
    notify(r.restorable ? `${b.name} is restorable` : `${b.name} would not restore: ${r.checks.find((x) => !x.ok)?.detail ?? 'see the checks'}`, r.restorable ? undefined : 'bad');
  });
  const toggle = () => void run('sched', () => api.post(`${backupBase(c.name)}/schedule`, { enabled: !(c.schedule?.enabled ?? false) }), c.schedule?.enabled ? 'Automatic backups are off' : 'Automatic backups are on', async () => reload());
  const doRestore = async () => {
    if (!pick) return;
    const ok = await run('restore', () => api.post(`${backupBase(c.name)}/${encodeURIComponent(pick.name)}/restore`, { confirm: typed.trim() }), `Restored ${c.name} from ${pick.name}`, async () => reload());
    if (ok) close();
  };
  return (
    <Card title={<>Backups: {c.name}</>} action={<Pill tone={f.tone}>{f.text}</Pill>}>
      <p className="fx-muted">{scheduleWord(c.schedule)} {c.disk_free_bytes !== null ? <>{sizeWord(c.disk_free_bytes)} free on the disk that holds them.</> : null}</p>
      <p>
        <Button size="sm" onClick={backupNow} busy={busy === 'now'}><DatabaseBackup size={14} /> Back up now</Button>{' '}
        <Button size="sm" variant="ghost" onClick={toggle} busy={busy === 'sched'}>{c.schedule?.enabled ? 'Turn automatic backups off' : 'Turn automatic backups on'}</Button>{' '}
        <CopyCmd notify={notify} cmd={c.commands.backup} />
      </p>
      {c.backups.length === 0 ? <p className="fx-muted">No backups yet. Back up now takes one (SQLite clusters pause for a few seconds).</p> : (
        <ul className="fx-feed">
          {c.backups.map((b) => (
            <li key={b.name}>
              <Pill tone={b.has_checksum ? 'idle' : 'warn'}>{kindWord(b.kind)}</Pill>
              <span>
                <code className="fx-mono">{b.name}</code>
                <div className="fx-muted">{ageWord(b.age_secs)} · {sizeWord(b.bytes)}{b.has_checksum ? '' : ' · no checksum, cannot be restored'}</div>
              </span>
              <span>
                <Button size="sm" variant="ghost" onClick={() => verify(b)} busy={busy === 'verify:' + b.name}><ShieldCheck size={14} /> Verify</Button>{' '}
                <Button size="sm" variant="danger" disabled={!b.has_checksum} onClick={() => { setPick(b); setStep(1); setTyped(''); }}>Restore</Button>
              </span>
            </li>
          ))}
        </ul>
      )}
      {report ? (
        <div className="fx-muted">
          <b>{report.restorable ? 'Restorable' : 'Would not restore'}</b>: {report.file.split('/').pop()}
          <ul>{report.checks.map((k) => <li key={k.name}>{k.ok ? 'ok' : 'FAIL'} · {k.name}: {k.detail}</li>)}</ul>
        </div>
      ) : null}
      <p className="fx-muted">Each backup holds the cluster token: keep the folder private. The state a restore replaces is backed up first.</p>
      <Sheet open={!!pick} title="Restore this backup" onClose={close}>
        {pick && step === 1 ? (
          <>
            <p>Restore <b>{c.name}</b> from <code className="fx-mono">{pick.name}</code> ({ageWord(pick.age_secs)})?</p>
            <p className="fx-muted">The cluster goes back to how it was then: everything changed since is lost, and the cluster is unavailable for a minute or two. The current state is backed up first.</p>
            <Button onClick={close} variant="ghost">Cancel</Button>{' '}
            <Button variant="danger" onClick={() => setStep(2)}>Continue</Button>
          </>
        ) : null}
        {pick && step === 2 ? (
          <>
            <p>Type the backup's name to confirm:</p>
            <p><code className="fx-mono">{pick.name}</code></p>
            <input className="fx-input" value={typed} onChange={(e) => setTyped(e.target.value)} placeholder={pick.name} aria-label="Backup name" />
            <p>
              <Button onClick={close} variant="ghost">Cancel</Button>{' '}
              <Button variant="danger" disabled={!restoreConfirmed(typed, pick.name)} busy={busy === 'restore'} onClick={() => void doRestore()}>Restore now</Button>
            </p>
            <p className="fx-muted">Prefer a terminal? <Button size="sm" variant="ghost" onClick={() => void navigator.clipboard.writeText(restoreCommand(c.name, pick.name)).then(() => notify('Copied: ' + restoreCommand(c.name, pick.name)), () => notify('Copy blocked by the browser', 'bad'))}><Copy size={14} /> Copy as fluxctl</Button></p>
          </>
        ) : null}
      </Sheet>
    </Card>
  );
}

/** The Backups card on Home: every k3s cluster's backups, a Back up now button, verify, and a two-step restore. */
export function BackupsCard({ notify }: { notify: Notify }) {
  const p = usePoll(() => api.get<BackupListing>('/v1/k3s/backups'), 20000);
  if (!p.data) {
    return <Card title="Backups"><p className="fx-muted">{p.loading ? 'Looking for backups.' : 'This daemon cannot list cluster backups (it needs an admin token, or is older than this page). Run the command below on the machine.'}</p><CopyCmd notify={notify} cmd="fluxctl kube backups" /></Card>;
  }
  if (p.data.clusters.length === 0) {
    return <Card title="Backups"><p className="fx-muted">No k3s cluster here yet. Backups start on their own once one exists (<code className="fx-mono">fluxctl up</code>).</p></Card>;
  }
  return <>{p.data.clusters.map((c) => <div className="fx-section" key={c.name}><Cluster c={c} notify={notify} reload={() => void p.refresh()} /></div>)}</>;
}
