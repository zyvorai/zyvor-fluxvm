<!--
Copyright 2026 Zyvor AI Labs · https://zyvor.dev
SPDX-License-Identifier: Apache-2.0
-->

# Egress policy: fluxvm-network vs. the userspace gateway

Two enforcement points decide what a guest may reach:

| | Where | Type | Platform |
|---|---|---|---|
| fluxvm-network | host kernel (nftables / eBPF TC) | `dataplane::VmNetworkPolicy` | Linux |
| fluxvm-usernet | userspace NAT gateway of `apple.usernet` | `EgressPolicy` | any (macOS first) |

`fluxvm-network` pulls in eBPF, nftables and an HTTP proxy stack and `fluxvm-usernet` deliberately depends on none of
it, so the two types are **not** unified into one Rust type. Instead `fluxvm_apple::usernet_policy` converts a
`VmNetworkPolicy` document (it reads the JSON form through a serde mirror, `NetworkPolicyView`) into an
`EgressPolicy`, reporting what could not be carried over. fluxvm-network's behaviour is unchanged.

```rust
let c = fluxvm_apple::usernet_policy::egress_from_network_policy(&view)?;
cfg.egress = c.egress;
cfg.icmp_proxy = c.icmp_proxy;
// c.notes: differences that do not weaken the policy; c.unsupported: features that would be weaker.
```

## Models

**VmNetworkPolicy.** Enforcing when `allow_cidrs`, `allow_ports` is non-empty or `default_allow` is false; otherwise
everything is allowed (including private ranges). When enforcing, a packet must match the CIDR dimension (if set) and
the L4 dimension (if set), `deny_cidrs` always wins, ICMP needs `allow_icmp`. Ports are `proto/port` pairs.

**EgressPolicy.** Order: non-unicast, `deny_cidrs`, `deny_ports`, `allow_ports` (a miss denies), then explicit allow
(`allow_cidrs`, or an address learned from a DNS answer for `allow_domains`), then private ranges denied unless
`allow_private`, then any configured allow list denies the rest. With no lists the default is "public internet only".
Ports are protocol-agnostic. Domains also gate DNS itself (NXDOMAIN).

## Conversion rules

| VmNetworkPolicy | EgressPolicy / UserNetConfig | Notes |
|---|---|---|
| `allow_cidrs` (v4) / (v6) | `allow_cidrs` / `allow_cidrs6` | `/prefix` required on input |
| `deny_cidrs` | `deny_cidrs` / `deny_cidrs6` | |
| `allow_ports` `tcp/N`, `udp/N` | `allow_ports` (N, deduplicated) | protocol is lost: `tcp/443` + `udp/53` allows 443 and 53 for both; reported in `notes` when the tcp and udp sets differ |
| `allow_ports` `icmp/*`, `icmp6/*`, `allow_icmp` | `icmp_proxy = true` | `icmp_proxy` is also true when the source is not enforcing |
| `allow_ports` `sctp/N` | dropped | the gateway never forwards SCTP; reported in `notes` |
| `allow_fqdns` `name`, `*.suffix` | `allow_domains` | the gateway matches the name and everything under it, and learns addresses at DNS time (the kernel path resolves once at apply time and skips wildcards) |
| `allow_fqdns` other patterns | dropped | narrower, reported; if nothing usable remains and there are no CIDRs the result is deny-all (`0.0.0.0/0`, `::/0` in `deny_cidrs`) |
| `default_allow = false`, no lists | `deny_cidrs = 0.0.0.0/0, ::/0` | deny-all |
| (always) | `allow_private = true` | fluxvm-network never filters private ranges on its own; an allow list denies them by omission on both sides. Tighten to `false` for a hardened gateway |
| `entities`, `deny_udp`, `max_egress_mbps`, `max_egress_pps` | not representable | listed in `unsupported`: the converted policy is weaker |
| `audit_mode` | enforced | gateway has no log-and-allow; stricter, noted |
| `groups`, `labels` | n/a | merged by fluxvm-network before conversion |

CIDR and port dimensions combine as AND in both models, so `allow_cidrs x allow_ports` (the nftables cross product)
is exactly `allow_cidrs` AND `allow_ports` in the gateway.

## Gateway-only features

`deny_ports`, `deny_domains`, the default private-range deny, `allow_host_loopback` and the per-flow DNS learning have no
counterpart in `VmNetworkPolicy`; they are set from `apple.usernet` directly.

## Verification

Unit tests in `crates/fluxvm-apple/src/usernet_policy.rs` evaluate the converted policy with `EgressPolicy::check`.
Nothing compares the two enforcers against the same live traffic.
