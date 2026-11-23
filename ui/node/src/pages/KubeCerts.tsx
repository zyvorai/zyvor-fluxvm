// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { Copy } from 'lucide-react';
import type { Notify } from '../App';
import { Button, Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { kubeProblem } from '../lib/kube';
import { type CertsAnswer, certLabel, certTone, certsHeadline, endDate } from '../lib/kubeInventory';

const enc = encodeURIComponent;

/** The certificates the cluster's Ingresses and Gateways serve: when each ends, what is wrong, and the command that fixes it. */
export function KubeCerts({ cluster, notify }: { cluster: string; notify: Notify }) {
  const poll = usePoll(async () => await api.get<CertsAnswer>(`/v1/kube/${enc(cluster)}/certs`), 15000, [cluster]);
  const d = poll.data;
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  if (poll.error && !d) {
    const p = kubeProblem(poll.error.message);
    return <Card><EmptyState title={p.title} body={p.body} /></Card>;
  }
  if (!d) return <Skeleton h={200} />;
  if (d.error) {
    return (
      <Card>
        <EmptyState title="Could not read this cluster's certificates" body={`${d.error.message} Next, run: ${d.error.next}`} />
        <Button size="sm" variant="ghost" onClick={() => void copy(d.error!.next)}><Copy size={14} />Copy as fluxctl</Button>
      </Card>
    );
  }
  const rows = d.certs ?? [];
  return (
    <Card title="Certificates">
      <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap', marginBottom: 12 }}>
        <Pill tone={d.problems ? 'warn' : 'ok'}>{certsHeadline(rows)}</Pill>
        <Button size="sm" variant="ghost" onClick={() => void copy(d.fluxctl)}><Copy size={14} />Copy as fluxctl</Button>
      </div>
      {rows.length === 0 ? (
        <p className="fx-muted">Certificates appear here once an Ingress or a Gateway listener names a TLS Secret. Warnings start {d.warn_days ?? 30} days before a certificate ends.</p>
      ) : (
        <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Status</th><th>Secret</th><th>Hosts</th><th>Ends</th><th>What is wrong</th><th>Used by</th><th /></tr></thead><tbody>
          {rows.map((r) => (
            <tr key={`${r.namespace}/${r.secret}`}>
              <td><Pill tone={certTone(r.state)}>{certLabel(r.state)}</Pill></td>
              <td className="vm-name"><span className="fx-muted">{r.namespace}/</span>{r.secret}</td>
              <td className="fx-mono">{r.hosts.join(', ') || '—'}</td>
              <td className="fx-muted fx-mono">{endDate(r.not_after)}</td>
              <td style={{ whiteSpace: 'normal', minWidth: 220 }}>{r.summary}</td>
              <td className="fx-muted">{r.used_by.join(', ')}</td>
              <td>{r.fix ? <Button size="sm" variant="ghost" aria-label={`Copy the fix for ${r.secret}`} title={r.fix} onClick={() => void copy(r.fix)}><Copy size={14} />Copy fix</Button> : null}</td>
            </tr>
          ))}
        </tbody></table></div>
      )}
    </Card>
  );
}
