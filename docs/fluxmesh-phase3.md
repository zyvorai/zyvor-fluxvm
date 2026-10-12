# FluxMesh Phase 3 — automatic personal-cloud networking runtime

Phase 1 added durable overlay/IPAM/route/path intent. Phase 2 added private DNS,
service discovery, ACLs, gateway policy and an opaque relay. Phase 3 turns those
pieces into an operator-grade runtime while preserving FluxVM's existing
Network Fabric and Service Fabric ownership boundaries.

```text
                         ┌───────────────────────────┐
                         │      fluxmesh-agent       │
                         │ desired state -> runtime  │
                         └─────────────┬─────────────┘
                                       │
                 ┌─────────────────────┼──────────────────────┐
                 │                     │                      │
                 v                     v                      v
         Linux WireGuard         FluxVM REST            macOS handoff
      exact reconcile + keys   CNP + Service Fabric   NetworkExtension plan
                 │                     │                      │
                 └──── direct UDP / native TLS :443 relay ────┘
```

## What ships in this PR

### Native TLS relay on TCP :443

Phase 2 required a TLS stream proxy in front of the opaque relay. Phase 3 adds
native rustls transport while leaving the payload end-to-end WireGuard
ciphertext.

Server:

```bash
fluxmesh-relay serve-tls 0.0.0.0:443 \
  /etc/fluxvm/relay.crt \
  /etc/fluxvm/relay.key \
  /etc/fluxvm/relay.tokens
```

Client:

```bash
fluxmesh-relay client-tls \
  203.0.113.20:443 relay.example.com /etc/fluxvm/relay-ca.crt \
  home-mac "$TOKEN" 127.0.0.1:51820 \
  office-mac=127.0.0.1:51901 \
  cloud=127.0.0.1:51902
```

Properties:

- certificate hostname validation is performed by rustls;
- CA trust is explicit per client (`CA_PEM`);
- per-node relay tokens from Phase 2 remain mandatory;
- token comparison is constant-time for equal-length values;
- the relay only forwards framed opaque payloads;
- slow destinations use a bounded queue and drop instead of globally
  backpressuring the relay;
- replacing the server certificate/key affects new connections automatically;
  an invalid partial rotation keeps the last known-good TLS config.


When a peer must use the TLS relay, point the reconciler at the relay client's
per-peer local UDP socket so it does not overwrite that WireGuard endpoint with
a direct address:

```bash
fluxmesh-agent run \
  --relay-binding office-mac=127.0.0.1:51901 \
  --relay-binding cloud=127.0.0.1:51902
```

The matching `fluxmesh-relay client-tls` process owns those UDP sockets. This
keeps path policy explicit: direct peers can stay direct while hard-NAT peers
use the native TLS :443 fallback.

### STUN observations and UDP hole punching

`fluxmesh-agent stun` implements a minimal RFC 5389 binding client. It supports
XOR-MAPPED-ADDRESS for IPv4 and IPv6 and classifies only what multiple
observations prove:

```bash
fluxmesh-agent stun 0.0.0.0:0 198.51.100.10:3478 203.0.113.10:3478
```

A separate helper emits NAT-opening UDP datagrams before WireGuard's encrypted
handshake:

```bash
fluxmesh-agent punch 0.0.0.0:51820 203.0.113.7:51820 4
```

Important: an ephemeral STUN port is useful for NAT classification but is **not**
published as a WireGuard endpoint. `fluxmesh-agent` persists a `public_udp`
endpoint only when the STUN probe used the actual WireGuard listen port and the
observations are endpoint-independent. This avoids advertising a false public
port.

### Exact Linux WireGuard reconciliation

`fluxmesh-agent once` / `run` reconcile a dedicated `fluxmesh0` interface:

- create the interface if absent;
- replace the overlay /32;
- reconcile listen port and private key;
- remove stale peers;
- configure desired peers, allowed IPs, endpoint and keepalive;
- remove stale routes from the dedicated interface;
- replace desired routes;
- bring the link up.

No shell is used. Every operation is executed as an argv vector.

Dry run:

```bash
fluxmesh-agent once \
  --state /var/lib/fluxvm/mesh/state.v1 \
  --catalog /var/lib/fluxvm/mesh/catalog.v1 \
  --key /var/lib/fluxvm/mesh/wireguard.key \
  --dry-run
```

Continuous reconcile:

```bash
fluxmesh-agent run \
  --api http://127.0.0.1:7788 \
  --token "$FLUXVM_TOKEN" \
  --interface fluxmesh0 \
  --listen-port 51820 \
  --interval 15
```

### Automatic WireGuard key rotation

The agent can rotate the local private key when it reaches a configured age.
Automatic rotation is **disabled by default** until the deployment has a key-distribution
control plane; enable it with `--key-max-age-days 30`. It uses `wg genkey`/`wg pubkey`, writes the private key
atomically with mode `0600`, updates the local node public key in FluxMesh state,
and lets the next reconcile apply it.

```bash
fluxmesh-agent wg-rotate \
  /var/lib/fluxvm/mesh/state.v1 \
  /var/lib/fluxvm/mesh/wireguard.key \
  30
```

Set `--key-max-age-days 0` to disable it again.

Key distribution between machines remains a control-plane responsibility. The
agent never assumes that editing one host's local state magically updates a
remote host.

### Network Fabric ACL compilation

Phase 2 ACL intent is now compiled into FluxVM's existing CNP-compatible API.
For a node named `home-mac`, the generated policy selects:

```json
"endpointSelector": {
  "matchLabels": {
    "fluxmesh.node": "home-mac"
  }
}
```

VMs that should inherit node-level personal-cloud policy therefore carry the
label `fluxmesh.node=home-mac`.

The compiler is deliberately fail-closed:

- allow rules compile to `toCIDR` and optional TCP/UDP `toPorts`;
- node destinations become peer overlay `/32`s;
- service destinations become service VIP `/32`s;
- CIDR selectors stay CIDRs;
- default egress deny is always enabled;
- targeted ordered deny rules are rejected instead of changing first-match
  semantics into CNP deny precedence;
- ICMP allow rules are rejected because the current FluxVM CNP subset cannot
  express them without widening the rule.

Preview:

```bash
fluxmesh-agent cnp-plan
```

### Automatic Service Fabric reconciliation

The agent posts every current FluxMesh service plan to the existing
`POST /v1/network/services` API. A small atomic ledger records what FluxMesh
previously managed; services removed from the catalog are deleted with
`DELETE /v1/network/services/{name}`.

The same ledger tracks the generated CNP so stale managed policy is removed.
FluxMesh does not touch services or CNPs it did not previously record in that
ledger.

Default ledger:

```text
<mesh dir>/reconcile.v1
```

The reconciler intentionally accepts an `http://` API endpoint only. The normal
same-host deployment uses loopback. If the FluxVM daemon is remote, terminate a
mutually authenticated local proxy rather than silently disabling TLS checks.

### macOS NetworkExtension handoff

macOS has no Linux WireGuard/eBPF/tap execution path. Phase 3 therefore adds an
explicit, atomic desired-state contract for a signed `NEPacketTunnelProvider`
host application:

```bash
fluxmesh-agent mac-plan
```

The JSON document includes:

- local overlay /32;
- listen port and MTU;
- private DNS suffix;
- peer public keys;
- selected direct endpoints;
- allowed IPs/routes;
- keepalive values.

When `fluxmesh-agent run` executes on macOS it writes this contract atomically
to `FLUXMESH_MACOS_PLAN` (or `<mesh dir>/macos-tunnel.v1.json`).

This PR **does not claim a signed Apple NetworkExtension or a native macOS
WireGuard crypto engine is hardware-verified**. The handoff is the stable
cross-platform contract; entitlement/signing and the packet engine remain a
separate Apple deliverable.

## Runtime ownership

```text
FluxMesh
  node/peer identity
  overlay IPAM
  route intent
  STUN/NAT observations
  direct-vs-relay transport
  private service catalog
  personal-cloud ACL intent
       |
       +--> Network Fabric CNP API  -> VM-edge eBPF policy
       |
       +--> Service Fabric API      -> node-local Maglev/NAT/DSR
       |
       +--> Linux wg/ip             -> host-to-host encrypted overlay
       |
       +--> macOS desired-state     -> signed NetworkExtension host app
```

No VM-edge BPF map ABI or Service Fabric value ABI moves into FluxMesh.

## Suggested system service

Linux systemd example:

```ini
[Unit]
Description=FluxMesh personal-cloud reconciler
After=network-online.target fluxvm.service
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/fluxmesh-agent run \
  --api http://127.0.0.1:7788 \
  --interface fluxmesh0 \
  --key /var/lib/fluxvm/mesh/wireguard.key
Restart=always
RestartSec=2
EnvironmentFile=-/etc/fluxvm/fluxmesh.env

[Install]
WantedBy=multi-user.target
```

`/etc/fluxvm/fluxmesh.env` can provide `FLUXVM_TOKEN` without placing it in the
unit's command line.

## Tests

```bash
./scripts/test-fluxmesh-phase3.sh
```

The static lane checks source/contracts and applies all three stacked patches.
When a Rust toolchain is available it also runs:

```bash
cargo test -p fluxvm-mesh --all-targets
```

Privileged/live certification remains separate because CI must not create host
WireGuard interfaces or rewrite a developer machine's firewall/routes.

## Remaining production certification

Phase 3 intentionally distinguishes code completion from live certification.
Before calling the full personal-cloud path production-certified, run:

1. two Linux hosts behind different NATs with two independent STUN observers;
2. direct endpoint-independent NAT handshake + route test;
3. symmetric/CGNAT case through native TLS relay :443;
4. certificate replacement while existing relay sessions remain active;
5. key rotation across two nodes with control-plane key propagation;
6. Network Fabric labeled-VM deny/allow verification;
7. cross-node Service Fabric VIP failover/drain verification;
8. signed macOS NetworkExtension integration on Apple Silicon hardware.
