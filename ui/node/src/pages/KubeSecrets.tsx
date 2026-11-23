// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { useMemo, useState } from 'react';
import { Copy } from 'lucide-react';
import type { Notify } from '../App';
import { Button, Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { ageOf, kubeProblem } from '../lib/kube';
import { type SecretsAnswer, filterSecrets, namespacesOfSecrets, setSecretCommand } from '../lib/kubeInventory';

const enc = encodeURIComponent;

/** The cluster's Secrets: names, types and the names of their keys. Values are never sent to the browser; setting one is a fluxctl command. */
export function KubeSecrets({ cluster, notify }: { cluster: string; notify: Notify }) {
  const poll = usePoll(async () => await api.get<SecretsAnswer>(`/v1/kube/${enc(cluster)}/secrets`), 10000, [cluster]);
  const [ns, setNs] = useState('');
  const [q, setQ] = useState('');
  const d = poll.data;
  const rows = useMemo(() => filterSecrets(d?.secrets ?? [], ns, q), [d, ns, q]);
  const copy = (cmd: string) => navigator.clipboard.writeText(cmd).then(() => notify('Copied: ' + cmd), () => notify('Copy blocked by the browser', 'bad'));
  if (poll.error && !d) {
    const p = kubeProblem(poll.error.message);
    return <Card><EmptyState title={p.title} body={p.body} /></Card>;
  }
  if (!d) return <Skeleton h={200} />;
  if (d.error) {
    return (
      <Card>
        <EmptyState title="Could not read this cluster's Secrets" body={`${d.error.message} Next, run: ${d.error.next}`} />
        <Button size="sm" variant="ghost" onClick={() => void copy(d.error!.next)}><Copy size={14} />Copy as fluxctl</Button>
      </Card>
    );
  }
  const all = d.secrets ?? [];
  const now = Date.now();
  return (
    <Card title="Secrets">
      <p className="fx-muted" style={{ marginTop: 0 }}>Names only: a value is never shown here or sent to this page. Set one with <span className="fx-mono">fluxctl kube secrets set</span>, which asks for it at a hidden prompt.</p>
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', marginBottom: 12 }}>
        <Pill tone="idle">{all.length} secret{all.length === 1 ? '' : 's'}</Pill>
        <select className="fx-input" aria-label="Namespace" value={ns} onChange={(e) => setNs(e.target.value)} style={{ maxWidth: 220 }}>
          <option value="">All namespaces</option>
          {namespacesOfSecrets(all).map((x) => <option key={x} value={x}>{x}</option>)}
        </select>
        <input className="fx-input" aria-label="Filter secrets" placeholder="Filter by name, type or key" value={q} onChange={(e) => setQ(e.target.value)} style={{ maxWidth: 320 }} />
        <Button size="sm" variant="ghost" onClick={() => void copy(d.fluxctl)}><Copy size={14} />Copy as fluxctl</Button>
      </div>
      {rows.length === 0 ? (
        <p className="fx-muted">{all.length ? 'No Secret matches this filter.' : 'This cluster has no Secrets yet. Create one with fluxctl kube secrets set NAME KEY.'}</p>
      ) : (
        <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Name</th><th>Type</th><th>Keys</th><th>Age</th><th /></tr></thead><tbody>
          {rows.map((r) => (
            <tr key={`${r.namespace}/${r.name}`}>
              <td className="fx-muted">{r.namespace}</td>
              <td className="vm-name">{r.name}</td>
              <td className="fx-muted fx-mono">{r.type}</td>
              <td className="fx-mono">{r.keys.join(', ') || '—'}</td>
              <td className="fx-muted">{ageOf(r.created, now)}</td>
              <td><Button size="sm" variant="ghost" aria-label={`Copy fluxctl command to change ${r.name}`} onClick={() => void copy(setSecretCommand(cluster, r.namespace, r.name, r.keys[0] ?? 'KEY'))}><Copy size={14} />Copy as fluxctl</Button></td>
            </tr>
          ))}
        </tbody></table></div>
      )}
      {d.truncated ? <p className="fx-muted">Showing the first 500 Secrets.</p> : null}
    </Card>
  );
}
