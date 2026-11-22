# `fluxctl up`: from nothing to a running cluster in one command

You installed FluxVM and want a working Kubernetes cluster without reading anything first. Run:

    fluxctl up

(With a `fluxvm.toml` in the folder, or service names, `-f` or `--fleet`, `fluxctl up` still brings up that stack as it always
did; the setup below is what it does when there is no stack to bring up. `--yes`, `--dry-run` and `--json` are refused
for a stack instead of being ignored.)

It looks at the machine, tells you in plain English what it found and what it will do, asks once, and does it. Run it
again later: on a healthy setup it changes nothing and says so.

## What it does

1. **Checks the host.** The operating system and CPU, hardware virtualization (`/dev/kvm` on Linux, Apple Virtualization on
   a Mac), free disk (at least 10 GiB to create the cluster, 30 GiB is comfortable), memory (the cluster VM takes 4 GiB),
   and the programs FluxVM needs (QEMU, `qemu-img`, `ip`, `cloud-localds` on Linux; the Mac runner on a Mac).
2. **Reuses or starts the daemon.** If the daemon answers it is reused. On a Mac a stopped LaunchAgent is started for you.
   On Linux starting a system service needs sudo, so `up` prints `sudo systemctl start fluxvm` instead and carries on:
   the command line works without the daemon, Studio needs it.
3. **Creates the default cluster** with the same code as `fluxctl kube cluster create default`: a single-server k3s in its
   own VM (2 CPUs, 4 GiB, 30 GiB disk). k3s is downloaded from its GitHub release and checked against the release's SHA-256
   list before it goes into the VM; the copy is kept in `~/.fluxvm/up-cache/` so the next run downloads only the list. A
   stopped cluster is started. A create that stopped part way is resumed (nothing is created twice).
4. **Waits until it is healthy** (up to two minutes after the create itself).
5. **Prints the next three commands**: point `kubectl` at the cluster, `fluxctl cluster status`, `fluxctl dashboard`.

## Flags

    fluxctl up --dry-run   # print the plan, change nothing
    fluxctl up --yes       # do not ask
    fluxctl up --json      # one document, schema 1: dry_run, changed, applied, error, plan, after

Without a terminal and without `--yes` it refuses and says so instead of guessing. The exit status is 0 when the cluster
is up (or a dry run could run), 1 when something blocks or fails.

## The plan

    1 step to go
      box

      [ok]       This machine: linux x86_64: Linux is supported
      [ok]       Hardware virtualization: /dev/kvm is present and usable
      [ok]       Required programs: everything FluxVM needs is installed
      [ok]       Disk space: 212 GiB free
      [ok]       Memory: 9.0 GiB free of 15.6 GiB
      [ok]       FluxVM daemon: the daemon is running and will be reused
      [to do]    Kubernetes cluster: create the single-server k3s cluster default ...

Each step is `[ok]`, `[to do]` (`up` will do it), `[warn]` (works, but look) or `[blocked]` (`up` stops until you fix it).
Every warning and block names its fix and a code: `fluxctl explain NO_KVM` shows the causes and fixes in full. The codes
are the ones `fluxctl cluster status` uses: `NO_KVM`, `MISSING_BINARIES`, `LOW_DISK`, `LOW_MEMORY`, `DAEMON_DOWN`,
`UNSUPPORTED_HOST`, `K3S_NOT_READY`, `CHECK_FAILED`.

When the create fails, the message says what to do next: `fluxctl kube doctor` names the failing check, `fluxctl explain
K3S_NOT_READY` lists the usual causes, and running `fluxctl up` again resumes.

## In Studio

The **Home** page has a **Get started** button. It shows the same checklist from the read-only `GET /v1/up/plan` (schema 1,
the `plan` object of `fluxctl up --dry-run --json`) and two copy buttons: `fluxctl up` and `fluxctl up --dry-run`, plus the
three next commands. Studio never changes anything; it shows the plan and you run the command in a terminal on the machine.
A daemon without the route still shows the dry-run command to copy.

## Limits

* `up` sets up the machine it runs on; it does not use `--server`. For a Mac fleet see [cluster-home.md](cluster-home.md).
* The default cluster is single-server, SQLite. More servers, workers and add-ons are `fluxctl kube cluster create` flags
  ([kubernetes-unified.md](kubernetes-unified.md)).
* The daemon's plan sees the cluster's VM and its API port; the CLI reads the cluster's own record and status, so the two can
  differ for a moment while a cluster starts.
