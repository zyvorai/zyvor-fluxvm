# Mac fleet scheduler: `mac-fleet run`, drain, templates, prune

Builds on [mac-fleet-quickstart.md](mac-fleet-quickstart.md). One command line picks the Mac for a macOS job and lets that Mac's own FluxVM
daemon create the guest.

> **Status.** Unit-tested with a fake ssh transport only. **Not run on a real Mac or a real macOS guest.** Read the commands with `--dry-run`
> first. See [What is not verified](#what-is-not-verified).

## Run a command on the best Mac

```bash
fluxctl mac-fleet templates distribute -f fleet.yaml --template registry.example/xcode:15@sha256:<64 hex>
fluxctl mac-fleet run -f fleet.yaml --template registry.example/xcode:15@sha256:<64 hex> --cpus 4 --memory 8g \
    --queue-timeout 600 -- xcodebuild -version
fluxctl mac run --fleet fleet.yaml --cpus 4 registry.example/xcode:15@sha256:<64 hex> -- xcodebuild -version   # same thing
```

What happens: every Mac is probed read-only over ssh (one call each, `--probe-timeout`), each is scored for the job, the best is chosen, a lease
takes its slot, and `fluxctl mac run --rm` runs there over ssh. The guest is deleted when the command ends; if the job fails, is killed by
`--timeout` or the connection is lost, a second call (`fluxctl mac rm fleet-<id>`) makes sure the guest is gone. The exit status is the job's.

### What a Mac reports

From the probe: macOS slots in use (a macOS VM that is not stopped or failed holds one; Apple allows 2), free CPUs (cores minus running VMs'), free
memory (Mac memory minus the macOS headroom, `max(3 GiB, a quarter)`, minus running VMs'), free disk, AC or battery, and
`fluxctl mac images --json`: the templates it holds as `name:tag` plus the digest. Labels come from the inventory (`labels: [zone=lab]`).
Where the central fleet agent reports capacity (`AppleHostCaps`), nothing was changed: the scheduler reads the Macs itself over ssh.

### How a Mac is chosen

Hard filters (each Mac that fails one is listed with every reason):

| Filter | Clears by itself? |
|---|---|
| ssh, daemon or install problem | maybe |
| `--host NAME` names another Mac | no |
| drained | no (`mac-fleet resume`) |
| lacks a `--host-label` | no |
| does not hold the template, or holds it at another digest than the pinned one | no (`templates distribute`) |
| less free disk than `--disk-gib`; on battery with `--ac-only` | yes |
| both macOS slots taken; not enough free CPU or memory for `--cpus` / `--memory` | yes |

Then `fluxvm_scheduler::apple_placement::score` (tightest fit: small jobs pack onto busy Macs and keep big Macs free) plus a bonus
for holding the exact template. When no Mac fits now but one would when a job ends, the job **waits** if you gave `--queue-timeout SECS`
(polling every `--poll`, one "queued: ..." line per change) and otherwise fails at once saying so. When nothing can fit without an
operator, it fails immediately and says what each Mac needs. Without `--cpus` and `--memory` only the slot is checked (the template's
own defaults apply).

Leases: a placement holds its slot, CPUs and memory in this process for 120 s or until its job ends, so a second placement before the next
probe cannot take the same slot. Since this version they are also **shared between the `fluxctl` processes of one machine**: each placement takes
an advisory `flock` on `mac-fleet-leases.lock` (next to the state file), reads the other processes' live leases from `mac-fleet-leases.json`, decides, writes its own and unlocks. The
critical section is pure computation (the ssh probes happen before it), so it is short. A lease of a process that died is dropped at once (its pid is gone), or after 120 s at the latest.

Limits: it is a local `flock`, so it coordinates processes on **one machine** on a file system with working locks (not NFS); two supervisors on two machines still race for the last slot,
and the Mac's daemon then answers 409 (the job fails; run it again). Waiting placements poll the files and the Macs; there is **no first-come-first-served order** between processes, and the queue is still the
polling loop of the command that is waiting. `FLUXVM_MAC_FLEET_NO_SHARED_LEASES=1` turns the sharing off (leases are then in-process only, as before). A lease that cannot be read or written fails the placement with the reason rather than being ignored.

## Drain and resume

```bash
fluxctl mac-fleet drain mini-2 -f fleet.yaml      # no new job is placed there; running jobs finish
fluxctl mac-fleet drain --list
fluxctl mac-fleet resume mini-2 -f fleet.yaml
```

State is a small JSON file (`~/.config/fluxvm/mac-fleet-state.json`, or `$FLUXVM_MAC_FLEET_STATE`; no secrets), read before every placement, so it
applies to a runner that is already running. Drain is by the host's name in the inventory.

## Templates

```bash
fluxctl mac-fleet templates ls -f fleet.yaml
FLUXVM_REGISTRY_USER=robot FLUXVM_REGISTRY_PASSWORD=... \
  fluxctl mac-fleet templates distribute -f fleet.yaml --template registry.example/xcode:15@sha256:<64 hex>
```

`distribute` requires the digest. Each Mac that does not already hold that digest runs `fluxctl mac pull registry.example/xcode:15`, then
`mac images --json` is read again and the digest compared with yours. A mismatch (the tag moved, or the registry served something else) is reported
per Mac; nothing is deleted, and the scheduler will not place a job pinned to your digest there because that Mac holds a different one. The registry
user and password are read from this shell's environment, sent on ssh stdin, exported on the Mac only for the pull, never an argument and scrubbed from
output. `--pull-timeout` bounds each pull (default 3600 s). Apple's licence limits where macOS images may be stored: use a private registry.

## Removing Macs: `apply --prune`

`apply` records which Macs the inventory listed (in the state file, per inventory path). `apply --prune` then removes FluxVM (LaunchAgents and install;
VM images and data stay) from Macs that the previous `apply` listed and this one does not. Without `--prune` it only names them. A Mac that still runs VMs is
refused and stays on the list for the next `apply --prune`. A Mac that was never applied from this machine is not known and is never pruned. `apply --dry-run` records nothing.

## `status --detail`

```bash
fluxctl mac-fleet status --detail -f fleet.yaml [--json]
```

The `status` table plus, per Mac, what the scheduler and the operator of a headless Mac need: **drain state** (and for how long), macOS guests in use (n/2), AC or battery, free disk, the **templates it holds**
(name and digest) and the **headless checks** of `fluxctl mac host headless-check` (an unlocked console session, sleep, auto-login), each failing one with its fix. Read-only: one probe and one `mac host headless-check --json` per Mac. A Mac whose
FluxVM is too old to have `mac host` shows `?` for headless. It also prints how many shared leases the placements on this machine hold. `--json` gives `{"hosts": [...], "leases": N}`. Plain `status` is unchanged.

## `mac ps --fleet` and `mac images --fleet`

```bash
fluxctl mac ps --fleet fleet.yaml [-a] [-f status=running] [--json]
fluxctl mac images --fleet fleet.yaml [-a] [-f reference=xcode*] [--json]
```

The same listings from every Mac of the inventory at once over ssh, one table with the Mac in the first column. `--json` prints one document: `{"schema_version": 1, "hosts": [{host, name, ok, error}], "instances"|"images": [...]}`, each row with `host` (the Mac's name) and
`host_label` added and the daemon's raw `vm` record left out. A Mac that cannot be reached is a row in `hosts` with the reason, and the command exits non-zero. `-a` and `-f` are passed to each Mac; `--format` and `-q` are refused with `--fleet`. ssh uses the defaults (`~/.ssh/config`; no `--identity` flag here, like `mac run --fleet`). Read-only.

## Timeouts

`--step-timeout SECS` (default 1800, `0` = none) bounds every ssh call of every `mac-fleet` command: each step of `up`/`apply` (install, daemon,
enroll, k3s, probe) is one call, and a call over its deadline is killed and reported as `timed out after Ns`. Raise it above a long `--k3s-wait`.
The commands in this page add `--probe-timeout` (30 s), `--timeout` (the job, 3600 s) and, for `templates distribute`, `--pull-timeout`. Killing the local `ssh` stops
waiting for the remote command; for a job the guest is then deleted by a separate call.

## What is not verified

- Everything over a real `ssh`, on a real Mac; the probe lines (`sysctl hw.ncpu`, `hw.memsize`, `pmset -g batt`, `fluxctl mac images --json`) and their parsing against real output.
- That `fluxctl mac run --rm ... -- CMD` run from an ssh session reaches the Mac's daemon (default endpoint) and boots a guest; that `mac rm` removes a half-started one.
- A real registry pull, the digest the store records for it (taken from `mac images --json`), and what `mac pull` does with a moved tag.
- Races between two `fluxctl` processes: the file lock is unit-tested (threads, a second pid, dead pids, expiry), not with two real `fluxctl` processes placing on real Macs at once.
- `mac run --fleet` takes the system ssh defaults (no `--identity`/`--known-hosts` flags there; use `~/.ssh/config`), and refuses `-p`, `-e`, `-v`, `-w`, `--label`, `--name`, `--restart` and `-d`.
