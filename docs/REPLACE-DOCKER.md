# FluxVM as a Docker replacement

FluxVM does not need to be Docker-compatible. It can be the thing people use instead.

## Why replace instead of emulate

- FluxVM already has its own REST API and `fluxctl`.
- Multi-hypervisor backends (Firecracker, Cloud Hypervisor, QEMU, in-tree KVM, Apple VZ).
- Guest agent over vsock.
- Kubernetes via Kairon and built-in k3s (`fluxctl kube`).
- eBPF networking and density/restore work (userfaultfd, dirty-log).

Emulating the Docker Engine API is optional and not the primary path.

## Primary interface

| Task | Command |
|------|---------|
| Run a workload | `fluxctl run` or REST API |
| Kubernetes app | `fluxctl kube run IMAGE` |
| Cluster | `fluxctl cluster init` |
| Snapshot / restore | REST `snapshot_save` / density path |
| Share a port | `fluxctl share` (stub today) |

## Optional compatibility

A thin Docker API adapter exists in `crates/fluxvm-docker-api` for teams that still have Compose files or CI that calls `docker`. It is not required and is not the focus.

## What this PR changes

- Documents the replacement stance.
- Adds a short operator note on migrating off Docker Desktop / OrbStack.
- Leaves all existing code untouched.
