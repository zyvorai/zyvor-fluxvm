<!-- Copyright 2026 Zyvor AI Labs · https://zyvor.dev -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0 -->
# Design: a Docker-style workflow for macOS guests

Status: **proposal, nothing implemented.** It lives in this repository for now; it can move to its own project once the shape
is settled, because it only needs FluxVM's daemon API.

## Goal

A developer or CI job on an Apple silicon Mac gets the Docker habits for **macOS** guests: pull an image by tag, run a throwaway
instance, list what is running, exec into it, throw it away, and build an image from a file.

```
fluxctl mac pull ghcr.io/acme/macos-xcode:15       # a template, from an OCI registry
fluxctl mac run --rm -p 2222:22 macos-xcode:15 -- xcodebuild test
fluxctl mac ps
fluxctl mac exec <name> -- sw_vers
fluxctl mac build -t macos-xcode:15 -f Macfile .    # provision a template from a file
fluxctl mac images | tag | rmi | push
```

(`mac` is a placeholder name.) It is not Docker for Mac and not Linux containers: each "container" is a full macOS VM.

## What already exists (reuse, do not rebuild)

| Need | Existing piece |
|---|---|
| Create a macOS guest | `fluxctl create --image macos --guest macos --install` (IPSW install, `crates/fluxvm-apple/src/macos_install.rs`) |
| A reusable template | a directory with `disk.raw`, `hardware.bin`, `auxiliary.bin`; `scripts/macos-session-template.sh` builds one |
| Instant throwaway copy | `POST /v1/vms` with `image` = a template's `disk.raw`: an APFS `cp -c` clone with a fresh machine identifier |
| Images in a registry | `fluxctl vm-image push/pull` (OCI artifact of a VM directory, chunked, sha256-verified); tags and digests come from the registry reference |
| Other people's images | Tart image import (`crates/fluxvm-image/src/tart.rs`) |
| Exec, files, shell | SSH with the daemon's key; the vsock guest agent for macOS is experimental (verified on one M4, macOS 27.0.1) |
| List and delete | `fluxctl list`, `delete`; port forwards `-p`; virtiofs `-v` |
| Warm instances | `fluxctl macos-pool` (at most 2 slots; not verified on hardware) |

## Hard limits (they shape the design)

- **At most 2 macOS VMs at a time** on one host (`MAX_MACOS_GUESTS`). `ps` and `run` must say "2 of 2" and refuse, not queue silently.
- **No fork or branching.** A saved state is bound to its machine identifier and MAC, so "fast" means *clone a stopped, pre-snapshotted
  template and restore it*, not many restores of one snapshot.
- **Restore needs an unlocked login session** on the host. A headless server needs a dedicated auto-login user with the lock off.
- A clone is instant only on the **same APFS volume** as the template; otherwise it is a full 20+ GB copy.
- A cold first boot of a clone is slow (minutes to SSH on a USB disk); the guest agent starts minutes in.
- Not supported for macOS guests: extra disks, Rosetta, nested virtualization, private networks, console ports, tagged shares.
- FileVault must be off in a template, or key login fails on clones.

## Shape

**Image** = the existing template directory, pushed and pulled with `vm-image`. Layers are not snapshots (those are per-VM and
host-bound): an image is one disk plus hardware and auxiliary files, and "layers" are the registry's chunks.

**Local store** = a directory of templates keyed by reference, with a small index (`name:tag` to digest). `images`, `tag`, `rmi`
operate on it. `pull` and `push` wrap `vm-image`; `import-tart` wraps the Tart importer.

**Instance** = a VM created from a template by APFS clone, labelled `fluxvm.mac=instance` so `ps` is `list` filtered by label, in the
same way the macOS pool uses `fluxvm.pool=macos`. `run --rm` creates, starts, runs the command over SSH, then deletes. `exec`, `cp`
and `logs` are SSH (or the vsock agent when it is enabled and answering).

**Build** = a file of provisioning steps run in a throwaway instance, then stopped and stored as a new template:

```
FROM macos-base:15            # a template, or: IPSW latest
USER builder
RUN  softwareupdate --install-rosetta --agree-to-license
COPY ./ci-tools /opt/ci-tools
RUN  brew install xcodes && xcodes install 15.4
SNAPSHOT warm                 # optional: also save running state (host-bound; documented as such)
```

Each `RUN` executes over SSH in the build instance. There is no per-step layer cache in v1: the template is the single result,
and a changed file rebuilds from `FROM`. A cache keyed by step plus parent digest is possible later by storing intermediate templates
(each is a 20+ GB APFS clone, so it needs a size budget).

## Phases

1. **Store and registry.** `images`, `tag`, `rmi`, `pull`, `push` over `vm-image`; the index file; tests with a local registry.
2. **Run and ps.** `run [--rm]`, `ps`, `rm`, `exec`, `cp` from a template, honouring the 2-VM limit and the unlocked-session check
   (`LoginSession`), with the same clear 409 and 503 messages the pool already uses.
3. **Build.** The step file, `FROM`/`RUN`/`COPY`/`USER`, and the provisioning account (`apple.provision_*`) with no manual Setup Assistant.
4. **Warm start.** Use `macos-pool` slots for `run` when a matching warm slot exists; measure on real hardware first.

## Open questions

- A name. `fluxctl mac ...`, or a separate binary that only talks to the daemon (the cleaner split if this becomes its own project)?
- Apple's licence terms for redistributing macOS images through a registry: pushing a template to a *public* registry is likely not
  allowed; private registries need a statement in the docs. Needs a legal read, not an engineering decision.
- Is `ps` per host enough, or should it list across a fleet (the macOS pool and `fluxvm-agent` already carry labels)?
- Should `build` support `SNAPSHOT`, given that a saved state cannot be moved to another Mac?

## Not verified

This is built from reading the source and docs. Nothing here was run: the clone, pool, vsock agent and restore behaviour are as
documented in `docs/macos.md` and `docs/macos-architecture.md`, several parts of which are marked unverified there.
