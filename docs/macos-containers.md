<!-- Copyright 2026 Zyvor AI Labs · https://zyvor.dev -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# macOS images: `fluxctl mac`

`fluxctl mac` manages a local store of **macOS guest templates** and moves them to and from an OCI registry. It is the image half of the
Docker-style workflow proposed in [macos-containers-design.md](macos-containers-design.md). A "container" here would be a full macOS VM,
not a Linux container.

Status: the commands below are implemented and unit-tested. Nothing has been run against a real registry, a daemon or a real macOS VM.

## What is available, and what is not

| Command | State |
|---|---|
| `fluxctl mac images`, `pull`, `push`, `tag`, `rmi`, `import` | Implemented (this page) |
| `fluxctl mac run [--rm] [--name N] [--user U] [--wait S] [-p H:G] IMAGE [-- CMD]`, `ps`, `rm`, `exec [--user U]` | Implemented, needs the daemon (`--server` or a context). **Unit-tested only; never run against a real VM.** An instance is a VZ macOS VM, an APFS clone of the image's disk, labelled `fluxvm.mac=instance`. A 409 means the Mac already runs 2 macOS guests; a 503 means its login session is locked. |
| `fluxctl mac build -t NAME [-f Macfile] [--build-arg K=V] [--label K=V] [--secret id=,src=] [--ssh default] [--platform darwin/arm64] [--dry-run] [--wait SECS] [CONTEXT]` | `--dry-run` parses a Macfile and prints the plan, with line-numbered errors (client-side). Without it the build runs: needs the daemon, and is **unit-tested only with a fake host; never run against a real macOS VM**. `SNAPSHOT` and `FROM IPSW latest` are refused (not supported). A `#` comment must be on its own line (a trailing one after a directive is an error). |

| `inspect`, `logs [-f]`, `cp`, `stats`, `stop`, `start`, `restart`, `pause`, `unpause`, `rename`, `wait`, `top`, `port`, `prune`, `reconcile` | Implemented (section "Instance commands"), needs the daemon. **Unit-tested on parsing and rendering only; never run against a daemon or a real VM.** |
| `run -e/--env-file/-v/-w/--label/--restart/--cpus/--memory/-d`, `ps -a/-f/--format/-q`, `exec -e/-w` | Implemented (section "Run options"), same caveat. |
| `history`, `save`, `load`, `commit`, `image inspect/prune/history/save/load`, `system df/prune` | Implemented (section "Image tools"). Store-side parts run on temporary stores in tests; `commit`, `df`'s instance half and `prune --all` need the daemon. |
| Macfile `ENV`, `ARG`, `WORKDIR`, `LABEL`, `EXPOSE`, `CMD`, `ENTRYPOINT`, `ADD`, `HEALTHCHECK`; `.macignore` | Implemented (section "Macfile reference"). Parser and planner unit-tested; never built on a real VM. |
| `mac doctor`, `mac wait-ready`, `run --queue`, `run/ps/inspect --json` | Implemented (sections "Preflight", "Waiting", "JSON output"). Unit-tested with fakes and golden files; **never run against a real daemon or VM**. `mac doctor`'s probes were run once on a developer Mac, which has no daemon. |
| `scripts/test-mac-vz.sh` | A live acceptance script for all of the above. **Written, not yet run on hardware**; its `--self-test` (fake `fluxctl`) runs anywhere. |
| `compose up/down/ps/logs/exec/restart/config/stop/start/images/pull/push/ls/run/top/cp` | Implemented (section "Compose"). **At most 2 services (2 macOS VMs).** `up --dry-run`, `config`, `images`, `pull` and `push` are client-side; the rest needs the daemon and was never run. |

The image commands are client-side only: they talk to a registry and the local store, never to the daemon. `run` and the other instance
commands start a stored image through the daemon.

## The local store

- Location: `~/.fluxvm/mac-images`, or the directory in `$FLUXVM_MAC_STORE` when that is set and non-empty. It is created on first use.
- `index.json` in the store maps `name:tag` to a template directory. It is written atomically (temp file, then rename). A corrupt index
  is reported as an error, not treated as an empty store.
- A template is a directory holding `disk.raw`, `hardware.bin` and `auxiliary.bin`, the same layout `fluxctl vm-image push|pull` uses.
- Pulled images live in a directory named after the first 32 hex characters of the manifest digest; imported ones in `local-<12 hex>`.
- Several names can point at one directory. The files are deleted only when the last name using them goes.
- A reference without a tag gets `:latest`. A tag after the last `/` is kept, so `localhost:5000/macos` becomes
  `localhost:5000/macos:latest`. `@sha256:...` references are kept as given. Empty references, whitespace, `..` and empty tags are refused.

## Commands

### `fluxctl mac images [--json]`

Lists local images: reference, the first 12 digits of the digest (`-` for a local import), size on disk of the three files, and source
(the registry reference, or `local`). `--json` prints `reference`, `digest`, `source` and `size_bytes` per image.

### `fluxctl mac import DIR NAME[:TAG]`

Adds a template directory to the store. `DIR` must contain `disk.raw`, `hardware.bin` and `auxiliary.bin`; a missing file is named in
the error. Each file is copied with `cp -c` (an APFS clone, instant on the same volume as the store), and with a plain copy when the clone
fails. If `NAME:TAG` already exists it is replaced, and the old files are freed if no other name uses them.

### `fluxctl mac pull REGISTRY/REPO[:TAG]`

Pulls a VM artifact (as written by `vm-image push`) into a scratch directory inside the store, checks it has the three template files,
moves it into place as `<digest-dir>`, and records the reference. Pulling a digest already in the store keeps the stored copy and drops
the new one. A failed pull removes its scratch directory.

### `fluxctl mac push REFERENCE [REMOTE]`

Pushes a local image. `REMOTE` must name a registry (first path part has a `.` or `:`, or is `localhost`), for example
`ghcr.io/acme/macos-xcode:15`. When `REMOTE` is omitted, `REFERENCE` is used if it already names a registry; a plain local name is refused
with a hint such as `fluxctl mac push macos-xcode:15 ghcr.io/acme/macos-xcode:15`. The artifact is marked `os=macos`. The registry's
JSON result is printed.

### `fluxctl mac tag SOURCE TARGET`

Gives an image a second name. No files are copied. If `TARGET` already exists it is replaced, and its old files are freed when nothing
else uses them.

### `fluxctl mac rmi REFERENCE`

Removes a name. Prints whether the files were deleted or kept because another name uses them. An unknown reference is an error.

### `fluxctl mac build -t NAME[:TAG] [-f Macfile] [--dry-run] [--wait SECS] [CONTEXT]`

Builds an image from a Macfile (default `CONTEXT/Macfile`; `CONTEXT` defaults to `.` and is where `COPY` sources are read from).
`--dry-run` prints the numbered plan and changes nothing. A real build:

1. Refuses a Macfile with `SNAPSHOT` or `FROM IPSW latest` before creating anything (a saved running state cannot move to another Mac;
   installing from an IPSW is not wired into `build`).
2. Creates a throwaway instance named `build-<6 hex>` from the `FROM` template (an APFS clone through the daemon, labelled like `run`
   instances, so it shows in `mac ps`) and waits up to `--wait` seconds (default 900) for SSH. The 2-macOS-VM limit (409) and locked-session
   (503) errors are the same as for `run`.
3. Runs each step in order: `RUN` over `ssh` as the `USER` in force (`admin` when there is none), `COPY` with `scp -r` from the context.
   Output streams to the terminal.
4. Stops the instance and clones its `disk.raw`, `hardware.bin` and `auxiliary.bin` into the store as `NAME:TAG` (an existing tag is replaced).
5. Deletes the instance.

On any failure the instance is deleted and the error names the failing step, numbered as in `--dry-run` (`build failed at step 3 (...)`).
By default the build uses a step cache (see [Build cache](#build-cache)); `--no-cache` runs the plain straight-through build above. The build instance runs on the daemon's Mac, so the daemon must be on this machine
(the template and the result are local paths).

## Run options

`fluxctl mac run [OPTIONS] IMAGE [-- CMD...]`. `CMD` is an argv after `--`: one word is shell text as written
(`-- "make && make test"`), several words are quoted one by one (`-- sh -c "echo hi"`). With no `CMD`, the image's `ENTRYPOINT`
and `CMD` (from its Macfile) run; with neither, the instance just stays up.

| Option | Effect |
|---|---|
| `-e NAME[=VALUE]` | Environment for `CMD` (`NAME` alone copies it from this shell). Repeatable. Not stored in the VM, so a later `mac exec` needs its own `-e`. |
| `--env-file FILE` | `NAME=value` lines, `#` comments. `-e` wins over the file. |
| `-v HOST:GUEST[:ro]` | A virtiofs share (`shared_folders`) from a directory on this Mac; the daemon must run on this Mac. A bare name (`-v cache:/var/cache`) is a named volume, made on first use (see "Volumes, ports and networks"); compose still refuses named volumes. A macOS guest shows shares under `/Volumes/My Shared Files`; `run` then makes the GUEST path a symlink to the share over SSH (tries without, then with `sudo -n`; a failure is a warning). **The share's name under that directory (`fs0`, `fs1`, ...) is a guess from the Linux tags and is unverified on a real guest.** |
| `-w DIR` | Directory `CMD` runs in (overrides the image's `WORKDIR`). |
| `--label KEY=VALUE` | A label on the VM. Keys starting `fluxvm.mac` are reserved. |
| `--restart POLICY` | `no`, `always`, `unless-stopped`, `on-failure`. **A label only**: the daemon never restarts a macOS VM. `fluxctl mac reconcile` applies it (run it from launchd or cron). `mac stop` marks an instance so `unless-stopped` leaves it alone. |
| `--cpus N`, `--memory 8g` | `vcpus` and `memory_mib` of the VM request. |
| `-d` | With `CMD`: run it in the background in the guest (`nohup`, output in `/tmp/fluxvm-mac-cmd.log` there) and return. Cannot be combined with `--rm`. |
| `-u`, `--user U` | SSH user (default: the image's `USER`, else `admin`). |
| `-i`, `-t` (`-it`) | Interactive, like `docker run -it`: `-t` gives the command a terminal. With no `CMD` (and no image command) you get the guest's login shell in the new instance. Not with `-d` or `--fleet`. |

`mac exec [-it] [-e NAME=V] [-w DIR] [-u USER] NAME [-- CMD]` applies the image's `ENV` and `WORKDIR` (while the image is still
in the store) and then its own flags; it does not use `ENTRYPOINT`.

### Interactive access: `exec -it`, `run -it`, `attach`

All three use the `ssh` that `mac exec` and `mac run` always used (generated key, host key not remembered for local guests); there
is no extra agent or port.

- `fluxctl mac exec -it NAME` opens a login shell; `fluxctl mac exec -it NAME -- top` runs one program with a terminal. `-e`,
  `-w` and `-u` work as in Docker (`-u` is the guest account that logs in, so it must exist in the guest).
- Terminal rules: `-t` needs a terminal on this side; from a pipe or CI it prints a warning and runs without one. `-i` alone means
  pipes. With neither flag you get a terminal only when both stdin and stdout are terminals, so `mac exec NAME -- cmd | grep x`
  is clean and typing at a prompt just works. (Before, `mac exec` always forced a terminal.)
- Exit codes: `mac exec` exits with the command's own code (`mac exec NAME -- false; echo $?` prints 1). ssh failing to connect is
  exit 1 with an explanation (`MAC_GUEST_SSH_FAILED`).
- `fluxctl mac exec --print NAME -- CMD` prints the exact ssh command and runs nothing, to paste into another tool.
- `fluxctl mac attach NAME` follows the console log live (read-only; Ctrl-C detaches, the instance keeps running). It is the same
  stream as `mac logs -f`, with a banner. `mac attach --cmd NAME` follows the output of a command started with `mac run -d`
  (`/tmp/fluxvm-mac-cmd.log` in the guest, over ssh).
- A stopped or still-booting instance gets a plain message with the fix (`MAC_EXEC_NOT_RUNNING`; `fluxctl explain` knows both codes).

**Not verified on a real macOS guest or in a real terminal session** (nothing here can run a Mac): the terminal decision, ssh
arguments, exit-code handling and messages are unit tested with fakes; starting `ssh` and the full-screen behaviour of `-it` are not.
Studio has no button for this yet; copy the command from this page.

## Create, kill, update and the run flags (Docker parity)

Unit-tested with fakes (argument parsing, request bodies, label patches, refusal texts, golden JSON). **Not run against a real daemon or a macOS VM.**
Every refusal ends with `fluxctl explain CODE` (codes below). Studio has no page for macOS instances, so these are CLI only.

### Extra `mac run` and `mac create` flags

| Flag | Effect |
|---|---|
| `--entrypoint CMD` | Replaces the image's `ENTRYPOINT` and, like Docker, drops its `CMD`; `CMD` after `--` still applies. Shell form or a JSON array; `""` clears both. |
| `--pull always\|missing\|never` | Default `missing`: a registry reference (`host/repo:tag`) that is not in the local store is pulled for you (it prints that it does; macOS images are tens of GB). `always` pulls first even when present. `never` uses the local store or fails. A local name (no registry host) cannot be pulled: `always` refuses it and `missing` says so. Not available with `--fleet`. |
| `--health-cmd CMD` | The instance's own healthcheck (run over SSH in the guest, exit 0 = healthy). It is stored on the VM in the label `fluxvm.mac.healthcheck` and wins over the image's `HEALTHCHECK`, so `mac health`, `run --wait-healthy`, compose `service_healthy` and `mac supervise` all use it. |
| `--health-interval D`, `--health-probe-timeout D`, `--health-retries N`, `--health-start-period D` | Tuning (defaults 30s, 30s, 3, 0s). With no `--health-cmd` they tune the image's HEALTHCHECK. Failures within the start period (counted from the last `mac start`/`run`, until the first success) do not count towards the retries. **`--health-probe-timeout` is not Docker's `--health-timeout`**: that name was already taken by `--wait-healthy`'s wait in seconds, so the per-probe timeout has its own name. A Macfile `HEALTHCHECK` now also accepts `--start-period=`. |
| `--no-healthcheck` | No healthcheck for this instance even if the image has one (label value `none`). Cannot be combined with the other `--health-*` flags. |
| `--network`, `--platform` | Not meaningful for a macOS guest (one NAT network; always darwin/arm64). Hidden from `--help`; using them says why and what to do instead (`MAC_FLAG_NOT_APPLICABLE`). |

### `fluxctl mac create [OPTIONS] IMAGE`

Makes the VM and labels it (owner, created time, `--label`, `--restart`, healthcheck) **without starting it**; it prints the name (or
`--json`, see below). `mac start NAME` boots it; until then no restart policy starts it (the label `fluxvm.mac.notstarted`, cleared by
`mac start`). Takes `--name`, `-p`, `-v`, `--label`, `--restart`, `--cpus`, `--memory`, `--pull` and the `--health-*` flags. A macOS VM stores
no command or environment (`mac run` sends them over SSH), so `CMD`, `-e`, `--env-file`, `-w`, `-d`, `--entrypoint` and `--rm` are refused
with a pointer to `mac exec` (`MAC_CREATE_RUN_ONLY`). With `-v` the share is attached at boot and appears under
`/Volumes/My Shared Files`; only `mac run` makes the GUEST symlink. The 2-VM limit applies when it starts, not when it is created.

### `fluxctl mac kill [-s KILL] NAME...`

Ends the VM's runner process at once, with no guest shutdown (`POST /v1/vms/{id}/kill`, a new daemon route over the scheduler's existing
`kill`); the guest may check its disk on the next boot. It marks the instance like `mac stop` does so `unless-stopped` leaves it alone.
Only `KILL`/`SIGKILL`/`9` is accepted for `--signal`: a VM cannot be sent other signals, so `TERM`, `HUP`, ... are refused with the
alternatives (`mac stop`, or `mac exec NAME -- kill -TERM PID` inside the guest) (`MAC_KILL_SIGNAL`). A daemon without the route gives
`MAC_KILL_UNAVAILABLE`. Needs a running instance. **Whether the vz runner exits cleanly and the daemon records `stopped` after a kill is unverified.**

### `fluxctl mac update NAME [--cpus N] [--memory SIZE] [--restart POLICY] [--label K=V] [--rm-label K] [--restart-now] [--json]`

Like `docker update`. `--restart` (`no` removes the policy), `--label` and `--rm-label` apply at once, running or stopped. `--cpus` and
`--memory` are read from the VM's request when it boots, so on a **stopped** instance they are applied at once (the daemon checks them
with the vz admission rules) and take effect at the next `mac start`. On a **running** instance they are not applied: the labels and
policy that were asked for are, the size is listed as needing a restart, and the command exits 1 with `MAC_UPDATE_NEEDS_RESTART` and
the exact `--restart-now` command. `--restart-now` does: mark the instance stopped-by-you (so `mac supervise` leaves it alone), stop
it, save a disk checkpoint `auto-before-update-<unix time>` (`--no-checkpoint` skips it; a checkpoint that cannot be saved, usually
for lack of disk, starts the VM again unchanged and says so), apply the size, start it and restart the health clock. Same VM, same
name, same disk and shared folders. If the start fails the answer names `mac checkpoint restore NAME auto-before-update-<time>`.
The `auto-before-update-*` checkpoints are not pruned (an APFS clone costs little until the disk diverges; `mac checkpoint rm` them).

`--env` is refused (`MAC_UPDATE_ENV_NOT_STORED`): a macOS VM stores no environment; use `mac exec NAME -e K=V -- CMD`. An older daemon
(no `PATCH /v1/mac/instances/{id}`) still takes labels and the policy through `PATCH /v1/vms/{id}`; a size change gets
`MAC_UPDATE_NOT_SUPPORTED` with the stop/commit/rm/create way. Studio: the Edit panel of an instance on the Mac page (same fields, a
"restart now" box, and Copy as fluxctl). **Nothing here has run against a real Mac: that the runner picks up the new request on the
next boot is read from the code, not observed.**

API: `PATCH /v1/mac/instances/{id}` (admin), body `{cpus?, memory_mib?, restart?, labels?: {k: v}, rm_labels?: [k], restart_now?, checkpoint?}`;
the answer is `{ok, name, applied[], pending_restart[], restarted, checkpoint, explain, next, fluxctl}`. `ok` is false when something
is pending a restart.

### JSON

`mac create --json` prints `{"schema_version":1,"name","id","image","status":"created","started":false}`; `mac update --json` prints
`{"schema_version":1,"name","id","restart_policy" (new policy, "no" when cleared, null when not given),"labels" (the user labels set),
"cpus","memory_mib" (asked for, else null),"applied","pending_restart","restarted","checkpoint"}` (the last six keys were added later; keys only get added).
Both are pinned by golden files in `crates/fluxctl/src/mac_json_golden/`.

### Explain codes

`MAC_FLAG_NOT_APPLICABLE`, `MAC_PULL_POLICY`, `MAC_HEALTHCHECK_FLAGS`, `MAC_CREATE_RUN_ONLY`, `MAC_UPDATE_NOT_SUPPORTED`, `MAC_UPDATE_NEEDS_RESTART`, `MAC_UPDATE_ENV_NOT_STORED`, `MAC_KILL_SIGNAL`,
`MAC_KILL_UNAVAILABLE`: `fluxctl explain CODE`.

## Instance commands

All need the daemon (`--server` or a context). `NAME` is the instance name from `mac ps`.

| Command | What it does |
|---|---|
| `mac ps [-a] [-f KEY=VALUE]... [--format T] [-q] [--json]` | Without `-a` only running, paused and starting instances are listed (this changed from "all"). Filters: `name` (substring), `id` (prefix), `status` (`creating`, `running`, `paused`, `stopped`, `failed`), `image`, `label=k` or `label=k=v`. All filters must match (Docker ORs repeats of one key). `--format` takes `{{.Name}}`-style fields (`ID`, `Name`, `Image`, `Status`, `IP`, `Ports`, `Created`, `Restart`, `Project`, `Service`, `Labels`), `{{json .}}`, and a `table ` prefix (tab-separated columns with a header). `-q` prints short ids. |
| `mac inspect [-f TEMPLATE] NAME...` | JSON array (or, with `--format`, one templated line each: [subset](#mac-inspect---format)): state, restart policy, network, ports, cpus, memory, mounts, user labels, disk path, the image's config and the raw VM record. |
| `mac logs [-f] [-t] [--tail N] NAME` | The VM's console log (`GET /v1/vms/{id}/logs`; serial output, not the guest's own logs). `-f` streams until Ctrl-C; `-t` (with `-f`) stamps lines with when they arrived. `--since` is refused: [why](#mac-logs---since-and--t-what-is-and-is-not-possible). |
| `mac events [--since T] [--filter K=V]... [-f] [--json]` | Recorded `mac.*` and `kube.apple.*` events; [details](#mac-events---since-and---filter). |
| `mac exec [-it] [-e] [-w] [-u] NAME [-- CMD]` | Run a command, or a login shell, in the guest over ssh; see [Interactive access](#interactive-access-exec--it-run--it-attach). |
| `mac attach [--cmd] NAME` | Follow the console log live (read-only), or with `--cmd` the output of `mac run -d`. |
| `mac cp SRC DST` | `NAME:/path` is the guest side; either direction; `scp -r`. Between two instances is refused. |
| `mac stats [NAME...] [--json]` | One reading of CPU, memory, disk I/O (`GET /v1/vms/{id}/stats`). CPU is averaged over the VM's life, and there is no live stream. |
| `mac stop`, `start`, `restart NAME...` | `stop` keeps the disk. `start` needs a free macOS slot, and boots an instance made by `mac create`. See also `mac kill` and `mac update` below. |
| `mac pause`, `unpause NAME...` | The daemon supports this for `vz` guests (`/pause`, `/resume`): vCPUs freeze, memory and the macOS slot are kept. |
| `mac rename OLD NEW` | `PATCH /v1/vms/{id}`. |
| `mac wait NAME [--timeout S]` | Blocks until the instance is `stopped` or `failed` and prints which. There is no exit code to report; `failed` exits non-zero. |
| `mac top NAME [-- PS_ARGS]` | `ps -axo pid,user,%cpu,%mem,etime,command` over SSH, or `ps` with your arguments. |
| `mac port NAME [GUEST_PORT]` | The published ports (`-p`). |
| `mac prune [--orphaned] [-f]` | Deletes stopped and failed instances; with `--orphaned`, also non-running ones whose image name is no longer in the store. Never a running or paused one. Asks unless `-f` (and refuses when stdin is not a terminal without `-f`). |
| `mac reconcile [--dry-run]` | Starts instances their restart policy says should be running. |

Errors from the daemon are explained: a **409** that mentions the macOS guest limit says both slots are taken and which commands free
one; a **409** "start it first" says the instance is not running; any other 409 is reported as a state conflict; a **503** says the
Mac's login session is locked (a headless host needs an auto-login user with the lock off); a **404** says the VM is gone.

## Image tools

| Command | What it does |
|---|---|
| `mac history IMAGE` | One row per recorded Macfile step, newest first, with the time the build finished. An image that was imported or pulled shows that as its only row. Also `mac image history`. |
| `mac image inspect IMAGE` | JSON: directory, digest, source, other names for the same files, per-file size and allocated bytes, and the config (ENV, WORKDIR, LABEL, EXPOSE, CMD, ENTRYPOINT, HEALTHCHECK, history). |
| `mac save IMAGE --output FILE`, `mac load -i FILE [NAME:TAG]` | A tar of `disk.raw`, `hardware.bin`, `auxiliary.bin`, `config.json` and a `manifest.json` naming the reference. `load` lists the archive first and refuses unexpected or unsafe entries, then unpacks into a scratch directory, so a bad archive leaves nothing in the store. The disk is archived at its **apparent** size (a sparse 64 GiB disk makes a 64 GiB tar); there is no compression. Both shell out to `tar`. Also `mac image save|load`. |
| `mac commit INSTANCE NAME:TAG [-m MSG] [-a AUTHOR]` | Stores a **stopped** instance's disk, hardware model and auxiliary storage as a template (APFS clones), with the source image's config and a history line. The daemon must run on this Mac. |
| `mac image prune [--all] [--dry-run] [-f]` | Removes store directories no name uses (a crashed pull) and scratch directories older than an hour. `--all` also removes images no instance uses and needs the daemon. |
| `mac system df [--json]` | Images (names, in use, size, reclaimable), instances (running, size, reclaimable) and unreferenced directories. Sizes are **allocated** bytes (`st_blocks`), because `disk.raw` is sparse; APFS clones share blocks, so deleting can free less than shown. Instance sizes count only disks this Mac can read. `mac images` still shows apparent size. |
| `mac system info`, `mac system version` `[--json]` | [`docker info` and `docker version`](#mac-system-info-and-mac-system-version). |
| `mac system prune [--all] [--volumes] [--dry-run] [-f] [--json]` | `mac prune --orphaned`, plus `image prune`; with `--all`, also unused images; with `--volumes`, also volumes no remaining instance uses. Always prints what goes, how much it frees and the Mac's free space before and after; `--dry-run` stops there. See "Volumes, ports and networks". |

## Inspecting and observing: `--format`, `--since`, `--filter`, `system info`

Everything here is unit-tested with fakes (sample daemon records, a fake event list, fixed clocks); **none of it was run against a
real daemon or a real Mac**. No flag or set-up is needed for any of it; each command works as before when the new options are left
out. Studio has no `fluxctl mac` page, so there is nothing to click for these (see "Studio" below).

### `mac inspect --format`

```
fluxctl mac inspect --format '{{.State.Status}}' build-1
fluxctl mac inspect -f '{{.Name}} {{.Network.IP}} {{index .Labels "team"}}' build-1 build-2
fluxctl mac inspect -f '{{range .Network.Ports}}{{.Host}}->{{.Guest}} {{end}}' build-1
fluxctl mac inspect -f '{{json .State}}' build-1
```

One output line per instance. The template is run against the same document as the default JSON (`mac inspect NAME`, without the
array), so the way to find a field is to look at that JSON. Field names ignore case and underscores: `.State.Status`,
`.state.status` and `.Resources.MemoryMiB` (the document's `memory_mib`) all work.

The supported subset of Go templates, and nothing else:

| Syntax | Meaning |
|---|---|
| `{{.A.B}}` | a field path; `{{.}}` is the current value |
| `{{json X}}` or `{{X \| json}}` | compact JSON of a value |
| `{{index X "key"}}`, `{{index X 0}}` | a key of an object, or a position in a list; more keys go deeper |
| `{{range X}}...{{end}}` | repeat for each element of a list (or each value of an object, in key order); `.` is the element |
| `"text"`, whole numbers | arguments to `index` |
| `\n`, `\t` | in the text between actions |

A missing or null value prints `<no value>`; text prints as is; numbers and booleans print as JSON; an object or list prints as
compact JSON (Go would print `map[...]`). `if`, `with`, `printf`, `len`, variables, `{{-` trimming and every other function are
refused with a message that lists the supported syntax and names `fluxctl explain MAC_TEMPLATE_SYNTAX`. The template is checked
before the daemon is contacted. `range` over text or a number, and `index` past the end of a list, are errors too.

### `mac images --format` and filters

`mac images --format T` uses the same engine over one document per image: `Reference`, `Repository`, `Tag`, `ID` (first 12 digits
of the digest), `Digest`, `Size` (allocated, human), `SizeBytes`, `AllocatedBytes`, `Source`, `Labels` (`{{index .Labels "k"}}`),
`AddedUnix`, `LastUsedUnix`; `{{json .}}` prints the record. A `table ` prefix adds a header row and aligns the columns (columns are
separated by `\t`; a bare `table` shows repository, tag, id and size). `--format` cannot be combined with `--json`, `--usage` or
`--fleet`.

`mac images --filter` already existed (`label=`, `reference=` with `*`, `source=`). `mac ps --filter` already had `name`, `id`,
`status`, `image` and `label`; it now also accepts `project`, `service`, `ip` and `publish=PORT` (host or guest side). A wrong key
or value says which keys exist and names `fluxctl explain MAC_FILTER_INVALID`.

### `mac events --since` and `--filter`

```
fluxctl mac events --since 1h
fluxctl mac events --since 2026-11-21 --filter type=mac --filter event=started
fluxctl mac events -f --filter name=ci-1
fluxctl mac events --since 30m --filter label=kairon.zyvor.dev/pool=ios --json
```

`--since` is a duration back from now (`30s`, `10m`, `2h`, `1d`, `1h30m`; units `s m h d w`), an RFC 3339 timestamp or a date
(midnight UTC); it includes events at exactly that time. `--filter KEY=VALUE` is repeatable and every filter must match:

| Key | Matches |
|---|---|
| `type` | `mac` (instances and slots) or `kube` (k3s nodes and clusters) |
| `event` | the whole event name (`mac.instance.started`), its last part (`started`) or a leading part (`mac.instance`) |
| `name` | part of the event's `name` field, or of its `node` field for k3s node events |
| `image`, `project`, `status`, `cluster` | part of that field of the event |
| `label` | `KEY` or `KEY=VALUE`; events carry only the `kairon.zyvor.dev/*` labels, as fields of the same name |

They work with `-f` (follow: new events are filtered; `--since` applies to the history printed first) and with `--json` (the same
one-object-per-line output, restricted). With filters, up to 10000 recent events are fetched from the daemon and filtered here, then
the newest `--limit` are shown. A bad time names `fluxctl explain MAC_TIME_INVALID`.

### `mac logs --since` and `-t`: what is and is not possible

`mac logs` shows the VM's console log: raw serial output. The daemon records **no time per line**, and fluxctl does not invent one.
So:

- `mac logs --since X` is **refused**, with a message that says why (a bad `X` is reported as a bad time first). Use `--tail N`, or
  `mac events --since 1h --filter name=NAME` for what happened to the instance, or `mac exec NAME -- log show --last 10m` for the
  guest's own timestamped logs.
- `mac logs -f -t NAME` works: each new line is prefixed with the time **fluxctl received it** (UTC, milliseconds), which is when the
  guest wrote it to within the network delay and the daemon's 300 ms polling. The `--tail` lines already in the log are printed first
  with `-` in the time column, because their time is unknown. Following asks the daemon for one more line of history and drops it when
  it arrives at once, since it is the last line already printed; a VM that writes two identical lines in that first second could lose
  one. `-t` without `-f` is refused, because a receipt time for old lines would be a lie.

Both refusals name `fluxctl explain MAC_LOGS_NO_TIMESTAMPS`.

### `mac system info` and `mac system version`

`mac system info [--json]` is the `docker info` of `fluxctl mac`: fluxctl and daemon versions, the Mac (macOS version and build,
chip), instances in use out of 2, images and their disk, instance disks, what could be reclaimed, free disk, the store path and the
`mac doctor` summary with each problem and its fix. It reuses the `mac doctor` checks and the `mac system df` numbers and adds only
the daemon's version (read from `GET /v1/openapi.json`). It needs no flags and no daemon: without one, the daemon line says how to
connect (`--server`, `FLUXVM_URL`, a context), instances and instance disks show as unknown, and everything local is still shown.
Like `mac doctor` (whose checks it runs), on a Mac it creates the store directory when it is missing; the disk figures read the
store only when it exists. On a machine that is not a Mac it says so, and the doctor summary shows the platform failure with its fix.

`mac system version [--json]` is `docker version`: client (fluxctl version, OS, architecture) and server (daemon version and address,
or why there is none). `--json` documents of both carry `schema_version` 1; golden files `system-info.json` and
`system-version.json` pin them under `crates/fluxctl/src/mac_json_golden/`. Keys are only added within a version.

### `docker export`: not built

`mac export NAME` is **not provided**. A stopped instance cannot be exported as a plain disk: a macOS guest boots from
`disk.raw`, `hardware.bin` and `auxiliary.bin` together, so a tar of the disk alone would not start anywhere. A tar with all three
plus the config is exactly what `mac commit NAME IMAGE:TAG` followed by `mac save IMAGE:TAG --output FILE.tar` already produces
(and `mac load` reads back), so an `export` would duplicate `save`.

### Studio

Studio has no page for `fluxctl mac` instances, images or events, so none of the above has a Studio counterpart and none was added. The
generic Machines page already has a Console tab and Overview shows live events for every VM; those were not changed or checked here.
The exception is checkpoints: the machine's Protect tab does them (see [Checkpoints](#checkpoints-save-a-state-come-back-to-it)).

## Macfile reference

One directive per line, `#` comment lines on their own line, a trailing `\` continues a line, and every parse error names the line
(`Macfile line 7: ...`). Directive names are case-insensitive.

| Directive | Meaning |
|---|---|
| `FROM name:tag` | The template to clone. `FROM IPSW latest` and `FROM IPSW /abs/path.ipsw` are parsed and validated, and `build` refuses to run them: see [IPSW installs](#ipsw-installs-from-ipsw). |
| `USER name` | The SSH user for later `RUN`/`COPY`/`ADD`, and the image's default user for `run`. |
| `RUN cmd` | Run over SSH, verbatim. |
| `COPY src dst` | `scp -r` from the build context. A relative `dst` is under the current `WORKDIR`. Honors `.macignore`. |
| `ADD src dst` | Like `COPY`; an `http://` or `https://` source is downloaded **in the guest** with `curl -fsSL` (no archive unpacking, unlike Docker). |
| `ENV K=V ...` or `ENV K some value` | Exported before each later `RUN`, and recorded in the image so `run`/`exec` set it too. |
| `ARG NAME[=default]` | A build variable (`--build-arg NAME=V`). Exported to later `RUN` steps, expanded as `$NAME`, `${NAME}`, `${NAME:-d}` in later `ENV`, `LABEL`, `WORKDIR`, `USER`, `COPY`, `ADD`, `EXPOSE`; `\$` is a literal `$`. Not recorded in the image. A value you did not give and that has no default is empty. |
| `WORKDIR dir` | `cd` before each later `RUN`; relative paths stack. Recorded; `run` and `exec` start there. |
| `LABEL k=v ...` | Recorded in the image config (shown by `image inspect`). |
| `EXPOSE port[/tcp\|udp] ...` | Recorded only. It does not publish anything: use `run -p`. |
| `CMD ...`, `ENTRYPOINT ...` | Exec form (`["a","b"]`) or shell form. Recorded; `run IMAGE` with no `--` runs `ENTRYPOINT` then `CMD`, and words after `--` replace `CMD`. |
| `HEALTHCHECK [--interval=D] [--timeout=D] [--retries=N] CMD ...` or `NONE` | Recorded in the image and **run over SSH** by `mac health`, `run --wait-healthy`, `compose` `condition: service_healthy` and `mac supervise` (see [Operations](#operations-health-restart-ttl-verify)). |
| `SNAPSHOT tag` | Parsed; `build` refuses it. |

`ENV`, `ARG` and `WORKDIR` are folded into each `RUN` as `export K='v'; cd 'dir' && cmd`, so `mac build --dry-run` shows exactly what
will run. The build writes `config.json` into the new template's directory.

`.macignore` (in the build context, same rules as `.dockerignore`): one pattern per line, `#` comments, patterns relative to the
context root (`*.md` matches only at the top; `**/*.md` at any depth), `*` and `?` stay inside one path part, `**` spans
directories, a leading `!` re-includes, the last matching pattern wins, and a pattern that matches a directory excludes what is
under it. With a `.macignore`, a `COPY` of a directory is sent from a filtered scratch copy (deleted afterwards); a source that is
itself ignored is an error. When there is no `.macignore`, a `.dockerignore` in the context is read with the same rules, so a Docker
project's ignore file works unchanged (`.macignore` wins if both exist).

### Docker-style build flags: secrets, SSH, labels, platform, build-arg checks

All of these work with no set-up; each one is checked **before** the build instance is created, and a failure says what to run next
(`fluxctl explain CODE`).

| Flag | What it does |
|---|---|
| `--secret id=NAME,src=FILE` or `id=NAME,env=VARIABLE` | Makes a secret available to `RUN --mount=type=secret,id=NAME CMD` steps. The file is copied into the guest as `/private/tmp/fluxvm-secrets/NAME` (mode 600, directory mode 700; or the mount's `target=/abs/path`), the variable `FLUXVM_SECRET_NAME` holds that **path** (never the value), and the file is removed before the step ends and before the build cache takes its disk snapshot. If it cannot be removed the build fails and the image is not stored (`MAC_SECRET_CLEANUP`). The value is not in the plan, `--dry-run`, the cache key (which secrets a step can read is, their content is not, as in Docker), the image history or config, or any message from `fluxctl`; error text is also run through a redaction of the secret values as a last guard. `env=` values are written to a private (0600) temporary file on this Mac and deleted when the command ends. A step that mounts a secret you did not pass, or a missing file or variable, is refused up front (`MAC_SECRET_MISSING`); a `--secret` no step uses is a warning and is not sent. |
| `--ssh default` | Forwards this shell's SSH agent (`ssh -A`) to `RUN --mount=type=ssh CMD` steps only. No private key is copied into the guest. Needs a running agent (`SSH_AUTH_SOCK`; `MAC_SSH_AGENT_MISSING`). Only `default` is accepted. Add the host's key in the step (`ssh-keyscan github.com >> ~/.ssh/known_hosts`) because the guest has none. |
| `--label KEY=VALUE` | Adds an image label; overrides a `LABEL` of the same key in the Macfile. `--dry-run` prints the labels. |
| `--platform darwin/arm64` | Accepted for Docker scripts (`darwin/arm64`, `macos/arm64`, `arm64`); any other value is refused (`MAC_PLATFORM_UNSUPPORTED`) because macOS guests are Apple silicon only. |
| `--build-arg K=V` checks | A warning (the build still runs) when the Macfile has no `ARG K` (with a spelling hint for `VERSON` vs `ARG VERSION`), when an `ARG` has no default and no value but is used in a later `$NAME`, and when the name looks like a credential (`TOKEN`, `PASSWORD`, `SECRET`, `API_KEY`...) with the advice to use `--secret`. `MAC_BUILD_ARG_UNUSED`. |
| `--target STAGE` | **Not supported.** A Macfile has one `FROM`, so there are no stages. The flag exists only to answer in plain English (`MAC_BUILD_TARGET_UNSUPPORTED`) with the two-Macfile alternative. |

Limits, stated plainly: output a `RUN` step prints in the guest goes straight to your terminal and is **not** filtered, so a command
that prints the secret (`cat` of the file) shows it; do not do that. Build args are still visible in `--dry-run` and in the plan's
`export` text; use `--secret` for credentials. A changed secret does not rebuild a cached step (the same as Docker): use `--no-cache`.
`RUN --mount` types other than `secret` and `ssh` (`cache`, `bind`, `tmpfs`) are refused. Studio has no page that builds Mac images;
the MCP tool `mac_build_dry_run` takes `platform` and reports the build-arg warnings, and the same command line works from a
terminal. Compose `build:` has no `target`, `secrets` or `ssh` keys.

## Compose

```yaml
# mac-compose.yaml
name: ci
services:
  db:
    image: macos-postgres:16
    ports: ["5432:5432"]
    restart: unless-stopped
  tests:
    image: macos-xcode:15
    depends_on: [db]
    command: ["xcodebuild", "test"]
    environment: {DB_PORT: 5432}
    volumes: ["./src:/src:ro"]
    working_dir: /src
    cpus: 4
    mem_limit: 8g
```

**At most 2 services: a Mac runs at most 2 macOS VMs at once (Apple's licence).** A file with more is refused before anything is
created, and the two must also fit beside any other macOS VM running on the host (otherwise the daemon answers 409 and `up` stops
at that service, naming what it already started).

- `mac compose up [-f FILE] [-p PROJECT] [--wait S] [--dry-run]` creates `PROJECT-SERVICE` instances in `depends_on` order (ties
  alphabetical). A service that has dependents, a command or volumes is waited for on SSH before the next starts. `command` runs
  in the background in the guest, like `run -d`. `--dry-run` prints the order and checks the images exist; it needs no daemon.
- `mac compose down` deletes the project's instances, dependents first. `mac compose ps [-a]` lists them.
- File: `mac-compose.yaml`, `.yml` or `.json` in the directory, or `-f`. The project name is `-p`, else `name:`, else the directory.
- Keys: `image`, `command` (string = shell text, list = argv), `environment` (map or list), `env_file`, `ports` (`HOST:GUEST`),
  `volumes` (`./host:/guest[:ro]`, relative to the file), `working_dir`, `depends_on` (list, or map with a `condition`),
  `labels`, `restart`, `cpus`, `mem_limit`, `user`, `build`, `profiles`. **An unknown key (`networks`, `deploy`, ...) is an error**, not skipped.
- `depends_on` orders the start and waits for SSH. With `condition: service_healthy` it also waits (up to 10 minutes) for the dependency's
  image HEALTHCHECK to pass; the dependency's image must have one (`up --dry-run` checks it), otherwise it is an error. Any other
  condition means "started".
- `build: ./dir` or `build: {context, dockerfile, args}` (`dockerfile` names the Macfile, `args` are `--build-arg`s): `up` runs `mac build`
  for a service whose image is not in the store, or for every such service with `up --build`. Without `image:` the result is tagged
  `PROJECT-SERVICE:latest`. Paths are relative to the compose file.
- `profiles: [debug]` makes a service optional: `up --profile debug` (repeatable) or `COMPOSE_PROFILES=debug,x` selects it. The 2-VM
  refusal counts the **selected** services and says so; `down`, `ps`, `logs`, `exec` and `restart` consider every profile.
- `up --no-build` never builds: a `build:` service whose image is missing is an error (`--build` and `--no-build` conflict).
  `up --wait-healthy` waits after the last service started until every service whose image has a HEALTHCHECK is healthy (services
  without one count as up once SSH answered). `--health-timeout SECS` (default 600, 0 = forever) bounds that wait and the
  `service_healthy` dependency waits.
- `depends_on` conditions are checked when the file is read: only `service_started` and `service_healthy` exist.
  `service_completed_successfully` is refused (a macOS service is a VM that keeps running), as is any unknown condition, and a
  dependency on a service in a profile that is not enabled says which `--profile` to pass.
- Each instance `up` creates carries `fluxvm.mac.owner` and `fluxvm.mac.created` (never an expiry), so `mac reap --owner NAME` and
  `mac ps` can tell whose project it is.
### Compose: watch, scale, one-shots and a fleet

Not run against a real Mac; unit-tested with fakes.

- **`fluxctl mac compose watch`** starts the project (unless `--no-up`), then looks at the files named in each service's `develop: watch:` every 2 seconds. A rule is `{path, action, target, ignore}`: `sync` copies changed files to `target` in the guest, `sync+restart` also runs the service's command again, `restart` only runs it again, `rebuild` builds the image again and recreates the instance. It also applies `restart:` policies, because the daemon never restarts a macOS VM by itself; `restart: on-failure:3` means three restarts, then the instance stays down and the message says how to start it by hand.
- **`up --scale web=2`** runs two copies (`PROJECT-web-1`, `-2`). On one Mac the project may hold at most 2 VMs at the same moment; a fixed host port in `ports:` cannot be scaled.
- **`depends_on` conditions**: `service_started`, `service_healthy` (waits for the dependency's `healthcheck:` or its image HEALTHCHECK, probed over SSH in the guest) and `service_completed_successfully` (the dependency's command runs to the end; on success its VM is stopped, so it frees its slot and the limit counts VMs at the same moment, not services). A failed one-shot stays up for `mac logs` and nothing that waits for it starts.
- **`profiles:` / `--profile` / `COMPOSE_PROFILES`** select services (a service without profiles always runs).
- **`up --fleet hosts.yaml`** spreads the project over the Macs of a `mac-fleet` inventory (2 macOS VMs per Mac). `up --dry-run --fleet hosts.yaml` prints, for every service, the Mac it would use and why, and why every other Mac was not used. `placement: {host, labels, ac_only, disk_gib}` narrows the choice. `ps --fleet` and `down --fleet` act on every Mac. Services find each other through `COMPOSE_<SERVICE>_HOST`. In fleet mode `environment:` values travel on ssh stdin to a 0600 temp file that is sourced and removed (never in the command line); bind-mount `volumes:`, `secrets:` and `configs:` are refused, because the other Macs cannot see this Mac's folders. `on-failure:N` is not counted on a fleet Mac.

### Docker Compose parity

Everything works with no flags or set-up: `fluxctl mac compose config` is the first thing to run on a file. Nothing below was run
against a real Mac, daemon or guest; the parsing, plans, request bodies and error texts are unit tested with fakes.

| Docker Compose | Here |
|---|---|
| `up --wait` | `up --wait` (same as `--wait-healthy`). **Change:** `--wait` used to require a number (SSH seconds); `--wait 300` still works and means that and also waits for healthchecks. `--wait-timeout SECS` is a second name for `--health-timeout`. |
| `config [-q] [--format json]` | `compose config` prints the file as used: `${VAR}` filled in, `.env` read, `include`/`extends` merged, `env_file` merged into `environment`, only the selected profiles (`--profile`, `COMPOSE_PROFILES`). `--quiet` only validates (exit status), `--services` lists the start order. No daemon needed. |
| `stop`, `start` | `compose stop [SERVICE...]` (dependents first) and `compose start [SERVICE...]` (dependencies first). `start` is refused up front, naming the fix, when it would need more than the 2 macOS slots. |
| `images`, `pull`, `push` | Table of service, image, id and size from the local store; `pull` fetches the `image:` of each service (a service with only `build:` is skipped); `push` pushes them (each `image:` must name a registry; the licence check, `--sign` and `--i-have-checked-the-licence` are those of `mac push`). No daemon needed. |
| `ls [-a] [-q] [--format json]` | `compose ls [-a] [-q] [--json]`: projects on this Mac, `running(2)` / `exited(1)`. There is no config-file column (the daemon does not record it). |
| `run [--rm] [-e] [-w] [-u] SERVICE [CMD]` | `compose run`: a **new** VM from the service's image, the command in the foreground over SSH. It needs a free macOS slot, so with 2 VMs running it is refused with a message naming `compose stop SERVICE`. It does not start dependencies (they must already run) and does not publish ports. Without `--rm` the one-off VM stays (labelled into the project; `compose down` removes it). |
| `top`, `cp` | `compose top [SERVICE...] [-- ps-args]` and `compose cp SERVICE:/path LOCAL` (either direction), over SSH like `mac top` / `mac cp`. |
| `extends` | `extends: other` or `extends: {service, file}`. Merge rules as Docker: scalars and `command` replaced; `environment`, `labels`, `depends_on`, `build` merged by key; `ports`, `profiles`, `env_file`, `secrets`, `configs` appended; a volume with the same guest path replaces the base one. Chains work; loops are an error. |
| `include` | `include: [a.yaml]` or `- path: a.yaml`. Relative paths inside an included file are relative to that file. A name defined twice is an error. The options `project_directory` and `env_file` are refused with a message. |
| `secrets`, `configs` | Only `file:` sources. The file is copied (mode 0444, in a 0700 staging directory under the temp directory) and shared **read-only** into the guest. Default place: `/private/var/run/secrets/NAME` and `/private/var/run/configs/NAME` (macOS has no `/run` and a read-only root); `target:` may be another absolute path (linked, needs sudo in the guest as `volumes:` do) or relative to those directories. **Refused with a plain message:** `external`, `environment`, inline `content`, and `uid`/`gid`/`mode` (the share is read-only as it is). The staged copy lives in the temp directory, so after a reboot `compose down` then `up` again. |
| `env_file` | A path, a list of paths, or a list with `- path: X` / `required: false` (a missing optional file is skipped). `format:` other than `raw` is refused. |
| `${VAR}`, `.env` | `${VAR}`, `${VAR:-d}`, `${VAR-d}`, `${VAR:+x}`, `${VAR:?msg}`, `$$`; values come from the environment, then a `.env` next to the compose file. **Differences from Docker:** a bare `$VAR` is left alone (so a guest `command: echo $HOME` keeps working) and an unset `${VAR}` is empty without a warning. |
| `x-` keys, `version`, `<<` | `x-` extension keys, YAML anchors with `<<` merge, and an obsolete `version:` are accepted. |

Not implemented, and an error that names the key: `networks`, `deploy`, `healthcheck:` (use the image's HEALTHCHECK), `extends`/`include` options not listed,
`build` keys other than `context`/`dockerfile`/`args`, and every other Docker-only key. `compose ps/ls/stop/start/run` count against the 2-VM limit.
A failure prints a `fluxctl explain` code: `MAC_COMPOSE_KEY_UNSUPPORTED`, `MAC_COMPOSE_VARIABLE`, `MAC_COMPOSE_INCLUDE`,
`MAC_COMPOSE_FILE_MISSING`, `MAC_COMPOSE_NOT_UP` (and `MAC_SLOT_LIMIT` for the slot refusals).

**Studio:** there is no Studio page for macOS instances or compose projects today (the pages are for Linux VMs, clusters and
the node), so nothing was added there; the CLI is the only surface.

- `mac compose logs SERVICE [--tail N] [-F]` (the VM's console log), `mac compose exec SERVICE [--user U] -- CMD` (over SSH) and
  `mac compose restart [SERVICE...]` act on the project's instances. `-f FILE` and `-p/--project-name NAME` work on every subcommand.

## Shell completions

`fluxctl completions bash|zsh|fish|elvish|powershell` is generated from the command tree, so every `mac` subcommand and flag above
is included; there is no separate `mac` script. Every command has `--help`, and the ones with several options show examples
(`mac run --help`, `mac ps --help`, `mac logs --help`, `mac cp --help`, `mac compose --help`).

## Registry credentials

`pull` and `push` read `FLUXVM_REGISTRY_USER` and `FLUXVM_REGISTRY_PASSWORD` (a missing password is sent as empty). With no user set, no
credentials are used.

```
export FLUXVM_REGISTRY_USER=alice
export FLUXVM_REGISTRY_PASSWORD=...        # a token, not an account password
fluxctl mac import ~/templates/macos-15 macos-xcode:15
fluxctl mac push macos-xcode:15 ghcr.io/acme/macos-xcode:15
fluxctl mac pull ghcr.io/acme/macos-xcode:15
fluxctl mac tag ghcr.io/acme/macos-xcode:15 macos-xcode:stable
fluxctl mac images
fluxctl mac rmi macos-xcode:stable
```

### Saved logins: `mac login`, `mac logout`, `mac logins`

```
fluxctl mac login ghcr.io -u alice                       # asks for the token without echo
printf %s "$TOKEN" | fluxctl mac login ghcr.io -u alice --password-stdin
fluxctl mac logins                                       # registries and user names, never passwords
fluxctl mac logout ghcr.io                               # or: fluxctl mac logout --all
```

`mac pull`, `push`, `verify`, `sign` and `search` use the saved login of the registry they talk to, with no flags. The environment
variables above still win when `FLUXVM_REGISTRY_USER` is set. A login is only ever sent to the registry it was saved for.

- The password comes from stdin (`--password-stdin`, first line) or a prompt with echo off. There is no password argument, so it never
  lands in shell history or `ps`. It is not printed in any message, `--json` output or error; the tests grep those paths for it.
- Stored in `~/.config/fluxvm/mac-registry-auth.json` (override: `FLUXVM_MAC_REGISTRY_AUTH`), mode 0600, in clear text like
  `~/.docker/config.json` without a credential helper. fluxctl refuses to read or write the file while a group or other permission bit
  is set (`chmod 600` it); until then pulls and pushes go out anonymously, with a warning. Use an access token, not an account password.
- `login` makes one `GET /v2/` (following a Bearer challenge to the token endpoint). A refusal saves nothing and exits 1
  (`MAC_REGISTRY_LOGIN_FAILED`). If the registry cannot be reached the login is saved anyway and the output lists, under
  `NOT verified:`, that it was not checked. Even a verified login does not prove it may pull or push a given repository.
- `logout` does not revoke the token at the registry; do that in the registry's settings if it leaked.
- `mac-fleet templates distribute` forwards the environment pair, or else the saved login of the template's registry, to each Mac (see [mac-fleet-ops.md](mac-fleet-ops.md#registry-login-on-the-macs)).
- Studio has no page for Mac instances or registry credentials, so there is no UI for this yet.

### `fluxctl mac fork NAME -n N [--name-prefix P] [--start] [--json]`

Creates N instances from a **stopped** instance's disk. Like `mac commit`, the instance's disk, hardware model and auxiliary storage are
cloned (APFS copy-on-write, instant on the same volume, almost no space until the copies diverge) into one hidden store image
`forked-NAME-XXXXXX:latest`; like `mac run`, each fork is then created from that image through the daemon, so it has its own VM record
and name (`PREFIX-1` ... `PREFIX-N`, default prefix `NAME-fork`) and the labels `fluxvm.mac=instance` and `fluxvm.mac.forked-from=NAME`. The
fork gets the source's vCPU count and memory when the daemon recorded them.

- **A fork is a cold boot of the cloned disk.** A saved running state cannot be restored on macOS, so no memory or running process is
  carried over. Things inside the guest (hostname, ssh host keys, accounts) are the source's; change them if forks must differ.
- Forks are created **stopped**. `--start` boots them and needs a free macOS slot for each (2 per Mac); if there are not enough it stops
  before creating anything with `MAC_FORK_SLOTS` (stop one, start forks one at a time, or use the fleet). Slots held by VMs that are
  not `mac` instances are not counted by this check; the daemon's own 409 is explained if it comes.
- A running (or paused, starting) source is refused with `MAC_FORK_SOURCE_RUNNING`: `fluxctl mac stop NAME` first.
- The hidden image stays while forks use it (their `mac ps` IMAGE column shows it). `fluxctl mac rmi` it after the forks are gone, or
  let `mac image prune --all` remove it once no instance uses it.
- Needs the daemon on the Mac that owns the source instance (it reads the instance's disk path), like `mac commit`.
- Whether the daemon gives each fork a fresh MAC address and machine identity was not verified; it is created through the same API as `mac run`.

## Checkpoints: save a state, come back to it

A checkpoint is a named saved state of one instance. It is the daemon's existing snapshot (`<workspace>/snapshots/<name>/`:
`fluxvm_apple::snapshot_save` for a running guest, `snapshot_save_disk` for a stopped one), with friendlier commands on top and
the same calls Studio makes (`POST /v1/vms/{id}/snapshot`, `GET .../snapshots`, `DELETE .../snapshots/{tag}`,
`POST .../restore`). Nothing needs turning on.

```sh
fluxctl mac checkpoint ci warm              # save ci as "warm" (no name: ckpt-<UTC time>)
fluxctl mac checkpoint ci warm --disk-only  # disk only: no memory, no unlocked screen needed
fluxctl mac checkpoints [NAME] [--json]     # kind, size and age, newest first
fluxctl mac run --from-checkpoint ci:warm   # come back to it (add -- CMD to run something in the guest)
fluxctl mac checkpoint restore ci warm      # the same, without the run
fluxctl mac checkpoint rm ci warm
fluxctl mac diff ci warm [OTHER]            # how much of the disk differs from now, or from another checkpoint
```

- **What is saved.** A running instance is saved with its memory and devices plus an APFS clone of its disk (it pauses for a
  moment, then carries on), so coming back resumes it where it was, with no boot. A stopped instance, or `--disk-only`, saves the
  disk and firmware only, and coming back boots it (a macOS boot takes minutes). The list shows which each one is.
- **Cost.** The disk part is an APFS clone: nearly free until the instance and the checkpoint drift apart. The sizes shown are the
  files' full sizes, not the extra space used. A checkpoint with memory also writes about the guest's RAM.
- **Names.** 1 to 128 letters, digits, `.`, `_`, `-`; unique per instance. A checkpoint belongs to the instance it was taken from.
  `--from-checkpoint` takes `NAME:TAG`, or just `TAG` when exactly one instance has it. It brings an existing instance back, so it
  refuses an IMAGE, `--name`, `-p`, `--rm` and `--fleet`. An instance cannot be called `rm` or `restore` in `checkpoint NAME`.
- **The safety net.** A restore first saves where the instance is now as `auto-before-restore-<UTC time>` (with memory when it is
  running and the runner can, else the disk) and the answer says how to go back:
  `fluxctl mac checkpoint restore NAME auto-before-restore-...`. Only the newest 3 `auto-` checkpoints are kept; one you named is
  never pruned. If the safety checkpoint cannot be saved (usually no room) the restore is refused and nothing changes
  (`MAC_CHECKPOINT_SAFETY`); `--no-safety-checkpoint` skips it. This lives in the daemon (`restore_vm_snapshot_safe`, and the
  `safety_checkpoint` request field of `/restore`, default true), so Studio gets it too. Other callers of the restore (agent
  sessions, the self-control socket) are unchanged. `mac update --restart-now` saves its own `auto-before-update-<time>` checkpoint
  before a restart that changes the size.
- **Host safety.** Before a checkpoint of a macOS instance the daemon checks free disk (at least 4 GiB, plus the guest's memory size
  when memory is saved; an unreadable figure never blocks) and refuses in plain words with the next step (`MAC_DISK_FULL`);
  the runner's own `save-support` check still refuses a device set that cannot be saved, with its reason
  (`MAC_CHECKPOINT_NOT_SAVABLE`; `--disk-only` always works). A restore keeps the checks it had: the unlocked-session check
  (`FLUXVM_SKIP_SESSION_CHECK`) and the macOS-build check, so saved memory from before a macOS update cold-boots the disk
  instead of failing (`FLUXVM_RESTORE_ON_OS_UPDATE=refuse` makes that an error).
- **`mac diff`.** Compares the checkpoint's disk file (and the other saved files) with the instance's current ones, 1 MiB block by block
  of the disk image: it says how much changed, not which files, and does not compare memory. It reads both files once, so a large
  disk takes a while, and it must run on the Mac whose daemon owns the instance (`MAC_CHECKPOINT_NOT_LOCAL`). A running guest keeps
  writing, so stop it for an exact answer.
- **Studio.** The machine's **Protect** tab lists checkpoints (memory or disk only, automatic ones marked, size, age), saves one
  (a "disk only" box for a running machine; a stopped one saves its disk), starts from one and deletes one. After a restore the
  toast names the safety checkpoint. It works for any machine the daemon snapshots, so it applies to macOS instances.
  There is no Studio view of `mac diff`.
- `mac build` still refuses `SNAPSHOT`: a saved running state is bound to this Mac and its macOS build and cannot move to
  another one.

## Preflight: `fluxctl mac doctor [--json]`

Read-only checks of the limits listed under "Hard limits", so they fail early with a fix instead of late with a 409, a 503 or a
20 GB copy. It creates the store directory if missing (to test that it is writable) and changes nothing else. The exit status is 1
when any check is `FAIL`; a `warn` does not fail it. Each non-ok line is followed by `fix: ...`.

| Check | Fails or warns when |
|---|---|
| apple silicon, macOS version | not arm64 / older than macOS 14 (FAIL) |
| store location | the store directory cannot be created or written (FAIL); shows whether `FLUXVM_MAC_STORE` set it |
| free disk | under 25 GiB free on the store's volume (FAIL), under 80 GiB (warn) |
| store filesystem | not APFS (FAIL: `import` and every clone become full copies) |
| clone volume | the store and the daemon's instance directory are on different volumes (FAIL). The instance directory comes from `FLUXVM_STATE_DIR`, else from an existing instance's workspace; with neither it warns |
| login session | the screen is locked or nobody is on the console (FAIL: the daemon answers 503) |
| daemon | no address configured (warn), or `/healthz` unanswered (FAIL). Reads `--server`, `FLUXVM_URL` or the current context |
| macOS VM slots | 2 of 2 `fluxctl mac` instances running or paused (warn: `run` would answer 409; use `--queue`). A macOS VM that `fluxctl mac` did not create also holds a slot and is not counted here |
| ssh key | no `~/.ssh/id_*.pub` and no generated `~/.ssh/fluxvm_ed25519` (warn) |
| FileVault (host) | on (warn: no auto-login, so a headless Mac cannot start guests after a reboot) |

`--json` prints `{"ok": bool, "checks": [{"name", "status": "ok|warn|fail", "detail", "fix"}]}`, the same shape as `vz doctor --json`.
FileVault inside the **template** cannot be checked from the host; see "Hard limits".

## Waiting: `mac wait-ready` and `run --queue`

`fluxctl mac wait-ready NAME [--timeout S] [--user U] [--check CMD] [--json]` blocks until the instance is `running`, has an address,
and accepts a TCP connection on port 22; with `--check`, `CMD` must also exit 0 in the guest over SSH (BatchMode, so a missing key
makes it time out rather than prompt). `--timeout` defaults to 900, `0` waits forever. A `stopped`, `failed` or `paused` instance
fails at once with the reason. On success it prints `ready: NAME IP (after Ns)`, or with `--json`
`{"schema_version":1,"name","id","ready":true,"ip","waited_secs"}`. A timeout says what was still missing (no address, port 22 closed, check failing).

`fluxctl mac run --queue [--queue-timeout SECS]` waits for a free macOS slot instead of failing with 409 (default 3600 s, `0` forever).
While 2 `fluxctl mac` instances are running or paused it only waits, so it does not clone a disk each time; otherwise it tries, and a
409 naming the guest limit sends it back to waiting. Any other error ends the run. Limits: the queue lives in each client process, so
several waiting clients are **not** served first-come-first-served; and a macOS VM that `fluxctl mac` did not create is visible only
through that 409. Ctrl-C while waiting creates nothing to clean up. `mac build` has no `--queue` yet.

## JSON output (schema version 1)

`mac run --json`, `mac ps --json` and `mac inspect` print documents with a `schema_version` key. **Within a version, keys are only
added; none is renamed, removed or retyped.** A new incompatible shape gets a new version number. The `vm` key is the daemon's raw
VM record: a passthrough, not part of the contract. Golden files in `crates/fluxctl/src/mac_json_golden/` pin the shape (a change
fails the tests; regenerate with `UPDATE_GOLDEN=1`).

**`mac ps --json`**: an array, one object per listed instance (`-a`, `-f` apply as usual). Before this schema `ps --json` printed
the raw VM records; those are now under each object's `vm` key.

| Key | Type | Meaning |
|---|---|---|
| `schema_version` | number | `1` |
| `id`, `short_id` | string | full VM id; first 12 hex characters |
| `name`, `image` | string | instance name; `name:tag` it was started from (`null` if unlabelled) |
| `status` | string | `creating`, `running`, `paused`, `stopped`, `failed`, or `unknown` |
| `ip` | string or null | guest address once it has one |
| `ports` | array | `{"host": n, "guest": n}` per published port |
| `ssh_port` | number or null | the host port forwarding to guest port 22, if `-p` published one |
| `created` | string or null | the daemon's creation time |
| `restart_policy` | string | `no`, `always`, `unless-stopped`, `on-failure` |
| `project`, `service` | string or null | compose project and service |
| `labels` | object | the labels you set (`--label`); `fluxvm.mac*` are hidden |
| `vm` | object | raw record (unstable) |

**`mac inspect NAME...`**: an array with one object per name: `schema_version`, `id`, `name`, `image`, `state` (`status`, `pid`, `error`,
`restart_policy`), `created`, `network` (`ip`, `ports`), `resources` (`cpus`, `memory_mib`), `mounts` (`source`, `destination`,
`read_only`), `labels`, `disk`, `workspace`, `image_config`, `vm` (raw, unstable).

**`mac run --json`**: with `--json`, the instance name is not printed; instead one compact JSON object is the **last line of stdout**
(the command's own output goes before it on stdout, ssh's stderr stays on stderr). It is printed only when the run succeeded; a failed
`CMD` or boot is an error message and a non-zero exit, with no JSON.

| Key | Type | Meaning |
|---|---|---|
| `schema_version` | number | `1` |
| `name`, `id`, `image` | string | the instance |
| `status` | string | `removed` after `--rm`, else `running` |
| `ip` | string or null | null when the run did not wait for SSH (no CMD, no `-v`) |
| `removed` | bool | the instance was deleted |
| `ran_command` | bool | a command ran (in the background with `-d`) |
| `detached` | bool | `-d` was given |

`mac system info --json` and `mac system version --json` carry `schema_version` 1 and are pinned by goldens; see [above](#mac-system-info-and-mac-system-version).

Not covered yet: `images`, `system df` and `stats` already print JSON but have no versioned schema.

## Live acceptance: `scripts/test-mac-vz.sh`

A script for a real Mac that exercises every command above: import a template, `run --rm`, `run -d` + `wait-ready`, `exec`,
`ps`/`inspect` JSON, `cp` both ways, `stop`/`start` (the disk survives), `commit`, `build` of a tiny Macfile, `--queue` with both slots
taken, `push`/`pull` against a local registry (`FLUXVM_E2E_REGISTRY`, or a `registry:2` container if docker exists), `prune`, `system df`.
It uses its own temporary store, names its instances `acc-*`, and cleans up. It needs a template whose guest accepts your SSH key and
two free macOS slots. Exit 0/1/2 = all passed / a check failed / a precondition is missing; skipped checks are listed.

```
FLUXCTL=./target/release/fluxctl FLUXVM_URL=http://127.0.0.1:7788 FLUXVM_MAC_TEMPLATE_DIR=/Volumes/X/macos-15 scripts/test-mac-vz.sh
scripts/test-mac-vz.sh --self-test     # no Mac needed: the same script against a fake fluxctl
```

**It has not been run on hardware.** `--self-test` proves the script's own logic (including that a broken command is reported and that a
missing template or a failing `doctor` stops it); it says nothing about `fluxctl mac`. The first live run will show how a real
registry treats `http://localhost`, how long a clone takes to answer SSH, and whether the checks' expected strings match real output.

## Signing, licence guard, cache, search and contents

Added on top of the commands above. Unit-tested with fakes and the mock registries of `fluxvm-image`; **nothing here was run against a
real registry, a real Mac or a real macOS template**.

### Signing and verification

`fluxctl mac keys generate` creates an Ed25519 keypair (the same scheme as the image catalog, `fluxvm_core::signing`) in
`$FLUXVM_MAC_TRUST` or `~/.fluxvm/mac-trust/` (next to the store, never inside it); the private key file is mode 0600 and is never
overwritten. `mac keys trust NAME FILE` adds someone's public key, `mac keys ls` lists them (your own key shows as `self`), `mac keys untrust NAME` removes one.

- `mac push --sign REF [REMOTE]` signs `fluxvm-mac-image-v1\n<manifest digest>` and attaches the signature as an OCI referrer (artifact type
  `application/vnd.zyvor.fluxvm.signature.v1+json`; the signature and signer key are in the referrer's annotations). `mac sign REGISTRY_REF` signs an image already pushed.
- `mac pull --verify REF` (or `FLUXVM_MAC_REQUIRE_SIGNED=1`) fetches the signatures, requires one from a trusted key, and then pulls **that
  digest** (`repo@sha256:...`), so a tag that moves after the check cannot swap the image. Unsigned, signed by an untrusted key, and signed
  for a different digest are three distinct errors. The verified signatures are kept beside the trust store.
- `mac verify REF` checks a registry reference (asks the registry), or a local image against the signatures kept at pull or push time. It exits 1 on failure; `--json` for scripts.

This is **not cosign**: no keyless flow, no transparency log, no certificate chain. A key inside a signature proves nothing unless you
trusted it. A signature covers the manifest digest, hence the config and every chunk; it does not say the image is safe. The referrers
API or the `sha256-<hex>` tag fallback must be available on the registry. Local-only images (built or imported, never pushed) have no manifest digest and cannot be signed.

### Licence guard

`mac push` refuses well-known public registries (Docker Hub, GHCR, Quay, public ECR, GitLab, MCR, Codeberg, registry.k8s.io) before any
upload. Apple's macOS licence terms may restrict copying and redistributing macOS; the message says plainly that FluxVM cannot tell what
your licence allows, that it is not legal advice and that the clauses were not verified. Pass `--i-have-checked-the-licence` (or
`FLUXVM_MAC_ALLOW_PUBLIC_REGISTRY=1`) to proceed; a warning is printed and the image gets the annotation
`dev.zyvor.fluxvm.licence-ack=public-registry-override`. The list is a safety default, not a statement that other registries are fine:
a private registry on a public host name is your responsibility.

### Build cache

`mac build` keeps an intermediate template after every `RUN`/`COPY` as `fluxvm-cache/<key>:c` in the store (hidden from `mac images`
unless `--all`). The key chains: `sha256(previous key, step text, hash of the COPY source tree)`, starting from the `FROM` template's
digest (or its store directory for a local import). A later build restarts from the longest cached prefix; a fully cached build starts no
VM at all and just clones the entry into `-t NAME`. Macfile line numbers and `--dry-run` do not affect keys; `ENV`, `ARG`, `WORKDIR` are folded into the `RUN` text and so are part of it.

- `--no-cache` runs the old straight-through build and neither reads nor writes the cache.
- `mac cache ls [--json]`; `mac cache prune [--older-than 7d] [--all] [--dry-run]`.
- Size budget: `FLUXVM_MAC_CACHE_MAX` (`50g`, `512m`; default 100g). After each build the oldest entries beyond it are removed, except those that build used. Sizes are allocated blocks; APFS clones share blocks, so the total can overstate what deleting frees.
- Costs: every checkpoint stops the guest and boots a new one from the stored disk (a macOS boot takes minutes), so a long Macfile pays that once per step. Non-deterministic steps (a `curl`, a package index) are cached like any other: use `--no-cache` to refresh. `mac system prune --all` and `image prune --all` treat cache entries as unused images.

### Chunk reuse on pull and push

The `vm-image` format is unchanged (64 MiB zstd chunks, each annotated with file, offset and raw sha256). **Push** already skips chunks the registry has (`HEAD` per layer) and all-zero chunks. **Pull** now takes up to two other local tags of the
same repository as seeds: a chunk whose raw sha256 already matches the bytes at the same file and offset in a seed is copied locally and never downloaded; everything is still checked against the manifest. This helps point releases that change some blocks in place; a disk that grew or shifted data gets no reuse. `mac pull --no-reuse` disables it. There is no cross-repository mount on push and no peer or LAN chunk source.

### Labels and `mac images --filter`

`LABEL` in a Macfile already records labels in the image config. `mac label IMAGE k=v ... [-k KEY]` edits them (names sharing the image's files share labels); `mac push --label k=v` adds more. Labels travel as push annotations (`dev.zyvor.fluxvm.label.<key>`) and in the pushed `config.json`.
`mac images --filter label=KEY[=VALUE] --filter reference=GLOB --filter source=TEXT` (all must match; `*` wildcard); `--json` includes the labels.

### `mac search REGISTRY [TERM]`

Lists repositories from `GET /v2/_catalog` (follows pagination), filtered by substring; `--tags` adds each repository's tags, `--limit N`. Many
registries, including Docker Hub and GHCR, do not serve the catalog; the error says so. It lists names, not whether an image is a macOS image.

### Archives, export and registry manifests (Docker image parity)

Already present before this change: `mac tag`, `mac history`, `mac save`/`mac load`, `mac wait`, `mac stats`, `mac port`, `mac commit`, `mac image prune`, `mac search`.

- `mac save IMAGE --output FILE` now also writes `FILE.sha256` (`shasum -a 256 -c` format). `mac load` and `mac import FILE NAME:TAG` check it when it exists and refuse a mismatch (`fluxctl explain MAC_ARCHIVE_CHECKSUM`); an archive without the file still loads.
- `mac import` accepts a template directory (as before) or a tar from `save`/`export`.
- `mac export NAME --output FILE` (stopped instance, daemon on this Mac): the instance's disk, hardware and auxiliary files as a `save` archive plus checksum. It is not a filesystem export: a macOS disk alone does not boot. A running instance is refused (`MAC_EXPORT_RUNNING`).
- `mac manifest inspect REF [--json]`: digest, platforms, config and layers from the registry, with your `mac login` credentials (`MAC_MANIFEST_UNREADABLE` on failure).
- `mac stats --no-stream` (accepted; stats is always one reading) and `--format` (the `mac inspect --format` template subset; fields Name, CPUPerc, CPUPercent, MemUsage, MemBytes, DiskRead, DiskWrite, DiskReadBytes, DiskWriteBytes; `table ...` supported).
- Studio: Images, OCI tab (on a Mac node) has Copy-as-fluxctl buttons for save, import, export and manifest inspect. These are local-store commands with no daemon API.

### `mac sbom REF` (also `mac image sbom REF`)

A JSON contents manifest of the template bundle: every file (`disk.raw`, `hardware.bin`, `auxiliary.bin`, `config.json`) with size and sha256, plus the registry digest, labels and
build history. `--to FILE` writes it; `--attach REGISTRY_REF` publishes it as a referrer (`application/vnd.zyvor.fluxvm.contents.v1+json`). It does
**not** list the files inside the guest's disk, so it is not an SPDX/CycloneDX software inventory. Hashing reads the sparse `disk.raw` at its full size: minutes for a 64 GiB disk.

## Operations: health, restart, TTL, verify

All of these are unit-tested against fakes (state machines, back-off, expiry, selection). **None was run against a real daemon or a macOS VM**:
the SSH probe, the launchd calls and the label PATCHes are unverified.

### Healthcheck (enforced)

A Macfile `HEALTHCHECK` is stored in the image. The command runs **in the guest over SSH from the Mac that drives it** (not inside the daemon).
The state lives in VM labels (`fluxvm.mac.health`, `.health.streak`, `.health.checked`) so it survives the process and shows in
`mac inspect` as `state.health` (`status` `starting`, `healthy` or `unhealthy`, `failing_streak`, `last_check_unix`). Semantics follow Docker: `starting` until the
first success, `healthy` after one, `unhealthy` after `--retries` consecutive failures; a success resets the streak.

- `mac health NAME [--json]`: probe once now, record, print. Exit 1 unless healthy.
- `mac run --wait-healthy [--health-timeout SECS]`: after the instance is up, block until healthy (probing at most every 2 s); fails on unhealthy, on a
  stopped instance, on timeout, and when the image has no HEALTHCHECK. (Not `--wait healthy`: `--wait` is already the SSH wait in seconds.) Cannot be combined with `--rm`.
- `compose` `depends_on: {db: {condition: service_healthy}}`: see Compose.
- `mac supervise` probes instances when their interval is due.

### `mac supervise`: restart policies that are enforced

The daemon never restarts a macOS VM, so `mac run --restart` was a label. `fluxctl mac supervise [--interval 15] [--once] [--dry-run] [--no-reap]` is the loop that
acts on it: each pass it starts instances whose policy says so (`always`; `unless-stopped` unless `mac stop` marked it; `on-failure` for `failed`), with an exponential
back-off per instance (5 s doubling to 5 min, reset once the instance runs; a failed start such as a 409 is reported and retried later), runs due healthchecks, and reaps
expired TTLs. `on-failure:N` is accepted, the count is not enforced. A transient daemon error does not end the loop.
`mac supervise --install [--daemon URL] [--daemon-context NAME] [--interval S]` writes `~/Library/LaunchAgents/dev.zyvor.fluxvm.mac-supervise.plist` (RunAtLoad, KeepAlive, log
`~/Library/Logs/fluxvm-mac-supervise.log`) and loads it; `--status` and `--uninstall` complete it. No sudo. The plist never holds a token: use a context, or the
agent inherits only `$FLUXVM_URL`.

### TTL and ownership: `mac run --ttl`, `mac reap`, `mac ttl`

Every `mac run` instance is stamped `fluxvm.mac.owner` (`$USER`) and `fluxvm.mac.created`; `--ttl 2h` (`90s`, `30m`, `2h`, `1d`) adds `fluxvm.mac.expires` (unix seconds, counted from when
the run started; the labels are written after the run returns, so a foreground command that never ends is not covered). `mac reap [--dry-run] [--all | --owner NAME]` deletes expired instances: yours by default,
`--all` every owner's (and ownerless ones). `mac supervise` reaps on every pass unless `--no-reap`. `mac ttl NAME 2h|none` changes or clears it. `mac ps -f label=fluxvm.mac.owner=NAME` filters by owner.
A reaper deletes the VM and its disk: it is a safety net for forgotten CI instances, not a pause.

### `mac run --verify`

Before any VM is created, checks the local image against the signatures kept when it was pulled or pushed, using the trust store (`mac keys trust`); an unsigned, untrusted or
invalid image is refused. `FLUXVM_MAC_REQUIRE_SIGNED=1` makes it the default. It needs a registry-backed image (a plain `import` has no digest). Limits: it verifies the manifest digest recorded in the
store, not the bytes of `disk.raw` now on disk; it does not apply with `--fleet`.

### Image usage

`mac run` records when each image was last used (`last-used.json` in the store). `mac images --usage` shows allocated size (the disk is sparse), apparent size, last use and age; `mac images --json`
gains `allocated_bytes`, `added_unix` and `last_used_unix`. `mac image prune --keep-last N [--dry-run] [-f]` keeps, per repository, the N most recently used tags and untags the rest (files go
with the last name; build-cache entries are left to `mac cache prune`). Instances keep their own disk clones, but health and `exec` read the image's config from the store.

### IPSW installs: `FROM IPSW`

`FROM IPSW latest` and `FROM IPSW /absolute/path.ipsw` parse (a bad selector says "only 'latest', or an absolute path to a .ipsw file"), and `mac build` still **refuses to run them**.
`fluxctl mac ipsw [latest|PATH] [--disk-gib 64] [--user admin]` validates the source (a path must exist, be a zip and be several GB) and prints the unattended order, marking each phase as
`have` (in the daemon: `apple.install`, `provision_*`, `fluxctl vz ipsw`; run once on an M4 per docs/macos.md), `partial` (scripts) or `MISSING`:
resolve the IPSW, create the sparse disk (at least 40 GiB), run the installer, provision the admin account with no one at the console, first boot and SSH, authorize the daemon key and install the agent,
disable sleep and the screen lock, shut down and store the template, then the Macfile steps. What still needs hardware: wiring that sequence as a build step, and running the whole chain on a Mac
with an unlocked session and about 90 GB free; nothing here has done that.

## Studio: the Mac page

Open **Mac** in Studio's Platform menu (or `/console/mac`). It needs no set-up: it reads the daemon you are already connected to, and
works for an administrator token. Each part says what it shows, and every action has its `fluxctl` command next to it (the copy
button) so you can do the same in a terminal.

| Part | What it shows | `fluxctl` equivalent |
|---|---|---|
| Instances | name, address, image, state, CPUs and memory, published ports, age; "N of 2 macOS VMs running" | `fluxctl mac ps -a` |
| Start, Stop, Restart, Kill, Remove | the same sequence the CLI uses, including the labels it keeps (a stopped instance is not restarted by `mac reconcile`; start begins a new health period). Kill and Remove ask twice. | `fluxctl mac start\|stop\|restart\|kill\|rm NAME` |
| Logs | the tail of the VM console log (100 to 3000 lines), refreshed while it runs | `fluxctl mac logs --tail 300 NAME` |
| Inspect | the instance's facts and the daemon's full record | `fluxctl mac inspect NAME` |
| Images | the local template store: reference, source, disk size, age | `fluxctl mac images` |
| Compose | projects and their services, read-only | `fluxctl mac compose ls` |
| Registry login | which registries have a saved login and the user name (never a password; logging in stays in the terminal) | `fluxctl mac logins`, `fluxctl mac login REGISTRY` |
| Host health | this Mac's health report (macOS slots, memory, disk, login session, backups), as on Home | `fluxctl cluster status`, `fluxctl mac doctor` |

Home also gets a **Mac instances** card (on a Mac only) with the running count and a link.

The page uses two daemon routes (admin only, listed in `GET /v1/openapi.json`): `GET /v1/mac` for the instances, projects, images and
logins in one answer, and `POST /v1/mac/instances/{id}/{start|stop|restart|kill|rm}`; the answer names the `fluxctl` command. Logs and
inspect use the existing `GET /v1/vms/{id}/logs` and `GET /v1/vms/{id}`. A refusal is explained in words: the two-VM limit says to stop
or remove one, a locked login session says to unlock the Mac.

The image list and the login list are read from the account the **daemon** runs as (`FLUXVM_MAC_STORE`, `FLUXVM_MAC_REGISTRY_AUTH`, or
`~/.fluxvm/mac-images` and `~/.config/fluxvm/mac-registry-auth.json`). If you run `fluxctl` as a different user than the daemon, the
page says which folder it looked in and the command to run as your user. A login file other people can read is not used, and the page
says to `chmod 600` it. Not verified against a real Mac, daemon or browser.
## Volumes, ports and networks (Docker resource parity)

**Named volumes.** `fluxctl mac volume create [NAME] [--label K=V]`, `ls [--json|-q]`, `inspect NAME...`, `rm [-f] NAME...`, `prune [--dry-run] [-f]`. A volume is a folder on this Mac, `~/.fluxvm/mac-volumes/NAME/data` (`FLUXVM_MAC_VOLUMES` moves the root), shared into a guest over virtiofs exactly like `-v ./dir:/path`. `mac run|create -v NAME:/path[:ro]` uses the volume and creates it on first use; a path (`./x`, `/x`, `~/x`) is still a path. Names are letters, digits, `_`, `.`, `-`. A volume counts as in use while any instance, running or stopped, has its folder among its shared folders, and `volume rm` refuses with the instance's name (`MAC_VOLUME_IN_USE`). `volume create` needs no daemon; the other volume commands read the daemon's instances. Compose does not accept named volumes yet.

**`mac cp`** already copies both ways (`mac cp NAME:/guest/path ./local`, `mac cp ./local NAME:/guest/path`, over `scp -r`); nothing was added.

**Ports.** `mac port NAME [GUEST_PORT]` lists what an instance publishes. `-p [ADDR:]HOST:GUEST` is now checked before the VM is created: a host port below 1024 (`MAC_PORT_PRIVILEGED`, the daemon does not run as root), the same host port twice, a host port a running instance already publishes, or one another program on this Mac is listening on (found by binding it for an instant, then `lsof` names the program; only when the daemon is on this machine; `FLUXVM_MAC_SKIP_PORT_PROBE=1` skips it) all fail with a plain message naming the holder and a free port to try (`MAC_PORT_IN_USE`). Forwards are TCP only.

**Networks.** `mac network ls` and `mac network inspect [nat]` show the one NAT network, who is on it, and what it does not do: guests on it cannot reach each other, no DNS names, no user-defined networks. `network create|rm|connect|disconnect|prune` are refused with that explanation (`MAC_NETWORK_UNSUPPORTED`).

**Disk.** `mac system df` adds a Volumes row and the Mac's free space (and a warning under 30 GiB free). `mac system prune --dry-run` prints a table of instances, images (`--all`), store folders and volumes (`--volumes`) with sizes, "Would free up to X", and "Disk: A free of B. After this: about C free." Sizes are allocated bytes and an upper bound because APFS clones share blocks; after a real prune it prints the measured gain. With `--json` the plan has schema 1 (`would_free_bytes`, `disk_free_bytes`, ...).

**Studio and API.** `GET /v1/mac/disk` (admin, read-only) returns the same numbers for Studio (disk, images, instances, volumes with their users, a prune preview) plus the `fluxctl` commands that act on them (Copy as fluxctl). No Studio page calls it yet.

Not verified: any of this on a real Mac or macOS guest (the virtiofs share name under `/Volumes/My Shared Files` is the same unverified guess as for `-v`), the `lsof` output format on macOS, and the free-space numbers against Finder's "available" figure (which counts purgeable space; `df` does not).

## Hard limits

These come from the design and from macOS itself; they apply to the whole workflow.

- **At most 2 macOS VMs at a time** on one host (`MAX_MACOS_GUESTS`). `run` and `build` refuse at the limit; `run --queue` waits instead.
- **No fork or branching.** A saved state is bound to its machine identifier and MAC, so "fast" means cloning a stopped template, not
  many restores of one snapshot.
- **Restore needs an unlocked login session** on the host. A headless server needs a dedicated auto-login user with the lock off.
- A clone is instant only on the **same APFS volume** as the template; otherwise it is a full 20+ GB copy. This applies to `import`
  (`cp -c`) as well as to later clones.
- FileVault must be off in a template, or key login fails on clones.
- Not supported for macOS guests: extra disks, Rosetta, nested virtualization, private networks, console ports, tagged shares.
- **Apple's licence terms limit where macOS images may be stored.** Pushing a template to a public registry is likely not allowed; use
  a private registry. This is a caution, not a legal reading; check the terms that apply to you. `mac push` refuses well-known public registries unless overridden (see the licence guard).

## See also

[macos-containers-research.md](macos-containers-research.md): a survey of Tart/Orchard, Anka, Orka, Lima, OrbStack, Apple `container` and
others, a comparison with `fluxctl mac`, and a prioritised backlog.

## Not verified

Checkpoints (`mac checkpoint`, `checkpoints`, `diff`, `run --from-checkpoint`, the safety checkpoint before a restore, the disk-space
check, the Studio Protect tab): unit-tested with a fake daemon, temporary files and the Studio helper tests; the Studio bundle type-checks
and builds. Never run against a real daemon or a macOS guest, so these are unchecked: that a memory checkpoint of a running guest then
resumes, the pause length, the `auto-before-restore` save on a real restore (and its pruning), the size of a memory checkpoint, `mac diff` on
a multi-GiB disk, and the Studio tab in a browser.
The Docker-style build flags (`--secret`, `--ssh`, `RUN --mount`, `--label`, `--platform`) are unit-tested with a fake guest and temporary files only: the real `scp` of a secret, `ssh -A` agent forwarding into a macOS guest, file modes and the removal on a real VM, and a git clone through the forwarded agent were never run.

The observability options (`inspect --format`, `images --format`, `events --since/--filter`, `logs -t`, `system info/version`) were never run against a daemon or a Mac: the template engine, filters, time parsing, line stamping and the info document are unit-tested; the HTTP calls (`/v1/events?since=`, `/v1/openapi.json`, the two log requests) are not. The daemon's version is read from the OpenAPI document and has not been checked against a running daemon.

Written from the source. The commands are covered by unit tests on a temporary store and on sample daemon records; none was run
against a real registry or daemon, and no imported, pulled, built or committed template was booted. Specifically unverified: the
virtiofs share names inside a macOS guest, the generated `export ...; cd ... &&` shell text on a real guest, `scp` and `curl` in a
guest, the `/logs`, `/stats`, `/pause`, `/resume`, `/restart` routes' exact output for macOS VMs, `tar` on a multi-GiB sparse disk,
and everything in `compose`. Also unverified: signing and verification against a real registry's referrers API, `--sign` attach, `mac search` against a real catalog, the build cache on a real guest (stop, clone, reboot per step), chunk reuse on a real 20+ GB template, and `mac sbom --attach`. `build --dry-run`, `compose up --dry-run` and the image tools on a store are the parts you can try
without a VM.

`mac login` / `logout` / `logins` and `mac fork`: unit-tested against a loopback fake registry and a loopback fake daemon only. Not run
against a real registry (GHCR, Docker Hub, a private one), not run against a real daemon, and no fork was booted. The no-echo
prompt was not exercised in a terminal in tests; `--password-stdin` is the tested path.
