# Mac fleet: logs, events, inventory import and runner sweep

Read-only views and small helpers on top of `fluxctl mac-fleet` (see [docs/mac-fleet-quickstart.md](mac-fleet-quickstart.md) and
[docs/mac-fleet-scheduler.md](mac-fleet-scheduler.md)). Each one runs the Mac's own `fluxctl` over `ssh`, so a Mac whose `fluxctl`
is too old, or that cannot be reached, is reported in its own line and does not fail the view. Nothing here changes a Mac.

Already covered elsewhere: `mac-fleet status --detail` (drain state, held templates, headless checks), `mac ps --fleet` and
`mac images --fleet`, signal handling and the startup sweep of `mac runner` ([docs/mac-runner.md](mac-runner.md)), and leases shared
between processes on one machine (`FLUXVM_MAC_FLEET_NO_SHARED_LEASES=1` or `FLUXVM_MAC_FLEET_LEASES=off` turns them off).

## A guest's log

```sh
fluxctl mac-fleet logs --name fleet-ab12 -f fleet.yaml --tail 200
```

Asks every selected Mac for `mac logs --tail N NAME` and prints the lines prefixed with the Mac that has the guest. A Mac that does not
have it is not an error; the command fails only when no Mac has it. `--json` prints one object per Mac (`host`, `found`, `error`,
`lines`).

## Events

```sh
fluxctl mac-fleet events -f fleet.yaml --limit 50 --event mac.
```

The recorded `mac.*` and `kube.apple.*` events of every Mac, merged by time (oldest first, newest `--limit` kept). `--event PREFIX`
filters by event name. `--json` prints one JSON object per line. A Mac whose events could not be read is named on stderr and the
command exits non-zero after printing the rest.

## Make an inventory from what you have

```sh
fluxctl mac-fleet inventory import --ssh-config --ssh-pattern 'mini-*' --out fleet.yaml
fluxctl mac-fleet inventory import --hosts-file macs.txt --user ci --label zone=lab --merge fleet.yaml
fluxctl mac-fleet inventory import --bonjour --bonjour-secs 6
```

- `--hosts-file FILE`: one Mac per line, `[user@]host[:port] [label ...]`, `#` comments.
- `--ssh-config [FILE]`: the `Host` blocks of an OpenSSH config (default `~/.ssh/config`) that name one literal host; wildcard, multi-alias
  and `Match` blocks are ignored. `HostName`, `User`, `Port` and `IdentityFile` are used.
- `--bonjour` (macOS only): browses `_ssh._tcp` with `dns-sd`. An instance name is a computer name, so `<name>.local` is a guess; check
  it with `mac-fleet doctor`.
- Without `--out` or `--merge` the YAML goes to stdout. `--out` refuses an existing file without `--force`. `--merge FILE` adds to an
  inventory: hosts it already lists (by name or host) are kept as they are, other keys are kept, comments are not.
- The result is checked with the same parser `apply` uses before it is written. Nothing is sent to any Mac.

## Remove everything a runner left: `--sweep-all`

`mac runner` sweeps leftovers of a killed run at start, sparing ids that another process holds a live lease for. `--sweep-all` drops that
protection: every `fleet-*` guest (labelled `fluxvm.owner=mac-fleet`) on the Macs and every idle `fluxvm-*` runner of the scope goes.
Use it only when no other runner or `mac-fleet run` is active on those Macs. It cannot be combined with `--no-sweep`.

## Registry login on the Macs

`mac-fleet templates distribute` makes every Mac pull a template, and a private registry needs a login on each Mac. Two sources, in this order:

1. `FLUXVM_REGISTRY_USER` / `FLUXVM_REGISTRY_PASSWORD` in your shell (as before).
2. The login you saved with `fluxctl mac login REGISTRY`. Only the entry for the template's registry is sent, as a small login file; other saved
   logins never leave your machine.

The login travels on the ssh stdin pipe, never in an argument, so it is not in `ps`, in `--dry-run` output or in a log. On the Mac it is written to a
fresh file with mode 0600, used for that one `mac pull` (through `FLUXVM_MAC_REGISTRY_AUTH`) and deleted when the script ends, also after an error. Output
that comes back is scrubbed of the password. A saved login file that other users can read is not forwarded (a warning says `chmod 600`).
`fluxvm_image::oci_registry::Credentials` now prints `<redacted>` for the password in `Debug`.

## Add a Mac and know it works: `add`

```
fluxctl mac-fleet add admin@mini-3.local -f hosts.yaml --central https://fleet:7790 --token-file join.tok
```

`add` runs, per Mac and in order: **ssh** (reachable, key accepted, host key trusted), **preflight** (Apple silicon, macOS, disk, session; nothing is
installed if this fails), **install** (the same idempotent steps as `up`), **runner** (the daemon lists VMs, `mac images` answers so templates can be
distributed, the login session can restore saved state). Each failed step prints a `FIX` line with the exact command, and the run ends with
`FLEET_ADD_BLOCKED` (`fluxctl explain FLEET_ADD_BLOCKED`). A Mac that passes is `READY` and shows in `top`. `up` is unchanged (and `add` is no longer an alias of it).
With `--dry-run`, `add` prints what `up` would send.

## Keep jobs off a Mac: `cordon`, `uncordon`, `drain`

| Command | What it does |
|---|---|
| `cordon HOST...` | No new job is placed on the Mac. Running jobs are untouched. Same list the scheduler reads before every placement. |
| `uncordon HOST...` | The Mac takes jobs again. |
| `drain HOST... [--wait] [--wait-timeout SECS]` | Cordons; with `--wait` stays until the Mac has no macOS guest left ("safe to upgrade or reboot"). On timeout the Mac stays cordoned and `FLEET_DRAIN_TIMEOUT` says what to do. |
| `resume HOST...` | Same as `uncordon`. `drain --list` shows what is cordoned. |

## Live capacity: `top`

```
fluxctl mac-fleet top -f hosts.yaml          # refreshes every 5 s in a terminal; once into a pipe
fluxctl mac-fleet top -f hosts.yaml --once --json
```

One row per Mac: state (`ready`, `full`, `cordoned`, `draining`, `unreachable`), macOS VMs against the 2-per-Mac limit, thermal state
(`pmset -g therm`; `throttled 70%` when macOS limits the CPU), free disk (flagged under 50 GiB) and power. A row that needs attention says what to do
(`FLEET_HOST_THROTTLED`, `FLEET_DISK_LOW`). Each refresh also writes `mac-fleet-top.json` next to the fleet state file (`$FLUXVM_MAC_FLEET_TOP` overrides).
It holds no secret.

## Studio: the Mac fleet page

Studio's **Mac fleet** page (`GET /v1/mac-fleet`, admin) shows the same table. The daemon does not ssh into Macs: it reads the last `top` snapshot and the live
cordon list, so a `cordon` from a terminal shows at once, and the page says how old the snapshot is. Every action is a **Copy as fluxctl** button (cordon,
uncordon, wait for empty, diagnose); Studio never runs them. With no snapshot yet the page lists the Macs of the last `apply` as "not checked" and shows the
command that refreshes them; with no Macs it shows the `add` command.

## Not verified

Everything above is unit-tested with fake transports and temp files. Not run against real Macs, GitHub, `dns-sd` or a real `ssh`.
In particular: the remote login-file script (`mktemp`, 0600, trap cleanup) is checked as text, not executed on macOS; `top` live refresh and the Studio page were not seen in a browser against a real snapshot.
