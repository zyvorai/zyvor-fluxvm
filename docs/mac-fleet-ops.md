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

## Not verified

Everything above is unit-tested with fake transports and temp files. Not run against real Macs, GitHub, `dns-sd` or a real `ssh`.
