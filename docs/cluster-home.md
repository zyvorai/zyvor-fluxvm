# Cluster home: is everything OK, in plain English

You run a few Macs and Linux machines with Docker and k3s on them, and you want one answer to "is it all fine, and if
not, what do I do?". That is `fluxctl cluster status`, `fluxctl fix` and the **Home** page of
[FluxVM Studio](ui.md). All three use one health model, so they never disagree.

## `fluxctl cluster status`

    fluxctl cluster status                      # this machine
    fluxctl cluster status -f fleet.yaml        # this machine and every Mac of a fleet inventory (read-only ssh)
    fluxctl cluster status --no-local -f fleet.yaml --json

You get a light for the cluster and for each machine, in words:

    (yellow)  1 of 2 machines need attention
       room for 6 more VMs, 2 of 4 macOS slots in use, 412 GiB disk free

    (green) mini-01 (this machine): mini-01 is healthy
          room for 3 more VMs, 0 of 2 macOS slots in use, 212 GiB disk free, 11 GiB of 16 GiB memory free
          last backup: 3 hours ago · FluxVM 0.4.21
    (yellow) mini-02: mini-02 needs attention: 2 of 2 macOS slots are in use
       (yellow) 2 of 2 macOS slots are in use on mini-02
            why: Apple's licence allows two macOS VMs per Mac at a time
            fix: stop one macOS VM, or run the job on another Mac
            more: `fluxctl explain MACOS_SLOTS_FULL`

    Next: 2 of 2 macOS slots are in use on mini-02: stop one macOS VM, or run the job on another Mac

* **Lights.** Green: nothing to do. Yellow: works, but something deserves a look. Red: something does not work. A
  newer FluxVM being available never turns a light yellow; it is shown next to the version.
* **Room.** "Room for N more VMs" is how many more small VMs (2 vCPUs, 4 GiB) fit by free CPU and free memory. macOS
  guests are counted separately: Apple's licence allows 2 at a time per Mac. A new macOS VM also needs about 60 GiB of disk.
* **Backups.** The age of the newest backup under the daemon's `backups` folder. A machine with VMs and no backup, or a
  backup older than 7 days, is yellow. For a Mac of a fleet the backup age is "not reported": the ssh probe does not read it.
* **Next.** The one thing to do now: the worst problem first, and among equals one `fluxctl fix` can do for you.
* Exit status: 1 when the cluster is red, else 0.

`--json` prints schema version 1 (`schema`, `light`, `headline`, `machines[]`, `items[]`, `capacity`, `next_action`;
every item has `severity` (`ok|warn|fail`), `message`, `cause`, `fix`, `explain`). Fields may be added without a version
bump; one that is removed or changes meaning bumps `schema`. The daemon serves the same document for its own node at
`GET /v1/cluster/health`.

### What a fix says

Every item with a fix says how risky it is:

| Field | Meaning |
|---|---|
| `command` / `steps` | the exact command to run on that machine, or plain steps when there is no single command |
| `safe` | additive and reversible: `fluxctl fix` may apply it |
| `needs_sudo`, `needs_restart`, `needs_login` | what the fix needs from you |
| `changes_security` | FileVault, automatic login, the screen lock: **never** applied by FluxVM |

## `fluxctl fix`

    fluxctl fix --dry-run     # show the plan, change nothing
    fluxctl fix               # show the plan, ask, apply
    fluxctl fix --yes         # no question
    fluxctl fix --sudo        # also the few additive sudo fixes (below)
    fluxctl fix -f fleet.yaml # also look at a fleet's Macs (fixes still apply only to this machine)

What it may do by itself, and nothing else:

1. **Start a stopped service**: the FluxVM LaunchAgent on a Mac (`launchctl kickstart`, or `bootstrap` when it was
   never loaded). On Linux the unit needs sudo, so it is a `--sudo` fix.
2. **Create a missing directory** FluxVM wants to write to (the Mac VM store).
3. **Pull a missing image** (`fluxctl mac pull REFERENCE`).

With `--sudo` it may also run, via your own `sudo` (which asks for your password itself), exactly these additive
commands: `sudo systemctl start ...`, `sudo modprobe ...`, `sudo sysctl -w ...`. Anything with a shell operator is
refused. Everything else is printed as the exact command, with the machine to run it on: adding yourself to the `kvm`
group (needs a new login), `fluxctl mac host tune` (sudo), reinstalling, and every security setting.

Fixes for another machine (a Mac of the inventory) are printed, not applied: run them on that Mac.

Every change is appended as one JSON line to `<state_dir>/health/fix-log.jsonl` (or
`~/.config/fluxvm/fix-log.jsonl` when the state directory is not writable): time, machine, item, what, whether it
worked, and its output. After applying, the status is read again and printed, and the exit status is 1 if it is still red
or a fix failed.

## The Home page in Studio

`http://127.0.0.1:7788/console/` opens on **Home** (the old Overview is at `#/overview`). It shows the same model for
the node you are connected to: the headline and light, the next best action, each machine with its room, backup age and
version, and each problem with its cause, its fix and a **Copy as fluxctl** button (`fluxctl fix` for a safe fix, the
exact command otherwise, `fluxctl explain CODE` when there is only advice). Studio only shows and explains: it does not
run fixes, so nothing changes until you run the copied command in a terminal. The page needs no new runtime
dependency. It covers what the daemon can see (VMs, macOS slots, memory, disk, backups, version); the host-setting
checks of `fluxctl doctor` and `fluxctl mac host headless-check` and the Macs of a fleet are in the CLI.

## Codes: `fluxctl explain`

Every warning and failure carries a code; `fluxctl explain DAEMON_DOWN` says what it means, the usual causes and what to
do. They are listed with `fluxctl explain --list` (status `-`, as they are not API errors). Codes: `HOST_UNREACHABLE`,
`NOT_INSTALLED`, `DAEMON_DOWN`, `NO_KVM`, `CGROUPS`, `OPEN_FILES`, `NET_DEVICES`, `IP_FORWARD`, `MISSING_BINARIES`,
`RUNNER_PROBLEM`, `LOW_DISK`, `LOW_MEMORY`, `NO_LOGIN_SESSION`, `SECURITY_SETTING`, `AGENT_NOT_LOADED`,
`POWER_SETTINGS`, `STORE_PROBLEM`, `NO_SSH_KEY`, `STALE_PATHS`, `MACOS_SLOTS_FULL`, `NO_BACKUP`, `STALE_BACKUP`,
`K3S_NOT_READY`, `MISSING_IMAGE`, `UNSUPPORTED_HOST`, `NO_MACHINES`, the Gateway codes below, `CHECK_FAILED` (a check with no dedicated
explanation; its own message is shown).

## Gateways: problems with your Gateway API setup

When you have a FluxVM cluster kubeconfig (`~/.kube/fluxvm-NAME.yaml`, written by `fluxctl kube up`), `cluster status` and
`fix` also read that cluster's Gateways, HTTPRoutes and their Services with `kubectl get` (read-only, that kubeconfig only;
your default kubeconfig and clusters you have no FluxVM kubeconfig for are never touched). A cluster without Gateway API,
or one that cannot be reached, adds nothing. The problems show as cluster-level items, with a fix that is a command:

| Code | When | Fix |
|---|---|---|
| `ENVOY_GATEWAY_NOT_INSTALLED` | you have Gateways or routes of the Envoy class but its controller is not running. Never raised just because Envoy is absent. It hides the symptoms below for those objects. | `fluxctl kube cluster platform envoy-gateway NAME` |
| `GATEWAY_NOT_PROGRAMMED` | a Gateway's `Programmed` condition is False | `kubectl describe gateway NAME -n NS` |
| `ROUTE_NOT_ACCEPTED` | a route's `Accepted` condition is False for a Gateway | `fluxctl gateway inspect ROUTE -n NS --cluster NAME` |
| `ROUTE_BACKEND_UNRESOLVED` | a route's `ResolvedRefs` condition is False | `fluxctl gateway inspect ROUTE -n NS --cluster NAME` |
| `BACKEND_NO_READY_ENDPOINTS` | a route's Service has no ready endpoints (yellow: pods may still be starting) | `fluxctl gateway inspect ROUTE -n NS --cluster NAME` |

A condition whose `observedGeneration` is older than the object's `generation`, one that is `Unknown`, and one the
controller has not written yet all mean "not reconciled yet" and raise nothing, so a Gateway you just edited never flashes
red. A Service whose endpoints could not be read is not reported as having none. None of these fixes is applied by
`fluxctl fix`; it prints them. `fluxvm_core::health::gateway_items` is the pure rule set (fixtures cover every code, stale
generations and the healthy case); the kubectl reads are `Cluster::facts` in `crates/fluxctl/src/gateway/kube.rs`. Not
run against a live cluster when this section was written. The daemon's `GET /v1/cluster/health` describes its own node and
does not include these.

## Where the checks come from

| Source | What | Where it runs |
|---|---|---|
| `doctor` | daemon, binaries, KVM, cgroups, runner entitlement, memory, session, agents | this machine |
| `vz` | `fluxctl mac doctor`: store, disk, APFS, session, ssh key, FileVault | this Mac |
| `host` | `fluxctl mac host headless-check`: console session, sleep, power | this Mac |
| `fleet` | the `mac-fleet` ssh probe and preflight | each Mac of `-f` |
| `capacity`, `backup` | slots, disk, memory, backup age | everywhere |

The model is `fluxvm_core::health` (pure; the rules are unit tested with fixtures); the commands are
`crates/fluxctl/src/cluster_cmd.rs`; the endpoint is `crates/fluxvm-api/src/cluster_health.rs`.

## What is not verified

Unit tests cover the model, the planning and the fakes of every action, and the fleet probe against a fake ssh. See the
branch report for what was run on a real machine.

## Related

[mac-fleet-quickstart.md](mac-fleet-quickstart.md), [macos-host.md](macos-host.md), [ui.md](ui.md).

See also [fluxctl doctor](doctor.md): the same model as one short list for the whole setup, with `--fix`.
