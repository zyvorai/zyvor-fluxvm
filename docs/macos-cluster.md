# FluxVM on Macs: Mac mini, Mac Studio, MacBook Pro and Mac clusters

FluxVM's control plane and its `vz` backend run natively on Apple silicon (see [macos.md](macos.md) for the backend itself).
This page is about where that fits: Linux VMs over a REST API on the Mac you already have, next to a private LLM endpoint
from [Velora](https://github.com/zyvorai/zyvor-velora), with [Kairon](https://github.com/zyvorai/zyvor-kairon) scheduling
across several Macs.

![Mac mini, Mac Studio, MacBook Pro](assets/macos/readme-macs.jpg)

How the `vz` backend works inside, in detail: [macos-architecture.md](macos-architecture.md). A step-by-step fleet setup
(launchd, registry, placement): [mac-cloud.md](mac-cloud.md).

## Which Mac, for what

| Tier | Hardware | FluxVM's role |
| --- | --- | --- |
| Home, low cost | [Mac mini](https://www.apple.com/in/mac-mini/) | One or two Linux VMs (dev box, Home Assistant, CI) next to a private chat endpoint |
| Developer | [MacBook Pro](https://www.apple.com/in/macbook-pro/) | The same `fluxctl` and REST API you use on Linux hosts, for local VMs |
| Team | [Mac Studio](https://www.apple.com/in/mac-studio/) (M5 Max, 36 GB and up) | VMs for CI and sandboxes while most memory serves models |
| On-premise cluster | 2-4 Mac Studios | A Kairon Node per Mac; Kairon schedules `Machines`, FluxVM runs them with `backend: vz` |

Sizing a Mac Studio by use case:

| Use case | Memory | Notes |
| --- | --- | --- |
| CI and one or two Linux VMs | 36 GB and up | Comfortable with models running alongside |
| macOS guest templates | 64 GB and up | Two macOS VMs (Apple's limit), the host and models |
| Agent sandboxes | 36 GB and up | Prefer Linux guests and the `tiny` profile for density (see [agent-density.md](agent-density.md)) |

Use macOS guests when you need the real macOS userspace (Xcode, Apple frameworks, signing); keep the template on the same APFS
volume as the VMs so `cp -c` clones stay instant.

FluxVM does not enforce memory on a Mac; macOS memory pressure applies. Size VMs against what is left after the models:
plan one or two small VMs on a 16 GB Mac. Kairon's Mac Node reports allocatable memory as unified memory minus the larger of
3 GiB and a quarter, so the scheduler already keeps headroom for macOS.

![Which model fits which Mac](assets/macos/readme-model-fit.jpg)

## A Mac cluster for private inference

![A private LLM cluster made of Macs](assets/macos/readme-home-cluster.jpg)

- **Each Mac:** Velora (MLX runtime, OpenAI-compatible endpoints), `fluxctl serve` (VMs with `backend: vz`) and `kairon-node`
  (registers the Mac as a Node with `kairon.zyvor.dev/backend.vz=true`). What each Mac reports for placement (memory pressure, warm slots) and how
  the built-in fleet registry uses it: [kairon-mac-scheduling.md](kairon-mac-scheduling.md).
- **Network:** 10 GbE for management and the fleet; a Thunderbolt 5 mesh for model traffic when one model is sharded across
  Macs (roadmap, see below).
- **What FluxVM adds:** real Linux VMs on the same hardware for the parts that are not models: gateways, vector databases,
  CI runners, agent sandboxes, k3s nodes.

The cluster design follows GK Servis's [Mac Studio LLM inference cluster](https://www.gkservis.com/case-studies/llm-inference-cluster.html)
(4x Mac Studio, Thunderbolt 5 RDMA, EXO + MLX, about 1 TB pooled, under 250 W). Their figures are theirs, not FluxVM measurements.

![Why Macs for private inference](assets/macos/readme-cost.jpg)

## Placing a VM on the right Mac

Macs in one cluster differ: macOS version (vmnet needs 26, custom Virtio devices 27), nested virtualization (M3 or later), which
bridged interfaces exist, which images they already hold, and how much memory is really free. `fluxvm_scheduler::apple_placement`
is a pure scorer for this; `fluxvm-agent central` runs it for every `POST /fleet/vms` with `backend: vz`, and a scheduler outside
the registry, such as Kairon, can call it itself.

### What each Mac reports

`fluxvm-agent node` sends this in every heartbeat (every 10 s) as the node's `apple` object. Nodes without it, and nodes that
are stale, cordoned, at critical memory pressure or whose daemon is unreachable, are never chosen for a `vz` request.

| Field | Meaning |
| --- | --- |
| `free_cpu` | vCPUs not requested by an active VM (creating, running, paused) |
| `free_memory_mib` | **allocatable** memory minus what active VMs request. Allocatable is unified memory minus the larger of 3 GiB and a quarter of it, so a 16 GiB Mac offers 12 GiB and macOS keeps the rest |
| `nested_virtualization` | the Mac and macOS support it (M3 or later, macOS 15+) |
| `vmnet`, `custom_virtio`, `secure_boot`, `rosetta` | the feature is usable here (vmnet needs the networking entitlement) |
| `bridged_interfaces` | host interfaces a VM may bridge to |
| `macos_guests` | macOS guests holding one of Apple's two slots |
| `images` | built-in image names already downloaded and converted (`GET /v1/host/images`, no network access) |
| `macos_pool_idle` | idle slots in the Mac's macOS pool; absent when the daemon would not say |

### How a Mac is chosen

1. **Hard filters.** A Mac is dropped if it lacks the free vCPUs or memory, the needed feature (nested, vmnet, custom Virtio,
   Secure Boot, Rosetta, the named bridge interface), or, for a macOS guest, already runs two macOS guests (Apple's licence
   limit, `MAX_MACOS_GUESTS`). A bonus below never admits a Mac these filters dropped.
2. **Tightest fit.** Among the Macs that fit, the one left with the least free CPU and memory wins
   (`-(cpu_left * 1024 + memory_left_mib / 64)`), so small agent VMs pack onto busy Macs and the big Macs stay free for big VMs.
3. **Image already there.** When `image` is a bare name (`debian-13`) and a Mac already holds it, that Mac gets a bonus of
   1 000 000, larger than any fit difference. A new VM then lands where the download (about 400 MB for `debian-13`) is already
   done. A path in `image` is your own file and carries no bias.
4. **Ties** (equal scores) go to the Mac under less memory pressure, then the one with the larger free fraction, then the one
   with fewer VMs, then the first by name.

Why headroom matters: Virtualization.framework does not enforce guest memory, so macOS memory pressure applies to everything on
the Mac. Placing VMs up to the last MiB of unified memory leaves macOS to swap; the reserve is the same rule Kairon's Mac node
uses, so the two schedulers agree.

### Relieving an overloaded Mac (dry run)

`vz` has no live migration, so placement is final once a VM runs. `GET /fleet/rebalance/plan` (operator token) answers "what
could leave this Mac?" without changing anything:

```bash
curl -s "$CENTRAL/fleet/rebalance/plan?high_water=0.85&max_moves=5" -H "Authorization: Bearer $ADMIN_TOKEN"
```

```json
{
  "dry_run": true,
  "plan": {
    "high_water": 0.85,
    "moves": [{"vm": "3f0c…", "from": "mini-1", "to": "studio-1", "vcpus": 2, "memory_mib": 4096}],
    "unresolved": [],
    "before": [{"node": "mini-1", "used_mib": 11500, "allocatable_mib": 12288, "utilisation": 0.94}],
    "after":  [{"node": "mini-1", "used_mib": 7404, "allocatable_mib": 12288, "utilisation": 0.60}]
  },
  "nodes_considered": 2,
  "vm_lists_unavailable": []
}
```

- A Mac above `high_water` (default 0.85 of its allocatable memory) is a source. The planner picks the smallest movable VM that
  clears the excess, else the largest one, and sends it to the **tightest** Mac that stays at or under the mark afterwards and has
  the CPU. Cordoned or unhealthy Macs can be sources, never destinations.
- **Movable** means the VM was created `reschedulable` (central kept its spec) or is ephemeral (a TTL or the
  `fluxvm.dev/ephemeral` label), the same rule as `drain --mode reschedule`. macOS guests never move.
- A Mac still above the mark when nothing more can move is listed in `unresolved`; a Mac whose VM list could not be read is
  in `vm_lists_unavailable`.
- Applying a move is a recreate on the target (disk state is not carried): drain with `mode=reschedule`, or delete and create.

### Checking out a macOS pool slot across Macs

Each Mac can hold up to two slot-bound macOS guests in its [macOS pool](macos.md) (`/v1/macos-pool`, restored from a `warm`
snapshot on checkout, discarded on return). From central:

```bash
curl -s -X POST "$CENTRAL/fleet/macos-pool/checkout" -H "Authorization: Bearer $ADMIN_TOKEN"
# {"node": "studio-1", "vm": {"id": "11111111-…", ...}}
curl -s -X POST "$CENTRAL/fleet/macos-pool/studio-1/11111111-…/return" -H "Authorization: Bearer $ADMIN_TOKEN"
```

Central takes the healthy, uncordoned Mac with the most idle slots, and tries the next one if a report was stale (the Mac
answers 409). With no idle slot anywhere the answer is 409. A slot belongs to its Mac, so keep the `node` from the reply for the
return. Operator token only; slots are not leased to tenants.

### What has been checked

Unit tests cover the scorer, the headroom rule, the image bonus, the planner (7 cases) and the checkout fallback against stub
nodes. On hardware, only one real M4 plus fake nodes registered by hand has been used: **no second Mac has run placement, the
rebalance plan or the pool checkout**. The new-field paths (`images`, `macos_pool_idle`) were not run against a live
daemon's heartbeat either.

Also note that sandboxes restored from warm slots keep the address they had when the slot was built, so on one Mac a slot whose
address is in use is skipped and that sandbox cold-boots instead.

## Quick start on one Mac

```bash
xcode-select --install && brew install hivex
cargo build -p fluxctl
./scripts/macos-live-test.sh          # boots Debian 13 through the API, SSH, pause/resume, delete
fluxctl serve                         # API on 127.0.0.1:7788, state in ~/Library/Application Support/FluxVM
```

![A Debian 13 guest on Virtualization.framework, captured on an Apple M4 running macOS 27](assets/macos/velora-debian-macos27.png)

*A real capture on an Apple M4, macOS 27: a Debian 13 guest on Virtualization.framework, shown in Velora's app.*

Create a VM with `"backend": "vz"` (or `"auto"`, which resolves to `vz` on a Mac); see [macos.md](macos.md#quick-start).

## Verified and not verified

- **Verified** on an Apple M4, macOS 27.2: the core crates' unit tests, `fluxvm-apple` tests, and the live test (Debian 13: the VM
  lifecycle, forwards, shares, snapshots, `fluxctl run`, stacks and agent sandboxes; see [macos.md](macos.md)); a Kairon `vz` Machine
  scheduled to the Mac and run through FluxVM. Container sandboxes ([oci-sandboxes.md](oci-sandboxes.md)): `scripts/oci-live-test.sh` passes on the same Mac (exit codes, exec, offline
  and allow-listed network, a private network, `linux/amd64` under Rosetta, a warm-pool claim); `scripts/vz-devices-live-test.sh` passes (extra disks, USB hot-plug, console ports, display size). Published ports, volumes, health checks, stacks and `--fleet` placement are unit-tested only.
- **Also verified (2026-10-15, same M4):** `scripts/vz-usernet-live-test.sh` (offline-injected SSH key, TCP and UDP forwards,
  live netobs flows and DNS, the pause precondition) and `scripts/vz-sandbox-volume-live-test.sh` (named volumes on a VM
  sandbox); the whole `vz` suite passed from `main` ([macos.md](macos.md)). **Unit-tested only, one Mac:** the image bonus, the
  memory headroom, the rebalance plan and the fleet macOS pool checkout described above.
- **Verified by hand only:** a macOS 27 guest installed from an IPSW, cloned and reached over SSH ([macos.md](macos.md#macos-guests)). **Implemented, not verified on hardware:** installing macOS guests through the API (`apple.install`), first-boot keys, and snapshots of macOS guests. **Verified on loopback only:** `vz` placement in `fluxvm-agent central` (one real node plus fake nodes). **Not verified:** multi-Mac clusters on real hardware, Thunderbolt RDMA, the shared vmnet broker, physical USB passthrough, any throughput figure on this page.
- **Roadmap (not FluxVM's job, listed for the whole stack):** sharding one model across Macs (EXO or MLX distributed), a
  topology view of Thunderbolt links, per-node temperature and tok/s, and a benchmark command.

The hardware drawings are original illustrations for this project, not Apple artwork. Mac, Mac mini, Mac Studio and
MacBook Pro are trademarks of Apple Inc.
