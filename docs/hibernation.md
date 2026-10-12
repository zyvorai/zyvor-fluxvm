# Hibernation

Hibernation saves a running QEMU VM's RAM, vCPU and device state to a directory, stops QEMU and frees the VM's CPU
and memory. The next `start` loads the state back and the guest carries on from where it was paused: same boot,
same processes, same `/dev/shm`.

```sh
curl -XPOST localhost:7788/v1/vms/$ID/hibernate      # -> VmRecord, status "stopped", hibernation {dir, manifest}
curl -XPOST localhost:7788/v1/vms/$ID/start          # resumes from the saved state
curl -XDELETE localhost:7788/v1/vms/$ID/hibernation  # drop the state; the next start boots from disk
fluxctl hibernate $ID
```

## What happens

1. QMP `stop` pauses the vCPUs. The guest is not asked to do anything, so no guest agent and no `fsfreeze` is needed:
   nothing in the guest runs while its state is written.
2. QMP `migrate` to `file:<dir>/state.bin` writes the migration stream (RAM, CPU, every device) to the file.
3. `manifest.json`, `record.json` (the VM record), `ovmf_vars.fd` and the cloud-init `seed.img` are written next to it.
4. QMP `quit`, then the normal stop teardown (tap, netns, cgroup, shared-disk claim). The record is `stopped` with
   `hibernation` set.

`start` on a hibernated VM launches QEMU with the VM's own command line plus `-incoming defer`, runs
`migrate-incoming file:<dir>/state.bin`, waits for the load and sends `cont`. The state directory is removed once the
guest runs, because the disk has moved on and the state must never be loaded again. A failed resume leaves the VM
`stopped` with the state in place, so `start` can be retried.

The root disk is not snapshotted. It is consistent with the saved RAM as long as nothing writes to it while the VM is
hibernated.

## The state directory

`hibernation_dir` in `/etc/fluxvm.toml` (default `<state_dir>/hibernation`) holds one directory per VM:

```
<hibernation_dir>/<vm-id>/
  manifest.json   format, node, QEMU version, q35 machine type, CPU vendor and flags, vCPUs, memory, state size
  state.bin       the QEMU migration stream (about the guest's touched RAM)
  record.json     the VM record, for import on another node
  ovmf_vars.fd    UEFI variables, when the VM uses UEFI
  seed.img        cloud-init seed
  imported-by     written by the node that imported the state
```

Deleting a hibernated VM removes its directory, unless `imported-by` names another host.

## Resuming on another node

Put `hibernation_dir` on storage every node mounts at the same path, and keep root disks on shared storage
(`storage: shared` or `ceph-rbd-in-place`). Then on the target node:

```sh
curl -XPOST localhost:7788/v1/vms/import-hibernated -d '{"id":"'$ID'"}'  # registers the VM, stopped and hibernated
curl -XPOST localhost:7788/v1/vms/$ID/start
```

Import keeps the VM id and vsock CID (refused if the CID is taken here), copies `ovmf_vars.fd` and the seed into a
new workspace, checks host quota and refuses when a VM with that id already exists. Delete the record on the original
node afterwards; its delete leaves the shared state alone.

A saved state only loads into the same QEMU device model on a compatible CPU. Before any resume (and on import) the
manifest is checked against this host:

| Check | Rule |
|---|---|
| Format | `format` is 1 |
| QEMU | same version (`qemu-system-x86_64 --version`) |
| Machine | `q35` resolves to the same versioned type, e.g. `pc-q35-10.2` |
| CPU vendor | same `vendor_id` |
| CPU flags | this host has every flag the source had (guests run with `-cpu host`) |

`GET /v1/host/hibernation` returns these facts for this host; `POST /v1/host/hibernation/check` with
`{"manifest": ...}` answers `{compatible, reasons}`. Capability `host.hibernate`.

## Limits

- QEMU backend only.
- Refused: vTPM, VFIO devices, virtiofs shares, confidential VMs, and VMs with hot-plugged CPUs, memory or NICs since
  they started (restart them first).
- Import to another node additionally needs shared root storage, no data disks, no attached install media, and no
  direct-datapath or macvtap NIC.
- Saving takes about as long as writing the guest's touched RAM to the state directory; a 1 GiB Ubuntu guest wrote
  467 MB in about 11 s and resumed in about 1 s on local disk.
- The guest clock jumps on resume, like after a laptop lid close. Guests that care should run an NTP client.
