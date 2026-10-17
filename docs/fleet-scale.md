# Fleet scale: what central can take today

`fluxvm-agent central` is the fleet registry. This page records how it behaves when many nodes heartbeat it, how that was
measured, and what the numbers do not show. It is the baseline for the work to make installing and running 10,000 nodes
safe; the "after" numbers go beside it using the same method.

## The tool

`fluxvm-agent sim-fleet` runs N simulated nodes against one central. Each is one task that POSTs the same body a real
`fluxvm-agent node` sends to `/fleet/register`, at the same interval, with no daemon behind it. At the end it asks
central for `/fleet/nodes` and checks that every simulated node is registered exactly once and healthy. It prints a JSON
report and exits 1 when a beat failed or a node is missing, duplicated or unhealthy.

```bash
fluxvm-agent sim-fleet --central http://127.0.0.1:7799 --token "$TOKEN" --nodes 1000 --interval-secs 10 --duration-secs 60
fluxvm-agent sim-fleet ... --herd        # every node's first beat at the same instant (a mass reboot)
scripts/bench-fleet-central.sh 1000:10:30 3000:10:40 5000:10:40     # fresh central per scenario, one line each
```

Raise the open-file limit first for many nodes (`ulimit -n 10240`); the tool raises its own soft limit as far as the
hard limit allows. Build `--release`; a debug build says nothing about speed.

## Baseline: the current central

One run each, release build, default central settings, on an Apple M-series Mac (12 cores), central and simulator on the
same machine over loopback, empty state directory. Every node beats every 10 s, with first beats spread over the interval.

| Nodes | Target beats/s | Latency p50 / p99 / max | Beats that failed | Registered, healthy | Central CPU / memory | `fleet-nodes.json` |
|---:|---:|---|---|---|---|---:|
| 1,000 | 100 | 0 / 2 / 12 ms | 0 | 1,000 / 1,000 | 2 s / 28 MiB | 0.6 MB |
| 3,000 | 300 | 3 ms / **4.5 s** / 4.7 s | 0 | 3,000 / 3,000 | 18 s / 107 MiB | 1.8 MB |
| 5,000 | 500 | **6.0 s / 10 s** / 10 s | **9,737 of 19,996 (timeouts)** | 5,000 registered, **795 unhealthy** | 31 s / 273 MiB | 3.0 MB |

(The achieved rate is below the target in these short runs, because the first beats are spread over the first interval.)

What this shows:

- Central is fine up to roughly 1,000 nodes, starts to stall in bursts around 3,000, and **falls over at about 5,000
  nodes at a 10 s interval** (about 400 beats/s achieved): half the beats time out and hundreds of nodes are marked
  unhealthy, although the 40 s run only just exceeded the 30 s unhealthy window.
- The persisted registry is about 600 bytes per node. Central rewrites that whole file on every heartbeat, under one lock
  (`persist_json`, called from `register`). At 10,000 nodes that is a ~6 MB file rewritten ~1,000 times a second, which no
  disk does. That, and not the network, is the wall; central used about one core flat out at 5,000 nodes.
- No registration was lost or duplicated in any run; the failure mode is timeouts and nodes going stale, not wrong data.

## What these numbers do not show

- One run per row, on a laptop-class machine. Treat them as an order of magnitude, not a capacity figure.
- Loopback, one process, pooled connections: no TLS handshakes from thousands of distinct clients, no real latency, no
  NAT, no kernel connection-table pressure. Real fleets will do worse on those.
- The simulated nodes advertise an address that refuses connections at once, so central's own polling of nodes (metrics,
  reconcile) fails fast and costs less than polling real nodes would.
- No Redis, no HA, no tenants, no VMs. Central's other background work was at its defaults.
- The ~6 MB figure at 10,000 nodes is extrapolated from the measured ~600 bytes per node.

## After: write-behind registry and heartbeat admission

Same method and machine as the baseline (release build, loopback, empty state directory, one run each). Central no longer
rewrites `fleet-nodes.json` on every heartbeat: a heartbeat updates memory and marks the registry dirty only when something
durable changed (a new node, address, size, labels, capabilities, a cordon), and one background task writes the file at most
every 5 s (`FLUXVM_REGISTRY_FLUSH_SECS`), at least every 60 s, with `fsync` of the file and its directory. Heartbeats pass a
concurrency limit and a token bucket (`FLUXVM_REGISTER_INFLIGHT` 512, `FLUXVM_REGISTER_RATE` 5000/s, `FLUXVM_REGISTER_BURST`
10000) and a refused one gets `429` with a jittered `Retry-After`; a node honours it and does not try another central.
`GET /fleet/registry` shows flush and refusal counters.

| Nodes | Beats/s achieved | Latency p50 / p99 / max | Failed beats | Registered, healthy | Central CPU / memory | `fleet-nodes.json` |
|---:|---:|---|---|---|---|---:|
| 3,000 | 240 | 0 / 0 / 9 ms | 0 | 3,000 / 3,000 | 2.8 s / n.a. | 1.8 MB |
| 5,000 | 400 | 0 / 0 / 11 ms | 0 | 5,000 / 5,000 | 5 s / 82 MiB | 3.6 MB |
| 10,000 | 857 | 0 / 0 / 36 ms | 0 | 10,000 / 10,000 | 13.7 s / 155 MiB | 7.2 MB |

Before, 5,000 nodes timed out half their beats; now 10,000 nodes run clean on the same laptop. The same caveats as above apply
(loopback, no TLS from distinct clients, simulated nodes, one run). What it costs: a restart can lose up to one flush interval
of *durable* changes (a node re-registers on its next beat, so the effect is a short gap, not lost data), and `last_seen` in the
file can be up to a minute old. A journal was not needed for these numbers and is not built; the incremental HA feed that
depends on it is therefore not built either.

## What changes next

Per-node enrollment, bounded polling and the paginated node list follow in the 10,000-node install work. Each step is
measured with `scripts/bench-fleet-central.sh` against these numbers.
