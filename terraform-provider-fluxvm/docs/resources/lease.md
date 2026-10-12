---
page_title: "fluxvm_lease Resource - FluxVM"
subcategory: ""
description: |-
  A whole Mac leased for at least 24 hours.
---

# fluxvm_lease (Resource)

A whole Mac, leased by one tenant for at least 24 hours (the macOS licence's minimum). Destroying it releases the
lease: central deletes the tenant's VMs on that Mac and marks the Mac for scrubbing before anyone else gets it.

Central refuses a release before `min_end`. Plan your destroys accordingly, or (operator) set `force_release`, which
still bills to the 24-hour minimum. A lease whose `hours` ran out is removed from state on the next refresh, and
Terraform plans a new one.

## Example Usage

```terraform
resource "fluxvm_lease" "ci" {
  hours                 = 72
  min_memory_mib        = 65536
  nested_virtualization = true
}

# Operator: lease a named Mac for a tenant.
resource "fluxvm_lease" "acme_studio" {
  tenant = "acme"
  node   = "studio-1"
}
```

## Argument Reference

Every argument except `force_release` replaces the lease when changed.

- `tenant` (String) Operator only: the tenant to lease for. A tenant token always leases for itself.
- `node` (String) A specific Mac; otherwise central picks the smallest eligible Mac (healthy, not cordoned, not leased,
  scrubbed, empty, large enough).
- `node_selector` (Map of String) Only Macs whose labels match.
- `hours` (Number) Lease length, at least 24. Unset runs until destroyed.
- `min_vcpus`, `min_memory_mib` (Number) Smallest acceptable Mac.
- `nested_virtualization` (Boolean) Only Macs with nested virtualization (M3 or later).
- `force_release` (Boolean) Operator only: release on destroy even before the 24-hour minimum.

## Attribute Reference

- `id` The lease id.
- `state` `active` (a released lease leaves the state).
- `started_at`, `min_end` (the earliest release), `ends_at`.
- `price_class` The Mac's price class when the lease started.

## Import

```shell
terraform import fluxvm_lease.ci lease-3f2a...
```
