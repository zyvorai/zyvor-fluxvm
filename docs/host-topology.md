# Host topology and NUMA binding

FluxVM reports the host's hardware graph and can bind a QEMU VM's guest NUMA nodes to host NUMA nodes. Kairon's
topology scheduler uses both: it reads the graph into a `NodeTopology` object and sends the chosen layout on create.

## Reading the graph

`GET /v1/host/topology` reads sysfs and returns:

- `numaNodes[]`: `id`, `cpus`, `threadSiblings`, `memoryMib`, `freeMemoryMib`, `hugepages[]` (`sizeKib`, `total`,
  `free`) and `distances`. A kernel without NUMA support is reported as one node holding every online CPU.
- `devices[]`: GPUs, NICs, NVMe controllers and accelerators with `bdf`, `class`, vendor and device IDs, `driver`,
  `numaNode`, `pcieRoot`, `iommuGroup` and its members, `sriov` (`totalVfs`, `numVfs`, `vfs`), `physfn` for a VF,
  `netdevs` and `linkSpeedMbps` for NICs, `sizeBytes` for NVMe, `vramGib` (operator-recorded) and `allocated` (listed in
  a VM's `vfio_devices`).

`capabilities.host` says whether the build supports `topology`, `numaBinding` and `pciBind`.

## Binding a VM

`numa` on `POST /v1/vms` (QEMU backend only):

```json
{
  "vcpus": 4,
  "memory_mib": 4096,
  "numa": [
    {"host_node": 0, "vcpus": [0, 1], "memory_mib": 2048},
    {"host_node": 1, "vcpus": [2, 3], "host_cpus": [24, 25, 26, 27], "memory_mib": 2048}
  ]
}
```

- Memory must add up to `memory_mib`, and every vCPU `0..vcpus` must be on exactly one node. `numa` cannot be combined
  with `numa_node` or `cpuset`. A bad layout is a 400 at create.
- Each guest node gets its own memory backend with `host-nodes=H,policy=bind`: `memory-backend-ram`, or
  `memory-backend-file` on `/dev/hugepages` when `hugepages` is set, or `memory-backend-memfd,share=on` when virtiofs or
  `shared_memory` needs shared memory. QEMU fails to start if the kernel cannot bind the memory.
- vCPU hotplug slots above `vcpus` (up to `max_vcpus`) are placed on the last guest node.
- After QEMU starts (create, start and the incoming side of a live migration), FluxVM reads each vCPU's thread ID with
  `query-cpus-fast` and pins it with `sched_setaffinity` to `host_cpus`, or to every CPU of `host_node` when
  `host_cpus` is empty. Pinning is best-effort, like cgroup setup: a failure is logged and the VM keeps running.
- The legacy `numa_node` without `cpuset` now means one bound guest node holding the whole VM.

Not bound: memory added later with `hotplug/memory` (placed on guest node 0, not host-bound) and vCPUs added with
`hotplug/cpu` (not pinned until the next start). `POST /v1/vms/{id}/resources` with `cpuset_cpus` changes the cgroup
cpuset, which can override the pinning.

## Checking what applied

`GET /v1/vms/{id}/numa` on a running or paused QEMU VM returns `nodes` (the layout it was launched with), `vcpus[]`
(`vcpu`, `thread_id`, `host_node`, `host_cpus` read from `/proc/<pid>/task/<tid>/status`) and `memdevs[]` (`id`,
`size_mib`, `host_nodes`, `policy` from QMP `query-memdev`).

## Devices

`POST /v1/host/pci/bind` with `{"bdf": "0000:3b:00.0"}` binds the device's whole IOMMU group to `vfio-pci`, refusing a
group that is held or allocated to a VM. `POST /v1/host/pci/release` with `{"bdf": ..., "restore_driver": true}` puts the
previous host driver back. Both require the admin role. The GPU routes (`/v1/host/gpus/bind|release`) are the same
operation for GPUs. Slices of a GPU (vGPU, mediated devices) are listed, created and passed through by UUID; see
[vgpu.md](vgpu.md).

## Tested

- Argument building, validation, sysfs parsing: unit tests.
- A real QEMU (10.2, TCG) started with a bound node, then pinned and read back through `/v1/vms/{id}/numa`'s code path:
  `FLUXVM_QEMU_NUMA_TEST=1 cargo test -p fluxvm-qemu --test numa_qemu`. Run on a single-NUMA-node host only.
- Not run: a multi-node host, and device binding on real NIC, NVMe or SR-IOV hardware.
