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
`GET /fleet/usage`, `GET /fleet/tenants/me` and `GET /fleet/capacity`. Everything else is operator-only.

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
breach returns `429`. A tenant's create is always stamped with `"tenant": "<id>"`. Tenants see, delete and exec only VMs
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

## Not done yet

- A two-Mac hardware run of leases, scrub and drain.
- Host-level scrubbing beyond VM workspaces.
- A highly available central: the registry is still one process with JSON files.
- Per-tenant vmnet isolation between a tenant's VMs on different Macs.
- Quota checks are not atomic with the create, so a burst of parallel creates by one tenant can overshoot its quota
  slightly.
- In lab mode (no `--token`), any caller without a tenant token is the operator, even once tenants exist. Central logs
  a warning at start.
