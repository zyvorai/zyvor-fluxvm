// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Copy, Download, Rocket } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Pill, Reveal, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { CopyCmd } from './Images';
import { MacHomeCard } from './Mac';
import { BackupsCard } from './Backups';
import { useAct } from '../lib/act';
import { BUNDLE_COMMAND, BUNDLE_NOTE, bundleSummary, downloadSupportBundle } from '../lib/bundle';
import { stepTone, stepWord, upAdvice, upCommand, type UpPlan } from '../lib/up';
import { copyFor, lightWord, needs, tone, type ClusterHealth, type HealthItem, type MachineHealth } from '../lib/health';

function ItemRow({ i, notify }: { i: HealthItem; notify: Notify }) {
  const cmd = copyFor(i);
  const f = i.fix;
  return (
    <li>
      <Pill tone={tone(i.severity)}>{lightWord(i.severity)}</Pill>
      <span>
        {i.message}
        {i.cause ? <div className="fx-muted">Why: {i.cause}</div> : null}
        {f ? (
          <div className="fx-muted">
            {f.command ? <>Fix: <code className="fx-mono">{f.command}</code></> : f.steps ? <>Fix: {f.steps}</> : null}
            {needs(f).length ? <> · {needs(f).join(' · ')}</> : null}
          </div>
        ) : null}
        {i.explain ? <div className="fx-muted">More: <code className="fx-mono">fluxctl explain {i.explain}</code></div> : null}
      </span>
      {cmd ? <CopyCmd notify={notify} cmd={cmd} /> : null}
    </li>
  );
}

/** "Get started": the checklist `fluxctl up` works through, read-only, with the exact command to copy. */
function GetStarted({ notify }: { notify: Notify }) {
  const p = usePoll(() => api.get<UpPlan>('/v1/up/plan'), 15000);
  if (!p.data) {
    // An older daemon has no plan route; the command still works, so say so instead of hiding the feature.
    return <Card title="Get started"><p className="fx-muted">{p.loading ? 'Checking this machine.' : 'This daemon cannot show the plan. Run the command below on the machine; it checks everything itself.'}</p><CopyCmd notify={notify} cmd="fluxctl up --dry-run" /></Card>;
  }
  const d = p.data;
  return (
    <Card title="Get started" action={<Pill tone={d.nothing_to_do ? 'ok' : d.can_run ? 'warn' : 'bad'}>{d.nothing_to_do ? 'Set up' : d.can_run ? 'Ready to start' : 'Blocked'}</Pill>}>
      <p>{d.headline}</p>
      <p className="fx-muted">{upAdvice(d)}</p>
      <ul className="fx-feed">
        {d.steps.map((s) => (
          <li key={s.id}>
            <Pill tone={stepTone(s.state)}>{stepWord(s.state)}</Pill>
            <span>
              {s.title}: {s.detail}
              {s.state !== 'done' && s.fix ? <div className="fx-muted">Fix: {s.fix}</div> : null}
              {s.state !== 'done' && s.code ? <div className="fx-muted">More: <code className="fx-mono">fluxctl explain {s.code}</code></div> : null}
            </span>
          </li>
        ))}
      </ul>
      <p><CopyCmd notify={notify} cmd={upCommand(d)} /> <CopyCmd notify={notify} cmd={upCommand(d, true)} /></p>
      <p className="fx-muted">Then try:</p>
      <ul className="fx-feed">{d.next_commands.map((c) => <li key={c.command}><span>{c.why}</span><CopyCmd notify={notify} cmd={c.command} /></li>)}</ul>
      <p className="fx-muted">Studio only shows the plan. Run the copied command in a terminal on this machine.</p>
    </Card>
  );
}

function Machine({ m, notify }: { m: MachineHealth; notify: Notify }) {
  return (
    <Card title={<>{m.name}{m.local ? <span className="fx-muted"> (this machine)</span> : null}</>} action={<Pill tone={tone(m.light)}>{lightWord(m.light)}</Pill>}>
      <p>{m.headline}</p>
      <p className="fx-muted">{m.capacity.summary}</p>
      <p className="fx-muted">
        Last backup: {m.last_backup}
        {m.version ? <> · FluxVM {m.version}</> : null}
        {m.update ? <> · update to {m.update.wanted} available</> : null}
      </p>
      {m.items.length === 0 ? <p className="fx-muted">{m.checks_passed} checks passed. Nothing to do.</p> : <ul className="fx-feed">{m.items.map((i) => <ItemRow key={i.id} i={i} notify={notify} />)}</ul>}
    </Card>
  );
}

/** Download the support bundle: versions, health, recent logs, instances, config and host facts with secrets removed. */
function SupportBundle({ notify }: { notify: Notify }) {
  const { busy, run } = useAct(notify);
  const copy = () => navigator.clipboard.writeText(BUNDLE_COMMAND).then(() => notify('Copied: ' + BUNDLE_COMMAND), () => notify('Copy blocked by the browser', 'bad'));
  const download = () => void run('bundle', async () => {
    const d = await downloadSupportBundle();
    notify(bundleSummary(d.name, d.bytes, d.skipped));
  });
  return (
    <Card title="Support bundle" action={<Button size="sm" onClick={download} busy={busy === 'bundle'}><Download size={14} /> Download support bundle</Button>}>
      <p>One file with the versions, this health report, recent logs, your instances, the settings in effect and disk, memory and network facts. Tokens, passwords, keys and kubeconfig credentials are removed.</p>
      <p className="fx-muted">{BUNDLE_NOTE} If the daemon is stopped, run <code className="fx-mono">{BUNDLE_COMMAND}</code> in a terminal; it works without the daemon.</p>
      <Button size="sm" variant="ghost" onClick={() => void copy()}><Copy size={14} /> Copy as fluxctl</Button>
    </Card>
  );
}

/** The first page: is the cluster fine, in words anyone can read, with the next thing to do and a copy button for each fix. */
export function HomePage({ notify }: { notify: Notify }) {
  const [start, setStart] = useState(false);
  const h = usePoll(() => api.get<ClusterHealth>('/v1/cluster/health'), 10000);
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  if (h.loading && !h.data) return <><Hero eyebrow="Home" title="Checking your cluster" lede="One moment." /><Skeleton h={140} /></>;
  if (!h.data) {
    return (
      <>
        <Hero eyebrow="Home" title="Cluster health is not available" lede="This node did not answer the health request." />
        <EmptyState title="Could not read the health report" body={`${h.error?.message ?? 'No answer'}. An older FluxVM daemon does not have this page's data; run fluxctl cluster status on the machine instead.`} action={<Button onClick={() => void copy('fluxctl cluster status')}><Copy size={14} /> Copy as fluxctl</Button>} />
        <div className="fx-section"><SupportBundle notify={notify} /></div>
      </>
    );
  }
  const d = h.data;
  const next = d.next_action;
  return (
    <>
      <Hero eyebrow="Home" title={d.headline} lede={d.capacity.summary}>
        <Pill tone={tone(d.light)}>{lightWord(d.light)}</Pill>
        <Button onClick={() => void copy('fluxctl cluster status')}><Copy size={14} /> Copy: fluxctl cluster status</Button>
        <Button onClick={() => setStart((v) => !v)}><Rocket size={14} /> Get started</Button>
      </Hero>
      {start ? <Reveal><GetStarted notify={notify} /></Reveal> : null}
      <Reveal>
        <Card title="Next best action" action={next.safe ? <Button size="sm" onClick={() => void copy('fluxctl fix')}><Copy size={14} /> Copy: fluxctl fix</Button> : next.command ? <CopyCmd notify={notify} cmd={next.command} /> : undefined}>
          <p>{next.text}</p>
          <p className="fx-muted">Studio only shows and explains. Run the copied command in a terminal on the machine named; <code className="fx-mono">fluxctl fix --dry-run</code> shows what it would do first.</p>
        </Card>
      </Reveal>
      {d.items.length ? <div className="fx-section"><Card title="Across the cluster"><ul className="fx-feed">{d.items.map((i) => <ItemRow key={i.id} i={i} notify={notify} />)}</ul></Card></div> : null}
      <div className="fx-grid cols-2 fx-section">
        {d.machines.map((m, n) => <Reveal key={m.name} delay={n * 60}><Machine m={m} notify={notify} /></Reveal>)}
      </div>
      <MacHomeCard notify={notify} />
      <BackupsCard notify={notify} />
      <div className="fx-section"><SupportBundle notify={notify} /></div>
      <p className="fx-muted fx-section">This page shows the node you are connected to. For every Mac of a fleet, run <code className="fx-mono">fluxctl cluster status -f inventory.yaml</code>.</p>
    </>
  );
}
