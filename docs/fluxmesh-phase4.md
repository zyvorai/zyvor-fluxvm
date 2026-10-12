# FluxMesh Phase 4 — zero-touch personal cloud

Phase 4 turns the connectivity/service foundation into an operable personal-cloud network.
It remains additive: VM-edge enforcement stays in Network Fabric and node-local VIP execution stays in Service Fabric.

## Shipped in this phase

### Zero-touch enrollment primitives

`fluxctl cloud invite` creates a short-lived HMAC-SHA256 capability. By default the invitation is **unbound** to a WireGuard key: the joining host creates its private key locally and submits only its public key with the one-time invitation over verified TLS. `--public-key` remains available for pre-provisioned/manual enrollment.

`fluxctl cloud serve-enrollment --cert ... --tls-key ...` (or `fluxmesh-enroll serve`) exposes the native rustls enrollment endpoint. `fluxctl cloud join CONTROLLER --server-name NAME --ca CA TOKEN` generates or reuses the local WireGuard key, verifies the controller certificate, submits the public key, and atomically installs the returned mesh snapshot. The controller validates signature/expiry, atomically consumes the nonce, allocates the overlay address, and persists the new peer before returning state with the joining node marked local.

The HMAC authority key is 32 random bytes stored mode `0600` at `<mesh dir>/enroll.hmac` by default (`/var/lib/fluxvm/mesh` on Linux, `~/Library/Application Support/FluxVM/mesh` on macOS, `FLUXMESH_DIR` to move it). Invitations and enrollment responses never contain a WireGuard private key.

### Safe distributed WireGuard key rotation

Phase 3 intentionally disabled automatic rotation because there was no safe public-key distribution path. Phase 4 adds a two-phase key store:

1. write a new private key to `wireguard.pending`;
2. publish the corresponding public key as generation N;
3. every current member acknowledges generation N;
4. only after all acknowledgements does the owner atomically activate the pending private key and update mesh state.

`fluxmesh-agent --key-max-age-days N` now uses this protocol through `keys.v1` rather than switching a key unilaterally.

### Dual-stack overlay

IPv4 remains the durable IPAM source. A deterministic ULA `/64` is derived from the mesh IPv4 pool and each node gets a stable `/128` derived from its node id. This avoids another allocator while keeping addresses stable across restarts.

Linux reconciliation now configures both families and uses family-correct route commands. The macOS NetworkExtension plan carries the same IPv6 address and peer routes.

### FluxDNS HA

`fluxmesh-dns-ha` uses deterministic rendezvous placement to choose N DNS replicas. Every selected node builds the same private zone from the mesh state and service catalog and serves it locally. A non-selected node exits cleanly as standby.

### Relay HA / geo selection

`RelaySelector` scores only healthy relays and considers preferred region, failure count, capacity pressure, RTT and existing relay priority. `ordered()` returns the complete failover chain rather than only a single relay.

### Multi-home failover

`MultiHomeState` chooses between LAN, public IPv6, public UDP and relay endpoints with hysteresis. A currently active healthy path is retained unless another path is meaningfully better; three consecutive failures force failover.

### macOS NetworkExtension host bridge

`native/fluxmesh-macos` contains a native `NEPacketTunnelProvider` implementation. It reads the FluxMesh plan, configures IPv4/IPv6 routes plus split DNS, and bridges raw packets to a local userspace crypto engine through a Unix datagram socket.

The extension does not embed a third-party tunnel library. Packet-engine framing is documented and kept separate from Apple entitlement/signing lifecycle.

### One-command UX

The FluxMesh binary adds:

```text
fluxmesh cloud-up NODE HOSTNAME [POOL]
fluxmesh cloud-status
```

The PR also includes the `crates/fluxctl/src/cloud.rs` integration and patch hunks for:

```text
fluxctl cloud up NODE HOSTNAME
fluxctl cloud status
fluxctl cloud invite NODE HOSTNAME [--public-key PUBLIC_KEY]
fluxctl cloud serve-enrollment --cert server.pem --tls-key server.key
fluxctl cloud join CONTROLLER --server-name cloud.example --ca ca.pem TOKEN
fluxctl cloud accept TOKEN   # manual, key-bound invitations only
```

## Ownership boundaries

| Capability | Owner |
| --- | --- |
| overlay membership, enrollment, public-key distribution, relay choice | FluxMesh |
| host-to-host WireGuard/macOS tunnel desired state | FluxMesh |
| VM edge L3/L4/FQDN/identity policy | FluxVM Network Fabric |
| node-local VIP/Maglev/NAT/DSR | FluxVM Service Fabric |
| enterprise BGP/ECMP/fleet orchestration | Zyvor Fabric |

## Security notes

- Enrollment tokens are HMAC authenticated and expire.
- Zero-touch enrollment is carried over verified TLS; the joining host never uploads its private key.
- Used invitation nonces are atomically persisted (`create_new`) to prevent replay even under concurrent joins.
- Unbound invitations cannot be consumed by the manual `accept` path; they require the TLS flow to supply a real public key.
- Key activation is fail-closed until every current member acknowledges.
- IPv6 is ULA-only by default; this phase does not silently advertise public prefixes.
- NetworkExtension requires Apple signing and the packet-tunnel entitlement.
