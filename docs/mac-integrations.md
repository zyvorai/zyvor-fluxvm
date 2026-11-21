# Mac integrations: MCP tools, lifecycle events, metrics

Three additions that make the macOS workflow (`fluxctl mac`) and Apple-native k3s (`fluxctl kube apple`) visible to agents, webhooks
and Prometheus. Everything here is unit-tested with fakes. **Nothing was run against a live daemon, a real macOS VM, a k3s node, an
MCP client, a webhook receiver or a Prometheus.**

## MCP tools

Ten tools join `fluxctl mcp serve` (listed in [mcp.md](mcp.md#macos-workflow-and-apple-native-k3s-tools)). Safety conventions are those of
the existing tools:

- Reads are always offered; `mac_run` and `mac_exec` only with `--allow-write`. A read-only server neither lists nor runs them.
- Nothing writes to stdout (it is the protocol). `mac run` and `mac exec` stream a guest's output to the terminal, so they are not
  reused: `mac_exec` goes through the daemon's `POST /v1/vms/{id}/agent` like `vm_exec`, and `mac_run` only starts an instance.
- No secrets in output. There is no tool for the k3s token, kubeconfig or SSH host keys. `kube_apple_list` is built from named fields,
  not a dump of the journal. `mac_ps` shows an instance's labels nowhere and its environment never. `kube_apple_logs` withholds any line
  containing `token`, `password`, `secret`, `bearer`, `authorization` or a PEM header (it errs towards hiding).
- `mac_run` takes no command, volume or environment, and cannot set the reserved `fluxvm.mac*` labels.
- `mac_build_dry_run` takes the Macfile text and reads no file, so an agent cannot use it to read one.
- `mac_exec` output is whatever the command prints; the server's 64 KiB result cap applies, secrets in a guest are the guest's.

`kube_apple_*` tools read the local journal and SSH key of the Mac they run on, so run the MCP server on the Mac that holds the cluster.
`kube_apple_status` and `_logs` and `_snapshot_ls` open SSH connections to the nodes (read-only scripts).

## Lifecycle events

The daemon already logs raw `vm.*` events. What it cannot say is what a VM is to fluxctl, so `fluxctl mac watch` polls the VM list (and with
`--kube` the local k3s journals), diffs two snapshots and prints one event per change. The first poll is the baseline; a failed poll keeps
the previous baseline, so a daemon restart does not look like every VM vanishing.

| Event | Fields |
|---|---|
| `mac.instance.created`, `.started`, `.paused`, `.resumed`, `.stopped`, `.failed`, `.removed`, `.state` | `name`, `image`, `status`, `from`, `project`, `node`, `kairon.zyvor.dev/*`; `vm_id` |
| `mac.slots.full`, `mac.slots.free` | `used`, `max` (2), `node` |
| `kube.apple.node.created`, `.started`, `.stopped`, `.failed`, `.removed`, `.state` | `cluster`, `node`, `role`, `status`, `from`, `host`; `vm_id` |
| `kube.apple.cluster.created`, `.phase`, `.removed` | `cluster`, `phase`, `from` |

A paused instance still holds its macOS slot, so pausing never emits `mac.slots.free`. Fields are chosen in code; only labels under the
Kairon prefix (`kairon.zyvor.dev/`, see [kairon-mac-scheduling.md](kairon-mac-scheduling.md)) are forwarded, no other user labels, no
environment, no command, never a token. `--node NAME` puts this Mac's fleet name on each event (as `node`; `host` for kube events, where
`node` is the Kubernetes node).

```sh
fluxctl mac watch --kube --node studio-1 --record-to ~/.fluxvm     # print, and append to the daemon's events.jsonl
fluxctl mac events -f                                                # read mac.* and kube.apple.* back from the daemon
```

`--record-to DIR` appends to `DIR/events.jsonl`, the file `/v1/events`, `/v1/events/stream` and `[[event_sinks]]` read ([event-webhooks.md](event-webhooks.md)).
Give it the daemon's `state_dir` (it needs write access); a sink with `events = ["mac.", "kube.apple."]` then POSTs them signed like any other event.
Run `mac watch` under launchd to keep it going. Without `--record-to` it only prints.

## Metrics

`fluxctl mac metrics [--node NAME] [--kube] [--textfile PATH]` prints Prometheus text, or writes it atomically for node_exporter's textfile collector.

| Family | Labels | |
|---|---|---|
| `fluxvm_mac_instances` | `state` (creating, running, paused, stopped, failed, unknown; zeros included) | gauge |
| `fluxvm_mac_slots_used`, `fluxvm_mac_slots_max` | | slots held (running or paused) of 2 |
| `fluxvm_mac_templates`, `fluxvm_mac_template_store_bytes` | | the local template store |
| `fluxvm_kube_apple_cluster_healthy` | `cluster` | `kube apple status` found no problem |
| `fluxvm_kube_apple_node_up`, `fluxvm_kube_apple_node_ready` | `cluster`, `k8s_node`, `role` | VM running; Kubernetes Ready (absent when unknown) |
| `fluxvm_kube_apple_snapshots`, `fluxvm_kube_apple_snapshot_age_seconds` | `cluster` | etcd snapshots; age of the newest |
| `fluxvm_mac_collect_ok` | `source` (daemon, store) | 0 when a part could not be read, so a gap is not a zero |

`node` is this Mac's name when `--node` is given, the same meaning it has in the fleet families of [fleet-metrics.md](fleet-metrics.md). The
Kubernetes node is `k8s_node` so the two cannot clash. `--kube` SSHes into every node on each run: run it once a minute from launchd, not on every scrape.

```promql
fluxvm_mac_slots_used == fluxvm_mac_slots_max          # both macOS slots busy
fluxvm_kube_apple_snapshot_age_seconds > 86400         # no etcd snapshot for a day
```

## Research: what others expose

| Claim | Status |
|---|---|
| Tart's orchestrator Orchard serves a REST API described by an OpenAPI file and requires basic authentication outside dev mode ([Orchard integration guide](https://tart.run/orchard/integration-guide/), [openai/orchard](https://github.com/openai/orchard)) | sourced (search result excerpts) |
| Orchard has "built-in metrics" | sourced only as a third-party feature list ([Roundfleet](https://www.roundfleet.com/library/orchard)); whether it is a Prometheus endpoint, and its path, **not confirmed** |
| Lima's `limactl list --json` gives machine-readable instance state | inferred from memory, not checked this session |
| Apple's `container` CLI offers JSON listing and has no MCP server or metrics endpoint | inferred from memory, not checked this session |
| Neither Tart, Lima nor Apple `container` ships an MCP server for VM operations | inferred; a search for it returned nothing, which is weak evidence |
| node_exporter's textfile collector reads `*.prom` files from a directory | general Prometheus knowledge |

What this implies, and what was decided: the polling-and-diffing design needs only a list endpoint, which every one of those tools has, so it is
not tied to a daemon that can push events. FluxVM's REST API already exists, so MCP tools wrap it instead of adding a second control surface.
The k3s `etcd-snapshot ls` layout used for snapshot age is **inferred**, not seen on a node: the parser looks for an RFC 3339 timestamp on each
line and falls back to the `-<unix seconds>` suffix of k3s's snapshot names.

## Not run live

- No call to a real daemon: `mac_ps`, `mac_run`, `mac_exec`, `mac_compose_ps`, `mac watch`, `mac events` and `mac metrics` against `/v1/vms` and `/v1/events` are tested with fakes and JSON fixtures only. In particular `POST /v1/vms/{id}/agent` for a **macOS** guest through `mac_exec` is taken from the `vm_exec` documentation, not exercised.
- No real k3s node: `kube_apple_*` tools and `--kube` metrics reuse the existing lifecycle functions, which themselves were only tested with a fake host. Snapshot parsing is on invented sample text.
- No MCP client, webhook receiver or Prometheus scraped these outputs; the exposition format is checked by string assertions, not `promtool check metrics`.
- `mac watch --record-to` writing into a running daemon's `events.jsonl`, and the daemon's sinks picking those lines up, rests on reading the code (`EventLog` appends to a shared file; `event_sinks::spawn` tails it), not on a test with a daemon.

## Backlog (not done)

- A daemon-side `POST /v1/events` so `mac watch` need not write to the state directory (needed for a remote daemon; today `--record-to` requires local access).
- Emit events from the commands themselves (`mac run`, `rm`, `kube apple create|join|upgrade`) instead of polling, which misses short-lived changes between polls and cannot say who or why.
- Readiness and upgrade events for k3s nodes (Ready flips) in `watch`: today `--kube` sees VM state and journal phase only, because Ready needs SSH.
- `mac metrics --serve ADDR` (an HTTP endpoint) and an HA-aware aggregation into central's `/metrics`; today it is a textfile or stdout exporter.
- Grafana dashboard and alert rules for these families (like `deploy/grafana/fluxvm-vz.json`).
- Per-instance CPU, memory and disk series (`mac stats` data), with the cardinality bound `fleet-metrics.md` applies.
- MCP: `mac_stop`/`start`/`rm`, `mac_stats`, `mac_logs`, `kube_apple_snapshot_save`/`delete`, `mac_compose_up|down`; deliberately left out of this first set so the write surface stays two tools. An `mac_exec` environment/workdir argument and streaming output.
- Mac-side Orchard compatibility (a read-only adapter speaking Orchard's API) if existing Tart tooling should see FluxVM Macs; unresearched.
- A Kairon-side consumer of `mac.slots.full` for scheduling; the Kairon label convention is forwarded but nothing reads the events yet.
- The tool-count figure in `docs/install-macos.md` ("66 tools with `--allow-write` in 0.4.11") is a dated statement and was left; this change adds 10 tools to later versions. A current figure should be taken from `tools/list` after a release build.
