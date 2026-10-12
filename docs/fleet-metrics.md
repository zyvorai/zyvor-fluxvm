# Fleet metrics: graphs and Prometheus

Central samples every healthy Mac once every 15 seconds and keeps a day of history in memory. The console draws it
(VM details, Mac details, the overview), `fluxctl fleet top` and `fluxctl fleet metrics` print it, and `GET /metrics`
exposes the fleet to Prometheus.

## What is measured

Each node serves `GET /v1/stats` (on its `fluxctl serve`): one sample of the host and every VM on it. Everything that
moves is a cumulative counter, so central takes rates between two samples.

| | Where it comes from |
|---|---|
| Mac CPU % | Busy share of all cores: `host_statistics(HOST_CPU_LOAD_INFO)` on macOS, `/proc/stat` on Linux |
| Mac memory | Total and available (`vm_stat` / `/proc/meminfo`, the same reading memory-pressure admission uses) |
| Mac disk free | `statvfs` of the volume that holds the state directory (the VM disks) |
| VM count | Every VM the daemon knows, and the running ones |
| VM CPU % | CPU seconds over the interval, divided by the VM's vCPUs (0-100%) |
| VM memory | Resident memory on the host |
| VM disk read/write | Bytes per second |
| VM network in/out | Bytes per second, where measurable (below) |

Per-VM numbers depend on the backend; every sample says which (`source`):

- **`vz-process`**, VMs on Apple's Virtualization framework: the runner process plus the
  `com.apple.Virtualization.VirtualMachine` XPC service that actually runs the guest, read with `proc_pid_rusage`
  (CPU time, resident size, `ri_diskio_bytesread/written`). The service is matched to its VM by the files it holds
  open in the VM's workspace (see `fluxvm_apple::vm_usage`).
- **`vz-runner`**: the XPC service could not be matched, so CPU and memory are the runner process only and **do not
  include the guest**. The console says so under the graphs.
- **`cgroup`**: QEMU, Cloud Hypervisor and Firecracker VMs on Linux, read from the VM's cgroup v2 (`cpu.stat`,
  `memory.current`, `io.stat`), with network bytes from its TAP device.
- **`none`**: the VM is stopped, or nothing could be read. Nothing is drawn rather than a made-up zero.

Known gaps, stated plainly:

- **No network numbers for `vz` guests.** Virtualization.framework has no per-VM network counters; the graph says
  "not measured". (The simulated Mac reports network so the graph can be tried.)
- Memory for a `vz` guest is the host's resident size of its processes, which is not the guest's own view of its
  memory (a guest that touched memory once and freed it still shows it until macOS reclaims it).
- A TAP device inside a separate network namespace is not visible to the node; such VMs show no network numbers.
- The first sample after a VM (or Mac) appears has no rates yet: CPU and I/O need two samples.
- History lives in central's memory: it is empty after central restarts, and in an HA pair only the leader collects
  (a standby that takes over starts a new history). Use Prometheus for anything long-term.

## How central keeps it

- A collector polls the healthy nodes every 15 s (`--metrics-interval-secs`), at most 16 at a time, each with a 5 s
  timeout and a 4 MiB cap on the answer. Only the HA leader collects.
- Samples are averaged into one point per minute and kept for 24 hours.
- Memory is bounded: at most 1000 VM series and 500 Mac series (about 50 KiB each when full). A VM missing from its
  Mac's sample (deleted) loses its series at once; a deregistered Mac loses its own and its VMs'. Samples past the
  cap are counted in `fluxvm_central_metrics_series_dropped_total`.
- A counter that goes backwards (a VM restarted) gives no rate for that interval, never a negative one.

## API

| Route | Who | What |
|---|---|---|
| `GET /fleet/metrics/vms/{node}/{id}?range=1h\|6h\|24h` | the VM's owner, operator | One VM's points (`cpu_percent`, `memory_used_mib`, `disk_read_bps`, `disk_write_bps`, `net_rx_bps`, `net_tx_bps`), its latest sample, `source` and a `note` about anything not measured |
| `GET /fleet/metrics/nodes/{name}?range=` | operator; a tenant for the Macs it leases | One Mac's points (`cpu_percent`, `memory_used_mib`, `memory_total_mib`, `disk_free_bytes`, `vms`, `vms_running`) |
| `GET /fleet/metrics/vms` | everyone (a tenant sees its own VMs) | The latest sample of every VM, for `fluxctl fleet top` |
| `GET /fleet/metrics/summary` | everyone (a tenant: its leased Macs and own VMs) | CPU and memory utilisation now and over the last hour |
| `POST /fleet/metrics/collect` | operator | Sample every Mac now instead of waiting for the next tick |
| `GET /metrics` | fleet token or `--metrics-token` | Prometheus text format |

A VM or Mac a tenant may not see answers 404, as everywhere in `/fleet`. `range` defaults to `1h`; anything other than
`1h`, `6h` or `24h` is a 400. Points have `t` (Unix seconds, start of the minute) and `null` where nothing was measured.

```sh
curl -s -H "Authorization: Bearer $TOKEN" "$CENTRAL/fleet/metrics/vms/studio-1/$VM?range=6h" | jq '.latest'
```

## CLI

```sh
fluxctl fleet top                    # VMs by CPU: CPU %, memory, disk and network rates
fluxctl fleet top --sort mem -n 10   # the ten biggest by memory
fluxctl fleet metrics ios-build-1 --range 6h
```

`fleet metrics` prints a sparkline per measure with the latest value, average and maximum. Piped, or with `-o json`,
both print the API's JSON.

## Console

- **VM details → Usage**: CPU (share of the VM's vCPUs), memory (of its size), disk read/write and network in/out, with
  a 1h / 6h / 24h toggle. Hover, or focus a chart and use the arrow keys, for the values at a minute.
- **Mac details → Usage**: CPU, memory and VM count, plus free disk space.
- **Overview**: CPU and memory in use across the fleet (operators) or across the tenant's leased Macs, with the last
  hour.

## Prometheus

`GET /metrics` needs the fleet token (`--token`) or a scrape-only token given with `--metrics-token` (or
`FLUXVM_AGENT_METRICS_TOKEN`), which can read nothing else. Tenant tokens are refused. Without either token (lab mode)
it is open until the first tenant exists, like the rest of central.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: fluxvm-central
    scheme: https            # http when central runs without --tls-cert
    metrics_path: /metrics
    scrape_interval: 30s
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/fluxvm-metrics-token
    static_configs:
      - targets: ["fleet.example.com:7799"]
    # With HA, scrape every central: fluxvm_central_ha_leader says which one leads, and only the leader has
    # node and VM samples.
```

What is exported (all names start with `fluxvm_`):

| Family | Labels | |
|---|---|---|
| `fleet_nodes` | `state` = healthy, unhealthy, cordoned | gauge |
| `fleet_vcpus`, `fleet_memory_bytes`, `fleet_vms` | | healthy nodes, from heartbeats |
| `fleet_leases_active`, `fleet_nodes_scrub_pending`, `fleet_nodes_draining` | | gauges |
| `node_up`, `node_cordoned`, `node_vcpus`, `node_vms` | `node` | from heartbeats |
| `node_cpu_utilization_ratio` | `node` | 0-1, between the last two samples |
| `node_cpu_busy_ticks_total`, `node_cpu_ticks_total` | `node` | counters; `rate(busy) / rate(total)` is CPU use |
| `node_memory_bytes`, `node_memory_used_bytes`, `node_disk_free_bytes`, `node_disk_bytes`, `node_vms_running` | `node` | gauges |
| `vm_vcpus`, `vm_running`, `vm_memory_used_bytes`, `vm_cpu_utilization_ratio` | `node`, `vm`, `name`, `tenant` | gauges |
| `vm_cpu_seconds_total`, `vm_disk_read_bytes_total`, `vm_disk_written_bytes_total`, `vm_network_receive_bytes_total`, `vm_network_transmit_bytes_total` | `node`, `vm`, `name`, `tenant` | counters, as the node reports them |
| `tenant_vms`, `tenant_vcpus`, `tenant_memory_bytes`, `tenant_leases_active`, `tenant_disabled` | `tenant` | from the ledger |
| `central_ha_enabled`, `central_ha_leader` | | this central |
| `central_webhook_deliveries_total` | `result` = ok, failed | counter |
| `central_http_requests_total` | `method`, `route` (the route pattern, not the path), `status` | counter |
| `central_metrics_polls_total` | `result` | counter |
| `central_metrics_series`, `central_metrics_series_dropped_total` | `kind` | the collector's own bounds |

Label values are escaped as the text format requires (`\\`, `\"`, `\n`); route labels are patterns such as
`/fleet/vms/{node}/{id}`, so their number stays bounded.

Example queries:

```promql
# VM CPU as a share of its vCPUs, from the raw counter
rate(fluxvm_vm_cpu_seconds_total[5m]) / fluxvm_vm_vcpus

# Mac CPU
rate(fluxvm_node_cpu_busy_ticks_total[5m]) / rate(fluxvm_node_cpu_ticks_total[5m])

# Memory in use per tenant
sum by (tenant) (fluxvm_vm_memory_used_bytes)

# Central error rate
sum(rate(fluxvm_central_http_requests_total{status=~"5.."}[5m]))
```
