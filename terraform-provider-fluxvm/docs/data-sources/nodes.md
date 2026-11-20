---
page_title: "fluxvm_nodes Data Source - FluxVM"
subcategory: ""
description: |-
  The Macs you can see.
---

# fluxvm_nodes (Data Source)

With the fleet token, every registered node (`GET /fleet/nodes`). With a tenant token, the Macs the tenant leases,
showing their size, free capacity and Apple capabilities only (no address, labels or security report).

## Example Usage

```terraform
data "fluxvm_nodes" "mine" {}

output "macs" {
  value = data.fluxvm_nodes.mine.names
}
```

## Attribute Reference

- `names` (List of String)
- `nodes` (List of Object) `name`, `healthy`, `cordoned`, `vcpus_total`, `memory_mib_total`, `free_vcpus`,
  `free_memory_mib`, `vm_count`, `is_mac`, `nested_virtualization`, `labels` (operator only).
