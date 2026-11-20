# FAQ and fit

Is this for you, and the questions people ask first.

[Back to README](https://github.com/zyvorai/zyvor-fluxvm/blob/main/README.md)

## Is this for you?

**A strong fit when you…**

- want VM lifecycle without a systemd dependency — direct netlink networking, JSON specs, a real REST API;
- run CI runners, sandboxed execution or per-branch environments and want cleanup you don't have to remember;
- are looking for a Kubernetes-native VM path (`DisposableVm`, `fluxvm-microvm`);
- want to adopt it standalone — no Fabric, Ragnarok or other Zyvor product required.

**Look elsewhere (for now) when you…**

- need a finished multi-tenant security boundary today ([details](proof-and-status.md#maturity-whats-real-today));
- need full CDI / non-QEMU Secure Containers VMM parity (Secure Containers is GA with documented scope boundaries);
- need published performance numbers for capacity planning.

---

## FAQ

**Is FluxVM production-ready?** For the core VM-lifecycle primitives (auth/RBAC, jailer, cgroups, netns), yes — with the caveats in [Proof & status](proof-and-status.md#maturity-whats-real-today). Turn on `policy.require_catalog_names`, the quota ledger, `FLUXVM_VMM_SECCOMP`, and the AppArmor or SELinux profile before exposing it to untrusted tenants. Secure Containers is GA with documented scope boundaries .

**Do I need Fabric or Ragnarok?** No. Clone it, build it, run `fluxctl create`. Fabric and Ragnarok are separate products that use FluxVM as their VM engine — see [Ecosystem](ecosystem.md).

**What storage backends are supported?** qcow2/raw, LVM thin, NBD and Ceph RBD — see [Bring-your-own storage backend](use-cases.md#bring-your-own-storage-backend).

**Are there published boot-latency or density numbers?** The measurement method is `scripts/record-baseline.sh`. Each evidence file is one host, and `avg_create_ms` is API create time, not guest init. It is not a sizing SLA until the same script has been repeated on a second host. Don't treat Firecracker's 125 ms target as a FluxVM number.

**How do I test security profiles without SNP/TDX hardware?** Run `./scripts/test-security-profiles.sh` (same suite as [CI](../.github/workflows/security-profiles.yml)). Measured evidence is always `software-test` — see [docs/guides/security-profiles-howto.md](guides/security-profiles-howto.md).

**What license is this under?** Zyvor Production License v1.0 for the whole repository. Non-production use is permitted; production use requires a commercial license (sales@zyvor.dev) — see [License](https://github.com/zyvorai/zyvor-fluxvm/blob/main/README.md#license).
