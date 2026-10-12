---
page_title: "fluxvm_webhook Resource - FluxVM"
subcategory: ""
description: |-
  A URL that receives signed notifications.
---

# fluxvm_webhook (Resource)

A URL that receives central's notifications as JSON, signed with `X-FluxVM-Signature: sha256=<hex>` (HMAC-SHA256 of
the body with `secret`, the same scheme as GitHub's). A tenant's webhook gets that tenant's events and must use
`https` to a public address; the operator's gets every event. Changing anything replaces the webhook (and makes a
new secret).

## Example Usage

```terraform
resource "fluxvm_webhook" "alerts" {
  url    = "https://hooks.example.com/fluxvm"
  events = ["lease.ending", "budget.over"]
}

output "webhook_secret" {
  value     = fluxvm_webhook.alerts.secret
  sensitive = true
}
```

## Argument Reference

- `url` (String, Required)
- `events` (Set of String) `vm.created`, `vm.deleted`, `lease.created`, `lease.released`, `lease.ending`,
  `lease.ended`, `budget.near`, `budget.over`, and for the operator `node.down`, `node.scrub`, `drain.done`. Unset
  sends everything.

## Attribute Reference

- `id` `whk_…`. `secret` (Sensitive; only known to the run that created it), `owner`, `created_at`.

## Import

```shell
terraform import fluxvm_webhook.alerts whk_51a0c2e9b3f1
```

The secret is not shown again, so it is null after an import.
