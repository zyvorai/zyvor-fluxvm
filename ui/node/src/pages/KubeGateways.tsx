// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { Copy } from 'lucide-react';
import type { Notify } from '../App';
import { Button, Card, EmptyState, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import { type Gateways, backendText, exposeCommand, gatewayTone, kubeProblem } from '../lib/kube';

const enc = encodeURIComponent;

/** The cluster's Gateways and the routes attached to them, in plain words, with the fluxctl command for each row. */
export function KubeGateways({ cluster, notify }: { cluster: string; notify: Notify }) {
  const poll = usePoll(async () => await api.get<Gateways>(`/v1/kube/${enc(cluster)}/gateways`), 8000, [cluster]);
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
        <EmptyState title="Could not read this cluster's Gateways" body={`${d.error.message} Next, run: ${d.error.next}`} />
        <Button size="sm" variant="ghost" onClick={() => void copy(d.error!.next)}><Copy size={14} />Copy as fluxctl</Button>
      </Card>
    );
  }
  if (!d.gateways.length) {
    return (
      <Card>
        <EmptyState
          title="No Gateways yet"
          body={`Run fluxctl gateway expose SERVICE to publish a Service on a hostname. ${d.envoy_gateway.installed ? 'Envoy Gateway is already installed.' : 'The first run installs Envoy Gateway for you.'}`}
        />
        <Button size="sm" variant="ghost" onClick={() => void copy(exposeCommand('SERVICE'))}><Copy size={14} />Copy as fluxctl</Button>
      </Card>
    );
  }
  return (
    <>
      {!d.envoy_gateway.installed ? <Card><EmptyState title="Envoy Gateway is not installed" body="Gateways need a controller. Run fluxctl gateway expose SERVICE and it is installed for you." /></Card> : null}
      <Card title="Gateways">
        <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Name</th><th>Listeners</th><th>Status</th><th>Routes</th></tr></thead><tbody>
          {d.gateways.map((g) => (
            <tr key={`${g.namespace}/${g.name}`}>
              <td className="fx-muted">{g.namespace}</td>
              <td className="vm-name">{g.name}</td>
              <td className="fx-mono">{g.listeners.map((l) => `${l.protocol} ${l.hostname ?? '*'}:${l.port}`).join(', ') || '—'}</td>
              <td><Pill tone={gatewayTone(g.state)}>{g.summary}</Pill></td>
              <td className="fx-muted">{g.routes.length}</td>
            </tr>
          ))}
        </tbody></table></div>
      </Card>
      <Card title="Routes">
        <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Namespace</th><th>Route</th><th>Gateway</th><th>Hosts</th><th>Matches</th><th>Backends</th><th>Status</th><th /></tr></thead><tbody>
          {d.gateways.flatMap((g) => g.routes.map((r) => (
            <tr key={`${g.namespace}/${g.name}/${r.namespace}/${r.name}`}>
              <td className="fx-muted">{r.namespace}</td>
              <td className="vm-name">{r.name}</td>
              <td className="fx-muted">{g.name}</td>
              <td className="fx-mono">{r.hostnames.join(', ') || '*'}</td>
              <td className="fx-mono fx-muted">{r.matches.join(', ')}</td>
              <td className="fx-mono fx-muted">{r.backends.map(backendText).join(', ') || '—'}</td>
              <td><Pill tone={gatewayTone(r.state)}>{r.summary}</Pill></td>
              <td><Button size="sm" variant="ghost" aria-label={`Copy fluxctl command for ${r.name}`} title={r.inspect} onClick={() => void copy(r.inspect)}><Copy size={14} />Copy as fluxctl</Button></td>
            </tr>
          )))}
        </tbody></table></div>
      </Card>
    </>
  );
}
