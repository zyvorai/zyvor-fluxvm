import { Card, EmptyState, VmDisplay, type Vm } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { hasDisplay } from '../lib/connect';

/** The screen of a `vz` guest, with clicks and keys when you take control. Admin only, like the API behind it. */
export function DisplayTab({ vm, notify }: { vm: Vm; notify: Notify }) {
  if (vm.backend !== 'vz') return <Card><EmptyState title="No screen for this machine" body="A screen is shown for macOS-style (vz) guests. For a Linux machine, use the Console tab." /></Card>;
  if (!hasDisplay(vm)) return <Card><EmptyState title="The machine is not running" body="Start it to see its screen." /></Card>;
  return (
    <Card title="Screen">
      <VmDisplay base={`/v1/vms/${encodeURIComponent(vm.id)}`} screenPath="screenshot" name={vm.name} canControl notify={notify} />
    </Card>
  );
}
