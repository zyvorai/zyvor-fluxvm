# VM images in OCI registries

`fluxctl vm-image push` and `pull` move VM images through any OCI registry: GHCR, Docker Hub, Harbor, ECR, Zot, or a
`registry:2` on your LAN. Build a golden macOS or Linux image once, push it, and pull it onto every Mac in the fleet.

**Status:** implemented and tested against an in-process registry (auth, dedupe, sparse round trip, tamper detection).
Not yet run against a public registry.

```bash
export FLUXVM_REGISTRY_USER=me FLUXVM_REGISTRY_PASSWORD=$GHCR_TOKEN

# A prepared macOS template directory: disk.raw, hardware.bin, auxiliary.bin
fluxctl vm-image push ~/vms/macos-27-xcode ghcr.io/acme/vm/macos:27-xcode-27.1 --name "macOS 27 + Xcode 27.1"

# On another Mac
fluxctl vm-image pull ghcr.io/acme/vm/macos:27-xcode-27.1 ~/vms/macos-27-xcode
# then create clones with "image": "~/vms/macos-27-xcode/disk.raw" and apple.guest_os = "macos"

# A single Linux disk
fluxctl vm-image push /images/debian-13.raw ghcr.io/acme/vm/debian:13
```

## How it is stored

- Each file is cut into chunks (64 MiB by default, `--chunk-mib 1-512`). Each chunk is compressed with zstd and stored as
  one layer, annotated with its file, offset, raw size and raw SHA-256.
- **All-zero chunks are skipped.** A pull recreates each file at its full size as a sparse file and writes only the data
  chunks. A 100 GB macOS disk with 25 GB in use moves about that 25 GB, compressed.
- **Unchanged chunks are never re-uploaded.** Each layer is checked with `HEAD` before upload. Pushing a new version of an
  image, such as one with a newer Xcode, uploads only the chunks that changed, and registries share identical layers
  across repositories.
- The manifest has `artifactType: application/vnd.zyvor.fluxvm.vm.v1`. Its config
  (`application/vnd.zyvor.fluxvm.vm.config.v1+json`) records the guest OS, the architecture and each file's name and
  size.

## What is left out

A directory push skips per-VM identity and runtime state:
- `identity.bin`, a macOS guest's machine identifier. Each clone must get its own, as with local clones.
- `generic-id.bin` and `vz-mac`.
- Saved state (`*.vzvmsave`), which is tied to the host it was saved on.
- Logs, sockets, pid and lock files, and dot-files.

Use `--exclude NAME` to leave out more.

Because the identity is left out, a macOS guest pulled onto another Mac starts with a new identity. If the image was
signed in to an Apple Account, iCloud asks to sign in again (Apple does the same when a VM moves between Macs).
Templates should not be signed in.

## Safety

- The manifest digest pins the config and every layer. A pull checks each layer's digest before decompressing it and
  the raw SHA-256 after.
- File names must be single plain path components, and every chunk must lie inside its file.
- A pull writes into a temporary sibling directory and renames it into place only after every chunk has verified. A
  failed pull leaves nothing behind. Pulling by digest (`…@sha256:…`) pins the exact image.

## Not done yet

- Streaming upload of chunks larger than memory. Chunks are held in memory, so keep `--chunk-mib` modest.
- Using a pulled image as a read-only DiskImageKit base layer with an ASIF cache layer on each Mac (macOS 27), instead
  of a full local copy.
- Reading Tart's OCI image format.
