import { VmDisplay, VncScreen } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import type { FleetVm } from '../lib/fleet';

const enc = encodeURIComponent;

/**
 * A VM's display through central. A macOS guest is the node's screenshot and input, reached at `/fleet/vms/NODE/ID/screen` and
 * `/input`. Any other VM gets the graphical console (VNC, QEMU machines): central mints a ticket and relays the node's VNC socket,
 * under the same tunnel policy, limits and activity log as SSH. A VM without one says why.
 */
export function VmScreen({ vm, macos, canControl, notify }: { vm: FleetVm; macos: boolean; canControl: boolean; notify: Notify }) {
  const base = `/fleet/vms/${enc(vm.node)}/${enc(vm.id)}`;
  if (macos) return <VmDisplay base={base} screenPath="screen" name={vm.name} canControl={canControl} notify={notify} unreachable="Central is not reachable; retrying." />;
  return <VncScreen key={`${vm.node}/${vm.id}`} ticketPath={`${base}/ws-ticket`} name={vm.name} notify={notify} footer="Clicks and keys go to the VM while it has focus, through central and the VM's node. Opening it is recorded in the activity log." />;
}
