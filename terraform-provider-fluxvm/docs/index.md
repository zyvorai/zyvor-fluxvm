---
page_title: "Provider: FluxVM"
description: |-
  Manage a FluxVM Mac cloud (fluxvm-agent central): Mac leases, VMs, private networks, volumes, warm pools,
  API keys, webhooks and tenants.
---

# FluxVM Provider

The FluxVM provider manages a [FluxVM](https://github.com/zyvorai/zyvor-fluxvm) Mac cloud through central's fleet
API (`/fleet/*`). A team scripts its Macs the same way it scripts the rest of its infrastructure: lease a Mac, launch
macOS VMs from the image catalog, join them to a private network, attach volumes, and get webhooks before a lease
ends.

Which resources you can use depends on the token:

| Token | Can manage |
|---|---|
| A tenant's primary token | everything the tenant owns: leases, VMs, networks, volumes, pools, webhooks, API keys |
| A tenant's full-access API key | the same, except API keys (only the primary token makes keys) |
| A read-only API key | data sources only |
| The fleet (operator) token | tenants, and any of the above on a tenant's behalf (`tenant` arguments) |

## Example

```terraform
provider "fluxvm" {
  endpoint = "https://fleet.example.com"   # or FLUXVM_ENDPOINT
  # token = var.fluxvm_token               # or FLUXVM_TOKEN
}

resource "fluxvm_lease" "ci" {
  hours = 72
}

resource "fluxvm_vm" "runner" {
  name          = "ci-runner-1"
  catalog_image = "macos-27-xcode"
  node          = fluxvm_lease.ci.node
}
```

A complete example (lease, network, volume, VM, webhook and API key) is in
[`examples/main.tf`](https://github.com/zyvorai/zyvor-fluxvm/tree/main/terraform-provider-fluxvm/examples/main.tf).

## Things to know

- **Leases are at least 24 hours.** The macOS licence requires a lessee to have sole use of a Mac for 24 consecutive
  hours, so central refuses to release a lease earlier. `terraform destroy` within 24 hours of creating a
  `fluxvm_lease` fails with central's message and the earliest release time; the operator can set `force_release`.
- **macOS VMs run on Macs you lease**, two per Mac. Give a `fluxvm_vm` a `node = fluxvm_lease.x.node` (or a
  `depends_on`) so the lease exists first.
- **VMs are replaced, not edited.** Virtualization.framework cannot resize or re-image a VM, so changing anything but
  `power_state` plans a replacement.
- **Secrets are shown once.** Tokens (`fluxvm_tenant.token`, `fluxvm_api_key.token`) and webhook secrets are known
  only to the run that created them and are null after an import. Keep state encrypted.
- `fluxvm_volume` and `fluxvm_pool` need a central with volumes and warm pools.
- Calls that only read (GET), and PUT/DELETE, are retried on 429 and 5xx with backoff; creates are not.

## Schema

### Optional

- `endpoint` (String) Central's URL, e.g. `https://fleet.example.com`. Defaults to `FLUXVM_ENDPOINT`.
- `token` (String, Sensitive) A tenant token, an API key or the fleet token. Defaults to `FLUXVM_TOKEN`.
- `retries` (Number) How often an idempotent call is retried after a 429 or 5xx (default 3).
