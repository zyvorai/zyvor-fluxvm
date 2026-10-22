import { Card, EmptyState, Hero, Skeleton, api, usePoll } from '@fluxvm/ui-core';
import type { Ctx } from '../App';
import { GenericTable, rowsOf } from '../lib/generic';

/** Warm pools keep ready-to-claim VMs so a launch skips the boot. Read-only here: create and tune them with the API. */
export function PoolsPage(_: Ctx) {
  const list = usePoll(async () => rowsOf(await api.get<unknown>('/fleet/pools'), 'items'), 8000);
  const rows = list.data ?? [];
  return (
    <>
      <Hero eyebrow="Warm pools" title="Machines already" accent="booted and waiting." lede="A pool keeps a few VMs ready to hand out, then refills. Create and tune pools through the API; this shows what they hold." />
      <Card>
        {list.loading && !rows.length ? <Skeleton h={120} /> : rows.length === 0 ? <EmptyState title="No warm pools" body="Create one with POST /fleet/pools." /> : <GenericTable rows={rows} empty="" />}
      </Card>
    </>
  );
}
