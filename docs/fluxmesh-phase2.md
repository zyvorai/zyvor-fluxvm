# FluxMesh Phase 2 — personal-cloud services, DNS, ACLs, gateways and relay

Phase 1 gave FluxVM a durable host-to-host overlay model. Phase 2 adds the
personal-cloud control plane above that connectivity without moving VM-edge
policy or node-local VIP load balancing out of their existing owners.

```text
                       private DNS
                    photos.svc.flux
                           |
                           v
 laptop ---- FluxMesh ---- service VIP ---- Service Fabric
    |          |                              /      \
    |          +---- ACL decision -------- VM A    VM B
    |          |
    |          +---- subnet router ------ home LAN / NAS
    |          |
    |          +---- exit node ---------- internet
    |          |
    +-- direct path fails -- relay TCP -- TLS proxy :443
```

## What ships in this PR

### Cross-node service catalog

`fluxmesh service-add` assigns a stable service VIP from `100.88.0.0/16` by
default. Endpoints are tied to known FluxMesh nodes and carry address, port,
weight and Ready/Draining/Unhealthy state.

The generated `service-fabric-plan` matches FluxVM Service Fabric's existing
shape: VIP, TCP/UDP protocol, Maglev, NAT, east-west exposure and backend
`address`/`port`/`weight`/`state` fields. FluxMesh owns distributed endpoint
intent; Service Fabric continues to own the node-local dataplane.

```bash
fluxmesh service-add photos 443 tcp
fluxmesh service-endpoint-add photos home-mac 10.42.1.20 8443 2 ready
fluxmesh service-endpoint-add photos office-mac 10.43.1.20 8443 1 ready
fluxmesh service-fabric-plan photos
```

### FluxDNS

Node names resolve as `<node>.flux`. Services resolve as
`<service>.svc.flux`. SRV records use:

```text
_<service>._tcp.svc.flux
_<service>._udp.svc.flux
```

Examples:

```bash
fluxmesh dns home-mac.flux
fluxmesh dns photos.svc.flux
fluxmesh dns _photos._tcp.svc.flux

# Real UDP DNS server; bind to a mesh-reachable address in production.
fluxmesh dns-serve 127.0.0.1:5354
```

The DNS server is authoritative-only, one-question-per-packet, and supports A
and SRV. It does not recursively resolve public DNS.

### Fail-closed ACLs

ACLs are ordered by numeric priority and default to deny when no rule matches.
They are portable control-plane intent that can later compile to Network
Fabric, nftables or a userspace tunnel backend.

```bash
fluxmesh acl-add allow-photos 100 allow node:laptop service:photos tcp 443
fluxmesh acl-add deny-rest 1000 deny any any any
fluxmesh acl-check laptop service:photos tcp 443
# allow
```

Selectors:

- source: `any`, `node:<id>`
- destination: `any`, `node:<id>`, `service:<name>`, `cidr:<prefix>`
- protocol: `any`, `tcp`, `udp`, `icmp`

### Exit nodes and subnet routers

Gateway policy is explicit. An empty allow-list means every known mesh node;
a non-empty list restricts forwarding to exactly those overlay /32s.

```bash
# Only laptop and work-mac may use home-mac as an Internet exit.
fluxmesh exit-enable home-mac laptop work-mac

# Only laptop may enter the home LAN through home-mac.
fluxmesh subnet-policy 192.168.1.0/24 home-mac laptop

# Print the Linux forwarding plan. It enables ip_forward and emits a
# FluxMesh-owned nftables table with default-drop forwarding.
fluxmesh gateway-plan home-mac fluxmesh0 enp1s0
```

The command prints a plan; it does not silently modify the host.

### Relay fallback

`fluxmesh-relay` is an opaque relay: its payload is intended to be WireGuard
UDP packets, so the relay never needs guest/application plaintext.

A relay server authenticates each node with a separate opaque token:

```text
# /etc/fluxvm/relay.tokens (0600)
home-mac   8a3f...at-least-16-chars
office-mac 71be...at-least-16-chars
```

Run the relay on loopback behind a TLS stream proxy:

```bash
fluxmesh-relay serve 127.0.0.1:7443 /etc/fluxvm/relay.tokens
```

The public TLS listener can be Caddy/nginx/HAProxy on TCP :443 forwarding the
stream to `127.0.0.1:7443`. The Phase-2 relay client speaks the inner FluxMesh
TCP protocol, so when the public relay is TLS-terminated the node also needs a
local TLS client tunnel (for example stunnel) from `127.0.0.1:<local-port>` to
the public `relay.example:443`; point `fluxmesh-relay client` at that local
port. Native rustls client/server TLS belongs in the next transport PR rather
than embedding certificate lifecycle into this patch.

On a node, one client session can carry multiple WireGuard peers. Give every
peer a unique local UDP endpoint:

```bash
# 127.0.0.1:7443 may be the relay directly on a trusted network, or a local
# TLS client tunnel whose remote side is relay.example:443.
fluxmesh-relay client 127.0.0.1:7443 home-mac "$TOKEN" 127.0.0.1:51820 \
  office-mac=127.0.0.1:51901 \
  cloud=127.0.0.1:51902
```

Then set the WireGuard endpoint for `office-mac` to `127.0.0.1:51901` and for
`cloud` to `127.0.0.1:51902`. Outbound packets arriving on each socket are
framed for that destination. Inbound frames are emitted from the same socket
to the local WireGuard listen port, preserving a stable endpoint per peer.

Relay inventory and selection are also stored in the cloud catalog:

```bash
fluxmesh relay-add blr1 tcp443 203.0.113.10:443 10
fluxmesh relay-add sin1 tcp443 203.0.113.11:443 20
fluxmesh relay-health blr1 up 17
fluxmesh relay-select tcp443
```

Healthy relays are selected by priority, then load.

## State ownership

Phase 2 intentionally does not rewrite the high-churn Phase 1 connectivity
file. It adds an independently atomic policy catalog:

```text
/var/lib/fluxvm/mesh/state.v1    # nodes, endpoints, routes, observations
/var/lib/fluxvm/mesh/catalog.v1  # services, DNS suffix, ACLs, gateways, relays
```

The directory is `/var/lib/fluxvm/mesh` on Linux and
`~/Library/Application Support/FluxVM/mesh` on macOS (where the daemon runs
unprivileged); `FLUXMESH_DIR` moves all of it. Override single files with
`FLUXMESH_STATE` and `FLUXMESH_CATALOG` or `--state` and `--catalog`.

The catalog file is mode `0600` on Unix, line-oriented, versioned, and writes
through temp-file + rename.

## Security properties

- ACL decision is fail-closed.
- Gateway forwarding rules are default-drop and scoped to allowed overlay /32s.
- Relay registration uses a separate token per node and limits node IDs/frame sizes.
- Relay payloads are intended to remain WireGuard ciphertext end to end.
- Internet-facing TCP relay should be protected by TLS before exposure.
- No shell is invoked for WireGuard or sysctl argv plans.

## Tests

```bash
./scripts/test-fluxmesh-phase2.sh
```

This runs Phase 1 structural checks and, when Rust is available:

```bash
cargo test -p fluxvm-mesh --all-targets
```

All unit tests are unprivileged; no interfaces, routes, firewall rules or DNS
ports are modified by the test suite.

## Next PR

Phase 3 should make runtime reconciliation automatic:

1. native rustls :443 relay with certificate rotation;
2. STUN observers + active UDP hole punching;
3. signed enrollment and per-node key rotation;
4. Linux WireGuard executor/reconciler with health probes;
5. macOS utun/NetworkExtension tunnel backend;
6. compile FluxMesh ACLs into Network Fabric where a VM edge exists;
7. reconcile cross-node service plans directly into Service Fabric;
8. API + `fluxctl cloud` dashboard integration;
9. HA DNS/relay discovery and multi-relay failover;
10. IPv6 overlay and IPv6 subnet/exit parity.
