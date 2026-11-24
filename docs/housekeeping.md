# VM housekeeping: protect, df, prune, top

Four commands for the day-to-day care of a FluxVM host. They talk to the daemon (`--server`, the current context, or the
local `listen` address); with no daemon they say so (`fluxctl explain DAEMON_REQUIRED`).

## `fluxctl protect` and `unprotect`

```
fluxctl protect db                  # one VM (name, id or id prefix)
fluxctl protect -l env=prod         # every VM with that label
fluxctl protect --list              # what is protected
fluxctl unprotect db
```

A protected VM carries the label `fluxvm.io/protected=true`. `VmManager::delete` refuses it, so every route is covered
(`fluxctl delete`, `-l` bulk delete, `DELETE /v1/vms/{id}`, `fluxctl terminate`, `fluxctl prune`). The REST answer is
`409 Conflict` with the fix in the message (`fluxctl explain VM_PROTECTED`). Stop, snapshot, label and edit still work: only
removal is blocked. The label is an ordinary label, so it shows in `fluxctl ps --json` and `--label` filters.

## `fluxctl df`

```
TYPE        TOTAL  ACTIVE         SIZE    RECLAIMABLE
VMs             3       1      9.0 GiB    1.0 GiB (11%)
Branches        1       -      1.0 GiB              -
Images          -       -      2.0 GiB              -
```

Like `docker system df`. Sizes are blocks really allocated (a sparse disk counts what it occupies). RECLAIMABLE counts
stopped or failed VMs that are not protected, which is what `fluxctl prune --older-than 0s` would delete. Copy-on-write
clones share blocks, so rows can add up to more than the volume; the "Volume: N free" line is the truth. `-v` lists every
VM and branch, `--json` is machine-readable. REST: `GET /v1/host/storage`.

## `fluxctl prune`

```
fluxctl prune                       # preview: stopped VMs created more than 7 days ago
fluxctl prune --older-than 0s -l env=dev --yes
fluxctl prune --failed --yes        # also VMs in the failed state
```

Without `--yes` nothing is deleted; the plan shows each VM, its age and size, and what was kept and why (protected, or newer
than `--older-than`). It never touches a running, paused or creating VM. "Older" is time since creation: the daemon does not
record when a VM last stopped. A VM the daemon refuses to delete is reported and makes the command exit non-zero.

## `fluxctl top`

```
fluxctl top                         # live, refreshes every 2 s (Ctrl-C to quit)
fluxctl top --once --sort mem --all
fluxctl top --json --count 3        # one JSON document per sample
```

Reads `GET /v1/stats` twice and shows rates: CPU as a percentage of one core (200 means two busy cores), memory used of
configured, disk and network bytes per second. A counter that reset between samples, or a VM that appeared between them,
shows `-` instead of a made-up number.
