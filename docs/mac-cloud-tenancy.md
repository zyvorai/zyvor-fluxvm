# Mac cloud: tenants, leases, drain and metering

`fluxvm-agent central` can run a shared fleet of Macs as a cloud for several teams or customers. This page covers the
tenancy layer: tenants and their tokens, Mac host leases, quotas, scrubbing, draining and usage metering. For the fleet
itself (registry, node heartbeats, placement) start with [mac-cloud.md](mac-cloud.md).

**Status:** implemented with unit and in-process HTTP tests. Not yet run against real Macs.

## Why leases

Apple's macOS licence limits macOS virtualisation on a Mac to two extra copies. It also allows a Mac to be leased only
when one lessee has sole use of it for at least 24 consecutive hours. AWS's 24-hour minimum for EC2 Mac hosts comes from
the same rule. A cloud that hands out macOS VMs per minute on a shared Mac does not fit that, so the unit a tenant rents
here is a **whole Mac, for at least 24 hours**. Inside its lease a tenant can create and delete as many short-lived VMs
as it likes, for example one per CI job. Linux VMs have no such restriction and can still run on shared, unleased nodes.

This is an engineering control, not legal advice. The rules are defaults in `GET/PUT /fleet/policy`:

| Policy field | Default | Effect |
|---|---|---|
| `require_lease_for_macos` | `true` | A tenant's macOS guests run only on Macs that tenant leases |
| `exclusive_leased_hosts` | `true` | A leased Mac runs only its lessee's VMs |
| `min_lease_hours` | `24` | Values below 24 are raised to 24 |
| `ledger_retention_days` | `400` | Ended VMs and leases older than this are dropped by the reconciler (`0` keeps all) |
| `isolate_tenant_networks` | `false` | Each tenant's `vz` VMs join their own vmnet network (`fluxvm-tenant-<id>`, one /24 each) |
| `tenant_subnet_prefix` | `10.230` | Tenant n gets `10.230.n.0/24` (indices are assigned once and kept) |

## Console

`http://<central>/console` is the cloud's web console. The page needs no token: it asks for one, keeps it in the tab's
session storage and sends it only to this central.

With a **tenant token** you get:
- **Overview**: a getting-started checklist (lease a Mac, launch a VM, connect VMs), quota meters, your Macs and recent VMs.
- **My Macs**: each Mac shows its two macOS slots (Apple's licence limit) and how far the lease is through its 24-hour
  minimum. Select one to launch a VM on it or release it once the minimum is over.
- **Virtual machines**: search and filter, and a launch sheet that picks an image from the catalog. It suggests a name
  and size, shows how much quota the VM takes, offers only Macs with a free macOS slot, and can join private networks
  or delete the VM automatically. Select a VM to run commands in it through the guest agent, or delete it after typing
  its name.
- **Networks**: create one across your Macs, see each Mac's address range, add or remove Macs, rotate the key.
- **Screen**: a VM's details show its display, refreshed every 2 seconds while open, so you can watch macOS boot,
  finish setup or see why a job hangs, without VNC. It comes through central from the node's screenshot API.
- **Activity**: every change made with your token (launches, deletes, leases, networks, commands) and whether it worked.
- **Images** (the catalog) and **Usage** (7, 30 or 90 days, with a CSV download).

The **fleet token** adds the operator's pages:
- every Mac, with what needs attention first (not responding, needs a scrub, finished draining);
- cordon, drain (with a choice of what happens to running VMs), scrub, and lease a Mac to a tenant;
- tenants: create with quotas, edit quotas, disable, issue a new token (shown once), delete;
- catalog editing and the policy.

There is a command palette (⌘K or Ctrl-K) to jump to any page, VM, Mac, network or tenant, and to start any action.
The console follows the system's light or dark mode unless you pick one, and it works on a phone. Links to a page or
an item can be shared (`/console#/vms/<mac>/<id>`).

## Command line

`fluxctl fleet` talks to the same API (`--central`, default `http://127.0.0.1:7799`, or `CENTRAL_URL`; `--token` or
`FLUXVM_AGENT_TOKEN`). On a terminal it prints tables, short confirmations and the next command to run. Piped, or
with `-o json`, it prints the API's JSON unchanged, so scripts keep working. `-o table` and `-o wide` force the tables
(`wide` adds IDs, labels, addresses and disk paths).

```text
$ fluxctl fleet status
Acme Mobile  tenant acme, http://fleet:7799

Macs    ████████████░░░░  2 of 3
VMs     ████████░░░░░░░░  5 of 10
vCPUs   ███████░░░░░░░░░  20 of 48
Memory  ████░░░░░░░░░░░░  48 GiB of 192 GiB

MAC       LEASE                MACOS  VMS  FREE
studio-1  releasable now       ●●     3    10 vCPUs, 156 GiB
mini-1    releasable in 19h    ●○     2    6 vCPUs, 20 GiB

Next: fluxctl fleet vms

$ fluxctl fleet launch --image macos-27-xcode --ttl 8h
✓ Launched macos-27-xcode-1 on mini-1 (macOS 27 with Xcode, 6 vCPUs, 16 GiB)
  ID  0b9c…
Next: fluxctl fleet run macos-27-xcode-1 -- sw_vers
```

| Command | What |
|---|---|
| `fleet status` | A tenant's quota, Macs and next step; for the operator, fleet health and what needs attention |
| `fleet launch [NAME] --image I` | Launch from the catalog (or `--disk PATH --guest macos`); `--mac`, `--network` (repeatable), `--ttl 8h`, `--vcpus`, `--memory-gib` |
| `fleet run VM -- CMD…` | Run a command in a VM found by name or ID prefix; prints the guest's output and exits with its code |
| `fleet screen VM [-f out.png]` | Save a picture of a VM's display |
| `fleet events [--limit N]` | The activity log (with the API key used) |
| `fleet keys [create\|revoke]` | A tenant's named API keys |
| `fleet rm VM` | Delete a VM by name (asks on a terminal; `-y` to skip) |
| `fleet vms`, `fleet nodes`, `fleet node M` | VMs and Macs, with macOS slots (`●○`) and when each lease can be released |
| `fleet lease create\|list\|get\|release` | Leases (`create --hours 72 --min-vcpus 16 --nested`) |
| `fleet images [add\|remove]` | The image catalog |
| `fleet networks [show\|create\|delete\|rotate\|add-mac\|remove-mac]` | Private networks across Macs |
| `fleet tenant create\|list\|get\|me\|update\|rotate-token\|delete` | Tenants (the token is printed once) |
| `fleet drain`, `drain-status`, `scrub`, `cordon`, `uncordon`, `usage`, `reconcile` | Operator tasks |

A refused call shows central's own message plus a hint: a 401 says which flag carries the token, a 429 points at the
quota, a standby names the leader, and an unreachable central says how to point `--central` at it.

## API keys

A tenant's primary token is the one the operator hands over. Besides it, a tenant can hold up to 50 named API keys:
one per person, CI system or tool, so the activity log says which one did what and one can be revoked without
breaking the others.

```bash
fluxctl fleet keys create ci-github                   # full access, expires in 90 days; prints the key once
fluxctl fleet keys create grafana --read-only --expires-days 0
fluxctl fleet keys                                    # name, access, last used, expiry
fluxctl fleet keys revoke ci-github
```

- A key acts as its tenant, with the same quotas and visibility. A **read-only** key can only `GET`.
- Only the primary token (or the operator, with `--tenant` / `/fleet/tenants/{id}/keys`) creates and revokes keys, so a
  leaked key cannot mint more. An expired key is refused like an unknown one.
- Revoking takes a key's id or its name (an id wins; names may not look like ids). The console revokes by id.
- Rotating the primary token (`POST /fleet/tenants/{id}/rotate-token`) does **not** revoke named keys: CI keeps
  working. If the primary token leaked, also list the tenant's keys and revoke any nobody recognises (the activity log
  shows who created each). Disabling a tenant refuses its keys too; deleting it deletes them.
- Only a key's hash is stored. `last_used_at` is updated at most once a minute and saved with the next change to the
  tenancy state.
- The console has an **API keys** page for tenants, and the operator manages keys in a tenant's details.

## Activity log

Central records every change made through it: the time, who (`operator` or the tenant), the method and path, the
name or id from the request body if there is one (a VM name, a tenant id), and the HTTP status. Reads and node
heartbeats are not recorded, a command's body is never read, and nothing else from a request is kept, so the log never
holds a VM spec, a command, a token or a key. A tenant's refused attempt at an operator route (403) is recorded too;
calls without a valid token are not. The newest 5000 events are served by `GET /fleet/events`; all of them are appended to `fleet-audit.jsonl` (mode
0600) in the state directory, which rotates to `fleet-audit.jsonl.1` at 16 MiB. A tenant sees only its own actions.
With HA, each central keeps its own log, so the log of a central that was leader for a while covers that while.

## Image catalog

The catalog lists the images the operator has put on the Macs, so tenants launch by name instead of typing disk paths.
Central does not copy images: pull each one onto every Mac that should run it (`fluxctl vm-image pull`, see
[vm-images.md](vm-images.md)), then add it.

```bash
curl -X PUT $CENTRAL/fleet/images/macos-27-xcode -H "Authorization: Bearer $FLEET_TOKEN" \
  -H 'content-type: application/json' -d '{
  "display_name": "macOS 27 with Xcode", "guest_os": "macos",
  "image": "/Users/fluxvm/vms/macos-27-xcode/disk.raw", "vcpus": 6, "memory_mib": 16384,
  "source": "ghcr.io/acme/macos-xcode:27"}'
```

`vcpus` and `memory_mib` are the defaults the console offers (4 and 8192 if omitted). Tenants read the catalog with
`GET /fleet/images`. Only the operator can add, replace (`PUT`) or remove (`DELETE /fleet/images/{name}`) entries.

## Turning it on

Tenancy is off until the first tenant exists. Until then central behaves exactly as before. Give central a fleet token
first: that token is the operator, and node heartbeats use it too.

```bash
fluxvm-agent central --token "$FLEET_TOKEN"          # state in /var/lib/fluxvm-agent/fleet-tenancy.json
export CENTRAL_URL=http://fleet-registry:7799 FLUXVM_AGENT_TOKEN="$FLEET_TOKEN"

fluxctl fleet tenant create acme --max-leased-hosts 2 --max-vms 20 --max-memory-mib 98304
# {"tenant": {...}, "token": "fvt_…", "note": "store this token now; …"}
```

Central stores only the SHA-256 of a tenant token. `fluxctl fleet tenant rotate-token acme` issues a new one, and
`fluxctl fleet tenant update acme --disable` blocks the tenant at the door.

A tenant token can call only these routes: `POST/GET /fleet/vms`, `DELETE /fleet/vms/{node}/{id}`,
`POST /fleet/vms/{node}/{id}/exec`, `GET/POST /fleet/leases`, `GET /fleet/leases/{id}`, `POST /fleet/leases/{id}/release`,
`GET /fleet/usage`, `GET /fleet/tenants/me`, `GET /fleet/capacity`, `GET /fleet/images`, `GET /fleet/nodes/{name}` (a Mac
it leases), and its own private networks (`GET/POST /fleet/overlays`, `GET/DELETE /fleet/overlays/{name}`,
`POST /fleet/overlays/{name}/members`, `DELETE /fleet/overlays/{name}/members/{node}`,
`POST /fleet/overlays/{name}/rotate`). Everything else is operator-only. In `GET /fleet/vms`, a tenant's
`unreachable_nodes` names only its own Macs and Macs running its VMs, without the node's error text.

## Leases

As the tenant:

```bash
export FLUXVM_AGENT_TOKEN=fvt_…                      # the tenant's token
fluxctl fleet lease create --min-memory-mib 65536 --nested
fluxctl fleet create --spec macos-ci.json            # lands on the leased Mac
fluxctl fleet lease release lease-…                  # refused before 24 h
```

A lease takes the smallest eligible Mac that is all of:
- healthy;
- not cordoned or draining;
- not leased and not waiting for a scrub;
- running no VMs (sole use);
- large enough and matching any `--node-selector` and `--nested`.

`--node` names a specific Mac instead. An operator can lease for a tenant (`--tenant`), can lease a busy Mac
(`allow_busy`) or a Linux host (`any_node`), and can force an early release (`?force=true`). A forced release is still
billed to the 24-hour minimum.

`hours` sets an end time. The reconciler ends the lease when that time passes. When a lease ends:
1. the tenant's VMs on that Mac are deleted;
2. the Mac is marked **scrub pending** and takes no new lease or placement until an operator scrubs it.

```bash
fluxctl fleet scrub mac-studio-3            # refused while VMs remain; --force deletes them
```

A scrub guarantees that no VM and no VM workspace is left: deleting a VM removes its disks, EFI and auxiliary storage.
Anything host-level beyond that, such as login Keychain items, caches or a full reinstall, belongs in your runbook before
you scrub.

## Quotas and ownership

Every VM created through central is recorded in a ledger with its tenant, node, size and start and end times. Quotas
(`max_vms`, `max_vcpus`, `max_memory_mib`, `max_leased_hosts`) are checked against the ledger before placement, and a
breach returns `429`. A create in flight holds its share of the quota until the node answers, so a burst of parallel
creates cannot overshoot. A tenant's create is always stamped with `"tenant": "<id>"`. Tenants see, delete and exec only VMs
the ledger records as theirs; anything else is a `404`.

The reconciler runs every 60 s (`--reconcile-secs`, or on demand with `fluxctl fleet reconcile`). It closes ledger
entries for VMs a node no longer lists, for example after a TTL or a delete made on the node directly, and ends expired
leases.

## Drain

```bash
fluxctl fleet drain mac-mini-2 --mode evict-ephemeral
fluxctl fleet drain-status mac-mini-2
```

A drain cordons the node, marks it draining, then handles each VM by mode:

| Mode | Ephemeral VMs (TTL, or label `fluxvm.dev/ephemeral=true`) | `"reschedulable": true` VMs | Others |
|---|---|---|---|
| `cordon` (default) | kept | kept | kept |
| `evict-ephemeral` | deleted | kept | kept |
| `reschedule` | deleted | recreated elsewhere, then the old one deleted | kept |

Virtualization.framework has no live migration, so rescheduling recreates the VM from its spec and does not carry disk
state. Only VMs created with `"reschedulable": true` keep their spec, because create bodies can carry secrets. A leased
Mac needs `--force`, because draining it breaks the tenant's sole use. `fluxctl fleet uncordon` ends a drain.

## Metering

```bash
fluxctl fleet usage --from 2026-10-01T00:00:00Z --to 2026-11-01T00:00:00Z --csv
```

Per tenant, for the window:

| Field | Meaning |
|---|---|
| `vm_seconds`, `vcpu_seconds`, `memory_mib_seconds` | VM time, weighted by size |
| `macos_vm_seconds` | VM time spent in macOS guests |
| `lease_seconds` | Actual lease time |
| `billable_lease_seconds` | Lease time with each released lease counted to at least its 24-hour minimum |
| `vms_started`, `leases_started` | Starts within the window |

The CSV reports the same figures in hours. A tenant token always sees only its own row.

The ledger covers VMs created through central. VMs created directly on a node are not metered. Records older than
`ledger_retention_days` are dropped, so export usage before then.

## API summary

A tenant can also `GET /fleet/nodes/{name}` for a Mac it leases. It sees the Mac's size, free capacity and Apple
capabilities, but not its address, labels or security report.

| Route | Who |
|---|---|
| `GET/POST /fleet/tenants`, `GET/PATCH/DELETE /fleet/tenants/{id}`, `POST /fleet/tenants/{id}/rotate-token` | operator |
| `GET /fleet/tenants/me` | tenant |
| `GET/POST /fleet/leases`, `GET /fleet/leases/{id}`, `POST /fleet/leases/{id}/release[?force=true]` | tenant, operator |
| `POST /fleet/nodes/{name}/scrub[?force=true]` | operator |
| `POST /fleet/nodes/{name}/drain?mode=…[&force=true]`, `GET /fleet/nodes/{name}/drain` | operator |
| `POST /fleet/vms/{node}/{id}/exec` (`{"command": "…", "timeout_seconds": N}`) | tenant (own VMs), operator |
| `GET /fleet/usage?from=&to=&tenant=&format=json\|csv` | tenant (own), operator |
| `POST /fleet/reconcile`, `GET/PUT /fleet/policy` | operator |
| `GET /fleet/images` | tenant, operator |
| `GET /fleet/vms/{node}/{id}/screen[?max_width=N]` | tenant (own VMs), operator: the display as PNG; `409` when the VM has none |
| `GET/POST /fleet/tenants/me/keys`, `DELETE /fleet/tenants/me/keys/{id or name}` | tenant (primary token for changes) |
| `GET/POST /fleet/tenants/{id}/keys`, `DELETE …/keys/{key}` | operator |
| `GET /fleet/events[?limit=N&tenant=T]` | tenant (own actions), operator (all, or one tenant's) |
| `PUT/DELETE /fleet/images/{name}` | operator |
| `GET /fleet/nodes/{name}` | operator; tenant for a Mac it leases (size, capacity, Apple capabilities only) |

## Not done yet

- A two-Mac hardware run of leases, scrub and drain.
- Host-level scrubbing beyond VM workspaces.
- High availability is in [fleet-ha.md](fleet-ha.md) (several centrals, one Redis), not yet run on real hosts.
- Tenant vmnet networks (`isolate_tenant_networks`) are per Mac. For one network spanning a tenant's Macs, use an
  overlay: [cross-mac-networks.md](cross-mac-networks.md).
- Lab mode (no `--token`) and tenants do not mix: central refuses to create tenants without `--token`, and if tenants
  exist when it starts without one, every caller except a tenant gets a `401` (node heartbeats included).
