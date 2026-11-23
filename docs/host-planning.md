# Planning reports: `host-compat`, `place` and `bench-report`

Three offline commands that answer a question before anything is started. They read JSON files, write text (or
`--json`), and need no daemon, VM, `/dev/kvm` or privileges. Background and sources: [RESEARCH.md](RESEARCH.md).

Fixtures to start from: [`examples/planning/`](../examples/planning/).

## `fluxctl host-compat check SOURCE DESTINATION [--spec VM.json] [--strict] [--json]`

Can a VM move from the source host to the destination (live migration or snapshot restore)? Both hosts are JSON
host descriptions ([schemas/host-description.schema.json](../schemas/host-description.schema.json)): architecture,
kernel, CPU vendor, family/model/stepping and flags (the `flags` line of `/proc/cpuinfo`), hypervisor versions, free
memory and, optionally, the host graph that `GET /v1/host/topology` returns under `topology`.

```bash
fluxctl host-compat check host-a.json host-b.json
fluxctl host-compat check host-a.json host-b.json --spec vm.json --json
```

```text
error   cpu.flags  the destination lacks 2 CPU flag(s) the source has: avx512bw, avx512f; the guest sees the host CPU, so it may already use them and fault after the move
        fix: use a CPU template or a lower-common-denominator model, or choose a newer destination
warning cpu.model  CPU family/model/stepping differ: 6/106/6 to 6/85/7 (Intel(R) Xeon(R) Gold 6338 to ...); ...
host-a -> host-b: INCOMPATIBLE (1 error, 1 warning, 0 notes)
```

Exit status is 1 when the verdict is `incompatible` (and with `--strict` on warnings too). Without `--spec` the check
covers every hypervisor the source reports and treats missing CPU flags as a warning, because it does not know what
the guest sees; with `--spec` it is the one VM's backend, CPU template, memory, huge pages, NUMA binding and
`vfio_devices`.

| Rule | Severity |
|---|---|
| Architecture differs; CPU vendor differs | error |
| Destination lacks a CPU flag the source has | error with a VM that has no `cpu_template`; warning with a template or without `--spec` |
| Family/model/stepping differ | warning |
| Hypervisor missing on the destination | error |
| Cloud Hypervisor: destination older than the source, or more than two releases newer | error |
| QEMU: destination older than the source | warning (backward migration is not promised) |
| Firecracker: destination older; different CPU model without a `cpu_template`; host kernel older | error / error / warning |
| `backend: vz` | error (Apple guests do not migrate between hosts) |
| Destination free memory, free huge pages, or a bound NUMA node missing | error |
| Passthrough devices | warning (only devices with migration support can move live); error on Firecracker, or when the device on the destination is already assigned |

`compatible` means "no known blocker", not a guarantee. It cannot see microcode, firmware or kernel-module
differences. The in-tree `fluxvm` backend is reported as "not modelled".

## `fluxctl place advise --host HOST.json --spec VM.json [--json]`

Where does one VM fit on a host? `HOST.json` is the topology the daemon builds (`numaNodes`, `devices`) or a host
description that has a `topology` key. The report lists each NUMA node with whether it fits (CPUs, free memory, free
huge pages, node of the passthrough devices), recommends one, and suggests `numa_node`, a `cpuset` of whole cores
(hyper-thread siblings together) and `hugepages`. It checks the IOMMU group of every passthrough device (a group is
assigned whole), devices already assigned, devices on different nodes, memory sizes that are not a multiple of any
huge page size, and, when no node fits alone, the nearest nodes to span and their SLIT distance. Exit status 1
when the VM does not fit or an explicit `numa_node` is wrong.

It does not know which CPUs other VMs already pin: check the suggested `cpuset` against running VMs. Only the `qemu`
backend uses `numa_node`, `cpuset` and `hugepages` (see [`vm-spec check`](operations.md#checking-a-vm-spec-fluxctl-vm-spec-check)).

## `fluxctl bench-report check FILE` and `compare BASE NEW [--threshold-pct 10]`

A report format for boot-time and density runs ([schemas/bench-report.schema.json](../schemas/bench-report.schema.json),
`fluxvm-bench/1`) that keeps the raw samples and the method: where the clock starts and stops, page-cache state,
backend, guest image and density per vCPU. `check` validates it and prints p50/p95/p99 (nearest rank). `compare`
refuses to compare reports whose method differs (exit 2), and marks a series as a regression (exit 1) when its p50 or,
with at least 100 samples on both sides, its p99 gets worse by more than the threshold; `lower_is_better: false`
series (throughput) invert. It is a threshold test, not a significance test: use enough samples and the same host.
The existing `scripts/bench-*.sh` print averages and do not write this format yet.

## Limits

Files over 2 MiB (16 MiB for bench reports) are refused; at most 2048 CPU flags, 1024 NUMA nodes, 8192 devices,
64 series and 200000 samples per series are read; text taken from a file is stripped of control characters and
shortened before it is printed.
