# Try the Mac cloud without Macs

`fluxvm-agent sim-node` is a simulated Mac. It answers the node API that central uses from memory, and heartbeats to
central like a real `fluxvm-agent node`, reporting a Mac's Virtualization.framework capabilities. VMs "run" as soon as
they are created; nothing boots. Apple's limit of two macOS guests per Mac is enforced as on a real Mac.

With it you can run the whole control plane on any Linux or macOS laptop: tenants, leases, quotas, placement, drain
and scrub, metering, private networks across Macs, the fleet console, the GitHub Actions scaler (its runners never
pick up a job, of course), and the rest of what a tenant does with a VM: power, screen and typed input, console logs,
snapshots and clones, persistent volumes, warm pools, tunnels (every port echoes), usage graphs, API keys and webhooks.

## One command

```bash
scripts/fleet-sim.sh
```

It needs cargo, curl and python3, and, like any build of this workspace, a sibling `guestkit` checkout (cargo reads
every member's manifest even for `-p fluxvm-agent`; guestkit itself is not compiled). It runs under macOS's bash 3.2.

It builds `fluxvm-agent`, starts a central on `127.0.0.1:7799` and three simulated Macs (`studio-1` 12 vCPUs/32 GiB,
`studio-2` 24 vCPUs/64 GiB with nested virtualization, `mini-1` 8 vCPUs/16 GiB), then plays a tenant:

1. the operator creates tenant `acme` (2 leased Macs, 4 VMs);
2. `acme` leases a Mac (24 h minimum; central picks the smallest free one, `mini-1`);
3. `acme` starts two macOS VMs on it, and a third is refused;
4. `acme` runs `sw_vers` in a VM through central;
5. `acme` creates a private network `acme-net`, and the leased Mac's `overlay.json` appears within a heartbeat.

It then prints the console URL and both tokens, and keeps running until Ctrl-C. Open `http://127.0.0.1:7799/console`
and paste the operator token to see the fleet, or the tenant token to see what `acme` sees. `FLUXVM_SIM_PORT` moves the
port. State and logs go to a temporary directory, printed at the end.

From another terminal, the CLI works against it too (it defaults to `http://127.0.0.1:7799`):

```bash
export FLUXVM_AGENT_TOKEN=<tenant token printed by the script>
fluxctl fleet status
fluxctl fleet launch --image debian-13
fluxctl fleet run debian-13-1 -- uname -a
```

## Try the rest as a tenant

Everything below was run against `sim-node` with the real `fluxctl`. Set the tenant's token once (the operator makes it:
`fluxctl fleet --token $TOKEN tenant create acme`, which prints the token once):

```bash
export CENTRAL_URL=http://127.0.0.1:7799
export FLUXVM_AGENT_TOKEN=fvt_...                  # the tenant's token, not the operator's

fluxctl fleet lease create                         # rent a whole Mac for at least 24 hours
fluxctl fleet launch --image debian-13 web-1       # the image catalog needs an entry first (the demo script adds two)

fluxctl fleet snapshot web-1 --name clean          # save the VM; add --memory to include its memory
fluxctl fleet clone web-1 web-2 --snapshot clean   # a new VM from that snapshot
fluxctl fleet restore web-1 clean                  # roll web-1 back to it
fluxctl fleet snapshots web-1

fluxctl fleet volume-create data --size 20         # a disk that outlives VMs (GiB)
fluxctl fleet volume-resize data --size 40         # grow only
fluxctl fleet volumes

fluxctl fleet stop web-1 --yes                     # --yes skips the question on a terminal
fluxctl fleet start web-1
fluxctl fleet pause web-1 && fluxctl fleet resume web-1
fluxctl fleet logs web-1                           # the console log

fluxctl fleet pool-create ci --image debian-13 --min-ready 1   # keep a booted VM waiting
fluxctl fleet top                                  # usage per VM (fills after the first 15-second sample)
fluxctl fleet tunnels                              # open tunnels and the port policy
```

Add `-o json` for scripts (the output is JSON automatically when piped). The console at `/console` shows the same, with
a screen you can click and type into. Each command is in [fluxctl-fleet.md](fluxctl-fleet.md).

## By hand

```bash
cargo build -p fluxvm-agent
TOKEN=$(openssl rand -hex 16)
target/debug/fluxvm-agent central --listen 127.0.0.1:7799 --state-dir /tmp/fleet --token $TOKEN &
target/debug/fluxvm-agent sim-node --name studio-1 --token $TOKEN --vcpus 12 --memory-gib 32 &
target/debug/fluxvm-agent sim-node --name studio-2 --token $TOKEN --vcpus 24 --memory-gib 64 --nested \
  --overlay-config /tmp/studio-2/overlay.json &
```

| `sim-node` flag | Default | What |
|---|---|---|
| `--name` | (required) | node name |
| `--central` | `http://127.0.0.1:7799` | central URL, or a comma-separated list with HA |
| `--token` | — | central's operator token |
| `--listen` | `127.0.0.1:0` | where the simulated node API listens (port 0: any free port) |
| `--vcpus`, `--memory-gib` | 12, 32 | the Mac's size |
| `--nested` | off | report nested virtualization (M3 or later) |
| `--label key=value` | — | node labels, for `nodeSelector` |
| `--interval-secs` | 10 | heartbeat interval |
| `--overlay-config` | none | write the overlay.json this Mac would get here |

`fluxctl fleet …` works against it as against a real fleet; see [mac-cloud-tenancy.md](mac-cloud-tenancy.md).
To try failover, run two centrals with `--ha-redis` ([fleet-ha.md](fleet-ha.md)) and give each sim node both URLs.

## What it does not simulate

- Booting, real disks and images, the guest agent beyond echoing the command, and the network: overlay.json is
  written but no switch runs. The screen is a drawn picture that shows what you typed and where you clicked; a
  volume is a name and a size (nothing is written); a tunnel is a local echo on every port.
- Resource use: free CPU and memory are the Mac's size minus 2 vCPUs and 2 GiB per VM, as a real node reports them.
- VM state survives only as long as the process; restarting a sim node empties it, which the reconciler then sees as
  VMs that vanished.
