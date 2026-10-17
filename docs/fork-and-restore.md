# Fork and snapshot restore (in-tree KVM engine)

How `POST /v1/vms/{id}/fork` works, how a snapshot's RAM is brought back, and
how each child gets its own identity. Written from `crates/fluxvm-scheduler/src/fork.rs`,
`crates/fluxvm-hypervisor/src/{memory,kvm_snap}.rs`,
`crates/fluxvm-guest-agent/src/main.rs` and the bench scripts.

## Status: read this first

| Piece | State |
|---|---|
| Fork flow, `MAP_PRIVATE` restore with eager fallback, `ResetIdentity`, `?ready=exec`, bench scripts | Implemented and type-checked. Unit tests cover `check_forkable`, hostname validation and request/response shapes. |
| Any of it running against real KVM guests | **Partly verified on one Linux host.** With `fluxvm_engine = "kvm"`, `POST /v1/vms/{id}/fork` created a child from a running flux-vm guest (3 of 3 with `identity: "best-effort"`). With the default `identity: "unique"` it failed on that guest: the child's agent closed the connection during the identity reset, so the cause is not understood. With the default Firecracker engine it answers 400. The timings (26–59 s per child) were dominated by copying the disk on ext4, so this page still makes no latency or memory claims. `bench-first-command.sh` does not pass `identity`, so its fork section fails on that guest. |

## Fork flow

`VmManager::fork_vm(id, count, name_prefix)` turns one running VM into `count`
new running VMs named `{prefix}-{n}` (prefix defaults to `{parent}-fork`).

Preconditions (`check_forkable`):

- backend `flux-vm` with the in-tree **KVM** engine (a Firecracker vmstate names
  the parent's disk and tap by absolute host path, so a child would share them);
- parent is Running or Paused; `1 <= count <= 32` (`MAX_FORK_COUNT`);
- local file-backed disk (`storage: default`);
- network `none`, `user`, or `tap` with `netns: true`, no `direct`, no extra
  NICs. Children keep the parent's MAC and IP, so they must never share an L2
  segment.

Steps:

1. Journal a `fork` intent (see [crash-recovery.md](crash-recovery.md)) naming
   the parent's fork snapshot directory `snapshots/fork-<12 hex>`.
2. One `SnapshotSave` of the parent: a pause of a few milliseconds plus a
   reflink of its rootfs. The parent keeps running afterwards.
3. For each child, in sequence: check tenant and host quota, record a
   `fork_child` resource, create the workspace, insert the record (with its own
   vsock CID if the parent had one), reflink `snap.rootfs` to the child's
   `root.raw` (`clone_cow`), prepare a fresh tap in its own netns (MAC cleared so
   it is regenerated), write a per-child snapshot spec pointing at the shared
   `snap.mem` and `snap.vmstate`, and `start_from_snapshot`.
4. After resume, `reset_child_identity` (below).
5. Delete the parent's fork snapshot and clear the intent.

All or nothing: if any child fails, every child created by this call is deleted
and the call returns an error. Children carry the label
`fluxvm.dev/forked-from = <parent id>`. `snap.mem` and `snap.vmstate` are shared
read-only; either restore path below means the files can be unlinked once every
child runs.

## RAM restore: MAP_PRIVATE with an eager fallback

`kvm_snap::load_memory_into` first checks the file size equals guest RAM. It
then calls `GuestMemory::remap_private_file`, which replaces the existing
anonymous RAM mapping **in place** with a `MAP_PRIVATE | MAP_FIXED |
MAP_NORESERVE` mapping of `snap.mem`. The host address does not change, so the
KVM memslot and every copy of `host_ptr()` stay valid. Pages fault in from the
page cache on first touch; children restored from one snapshot share every page
none of them wrote and copy only the pages they dirty. It must run before any
vCPU or device thread touches RAM.

It returns an error, leaving the anonymous mapping untouched, and the loader
falls back to a plain `read_exact` copy of the whole file (logging
`[kvm-engine] file-backed restore unavailable, copying RAM: <reason>`), when:

- `FLUXVM_KVM_EAGER_RESTORE=1` (forces the eager copy; use it for "before"
  numbers),
- `FLUXVM_KVM_LOCK_MEM=1` (mlock/populated RAM) or `FLUXVM_HUGEPAGES=1`, since a
  file mapping can honour neither,
- the file size differs from guest RAM,
- the `mmap` fails for any other reason, or on non-Linux hosts.

Two cautions:

- **SIGBUS.** The snapshot file must not be truncated or rewritten in place while
  a child runs on it. Touching a page past the new end of file kills the guest
  with SIGBUS. Unlinking the file is fine (the mapping keeps the inode alive).
  Nothing in the daemon guards against an operator truncating `snap.mem`.
- The vmstate: a `FLUXKVM1` version below 5 has no full-fidelity state; the
  restore then sets registers only and LAPIC, MSRs, FPU, clock, irqchip and PIT
  start fresh (logged as `[kvm] vmstate vN has no full-fidelity state`).

## Child identity reset

A fork resumes the parent's memory, so every child starts with the parent's
hostname, `/etc/machine-id`, SSH host keys, cloud-init instance, DHCP identity,
kernel RNG state and **agent token**. The request's `identity` says how hard to
try (`POST /v1/vms/{id}/fork`, `fluxctl vm fork --identity`, MCP `vm_fork`):

| `identity` | Behaviour |
|---|---|
| `unique` (default) | Every step must be confirmed by the guest or the whole fork fails and every child is deleted. When the vsock agent is absent, or too old to confirm the new steps, a vz guest is reset over SSH instead; if that is not reachable either the fork is refused with the missing capability named. |
| `best-effort` | Try everything; a failed reset is logged (`vm.fork.identity-reset-failed`) and the child is kept. This was the only behaviour before fork v2, and is what speculation uses. |
| `preserve` | Reset nothing (debugging, deterministic replay). |

The host sends `AgentRequest::ResetIdentity` (retried for up to 15 s while the
resumed agent starts answering) with the child's name as hostname, 32 bytes of
host entropy, a fresh agent token and the flags below. The Linux agent does each
step independently and reports the ones it confirmed in `applied`:

1. `entropy`: `RNDADDENTROPY` on `/dev/urandom`, **then `RNDRESEEDCRNG`**. Without the
   reseed, children restored from one state draw identical "random" machine-ids
   and SSH host keys for the first moments (crediting the pool does not make the
   kernel reseed at once; found on hardware, see the macOS hardware log).
2. `hostname`: `sethostname` and `/etc/hostname`.
3. `machine-id`: a fresh id into `/etc/machine-id` (and the dbus copy if a file).
4. `ssh-host-keys`: remove `/etc/ssh/ssh_host_*_key{,.pub}`, `ssh-keygen -A`,
   reload sshd. Reported applied when there is no sshd.
5. `cloud-init`: `cloud-init clean --logs`, so the child is a new instance (user
   modules are not re-run). Reported applied when cloud-init is absent.
6. `dhcp`: `networkctl reload` and `reconfigure` of the non-loopback links when
   systemd-networkd is active, so a changed lease is picked up.
7. `network-state`: neighbour cache, route cache and (if present) conntrack flush;
   also reported as `neigh-flush` for older callers.
8. `agent-token`: the agent writes the new token to its token file (0600,
   atomically) and then accepts only it. The request is authorised with the old
   token; the old one stays valid for exactly one further request, so a reply the
   host never saw cannot lock it out. The host stores the new token on the VM
   record only after the guest confirmed this step.

`unique` requires steps 1 to 7. An older agent ignores the fields it does not
know and simply omits their names, which is how it is recognised (the wire format
has no version number, and a new `op` would be a connection-dropping parse error,
so the existing request was extended with defaulted fields).

The response is `IdentityReset { applied, failures }`. Per child, the fork
report returns `identity: {via, applied, token_rotated, ms}` where `via` is
`agent`, `ssh`, `preserved` or `none`. The SSH fallback does not rotate the
agent token (the absent or old agent is why it is used), so `token_rotated` is
false there and the child still shares the parent's token.

Not done:

- **Open connections are not touched.** Established TCP connections, listening
  sockets and in-flight requests in the parent are present in the child (the
  guest believes the old state). Quiesce the parent before forking.
- The IP and MAC stay the parent's. On vz every child therefore needs a network
  of its own (see the macOS guide); on KVM, a netns per child.
- `boot_id` (`/proc/sys/kernel/random/boot_id`) cannot be changed from user
  space: every child reports the parent's.
- No clock step and no per-process re-seeding: an application that already read
  random bytes keeps them (a TLS session cache, a language runtime's PRNG), and
  secrets in application memory are copied. Fork does not rotate those.
- macOS guests are not forkable; the macOS agent only sets the hostname.
- A plain snapshot restore of the same VM (`vm_restore.rs`) does not call
  `ResetIdentity`; it only waits for the agent to answer again.

## Branches

A branch is a **named, durable checkpoint** of a running `vz` Linux guest: its saved machine state and disk (APFS
clones, so it costs nothing until something writes) plus the VM record it came from, kept under
`<state_dir>/branches/<id>/`, outside the VM's workspace. Start any number of VMs from it, now or after the source VM
is gone.

```sh
fluxctl branch create <vm> checkpoint-a --label purpose=demo   # the guest keeps running; held ~0.4 s
fluxctl branch list [<vm>]
fluxctl branch start <branch> -n 4 --prefix worker             # same flags as vm fork: --identity --network --ready --hook
fluxctl branch delete <branch>
```

REST: `POST /v1/vms/{id}/branches {"name", "labels"}`, `GET /v1/vms/{id}/branches`, `GET /v1/branches[/{id}]`,
`DELETE /v1/branches/{id}`, `POST /v1/branches/{id}/start` (the body and the response are the fork spec and report),
`POST /v1/branches/prune` (below).

**Pruning.** `POST /v1/branches/prune {"keep_last": N, "older_than_secs": S, "source_vm": "<id>", "name_prefix": "nightly-",
"dry_run": true}` (admin; CLI `fluxctl branch prune --keep-last 3 --older-than 7d [--source vm] [--name-prefix p] [--yes]`).
It removes branches that are beyond the newest `keep_last` in scope **and** older than `older_than_secs` (a branch must
satisfy every limit you give; at least one of the two is required, and `keep_last: 0` needs `older_than_secs`). `dry_run`
defaults to true: the response lists `selected` (with the reason for each), `protected` and `selected_state_bytes` and
deletes nothing until you send `dry_run: false` (`--yes`). A branch labelled `fluxvm.dev/keep: "true"` is never removed.
Each deletion is audited as `vm.branch.delete`; a failure on one branch is reported in `failed` and the rest continue.
VMs started from a pruned branch are independent and keep running.

**Diff, export and import.** `GET /v1/branches/{a}/diff/{b}` (`fluxctl branch diff A B`) compares two branches' saved
vCPUs, memory and state size, their lineage (parent, sibling or unrelated) and labels (added, removed, changed). It
compares what the branch records, not guest file contents. `POST /v1/branches/{id}/export {"path"}` (`fluxctl branch
export ID DIR`, admin) writes the branch to a new directory on the host: the saved state, copy-on-write disk clones and
`branch.json`. `POST /v1/branches/import {"path", "name"}` (`fluxctl branch import DIR [--name n]`, admin) makes a new
branch from such a directory, with a new id and no parent link; a name already used by a branch of the same source VM is
refused. Limits: the bundle carries the guest's agent token (as its disk does), so it is owner-only and you should treat
it like the VM's disk; a saved state restores only on a Mac of the same hardware model and a compatible macOS, which
import cannot check and a start reports; the bundle is a directory, not a single archive.

- **Same machinery as fork.** `start` restores each VM from the checkpoint with the same identity reset (strict by
  default), token rotation, parallel restore, hooks and readiness; the same limits apply (a Linux guest on
  `apple.usernet` or no network, no extra disks).
- **Semantics are those of the checkpoint instant:** a file written before `create` is in every started VM, one written
  after is in none. Verified on hardware.
- **Lineage.** A VM started from a branch carries `fluxvm.dev/branch=<id>`; a branch taken of it records
  `parent_branch`. Deleting a branch does not touch VMs started from it or branches derived from it (their files are
  clones).
- **Secrets.** The directory is 0700 and holds the source VM's record, including its agent token; the API never returns
  it. The saved memory also contains whatever the guest held, as with any snapshot.
- **Not here yet:** committing a branch's disk back, rebase, flatten, a disk diff, MCP tools. A crash during `create`
  can leave a hidden `.<id>.partial` directory in `branches/` (ignored by listing; safe to delete).

## Hooks and readiness

`hooks` (argv lists, at most 8) run in each child **after** its identity is reset and **before** readiness is probed,
as root through the guest agent (no shell; they need the agent). A non-zero exit fails the fork. `ready` says what a
child must do before it is reported: `running` (default: identity reset and nothing more), `agent` (answers a ping),
`exec` (a trivial command runs, through SSH for a vz guest without an agent), `{"tcp": 5432}` (accepts connections on
`127.0.0.1` inside the guest) or `{"http": {"port": 8080, "path": "/health"}}` (a 2xx). Probes retry for
`ready_timeout_seconds` (default 60, hooks included); a child that is not ready fails the whole fork and is deleted.
`fluxctl vm fork --hook "systemctl restart app" --ready http:8080/health --ready-timeout 30`. The per-child report has
`hooks_ms` and `ready_ms`.

This is the place to reset application identity the guest cannot know about (a node id, a session cache): the fork
copied it. Secrets held in application memory are not rotated.

### Secrets: what each fork mode does

There is no separate `--secrets` switch; the controls are `identity` and `hooks`, and they are explicit about what they
cover.

| You want | Use | What actually happens |
| --- | --- | --- |
| Rotate what FluxVM manages | `identity: unique` (default) | New machine-id, SSH host keys, kernel RNG seed and **agent token** in every child; the old token is valid for one more request only. |
| Keep everything as the parent had it | `identity: preserve` | Nothing is reset; the child shares the parent's agent token and host keys. |
| Reset secrets FluxVM does not manage | `hooks` | Run your own rotation after identity reset, for example `--hook "rm -f /root/.aws/credentials"` or `--hook "/usr/local/bin/fetch-credentials"`. A failing hook fails the fork. |
| Remove files | a `rm` hook, as above | Deletes the on-disk copy in the child. |

Not covered, and not claimed: secrets already loaded into a process's memory are copied into every child and survive a
file delete; rotate or restart that process in a hook (`systemctl restart app`). Credentials issued by a cloud provider
or an identity service need that service's own revocation. Fork cannot know which of a guest's secrets are
machine-specific.

## First-command latency: ?ready=exec

`?ready=exec` (or `?wait_first_command=true`) on `POST /v1/vms`,
`POST /v1/vms/{id}/fork` and `POST /v1/pools/{name}/claim` makes the API wait,
after the create/fork/claim itself returns, for a trivial `true` exec through the
guest agent to succeed, and adds timing to the response:

- `first_command_ms`: milliseconds from request receipt (measured by the server,
  not the client) to the first successful exec. For fork it is the **worst** child;
  per-child results are in `first_command`.
- `phases`: `create_done_ms` (when the create/fork/claim returned),
  `agent_wait_ms` (the extra wait for the agent) and `first_exec_ms`.

A `vm.first_command` event is also recorded. The request fails if the agent never
answers within 60 s. This changes the response (the call blocks longer),
so use it for measurement and for callers that really need a usable shell, not as
a default.

## Benchmarks (none run yet)

`scripts/bench-fork.sh`: `VM=<uuid> COUNT=8 RUNS=5 ./scripts/bench-fork.sh`,
against a running `fluxctl serve` and a running flux-vm VM. Linux/KVM only
(exits 0 elsewhere). Prints per child:

- `ready_ms_per_child_p50` / `_p95`: fork API time divided by child count. It
  covers the parent snapshot, reflinked disks, memory restores and the identity
  reset, not guest init;
- `pss_kb_per_child_p50`, `private_kb_per_child_p50`, `rss_kb_per_child_p50`
  from `/proc/<pid>/smaps_rollup` (needs root);
- `disk_bytes_copied_per_child_p50`: a filesystem-wide `used` delta, near zero for
  a reflink and noisy on a busy host.

Record a baseline with the daemon started under `FLUXVM_KVM_EAGER_RESTORE=1`,
then again without, and compare.

`scripts/bench-first-command.sh`: `N=10 IMAGE=... POOL=<pool> FORK_SRC=<uuid>
./scripts/bench-first-command.sh`. Cold create, warm-pool claim (needs `POOL` with
at least N ready members) and fork (needs `FORK_SRC`; never deleted) via
`?ready=exec`; prints p50/p95 of `first_command_ms` as JSON per section. It only
deletes VMs it created.

`scripts/bench-density-count.sh` (memory, not fork) boots VMs until create is
refused or a memory floor is hit. It consumes host memory and refuses to run
without `FLUXVM_BENCH_CONFIRM=1`.

Results table: not yet measured.

| Measure | Eager copy | MAP_PRIVATE |
|---|---:|---:|
| Fork ready ms per child, p50 / p95 | not yet measured | not yet measured |
| PSS per child | not yet measured | not yet measured |
| First command ms (fork), p50 / p95 | not yet measured | not yet measured |

## Not implemented

userfaultfd demand paging, dirty-log incremental snapshots and fork-backed warm
pools are designs only; see [ROADMAP-DENSITY.md](ROADMAP-DENSITY.md).
