// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useState } from 'react';
import { Copy } from 'lucide-react';
import { Button, Card, EmptyState, Hero, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { NO_RATES_NOTE, barHeights, formatBytes, formatCost, hours, usagePath, type UsageReport } from '../lib/usage';

/** What each machine used and, with rates set, what it cost. The same as fluxctl usage. */
export function UsagePage({ notify }: { notify: Notify }) {
  const [by, setBy] = useState('vm');
  const [key, setKey] = useState('');
  const [since, setSince] = useState('7d');
  const [label, setLabel] = useState('');
  const needKey = by === 'label' && !key.trim();
  const path = usagePath(needKey ? 'vm' : by, since, label, key);
  const poll = usePoll(() => api.get<UsageReport>(path), 30000, [path]);
  const r = poll.data;
  const cmd = `fluxctl usage --by ${by === 'label' ? `label:${key.trim() || 'KEY'}` : by}${since ? ` --since ${since}` : ''}${label.trim() ? ` --label ${label.trim()}` : ''}`;
  const copy = () => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  const heights = barHeights(r?.days ?? []);
  return (
    <>
      <Hero eyebrow="Usage" title="What each machine used," accent="and what it cost." lede="CPU, memory, disk and network, counted automatically while machines run (kept 90 days). The same as fluxctl usage." />
      <p>
        <select className="fx-input" value={by} onChange={(e) => setBy(e.target.value)} aria-label="Group by">
          <option value="vm">By machine</option>
          <option value="day">By day</option>
          <option value="label">By label</option>
        </select>{' '}
        {by === 'label' ? <><input className="fx-input" value={key} onChange={(e) => setKey(e.target.value)} placeholder="Label name, e.g. team" aria-label="Label to group by" />{' '}</> : null}
        <select className="fx-input" value={since} onChange={(e) => setSince(e.target.value)} aria-label="Period">
          <option value="24h">Last day</option>
          <option value="7d">Last 7 days</option>
          <option value="30d">Last 30 days</option>
          <option value="">Everything kept</option>
        </select>{' '}
        <input className="fx-input" value={label} onChange={(e) => setLabel(e.target.value)} placeholder="Only labels, e.g. env=dev" aria-label="Label filter" />{' '}
        <Button size="sm" variant="ghost" aria-label={`Copy ${cmd}`} title={cmd} onClick={() => void copy()}><Copy size={14} /> Copy as fluxctl</Button>
      </p>
      {needKey ? <p className="fx-muted">Type the label to group by, for example team. Showing by machine until then.</p> : null}
      {!r ? (
        poll.loading ? <Skeleton /> : <EmptyState title="Could not load usage" body="This daemon may be older than this page, or the filter is not valid. In a terminal: fluxctl usage" />
      ) : r.rows.length === 0 ? (
        <EmptyState title="No usage recorded yet" body="FluxVM samples running machines once a minute. Start a machine and check back in a few minutes." />
      ) : (
        <>
          {!r.priced ? <p className="fx-muted">{NO_RATES_NOTE}</p> : <p className="fx-muted">Total cost {formatCost(r.total_cost, r.currency)} for the period.</p>}
          <Card title="CPU hours per day">
            <svg viewBox={`0 0 ${Math.max(heights.length, 1) * 20} 60`} width="100%" height="80" role="img" aria-label="CPU hours per day" preserveAspectRatio="none">
              {heights.map((h, i) => (
                <rect key={r.days[i].key} x={i * 20 + 3} y={58 - h * 54} width={14} height={Math.max(h * 54, 1)} fill="currentColor" opacity={0.7}>
                  <title>{`${r.days[i].key}: ${hours(r.days[i].units.cpu_seconds)} CPU hours`}</title>
                </rect>
              ))}
            </svg>
          </Card>
          <Card>
            <div className="fx-table-wrap"><table className="fx-table">
              <thead><tr><th>{by === 'vm' ? 'Machine' : by === 'day' ? 'Day' : 'Group'}</th><th>Running h</th><th>CPU h</th><th>Memory GiB-h</th><th>Disk I/O</th><th>Network</th>{r.priced ? <th>Cost</th> : null}</tr></thead>
              <tbody>
                {[...r.rows.map((x) => ({ ...x, total: false })), { key: 'total', name: 'Total', units: r.total, cost: r.total_cost, total: true }].map((x) => (
                  <tr key={x.key} style={x.total ? { fontWeight: 600 } : undefined}>
                    <td>{x.name}</td><td>{hours(x.units.running_seconds)}</td><td>{hours(x.units.cpu_seconds)}</td><td>{x.units.memory_gib_hours.toFixed(2)}</td>
                    <td>{formatBytes(x.units.disk_read_bytes + x.units.disk_write_bytes)}</td><td>{formatBytes(x.units.net_rx_bytes + x.units.net_tx_bytes)}</td>
                    {r.priced ? <td>{formatCost(x.cost, r.currency)}</td> : null}
                  </tr>
                ))}
              </tbody>
            </table></div>
          </Card>
        </>
      )}
    </>
  );
}
