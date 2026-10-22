import { useEffect, useState } from 'react';
import { Eye, Plus, RotateCcw, Sparkles, X } from 'lucide-react';
import { Button, Card, Pill, Segmented, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { type EgressPolicy, type EgressRule, type EgressView, type ObservedFlow, type Proto, MODES, PRESETS, addRule, crowdedPrefixes, describeRule, fmtBytes, modeInfo, samePolicy } from '../lib/egress';

/**
 * Egress policy for user-mode guests. `vmId` edits one VM (with observe → suggest → apply); without it this
 * edits the node default that VMs without their own policy inherit.
 */
export function EgressEditor({ vmId, notify }: { vmId?: string; notify: Notify }) {
  const base = vmId ? `/v1/vms/${vmId}/network/egress` : '/v1/network/egress/defaults';
  const view = usePoll(async () => {
    const r = await api.get<EgressView | { policy: EgressPolicy }>(base);
    return r as EgressView;
  }, 8000, [base]);
  const [draft, setDraft] = useState<EgressPolicy | null>(null);
  const [busy, setBusy] = useState('');
  const [rule, setRule] = useState<{ cidr: string; proto: Proto; port: string; note: string }>({ cidr: '', proto: 'tcp', port: '', note: '' });
  const [err, setErr] = useState('');
  const [applyRunning, setApplyRunning] = useState(true);

  const server = view.data?.policy ?? null;
  useEffect(() => {
    if (server && !draft) setDraft({ mode: server.mode, rules: [...(server.rules ?? [])] });
  }, [server, draft]);
  const dirty = !!draft && !!server && !samePolicy(draft, { mode: server.mode, rules: server.rules ?? [] });
  const crowded = draft ? crowdedPrefixes(draft.rules) : [];

  const run = async (label: string, fn: () => Promise<unknown>, ok: string) => {
    setBusy(label);
    try {
      await fn();
      notify(ok);
      setDraft(null);
      await view.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  const save = () => draft && run('save', () => api.put(vmId ? base : `${base}${applyRunning ? '?apply_running=true' : ''}`, draft), vmId ? 'Egress policy applied' : 'Node default saved');
  const reset = () => run('reset', () => api.del(base), 'Back to the node default');

  const addFromForm = () => {
    if (!draft) return;
    const res = addRule(draft.rules, { cidr: rule.cidr, proto: rule.proto, port: rule.port ? Number(rule.port) : null, note: rule.note });
    setErr(res.error);
    if (!res.error) {
      setDraft({ ...draft, rules: res.rules });
      setRule({ cidr: '', proto: 'tcp', port: '', note: '' });
    }
  };

  if (view.loading && !draft) return <Card title="Outbound access"><p className="fx-muted">Loading…</p></Card>;
  if (view.error && !draft) return <Card title="Outbound access"><p className="fx-error">{view.error.message}</p></Card>;
  if (!draft) return null;
  const info = modeInfo(draft.mode);

  return (
    <>
      <Card
        title="Outbound access"
        action={
          <span style={{ display: 'inline-flex', gap: 8, alignItems: 'center' }}>
            {vmId && view.data?.source ? <Pill tone="idle" dot={false} raw>{view.data.source.replace('_', ' ')}</Pill> : null}
            <Pill tone={info.tone}>{info.label}</Pill>
          </span>
        }
      >
        <div className="fx-presets">
          {PRESETS.map((p) => (
            <button key={p.id} className="fx-preset" aria-pressed={samePolicy(draft, p.policy)} onClick={() => setDraft({ mode: p.policy.mode, rules: p.policy.rules.map((r) => ({ ...r })) })}>
              <b>{p.label}</b>
              <small>{p.blurb}</small>
            </button>
          ))}
        </div>
        <Segmented label="Egress mode" value={draft.mode} onChange={(mode) => setDraft({ ...draft, mode })} options={MODES.map((m) => ({ value: m.value, label: m.label }))} />
        <p className="fx-muted" style={{ marginTop: 10 }}>{info.blurb}</p>

        {draft.mode === 'allowlist' || draft.mode === 'audit' ? (
          <>
            <h3 className="fx-subhead">Allowed destinations</h3>
            {draft.rules.length === 0 ? <p className="fx-muted">No destinations yet. {draft.mode === 'allowlist' ? 'The guest can reach nothing until you add some.' : ''}</p> : (
              <ul className="fx-chips">
                {draft.rules.map((r, i) => (
                  <li key={i} title={r.note ?? undefined}>
                    <code>{describeRule(r)}</code>
                    {r.note ? <small>{r.note}</small> : null}
                    <button aria-label={`Remove ${describeRule(r)}`} onClick={() => setDraft({ ...draft, rules: draft.rules.filter((_, j) => j !== i) })}><X size={12} /></button>
                  </li>
                ))}
              </ul>
            )}
            {crowded.length ? <div className="fx-error" role="alert">More than 4 distinct ports under {crowded.join(', ')}: the datapath keeps only 4 per prefix. Widen the prefix or drop some ports.</div> : null}
            <div className="fx-rule-form">
              <input className="fx-input" aria-label="CIDR" placeholder="10.0.0.0/16 or 1.2.3.4" value={rule.cidr} onChange={(e) => setRule({ ...rule, cidr: e.target.value })} onKeyDown={(e) => e.key === 'Enter' && addFromForm()} />
              <select className="fx-input" aria-label="Protocol" value={rule.proto} onChange={(e) => setRule({ ...rule, proto: e.target.value as Proto })}>
                <option value="tcp">tcp</option><option value="udp">udp</option><option value="any">any</option>
              </select>
              <input className="fx-input" aria-label="Port" placeholder="port (optional)" inputMode="numeric" value={rule.port} onChange={(e) => setRule({ ...rule, port: e.target.value.replace(/\D/g, '') })} onKeyDown={(e) => e.key === 'Enter' && addFromForm()} />
              <input className="fx-input" aria-label="Note" placeholder="note" value={rule.note} onChange={(e) => setRule({ ...rule, note: e.target.value })} onKeyDown={(e) => e.key === 'Enter' && addFromForm()} />
              <Button onClick={addFromForm}><Plus size={14} />Add</Button>
            </div>
            {err ? <div className="fx-error" role="alert" style={{ marginTop: 8 }}>{err}</div> : null}
          </>
        ) : null}

        {!vmId ? (
          <label className="fx-check"><input type="checkbox" checked={applyRunning} onChange={(e) => setApplyRunning(e.target.checked)} /> Re-program running VMs that use the node default</label>
        ) : null}
        <div className="fx-actions" style={{ marginTop: 18 }}>
          <Button variant="primary" busy={busy === 'save'} disabled={!dirty} onClick={save}>{dirty ? 'Apply changes' : 'Up to date'}</Button>
          {dirty ? <Button variant="ghost" onClick={() => setDraft(null)}>Discard</Button> : null}
          {vmId && view.data?.source === 'vm' ? <Button variant="ghost" busy={busy === 'reset'} onClick={reset}><RotateCcw size={14} />Use node default</Button> : null}
        </div>
        {vmId && view.data?.running === false ? <p className="fx-muted" style={{ marginTop: 12 }}>The machine is stopped. The policy is saved and programmed when it starts.</p> : null}
      </Card>
      {vmId ? <Observed vmId={vmId} mode={server?.mode} notify={notify} onApplied={() => { setDraft(null); void view.refresh(); }} /> : null}
    </>
  );
}

function Observed({ vmId, mode, notify, onApplied }: { vmId: string; mode?: string; notify: Notify; onApplied: () => void }) {
  const flows = usePoll(async () => (await api.get<{ flows: ObservedFlow[] }>(`/v1/vms/${vmId}/network/egress/observed`).catch(() => ({ flows: [] }))).flows, 5000, [vmId]);
  const [sug, setSug] = useState<EgressRule[] | null>(null);
  const [busy, setBusy] = useState('');
  const list = flows.data ?? [];
  const suggest = async (apply: boolean) => {
    setBusy(apply ? 'apply' : 'suggest');
    try {
      const r = await api.post<{ rules: EgressRule[]; applied: boolean }>(`/v1/vms/${vmId}/network/egress/suggest${apply ? '?apply=true' : ''}`);
      setSug(r.rules ?? []);
      if (apply) {
        notify(`${r.rules.length} rule(s) applied`);
        setSug(null);
        onApplied();
      }
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy('');
    }
  };
  return (
    <Card title={mode === 'audit' ? 'Learning — what the guest tried to reach' : 'Blocked destinations'} action={<span className="fx-live"><i />Live</span>}>
      {list.length === 0 ? (
        <p className="fx-muted">{mode === 'audit' ? 'Use the guest normally; every outbound destination shows up here.' : 'Nothing has been refused. Switch to Learn to record what this guest needs.'}</p>
      ) : (
        <div className="fx-table-wrap">
          <table className="fx-table">
            <thead><tr><th>Destination</th><th>Proto</th><th>Port</th><th>Packets</th><th>Bytes</th><th /></tr></thead>
            <tbody>
              {[...list].sort((a, b) => b.packets - a.packets).slice(0, 50).map((f) => (
                <tr key={`${f.dst}-${f.proto}-${f.port}-${f.audit}`}>
                  <td className="fx-mono">{f.dst}</td><td>{f.proto}</td><td>{f.port || '—'}</td><td>{f.packets}</td><td>{fmtBytes(f.bytes)}</td>
                  <td><Pill tone={f.audit ? 'warn' : 'bad'} dot={false}>{f.audit ? 'would block' : 'blocked'}</Pill></td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <div className="fx-actions" style={{ marginTop: 16 }}>
        <Button busy={busy === 'suggest'} disabled={list.length === 0} onClick={() => suggest(false)}><Eye size={14} />Preview rules</Button>
        <Button variant="primary" busy={busy === 'apply'} disabled={list.length === 0} onClick={() => suggest(true)}><Sparkles size={14} />Allow what was seen</Button>
      </div>
      {sug ? (
        <div style={{ marginTop: 14 }}>
          <p className="fx-muted">{sug.length ? 'These rules would be added and the policy switched to Allow-list:' : 'No rules to suggest yet.'}</p>
          <ul className="fx-chips">{sug.map((r, i) => <li key={i}><code>{describeRule(r)}</code>{r.note ? <small>{r.note}</small> : null}</li>)}</ul>
        </div>
      ) : null}
    </Card>
  );
}
