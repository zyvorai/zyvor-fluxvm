---
page_title: "fluxvm_network Resource - FluxVM"
subcategory: ""
description: |-
  A private network spanning Macs.
---

# fluxvm_network (Resource)

A private network across Macs (an overlay): one /24, a UDP port and an encryption key, with a slice of host addresses
per member Mac. VMs join it through `fluxvm_vm.networks`. Members are added and removed in place. The key is never
exposed; central rotates it automatically (policy `overlay_key_rotation_days`).

## Example Usage

```terraform
resource "fluxvm_network" "build" {
  name  = "build"
  nodes = [fluxvm_lease.a.node, fluxvm_lease.b.node]
}
```

## Argument Reference

- `name` (String, Required) 1-40 of `a-z`, `0-9` and `-`. Replaces when changed.
- `nodes` (Set of String) Member Macs. Unset: every Mac the tenant leases when the network is created. Changes in place.
- `hosts_per_node` (Number) Addresses per member Mac (default 30). Replaces.
- `mtu` (Number) Default 1400. Replaces.
- `tenant` (String) Operator only: the owning tenant.

## Attribute Reference

- `id` The name. `subnet` (e.g. `10.89.3.0/24`), `port`, `host_ranges` (Mac to `first-last`), `key_epoch`,
  `created_at`.

## Import

```shell
terraform import fluxvm_network.build build
```
