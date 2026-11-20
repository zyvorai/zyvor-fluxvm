# Virtual networks (eBPF routing, Geneve overlay, SNAT, floating IPs)

A virtual network is an isolated IPv4 network identified by a VNI. FluxVM
routes it entirely in eBPF: there is no Linux bridge, no per-network kernel
device and no iptables. Guests on the same host are routed tap to tap.
Guests on other hosts are reached through one Geneve device per host in
collect-metadata mode. Traffic leaving the network can be translated to the
host's uplink address (SNAT). A public address can be translated 1:1 to a
guest (floating IP).

FluxVM does not allocate anything: the caller (Kairon's `VirtualNetwork`
controller, or an operator) chooses the VNI, the addresses and the
endpoints and pushes them with `PUT /v1/network/vnets/{vni}`.

Linux only. Reported as `host.vnet` in `GET /v1/runtime/capabilities`.

## Attaching a VM

A VM joins a network through its primary tap NIC:

```json
{
  "network": {
    "mode": "tap",
    "mac": "52:54:00:12:34:56",
    "vnet": {"vni": 100, "ip": "10.50.0.11", "prefix_len": 24, "gateway": "10.50.0.1"}
  }
}
```

- `mac` is required. `bridge`, `netns`, `direct` and `extra` are refused.
- `sandbox.dataplane.mode = "legacy"` refuses vnet VMs, because nothing
  would route them.
- The guest gets its address, gateway and MTU 1450 through FluxVM's static
  cloud-init `network-config`, even when the request has no other
  cloud-init.
- NIC hotplug is refused on vnet VMs.
- No VM-edge policy program is attached. The vnet program owns the tap.

The VM can be created before or after its network is applied. Until the
network exists, the tap has no route.

## Applying a network

```http
PUT /v1/network/vnets/100
```

```json
{
  "vni": 100,
  "cidr": "10.50.0.0/24",
  "gateway": "10.50.0.1",
  "uplink": "eno1",
  "snat_address": "198.51.100.1",
  "floating_ips": [{"address": "203.0.113.10", "ip": "10.50.0.11"}],
  "endpoints": [
    {"ip": "10.50.0.11", "mac": "52:54:00:12:34:56", "node_ip": "192.0.2.1"},
    {"ip": "10.50.0.12", "mac": "52:54:00:ab:cd:ef", "node_ip": "192.0.2.2"}
  ]
}
```

| Field | Meaning |
|---|---|
| `vni` | 1..16777215, must match the path. |
| `cidr`, `gateway` | IPv4, prefix /8 to /30. The gateway exists on every host with MAC `02:66:76:<vni>`. |
| `endpoints` | Every guest on the network, on any host. Guests attached here are routed locally whatever `node_ip` says. Others are tunneled to `node_ip`. |
| `uplink` | Host interface for SNAT and floating IPs. Required with either. |
| `snat_address` | Source address for traffic leaving the CIDR, normally the uplink's address. Absent: the network is closed. |
| `floating_ips` | 1:1 NAT between `address` and a guest `ip`, both directions. |

The PUT replaces the earlier config for the VNI. It is admin-only and
returns the network's status.

- `GET /v1/network/vnets` lists applied networks with their local
  endpoints, remote endpoint count and gateway MAC.
- `GET /v1/network/vnets/{vni}` returns one network.
- `DELETE /v1/network/vnets/{vni}` (admin) removes one. It is refused while
  a VM on this host is still attached, and returns 404 for an unknown VNI.

Configs persist under `<state_dir>/vnet/` and are re-applied as VMs attach.

## Datapath

`fluxvm_vnet.bpf.o` (built by `scripts/build-ebpf.sh`) has three TC
programs. Their maps are pinned under `<bpf pin root>/vnet/`.

- **`vnet_guest`**, on each vnet tap (TCX, or a `tc` filter at pref 49170
  without TCX):
  - Answers ARP for every address on the network with the gateway MAC.
  - Answers ping to the gateway.
  - Drops packets whose source is not the guest's address.
  - Routes inside the CIDR, locally with `bpf_redirect` or to a remote host
    with `bpf_skb_set_tunnel_key` and a redirect to `fvgnv0`.
  - Sends everything else out through the floating IP or SNAT, and drops it
    when the network has neither.
- **`vnet_tunnel`**, on `fvgnv0` (`ip link add fvgnv0 type geneve external
  dstport 6081`). It reads the VNI from the tunnel key and delivers only to
  guests of that VNI attached on this host.
- **`vnet_uplink`**, on the uplink:
  - Reverses SNAT for replies and translates floating IPs inbound.
  - Returns `TC_ACT_UNSPEC` for everything else, so the host's own traffic
    is untouched.
  - It is attached with `BPF_F_BEFORE` (`fluxvm-tcx attach-first`). Cilium's
    `cil_from_netdev` returns PASS and would otherwise end the chain first.

SNAT keeps an LRU flow table keyed by the 5-tuple. Ports come from
61000-65534, above Linux's default ephemeral range, so they do not collide
with the host's own connections. ICMP echo is translated by its id.

## Real limits

- IPv4 only: guests, underlay and floating IPs.
- No load balancing inside a network. Service Fabric does not balance onto
  vnet guests.
- No TTL decrement. IP options are dropped.
- SNAT has no TCP state machine. Closed flows stay until LRU eviction.
- The host itself cannot reach vnet guests. Use a floating IP or another
  guest.
- FluxVM does not announce or proxy-ARP floating IPs. Route them to the
  host (Kairon's kairon-node has an announce-only BGP speaker).
- MTU 1450 for a 1500-byte underlay. No PMTU handling for larger packets.
- Hosts must reach each other on UDP 6081. No encryption.

## Testing

`crates/fluxvm-network/tests/vnet_netns.rs` builds two simulated hosts in
network namespaces, each with its own pin root and state directory, plus an
"internet" namespace. It loads the real object and checks:

- same-host and cross-host (Geneve) routing in both directions, and ping
  to the gateway;
- isolation between VNIs and anti-spoofing;
- SNAT for ping and TCP;
- floating-IP DNAT and egress;
- that removal is refused while a VM is attached, plus detach and cleanup.

```sh
scripts/build-ebpf.sh /tmp/bpf
FLUXVM_TEST_BPF_DIR=/tmp/bpf sudo -E cargo test -p fluxvm-network --test vnet_netns -- --nocapture
```

Set `FLUXVM_TCX_HELPER` to a freshly built `fluxvm-tcx` (it needs
`attach-first`) when the installed one is older.
