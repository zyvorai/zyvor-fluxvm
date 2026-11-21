<!-- Copyright 2026 Zyvor AI Labs · https://zyvor.dev -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# macOS images: `fluxctl mac`

`fluxctl mac` manages a local store of **macOS guest templates** and moves them to and from an OCI registry. It is the image half of the
Docker-style workflow proposed in [macos-containers-design.md](macos-containers-design.md). A "container" here would be a full macOS VM,
not a Linux container.

Status: the commands below are implemented and unit-tested. Nothing has been run against a real registry or a real macOS VM.

## What is available, and what is not

| Command | State |
|---|---|
| `fluxctl mac images`, `pull`, `push`, `tag`, `rmi`, `import` | Implemented (this page) |
| `fluxctl mac run [--rm] [--name N] [--user U] [--wait S] [-p H:G] IMAGE [-- CMD]`, `ps`, `rm`, `exec [--user U]` | Implemented, needs the daemon (`--server` or a context). **Unit-tested only; never run against a real VM.** An instance is a VZ macOS VM, an APFS clone of the image's disk, labelled `fluxvm.mac=instance`. A 409 means the Mac already runs 2 macOS guests; a 503 means its login session is locked. |
| `fluxctl mac build -t NAME [-f Macfile] --dry-run [CONTEXT]` | Parses a Macfile and prints the plan, with line-numbered errors. Only `--dry-run` works: running the steps in a build VM is **not implemented**. A `#` comment must be on its own line (a trailing one after a directive is an error). |

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

- **At most 2 macOS VMs at a time** on one host (`MAX_MACOS_GUESTS`). When `run` exists it is meant to refuse at the limit, not queue.
- **No fork or branching.** A saved state is bound to its machine identifier and MAC, so "fast" means cloning a stopped template, not
  many restores of one snapshot.
- **Restore needs an unlocked login session** on the host. A headless server needs a dedicated auto-login user with the lock off.
- A clone is instant only on the **same APFS volume** as the template; otherwise it is a full 20+ GB copy. This applies to `import`
  (`cp -c`) as well as to later clones.
- FileVault must be off in a template, or key login fails on clones.
- Not supported for macOS guests: extra disks, Rosetta, nested virtualization, private networks, console ports, tagged shares.
- **Apple's licence terms limit where macOS images may be stored.** Pushing a template to a public registry is likely not allowed; use
  a private registry. This is a caution, not a legal reading; check the terms that apply to you.

## Not verified

Written from the source. The commands are covered by unit tests on a temporary store; none was run against a real registry, and no
imported or pulled template was booted.
