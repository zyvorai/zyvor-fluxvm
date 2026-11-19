# Instancetypes and preferences

Modelled on KubeVirt's [instancetypes and preferences](https://kubevirt.io/user-guide/user_workloads/instancetypes/).
Instead of repeating `vcpus`, `memory_mib` and device defaults in every VM spec, name them once and reference them at create time.

* An **instancetype** is a named, **versioned** size class: vCPUs, memory, and optionally hotplug ceilings, hugepages, a CPU model and
  network/block limits.
* A **preference** is a named bundle of device and firmware defaults: disk bus, NIC model, Secure Boot, TPM, firmware path.

## Using them

```sh
fluxctl instancetype ls
fluxctl create --spec web.json --instancetype large --preference linux
```

or in the create body (`POST /v1/vms`, `fluxctl create --spec`, `fluxctl vm-spec check`):

```json
{"name": "web-1", "backend": "qemu", "image": "/var/lib/fluxvm/images/debian.qcow2",
 "instancetype": "large:1", "preference": "linux"}
```

`instancetype` is `name` (the latest version) or `name:version`. `preference` is a name.

### Resolution rules

| | instancetype | preference |
|---|---|---|
| Role | **owns** the fields it defines | supplies **defaults** |
| Spec sets the same field | **400**, naming every clash | the spec wins; recorded as skipped |

`{"instancetype": "large", "vcpus": 2}` fails with ``spec sets `vcpus` but instancetype large:1 defines it; remove it from the spec, or drop
`instancetype` `` - as in KubeVirt, a size class and a hand-set size are two answers to one question. Everything else in the spec (image,
network, cloud-init, ...) is untouched.

### Snapshot semantics

Resolution writes the values into the VM's own spec, so **later edits or deletes of a class never change an existing VM**, and a restart
or recovery uses the same numbers. What was resolved is also kept with the VM:

* `GET /v1/vms/{id}/instancetype` (the file `<workspace>/instancetype.json`): the full class (name, version, spec) as it was, the spec keys that were
  `applied`, and the settings `skipped` with the reason.
* labels `fluxvm.io/instancetype=large:1` and `fluxvm.io/preference=linux`, so `fluxctl list -l fluxvm.io/instancetype=large:1` finds them.
* the create response carries the same snapshot as `instancetype_snapshot`.

## What each field does

Instancetype `spec`:

| field | VM spec key | note |
|---|---|---|
| `vcpus`, `memory_mib` | `vcpus`, `memory_mib` | required |
| `max_vcpus`, `max_memory_mib` | same | hotplug ceilings |
| `hugepages` | `hugepages` | |
| `cpu_model` | `cpu_template` | Firecracker only; other backends have no per-VM CPU model, so it is recorded as skipped |
| `limits.net_mbit_limit`, `net_pps_limit`, `blk_mbit_limit`, `blk_ops_limit` | same | |

Preference `spec`:

| field | VM spec key | note |
|---|---|---|
| `disk_bus` | `root_disk_io.bus` | only `virtio-blk` applies to the root disk (it is always virtio-blk); `scsi` and `vhost-user-blk` are recorded as skipped |
| `nic_model` | - | **not applied yet**: the VM spec has no NIC model setting; recorded as skipped |
| `secure_boot`, `tpm`, `firmware` | same | |

## Versions and built-ins

Classes are stored in `<state_dir>/instancetypes.json` and `<state_dir>/preferences.json`, next to `vm-templates.json`.
`fluxctl instancetype create web --vcpus 2 --memory-mib 4096` stores `web:1`; creating `web` again stores `web:2`, which an unversioned reference then
picks. `--version N --replace` overwrites version N.

Built in (version 1): `small` (1 vCPU, 2 GiB), `medium` (2, 4 GiB), `large` (4, 8 GiB), `memory-optimised` (4, 32 GiB); preferences `linux`
(virtio disk and NIC) and `windows` (virtio disk, e1000e NIC, Secure Boot, TPM). Built-ins cannot be deleted but can be **overridden**: store a class
of the same name and version with `--replace`, or a new version of the name to make it the default for unversioned references.

## CLI and REST

```
fluxctl instancetype ls | get NAME[:VER] | create NAME --vcpus N --memory-mib M [--version V] [--max-vcpus ..] [--hugepages true] [--cpu-model ..] [--net-mbit-limit ..] [--replace] | rm NAME[:VER]
fluxctl preference   ls | get NAME      | create NAME [--disk-bus virtio-blk] [--nic-model ..] [--secure-boot true] [--tpm true] [--firmware PATH] [--replace] | rm NAME
```

Against a daemon (`--server`, a context) these call the REST routes; otherwise they edit the local `state_dir`, like `vm-template`.

```
GET    /v1/instancetypes              GET    /v1/preferences
POST   /v1/instancetypes              POST   /v1/preferences
GET    /v1/instancetypes/{name}       GET    /v1/preferences/{name}      # name or name:version
DELETE /v1/instancetypes/{name}       DELETE /v1/preferences/{name}      # name (all stored versions) or name:version
GET    /v1/vms/{id}/instancetype                                          # the snapshot recorded at create
```

Reads are open to every role; writes need the admin role. A missing class is 404, an invalid or conflicting one 400.

## Spec lint

`fluxctl vm-spec check` accepts the two keys. It resolves them against the **built-in** classes (a class that exists only on the daemon's host is
reported as `info`: it is checked at create time), reports clashes as errors, and lints the resolved spec.

## Limits

* The selectors are handled in the create paths that take a full spec (`POST /v1/vms`, `fluxctl create`); `vm-template instantiate` and sandboxes do not
  resolve them.
* Resolution is by the raw body: a key set to `null` counts as unset.
