# Private networks across Macs

A private network (`apple.networks`) normally lives on one Mac: its `fluxvm-vz-switch` connects the guests on that Mac
only. An **overlay** stretches one network across several Macs, so a tenant's VMs on `studio-1` and `studio-2` share
one Layer-2 network and one subnet, as if they were on the same host.

**Status:**
- The switch protocol was compiled and tested standalone with a stand-in cipher. The tests cover the round trip,
  MAC learning, and refusal of replays, wrong keys, the wrong network, strangers, tampering and spoofing.
- Placement, allocation and the sync have unit and in-process HTTP tests.
- Nothing has run on two real Macs yet.

## Using it

As a tenant (with Macs leased):

```bash
curl -X POST $CENTRAL/fleet/overlays -H "Authorization: Bearer $TENANT_TOKEN" \
  -d '{"name": "acme-net"}'          # members default to every Mac the tenant leases
```

Within a heartbeat (10 s), each member Mac's `fluxvm-agent node` writes the Mac's `overlay.json` and the network key.
VMs then join with the usual spec on any member Mac:

```json
{"backend": "vz", "name": "db-1", "apple": {"networks": [{"name": "acme-net"}]}}
```

Guests on different Macs get addresses from disjoint slices of the same `10.89.N.0/24`, so they can reach each other
by IP.

| Call | What |
|---|---|
| `POST /fleet/overlays` `{name, nodes?, hosts_per_node? (30), mtu? (1400), tenant? (operator)}` | create |
| `GET /fleet/overlays`, `GET/DELETE /fleet/overlays/{name}` | list, show, delete (a tenant sees only its own) |
| `POST /fleet/overlays/{name}/members` `{node}`, `DELETE …/members/{node}` | add or remove a Mac |
| `POST /fleet/overlays/{name}/rotate` | new key (the previous one stays valid until the next rotation) |
| `GET /fleet/nodes/{node}/overlays` | the node's `overlay.json` with keys (operator token only; node agents use it) |

When a tenant's lease on a Mac ends, that Mac leaves the tenant's networks.

## How it works

- **Allocation (central).** Each network gets:
  - a fleet-wide subnet `10.89.N.0/24` with N from 128 upwards (Mac-local networks allocate upwards from 0);
  - a UDP port from 47100;
  - a fresh 256-bit key;
  - one host slice per member Mac (2–31, 32–61, …).

  The key is stored only in central (and its HA mirror). The API never shows it, except to node agents for their own
  networks.
- **Sync (node agent).** After each heartbeat, `fluxvm-agent node` fetches its networks. It writes
  `~/.config/fluxvm/overlay.json` and `overlay-keys/<network>.key` (mode 0600), and removes keys of networks it left.
  `--no-overlay-sync` turns this off; `FLUXVM_VZ_OVERLAY_CONFIG` moves the file.
- **Daemon.**
  - Assigns guest addresses from the fleet subnet, within this Mac's slice only.
  - Starts the network's switch with the overlay flags.
  - Refuses a network whose local subnet disagrees with the fleet.
- **Switch.** The switches form a full mesh over UDP. Each frame is sealed with AES-256-GCM. The nonce is a sender id
  plus a counter. The associated data binds in the network name. A 1024-packet replay window runs per sender. Switches
  learn which peer each remote MAC sits behind. They never forward peer traffic to other peers, and a peer cannot claim
  a local guest's MAC. Peers are re-read from `overlay.json` every 10 s, so Macs can join and leave without restarting
  anyone's network.

## Key rotation

Each network key has an epoch. `POST /fleet/overlays/{name}/rotate` (tenant or operator) issues a new key, and the
reconciler rotates any key older than `overlay_key_rotation_days` (policy, default 30; `0` turns it off). The previous
key stays valid until the next rotation:

1. each member Mac's agent writes both keys within a heartbeat;
2. each switch reloads them within 10 s, opens datagrams under either key at once, and keeps sealing with the old
   key for another 60 s;
3. by then every Mac opens the new key, so switches seal with it and traffic keeps flowing while Macs roll over.

A datagram's epoch is authenticated, so it cannot be moved to another key. Rotating twice retires a key completely.

## Network requirements

- **Reachability.** Every member Mac must reach every other on the network's UDP port. The address used is the
  `fluxvm.dev/overlay-ip` node label, else the host of the node's `--advertise-url`. Open the port range 47100–48099
  between Macs only.
- **MTU.** Encapsulation adds 68 bytes (IPv4). The default network MTU of 1400 fits a 1500-byte underlay with room to
  spare. With a jumbo-frame underlay, raise `mtu`.
- **Encryption.** Traffic is encrypted and authenticated between Macs. The underlay itself (10 GbE, a VLAN, or
  WireGuard between sites) needs no extra protection for this network.

## Limits

- The mesh is full, so every Mac sends broadcast and unknown unicast to every other. That is fine for tens of Macs,
  not hundreds.
- Each Mac's slice is fixed at creation (`hosts_per_node`, default 30 guests per Mac per network, up to 8 Macs at 30).
- Guest IPs are IPv4 only, like Mac-local private networks. Peers are matched on their exact IP, so there must be no
  NAT between Macs. Switches listen on IPv4 (`0.0.0.0`).
- Replay protection uses each datagram's send time (within 2 minutes) plus per-sender windows, so Macs need NTP. A
  datagram captured less than 2 minutes earlier could be replayed once to a switch that restarted within that time.
- Every member shares the network key, so a compromised member Mac could impersonate another member's guests.
  Treat member Macs as one trust domain (they belong to one tenant).
- Recreating a network under the same name changes its subnet, which running switches pick up only when they restart.
  Keys and peers reload live.
- An operator network with no tenant is not tied to leases: any VM on a member Mac can join it by name. Give tenant
  networks a tenant.
- Keys are stored in central's `fleet-tenancy.json` (mode 0600) and its HA mirror. Protect the Redis accordingly.
