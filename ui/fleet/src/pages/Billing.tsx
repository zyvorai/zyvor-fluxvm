import { Card, Hero, KV, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { GenericTable, flatten, rowsOf } from '../lib/generic';

export function BillingPage({ role }: Ctx) {
  const bill = usePoll(() => api.get<unknown>('/fleet/billing').catch(() => null), 30000);
  const usage = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/usage').catch(() => ({ items: [] })), 'items', 'usage', 'tenants'), 30000);
  const rows = flatten(bill.data);
  return (
    <>
      <Hero eyebrow="Billing & usage" title="What it costs," accent="and why." lede={role === 'operator' ? 'Fleet-wide usage and billing as central computes it.' : 'Your usage this month and what it comes to.'} />
      <div className="fx-grid cols-2">
        <Card title="Billing">{bill.loading && !bill.data ? <Skeleton h={100} /> : rows.length ? <KV rows={rows} /> : <p className="fx-muted">Billing is not set up on this central.</p>}</Card>
        <Card title="Usage">{usage.loading && !usage.data ? <Skeleton h={100} /> : <GenericTable rows={usage.data ?? []} empty="No usage recorded yet." />}</Card>
      </div>
    </>
  );
}
