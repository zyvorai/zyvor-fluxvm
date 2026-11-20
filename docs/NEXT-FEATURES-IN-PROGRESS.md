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

## Next features ready to implement

1. Full userfaultfd handler thread + `UFFDIO_COPY` from snap.mem
2. Wire dirty-log capture (`KVM_MEM_LOG_DIRTY_PAGES` + `KVM_GET_DIRTY_LOG`) and v6 vmstate
3. Implement fork-backed pool backfill
4. Remote agent PTY console over `--server`
5. Generated full OpenAPI (utoipa)
6. Tiny Windows / virtio-win boot gate on in-tree KVM

All designs already exist in the repo docs. This branch is the place to land them.
