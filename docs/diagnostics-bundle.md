# Incident bundles

One call collects what is needed to debug a misbehaving VM after the fact: QMP state, cgroup and PSI numbers, the
console log, network and eBPF state, an optional packet capture and an optional guest memory dump.

```sh
curl -XPOST localhost:7788/v1/vms/$ID/diagnostics -d '{"pcap_seconds": 10, "memory_dump": true}'
# -> 202 {"token": "...", "state": "running", ...}
curl localhost:7788/v1/vms/$ID/diagnostics/$TOKEN              # state running | done | failed
curl -o bundle.tar.gz localhost:7788/v1/vms/$ID/diagnostics/$TOKEN/bundle
curl -o mem.elf localhost:7788/v1/vms/$ID/diagnostics/$TOKEN/memory-dump
fluxctl incident $ID --pcap-seconds 10 --memory-dump --out bundle.tar.gz
```

All routes are admin-only.

## Request

| Field | Default | |
|---|---|---|
| `pcap_seconds` | 0 | 0-30 seconds of `tcpdump` on the VM's dataplane interface (needs the eBPF dataplane) |
| `pcap_filter` | | tcpdump filter |
| `memory_dump` | false | QEMU `dump-guest-memory` as an ELF core, kept on the host |
| `redact` | true | fingerprint secrets, see below |
| `log_lines` | 2000 | console log lines, at most 20000 |

## Bundle contents

Each part is collected on its own; a part that fails (no cgroup, no balloon device, QEMU not running) is listed in
`sections` with its error and the rest still lands. `manifest.json` in the archive is the job with every section.

```
manifest.json
vm/record.json             the VM record (redacted)
vm/metrics.json            CPU %, memory, disk bytes
vm/pressure.json           PSI cpu/memory/io
vm/cpuset.json  vm/frozen.json  vm/numa.json
vm/cgroup/*                cpu.stat, memory.stat, memory.events, io.stat, *.pressure, limits, cgroup.procs
logs/console.log           last N lines of the serial console / VMM log (redacted)
qmp/query-*.json           status, version, kvm, cpus-fast, hotpluggable-cpus, memory-size-summary,
                           memory-devices, balloon, block, blockstats, named-block-nodes, block-jobs, jobs,
                           migrate (+capabilities, parameters), pci, chardev, iothreads, rx-filter, dump
network/status.json        dataplane mode, attachment, BPF pin dir, schema, policy
network/stats.json  network/flows.json  network/drops.json
network/datapath.txt       ip -d -s link, tc qdisc/filters, bpftool net for the VM's interface
network/capture.pcap       when pcap_seconds > 0
host/memory.json           host MemAvailable and PSI
host/info.json             hostname, kernel, FluxVM version, uptime, load
```

## Memory dumps

`memory_dump: true` runs QMP `dump-guest-memory` (ELF, no paging) into
`<diagnostics_dir>/<id>/<token>/memory.elf`, after everything else is collected. QEMU pauses the guest while it writes
and resumes it afterwards; `memory_dump.paused_ms` records how long that took. The dump is not put in the bundle: the
job reports its path, size and sha256, and `GET .../memory-dump` downloads it. Open it with `crash` or `gdb`.

It is refused for non-QEMU backends, VMs that are not running or paused, confidential VMs, guests larger than
`diagnostics_max_memory_dump_mib` (default 16384; 0 disables dumps) and when the diagnostics directory has less free
space than the guest's memory plus 1 GiB.

## Redaction

With `redact` (the default) every JSON value whose key contains `password`, `passwd`, `secret`, `token`,
`private_key`, `api_key`, `credential`, `user_data`, `content` or `psk` becomes `redacted:sha256:<16 hex>`. That covers
cloud-init `write_files` contents, `secret_env`, guest agent tokens and the creating token. Same input, same
fingerprint, so two bundles can be compared. Text files (the console log, `datapath.txt`) and every other JSON
string value (cloud-init `runcmd` lines, for example) have the value after `password=`, `token:`, `secret=`,
`Bearer ` and similar replaced by `[redacted]`.

Redaction is pattern-based: a secret the guest prints without such a key word stays in the console log, and the
packet capture and memory dump are never redacted.

## Storage

`diagnostics_dir` in `/etc/fluxvm.toml` (default `<state_dir>/diagnostics`) holds `<id>/<token>/` with `job.json`,
`bundle.tar.gz` and `memory.elf`. FluxVM keeps the last five bundles per VM, deleting the oldest when a new one starts,
and removes them all when the VM is deleted. One bundle per VM runs at a time; another start gets 409. A bundle still running when FluxVM
restarts reads as failed.
