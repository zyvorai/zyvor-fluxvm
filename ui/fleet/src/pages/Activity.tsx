import { useState } from 'react';
import { Trash2 } from 'lucide-react';
import { Button, Card, Field, Hero, Sheet, Skeleton, Tabs, ago, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { GenericTable, rowsOf } from '../lib/generic';
import { SecretSheet } from './Secret';

type Tab = 'events' | 'notifications' | 'webhooks';
const when = (rows: Record<string, unknown>[]) => rows.map((r) => ({ ...r, ...(typeof r.at === 'string' ? { at: ago(r.at) } : {}), ...(typeof r.ts === 'string' ? { ts: ago(r.ts) } : {}) }));

export function ActivityPage({ notify }: Ctx) {
  const [tab, setTab] = useState<Tab>('events');
  const events = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/events?limit=200').catch(() => ({ items: [] })), 'items'), 6000);
  const ntf = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/notifications?limit=50').catch(() => ({ items: [] })), 'items'), 10000);
  const hooks = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/webhooks').catch(() => ({ items: [] })), 'items'), 15000);
  const { busy, run } = useAct(notify);
  const [add, setAdd] = useState(false);
  const [url, setUrl] = useState('');
  const [secret, setSecret] = useState<string | null>(null);
  const urlBad = !/^https?:\/\/\S+$/.test(url);
  return (
    <>
      <Hero eyebrow="Activity" title="Everything that" accent="just happened." lede="The audit trail, alerts, and webhooks that carry them elsewhere." />
      <Tabs<Tab> value={tab} onChange={setTab} tabs={[{ value: 'events', label: 'Audit log' }, { value: 'notifications', label: 'Alerts' }, { value: 'webhooks', label: 'Webhooks' }]} />
      <div className="fx-section">
        {tab === 'events' ? <Card>{events.loading && !events.data ? <Skeleton h={160} /> : <GenericTable rows={when(events.data ?? [])} empty="Nothing has happened yet." />}</Card> : tab === 'notifications' ? <Card>{ntf.loading && !ntf.data ? <Skeleton h={120} /> : <GenericTable rows={when(ntf.data ?? [])} empty="No alerts." />}</Card> : (
          <Card title="Webhooks" action={<Button size="sm" variant="primary" onClick={() => setAdd(true)}>Add webhook</Button>}>
            {(hooks.data ?? []).length === 0 ? <p className="fx-muted">No webhooks. Each event is POSTed as JSON with an <code>X-FluxVM-Signature</code> header (HMAC-SHA256 of the body).</p> : (
              <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>URL</th><th>Events</th><th /></tr></thead><tbody>
                {(hooks.data ?? []).map((w) => {
                  const id = String(w.id);
                  return (
                    <tr key={id}>
                      <td className="fx-mono">{String(w.url)}</td>
                      <td className="fx-muted">{Array.isArray(w.events) && w.events.length ? w.events.join(', ') : 'all'}</td>
                      <td><span className="fx-row-actions">
                        <Button size="sm" variant="ghost" busy={busy === 't' + id} onClick={() => run('t' + id, () => api.post(`/fleet/webhooks/${encodeURIComponent(id)}/test`), 'Test event sent')}>Test</Button>
                        <Button size="sm" variant="ghost" aria-label="Delete webhook" busy={busy === 'd' + id} onClick={() => run('d' + id, () => api.del(`/fleet/webhooks/${encodeURIComponent(id)}`), 'Webhook deleted', hooks.refresh)}><Trash2 size={14} /></Button>
                      </span></td>
                    </tr>
                  );
                })}
              </tbody></table></div>
            )}
          </Card>
        )}
      </div>
      <Sheet open={add} title="Add a webhook" onClose={() => setAdd(false)}>
        <Field label="URL">{(id) => <input id={id} className="fx-input fx-mono" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://hooks.example.com/fluxvm" />}</Field>
        {url && urlBad ? <p className="fx-error" role="alert">Use an http(s):// URL.</p> : null}
        <p className="fx-muted">Every event type is sent. Add per-event filters from the API.</p>
        <div className="fx-actions"><Button variant="primary" busy={busy === 'add'} disabled={urlBad} onClick={async () => {
          const ok = await run('add', async () => { const r = await api.post<{ secret: string }>('/fleet/webhooks', { url: url.trim(), events: [] }); setSecret(r.secret); }, undefined, hooks.refresh);
          if (ok) (setAdd(false), setUrl(''));
        }}>Add webhook</Button><Button onClick={() => setAdd(false)}>Cancel</Button></div>
      </Sheet>
      <SecretSheet title="Webhook secret" intro="Verify each delivery with it. This is the only time it is shown." secret={secret} notify={notify} onClose={() => setSecret(null)} />
    </>
  );
}
