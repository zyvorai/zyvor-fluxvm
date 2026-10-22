import { useState } from 'react';
import { GitBranch, Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, KV, Pill, Sheet, Skeleton, ago, api, fmtMiB, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { useAct } from '../lib/act';
import { type Branch, type ForkPlan, type PruneDraft, type PruneReport, type StartDraft, isKept, nameById, planSummary, pruneBody, pruneProblem, startBody, startProblem } from '../lib/branches';
import { flatten, rowsOf } from '../lib/generic';
import { fmtBytes } from '../lib/egress';

const enc = encodeURIComponent;

export function BranchesPage({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/branches')) as unknown as Branch[], 8000);
  const { busy, run } = useAct(notify);
  const [picked, setPicked] = useState<string[]>([]);
  const [diff, setDiff] = useState<Record<string, unknown> | null>(null);
  const [start, setStart] = useState<{ branch: Branch; draft: StartDraft; plan: ForkPlan | null } | null>(null);
  const [del, setDel] = useState<Branch | null>(null);
  const [prune, setPrune] = useState<{ draft: PruneDraft; report: PruneReport | null } | null>(null);
  const rows = list.data ?? [];
  const names = nameById(rows);

  const toggle = (id: string) => setPicked((p) => (p.includes(id) ? p.filter((x) => x !== id) : [...p, id].slice(-2)));
  const compare = () => run('diff', async () => setDiff(await api.get<Record<string, unknown>>(`/v1/branches/${enc(picked[0])}/diff/${enc(picked[1])}`)));
  const plan = () => start && run('plan', async () => setStart({ ...start, plan: await api.post<ForkPlan>(`/v1/branches/${enc(start.branch.id)}/start/plan`, startBody(start.draft)) }));
  const begin = async () => {
    if (!start) return;
    if (await run('start', () => api.post(`/v1/branches/${enc(start.branch.id)}/start`, startBody(start.draft)), `Started ${start.draft.count} from ${start.branch.name}`)) setStart(null);
  };
  const startErr = start ? startProblem(start.draft) : '';
  const pruneErr = prune ? pruneProblem(prune.draft) : '';
  return (
    <>
      <Hero eyebrow="Branches" title="Saved machines to" accent="start again, many times." lede="A branch is a machine's memory and disk at one moment. Start copies of it, compare two, and prune the old ones.">
        <Button size="lg" onClick={() => setPrune({ draft: { keepLast: '10', olderDays: '', prefix: '' }, report: null })}>Prune…</Button>
      </Hero>
      <Card action={<Button size="sm" disabled={picked.length !== 2} busy={busy === 'diff'} onClick={compare}>Compare selected</Button>}>
        {list.loading && !rows.length ? <Skeleton h={140} /> : rows.length === 0 ? <EmptyState title="No branches" body="Take one from a running machine's header, or with fluxctl branch create." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th /><th>Name</th><th>From</th><th>Size</th><th>Taken</th><th /></tr></thead><tbody>
            {rows.map((r) => (
              <tr key={r.id}>
                <td><input type="checkbox" aria-label={`Select ${r.name} to compare`} checked={picked.includes(r.id)} onChange={() => toggle(r.id)} /></td>
                <td className="vm-name"><GitBranch size={14} /> {r.name} {isKept(r) ? <Pill tone="info">kept</Pill> : null}{r.parent_branch ? <div className="fx-muted">child of {names.get(r.parent_branch) ?? r.parent_branch.slice(0, 8)}</div> : null}</td>
                <td className="fx-muted">{r.source_name}<div>{r.vcpus} vCPU · {fmtMiB(r.memory_mib)}</div></td>
                <td className="fx-muted">{fmtBytes(r.state_bytes)}</td>
                <td className="fx-muted">{ago(r.created_at)}</td>
                <td><span className="fx-row-actions">
                  <Button size="sm" variant="ghost" onClick={() => setStart({ branch: r, draft: { count: '1', prefix: '' }, plan: null })}>Start</Button>
                  <Button size="sm" variant="ghost" aria-label={`Delete ${r.name}`} onClick={() => setDel(r)}><Trash2 size={14} /></Button>
                </span></td>
              </tr>
            ))}
          </tbody></table></div>
        )}
      </Card>
      <Sheet open={!!diff} title="Branch comparison" onClose={() => setDiff(null)}>
        <KV rows={flatten(diff)} />
      </Sheet>
      <Sheet open={!!start} title={`Start from ${start?.branch.name ?? ''}`} onClose={() => setStart(null)}>
        {start ? (
          <>
            <div className="fx-grid cols-2">
              <Field label="How many">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={start.draft.count} onChange={(e) => setStart({ ...start, plan: null, draft: { ...start.draft, count: e.target.value } })} />}</Field>
              <Field label="Name prefix" hint="Optional.">{(id) => <input id={id} className="fx-input" value={start.draft.prefix} onChange={(e) => setStart({ ...start, plan: null, draft: { ...start.draft, prefix: e.target.value } })} />}</Field>
            </div>
            {startErr ? <p className="fx-error" role="alert">{startErr}</p> : null}
            {start.plan ? <p className={start.plan.ok ? 'fx-muted' : 'fx-error'} role="status">{planSummary(start.plan)}</p> : null}
            <div className="fx-actions">
              <Button busy={busy === 'plan'} disabled={!!startErr} onClick={plan}>Check first</Button>
              <Button variant="primary" busy={busy === 'start'} disabled={!!startErr || start.plan?.ok === false} onClick={begin}>Start</Button>
              <Button onClick={() => setStart(null)}>Cancel</Button>
            </div>
          </>
        ) : null}
      </Sheet>
      <Sheet open={!!del} title={`Delete ${del?.name ?? ''}`} onClose={() => setDel(null)}>
        <p>Delete branch <b>{del?.name}</b>? Machines already started from it are independent and keep running.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'del'} onClick={async () => {
          if (del && (await run('del', () => api.del(`/v1/branches/${enc(del.id)}`), `${del.name} deleted`, list.refresh))) setDel(null);
        }}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
      <Sheet open={!!prune} title="Prune branches" onClose={() => setPrune(null)}>
        {prune ? (
          <>
            <p className="fx-muted">Branches labelled <code>fluxvm.dev/keep: "true"</code> are never removed. Nothing is deleted until you confirm the preview.</p>
            <div className="fx-grid cols-2">
              <Field label="Keep the newest">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={prune.draft.keepLast} onChange={(e) => setPrune({ report: null, draft: { ...prune.draft, keepLast: e.target.value } })} />}</Field>
              <Field label="Older than (days)">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={prune.draft.olderDays} onChange={(e) => setPrune({ report: null, draft: { ...prune.draft, olderDays: e.target.value } })} />}</Field>
            </div>
            <Field label="Name prefix">{(id) => <input id={id} className="fx-input" value={prune.draft.prefix} onChange={(e) => setPrune({ report: null, draft: { ...prune.draft, prefix: e.target.value } })} />}</Field>
            {pruneErr ? <p className="fx-error" role="alert">{pruneErr}</p> : null}
            {prune.report ? (
              <p role="status">{prune.report.selected.length === 0 ? 'Nothing matches.' : `${prune.report.selected.length} branch${prune.report.selected.length === 1 ? '' : 'es'} would go (${fmtBytes(prune.report.selected_state_bytes)} of saved state): ${prune.report.selected.map((s) => s.name).join(', ')}.`}{prune.report.protected ? ` ${prune.report.protected} protected.` : ''}</p>
            ) : null}
            <div className="fx-actions">
              <Button busy={busy === 'preview'} disabled={!!pruneErr} onClick={() => run('preview', async () => setPrune({ ...prune, report: await api.post<PruneReport>('/v1/branches/prune', pruneBody(prune.draft, true)) }))}>Preview</Button>
              <Button variant="danger" busy={busy === 'prune'} disabled={!prune.report || prune.report.selected.length === 0} onClick={async () => {
                if (await run('prune', () => api.post('/v1/branches/prune', pruneBody(prune.draft, false)), 'Pruned', list.refresh)) setPrune(null);
              }}>Delete {prune.report?.selected.length ?? 0}</Button>
              <Button onClick={() => setPrune(null)}>Cancel</Button>
            </div>
          </>
        ) : null}
      </Sheet>
    </>
  );
}
