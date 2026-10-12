# Agent density on a Mac mini or Mac Studio

How to run many small Linux agent sandboxes on one Apple Silicon Mac with the `vz` backend. The sandbox API itself is described in
[macos-sandboxes.md](macos-sandboxes.md); this page covers sizing, admission, idle reclaim and the warm pool.

## Profiles

`POST /v1/sandboxes` takes a `profile`:

| Profile    | vCPUs | Memory   | Use                                  |
|------------|-------|----------|--------------------------------------|
| `tiny`     | 1     | 512 MiB  | shell-and-files agents; the densest  |
| `small`    | 1     | 1 GiB    | light coding agents                  |
| `standard` | 2     | 2 GiB    | the default sandbox shape            |

```json
{"name": "agent-1", "profile": "tiny", "ttl_seconds": 600}
```

An explicit `vcpus` or `memory_mib` wins over the profile. Warm slots are standard-sized, so `tiny` and `small` sandboxes cold-boot
(about 8 s with `debian-13`); `"image": "agent-micro"` gives them the [vsock guest agent](agent-micro.md) instead of SSH. A `standard` profile is the same as
no profile and can claim a warm slot. See [examples/sandbox-tiny.json](../examples/sandbox-tiny.json).

Container sandboxes ([oci-sandboxes.md](oci-sandboxes.md)) are the lightest shape: a container image booted straight into its
entrypoint with no systemd, cloud-init or sshd, 1 vCPU and 512 MiB unless a profile or size says otherwise. Profiles, admission,
quotas and TTLs apply to them as to any sandbox. They have their own warm pool (`apple.oci_warm_slots`, off by default), separate
from `sandbox.warm_slots`; see [below](#density-report-and-metrics) and [oci-sandboxes.md](oci-sandboxes.md#warm-pool).

## Admission on real memory pressure

The pressure gate in `policy` (all off by default) now works on macOS too. On a Mac the daemon samples available memory from
`vm_stat` (free + inactive + speculative + purgeable pages) and the kernel's own verdict, `kern.memorystatus_vm_pressure_level`
(normal, warn, critical):

```toml
[policy]
deny_host_pressure_level = "warn"     # refuse creates while macOS reports warn or critical
min_host_mem_available_mib = 4096     # keep 4 GiB free after the new VM's memory is counted
pressure_defer_secs = 30              # wait up to 30 s for pressure to clear before refusing
```

A refused create is reported with the reason (for example `host under memory pressure: level warn reaches
policy.deny_host_pressure_level (warn)`) and audited as `quota.deny`. The PSI thresholds (`max_host_mem_psi_*`) are Linux-only and
are skipped on a Mac.

## Idle sandboxes

Three idle stages, all off by default and all driven by the same scan (`sandbox.autopause_scan_secs`). Each `vz` sandbox is labelled
`fluxvm.sandbox`, so only sandboxes are touched, never an ordinary `vz` VM.

| Setting                         | After this long idle                  | What it frees                        | Comes back on the next request |
|---------------------------------|---------------------------------------|--------------------------------------|--------------------------------|
| `sandbox.idle_balloon_secs`     | inflate the balloon                   | `idle_balloon_percent` of its memory | deflated, instantly            |
| `sandbox.autopause_idle_secs`   | pause the VM                          | CPU only (memory stays resident)     | resumed, instantly             |
| `sandbox.hibernate_idle_secs`   | save memory to disk and stop the VM   | all of its memory                    | restored, about 2 s            |

"Request" means any sandbox API call: `/process`, `/fs/read`, `/fs/write` and the MCP tools. A hibernated sandbox shows as `stopped`
with the label `fluxvm.hibernated`. Hibernate uses snapshot restore, so it needs an unlocked login session; when the restore fails
(locked screen, macOS updated in between) the sandbox cold-boots instead, losing what was in memory but keeping its disk.
Set the stages in increasing order:

```toml
[sandbox]
idle_balloon_secs = 60
autopause_idle_secs = 180
hibernate_idle_secs = 900
```

## Warm pool

`sandbox.warm_slots` (default 2 on a Mac) keeps that many stopped, snapshotted VMs ready. Before a burst of creates, fill it further:

```bash
fluxctl --server http://127.0.0.1:7788 sandbox warm --count 8
curl -s -X POST localhost:7788/v1/sandboxes/warm -H 'Content-Type: application/json' -d '{"count": 8}'
# -> {"target": 8}   (202; slots are built one at a time in the background, about 8 s each)
```

`count` is 1-64 and never below `warm_slots`. After the burst, the pool refills to `warm_slots` only; surplus slots stay until claimed
or deleted. Each slot costs disk (an APFS clone plus its saved memory), not RAM. Warm requests and the density report are host-wide,
so a tenant-scoped token is refused.

## Density report and metrics

```bash
fluxctl sandbox density            # or: curl -s localhost:7788/v1/sandboxes/density
```

```json
{
  "warm_slots_configured": 2, "warm_slots_ready": 2, "warm_hits": 14, "warm_misses": 3,
  "active_sandboxes": 9, "paused_sandboxes": 4, "hibernated_sandboxes": 11,
  "resident_estimate_mib": 6656,
  "host_mem_available_mib": 9120, "host_mem_total_mib": 36864, "host_pressure_level": "normal",
  "oci_warm_slots_configured": 2, "oci_warm_slots_ready": 2, "oci_warm_slots_booting": 0,
  "oci_warm_resident_mib": 1024, "oci_warm_hits": 31, "oci_warm_misses": 2, "oci_warm_last_claim_ms": 410
}
```

The `oci_warm_*` fields describe the container sandbox warm pool ([oci-sandboxes.md](oci-sandboxes.md#warm-pool)): waiting slots
hold their memory, so `oci_warm_resident_mib` is not part of `resident_estimate_mib`.

`resident_estimate_mib` is the configured memory of running and paused sandboxes (a balloon gives some of it back). `/metrics` adds
`fluxvm_sandbox_warm_hits_total` and `fluxvm_sandbox_warm_misses_total`.

## What a sandbox costs on a real Mac

Measured on one Apple M4 Mac mini (10 CPUs, 16 GB, macOS 27.2) with `scripts/bench-sandbox-mac.sh`, a `debian-13` guest and
the daemon on loopback. Each figure is the resident size macOS reports for the sandbox's runner plus its Virtualization XPC
service (`usage[].rss_bytes` of `GET /v1/host/overview`), sampled after the sandboxes had idled for 20 to 45 seconds.

| Profile (configured) | Sandboxes | Create, each | RSS per sandbox, MiB: lowest / mean / highest seen |
|---|---|---|---|
| `tiny` (512 MiB, cold boot) | 1 | 10 s | 498 |
| `tiny` | 2 | 7 s | 770 / 775 / 779 |
| `tiny` | 4 | 6 s | 347 / 520 / 628 |
| `standard` (2 GiB, claims a warm slot) | 1 | 1 s | 712 |
| `standard` | 2 | 1 s | 248 / 275 / 303 |
| `standard` | 4 | 12 s (2 warm slots, 2 cold) | 205 / 460 / 687 |
| `small` (1 GiB, cold boot) | 1 | 8 s | 785 |
| `small` | 2 | 6 s | 784 / 786 / 788 |
| `small` | 4 | 7 s | 623 / 704 / 786 |

What these numbers say, and what they do not:

- **They vary a lot** (205 to 788 MiB across 21 sandboxes), and the same sandbox changes with time: one `tiny` sandbox sat at
  779 MiB for the first minute and 704 MiB at 90 s; a warm-claimed `standard` one started at 716 MiB and settled at 485 MiB
  within 15 s. macOS compresses and reclaims guest memory, and a cold boot touches more pages than a restored snapshot.
- **A sandbox's resident size is not its configured size.** A `tiny` (512 MiB) sandbox held more than 512 MiB at times,
  because the runner and the Virtualization service add their own footprint to the guest's pages.
- **The lowest figure is a floor, not a plan.** Sizing a Mac on 205 MiB would overstate what fits: sandboxes that boot or
  run work sit near the highest figure, about 790 MiB here. Use roughly 800 MiB per *active* sandbox when deciding how many
  to run, and expect idle, hibernated and warm-restored ones to cost less.
- **The "host available" drop was too noisy to publish.** The Mac was running other work: available memory moved by more
  than 1 GiB between samples with no sandbox change, so the script's drop column is only meaningful on a quiet Mac.
- **Not measured:** a guest doing real work (these were idle), Mac Studio sizes, 8 or more sandboxes at once, and the memory
  of the daemon itself. Memory pressure stayed `normal` throughout.

A reasoned estimate from those figures, not a measurement: a 16 GB Mac that keeps about 6 GB for macOS and other apps runs
roughly a dozen active sandboxes of these shapes (10 GB at about 800 MiB each). Hibernated sandboxes hold no memory, so many
more can exist; they are limited by disk. Run `scripts/bench-sandbox-mac.sh` on your own host for its real number; the older
`scripts/density-smoke.sh` creates a batch, runs a command in each and prints the density report.

## Unattended hosts

Snapshot restore (warm slots, hibernate) needs an unlocked login session. On a headless Mac: a dedicated user with automatic login,
the daemon started by launchd in that user's session, and the screen lock off. See [deploy/launchd-notes.md](../deploy/launchd-notes.md).
