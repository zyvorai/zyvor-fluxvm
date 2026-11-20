# vGPU (mediated devices) and GPU health (Linux, QEMU)

FluxVM lists, creates and removes mediated devices (mdev) and passes them to
QEMU by UUID. A vendor vGPU host driver (for NVIDIA, the vGPU manager)
registers each physical GPU as an mdev parent with one or more profiles; each
instance of a profile is a separate VFIO device a VM can own.

## Inventory

`GET /v1/host/mdev` reads `/sys/class/mdev_bus` and `/sys/bus/mdev/devices`:

```json
{
  "parents": [{
    "parent": "0000:3b:00.0", "pci": true, "vendor_id": 4318, "device_id": 8373, "numa_node": 0,
    "types": [{
      "id": "nvidia-558", "name": "GRID A100-3-20C", "description": "num_heads=1, frl_config=60, framebuffer=20480M",
      "device_api": "vfio-pci", "available_instances": 2, "framebuffer_mib": 20480,
      "mig": {"slices": 3, "memory_gib": 20}
    }]
  }],
  "instances": [{"uuid": "…", "parent": "0000:3b:00.0", "type_id": "nvidia-558", "iommu_group": 120,
                 "allocated": true, "held": true}]
}
```

- `vendor_id` and `device_id` are numbers (`4318` is `0x10de`).
- `framebuffer_mib` comes from NVIDIA's `framebuffer=` in the description, or
  "N MB video memory" (the kernel's `mbochs` sample). It is absent when the
  driver does not say.
- `mig` is parsed from MIG-backed profile names (`GRID A100-3-20C`: 3 slices,
  20 GiB).
- `allocated` means a VM lists the UUID in `vfio_devices`; `held` means its
  QEMU has the device open.

## Create and remove

`POST /v1/host/mdev` (admin) with `{"parent": "mbochs", "type": "mbochs-small", "uuid": "…"}` creates an instance
and returns it with `201`. `uuid` is optional; FluxVM picks one when it is missing. Creating a UUID that already
exists on the same parent and type returns it again, so callers can retry. The same UUID on another parent or
type is `409`, and a profile with no free instances is `503`.

`DELETE /v1/host/mdev/{uuid}` (admin) removes an instance and returns `204`, also when it is already gone. An
instance that a VM lists or holds is refused with `409`.

## Passing an instance to a VM

List the UUID in `CreateVmRequest.vfio_devices`, next to or instead of PCI addresses. QEMU gets
`-device vfio-pci,sysfsdev=/sys/bus/mdev/devices/<uuid>`. Each entry must be a PCI address or a UUID, and an
entry may appear only once. The VM's device cgroup allows the instance's `/dev/vfio/<group>` and
`/dev/vfio/vfio`.

Everything that applies to VFIO VMs applies here: no live migration, no hibernation, and no balloon.

## GPU health

`GET /v1/host/gpus/health` judges each display-class PCI device:

```json
{"items": [{"bdf": "0000:3b:00.0", "status": "degraded", "reasons": ["PCIe link runs x8 of x16"],
            "aer": {"correctable": 0, "nonfatal": 0, "fatal": 0},
            "link": {"current_width": 8, "max_width": 16, "current_speed": "16.0 GT/s PCIe"},
            "nvidia": {"memory_total_mib": 81920, "memory_used_mib": 0, "temperature_c": 41,
                       "ecc_uncorrected": 0, "mig_mode": "Enabled", "retired_pages_pending": false}}]}
```

- `unhealthy`: config space reads as `0xffff` (the device fell off the bus),
  fatal AER errors, or uncorrected ECC errors.
- `degraded`: non-fatal AER errors, a link narrower than the device's
  maximum, retired pages pending a reset, or 90 °C or more.
- `nvidia` is filled from `nvidia-smi` when it is installed (5 s timeout).

`GET /v1/capabilities` reports `host.mdev` and `host.gpu_health`.

## Tested

- Sysfs parsing, create, remove, health rules and `nvidia-smi` output: unit
  tests against a fake sysfs.
- From Kairon (`scripts/lab-vgpu.sh`, kernel 7.0, QEMU 10.2) with the
  kernel's `mbochs` sample driver: inventory, create, a VM booted with an
  instance by UUID (the guest saw the display function and its BAR), remove
  after stop, and health of the host's onboard VGA.
- Not run: NVIDIA vGPU, MIG-backed profiles and `nvidia-smi` on a real GPU.
