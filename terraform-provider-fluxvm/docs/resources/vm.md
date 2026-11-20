---
page_title: "fluxvm_vm Resource - FluxVM"
subcategory: ""
description: |-
  A VM placed by central, from the image catalog or a disk path.
---

# fluxvm_vm (Resource)

A VM that central places on a Mac (or a Linux host) and records against the tenant's quota. macOS VMs run only on
Macs the tenant leases, two per Mac.

Virtualization.framework cannot resize, re-image or move a running VM, so changing what the VM *is* plans a
replacement. Only `power_state` changes in place.

## Example Usage

```terraform
resource "fluxvm_vm" "runner" {
  name          = "ci-runner-1"
  catalog_image = "macos-27-xcode"
  node          = fluxvm_lease.ci.node
  networks      = [fluxvm_network.build.name]
  volumes       = [fluxvm_volume.cache.name]
  extra_json    = jsonencode({ labels = { team = "ios" } })
}

resource "fluxvm_vm" "linux" {
  name       = "builder"
  image      = "/var/lib/fluxvm/images/ubuntu-24.04.raw"
  guest_os   = "linux"
  vcpus      = 4
  memory_mib = 8192
  ttl_seconds = 28800
}
```

## Argument Reference

- `name` (String, Required) The VM's name.
- `catalog_image` (String) A catalog entry (see `fluxvm_images`). Fills `image`, `guest_os`, `vcpus`, `memory_mib`
  and `disk_size_gib` unless set.
- `image` (String) The disk's path on the Mac. Set this or `catalog_image`.
- `guest_os` (String) `macos` or `linux`. Required with `image`.
- `vcpus`, `memory_mib`, `disk_size_gib` (Number) Size.
- `ttl_seconds` (Number) The node deletes the VM after this long; Terraform then plans a new one.
- `node` (String) Pin to a Mac; otherwise central places it.
- `node_selector` (Map of String) Place only on Macs with these labels.
- `networks` (List of String) `fluxvm_network` names to join.
- `volumes` (List of String) `fluxvm_volume` names to attach (requires central with volumes).
- `reschedulable` (Boolean) Central keeps the spec so a drain can recreate the VM elsewhere. Leave it off when
  `extra_json` carries secrets.
- `tenant` (String) Operator only: create for this tenant.
- `extra_json` (String) A JSON object of further create-request fields (`cloud_init`, `agent`, `labels`, more `apple`
  options). Named arguments win.
- `power_state` (String) `running`, `stopped` or `paused`; changes in place (start, stop, pause, resume).

Everything except `power_state` replaces the VM when changed.

## Attribute Reference

- `id` `<node>/<vm_id>`.
- `vm_id` The VM's UUID on its node.
- `status` What the node reports: `creating`, `running`, `paused`, `stopped` or `failed`.
- `created_at`.

If the VM's Mac does not answer during a refresh, the provider keeps the last known state (with a warning) rather
than planning a duplicate. A VM that is gone (deleted on the node, TTL) leaves the state.

## Import

```shell
terraform import fluxvm_vm.runner mac-studio-1/0b9c6f1e-...
```

`catalog_image`, `extra_json`, `ttl_seconds` and the other create-only arguments are not stored by central; after an
import write them in the configuration as they were, or Terraform plans a replacement.
