<!-- Copyright 2026 Zyvor AI Labs · https://zyvor.dev -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
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
| `fluxctl mac build -t NAME [-f Macfile] [--build-arg K=V] [--dry-run] [--wait SECS] [CONTEXT]` | `--dry-run` parses a Macfile and prints the plan, with line-numbered errors (client-side). Without it the build runs: needs the daemon, and is **unit-tested only with a fake host; never run against a real macOS VM**. `SNAPSHOT` and `FROM IPSW latest` are refused (not supported). A `#` comment must be on its own line (a trailing one after a directive is an error). |

| `inspect`, `logs [-f]`, `cp`, `stats`, `stop`, `start`, `restart`, `pause`, `unpause`, `rename`, `wait`, `top`, `port`, `prune`, `reconcile` | Implemented (section "Instance commands"), needs the daemon. **Unit-tested on parsing and rendering only; never run against a daemon or a real VM.** |
| `run -e/--env-file/-v/-w/--label/--restart/--cpus/--memory/-d`, `ps -a/-f/--format/-q`, `exec -e/-w` | Implemented (section "Run options"), same caveat. |
| `history`, `save`, `load`, `commit`, `image inspect/prune/history/save/load`, `system df/prune` | Implemented (section "Image tools"). Store-side parts run on temporary stores in tests; `commit`, `df`'s instance half and `prune --all` need the daemon. |
| Macfile `ENV`, `ARG`, `WORKDIR`, `LABEL`, `EXPOSE`, `CMD`, `ENTRYPOINT`, `ADD`, `HEALTHCHECK`; `.macignore` | Implemented (section "Macfile reference"). Parser and planner unit-tested; never built on a real VM. |
| `compose up/down/ps` | Implemented (section "Compose"). **At most 2 services (2 macOS VMs).** `up --dry-run` is client-side; the rest needs the daemon and was never run. |

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
There is no layer cache: every build starts from `FROM`. The build instance runs on the daemon's Mac, so the daemon must be on this machine
(the template and the result are local paths).

## Run options

`fluxctl mac run [OPTIONS] IMAGE [-- CMD...]`. `CMD` is an argv after `--`: one word is shell text as written
(`-- "make && make test"`), several words are quoted one by one (`-- sh -c "echo hi"`). With no `CMD`, the image's `ENTRYPOINT`
and `CMD` (from its Macfile) run; with neither, the instance just stays up.

| Option | Effect |
|---|---|
| `-e NAME[=VALUE]` | Environment for `CMD` (`NAME` alone copies it from this shell). Repeatable. Not stored in the VM, so a later `mac exec` needs its own `-e`. |
| `--env-file FILE` | `NAME=value` lines, `#` comments. `-e` wins over the file. |
| `-v HOST:GUEST[:ro]` | A virtiofs share (`shared_folders`) from a directory on this Mac; the daemon must run on this Mac. A named volume is refused. A macOS guest shows shares under `/Volumes/My Shared Files`; `run` then makes the GUEST path a symlink to the share over SSH (tries without, then with `sudo -n`; a failure is a warning). **The share's name under that directory (`fs0`, `fs1`, ...) is a guess from the Linux tags and is unverified on a real guest.** |
| `-w DIR` | Directory `CMD` runs in (overrides the image's `WORKDIR`). |
| `--label KEY=VALUE` | A label on the VM. Keys starting `fluxvm.mac` are reserved. |
| `--restart POLICY` | `no`, `always`, `unless-stopped`, `on-failure`. **A label only**: the daemon never restarts a macOS VM. `fluxctl mac reconcile` applies it (run it from launchd or cron). `mac stop` marks an instance so `unless-stopped` leaves it alone. |
| `--cpus N`, `--memory 8g` | `vcpus` and `memory_mib` of the VM request. |
| `-d` | With `CMD`: run it in the background in the guest (`nohup`, output in `/tmp/fluxvm-mac-cmd.log` there) and return. Cannot be combined with `--rm`. |
| `--user U` | SSH user (default: the image's `USER`, else `admin`). |

`mac exec NAME [-e NAME=V] [-w DIR] [--user U] -- CMD` applies the image's `ENV` and `WORKDIR` (while the image is still in the
store) and then its own flags; it does not use `ENTRYPOINT`.

## Instance commands

All need the daemon (`--server` or a context). `NAME` is the instance name from `mac ps`.

| Command | What it does |
|---|---|
| `mac ps [-a] [-f KEY=VALUE]... [--format T] [-q] [--json]` | Without `-a` only running, paused and starting instances are listed (this changed from "all"). Filters: `name` (substring), `id` (prefix), `status` (`creating`, `running`, `paused`, `stopped`, `failed`), `image`, `label=k` or `label=k=v`. All filters must match (Docker ORs repeats of one key). `--format` takes `{{.Name}}`-style fields (`ID`, `Name`, `Image`, `Status`, `IP`, `Ports`, `Created`, `Restart`, `Project`, `Service`, `Labels`), `{{json .}}`, and a `table ` prefix (tab-separated columns with a header). `-q` prints short ids. |
| `mac inspect NAME...` | JSON array: state, restart policy, network, ports, cpus, memory, mounts, user labels, disk path, the image's config and the raw VM record. |
| `mac logs [-f] [--tail N] NAME` | The VM's console log (`GET /v1/vms/{id}/logs`; serial output, not the guest's own logs). `-f` streams until Ctrl-C. |
| `mac cp SRC DST` | `NAME:/path` is the guest side; either direction; `scp -r`. Between two instances is refused. |
| `mac stats [NAME...] [--json]` | One reading of CPU, memory, disk I/O (`GET /v1/vms/{id}/stats`). CPU is averaged over the VM's life, and there is no live stream. |
| `mac stop`, `start`, `restart NAME...` | `stop` keeps the disk. `start` needs a free macOS slot. |
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
| `mac system prune [--all] [-f]` | `mac prune --orphaned`, plus `image prune`; with `--all`, also unused images. |

## Macfile reference

One directive per line, `#` comment lines on their own line, a trailing `\` continues a line, and every parse error names the line
(`Macfile line 7: ...`). Directive names are case-insensitive.

| Directive | Meaning |
|---|---|
| `FROM name:tag` | The template to clone. (`FROM IPSW latest` is parsed and refused by `build`.) |
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
| `HEALTHCHECK [--interval=D] [--timeout=D] [--retries=N] CMD ...` or `NONE` | **Recorded only; nothing runs it**, and `compose` `depends_on` does not wait for it. |
| `SNAPSHOT tag` | Parsed; `build` refuses it. |

`ENV`, `ARG` and `WORKDIR` are folded into each `RUN` as `export K='v'; cd 'dir' && cmd`, so `mac build --dry-run` shows exactly what
will run. The build writes `config.json` into the new template's directory.

`.macignore` (in the build context, same rules as `.dockerignore`): one pattern per line, `#` comments, patterns relative to the
context root (`*.md` matches only at the top; `**/*.md` at any depth), `*` and `?` stay inside one path part, `**` spans
directories, a leading `!` re-includes, the last matching pattern wins, and a pattern that matches a directory excludes what is
under it. With a `.macignore`, a `COPY` of a directory is sent from a filtered scratch copy (deleted afterwards); a source that is
itself ignored is an error.

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
  `volumes` (`./host:/guest[:ro]`, relative to the file), `working_dir`, `depends_on` (list, or map whose `condition` is ignored),
  `labels`, `restart`, `cpus`, `mem_limit`, `user`. **An unknown key (`build`, `networks`, `deploy`, ...) is an error**, not skipped.
- `depends_on` orders the start and waits for SSH. It does not wait for a healthcheck.

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

## Hard limits

These come from the design and from macOS itself; they apply to the whole workflow.

- **At most 2 macOS VMs at a time** on one host (`MAX_MACOS_GUESTS`). `run` and `build` refuse at the limit, not queue.
- **No fork or branching.** A saved state is bound to its machine identifier and MAC, so "fast" means cloning a stopped template, not
  many restores of one snapshot.
- **Restore needs an unlocked login session** on the host. A headless server needs a dedicated auto-login user with the lock off.
- A clone is instant only on the **same APFS volume** as the template; otherwise it is a full 20+ GB copy. This applies to `import`
  (`cp -c`) as well as to later clones.
- FileVault must be off in a template, or key login fails on clones.
- Not supported for macOS guests: extra disks, Rosetta, nested virtualization, private networks, console ports, tagged shares.
- **Apple's licence terms limit where macOS images may be stored.** Pushing a template to a public registry is likely not allowed; use
  a private registry. This is a caution, not a legal reading; check the terms that apply to you.

## See also

[macos-containers-research.md](macos-containers-research.md): a survey of Tart/Orchard, Anka, Orka, Lima, OrbStack, Apple `container` and
others, a comparison with `fluxctl mac`, and a prioritised backlog.

## Not verified

Written from the source. The commands are covered by unit tests on a temporary store and on sample daemon records; none was run
against a real registry or daemon, and no imported, pulled, built or committed template was booted. Specifically unverified: the
virtiofs share names inside a macOS guest, the generated `export ...; cd ... &&` shell text on a real guest, `scp` and `curl` in a
guest, the `/logs`, `/stats`, `/pause`, `/resume`, `/restart` routes' exact output for macOS VMs, `tar` on a multi-GiB sparse disk,
and everything in `compose`. `build --dry-run`, `compose up --dry-run` and the image tools on a store are the parts you can try
without a VM.
