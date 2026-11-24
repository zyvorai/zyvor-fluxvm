// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useEffect, useState } from 'react';
import { ArrowUpCircle, ServerCog } from 'lucide-react';
import { Button, Card, Pill, Sheet, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { CopyCmd } from './Images';
import type { BackupListing } from '../lib/backups';
import {
  NODE_ADD_WARNING, NODE_REMOVE_WARNING, UPGRADE_WARNING, confirmed, explainCommand, failureText, nextStep, nodeAddPath, nodeRemovePath, opPath, opTone, opWord,
  upgradePath, upgradePlanPath, type KubeOp, type UpgradePlan,
} from '../lib/kubeOps';

type Dialog = { kind: 'upgrade'; plan: UpgradePlan } | { kind: 'add' } | { kind: 'remove' } | null;

/** Follows one started operation until it finishes (every 2 s). */
function useOp(first: KubeOp | null, onDone: () => void) {
  const [op, setOp] = useState<KubeOp | null>(first);
  useEffect(() => setOp(first), [first]);
  useEffect(() => {
    if (!op || op.state !== 'running') return;
    const t = setInterval(() => {
      api.get<KubeOp>(opPath(op.id)).then((o) => { setOp(o); if (o.state !== 'running') onDone(); }, () => undefined);
    }, 2000);
    return () => clearInterval(t);
  }, [op, onDone]);
  return op;
}

function Progress({ op, notify }: { op: KubeOp; notify: Notify }) {
  const fail = failureText(op);
  const next = nextStep(op);
  const explain = explainCommand(op);
  return (
    <div className="fx-section">
      <p><Pill tone={opTone(op.state)}>{opWord(op)}</Pill> <span className="fx-muted">{op.started_secs_ago} s</span></p>
      {op.lines.length ? <pre className="fx-mono fx-muted" style={{ maxHeight: 180, overflow: 'auto' }}>{op.lines.slice(-30).join('\n')}</pre> : null}
      {op.state === 'failed' ? (
        <>
          <p><b>What went wrong:</b> {fail || 'it stopped without a message; see the output above.'}</p>
          {next ? <p><b>What to do next:</b> {next}</p> : null}
          <p>{explain ? <CopyCmd notify={notify} cmd={explain} /> : null} <CopyCmd notify={notify} cmd={op.command} /></p>
        </>
      ) : null}
    </div>
  );
}

function ClusterOps({ name, notify }: { name: string; notify: Notify }) {
  const { busy, run } = useAct(notify);
  const [first, setFirst] = useState<KubeOp | null>(null);
  const op = useOp(first, () => undefined);
  const [dlg, setDlg] = useState<Dialog>(null);
  const [typed, setTyped] = useState('');
  const [node, setNode] = useState('');
  const [channel, setChannel] = useState('stable');
  const [removal, setRemoval] = useState<string | null>(null);
  const close = () => { setDlg(null); setTyped(''); };
  const running = op?.state === 'running';
  const preview = () => void run('plan', async () => {
    const plan = await api.get<UpgradePlan>(upgradePlanPath(name, channel.trim() || 'stable'));
    setDlg({ kind: 'upgrade', plan });
  });
  const start = async () => {
    if (!dlg) return;
    if (dlg.kind === 'upgrade') {
      const ok = await run('go', async () => setFirst(await api.post<KubeOp>(upgradePath(name), { channel: dlg.plan.channel, confirm: typed.trim() })), 'Upgrade started');
      if (ok) close();
    } else if (dlg.kind === 'add') {
      const ok = await run('go', async () => setFirst(await api.post<KubeOp>(nodeAddPath(name), { node: node.trim() || undefined, confirm: typed.trim() })), 'Adding a node');
      if (ok) close();
    } else {
      // Today the CLI answers with the steps (a refusal in words); show them rather than a red toast.
      try {
        await api.post(nodeRemovePath(name, node.trim()), { confirm: typed.trim() });
        notify('Node removed');
        close();
      } catch (e) {
        setRemoval((e as Error).message);
        close();
      }
    }
  };
  const target = dlg?.kind === 'remove' ? node.trim() : name;
  return (
    <Card title={<>Cluster operations: {name}</>} action={op ? <Pill tone={opTone(op.state)}>{opWord(op)}</Pill> : undefined}>
      <p className="fx-muted">The same as <code className="fx-mono">fluxctl kube upgrade</code> and <code className="fx-mono">fluxctl kube node</code>. You see what will happen before anything changes.</p>
      <p>
        <input className="fx-input" value={channel} onChange={(e) => setChannel(e.target.value)} aria-label="Release channel" placeholder="stable" style={{ width: 110 }} />{' '}
        <Button size="sm" onClick={preview} busy={busy === 'plan'} disabled={running}><ArrowUpCircle size={14} /> Upgrade k3s</Button>{' '}
        <Button size="sm" variant="ghost" onClick={() => { setNode(''); setTyped(''); setDlg({ kind: 'add' }); }} disabled={running}><ServerCog size={14} /> Add node</Button>{' '}
        <Button size="sm" variant="ghost" onClick={() => { setNode(''); setTyped(''); setDlg({ kind: 'remove' }); }} disabled={running}>Remove node</Button>
      </p>
      {op ? <Progress op={op} notify={notify} /> : null}
      {removal ? <div className="fx-section"><p><b>Removing a node:</b></p><pre className="fx-mono fx-muted" style={{ whiteSpace: 'pre-wrap' }}>{removal}</pre></div> : null}
      <Sheet open={!!dlg} title={dlg?.kind === 'upgrade' ? 'Upgrade k3s' : dlg?.kind === 'add' ? 'Add a node' : 'Remove a node'} onClose={close}>
        {dlg?.kind === 'upgrade' ? (
          <>
            <pre className="fx-mono" style={{ whiteSpace: 'pre-wrap' }}>{dlg.plan.plan}</pre>
            {dlg.plan.nothing_to_do ? <p>Everything is already on this version. Nothing to do.</p> : <p className="fx-muted">{UPGRADE_WARNING}</p>}
          </>
        ) : null}
        {dlg?.kind === 'add' ? (
          <>
            <p className="fx-muted">{NODE_ADD_WARNING}</p>
            <input className="fx-input" value={node} onChange={(e) => setNode(e.target.value)} placeholder={`${name}-a1 (default)`} aria-label="Node name" />
          </>
        ) : null}
        {dlg?.kind === 'remove' ? (
          <>
            <p className="fx-muted">{NODE_REMOVE_WARNING}</p>
            <input className="fx-input" value={node} onChange={(e) => { setNode(e.target.value); setTyped(''); }} placeholder="node name" aria-label="Node name" />
          </>
        ) : null}
        {dlg && !(dlg.kind === 'upgrade' && dlg.plan.nothing_to_do) ? (
          <>
            <p>Type <code className="fx-mono">{target || 'the node name'}</code> to confirm:</p>
            <input className="fx-input" value={typed} onChange={(e) => setTyped(e.target.value)} placeholder={target} aria-label="Confirm name" />
          </>
        ) : null}
        <p>
          <Button variant="ghost" onClick={close}>Cancel</Button>{' '}
          {dlg && !(dlg.kind === 'upgrade' && dlg.plan.nothing_to_do) ? (
            <Button variant={dlg.kind === 'add' ? 'primary' : 'danger'} disabled={!target || !confirmed(typed, target)} busy={busy === 'go'} onClick={() => void start()}>
              {dlg.kind === 'upgrade' ? 'Back up and upgrade' : dlg.kind === 'add' ? 'Add node' : 'Show the steps'}
            </Button>
          ) : null}
        </p>
        {dlg?.kind === 'upgrade' ? <p className="fx-muted">Prefer a terminal? <CopyCmd notify={notify} cmd={dlg.plan.command} /></p> : null}
      </Sheet>
    </Card>
  );
}

/** The Cluster operations card on Home: k3s upgrade and node add / remove for every cluster on this host, with a preview, a typed
 *  confirmation, progress and plain-English failures with the explain command. */
export function KubeOpsCard({ notify }: { notify: Notify }) {
  const p = usePoll(() => api.get<BackupListing>('/v1/k3s/backups'), 60000);
  if (!p.data || p.data.clusters.length === 0) return null;
  return <>{p.data.clusters.map((c) => <div className="fx-section" key={c.name}><ClusterOps name={c.name} notify={notify} /></div>)}</>;
}
