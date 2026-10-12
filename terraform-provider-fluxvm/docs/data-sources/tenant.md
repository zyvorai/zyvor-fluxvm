---
page_title: "fluxvm_tenant Data Source - FluxVM"
subcategory: ""
description: |-
  A tenant's quotas and current use.
---

# fluxvm_tenant (Data Source)

Without `id`, the calling tenant (`GET /fleet/tenants/me`). With `id`, that tenant (fleet token).

## Example Usage

```terraform
data "fluxvm_tenant" "me" {}

output "vms_left" {
  value = data.fluxvm_tenant.me.max_vms - data.fluxvm_tenant.me.vms
}
```

## Attribute Reference

- `id`, `display_name`, `disabled`, `created_at`.
- Quotas: `max_vms`, `max_vcpus`, `max_memory_mib`, `max_leased_hosts` (null is unlimited).
- In use now: `vms`, `vcpus`, `memory_mib`, `leased_hosts`, `leased_macs`.
- `monthly_budget`.
