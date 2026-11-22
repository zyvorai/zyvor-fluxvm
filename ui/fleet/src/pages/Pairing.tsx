// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

import { Button, Card, EmptyState, Hero, Pill, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { useAct } from '../lib/act';

interface Pending { id: string; name: string; code: string; address: string; age_secs: number; expires_in: number }
interface PendingList { enabled: boolean; items: Pending[] }

const enc = encodeURIComponent;
const minutes = (s: number) => (s >= 60 ? `${Math.ceil(s / 60)} min` : `${Math.max(0, s)} s`);

/** Machines that ran `fluxctl cluster pair` and wait for a person. Compare the code with the one on the machine's own screen. */
export function PairingPage({ notify }: Ctx) {
  const list = usePoll(async () => await api.get<PendingList>('/fleet/pair/pending'), 3000);
  const { busy, run } = useAct(notify);
  const items = list.data?.items ?? [];
  const decide = (p: Pending, action: 'approve' | 'deny') =>
    run(action + p.id, () => api.post(`/fleet/pair/${enc(p.id)}/${action}`), action === 'approve' ? `${p.name} may join` : 'Denied', list.refresh);
  return (
    <>
      <Hero eyebrow="Machines asking to join" title="Approve a machine" accent="by its code." lede="On the new machine run fluxctl cluster pair. It shows a short code. Approve only the request whose code matches that screen exactly." />
      {list.data && !list.data.enabled ? <p className="fx-error" role="note">Pairing is off. Start central with --pairing to let machines ask.</p> : null}
      <Card title="Waiting for you">
        {list.loading && !list.data ? <Skeleton h={100} /> : items.length === 0 ? <EmptyState title="Nobody is asking to join" body="Run fluxctl cluster pair on a Mac or Linux machine on this network." /> : (
          <div className="fx-table-wrap"><table className="fx-table"><thead><tr><th>Code</th><th>Name</th><th>From</th><th>Expires in</th><th /></tr></thead><tbody>
            {items.map((p) => (
              <tr key={p.id}>
                <td><Pill tone="warn"><span className="fx-mono">{p.code}</span></Pill></td>
                <td className="vm-name">{p.name}</td>
                <td className="fx-mono fx-muted">{p.address}</td>
                <td className="fx-muted">{minutes(p.expires_in)}</td>
                <td><span className="fx-row-actions">
                  <Button size="sm" variant="primary" busy={busy === 'approve' + p.id} onClick={() => decide(p, 'approve')}>Approve</Button>
                  <Button size="sm" variant="ghost" busy={busy === 'deny' + p.id} onClick={() => decide(p, 'deny')}>Deny</Button>
                </span></td>
              </tr>
            ))}
          </tbody></table></div>
        )}
      </Card>
    </>
  );
}
