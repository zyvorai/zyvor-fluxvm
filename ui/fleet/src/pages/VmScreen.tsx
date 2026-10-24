import { VmDisplay } from '@fluxvm/ui-core';
import type { Notify } from '../App';
import type { FleetVm } from '../lib/fleet';

const enc = encodeURIComponent;

/** A VM's display through central: the node's screenshot and input, reached at `/fleet/vms/NODE/ID/screen` and `/input`. */
export function VmScreen({ vm, canControl, notify }: { vm: FleetVm; canControl: boolean; notify: Notify }) {
  return <VmDisplay base={`/fleet/vms/${enc(vm.node)}/${enc(vm.id)}`} screenPath="screen" name={vm.name} canControl={canControl} notify={notify} unreachable="Central is not reachable; retrying." />;
}
