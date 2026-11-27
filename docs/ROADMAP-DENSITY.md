# Density roadmap: fork and restore efficiency

Status of Phase 3 work on the in-tree KVM engine.

## Done

- `MAP_PRIVATE` file-backed restore of `snap.mem`
  (`GuestMemory::remap_private_file`, called from `kvm_snap::load_memory_into`).
  The existing anonymous RAM mapping is replaced in place with `MAP_FIXED`, so
  the host address and the KVM memslot registration do not change. Children
  forked from one snapshot share every clean page through the page cache and
  copy only what they write. Falls back to the eager `read_exact` copy when
  `FLUXVM_KVM_LOCK_MEM=1`, `FLUXVM_HUGEPAGES=1`, `FLUXVM_KVM_EAGER_RESTORE=1`,
  on a size mismatch, or on any mmap error. Constraint: do not truncate or
  rewrite `snap.mem` in place while a restored guest runs (SIGBUS); unlinking is
  fine.
- Child identity: `AgentRequest::ResetIdentity` (hostname, machine-id, entropy
  via `RNDADDENTROPY`, neighbour-cache flush), sent by `fork_child` after resume.
  Failure is logged at error level and audited, not fatal.
- `scripts/bench-fork.sh`: p50/p95 per-child ready time, PSS/private/RSS per
  child, disk bytes per child. Use `FLUXVM_KVM_EAGER_RESTORE=1` on the daemon for
  the "before" numbers.

## Implemented (unverified on a KVM host): userfaultfd demand-paged restore (`FLUXVM_KVM_UFFD=1`)

**Code complete, unit-tested; not yet run under a real guest.** `crates/fluxvm-hypervisor/src/uffd.rs`
plus `GuestMemory::setup_uffd_restore`; preferred by `kvm_snap::load_memory_into` when
`FLUXVM_KVM_UFFD=1`.

1. Before touching RAM: env checks (`FLUXVM_KVM_LOCK_MEM`, `FLUXVM_HUGEPAGES`,
   `FLUXVM_KVM_EAGER_RESTORE` all refuse), size check, then
   `userfaultfd(O_CLOEXEC|O_NONBLOCK)` (or `/dev/userfaultfd` +
   `USERFAULTFD_IOC_NEW` when the syscall is refused) and the `UFFDIO_API`
   handshake. Only page-fault events are requested (no fork/remap/remove), and
   never `UFFD_USER_MODE_ONLY`: KVM faults guest RAM in from kernel mode.
2. Guest RAM is replaced in place (`MAP_FIXED`, same host address, so the KVM
   memslot and every `host_ptr()` copy stay valid) by an empty
   `MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE` mapping and registered with
   `UFFDIO_REGISTER_MODE_MISSING`; registration must offer `UFFDIO_COPY` and
   `UFFDIO_WAKE`.
3. A `fluxvm-uffd` thread polls the uffd and a stop eventfd. Each
   `UFFD_EVENT_PAGEFAULT` is answered from `snap.mem` (`pread` at the
   region offset into a page-aligned bounce buffer) for the faulting page plus
   `FLUXVM_KVM_UFFD_PREFETCH_PAGES - 1` following pages (default 16, clamped to
   1..=512 and to the region). An all-zero window uses `UFFDIO_ZEROPAGE` when
   offered. `EAGAIN` is retried from where the kernel stopped; `EEXIST` (part of
   the window already present) falls back to the faulting page alone and then
   to `UFFDIO_WAKE`; `ENOENT` (teardown) wakes; `ESRCH` (process exiting) stops
   the thread. A read error leaves the fault unserved and is counted rather
   than handing the guest wrong bytes. Counters: `GuestMemory::uffd_pager()`.
4. The pager lives on `GuestMemory` (shared by clones) and is shut down before
   the mapping is unmapped.
5. Fallback: any error before the remap leaves RAM untouched; any error after
   it leaves an empty mapping that the `MAP_PRIVATE` path (or the eager copy)
   then replaces completely. Either way restore continues and logs
   `[kvm-engine] uffd restore unavailable (<reason>), falling back`.
6. The hypervisor's seccomp allowlist now includes `userfaultfd` (it already had
   `ioctl`, `poll`, `pread64`, `eventfd2`); without it the opt-in would SIGSYS.

Needs `vm.unprivileged_userfaultfd=1`, `CAP_SYS_PTRACE`, or access to
`/dev/userfaultfd`. Same caution as `MAP_PRIVATE`: do not rewrite `snap.mem`
while a restored guest runs.

Tests: ioctl numbers and struct sizes against `<linux/userfaultfd.h>`, the
fault-window arithmetic, short-file zero fill, serving every page of a
region from a file with racing faulting threads, a non-zero file offset plus an
already-present page (the `EEXIST` path), prompt shutdown, and
`setup_uffd_restore` either serving the snapshot or leaving RAM untouched.
These use a user-mode-only uffd, so they run unprivileged even with
`vm.unprivileged_userfaultfd=0`; they skip only when userfaultfd is missing
entirely. `kvm_snap::tests::real_kvm::guest_runs_on_uffd_restored_ram` runs a
real-mode guest on uffd-backed RAM; it is `#[ignore]`d (needs a working
`/dev/kvm` and a kernel-fault uffd).

Not done: a background fill of the remaining pages after the guest is up, and
any measurement against `MAP_PRIVATE`. Balloon `MADV_DONTNEED` on uffd-backed RAM
makes those pages fault in from the snapshot again rather than as zeros (the
guest does not read a ballooned page, so this is harmless but wastes the copy).

## Implemented (unverified on a KVM host): KVM dirty-log incremental snapshots (`FLUXVM_KVM_DIRTY_LOG=1`)

**Code complete, unit-tested; the KVM side is not yet run under a real guest.**

- `KvmVm::set_dirty_logging(on)` re-registers memslot 0 with or without
  `KVM_MEM_LOG_DIRTY_PAGES`; `KvmVm::get_dirty_log()` returns the
  `KVM_GET_DIRTY_LOG` bitmap (one bit per 4 KiB page, cleared by the read).
- Host-side writes KVM never sees are tracked in software:
  `GuestMemory::write_at` (every virtio completion, block read, RNG fill) and
  `advise_dontneed` (balloon) mark `GuestMemory::host_dirty()`, a lock-free
  bitmap shared by all clones that is off (one relaxed load per write) until
  the first full snapshot with logging on.
- `kvm_snap::dump_tracked` (used by the live VM's `SnapCmd`):
  - full snapshot: as before, then (with `FLUXVM_KVM_DIRTY_LOG=1`) turn the log
    on, clear it, and record this snapshot's id as the baseline;
  - `SnapCmd.parent = Some(..)`: a `FLUXKVMD` delta of the KVM bitmap OR the
    software bitmap, collapsed into page ranges, then the vmstate.
  - A delta is only written when the parent's v6 lineage id equals the
    baseline (no other snapshot has consumed the log since), logging is on, and
    no kernel vhost-net datapath is active (vhost writes RX buffers into guest
    RAM behind both logs). Otherwise it logs why and writes a full snapshot,
    which always restores. A failure after the log was read resets the
    baseline, so the next snapshot is full.
- Format: `FLUXKVM1` **v6** = v5 plus a 36-byte lineage block (snapshot id,
  parent id, delta flag) between the MP states and the tagged sections; v1-v5
  files keep loading. The delta file (`dirty_log.rs`): header (magic
  `FLUXKVMD`, version, page size, RAM length, snapshot id, parent id, parent
  memory path) and a range table `(first gfn, pages)`, then the packed pages.
- Restore: `load_memory_into` detects `FLUXKVMD`, loads the parent chain (up to
  64 deep; relative parent paths resolve next to the delta) through the usual
  uffd / `MAP_PRIVATE` / eager path for the base, checks each parent's id where
  it can (a parent delta's header, or a full parent's sibling `.vmstate`), and
  copies each delta's pages over it. `memory_image_len` reports a delta's RAM
  size for the restore preflights.
- API: `{"action": "snapshot_save", "path": ..., "parent": "<earlier snapshot
  metadata path>"}` (control socket), i.e. `snapshot::save(st, path,
  Some(parent))`. Firecracker guests ignore it and write a full snapshot.

Tests: bitmap-to-range conversion (runs, word boundaries, padding bits),
bitmap merge and sizes, software bitmap enable/mark/take, delta write/read/apply
round trip and every rejection (range past RAM, wrong RAM size, truncation,
unordered ranges, not a delta), v6 lineage parse, restore of a base + two-delta
chain and refusal of a mismatched parent. `#[ignore]`d real-KVM tests:
`dirty_log_sees_guest_writes_only` (a real-mode guest increments one page; the
log shows exactly that page) and `incremental_snapshot_round_trip` (full
snapshot, guest + host writes, delta of exactly those pages, refusal cases,
restore equals live RAM).

Limits: deleting or rewriting a parent breaks its deltas (restore fails
cleanly with the missing/mismatched parent). Writes through
`GuestMemory::as_slice_mut()` or raw `host_ptr()` copies are not tracked (only
restore and tests use them today); a new device that writes RAM that way must
mark `host_dirty()` or block deltas like vhost-net does. A VM restored from a
snapshot starts without a baseline, so its first snapshot is full. vhost-net
guests never get deltas until vhost's own dirty log (`VHOST_SET_LOG_BASE`) is
wired.

## TODO: warm pool backfilled by fork

Not implemented. `backfill_pool` boots members and waits for the agent. A
fork-backed backfill would hold one parked "template" VM per `PoolSpec`, call
`fork_vm` for each missing member and rely on `ResetIdentity` for uniqueness.
Open issues: forked children keep the parent's MAC and IP, so the pool needs
per-VM netns (the same restriction `check_forkable` enforces), and the template
must be quiesced after the agent is ready so children do not inherit in-flight
requests.

## Update 2026-10-08: what the latest batch changed

Honest status. Everything below is **implemented and type-checked, with unit
tests passing. Nothing here has been run end to end on real VMs.** No density,
latency or throughput number has been measured, and none is claimed.

### Shipped in code (unverified on hardware)

- Fork and restore, described from the code in
  [fork-and-restore.md](fork-and-restore.md): journaled all-or-nothing fork,
  `MAP_PRIVATE` restore with an eager fallback, `ResetIdentity` for each child,
  `?ready=exec` first-command timing on create, fork and pool claim.
- Crash-safe lifecycle ([crash-recovery.md](crash-recovery.md)): fsynced writes,
  an operation journal for create/delete/snapshot/fork/restore, orphan sweep with
  grace periods, `Idempotency-Key`. Fork's cleanup now rides on it.
- Memory density, in `fluxvm-scheduler/src/density.rs`:
  - balloon control for the KVM engine: `GET|POST /v1/vms/{id}/balloon`;
  - per-VM memory report (PSS from `/proc/<pid>/smaps_rollup`, split private and
    shared): `GET /v1/vms/{id}/memory`;
  - idle reclaim: `[sandbox] idle_balloon_secs` (0 = off) and
    `idle_balloon_percent` (default 50) inflate an idle Running KVM sandbox's
    balloon and deflate it on activity;
  - pressure-aware admission, all off by default: `[policy]`
    `min_host_mem_available_mib`, `max_host_mem_psi_some_avg10`,
    `max_host_mem_psi_full_avg10`, `pressure_defer_secs`.
- Bench scripts: `bench-fork.sh` (per-child ready time, PSS, disk),
  `bench-first-command.sh`, `bench-density-count.sh` (consumes host memory; needs
  `FLUXVM_BENCH_CONFIRM=1`), and `bench-kvm-vhost.sh` for the network datapath
  ([native-io-performance.md](native-io-performance.md)).
- Speculative execution (changesets) uses fork for flux-vm sandboxes when
  `check_forkable` passes; see NEXT-FEATURES for the API.

### Not verified on hardware

- Whether `MAP_PRIVATE` restore actually shares pages between children, and how
  many. The bench scripts are the way to find out; run them once with
  `FLUXVM_KVM_EAGER_RESTORE=1` and once without.
- That `ResetIdentity` succeeds against the real golden guest agent image, and
  what happens to a child's open connections (they are not reset).
- The idle balloon's effect on host memory and on guest latency after deflate.
- Any fork-time or first-command figure. The previous density archive
  ([benchmarks/evidence/density-20260918-80.79.5.173.txt](benchmarks/evidence/density-20260918-80.79.5.173.txt))
  predates all of this and must not be quoted for it.

### Still to do


- userfaultfd demand paging and dirty-log incremental snapshots are now
  implemented (sections above) but have only been unit-tested; both need a run
  on a real KVM host (`cargo test -p fluxvm-hypervisor --lib real_kvm --
  --ignored`, then `bench-fork.sh` with `FLUXVM_KVM_UFFD=1`).
- A warm pool backfilled by fork is still a design.

The three TODO sections above are unchanged and still unimplemented (except the
userfaultfd entry point, which is now started):
userfaultfd demand paging (handler), dirty-log incremental snapshots, and a warm pool
backfilled by fork. They remain designs. `MAP_PRIVATE` is the only lazy restore
that exists, and the dirty-log design is the riskiest because host-side device
writes through `host_ptr()` are not tracked by KVM.

## Dirty-log capture skeleton

`crates/fluxvm-hypervisor/src/dirty_log_capture.rs` defines DirtyPage and DirtyLog. Full KVM_GET_DIRTY_LOG wiring is still TODO.


