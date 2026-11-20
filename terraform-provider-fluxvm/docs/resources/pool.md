---
page_title: "fluxvm_pool Resource - FluxVM"
subcategory: ""
description: |-
  A warm pool of booted VMs. Requires central with pools.
---

# fluxvm_pool (Resource)

Central keeps `min_ready` VMs booted from `image` and unclaimed, so a launch is instant. `min_ready`, `max_total` and
`nodes` change in place (`PATCH /fleet/pools/{name}`); a new name, image or size replaces the pool, because its warm
VMs are already booted.

**Requires central with pools** (`POST/GET /fleet/pools`, `GET/PATCH/DELETE /fleet/pools/{name}`).

## Example Usage

```terraform
resource "fluxvm_pool" "ci" {
  name      = "ci"
  image     = "macos-27-xcode"
  min_ready = 2
  max_total = 4
  nodes     = [fluxvm_lease.ci.node]
}
```

## Argument Reference

- `name` (String, Required) 1-40 of `a-z`, `0-9` and `-`, starting and ending with a letter or digit. Replaces.
- `image` (String, Required) A catalog image name. Replaces.
- `min_ready` (Number, Required) VMs kept booted and unclaimed.
- `vcpus`, `memory_mib` (Number) Default: the catalog entry's. Replaces.
- `max_total` (Number) Most VMs the pool holds, claimed or not (at least `min_ready`).
- `nodes` (List of String) Macs the pool may use.

Removing `vcpus`, `memory_mib`, `max_total` or `nodes` from the configuration leaves central's value as it is (the
API's PATCH has no "unset").

## Attribute Reference

- `id` The name. `tenant`; `ready` (booted, unclaimed VMs) and `total` (every VM of the pool alive: ready, booting and
  claimed ones still running), from central's `status`.

## Import

```shell
terraform import fluxvm_pool.ci ci
```
