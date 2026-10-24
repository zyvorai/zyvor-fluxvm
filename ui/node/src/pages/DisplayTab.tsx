import { Card, EmptyState, VmDisplay, VncScreen, type Vm } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import { displayKind } from '../lib/connect';

/** A machine's screen: a `vz` guest through screenshots and input events, a QEMU machine through VNC. Admin only, like the API behind it. */
export function DisplayTab({ vm, notify }: { vm: Vm; notify: Notify }) {
  const kind = displayKind(vm);
  if (vm.backend !== 'vz' && vm.backend !== 'qemu') return <Card><EmptyState title="No screen for this machine" body="A screen is shown for QEMU machines (VNC) and macOS-style (vz) guests. This machine has the Console tab." /></Card>;
  if (!kind) return <Card><EmptyState title="The machine is not running" body="Start it to see its screen." /></Card>;
  return (
    <Card title="Screen">
      {kind === 'vnc' ? (
        <VncScreen key={vm.id} ticketPath={`/v1/vms/${encodeURIComponent(vm.id)}/ws-ticket`} name={vm.name} notify={notify} footer="Clicks and keys go to the machine while it has focus. The console is a window onto the same screen as any other VNC client, so it is admin only." />
      ) : (
        <VmDisplay base={`/v1/vms/${encodeURIComponent(vm.id)}`} screenPath="screenshot" name={vm.name} canControl notify={notify} />
      )}
    </Card>
  );
}
