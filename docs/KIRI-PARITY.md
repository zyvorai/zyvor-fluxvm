# Kiri parity roadmap

Kiri (https://github.com/libracoder/kiri) is a lightweight Docker-compatible
container runtime. FluxVM is a multi-hypervisor VM control plane. This document
lists the Kiri features FluxVM does not yet have and the plan to close the gaps.

## Already covered by FluxVM

- Lightweight VMs (Firecracker, Cloud Hypervisor, QEMU, in-tree KVM, Apple VZ)
- Guest agent over vsock
- Kubernetes integration (via Kairon)
- eBPF networking
- Mac support (VZ + menu-bar app)
- Density / restore work (UFFD, dirty-log) in progress

## Gaps to close (priority order)

### P0 — Docker Engine API surface
- Minimal Docker Engine API endpoints (`/containers/json`, `/containers/create`,
  `/containers/{id}/start|stop|logs`, `/images/json`) so `docker --context fluxvm`
  works for basic operations.
- Status: not started.

### P0 — Container lifecycle
- `fluxctl run`, `exec`, `logs`, `cp`, `stats` mapped onto the existing guest
  agent and vsock path.
- Status: guest agent exists; Docker-style commands do not.

### P1 — Compose
- Accept a Compose file and create the corresponding VMs / network attachments.
- Status: not started.

### P1 — Image pipeline
- Pull/tag/save/load of OCI images and a BuildKit-compatible build path.
- Status: OCI sandbox path exists; full image management does not.

### P1 — Local domains + HTTPS
- `*.fluxvm.local` resolution and a local CA for HTTPS, similar to Kiri.
- Status: not started.

### P2 — Built-in single-node k3s
- `fluxctl k8s start` that launches a k3s node inside a FluxVM.
- Status: not started (Kairon already runs real clusters).

### P2 — Sharing
- `fluxctl share` that exposes a local port to the internet (Ngrok-style).
- Status: not started.

### P2 — CVE scanning
- `fluxctl scan <image>` using a local Grype or equivalent.
- Status: not started.

### P3 — Container GUI
- Extend the Mac menu-bar app with a container list, logs, and file browser.
- Status: menu-bar app exists; container views do not.

### P3 — USB passthrough
- Attach host USB devices to a guest.
- Status: not a current focus.

### P3 — Import
- Import VMs or images from OrbStack / Docker Desktop.
- Status: not started.

## Implementation notes

- Prefer mapping onto the existing multi-hypervisor API rather than building a
  second container runtime.
- Docker API compatibility should be a thin adapter, not a full dockerd rewrite.
- Each item above should land as its own focused PR.
