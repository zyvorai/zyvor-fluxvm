import { useMemo, useState } from 'react';
import { Copy, Play } from 'lucide-react';
import { Button, Card, EmptyState, Hero, KV, Pill, Skeleton, Tabs, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { GenericTable, flatten, rowsOf } from '../lib/generic';
import { curlFor, filterOps, operationsOf, runnable, type Operation } from '../lib/explorer';

type Tab = 'access' | 'events' | 'api';

function Explorer({ notify }: { notify: Notify }) {
  const doc = usePoll(() => api.get<unknown>('/v1/openapi.json').catch(() => null), 600000);
  const [q, setQ] = useState('');
  const [open, setOpen] = useState('');
  const [out, setOut] = useState<{ key: string; text: string; bad: boolean } | null>(null);
  const ops = useMemo(() => operationsOf(doc.data), [doc.data]);
  const shown = useMemo(() => filterOps(ops, q).slice(0, 150), [ops, q]);
  const key = (o: Operation) => o.method + ' ' + o.path;
  const run = async (o: Operation) => {
    try {
      const r = await api.get<unknown>(o.path);
      setOut({ key: key(o), text: JSON.stringify(r, null, 2).slice(0, 20000), bad: false });
    } catch (e) {
      setOut({ key: key(o), text: (e as Error).message, bad: true });
    }
  };
  if (doc.loading && !doc.data) return <Skeleton h={160} />;
  if (!ops.length) return <EmptyState title="No API description" body="This daemon did not serve /v1/openapi.json. The REST reference in the docs lists every route." />;
  return (
    <>
      <div className="fx-toolbar"><input className="fx-input" aria-label="Filter routes" placeholder={`Filter ${ops.length} operations…`} value={q} onChange={(e) => setQ(e.target.value)} /></div>
      <Card>
        <div className="fx-table-wrap"><table className="fx-table"><tbody>
          {shown.map((o) => (
            <tr key={key(o)}>
              <td style={{ width: 80 }}><Pill tone={o.method === 'GET' ? 'info' : o.method === 'DELETE' ? 'bad' : 'warn'} dot={false}>{o.method}</Pill></td>
              <td className="fx-mono">
                {o.path}
                <div className="fx-muted" style={{ fontSize: 12 }}>{o.summary}</div>
                {open === key(o) && out?.key === key(o) ? <pre className={`fx-code ${out.bad ? 'fx-code-err' : ''}`}>{out.text}</pre> : null}
              </td>
              <td><span className="fx-row-actions">
                {runnable(o) ? <Button size="sm" variant="ghost" aria-label={`Run ${o.path}`} onClick={() => (setOpen(key(o)), void run(o))}><Play size={14} /></Button> : null}
                <Button size="sm" variant="ghost" aria-label={`Copy curl for ${o.path}`} title={curlFor(o, location.origin)} onClick={() => navigator.clipboard.writeText(curlFor(o, location.origin)).then(() => notify('curl copied (token read from $FLUXVM_TOKEN)'), () => notify('Copy blocked by the browser', 'bad'))}><Copy size={14} /></Button>
              </span></td>
            </tr>
          ))}
        </tbody></table></div>
        {ops.length > shown.length && !q ? <p className="fx-muted">Showing the first {shown.length}. Filter to narrow.</p> : null}
      </Card>
      <p className="fx-muted">Only parameterless reads can be run here. Everything else you can copy as a curl command.</p>
    </>
  );
}

export function AccessPage({ notify }: { notify: Notify }) {
  const [tab, setTab] = useState<Tab>('access');
  const quota = usePoll(() => api.get<unknown>('/v1/quotas/me').catch(() => null), 15000);
  const caps = usePoll(() => api.get<unknown>('/v1/security/capabilities').catch(() => null), 60000);
  const runtime = usePoll(() => api.get<unknown>('/v1/runtime/capabilities').catch(() => null), 60000);
  const events = usePoll(async () => rowsOf(await api.get<unknown>('/v1/events').catch(() => ({ items: [] })), 'items', 'events'), 6000);
  const qrows = flatten(quota.data);
  return (
    <>
      <Hero eyebrow="Access & audit" title="Know who can do" accent="what, and what happened." lede="Your quota, what this node can enforce, the event history and every route the daemon serves." />
      <Tabs<Tab> value={tab} onChange={setTab} tabs={[{ value: 'access', label: 'Access' }, { value: 'events', label: 'Events' }, { value: 'api', label: 'API explorer' }]} />
      <div className="fx-section">
        {tab === 'access' ? (
          <div className="fx-grid cols-2">
            <Card title="Your quota">{quota.loading && !quota.data ? <Skeleton h={80} /> : qrows.length ? <KV rows={qrows} /> : <p className="fx-muted">No quota applies to this token.</p>}</Card>
            <Card title="Security features">{caps.loading && !caps.data ? <Skeleton h={80} /> : flatten(caps.data).length ? <KV rows={flatten(caps.data)} /> : <p className="fx-muted">Not reported.</p>}</Card>
            <Card title="Runtime">{runtime.loading && !runtime.data ? <Skeleton h={80} /> : flatten(runtime.data).length ? <KV rows={flatten(runtime.data)} /> : <p className="fx-muted">Not reported.</p>}</Card>
            <Card title="Tokens and roles"><p className="fx-muted" style={{ margin: 0 }}>Tokens are issued and revoked with <code>fluxctl token</code> on the node, so a secret never travels through a browser. This session's token lives in this tab only and is cleared when you sign out or close it.</p></Card>
          </div>
        ) : tab === 'events' ? (
          <Card title="Recent events">{events.loading && !events.data ? <Skeleton h={120} /> : <GenericTable rows={events.data ?? []} empty="No events recorded yet." />}</Card>
        ) : <Explorer notify={notify} />}
      </div>
    </>
  );
}
