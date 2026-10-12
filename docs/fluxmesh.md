# FluxMesh personal-cloud networking

FluxMesh is FluxVM's host-to-host networking layer for a personal cloud. It is
deliberately separate from Network Fabric (VM-edge eBPF policy) and Service
Fabric (node-local Maglev VIPs).

## Why this exists

A VM can already have excellent local networking and still be unusable as a
personal cloud when its host is behind home NAT, CGNAT, hotel Wi-Fi or a mobile
network. FluxMesh owns the distributed intent needed to make those machines
look like one private cloud:

```text
Mac mini ───── direct LAN / IPv6 / UDP ───── Linux host
    \                                         /
     \──── peer relay / relay:443 fallback ──/
                   |
              cloud node
```

Phase 1 in this PR provides the durable core that packet backends consume:

- stable overlay IPv4 allocation (`100.96.0.0/11` by default);
- node identity and peer metadata;
- endpoint inventory for LAN, public IPv6, UDP, peer-relay and relay:443;
- NAT mapping classification without pretending it can infer more than the
  observations prove;
- deterministic path ranking:
  LAN -> direct IPv6 -> direct UDP -> peer relay -> relay:443;
- VM/LAN/host/exit route advertisements and longest-prefix route lookup;
- atomic state persistence with mode `0600` on Unix;
- a Linux WireGuard argv plan (no shell invocation/string interpolation);
- a dependency-free `fluxmesh` operator CLI;
- unit tests for IPAM, NAT classification, path selection, routing, persistence
  and WireGuard planning.

## Ownership boundary

FluxMesh does **not** replace these existing components:

| Plane | Owner |
| --- | --- |
| VM L3/L4 policy, identity, rate limits, flow visibility | Network Fabric |
| Node-local VIP/NAT/DSR/SNAT/Maglev | Service Fabric |
| Host-to-host identity, overlay IPAM, route intent, direct/relay choice | FluxMesh |
| BGP/ECMP orchestration for enterprise clusters | Zyvor Fabric |

Keeping those boundaries avoids coupling a WAN failure to VM-edge enforcement.

## CLI

Build only the new crate:

```bash
cargo build -p fluxvm-mesh
```

Use a writable state path when running unprivileged:

```bash
export FLUXMESH_STATE=/tmp/fluxmesh.state

fluxmesh init home-mac macmini.local BASE64_WG_PUBLIC_KEY
fluxmesh peer-add office-mac office.local OFFICE_PUBLIC_KEY --relay

fluxmesh endpoint-add office-mac lan 192.168.50.20:51820 10
fluxmesh endpoint-add office-mac public_udp 203.0.113.20:51820 50
fluxmesh endpoint-add office-mac relay443 203.0.113.99:443 100

fluxmesh route-add 10.42.0.0/16 office-mac vm_subnet 10
fluxmesh route-add 192.168.50.0/24 office-mac lan_subnet 20

fluxmesh status
```

Path selection uses reachability supplied by a probe engine:

```bash
fluxmesh path office-mac --lan --udp --relay443
```

Example result:

```text
lan         score=10  endpoint=192.168.50.20:51820  same-LAN direct path
public_udp  score=30  endpoint=203.0.113.20:51820    UDP hole-punched direct path
relay443    score=90  endpoint=203.0.113.99:443      HTTPS-compatible relay fallback
```

NAT mapping classification is intentionally narrow:

```bash
fluxmesh nat-classify 198.51.100.7:50000 198.51.100.7:50000
# endpoint-independent

fluxmesh nat-classify 198.51.100.7:50000 198.51.100.7:50001
# symmetric
```

Generate a Linux WireGuard plan:

```bash
fluxmesh wg-plan fluxmesh0 51820 /etc/fluxvm/mesh.key
```

The library returns argv vectors; the CLI prints a shell-readable preview. The
runtime integration should execute argv directly with `std::process::Command`,
not pass the preview through a shell.

## State format

The state file is line-oriented, versioned and atomic. User-controlled text is
hex encoded, so tabs/newlines cannot corrupt record framing.

```text
VERSION 1
POOL    100.96.0.0/11
LOCAL   <hex node id>
NODE    <hex id> <overlay ip> <hex hostname> <hex public key> <last seen ms> <relay>
ENDPOINT <hex id> <kind> <ip:port> <priority>
ROUTE   <cidr> <hex via node> <kind> <metric>
```

The public API does not depend on this physical encoding; a later SQLite or
distributed control-plane store can replace it without changing route/IPAM
semantics.

## Phase 2 and Phase 3

Phase 2 is implemented in [fluxmesh-phase2.md](fluxmesh-phase2.md): private
A/SRV DNS, cross-node service discovery and Service Fabric plan generation,
fail-closed ACLs, explicit exit/subnet gateway policy, a durable cloud catalog,
relay inventory/selection, and an authenticated opaque TCP relay + WireGuard
UDP bridge.

Phase 3 is implemented in [fluxmesh-phase3.md](fluxmesh-phase3.md): native rustls
relay :443, RFC 5389 STUN observations, UDP hole punching, exact Linux
WireGuard reconciliation, key rotation, Network Fabric CNP compilation,
automatic Service Fabric fan-out/stale deletion, and the macOS
NetworkExtension desired-state contract.

## Testing

```bash
./scripts/test-fluxmesh.sh
```

The tests are all unprivileged. They do not create interfaces or modify host
routes.

## Phase 4 — zero-touch cloud

Phase 4 adds HMAC-signed enrollment, coordinated WireGuard key rotation, deterministic ULA IPv6, FluxDNS replica election, geo/load-aware relay HA, multi-home failover, a native macOS NetworkExtension bridge, and one-command cloud bootstrap. See [fluxmesh-phase4.md](fluxmesh-phase4.md).

## Phase 5 — public personal-cloud edge

Phase 5 adds native HTTPS ingress, ACME HTTP-01 certificate lifecycle, expiring share links, DDNS, PCP/NAT-PMP/UPnP mapping, overlay mDNS, DHCPv6 prefix delegation, multi-WAN policy routing, the `fluxmesh-edge` reconciler, and `fluxctl cloud diagnose`. See [fluxmesh-phase5.md](fluxmesh-phase5.md).

## Phase 6 public edge

Phase 6 adds an optional blind public relay for CGNAT sites, DNS-01 wildcard
ACME, HTTP/2 ALPN/proxying, IPv6 mDNS reflection and edge Prometheus/SLO
telemetry. See [fluxmesh-phase6.md](fluxmesh-phase6.md). The relay is not a TLS
termination point: SNI is used only for site selection and the visitor's TLS
ciphertext terminates at the personal-cloud edge.

## Phase 7 standards / production edge

Phase 7 adds standards-compliant browser HTTP/3 over QUIC, QUIC-first
edge-to-relay transport with TLS/TCP fallback, active/active relay leases with
rendezvous selection, DNSSEC-aware DNS-01 verification through a trusted
validating resolver, certificate hot reload for new QUIC connections, and
OTLP/HTTP ingress/reconcile spans. See [fluxmesh-phase7.md](fluxmesh-phase7.md).

Browser HTTP/3 remains a direct UDP/443 path. The CGNAT relay stays blind TLS
passthrough and does not claim to translate browser QUIC through a TCP visitor
listener.
