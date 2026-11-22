# FluxVM — Product Positioning

**One control plane. Four backends. A real API.**

**FluxVM** is a Rust-native control plane for creating and managing secure, isolated
virtual machines on Linux via Firecracker, Cloud Hypervisor, QEMU/KVM, or the in-tree
FluxVM hypervisor. Short-lived and disposable workloads (TTL cleanup, cheap CoW clones)
are a first-class *use* of FluxVM — not the product’s identity.

It is **not** positioned as "the VM engine under Fabric" first and a standalone product second — that gets the emphasis backwards. FluxVM is a complete, independently useful control plane on its own. It's also the VM engine other Zyvor products (Fabric, Ragnarok) build on, the same way a database is both usable directly and the thing an ORM sits on top of.

Part of the [Zyvor](https://zyvor.dev) product family from Zyvor AI Labs.

---

## Elevator pitch

> FluxVM manages secure, isolated virtual machines — via Firecracker, Cloud Hypervisor, QEMU/KVM, or its own in-tree hypervisor — from one Rust-native control plane with a real REST API. Run it standalone, or as the VM engine under another Zyvor product; it's the same binary and the same API either way. Use TTL and CoW when you want disposable compute; leave them off for longer-lived guests.

The strongest concrete hook is **a real REST API and a JSON VM spec** on every backend. Everything else in this doc builds on that.

---

## Who this is for

| Persona | Role | What they care about | Where FluxVM fits |
|---------|------|----------------------|--------------------|
| **CI/platform engineer building ephemeral infra** | Needs VM-per-job isolation that actually cleans up | TTL-guaranteed cleanup, no network path for untrusted code, cheap CoW cloning | Optional `ttl_seconds`, `network.mode: "none"` + vsock `exec`, qcow2 overlays — see [Use cases](index.md#use-cases) |
| **Kubernetes platform engineer wanting VMs** | Runs a K8s cluster, needs real VMs for some workloads | A lightweight path that still feels Kubernetes-native | `DisposableVm` CRD + `fluxvm-kube` operator, or `fluxvm-microvm` for scheduler-driven placement — see [docs/microvm.md](../docs/microvm.md) |
| **AI-agent builder who wants a disposable machine per task, on a Mac** | Runs coding agents (Claude Code, Cursor, Codex) locally or on a few Macs | Code and prompts that never leave the machine, a clean VM per attempt, trying several fixes in parallel | `fluxctl mcp install` plus offline sandboxes that fork into running copies in about 4 s (measured on an M4 mini; [agent-sandbox-demo](https://github.com/zyvorai/zyvor-fluxvm/tree/main/examples/agent-sandbox-demo)) |
| **Mac team lead with a shelf of Mac minis and Studios** | Runs CI and shared dev machines | One API, tenants and leases, quotas, volumes, SSH without exposing the Macs, graphs | The fleet platform ([mac-cloud-tenancy.md](mac-cloud-tenancy.md)); tested against simulated Macs, not yet a real multi-Mac fleet |
| **Economic buyer evaluating build-or-adopt for a host-local VM layer** | Deciding whether to build this in-house or adopt FluxVM | Whether the maturity level matches the use case, whether it's a maintained project or a dead end | Zyvor Production Licence v2.0. A 30-day trial is available on request from sales@zyvor.dev (a paid evaluation Order at no charge); production and any other use needs a paid Order., actively developed, but read the [maturity caveat](proof-and-status.md#maturity-whats-real-today) honestly before committing — this is not yet a finished multi-tenant security boundary |

The economic-buyer row matters here specifically because FluxVM is explicit about *not* being finished in one dimension (multi-tenant security hardening) while being solid in others (core VM lifecycle, Network Fabric GA, k3s-verified Kubernetes operator). A buyer should read the [maturity caveat](proof-and-status.md#maturity-whats-real-today) as the actual scoping tool, not a boilerplate disclaimer.

---

## What FluxVM Is

| Dimension | Positioning |
|-----------|-------------|
| **Category** | Host-local VM control plane |
| **Analogy** | A host-local VM engine with a REST API and JSON specs — also strong for short-lived VMs when you opt into TTL/CoW |
| **Runtime** | `fluxvm` CLI + `fluxctl serve` daemon — one binary, direct netlink networking |
| **Scope** | VM lifecycle across 4 backends, image build/catalog, per-VM networking (incl. eBPF dataplane), Kubernetes CRDs, multi-host fleets |
| **Interfaces** | CLI (`fluxvm`), REST API, Kubernetes CRDs (`DisposableVm`, MicroVM) |

---

## Standalone vs. via another Zyvor product

FluxVM is designed to be adopted **standalone** first. The two other Zyvor products that use it as their VM engine — [zyvor-fabric](../docs/zyvor-fabric.md) and [Ragnarok](../docs/ragnarok.md) — are optional orchestration/UX layers on top of the same REST API, not a requirement for using FluxVM.

| | Run FluxVM directly | Run it via Fabric | Run it via Ragnarok |
|---|---|---|---|
| What you get | `fluxvm` CLI + REST API, full control over VM JSON specs | Fabric's CLI/Web/K8s-operator/Terraform UX, plus auth/RBAC/network-policy layered on top | AI-assisted VM management, OIDC/SSO, RBAC via Ragnarok's FluxVM Hub |
| When to pick it | You want the smallest possible footprint, or you're building your own orchestration on top | You want a private-cloud control plane with multi-tenant auth and a web console | You want `DisposableVm` CRs created and managed through an AI-assisted hub with enterprise SSO |
| Does it require the others? | No — this is the base layer | Fabric talks to a local `fluxctl serve` over REST; it doesn't fork or vendor FluxVM | Ragnarok creates `DisposableVm` CRs against `fluxvm-kube`; same relationship |

If you're not sure which layer you need: start with FluxVM directly. Both Fabric and Ragnarok are additive — adopting FluxVM standalone never locks you out of adding either later, since they talk to the same unmodified API.

---

## When to look elsewhere

- **You need a finished multi-tenant security boundary today** — see the [maturity caveat](proof-and-status.md#maturity-whats-real-today).
- **You need full CDI or non-QEMU Secure Containers VMM parity** — Secure Containers is GA with documented scope boundaries; see [secure-containers.md](secure-containers.md).
- **You need published boot-latency/density/throughput numbers for capacity planning** — measure with [capability-figures.md](capability-figures.md) / benches; don't cite unpublished SPECs as product claims.

---

## Messaging Guidelines

### Say

- "FluxVM — Rust-native VM control plane, standalone or as another product's VM backend"
- "A host-local VM control plane with a real REST API"
- "A throwaway computer for every agent task on your Mac: offline by default, forked into running copies in seconds" (cite the measured demo and its caveats)
- "The same binary and API whether you run it directly or through Fabric/Ragnarok"
- **Supports disposable / short-lived workloads via TTL and CoW when you need them**
- "Firecracker isolation layering for production hosts; density figures optional for microVM packing" ([capability-figures.md](capability-figures.md))

### Avoid

- Leading with "Disposable Compute Engine" or defining FluxVM *as* disposable-only — disposable is a use pattern, not the category
- Leading with "FluxVM is the engine under Fabric" — that undersells it as standalone-adoptable
- Claiming the Mac cloud platform is proven at fleet scale: it was run against simulated Macs, not several real ones
- Advertising self-service sign-up or Stripe billing as proven: both are off by default, sign-up confirms the address through the operator's mail webhook (not run against a real mail service), and Stripe has only been run against a mock
- Citing unpublished boot-latency, density, or throughput numbers — measure first; see [capability-figures.md](capability-figures.md)
- Softening the MVP/multi-tenant-security-boundary caveat to sound more finished than it is
- Claiming unrestricted hostPath for Secure Containers — GA with documented scope boundaries

---

## License

Zyvor Production Licence v2.0 (SPDX `LicenseRef-Zyvor-Production-2.0`), applied to the entire repository. A 30-day trial is available on request from sales@zyvor.dev (a paid evaluation Order at no charge); production and any other use needs a paid Order. See [README.md — License](https://github.com/zyvorai/zyvor-fluxvm/blob/main/README.md#license) and [NOTICE](https://github.com/zyvorai/zyvor-fluxvm/blob/main/NOTICE). Fabric and Ragnarok, which use FluxVM as their engine, are separate products with their own licensing.

---

## Links

- Product: [zyvor.dev](https://zyvor.dev)
- Repository: [github.com/zyvorai/fluxvm](https://github.com/zyvorai/fluxvm)
- Product overview + metrics: [PRODUCT_OVERVIEW.md](PRODUCT_OVERVIEW.md)
- Exhaustive feature checklist: [../FEATURES.md](https://github.com/zyvorai/zyvor-fluxvm/blob/main/FEATURES.md)
- Use cases: [../docs/use-cases.md](../docs/use-cases.md)
- Documentation map: [index.md#documentation-map](index.md#documentation-map)
