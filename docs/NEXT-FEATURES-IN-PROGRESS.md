# Features in progress on this branch

This branch starts the next density / restore work from `docs/ROADMAP-DENSITY.md`.

## Shipped in this branch

- **Userfaultfd demand-paged restore entry point**
  - `GuestMemory::setup_uffd_restore` (Linux-only skeleton)
  - Preference in `kvm_snap::load_memory_into` when `FLUXVM_KVM_UFFD=1`
  - Clean fallback to existing `MAP_PRIVATE` path
  - Roadmap updated to mark it as started

## Next features ready to implement

1. Full userfaultfd handler thread + `UFFDIO_COPY` from snap.mem
2. KVM dirty-log incremental snapshots (v6 format + software bitmap for device writes)
3. Fork-backed warm pool backfill
4. Remote agent PTY console over `--server`
5. Generated full OpenAPI (utoipa)
6. Tiny Windows / virtio-win boot gate on in-tree KVM

All designs already exist in the repo docs. This branch is the place to land them.
