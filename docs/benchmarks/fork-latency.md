# Fork and first-command latency (published)

Numbers from the agent-sandbox audit on a **12-core Xeon E-2336**, `fluxvm_engine = "kvm"`, 256 MiB guest,
2.1 GB Ubuntu/Node rootfs (full distro, not minimal), XFS with reflinks on a loopback image.
Re-run: `scripts/bench-first-command.sh`, `scripts/bench-fork.sh`.

| Metric | p50 | p95 | Notes |
|--------|-----|-----|-------|
| Cold create → first command | 9.8 s | — | Guest uptime ≈ 8.8 s when the agent first answered; ext4 was 26–32 s (full disk copy) |
| Fork → first command | 7.9 s | 12.8 s | |
| Fork API only | 4.3–9.2 s | — | Parent snapshot ≈ 0.23 s; identity reset ≈ 3.5 s |

**Not claimed:** sub-second cold create or &lt;100 ms fork. Those need a minimal guest image and a cheaper identity reset
(see [agent-sandbox-gaps.md](../agent-sandbox-gaps.md), [fork-and-restore.md](../fork-and-restore.md)).
Apple `vz` restore stays on Apple's restore path (~seconds) and is not in this table.
