// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { Copy } from 'lucide-react';
import { useState } from 'react';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { Button, Card, EmptyState, Hero, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { LABEL_HINT } from '../lib/labelFilter';
import { type FleetView, ageText, diskText, filterHosts, labelCommand, labelsText, primaryAction, slotsText, stateLabel, stateTone, thermalText, viewTone } from '../lib/fleet';

/** The Mac fleet as `fluxctl mac-fleet top` last saw it: capacity per Mac, with the fluxctl command for each action to copy. Studio only shows this; the daemon does not ssh into Macs. */
export function MacFleetPage({ notify }: { notify: Notify }) {
  const view = usePoll(async () => await api.get<FleetView>('/v1/mac-fleet'), 10000);
  const v = view.data;
  const [q, setQ] = useState('');
  const shown = filterHosts(v?.hosts ?? [], q);
  const forbidden = view.error?.message && /admin/i.test(view.error.message);
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  return (
    <>
      <Hero eyebrow="Mac fleet" title="Every Mac in the fleet," accent="and what each can take." lede="Running macOS guests against the two-per-Mac limit, thermal state and free disk. Studio reads what fluxctl mac-fleet top last saw; every action is a fluxctl command you copy and run." />
      {view.loading && !v ? (
        <Skeleton h={160} />
      ) : forbidden ? (
        <Card><EmptyState title="Admin only" body="The fleet belongs to the whole host, so only an administrator can see it." /></Card>
      ) : view.error && !v ? (
        <Card><EmptyState title="The daemon could not read the fleet" body={view.error.message + ' An older FluxVM daemon does not have this page; run fluxctl mac-fleet top instead.'} /></Card>
      ) : v ? (
        <>
          <Card title="Fleet" action={<CopyCmd notify={notify} cmd={v.refresh_command} />}>
            <p className="fx-muted"><Pill tone={viewTone(v)}>{v.status === 'live' ? 'live' : v.status === 'stale' ? 'stale' : v.status === 'empty' ? 'empty' : 'not checked'}</Pill> {v.message}</p>
            {v.taken_unix ? <p className="fx-muted">Last checked {ageText(v.age_secs)}. Refresh with <code className="fx-mono">{v.refresh_command}</code> (the button copies it).</p> : null}
          </Card>
          {v.hosts.length === 0 ? (
            <Card>
              <EmptyState title="No Macs yet" body="Add a Mac and FluxVM checks ssh, installs, and proves it can take jobs, telling you exactly what to fix." />
              <div className="fx-actions"><Button onClick={() => void copy(v.add_command)}><Copy size={14} /> Copy: {v.add_command}</Button></div>
            </Card>
          ) : (
            <Card title={`Macs · ${v.summary.macs}`}>
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', marginBottom: 12 }}>
                <Pill tone="ok">{v.summary.ready} ready</Pill>
                {v.summary.cordoned ? <Pill tone="warn">{v.summary.cordoned} cordoned</Pill> : null}
                {v.summary.unreachable ? <Pill tone="bad">{v.summary.unreachable} unreachable</Pill> : null}
                <Pill tone="info">{v.summary.slots_used}/{v.summary.slots_total} macOS slots busy</Pill>
              </div>
              <p><input className="fx-input" aria-label="Filter Macs" placeholder="Filter by name or zone=lab" value={q} onChange={(e) => setQ(e.target.value)} /> <span className="fx-muted">{LABEL_HINT}</span></p>
              {shown.length === 0 ? <p className="fx-muted">No Mac matches. Clear the filter to see all {v.hosts.length}, or label one: <code className="fx-mono">{labelCommand({ name: 'MAC' })}</code></p> : null}
              <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Mac</th><th>State</th><th>macOS VMs</th><th>Thermal</th><th>Disk</th><th>Power</th><th /></tr></thead><tbody>
                {shown.map((h) => {
                  const a = primaryAction(h);
                  return (
                    <tr key={h.name}>
                      <td className="vm-name">{h.name}<div className="fx-muted fx-mono">{h.host}</div>{labelsText(h) ? <div className="fx-muted fx-mono" aria-label={`Labels of ${h.name}`}>{labelsText(h)}</div> : null}{h.error ? <div className="fx-muted">{h.error}</div> : h.next ? <div className="fx-muted">{h.next}</div> : null}</td>
                      <td><Pill tone={stateTone(h.state)}>{stateLabel(h.state)}</Pill></td>
                      <td className="fx-mono">{slotsText(h)}</td>
                      <td>{h.thermal === 'throttled' ? <Pill tone="warn">{thermalText(h)}</Pill> : <span className="fx-muted">{thermalText(h)}</span>}</td>
                      <td className={h.disk_low ? '' : 'fx-muted'}>{h.disk_low ? <Pill tone="warn">{diskText(h)}</Pill> : diskText(h)}</td>
                      <td className="fx-muted">{h.on_ac == null ? '—' : h.on_ac ? 'ac' : 'battery'}</td>
                      <td><Button size="sm" variant="ghost" aria-label={`Label ${h.name}: copy as fluxctl`} title={labelCommand(h)} onClick={() => void copy(labelCommand(h))}><Copy size={14} />Label</Button>{a ? <Button size="sm" variant="ghost" aria-label={`${a.label} ${h.name}: copy as fluxctl`} title={a.command} onClick={() => void copy(a.command)}><Copy size={14} />{a.label}</Button> : null}</td>
                    </tr>
                  );
                })}
              </tbody></table></div>
            </Card>
          )}
        </>
      ) : null}
    </>
  );
}
