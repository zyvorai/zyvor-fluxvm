// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! IPv6 for the guest (enabled by `UserNetConfig::ipv6`): neighbour discovery for the gateway addresses, router
//! advertisements carrying a /64 prefix for SLAAC plus an RDNSS option (so no DHCPv6), ICMPv6 echo to the
//! gateway, and the dispatch of TCP/UDP into the shared NAT engine.
//!
//! The gateway answers as three addresses: `prefix::2` (gateway, with the optional host-loopback alias),
//! `prefix::3` (DNS) and its EUI-64 link-local address, which is the router address in the RAs. Not supported:
//! extension headers and fragments (dropped), outbound ICMPv6 echo (denied with `icmpv6-proxy-unsupported`),
//! and MLD. A v6 port forward targets `PortForward::guest_addr` or, failing that, the last in-prefix source
//! address seen from the guest.

use crate::wire::{self, Ipv6, Mac};
use crate::{Engine, Proto};
use std::net::Ipv6Addr;
use std::time::{Duration, Instant};

const ICMP6_PTB: u8 = 2;
const ICMP6_ECHO_REQ: u8 = 128;
const ICMP6_ECHO_REPLY: u8 = 129;
const ICMP6_RS: u8 = 133;
const ICMP6_RA: u8 = 134;
const ICMP6_NS: u8 = 135;
const ICMP6_NA: u8 = 136;

/// Unsolicited RA interval; the router lifetime below is far longer so a lost RA does not drop the route.
const RA_INTERVAL: Duration = Duration::from_secs(60);
const ROUTER_LIFETIME: u16 = 1800;

const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

fn is_link_local(a: Ipv6Addr) -> bool {
    (a.segments()[0] & 0xffc0) == 0xfe80
}

impl Engine {
    fn is_gateway_addr6(&self, a: Ipv6Addr) -> bool {
        a == self.cfg.gateway_ip6() || a == self.cfg.dns_ip6() || a == self.cfg.gateway_ll6()
    }

    /// An IPv6 packet with an explicit hop limit (ND requires 255).
    fn emit_v6(&self, src: Ipv6Addr, dst: Ipv6Addr, next: u8, hop: u8, payload: &[u8]) {
        let ip = wire::build_ipv6(src, dst, next, hop, payload);
        let mac = if dst.is_multicast() {
            wire::v6_multicast_mac(dst)
        } else {
            self.guest_dst_mac()
        };
        self.emit_frame(wire::ETH_IPV6, mac, &ip);
    }

    fn send_icmp6(&self, src: Ipv6Addr, dst: Ipv6Addr, ty: u8, body: &[u8], hop: u8) {
        let m = wire::build_icmpv6(src, dst, ty, 0, body);
        self.emit_v6(src, dst, wire::PROTO_ICMPV6, hop, &m);
    }

    pub(crate) fn on_ipv6(&mut self, _src_mac: Mac, p: &[u8]) {
        let Some(ip) = wire::parse_ipv6(p) else {
            return self.mark_dropped();
        };
        if ip.total_len > usize::from(self.cfg.mtu) {
            if self.cfg.in_prefix6(ip.src) {
                self.icmp6_too_big(&p[..ip.total_len], ip.src);
            }
            return self.mark_dropped();
        }
        match ip.next_header {
            wire::PROTO_ICMPV6 => self.on_icmpv6(&ip),
            wire::PROTO_UDP | wire::PROTO_TCP => {
                // NAT only for addresses inside the advertised prefix (SLAAC, including privacy addresses).
                if !self.cfg.in_prefix6(ip.src) || self.is_gateway_addr6(ip.src) {
                    return self.mark_dropped();
                }
                if self.guest_ip6 != Some(ip.src) {
                    self.guest_ip6 = Some(ip.src); // target of GuestFamily::V6 forwards
                }
                let (src, dst) = (ip.src.into(), ip.dst.into());
                if ip.next_header == wire::PROTO_UDP {
                    if let Some(u) = wire::parse_udp(ip.payload) {
                        self.on_udp(src, dst, &u);
                    }
                } else if let Some(t) = wire::parse_tcp(ip.payload) {
                    self.on_tcp(src, dst, &t);
                }
            }
            nh => {
                tracing::debug!(next_header = nh, "usernet: dropping unsupported IPv6 next header");
                self.mark_dropped();
            }
        }
    }

    /// ICMPv6 "packet too big" for a guest packet over our MTU.
    fn icmp6_too_big(&self, original: &[u8], guest: Ipv6Addr) {
        let mut body = Vec::new();
        body.extend_from_slice(&u32::from(self.cfg.mtu).to_be_bytes());
        body.extend_from_slice(&original[..original.len().min(1280 - 48)]);
        self.mark_dropped();
        self.send_icmp6(self.cfg.gateway_ip6(), guest, ICMP6_PTB, &body, 64);
    }

    fn on_icmpv6(&mut self, ip: &Ipv6<'_>) {
        let p = ip.payload;
        if p.len() < 8 {
            return;
        }
        let src_ok = ip.src.is_unspecified() || is_link_local(ip.src) || self.cfg.in_prefix6(ip.src);
        if !src_ok {
            return self.mark_dropped();
        }
        match p[0] {
            ICMP6_NS if ip.hop_limit == 255 && p.len() >= 24 => {
                let target = Ipv6Addr::from(<[u8; 16]>::try_from(&p[8..24]).unwrap());
                if !self.is_gateway_addr6(target) {
                    return; // someone else's address (DAD for the guest's own, or another host)
                }
                let (dst, solicited) = if ip.src.is_unspecified() {
                    (ALL_NODES, false)
                } else {
                    (ip.src, true)
                };
                let router = target != self.cfg.dns_ip6();
                let flags = (u8::from(router) << 7) | (u8::from(solicited) << 6) | 0x20;
                let mut body = vec![flags, 0, 0, 0];
                body.extend_from_slice(&target.octets());
                body.extend_from_slice(&[2, 1]);
                body.extend_from_slice(&self.cfg.gateway_mac);
                self.send_icmp6(target, dst, ICMP6_NA, &body, 255);
            }
            ICMP6_RS if ip.hop_limit == 255 => {
                let dst = if ip.src.is_unspecified() { ALL_NODES } else { ip.src };
                self.send_ra(dst);
            }
            ICMP6_ECHO_REQ => {
                if self.is_gateway_addr6(ip.dst) {
                    let mut body = p[4..].to_vec();
                    if body.len() < 4 {
                        body.resize(4, 0);
                    }
                    self.send_icmp6(ip.dst, ip.src, ICMP6_ECHO_REPLY, &body, 64);
                } else if self.cfg.in_prefix6(ip.src) {
                    let id = u16::from_be_bytes([p[4], p[5]]);
                    self.deny(
                        Proto::Icmp,
                        ip.src.into(),
                        id,
                        ip.dst.into(),
                        0,
                        "icmpv6-proxy-unsupported",
                    );
                }
            }
            _ => {}
        }
    }

    fn send_ra(&self, dst: Ipv6Addr) {
        let c = &self.cfg;
        let mut b = vec![64, 0];
        b.extend_from_slice(&ROUTER_LIFETIME.to_be_bytes());
        b.extend_from_slice(&[0; 8]); // reachable time, retransmit timer: unspecified
        // Source link-layer address.
        b.extend_from_slice(&[1, 1]);
        b.extend_from_slice(&c.gateway_mac);
        // MTU.
        b.extend_from_slice(&[5, 1, 0, 0]);
        b.extend_from_slice(&u32::from(c.mtu).to_be_bytes());
        // Prefix information: on-link + autonomous.
        b.extend_from_slice(&[3, 4, 64, 0xc0]);
        b.extend_from_slice(&86_400u32.to_be_bytes());
        b.extend_from_slice(&14_400u32.to_be_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&c.ipv6_prefix.octets());
        // RDNSS (RFC 8106).
        b.extend_from_slice(&[25, 3, 0, 0]);
        b.extend_from_slice(&u32::from(ROUTER_LIFETIME).to_be_bytes());
        b.extend_from_slice(&c.dns_ip6().octets());
        self.send_icmp6(c.gateway_ll6(), dst, ICMP6_RA, &b, 255);
    }

    pub(crate) fn v6_tick(&mut self, now: Instant) {
        if !self.cfg.ipv6 || self.guest_mac.is_none() {
            return;
        }
        if self.last_ra.is_none_or(|t| now.duration_since(t) >= RA_INTERVAL) {
            self.last_ra = Some(now);
            self.send_ra(ALL_NODES);
        }
    }
}
