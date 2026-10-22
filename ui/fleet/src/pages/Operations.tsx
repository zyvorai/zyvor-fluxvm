import { useState } from 'react';
import { Button, Card, Field, Hero, KV, Pill, Sheet, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { GenericTable, flatten, rowsOf } from '../lib/generic';
import { type Ha, changedKeys, haSummary, parsePolicy, rebalancePath, rebalanceProblem } from '../lib/ops';

/** Operator-only controls: leader status, reconcile, rebalance planning, metrics collection and the fleet policy. */
export function OperationsPage({ notify }: Ctx) {
  const ha = usePoll(() => api.get<Ha>('/fleet/ha').catch(() => null), 8000);
  const policy = usePoll(() => api.get<Record<string, unknown>>('/fleet/policy').catch(() => null), 30000);
  const { busy, run } = useAct(notify);
  const [result, setResult] = useState<{ title: string; data: unknown } | null>(null);
  const [q, setQ] = useState({ highWater: '', maxMoves: '' });
  const [plan, setPlan] = useState<unknown>(null);
  const [edit, setEdit] = useState<string | null>(null);
  const planErr = rebalanceProblem(q);
  const parsed = edit == null ? null : parsePolicy(edit);
  const changed = parsed?.ok && policy.data ? changedKeys(policy.data, parsed.value) : [];
  const moves = plan && typeof plan === 'object' ? rowsOf(plan, 'moves') : [];
  return (
    <>
      <Hero eyebrow="Operations" title="Keep the fleet" accent="in shape." lede="Who leads, a reconcile pass on demand, a look at what a rebalance would move, and the fleet-wide policy." />
      <div className="fx-grid cols-2">
        <Card title="High availability">
          {ha.loading && !ha.data ? <Skeleton h={60} /> : ha.data ? (
            <>
              <p>{ha.data.enabled ? (ha.data.is_leader ? <Pill tone="ok">leader</Pill> : <Pill tone="warn">standby</Pill>) : <Pill tone="idle">single</Pill>} {haSummary(ha.data)}</p>
              {ha.data.me ? <p className="fx-muted fx-mono">{ha.data.me}</p> : null}
            </>
          ) : <p className="fx-muted">Not reported.</p>}
        </Card>
        <Card title="Run now">
          <p className="fx-muted" style={{ marginTop: 0 }}>Both also run on their own timers.</p>
          <div className="fx-actions">
            <Button busy={busy === 'reconcile'} onClick={() => run('reconcile', async () => setResult({ title: 'Reconcile', data: await api.post('/fleet/reconcile') }))}>Reconcile</Button>
            <Button busy={busy === 'collect'} onClick={() => run('collect', async () => setResult({ title: 'Metrics collection', data: await api.post('/fleet/metrics/collect') }))}>Collect metrics</Button>
          </div>
        </Card>
      </div>
      <div className="fx-section">
        <Card title="Rebalance plan" action={<span className="fx-muted">Plans only; nothing is moved.</span>}>
          <div className="fx-grid cols-2">
            <Field label="High water" hint="Memory fraction above which a host sheds VMs. Empty: the default.">{(id) => <input id={id} className="fx-input" value={q.highWater} onChange={(e) => setQ({ ...q, highWater: e.target.value })} placeholder="0.85" />}</Field>
            <Field label="Max moves" hint="Up to 100. Empty: 10.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={q.maxMoves} onChange={(e) => setQ({ ...q, maxMoves: e.target.value })} />}</Field>
          </div>
          {planErr ? <p className="fx-error" role="alert">{planErr}</p> : null}
          <div className="fx-actions"><Button variant="primary" busy={busy === 'plan'} disabled={!!planErr} onClick={() => run('plan', async () => setPlan(await api.get(rebalancePath(q))))}>Plan</Button></div>
          {plan ? (moves.length ? <GenericTable rows={moves} empty="" /> : <p className="fx-muted">No moves needed.</p>) : null}
        </Card>
      </div>
      <div className="fx-section">
        <Card title="Fleet policy" action={<Button size="sm" disabled={!policy.data} onClick={() => setEdit(JSON.stringify(policy.data, null, 2))}>Edit as JSON</Button>}>
          {policy.loading && !policy.data ? <Skeleton h={100} /> : flatten(policy.data).length ? <KV rows={flatten(policy.data)} /> : <p className="fx-muted">No policy reported.</p>}
        </Card>
      </div>
      <Sheet open={!!result} title={result?.title ?? ''} onClose={() => setResult(null)}>
        {flatten(result?.data).length ? <KV rows={flatten(result?.data)} /> : <p>Done.</p>}
      </Sheet>
      <Sheet open={edit != null} title="Edit fleet policy" onClose={() => setEdit(null)}>
        <p className="fx-muted">This replaces the whole policy. Central raises the minimum lease to its floor and checks the tunnel limits.</p>
        <textarea className="fx-input fx-mono" rows={16} aria-label="Policy JSON" spellCheck={false} value={edit ?? ''} onChange={(e) => setEdit(e.target.value)} />
        {parsed && !parsed.ok ? <p className="fx-error" role="alert">{parsed.error}</p> : parsed?.ok ? <p className="fx-muted" role="status">{changed.length ? `Changes: ${changed.join(', ')}.` : 'No changes.'}</p> : null}
        <div className="fx-actions"><Button variant="primary" busy={busy === 'policy'} disabled={!parsed?.ok || changed.length === 0} onClick={async () => {
          if (parsed?.ok && (await run('policy', () => api.put('/fleet/policy', parsed.value), 'Policy saved', policy.refresh))) setEdit(null);
        }}>Save policy</Button><Button onClick={() => setEdit(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
