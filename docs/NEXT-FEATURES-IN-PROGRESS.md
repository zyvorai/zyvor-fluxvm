# Features in progress on this branch

This branch starts the next density / restore work from `docs/ROADMAP-DENSITY.md`.

## Shipped in this branch

- **Userfaultfd demand-paged restore entry point**
  - `GuestMemory::setup_uffd_restore` (Linux-only skeleton)
  - Preference in `kvm_snap::load_memory_into` when `FLUXVM_KVM_UFFD=1`
  - Clean fallback to existing `MAP_PRIVATE` path
  - Roadmap updated to mark it as started

- **Dirty-log incremental snapshot skeleton**
  - New module `crates/fluxvm-hypervisor/src/dirty_log.rs`
  - `DirtyDelta` layout (magic `FLUXKVMD`, parent id, per-page records)
  - `SoftwareDirtyBitmap` for host-side device writes that KVM dirty log does not see
  - Unit test for the bitmap

- **Fork-backed warm pools design**
  - `docs/fork-backed-pools.md` — constraints, planned changes, and status

- **Userfaultfd demand-paged restore (full handler)** — `uffd.rs`
  - `UFFDIO_API` / `UFFDIO_REGISTER` (MISSING) / `UFFDIO_COPY` / `UFFDIO_ZEROPAGE` / `UFFDIO_WAKE` bindings
  - `fluxvm-uffd` handler thread serving faults from `snap.mem` with prefetch
    (`FLUXVM_KVM_UFFD_PREFETCH_PAGES`), `EAGAIN`/`EEXIST`/`ENOENT`/`ESRCH` handling, clean shutdown
  - Opt-in `FLUXVM_KVM_UFFD=1`; falls back to `MAP_PRIVATE` on any failure; `userfaultfd` added to the seccomp allowlist

- **KVM dirty-log incremental snapshots** — `dirty_log.rs`, `kvm.rs`, `kvm_snap.rs`
  - `KVM_MEM_LOG_DIRTY_PAGES` + `KVM_GET_DIRTY_LOG`, OR'd with a software bitmap of host writes
  - `FLUXKVMD` delta format, `FLUXKVM1` v6 lineage, delta-chain restore
  - Opt-in `FLUXVM_KVM_DIRTY_LOG=1` + `snapshot_save` `parent`; full-snapshot fallback when unsafe (vhost-net, stale parent)

- **Docker-free gaps documented** — `docs/DOCKER-FREE.md` now states what works, what does not, workarounds and planned work
  for image builds, bind mounts, networks/Compose and the Docker API shim

## Next features ready to implement

1. ~~Full userfaultfd handler thread + `UFFDIO_COPY` from snap.mem~~ (done; needs a real-KVM run)
2. ~~Wire dirty-log capture (`KVM_MEM_LOG_DIRTY_PAGES` + `KVM_GET_DIRTY_LOG`) and v6 vmstate~~ (done; needs a real-KVM run)
3. Implement fork-backed pool backfill
4. Remote agent PTY console over `--server`
5. Generated full OpenAPI (utoipa)
6. Tiny Windows / virtio-win boot gate on in-tree KVM

All designs already exist in the repo docs. This branch is the place to land them.

Follow-ups for 1-2: run the `#[ignore]`d real-KVM tests on a KVM host
(`cargo test -p fluxvm-hypervisor --lib real_kvm -- --ignored`, as root for the
uffd one), measure uffd vs `MAP_PRIVATE` with `scripts/bench-fork.sh`, add a
background fill to the uffd pager, wire vhost-net's own dirty log so vhost guests
can take deltas, and expose `parent` through the scheduler / `fluxctl`.
