---
page_title: "fluxvm_api_key Resource - FluxVM"
subcategory: ""
description: |-
  A tenant's named API key.
---

# fluxvm_api_key (Resource)

A named API key: one per person, CI system or tool, so the activity log says which one did what and each can be
revoked alone. Keys are created with the tenant's **primary token** (a key cannot mint keys) or by the operator.
Destroying the resource revokes the key at once. Every argument replaces the key.

## Example Usage

```terraform
resource "fluxvm_api_key" "github" {
  name            = "github-actions"
  expires_in_days = 90
}

resource "fluxvm_api_key" "grafana" {
  name      = "grafana"
  read_only = true
}
```

## Argument Reference

- `name` (String, Required) Unique within the tenant.
- `tenant` (String) `me` (default: the calling tenant) or, with the fleet token, a tenant id.
- `read_only` (Boolean) A read-only key can only GET.
- `expires_in_days` (Number) 1 to 3650; unset never expires.

## Attribute Reference

- `id` `key_…`. `token` (Sensitive; only known to the run that created it), `created_at`, `expires_at`,
  `last_used_at`.

## Import

```shell
terraform import fluxvm_api_key.github key_0a1b2c3d4e5f          # the calling tenant's key
terraform import fluxvm_api_key.github acme/key_0a1b2c3d4e5f     # operator
```
