import { useState } from 'react';
import { X } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Pill, Sheet, Skeleton, ago, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';
import { rowsOf } from '../lib/generic';
import { type Tunnel, fmtBytes, idleText, tunnelTarget } from '../lib/ops';

interface TunnelList { items: Tunnel[]; enabled?: boolean; max_per_tenant?: number; idle_timeout_minutes?: number; max_lifetime_minutes?: number; ports?: number[] }

export function TunnelsPage({ role, notify }: Ctx) {
  const list = usePoll(() => api.get<TunnelList>('/fleet/tunnels'), 5000);
  const { busy, run } = useAct(notify);
  const [close, setClose] = useState<Tunnel | null>(null);
  const d = list.data;
  const rows = d ? (rowsOf(d, 'items') as unknown as Tunnel[]) : [];
  return (
    <>
      <Hero eyebrow="Tunnels" title="Reach a VM port," accent="without opening it." lede="Open tunnels through central to a port, or the screen, of one of your VMs. Idle ones close on their own." />
      {d ? (
        <p className="fx-muted">
          {d.enabled === false ? <Pill tone="warn">disabled by the operator</Pill> : <Pill tone="ok">enabled</Pill>}{' '}
          {d.ports?.length ? `Ports ${d.ports.join(', ')}. ` : 'Any port. '}
          {d.max_per_tenant ? `Up to ${d.max_per_tenant} per tenant. ` : ''}
          {d.idle_timeout_minutes ? `Closes after ${d.idle_timeout_minutes} min idle` : ''}{d.max_lifetime_minutes ? `, or ${d.max_lifetime_minutes} min total.` : '.'}
        </p>
      ) : null}
      <Card>
        {list.loading && !d ? <Skeleton h={120} /> : rows.length === 0 ? <EmptyState title="No open tunnels" body="Open one from fluxctl or the API; it shows here while it is up." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>VM</th>{role === 'operator' ? <th>Tenant</th> : null}<th>Reaches</th><th>Opened</th><th>Traffic</th><th>Activity</th><th /></tr></thead><tbody>
            {rows.map((t) => (
              <tr key={t.id}>
                <td className="vm-name">{t.vm_name}<div className="fx-muted">{t.node}</div></td>
                {role === 'operator' ? <td className="fx-muted">{t.tenant || t.user || 'operator'}</td> : null}
                <td className="fx-mono">{t.kind === 'vnc' ? 'screen' : t.port}</td>
                <td className="fx-muted">{t.opened_at ? ago(t.opened_at) : '—'}</td>
                <td className="fx-muted">↑ {fmtBytes(t.bytes_sent)} · ↓ {fmtBytes(t.bytes_received)}</td>
                <td className="fx-muted">{idleText(t.idle_seconds)}</td>
                <td><span className="fx-row-actions"><Button size="sm" variant="ghost" aria-label={`Close the ${tunnelTarget(t)} tunnel to ${t.vm_name}`} onClick={() => setClose(t)}><X size={14} /></Button></span></td>
              </tr>
            ))}
          </tbody></table></div>
        )}
      </Card>
      <Sheet open={!!close} title="Close tunnel" onClose={() => setClose(null)}>
        <p>Close the tunnel to <b>{close?.vm_name}</b> <b>{close ? tunnelTarget(close) : ''}</b>? Anything using it is disconnected.</p>
        <div className="fx-actions"><Button variant="danger" busy={busy === 'close'} onClick={async () => {
          if (close && (await run('close', () => api.del(`/fleet/tunnels/${encodeURIComponent(close.id)}`), 'Tunnel closed', list.refresh))) setClose(null);
        }}>Close tunnel</Button><Button onClick={() => setClose(null)}>Cancel</Button></div>
      </Sheet>
    </>
  );
}
