import { useState } from 'react';
import { Trash2 } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Sheet, Skeleton, Stat, ago, api, fmtMiB, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { EgressEditor } from './EgressEditor';
import { rowsOf } from '../lib/generic';

export function NetworkPage({ notify }: { notify: Notify }) {
  const summary = usePoll(() => api.get<Record<string, unknown>>('/v1/netobs/summary').catch(() => null), 8000);
  const s = summary.data;
  const num = (k: string): number | null => (s && typeof s[k] === 'number' ? (s[k] as number) : null);
  const stats = ['flows', 'dns_queries', 'drops', 'bytes'].map((k) => [k, num(k)] as const).filter(([, v]) => v != null);
  return (
    <>
      <Hero eyebrow="Network" title="Decide what" accent="can reach out." lede="The default outbound policy for user-mode guests, and a live view of what the datapath sees." />
      <div className="fx-grid cols-4">
        {summary.loading && !s ? [0, 1, 2, 3].map((i) => <Skeleton key={i} h={104} />) : stats.map(([k, v]) => <Stat key={k} label={k.replace(/_/g, ' ')} value={v as number} tone="info" />)}
      </div>
      <div className="fx-section">
        <h2>Node default</h2>
        <p className="fx-muted" style={{ marginTop: -6 }}>Machines without their own policy inherit this. Changes can re-program running VMs immediately.</p>
        <EgressEditor notify={notify} />
      </div>
    </>
  );
}

export function BackupsPage({ notify }: { notify: Notify }) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/v1/backups'), 'backups'), 8000);
  const [del, setDel] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const rows = list.data ?? [];
  const remove = async () => {
    if (!del) return;
    setBusy(true);
    try {
      await api.del(`/v1/backups/${encodeURIComponent(del)}`);
      notify(`${del} deleted`);
      setDel(null);
      await list.refresh();
    } catch (e) {
      notify((e as Error).message, 'bad');
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <Hero eyebrow="Backups" title="Copies that" accent="outlive the node." lede="Standalone disk backups, newest first. Restore from a machine's Protect tab." />
      <Card>
        {list.loading && !rows.length ? <Skeleton h={140} /> : rows.length === 0 ? <EmptyState title="No backups yet" body="Open a machine, choose Protect, and back it up. Backups land here." /> : (
          <div className="fx-table-wrap">
            <table className="fx-table">
              <thead><tr><th>Name</th><th>Size</th><th>Mode</th><th>Created</th><th /></tr></thead>
              <tbody>
                {rows.map((b) => {
                  const name = String(b.name ?? '');
                  return (
                    <tr key={name}>
                      <td className="vm-name">{name}</td>
                      <td className="fx-muted">{typeof b.size_bytes === 'number' ? fmtMiB(Math.round(b.size_bytes / 1048576)) : '—'}</td>
                      <td className="fx-muted">{b.live ? 'live' : 'offline'}{b.quiesced ? ' · quiesced' : ''}</td>
                      <td className="fx-muted">{typeof b.created_at === 'string' ? ago(b.created_at) : '—'}</td>
                      <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Delete ${name}`} onClick={() => setDel(name)}><Trash2 size={14} /></Button></span></td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Card>
      <Sheet open={!!del} title="Delete backup" onClose={() => setDel(null)}>
        <p>Delete <b>{del}</b>? This cannot be undone.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy} onClick={remove}>Delete</Button><Button onClick={() => setDel(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
