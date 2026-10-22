import { useState } from 'react';
import { Plus } from 'lucide-react';
import { Button, Card, EmptyState, Field, Hero, Pill, Sheet, Skeleton, ago, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { type EnrolledNode, type JoinToken, type TokenDraft, tokenBody, tokenProblem, tokenState, usesText } from '../lib/ops';
import { SecretSheet } from './Secret';

const enc = encodeURIComponent;
const NEW: TokenDraft = { hours: '24', uses: '1', pattern: '', note: '' };
const TONE = { active: 'ok', revoked: 'idle', expired: 'idle', 'used up': 'idle' } as const;
const when = (sec: number | null | undefined): string => (sec ? ago(new Date(sec * 1000).toISOString()) : '—');

export function EnrollmentPage({ notify }: Ctx) {
  const tokens = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/enroll/tokens'), 'items') as unknown as JoinToken[], 10000);
  const nodes = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/enroll/nodes'), 'items') as unknown as EnrolledNode[], 10000);
  const { busy, run } = useAct(notify);
  const [draft, setDraft] = useState<TokenDraft | null>(null);
  const [secret, setSecret] = useState<{ title: string; intro: string; value: string } | null>(null);
  const [revoke, setRevoke] = useState<{ kind: 'token' | 'node'; id: string } | null>(null);
  const t = tokens.data ?? [];
  const n = nodes.data ?? [];
  const now = Math.floor(Date.now() / 1000);
  const problem = draft ? tokenProblem(draft) : '';
  const create = async () => {
    if (!draft || problem) return;
    const ok = await run('create', async () => {
      const r = await api.post<{ token: string }>('/fleet/enroll/tokens', tokenBody(draft));
      setSecret({ title: 'Join token', intro: 'Save it to a file on the new host and start the node with --token-file. It is shown once; central keeps only a hash.', value: r.token });
    }, undefined, tokens.refresh);
    if (ok) setDraft(null);
  };
  const act = (name: string, action: 'rotate' | 'rebind') =>
    run(action + name, async () => {
      const r = await api.post<{ credential?: string; previous_valid_secs?: number }>(`/fleet/nodes/${enc(name)}/${action}`);
      if (r.credential) setSecret({ title: `New credential for ${name}`, intro: `The old one keeps working for ${Math.round((r.previous_valid_secs ?? 0) / 60)} minutes. Shown once.`, value: r.credential });
      else notify(`${name} may enroll again`);
    }, undefined, nodes.refresh);
  return (
    <>
      <Hero eyebrow="Enrollment" title="Let a host join" accent="with a token." lede="Each host exchanges a one-time join token for its own credential, which renews daily. Revoke a token, or a single host, without touching the rest.">
        <Button variant="primary" size="lg" onClick={() => setDraft(NEW)}><Plus size={18} />New join token</Button>
      </Hero>
      <Card title="Join tokens">
        {tokens.loading && !t.length ? <Skeleton h={100} /> : t.length === 0 ? <EmptyState title="No join tokens" body="Create one, then start a host with it." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Token</th><th>State</th><th>Used</th><th>Expires</th><th>Allowed names</th><th>Note</th><th /></tr></thead><tbody>
            {t.map((k) => {
              const st = tokenState(k, now);
              return (
                <tr key={k.id}>
                  <td className="fx-mono">{k.id.slice(0, 8)}…</td>
                  <td><Pill tone={TONE[st]}>{st}</Pill></td>
                  <td className="fx-muted">{usesText(k)}</td>
                  <td className="fx-muted">{k.expires ? when(k.expires) : 'never'}</td>
                  <td className="fx-mono fx-muted">{k.name_pattern || 'any'}</td>
                  <td className="fx-muted">{k.note || '—'}</td>
                  <td><span className="fx-row-actions"><Button size="sm" variant="ghost" disabled={k.revoked} onClick={() => setRevoke({ kind: 'token', id: k.id })}>Revoke</Button></span></td>
                </tr>
              );
            })}
          </tbody></table></div>
        )}
      </Card>
      <div className="fx-section">
        <Card title="Enrolled hosts">
          {nodes.loading && !n.length ? <Skeleton h={100} /> : n.length === 0 ? <EmptyState title="No enrolled hosts" body="Hosts that joined with a token appear here." /> : (
            <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Host</th><th>State</th><th>Credential</th><th>Enrolled</th><th /></tr></thead><tbody>
              {n.map((h) => (
                <tr key={h.name}>
                  <td className="vm-name">{h.name}</td>
                  <td>{h.revoked ? <Pill tone="bad">revoked</Pill> : <Pill tone="ok">enrolled</Pill>}</td>
                  <td className="fx-muted">generation {h.generation}</td>
                  <td className="fx-muted">{when(h.enrolled_at)}</td>
                  <td><span className="fx-row-actions">
                    {h.revoked ? <Button size="sm" variant="ghost" busy={busy === 'rebind' + h.name} onClick={() => act(h.name, 'rebind')}>Allow again</Button> : (
                      <>
                        <Button size="sm" variant="ghost" busy={busy === 'rotate' + h.name} onClick={() => act(h.name, 'rotate')}>Rotate</Button>
                        <Button size="sm" variant="ghost" onClick={() => setRevoke({ kind: 'node', id: h.name })}>Revoke</Button>
                      </>
                    )}
                  </span></td>
                </tr>
              ))}
            </tbody></table></div>
          )}
        </Card>
      </div>
      <Sheet open={!!draft} title="New join token" onClose={() => setDraft(null)}>
        {draft ? (
          <>
            <div className="fx-grid cols-2">
              <Field label="Lifetime (hours)" hint="Empty: never expires.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={draft.hours} onChange={(e) => setDraft({ ...draft, hours: e.target.value })} />}</Field>
              <Field label="Uses" hint="Empty: unlimited.">{(id) => <input id={id} className="fx-input" inputMode="numeric" value={draft.uses} onChange={(e) => setDraft({ ...draft, uses: e.target.value })} />}</Field>
            </div>
            <Field label="Allowed host names" hint="A pattern such as mac-*. Empty: any name.">{(id) => <input id={id} className="fx-input" value={draft.pattern} onChange={(e) => setDraft({ ...draft, pattern: e.target.value })} />}</Field>
            <Field label="Note">{(id) => <input id={id} className="fx-input" value={draft.note} onChange={(e) => setDraft({ ...draft, note: e.target.value })} placeholder="rack 4 install" />}</Field>
            {problem ? <p className="fx-error" role="alert">{problem}</p> : null}
            <div className="fx-actions"><Button variant="primary" busy={busy === 'create'} disabled={!!problem} onClick={create}>Create token</Button><Button onClick={() => setDraft(null)}>Cancel</Button></div>
          </>
        ) : null}
      </Sheet>
      <Sheet open={!!revoke} title={revoke?.kind === 'token' ? 'Revoke join token' : `Revoke ${revoke?.id ?? ''}`} onClose={() => setRevoke(null)}>
        <p>{revoke?.kind === 'token' ? 'No new host can join with this token. Hosts that already joined keep their credentials.' : <>Revoke <b>{revoke?.id}</b>? It stops reporting in until you allow it again.</>}</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'revoke'} onClick={async () => {
          if (!revoke) return;
          const path = revoke.kind === 'token' ? `/fleet/enroll/tokens/${enc(revoke.id)}` : `/fleet/nodes/${enc(revoke.id)}/revoke`;
          const call = revoke.kind === 'token' ? () => api.del(path) : () => api.post(path);
          if (await run('revoke', call, 'Revoked', async () => { await tokens.refresh(); await nodes.refresh(); })) setRevoke(null);
        }}>Revoke</Button><Button onClick={() => setRevoke(null)}>Cancel</Button></div>
      </Sheet>
      <SecretSheet title={secret?.title ?? ''} intro={secret?.intro ?? ''} secret={secret?.value ?? null} notify={notify} onClose={() => setSecret(null)} />
    </>
  );
}
