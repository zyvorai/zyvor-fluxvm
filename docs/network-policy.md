# Network policy on the FluxVM VM-edge

FluxVM is not a CNI. This document is the supported **CNP-shaped** policy
subset that compiles onto **Network Fabric (GA; dataplane schema v4)** pins
under `/sys/fs/bpf/fluxvm`.

> **Not the same plane as Kubernetes NetworkPolicy.** Sentinel Secure
> Containers Sets 13–15 compile `networking.k8s.io/v1 NetworkPolicy` into
> Pod-scoped VM-edge policy (schema v2 / `fluxvm_prules`) via
> `fluxvm-networkpolicy-controller` — see
> [secure-containers-set14.md](secure-containers-set14.md) and
> [secure-containers-set15.md](secure-containers-set15.md). This file covers
> Fabric CNP/group policy for disposable VMs, not that controller.

Coexistence with a node CNI (`mode=cilium`) is separate — see
[ebpf-cilium.md](ebpf-cilium.md). That mode only *checks* the agent socket;
FluxVM never writes foreign private BPF maps.

Production runbook: [production-dataplane.md](production-dataplane.md).
Security groups: [network-groups.md](network-groups.md).

## Mapped features

| Concept | FluxVM |
|---------|--------|
| Numeric identities (`reserved:world=2`, local ≥256) | `identity.rs` + group FNV ≥ `0x10000` |
| `endpointSelector.matchLabels` | Group labels `app=web` |
| CNP document (`kind` NetworkPolicy / CNP JSON) | `POST /v1/network/cnp`, `fluxvm cnp apply` |
| `toCIDR` / `toCIDRSet` / `except` | `allow_cidrs` / `deny_cidrs` |
| `toEntities` world/host/cluster/remote-node | Entity → CIDR expansion |
| `toFQDNs` | Resolve to IPv4/32 + IPv6/128 at apply; wildcards skipped |
| `toPorts` + ranges + named ports | `tcp/443`, `tcp/8000-8003`, `https`→443 |
| `egressDeny` / `ingressDeny` | `fluxvm_deny4/6` before allow |
| `enableDefaultDeny` | `default_allow=false` |
| `auditMode` | sample_rate bit 31; log drop, forward packet |
| Conntrack | `fluxvm_ct` LRU learn/hit |
| Identity list | `GET /v1/network/identities`, `fluxctl identity list` |
| Group / identity policy | `fluxvm_gid` written at configure_maps |
| Observe snapshot | `GET /v1/network/observe`, `fluxctl observe` |
| toFQDNs live refresh | `POST /v1/network/refresh-dns`, `fluxvm dataplane refresh-dns` (best-effort fleet-wide; skipped VMs logged, call returns refreshed count) |
| ipcache | Guest IP → identity (`GET /v1/network/ipcache`) |
| Production health | `fluxvm dataplane health`, [production-dataplane.md](production-dataplane.md) |

Not in scope for **this** policy plane: kube-proxy replacement, WireGuard/IPsec
datapath, L7 Envoy/Kafka parsers, ClusterMesh, or a full flow UI.

Maglev / DSR / SNAT service LB lives in **Service Fabric v6 (BPF schema 4 /
program generation 6)** — [service-fabric.md](service-fabric.md). Per-VM Network
Fabric policy (this doc) is orthogonal.
([service-fabric.md](service-fabric.md)), not in CNP/group compilers.

## Apply a CNP

```bash
fluxvm cnp apply --spec examples/cnp-web.json
fluxvm cnp list
fluxctl identity list
fluxctl observe
```

Label the VM policy so the compiled group matches:

```json
{ "labels": ["app=web"], "default_allow": false }
```

## Tutorials

Hands-on Network Fabric policy guides:

**[docs/tutorials/network-policy/](tutorials/network-policy/README.md)** —
getting started, identities, security groups, CNP, default deny, named ports,
entities/FQDNs, audit mode, observe, multi-group merge.

## Tests

```bash
python3 scripts/test-network-policy.py
cargo test -p fluxvm-network --lib
python3 scripts/test-production-dataplane.py
sudo -E ./scripts/test-security-groups-e2e.sh
sudo -E ./scripts/test-production-dataplane-e2e.sh
```

## What-if: simulate a policy offline

`fluxctl what-if` answers "would this connection be allowed, and which rule decides?" and "what changes if I replace this policy?"
from JSON files. It needs no daemon, VM, root or eBPF, so it works in review and CI.

```bash
# the policy as stored for a VM (or the body you would POST to `network set-policy`)
fluxctl network effective $VM > current.json

# current.json: {"allow_cidrs": ["10.0.0.0/8"], "allow_ports": ["tcp/443"]}
fluxctl what-if current.json --probe tcp://10.1.2.3:443 --probe tcp://10.1.2.3:22 --probe udp://8.8.8.8:53
# ALLOW tcp://10.1.2.3:443  matches allow_cidrs and allow_ports
# DROP  tcp://10.1.2.3:22  tcp/22 matches no allow_ports entry
# DROP  udp://8.8.8.8:53  destination matches no allow_cidrs entry

# what would break if I tighten the allow list from 10.0.0.0/8 to 10.0.0.0/16?
# (--auto adds the first and last address of every CIDR of both policies, each with the allowed ports,
# plus a few well-known addresses)
fluxctl what-if current.json --against proposed.json --probe tcp://10.0.1.1:80 --probe tcp://10.5.1.1:80
# 2 probes, 1 change verdict, 0 change only the deciding rule
# ALLOW -> DROP  tcp://10.5.1.1:80  (CHANGED)
#     before: matches allow_cidrs
#     after:  destination matches no allow_cidrs entry
```

The file may be a bare policy or the JSON `network effective` returns (the policy is read from its `effective` key). Findings
are printed first: malformed CIDRs and port rules are errors (exit 1), and the linter flags allow entries that a `deny_cidrs`
entry already covers, `allow_ports` that `deny_udp` makes unreachable, `audit_mode` (nothing is dropped), wildcard FQDNs (skipped when
applied), duplicate entries and unknown keys (a misspelled field leaves the default, which allows everything). `--json` prints the
same data for tooling.

**Decision order**, as the TC program applies it to a new flow: `deny_udp` (UDP/SCTP) -> `deny_cidrs` -> IPv4 `allow_icmp` ->
no allow list: `default_allow` -> `allow_cidrs` AND `allow_ports` -> `audit_mode` forwards what would drop. One difference is
reproduced on purpose: for IPv6 the ICMPv6 allowance applies only after the default-deny check, so with no allow list and
`default_allow = false`, `allow_icmp` lets IPv4 ICMP through but still drops ICMPv6 (the linter warns about it).

**Not modelled** (each probe lists what applies): conntrack (established flows skip all checks), rate ceilings, the Pod policy
and the L7 edge contract, DHCP/NDP exemptions, and FQDN resolution (names become `allow_cidrs` entries when the policy is
applied, so a simulation of the stored policy sees the addresses, not the names). It evaluates the effective policy you give it;
it does not merge groups.
