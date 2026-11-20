# FluxMesh personal-cloud networking

FluxMesh is FluxVM's host-to-host networking layer for a personal cloud: it makes machines behind home NAT, CGNAT,
hotel Wi-Fi or a mobile network behave like one private network. It is deliberately separate from Network Fabric
(VM-edge eBPF policy) and Service Fabric (node-local Maglev VIPs), and from the L2 [Mac networks](cross-mac-networks.md)
that join Macs on one segment.

```text
Mac mini ───── direct LAN / IPv6 / UDP ───── Linux host
    \                                         /
     \──── peer relay / relay:443 fallback ──/
                   |
              cloud node
```

## Status

| Area | State |
|---|---|
| Control plane (IPAM, catalog, DNS, ACLs, path ranking, enrollment, key rotation, HA planning) | Implemented; unit-tested (`cargo test -p fluxvm-mesh`, 69 tests) |
| Linux WireGuard reconcile (`fluxmesh-agent`) | Implemented; argv-only, never run against a real interface in CI |
| macOS | Writes a desired-state plan for the NetworkExtension in `native/fluxmesh-macos`; the extension is **not built or signed** here, so macOS has no packet path yet |
| Public edge (HTTPS ingress, ACME, DDNS, router mapping, blind relay, HTTP/3, tracing) | Implemented; **not run** against a real browser, CA, resolver or router |
| Packaging | **Not** in the macOS package or the Homebrew formula; build with `cargo build -p fluxvm-mesh -p fluxctl` |

Treat everything here as pre-production until the runtime checks in [fluxmesh-phase7.md](fluxmesh-phase7.md) have run.

## What it provides

- stable overlay IPv4 allocation (`100.96.0.0/11` by default) and a deterministic dual-stack ULA IPv6 overlay;
- node identity and peer metadata; endpoint inventory for LAN, public IPv6, UDP, peer-relay and relay:443;
- NAT mapping classification that does not claim more than the observations prove;
- deterministic path ranking (LAN, direct IPv6, direct UDP, peer relay, relay:443) with hysteresis;
- VM/LAN/host/exit route advertisements and longest-prefix lookup;
- a cross-node service catalog with stable service VIPs, emitted in the Service Fabric backend shape;
- authoritative private DNS: `<node>.flux`, `<service>.svc.flux`, SRV; no recursion;
- fail-closed ACLs, compiled to Network Fabric CNP where the semantics can be kept exactly (otherwise rejected);
- exit-node and subnet-router policy with a FluxMesh-owned nftables table;
- an authenticated opaque relay (TCP, and native TLS on :443) that only ever carries WireGuard ciphertext;
- zero-touch enrollment over verified TLS and coordinated WireGuard key rotation;
- an optional public edge: HTTPS ingress with share links, ACME, DDNS, router mapping, a blind public relay for
  CGNAT sites, HTTP/3, metrics and tracing.

## Where files live

`<mesh dir>` is `$FLUXMESH_DIR` if set, else `/var/lib/fluxvm/mesh` on Linux and
`~/Library/Application Support/FluxVM/mesh` on macOS (the daemon runs unprivileged there). Individual files can still
be moved with `FLUXMESH_STATE`, `FLUXMESH_CATALOG`, `FLUXMESH_RECONCILE_STATE` and `FLUXMESH_MACOS_PLAN`, or with
each tool's `--state`/`--catalog`/`--key` flags.

| File (in `<mesh dir>`) | Holds | Mode |
|---|---|---|
| `state.v1` | nodes, endpoints, routes, observations | `0600` |
| `catalog.v1` | services, DNS suffix, ACLs, gateways, relays | `0600` |
| `wireguard.key` | this node's WireGuard private key | `0600` |
| `enroll.hmac`, `enroll-used/` | invitation authority key and used nonces | `0600` |
| `keys.v1` | pending key-rotation proposals and acknowledgements | `0600` |
| `reconcile.v1` | ledger of Service Fabric services FluxMesh created | `0600` |
| `public.json`, `share.key`, `acme-account.pk8`, `tls/` | public-edge config, share-link secret, ACME account, certificate | `0600` |

## Programs

| Binary | Role | Phase doc |
|---|---|---|
| `fluxmesh` | state, IPAM, routes, path ranking, catalog, DNS, ACL, gateway planning | [1](fluxmesh.md), [2](fluxmesh-phase2.md) |
| `fluxmesh-relay` | opaque WireGuard relay: `serve`, `client`, `serve-tls`, `client-tls` | [2](fluxmesh-phase2.md), [3](fluxmesh-phase3.md) |
| `fluxmesh-agent` | reconciler: WireGuard, Service Fabric fan-out, STUN, key rotation, macOS plan | [3](fluxmesh-phase3.md) |
| `fluxmesh-enroll` | enrollment server and `join` client | [4](fluxmesh-phase4.md) |
| `fluxmesh-dns-ha` | elected FluxDNS replica | [4](fluxmesh-phase4.md) |
| `fluxmesh-ingress`, `fluxmesh-edge` | HTTPS/H2/H3 ingress; public-edge reconciler (DDNS, ACME, router, WAN) | [5](fluxmesh-phase5.md), [7](fluxmesh-phase7.md) |
| `fluxmesh-mdns`, `fluxmesh-pd` | overlay mDNS reflection; DHCPv6 prefix delegation | [5](fluxmesh-phase5.md) |
| `fluxmesh-public-relay` | blind CGNAT relay (server and edge connector) | [6](fluxmesh-phase6.md), [7](fluxmesh-phase7.md) |
| `fluxmesh-metrics` | Prometheus edge metrics endpoint | [6](fluxmesh-phase6.md) |
| `fluxctl cloud` | operator flow over all of the above | [4](fluxmesh-phase4.md)-[7](fluxmesh-phase7.md) |

Note that `fluxctl cloud` works on local FluxMesh files (`--state` etc.); it does not use `--server` or
`--server-token` and does not talk to the FluxVM daemon's REST API.

## Quick start

On the first host, then a second one:

```bash
fluxctl cloud up home-mac macmini.local              # idempotent; creates state, key and overlay address
fluxctl cloud status
fluxctl cloud invite office-mac office.local          # prints a short-lived one-time token
fluxctl cloud serve-enrollment --cert server.pem --tls-key server.key   # verified-TLS endpoint

# on the joining host: the private key is generated locally and never sent
fluxctl cloud join CONTROLLER:PORT --server-name cloud.example --ca ca.pem TOKEN
```

Run `fluxctl cloud up` first: `fluxctl cloud status` on a host with no state prints a bare "No such file or
directory". Then `fluxmesh-agent run` reconciles WireGuard and the catalog every cycle (Linux).

## Ports

| Port | Used by | Notes |
|---|---|---|
| UDP 51820 | WireGuard | default listen port of `fluxmesh-agent` |
| TCP/UDP 443 | public ingress (H2 over TCP, H3 over QUIC), `serve-tls` relay | privileged port |
| TCP/UDP 7444 | public relay tunnel (TLS/TCP and QUIC) | example in [phase 7](fluxmesh-phase7.md) |
| TCP 9468 | `fluxmesh-metrics` | default `127.0.0.1:9468` |
| UDP 5354 | `fluxmesh dns-serve` example | choose your own; 53 needs privileges |

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
