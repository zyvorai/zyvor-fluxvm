# Contributing to FluxVM

## Build

`fluxvm-image` depends on the sibling repo `zyvorai/guestkit` by a relative path (`../../../guestkit`), so cargo needs a guestkit checkout next
to this one, at the tag in `packaging/guestkit.ref`. `scripts/check-guestkit-pin.sh . ../guestkit` checks that the ref, `Cargo.lock`, the
workflow checkouts and your checkout agree (see [docs/linux-package.md](docs/linux-package.md#releasing-the-guestkit-pin)).

```bash
make build          # cargo build --workspace --all-targets
make test           # cargo test --workspace
make test-policy    # python control-plane tests (no rustc required)
make check          # fmt-check, clippy, test-policy, preflight, test-devops, test-packetflow
make bpf            # scripts/build-ebpf.sh
make preflight
./scripts/test-security-profiles.sh   # Phase 6 measured + confidential control plane (no TEE hardware)
./scripts/release-checklist.sh   # docs + production python suites
```

## PR bar

- Apache-2.0 headers on new files
- Changelog entry under `## Unreleased` at the top of `CHANGELOG.md`
- Docs next to the feature (`docs/` + README TOC if it is operator-facing)
- Operator-facing production/auth/tenant/probe changes also update
  [docs/PRODUCTION.md](docs/PRODUCTION.md),
  [docs/tutorials/production/](docs/tutorials/production/), and
  `examples/create-vm-prod.json` when the create shape changes
- Tests: Rust `#[cfg(test)]` and/or `scripts/test-*.py`
- Do not write Cilium-private BPF maps
- Do not expand `extra_args` as a tenant feature

## Layout

See [docs/architecture.md](docs/architecture.md#project-layout). Network Fabric lives in `crates/fluxvm-network`
+ `bpf/`. Kubernetes packaging is `deploy/k8s/` + `crates/fluxvm-kube`.
Secure Containers (containerd runtime-v2) lives in `crates/fluxvm-container-*` /
`crates/fluxvm-containerd-shim` + `deploy/containerd/` — see
`docs/secure-containers.md`, `docs/secure-containers-set3.md` (events + OCI
process hardening), `docs/secure-containers-set4.md` (Pod-UID volumes + guest
security), and `docs/secure-containers-set5.md` (VSOCK stdio + TTY/PTY).
