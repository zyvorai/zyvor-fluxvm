# USB passthrough

Design for attaching a host USB device to a FluxVM guest.

This is not an implementation. It records what the tree already does, what
each VMM can realistically support, and a recommended path for Linux. The
Kiri parity roadmap (`feat/kiri-parity-roadmap`, `docs/KIRI-PARITY.md`) lists
"attach host USB devices to a guest" as a P3 item and marks it "not a current
focus." This note is that design.

## What the code supports today

FluxVM is a multi-hypervisor control plane (`docs/architecture.md`). One
`VmManager` drives five backends. USB support is uneven, and only the macOS
`vz` path has any USB code.

| Backend | Crate | Guest-facing USB today | Host-device passthrough today |
|---|---|---|---|
| QEMU/KVM | `fluxvm-qemu` | None. Disks are virtio-blk or SCSI; there is no `qemu-xhci`, `usb-host`, or `usbredir` argument. | None as USB. `CreateVmRequest.vfio_devices` can pass a PCI function or mdev UUID into QEMU as `vfio-pci` (used for GPUs; see [vgpu.md](vgpu.md)). An operator could list a host xHCI controller there today, but FluxVM does not inventory USB, bind a USB function, or hot-plug a device. `spec_lint` treats `vfio_devices` as QEMU-only. |
| Cloud Hypervisor | `fluxvm-cloud-hypervisor` | None. The CH device model FluxVM uses is virtio (block, net, vsock, fs, balloon). | None. `vfio_devices` is refused for this backend. |
| Firecracker | `fluxvm-firecracker` | None. Minimal virtio-mmio set (block, net, vsock, balloon, and the usual jailer constraints). | None. Firecracker has no USB or general VFIO surface in this tree. |
| In-tree KVM (`fluxvm` / `FluxVmBackend`) | `fluxvm-hypervisor` | None. Virtio-mmio (and a limited virtio-pci path) for net, block, vsock, balloon, rng, console, plus optional virtio-fs. | None. [DESIGN.md](../crates/fluxvm-hypervisor/DESIGN.md) lists USB among the things the in-tree VMM will not implement, on purpose: the point of this engine is a tiny device surface. |
| Apple Virtualization (`vz`) | `fluxvm-apple` + `fluxvm-vz-runner` | Yes. One to four XHCI controllers (`apple.usb_controller` / `apple.usb_controllers`, macOS 15+). USB mass-storage disk images at boot or hot-plugged (macOS 15+). The runner also adds an emulated USB keyboard and a USB screen-coordinate pointing device for the console; that is not device passthrough. | Partial. Physical accessories attach as `VZUSBPassthroughDevice` through Apple's Accessory Access API (macOS 27+, runner built with the macOS 27 SDK and `AccessoryAccess`). This path is written and unit-tested; it has **not** been verified on hardware (see [macOS](#macos-host-considerations)). |

Transport on the Linux microVM backends is virtio (virtio-mmio for Firecracker
and the in-tree engine; virtio-pci for QEMU and for Cloud Hypervisor). There
is no virtio-usb device in any of them, and this design does not propose
adding one. Guest-visible USB, when it exists, is an emulated XHCI
controller: Virtualization.framework's `VZXHCIControllerConfiguration` on
`vz`, and (proposed) QEMU's `qemu-xhci` on Linux.

### macOS `vz` surface (already in the tree)

Configuration (create-time, `request.apple`):

- `usb_controller: true` — add an XHCI controller (macOS 15+).
- `usb_controllers: 1..=4` — that many buses; implies `usb_controller`. A
  hot-attached disk image can pick a bus with `usb_bus` / `fluxctl disk attach
  --usb-bus N`. Physical passthrough uses bus 0. The guest may show one more
  XHCI controller than configured (Virtualization.framework's own layout).

Control-socket commands on `fluxvm-vz-runner`: `usb-list`, `usb-attach` (disk
image path, optional `read_only` and `bus`), `usb-detach` (by UUID),
`usb-physical-list`, `usb-physical-attach` (by Accessory Access
`registry_id`). A host-initiated unplug emits
`usb-passthrough-disconnected`.

REST and CLI that already exist:

- `GET /v1/vms/{id}/vz/usb` — devices on the VM's controllers now
  (`kind` is `mass-storage` or `passthrough`).
- `GET /v1/vms/{id}/vz/usb/physical` — accessories the signed
  `FluxVMUSBAccess.app` helper has been authorised to export.
- `POST /v1/vms/{id}/vz/usb/physical` with `{"registry_id": …}` — attach one;
  returns a `uuid` that `usb-detach` understands.
- `fluxctl vz usb ID` — list attached devices.
- `fluxctl vz usb ID --physical` — list authorised host accessories.
- `fluxctl vz usb ID --physical --attach REGISTRY_ID` — attach one.
- MCP tool `vm_vz_status` with `what=usb` or `usb_physical`.
- `fluxctl helper-agent usb install|uninstall|status` — LaunchAgent for
  `dev.zyvor.fluxvm.usbd` (`FluxVMUSBAccess.app`).

There is still **no** REST route or `fluxctl` verb that detaches a
passthrough device. Mass-storage images detach through the existing disk
API (`fluxctl disk detach` / the runner's `usb-detach`). Passthrough detach
is the same runner command, but nothing in the HTTP API or CLI calls it
yet.

Older sentences in [macos.md](macos.md) and
[macos-architecture.md](macos-architecture.md) that said physical
passthrough had "no REST route or `fluxctl` command" were true of the
control socket only; list and attach are now wired as above. This document
is the SoT for the current surface.

USB mass-storage hot-plug for disk *images* (and the OCI sandbox warm pool,
which hot-attaches a rootfs as USB mass storage) is a different feature. It
is verified by `scripts/vz-devices-live-test.sh` and
`scripts/vz-usb-buses-live-test.sh`. It is not host-device passthrough.

### Snapshot behaviour on `vz` (already in the tree)

`prepareUSBForSave` in the runner:

- A **hot-plugged USB disk image** blocks a memory snapshot. Restoring a
  state that was saved with one, without the disk, can crash the guest on
  macOS 27 (Apple 177528319). The operator must `usb-detach` first.
- A **passthrough** device is detached before save (Apple 174267926, macOS
  27 cannot save a VM that still has one). The `save` reply lists those
  UUIDs in `usb_detached`. They are not automatically re-attached on
  restore.
- Create-time USB mass-storage disks that are part of the static
  configuration are not in that hot-plug set; `save-support` /
  `validateSaveRestoreSupport` can still refuse a device set the framework
  cannot checkpoint.

A claimed OCI sandbox whose root is on a hot-attached USB disk is not
hibernated.

## Goals

- Let an operator list host USB devices and attach or detach a chosen
  device on a running (or next-boot) guest, with a single `fluxctl usb`
  surface that maps onto the backend that can actually do it.
- Keep the macOS path: wrap the existing `vz` routes rather than inventing
  a second Accessory Access stack.
- On Linux, add device-level passthrough for **QEMU only**, with a clean
  refusal on every other backend.
- Require an explicit grant. Do not attach the host's keyboard, a mounted
  system disk, or a device already assigned to another VM.
- Say honestly what happens to snapshots, live migration, balloon and
  hibernation when a USB device is attached.
- Stay out of the in-tree KVM and Firecracker device models.

## Non-goals

- Implementing an XHCI controller, usbredir client, or VFIO binder inside
  `fluxvm-hypervisor` or Firecracker.
- Cloud Hypervisor USB emulation (CH has no USB controller in the
  configuration FluxVM generates; this design does not add one).
- Virtio-usb or a new guest driver.
- Passing through a USB device to a Secure Containers guest as a
  first-class Pod device plugin (a later exercise, if ever).
- USB/IP as a product feature (evaluated below, then declined).
- Automatically sharing every newly plugged host device with a VM.
- Claiming that physical passthrough on macOS works until a Developer ID
  build with a real Accessory Access profile is verified on hardware.

## Use cases

- A token, smart card, FIDO key, or hardware-backed PKCS#11 device inside a
  development or CI guest.
- A USB serial adapter, JTAG probe, or microcontroller board for firmware
  work, without giving the guest the whole host controller.
- A USB mass-storage stick or camera in a desktop-like `vz` or QEMU guest.
- A Linux guest on a Mac that needs a physical accessory Apple has already
  consented through Accessory Access.
- (Secondary) Passing an entire host xHCI controller into a QEMU guest
  that must see a native host controller, including devices that plug in
  later on that controller. This is the VFIO option, not the default.

## Options

### 1. VFIO passthrough of a whole xHCI controller

Unbind the host PCI xHCI function (and every other function in its IOMMU
group) from `xhci_hcd`, bind them to `vfio-pci`, and add the BDF to QEMU
as `-device vfio-pci,host=…`. FluxVM already does this for GPUs and mdev
UUIDs (`fluxvm_core::gpu`, `CreateVmRequest.vfio_devices`,
[vgpu.md](vgpu.md), [sandbox-gpus.md](sandbox-gpus.md)).

Trade-offs:

- Best performance and the most faithful device behaviour (isochronous
  audio/video, quirky devices, USB 3 speeds) because the guest talks to
  the real controller.
- The **entire controller** leaves the host, including every device on it
  (often the host keyboard, a built-in camera, or a USB NIC on a workstation).
- IOMMU groups on many boards are coarse: the xHCI function may share a
  group with other devices that then also have to move.
- Same constraints as other VFIO VMs in this tree: no balloon, no
  hibernation, no live migration.
- Works only where an IOMMU is on and `vfio-pci` is available (typically a
  Linux host with QEMU). Not applicable to Firecracker, the in-tree engine,
  Cloud Hypervisor in this tree, or macOS (Apple does not expose VFIO).
- Hot-plug of a *device* is the controller's problem: once the controller
  is in the guest, a newly inserted device on that bus appears there, which
  may or may not be what the operator wanted.
- Needs the same host permissions FluxVM already documents for GPUs
  (IOMMU, `vfio-pci`, device cgroup allow for `/dev/vfio/<group>`).

Useful as an escape hatch for a dedicated USB controller (a PCIe USB card
in its own IOMMU group), not as the way to attach a single thumb drive.

### 2. usbredir over a socket

QEMU already speaks the [usbredir](https://www.spice-space.org/usbredir.html)
protocol (`usb-redir` on a `qemu-xhci` bus). A host-side client
(`usbredirserver`, a small FluxVM helper, or a remote spice-usbredir
source) opens a UNIX or TCP socket and forwards URBs.

Trade-offs:

- Device-level: one gadget, not the whole controller.
- The host can keep other devices. The helper process is the privilege
  boundary (`LIBUSB`, `/dev/bus/usb/*`), not the VMM's address space
  quite as directly as USBFS-in-QEMU.
- Natural fit for "attach this device from another machine" and for a
  sandboxed QEMU that should not itself have udev access.
- Adds a moving part and a userspace protocol. Isochronous devices and
  some USB 3 devices are historically weaker than VFIO.
- The socket is a security boundary: anyone who can write to it owns the
  device. UNIX sockets with mode `0600` in the VM workspace are fine; a
  TCP listener is not the default.
- Snapshot/migration: QEMU can sometimes migrate with usbredir if the
  destination reconnects; in practice FluxVM should detach or refuse,
  same as VFIO, until that is proven.

A good second backend once local `usb-host` works, and the right shape
for a remote or jailed VMM.

### 3. USB/IP

The in-kernel USB/IP stack (`usbip_host` / `vhci_hcd`) exports a device
over TCP and imports it on another machine as a virtual host controller.

Trade-offs:

- Not tied to QEMU; a guest could in theory run the client itself. That
  punches a hole through the guest network policy and is harder to audit
  than a VMM-level attach.
- The protocol and the upstream tools have a long history of
  reliability and security issues. Traffic is not authenticated or
  encrypted unless something else wraps it.
- Hotplug, reset, and suspend behaviour is weaker than usbredir or VFIO.
- FluxVM would be taking on a kernel-module and a network service for a
  problem QEMU already solves locally.

Declined as a FluxVM feature. Operators who already run USB/IP can do so
inside or beside a guest without API support.

### 4. Emulated XHCI in the VMM (`qemu-xhci` / virtio-based USB)

QEMU's `qemu-xhci` is an emulated USB 3 controller. Devices hang off it:
`usb-host` (Linux USBFS, `vendorid`/`productid` or `hostbus`/`hostaddr`),
`usb-redir`, `usb-storage`, `usb-tablet`, and so on. This is not a virtio
transport; the guest sees a standard XHCI PCI function.

Cloud Hypervisor, Firecracker, and the in-tree engine do not have an
equivalent controller, and this design does not propose writing one.
Apple's `VZXHCIControllerConfiguration` is the same idea on `vz`, already
shipped.

Trade-offs of QEMU `usb-host` on `qemu-xhci`:

- Device-level attach and detach (QMP `device_add` / `device_del`) without
  taking the host controller.
- Reuses QEMU, which FluxVM already supervises and jails.
- Needs access to `/dev/bus/usb` or a passed file descriptor. A tightened
  jailer or seccomp profile must allow that explicitly.
- USBFS is a large kernel interface in the QEMU process. That is a worse
  isolation story than usbredir's helper, but it is also the simplest and
  the usual QEMU operator path.
- Performance and isochronous behaviour are good enough for HID, serial,
  storage, and most tokens; not the path for a 10 Gbit USB NIC (use VFIO
  of a dedicated controller, or a PCIe NIC, instead).

## Recommended approach

Match the backend to what it already is, rather than inventing a sixth USB
stack.

1. **macOS `vz`** — keep Accessory Access + `VZXHCIController` as the only
   physical-passthrough path. Fill the gaps: a detach API, a unified
   `fluxctl usb` that calls the existing `/v1/vms/{id}/vz/usb*` routes, and
   a Developer ID build of `FluxVMUSBAccess.app` so the helper can actually
   run under SIP. Do not attempt VFIO, usbredir, or USB/IP on macOS.
2. **Linux QEMU** — default to **emulated `qemu-xhci` plus `usb-host`**
   (USBFS) for a single local device, with **usbredir over a UNIX socket**
   as an opt-in for jailed QEMU or a remote source. Expose **whole-controller
   VFIO** only as an explicit mode that reuses `vfio_devices` and the
   existing GPU bind/release machinery, and only when the operator names a
   PCI xHCI function (preferably one alone in its IOMMU group).
3. **Cloud Hypervisor, Firecracker, in-tree KVM** — refuse with a message
   that names the QEMU (or `vz`) alternative. Do not grow those device
   models.

Why this split: the in-tree engine and Firecracker exist to stay small;
USB would undo that. Cloud Hypervisor in this tree has no USB controller
and `vfio_devices` is already QEMU-only. QEMU is the Linux VMM that already
knows how to do USB and already has VFIO, QMP hotplug, and a jailer story.
macOS already has a first-party API; wrapping it is less work and more
correct than a Linux-style path that cannot run there.

## Host permissions, udev and hotplug

### Linux

- Listing devices can be a read of `/sys/bus/usb/devices` (and optionally
  `udev` properties). That should work without root.
- Attaching with `usb-host` needs the device node under `/dev/bus/usb` to
  be readable and writable by the QEMU process. A udev rule in the FluxVM
  package (for example tagging `uaccess` or a `fluxvm-usb` group, plus
  `MODE="0660"`) is the supported way; `chmod` by hand is not.
- The rule must not be "every USB device is public." A tag or an ACL that
  `fluxctl usb attach` (as admin) adds for the life of the assignment, and
  removes on detach or VM delete, is the intended shape.
- Unbind the host driver (`usbhid`, `usb-storage`, …) before QEMU opens
  the device, and rebind on detach, so the host does not keep using a
  gadget the guest now owns. Persist this next to the VM record, the same
  way GPU bind remembers the previous driver.
- `udev` `add`/`remove` events should update the host inventory and, if
  the device was assigned, emit a VM event (`usb-passthrough-disconnected`,
  matching the `vz` runner). They must not auto-attach.
- VFIO mode uses the existing IOMMU-group checks in
  `fluxvm_core::placement` and `fluxvm_core::gpu`: refuse a split group,
  refuse a group that is already held, allow `/dev/vfio/<group>` in the
  device cgroup.
- A QEMU jailer or Landlock/seccomp profile that today sees only the disk
  and TAP must be granted the specific device fd (preferred) or the
  `/dev/bus/usb/BBB/DDD` node. Do not punch a hole to all of `/dev/bus/usb`
  by default.

### macOS

- Physical attach already requires the user to grant the accessory to
  `FluxVMUSBAccess.app` in the Accessory Access UI. The broker then hands
  an `AAUSBAccessory` XPC representation to the runner. That consent is
  the permission model; FluxVM should not try to bypass it.
- The helper is a LaunchAgent (`dev.zyvor.fluxvm.usbd`), installed by
  `fluxctl helper-agent usb install`, and it must be signed with
  `com.apple.developer.accessory-access.usb` and an Apple-issued profile
  ([macos-signing.md](macos-signing.md)). An ad-hoc signature is killed
  at launch on a SIP-on host.

## Security isolation

- Attaching a USB device is equivalent to giving the guest that hardware:
  a HID device can inject keystrokes on the guest (not the host, once
  unbound), a storage device can read and write media, a NIC can bridge
  networks, a security key can sign. Treat attach as an admin operation
  (`require_admin` on the new routes, same as host GPU bind).
- Never auto-select a device by "the only one plugged in" for a
  non-interactive API call. Require a stable id (bus/port path, or
  `vid:pid` plus serial when unique).
- Refuse to attach a device the host is using as its root disk, its
  only keyboard, or a configured management NIC, and refuse a device
  already attached to another VM.
- Log attach and detach (VM id, device id, operator) and do not log
  device serials at debug level in shared support bundles without the
  same redaction rules as other hardware identifiers.
- usbredir UNIX sockets live in the VM workspace, mode `0600`, owned by
  the daemon user. No TCP listener unless explicitly configured, and
  then only on loopback with an explicit flag.
- For VFIO, the existing rule stands: the whole IOMMU group moves, balloon
  and migration stay off, and confidential / SNP profiles remain
  incompatible (`validate` already rejects balloon with `vfio_devices`).
- Guests in network-isolated or "offline" sandbox modes still see the
  USB device: USB is not a network path FluxVM's egress policy can
  filter. Call that out in the API so an offline sandbox is not assumed
  to be air-gapped after a NIC or modem is attached.

## API and CLI surface

Keep the `vz`-specific routes. Add a backend-neutral layer that delegates.

Proposed CLI (not implemented):

```bash
fluxctl usb list                  # host inventory (Linux sysfs, or vz physical list)
fluxctl usb list --vm ID          # devices attached to that VM
fluxctl usb attach ID --device BUS:DEV
fluxctl usb attach ID --vidpid VVVV:PPPP [--serial S]
fluxctl usb attach ID --redir /path/to.sock
fluxctl usb attach ID --vfio 0000:00:14.0    # QEMU, whole controller
fluxctl usb detach ID --id UUID
```

On a `vz` VM, `list` / `attach` / `detach` call the existing
`/v1/vms/{id}/vz/usb*` routes (and a new detach route). `--redir` and
`--vfio` are Linux/QEMU-only and must fail clearly on `vz`.

On a QEMU VM, the same verbs talk to the new routes below. On every other
backend they return a 501/422 with the reason.

Proposed REST (not implemented):

- `GET /v1/host/usb` — host inventory.
- `GET /v1/vms/{id}/usb` — attached devices (on `vz`, the same payload as
  `GET /v1/vms/{id}/vz/usb`).
- `POST /v1/vms/{id}/usb` — attach. Body names one of `device`, `vidpid`,
  `redir`, `vfio`, or (on `vz`) `registry_id`.
- `DELETE /v1/vms/{id}/usb/{uuid}` — detach (this is also the missing `vz`
  detach).

Create-time configuration may name devices to attach at start, so a VM
that always needs a token can be restarted without a second call. See
[Config schema](#config-schema).

`fluxctl vz usb` stays as a thin alias so existing scripts do not break.

## Config schema

Proposed optional field on `CreateVmRequest` (Linux QEMU; ignored or
refused elsewhere except as noted):

```json
{
  "backend": "qemu",
  "usb": {
    "xhci": true,
    "devices": [
      {"kind": "host", "bus": 1, "dev": 3},
      {"kind": "host", "vid": "1050", "pid": "0407", "serial": "1234"},
      {"kind": "usbredir", "socket": "/run/fluxvm/vms/<id>/usbredir.sock"},
      {"kind": "vfio", "pci": "0000:00:14.0"}
    ]
  }
}
```

- `xhci` defaults to true when `devices` is non-empty. It adds a single
  `qemu-xhci` device. VFIO-of-the-host-controller does not also add
  `qemu-xhci` for those devices (the guest sees the real controller).
- `kind: "host"` is USBFS `usb-host`. Identity is `bus`+`dev` or
  `vid`+`pid` with optional `serial`. Ambiguous `vid:pid` without a
  unique serial is refused.
- `kind: "usbredir"` is a UNIX socket path inside the workspace (or a
  path the daemon is willing to stat and restrict).
- `kind: "vfio"` is a PCI xHCI function. It is recorded both here and,
  internally, in the same bind/release path as `vfio_devices`. Listing the
  same BDF in both fields is refused as a duplicate.
- On `vz`, continue to use `apple.usb_controller` /
  `apple.usb_controllers`. Do not accept this `usb` object there in the
  first implementation; physical attach stays a runtime operation after
  Accessory Access consent (the accessory is often not plugged in at
  create time, and consent is interactive).

TOML in `fluxvm.toml` may later grow a `[usb]` section for udev group
name, whether VFIO is allowed, and a device allow-list. That is not
required for the first QEMU slice.

## Snapshot and migration

| Situation | Policy |
|---|---|
| `vz` hot-plugged USB disk image | Already refused until detached. Keep that. |
| `vz` passthrough device | Already detached before save; listed in `usb_detached`; not auto-reattached. Keep that, and document it on the unified `usb` verbs. |
| QEMU `usb-host` / usbredir | Refuse memory snapshot, live migration and hibernation while a device is attached, or detach first and report the UUIDs (operator-selectable later; default is refuse, which is safer than a surprise unplug). |
| QEMU VFIO xHCI (or any `vfio_devices` entry) | Already incompatible with balloon, hibernation and live migration. No change. |
| Firecracker / in-tree KVM / CH | No USB devices, so no new snapshot rules. |
| Restore / incoming migrate | Do not invent a device that is no longer present. Re-attach only on an explicit `usb attach` (or a create-time `usb.devices` entry that still resolves). |

Live migration of a USB device's in-flight URBs is not a goal. A CI guest
that needs a token should take a disk snapshot without the device, or
re-attach after restore.

## macOS host considerations

- Physical passthrough is macOS 27+ and needs a runner built with the
  macOS 27 SDK (`compiler(>=6.4)` in `USBPassthrough.swift`) and the
  Accessory Access framework.
- `FluxVMUSBAccess.app` needs the restricted entitlement
  `com.apple.developer.accessory-access.usb` and an Apple provisioning
  profile. Ad-hoc and plain "Apple Development" signatures are killed at
  launch on a SIP-on host (the same class of failure as
  `com.apple.vm.networking` for `fluxvm-vmnetd`). This was observed on
  the M4 test Mac: the helper exited 137 and no accessory was attached.
  A Developer ID build with the profile is required before this path can
  be called verified. See [macos-signing.md](macos-signing.md) and
  [macos-architecture.md](macos-architecture.md) §14.
- USB *disk image* hot-plug is a different, already-verified path (macOS
  15+, `scripts/vz-devices-live-test.sh`).
- Virtualization.framework does not offer VFIO or usbredir. There is no
  Linux-style `/dev/bus/usb` to hand to the runner.
- Nested Linux VMs on `vz` (`nested_virtualization`, macOS 15, M3 or
  later) do not get a second USB passthrough API from FluxVM; a nested
  hypervisor would have to use whatever the guest OS provides.
- An unlocked login session is already required for some `vz` save/restore
  operations. Physical USB consent is interactive in the same way: it is
  a user-session feature, not a headless daemon-only feature.

## Phases

0. **This document** and links from the roadmap and macOS docs.
1. **Honesty and wrap the `vz` path.** Fix remaining stale "no REST / no
   CLI" wording. Add `DELETE /v1/vms/{id}/usb/{uuid}` (and the `vz`
   equivalent) for passthrough detach. Introduce `fluxctl usb list|attach|detach`
   as a front end to the existing `vz` routes. Unit-test the routing and
   the refusal on non-`vz` VMs. Still no hardware claim for physical
   attach until the signed helper runs.
2. **Linux inventory.** `GET /v1/host/usb` from sysfs, `fluxctl usb list`,
   unit-tested against a fake sysfs. Read-only; no attach yet.
3. **QEMU `qemu-xhci` + `usb-host`.** Create-time `usb.devices` and runtime
   attach/detach via QMP, udev/group handling, unbind/rebind, jailer fd
   passing, snapshot/migration refusal, admin-only routes, tests with a
   fake QMP and a fake sysfs. A live test against a real gadget is
   opt-in lab hardware, like the vGPU `mbochs` run.
4. **usbredir** UNIX-socket backend for jailed or remote sources, same
   attach API (`kind: "usbredir"`).
5. **VFIO whole-controller** as an explicit `kind: "vfio"` that reuses
   `vfio_devices` bind/release and placement checks, documented as the
   "dedicated PCIe USB card" path.
6. **Only after a signed helper is verified:** macOS physical passthrough
   marked verified in the hardware log; no API change expected.

Each phase should land as its own PR. Nothing here authorises a USB
implementation inside Firecracker or `fluxvm-hypervisor`.

## Open questions

- Should a create-time `usb.devices` entry that does not match any plugged
  device fail VM start, or start and attach later when the device appears?
  (Proposal: fail start for `bus`+`dev` and for `vid:pid`+serial; never
  wait indefinitely.)
- Is "detach-then-snapshot" or "refuse snapshot" the better default for
  QEMU `usb-host`? (`vz` already mixes both: refuse for hot-plugged
  disks, detach for passthrough.) Proposal: refuse, with an explicit
  `--detach-usb` flag later.
- How stable is `bus:dev` across a host reboot? Prefer the kernel's
  port-path (`usb-0:1.2`) or `vid:pid`+serial in any saved spec.
- Should `GET /v1/host/usb` hide devices the caller is not allowed to
  attach (other users' grants, the host's own keyboard)? Proposal: show
  them, marked `blocked` with a reason, so the inventory is honest.
- Do we ever want a Device Plugin / CRA for Kubernetes Secure Containers,
  or is `fluxctl usb` on a named VM enough?
- For VFIO, do we automatically extend `vfio_devices` in the stored VM
  spec when `usb.devices` contains a `kind: "vfio"` entry, or keep a
  single source of truth in `usb`? Proposal: one source (`usb`), and the
  QEMU backend projects it into the same `-device vfio-pci` path.
- Can a Developer ID profile for `com.apple.developer.accessory-access.usb`
  be issued for the open-source package, or only for a branded Zyvor
  build? Until that is answered, physical passthrough on macOS stays
  "written, not verified."

## Related documents

- [architecture.md](architecture.md) — control plane and backends
- [macos.md](macos.md) / [macos-architecture.md](macos-architecture.md) —
  `vz` USB controllers, disk hot-plug, physical passthrough status
- [macos-signing.md](macos-signing.md) — `FluxVMUSBAccess.app` entitlement
- [vgpu.md](vgpu.md) / [sandbox-gpus.md](sandbox-gpus.md) — VFIO bind,
  migration and balloon constraints to reuse
- [NATIVE design (in-tree VMM)](../crates/fluxvm-hypervisor/DESIGN.md) —
  USB is an explicit non-goal of that engine
- [NEXT-FEATURES.md](NEXT-FEATURES.md) — cross-area backlog pointer
- Kiri parity roadmap (branch `feat/kiri-parity-roadmap`,
  `docs/KIRI-PARITY.md`) — P3 USB passthrough
