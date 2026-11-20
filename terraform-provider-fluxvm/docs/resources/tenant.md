---
page_title: "fluxvm_tenant Resource - FluxVM"
subcategory: ""
description: |-
  A tenant of the Mac cloud (operator only).
---

# fluxvm_tenant (Resource)

A tenant with its quotas. Needs the **fleet (operator) token**. The tenant's primary token is in `token`, known only
to the run that created the tenant: hand it over from there (or rotate it with `fluxctl fleet tenant rotate-token`).
Central refuses to delete a tenant that still has active leases or VMs.

## Example Usage

```terraform
resource "fluxvm_tenant" "acme" {
  id               = "acme"
  display_name     = "Acme Mobile"
  max_leased_hosts = 2
  max_vms          = 20
  max_memory_mib   = 98304
}
```

## Argument Reference

- `id` (String, Required) 1-63 of `a-z`, `0-9` and `-`, starting with a letter or digit; not `me`. Replaces.
- `display_name` (String)
- `max_vms`, `max_vcpus`, `max_memory_mib`, `max_leased_hosts` (Number) Quotas; unset is unlimited. A breach is a 429.
- `disabled` (Boolean) A disabled tenant's token and keys are refused.

## Attribute Reference

- `token` (Sensitive), `monthly_budget` (the tenant sets its own), `created_at`.

## Import

```shell
terraform import fluxvm_tenant.acme acme
```
