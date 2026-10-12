# Warm pools: VMs that start in seconds

A macOS guest takes a minute or more to boot. For CI that is a minute per job, every job. A **warm pool** keeps VMs
of one catalog image booted and idle, so a launch takes one that is already running: central renames it, hands it
over and answers at once with `"warm": true`, then boots a replacement in the background.

```console
$ fluxctl fleet pool-create ios-ci --image macos-27-xcode --min-ready 2
✓ Created warm pool ios-ci: keeps 2 of macos-27-xcode booted (6 vCPUs, 16 GiB each).
$ fluxctl fleet launch build-42 --image macos-27-xcode --ttl 2h
✓ Launched build-42 on studio-1 (macOS 27 with Xcode, 6 vCPUs, 16 GiB)
  Started instantly from warm pool ios-ci (0.9s)
```

## Two kinds of pool

Apple's macOS licence allows two macOS guests per Mac, and a leased Mac is exclusive to its tenant. So a warm macOS VM
on a leased Mac can only ever belong to that tenant. Pools come in two kinds:

| | Tenant pool | Shared pool |
|---|---|---|
| Created by | the tenant (or the operator with `"tenant"`) | the operator |
| Runs on | Macs the tenant leases | Macs nobody leases |
| Who can take a VM | the tenant | the operator, and tenants where policy allows (below) |
| Quota | warm VMs count against the tenant's quota | not in anyone's quota until taken |
| Billing | billed to the tenant like any of its VMs (free on a leased Mac unless prices say otherwise) | billed to whoever takes a VM, from the moment it is taken |

A tenant can take VMs from a shared pool only where its VMs could run on a shared host anyway: not for macOS guests
while `require_lease_for_macos` is on (the default), and not at all while `isolate_tenant_networks` is on, because a
warm VM was booted on no tenant's network. Such shared pools are hidden from tenants; creating one returns a `note`
saying so. For macOS CI, give each tenant its own pool.

## What takes a warm VM

A `POST /fleet/vms` is answered from a pool when:

- it names one: `"pool": "ios-ci"` (image and size may then be left out), or
- its `image` (catalog path or name) and, where given, `vcpus`, `memory_mib`, `apple.guest_os` and `disk_size_gib` match
  a pool the caller may use (its own pools first, then shared ones),

and it asks for nothing a booted VM cannot have. A warm VM has already booted, so `cloud_init`, networks
(`apple.networks`, vmnet, bridges), extra disks, a non-standard `security_profile` or `"reschedulable": true` all mean a
normal boot. `name`, `ttl_seconds`, `node`, `nodeSelector` and (for the operator) `tenant` are fine.

The oldest ready VM wins. The pick and the hand-over happen in one step under central's tenancy lock, so two
launches at the same moment never get the same VM; the losers boot normally. A launch that names a pool with nothing
ready boots a VM from the pool's image and size the usual way (`"warm"` is absent from the answer). If the node
cannot rename the VM, central deletes it and boots one the usual way.

When a VM is taken:

- it is renamed to the launch's `name` (`PATCH /v1/vms/{id}` on its node) and labelled `fluxvm.dev/warm-claimed=<pool>`
  instead of `fluxvm.dev/warm-pool=<pool>`;
- a shared pool's VM becomes the tenant's (checked against its quota first; a quota refusal is the launch's answer) and
  its metering and billing start now (`billed_from` in the ledger);
- `ttl_seconds` is applied by central, since the node booted the VM without one: the pool reconciler deletes it when
  it runs out, and the VM gets `fluxvm.dev/ephemeral=true` so a drain may delete it;
- the activity log shows the launch as usual (`POST /fleet/vms`, the VM's name), and the pool records the claim for its
  statistics.

## Keeping pools full

A reconciler in central (only on the HA leader) runs every 5 seconds (`FLUXVM_WARM_POOL_INTERVAL_SECS`, 1-3600) when
central runs its background reconciler (`--reconcile-secs` above 0), and at once after a claim or a pool change. Each
pass:

- checks each warm VM on its node: running ones are *ready* (only those are handed out); still booting after 15
  minutes, stopped, failed, or gone from the node, it is replaced;
- deletes warm VMs on a node the pool may no longer use: the node was leased (shared pool) or the lease ended (tenant
  pool), it is draining or waiting for a scrub, or the pool's `nodes`/`node_selector` changed;
- trims pools above `min_ready` (booting VMs first, then the newest);
- boots VMs for pools below it, at most 4 per pool per pass, on the best node the pool may use: leases and scrub
  respected, two macOS guests per Mac counted from the ledger, `node_selector` and `nodes` honoured;
- deletes claimed VMs whose TTL ran out.

`max_total` bounds what a burst costs: warm VMs plus VMs taken from the pool and still running. At the limit the pool
stops refilling until some of them are deleted. The default is 4 × `min_ready` (at least `min_ready` + 4).

After a failed boot (no room, the node refused) the pool waits 5 s, then 10 s, 20 s, … up to 10 minutes before the next
try, and shows the error. Changing the pool retries at once. The first failure in a row is written to the activity log
(actor `warm-pool`), as is each replacement.

Warm VMs are named `warm-<pool>-<8 hex>` and carry `fluxvm.dev/warm-pool=<pool>` and `fluxvm.dev/ephemeral=true` on
their node.

## API

| Route | Who | |
|---|---|---|
| `GET /fleet/pools` | tenant, operator | own pools; a tenant also sees shared pools it may use (without node details) |
| `POST /fleet/pools` | tenant, operator | create (below); `201` with the pool |
| `GET /fleet/pools/{name}` | as above | the pool and its `status` |
| `GET /fleet/pools/{name}/status` | as above | `ready`, `booting`, `claimed_24h`, `avg_claim_ms`, `last_error`, … |
| `PATCH /fleet/pools/{name}` | owner, operator | `min_ready` (0 pauses), `max_total`, `node_selector`, `nodes` |
| `DELETE /fleet/pools/{name}` | owner, operator | removes the pool and deletes its warm VMs; taken VMs keep running |

```json
POST /fleet/pools
{"name": "ios-ci", "image": "macos-27-xcode", "min_ready": 2,
 "max_total": 8, "vcpus": 6, "memory_mib": 16384,
 "node_selector": {"zone": "lab"}, "nodes": ["studio-1"], "tenant": "acme"}
```

Only `name` (1-40 of `a-z 0-9 -`) and `image` (a catalog name) are required; size defaults to the image's, `min_ready` to
1. A tenant's pool is always its own, and may name only Macs it leases. A pool's image and size cannot change: create
a new pool. Limits: 64 pools, 8 per tenant, `min_ready` up to 32, `max_total` up to 100.

```json
GET /fleet/pools/ios-ci
{"name": "ios-ci", "owner": "acme", "tenant": "acme", "image": "macos-27-xcode", "guest_os": "macos",
 "size": {"vcpus": 6, "memory_mib": 16384}, "min_ready": 2, "max_total": 8,
 "status": {"state": "ready", "ready": 2, "booting": 0, "claimed_running": 3, "claimed_24h": 14,
            "avg_claim_ms": 910, "last_claim_at": "…", "last_fill_at": "…", "last_error": null, "backoff_until": null},
 "warm": [{"node": "studio-1", "id": "…", "name": "warm-ios-ci-1a2b3c4d", "state": "ready", "since": "…"}]}
```

`state` is `ready` (at `min_ready`), `filling`, `backing_off` or `paused` (`min_ready` 0). `GET /fleet/vms` marks warm
VMs with `warm_pool` and `warm_state` (`ready`/`booting`) and taken ones with `claimed_from` and `claimed_at`.

A launch answered from a pool:

```json
POST /fleet/vms  {"name": "build-42", "pool": "ios-ci", "ttl_seconds": 7200}
→ {"node": "studio-1", "vm": {…, "name": "build-42"}, "warm": true, "pool": "ios-ci", "claim_ms": 870}
```

## Command line

```console
fluxctl fleet pools                                   # ready/booting, claims in 24 h, average start
fluxctl fleet pool-create ios-ci --image macos-27-xcode --min-ready 2 [--max 8] [--vcpus 6 --memory 16G]
                                 [--tenant acme] [--node-selector zone=lab] [--node studio-1]
fluxctl fleet pool-set ios-ci --min-ready 4           # or --max N; --min-ready 0 pauses it
fluxctl fleet pool-rm ios-ci                          # asks first on a terminal
```

`fluxctl fleet launch --image <name>` takes a warm VM whenever one matches and says so.

## Console

**Warm pools** shows a card per pool: image and size, a dot per VM (green ready, amber booting, dashed not booted
yet), a stepper for how many to keep ready, launches in the last day and the average start time, and the last error
with when it retries. **Create pool** picks an image (and, for the operator, the owner) and warns about the macOS
licence; **Launch** on a card starts a VM from the pool; **Delete** asks first. Warm VMs are tagged *Warm, ready* or
*Warm, booting* in Virtual machines, and a launch answered from a pool says "Started instantly from warm pool …".

## GitHub Actions runners

`fluxvm-agent gha-runners` ([github-actions-runners.md](github-actions-runners.md)) creates one VM per queued job. To
take those VMs from a warm pool, name the pool in the runner pool's `vm` body and start the runner through the guest
agent:

```json
{"name": "macos", "labels": ["self-hosted", "macOS", "fluxvm"], "launch": "exec",
 "vm": {"pool": "ios-ci"}, "max_runners": 2}
```

Each job's VM is then a booted VM renamed `fluxvm-gha-…`, and the runner starts as soon as the guest agent answers.
The scaler still sets `ttl_seconds` as a safety net (central applies it). A warm VM cannot run cloud-init, so the config
is refused with `launch: cloud-init` or a `cloud_init` in the `vm` body. Use the runner tenant's own pool (its API key
is the fleet token), and keep `min_ready` at about the number of jobs that usually start together.

## Limits

- A warm VM is booted once and handed over as it is: whatever the image does at first boot has happened, with no
  per-launch cloud-init, networks or disks.
- Readiness is "the node says running", not "the guest agent answers": a claim can land a moment before the guest is
  fully up (`exec` retries, as the GitHub runner scaler does).
- The TTL of a taken VM is enforced by central's reconciler, not the node, so it runs only while central does.
- `PATCH /v1/vms/{id}` on the node is used for the rename; a node agent too old to have it makes every claim fall back
  to a normal boot (and the warm VM is deleted).
