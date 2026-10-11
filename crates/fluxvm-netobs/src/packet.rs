// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A bounds-checked parser for the frames a guest NIC carries: Ethernet (with 802.1Q/802.1ad tags), ARP, IPv4,
//! IPv6 (with extension headers), TCP, UDP and ICMP/ICMPv6.
//!
//! Nothing here panics on hostile input: every read is checked, and a frame that is cut short or lies about its
//! lengths yields a [`Packet`] with `truncated` set and as many layers as could be decoded. The parser borrows the
//! frame; header fields are small copies.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const ETH_P_IPV4: u16 = 0x0800;
pub const ETH_P_ARP: u16 = 0x0806;
pub const ETH_P_VLAN: u16 = 0x8100;
pub const ETH_P_QINQ: u16 = 0x88a8;
pub const ETH_P_IPV6: u16 = 0x86dd;

pub const IPPROTO_ICMP: u8 = 1;
pub const IPPROTO_TCP: u8 = 6;
pub const IPPROTO_UDP: u8 = 17;
pub const IPPROTO_ICMPV6: u8 = 58;

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;

pub type Mac = [u8; 6];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arp {
    pub op: u16,
    pub sender_mac: Mac,
    pub sender_ip: Ipv4Addr,
    pub target_mac: Mac,
    pub target_ip: Ipv4Addr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ip {
    pub version: u8,
    pub src: IpAddr,
    pub dst: IpAddr,
    /// The upper-layer protocol after any IPv6 extension headers.
    pub proto: u8,
    pub ttl: u8,
    /// The datagram is a fragment (first or later).
    pub fragmented: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tcp {
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub wscale: Option<u8>,
    /// Payload bytes present in the frame, bounded by what the IP header declares.
    pub payload_len: usize,
}

impl Tcp {
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Udp {
    pub sport: u16,
    pub dport: u16,
    pub len: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icmp {
    pub v6: bool,
    pub kind: u8,
    pub code: u8,
    /// Identifier and sequence for echo request/reply; zero otherwise.
    pub id: u16,
    pub seq: u16,
}

impl Icmp {
    pub fn is_echo_request(&self) -> bool {
        (!self.v6 && self.kind == 8) || (self.v6 && self.kind == 128)
    }
    pub fn is_echo_reply(&self) -> bool {
        (!self.v6 && self.kind == 0) || (self.v6 && self.kind == 129)
    }
    pub fn is_unreachable(&self) -> bool {
        (!self.v6 && self.kind == 3) || (self.v6 && self.kind == 1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Net {
    /// Not a protocol we decode (or the L3 header was unusable).
    None,
    Arp(Arp),
    Ip(Ip),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum L4 {
    None,
    Tcp(Tcp),
    Udp(Udp),
    Icmp(Icmp),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet<'a> {
    pub dst_mac: Mac,
    pub src_mac: Mac,
    /// Outermost VLAN id, if the frame was tagged.
    pub vlan: Option<u16>,
    /// The ethertype after any VLAN tags.
    pub ethertype: u16,
    pub net: Net,
    pub l4: L4,
    /// Bytes after the TCP/UDP/ICMP header (excluding Ethernet padding).
    pub payload: &'a [u8],
    pub frame_len: usize,
    /// Some header or declared length did not fit in the frame.
    pub truncated: bool,
}

impl Packet<'_> {
    pub fn ip(&self) -> Option<&Ip> {
        match &self.net {
            Net::Ip(ip) => Some(ip),
            _ => None,
        }
    }

    /// `(sport, dport)` for TCP/UDP, `(0, 0)` otherwise.
    pub fn ports(&self) -> (u16, u16) {
        match &self.l4 {
            L4::Tcp(t) => (t.sport, t.dport),
            L4::Udp(u) => (u.sport, u.dport),
            _ => (0, 0),
        }
    }
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

fn mac_at(b: &[u8], at: usize) -> Option<Mac> {
    b.get(at..at + 6)?.try_into().ok()
}

/// Parses one Ethernet frame. Returns `None` only when there is no complete Ethernet header.
pub fn parse(frame: &[u8]) -> Option<Packet<'_>> {
    let dst_mac = mac_at(frame, 0)?;
    let src_mac = mac_at(frame, 6)?;
    let mut ethertype = be16(frame, 12)?;
    let mut off = 14usize;
    let mut vlan = None;
    let mut truncated = false;
    // Up to two stacked tags (802.1ad outer + 802.1Q inner); a third is treated as an opaque ethertype.
    for _ in 0..2 {
        if ethertype != ETH_P_VLAN && ethertype != ETH_P_QINQ {
            break;
        }
        let (Some(tci), Some(next)) = (be16(frame, off), be16(frame, off + 2)) else {
            truncated = true;
            ethertype = 0;
            break;
        };
        if vlan.is_none() {
            vlan = Some(tci & 0x0fff);
        }
        ethertype = next;
        off += 4;
    }
    let mut pkt = Packet {
        dst_mac,
        src_mac,
        vlan,
        ethertype,
        net: Net::None,
        l4: L4::None,
        payload: &[],
        frame_len: frame.len(),
        truncated,
    };
    let l3 = frame.get(off..).unwrap_or(&[]);
    match ethertype {
        ETH_P_ARP => parse_arp(l3, &mut pkt),
        ETH_P_IPV4 => parse_ipv4(l3, &mut pkt),
        ETH_P_IPV6 => parse_ipv6(l3, &mut pkt),
        _ => {}
    }
    Some(pkt)
}

fn parse_arp(b: &[u8], pkt: &mut Packet<'_>) {
    // Ethernet/IPv4 ARP only: htype 1, ptype 0x0800, hlen 6, plen 4.
    let ok = be16(b, 0) == Some(1)
        && be16(b, 2) == Some(ETH_P_IPV4)
        && b.get(4) == Some(&6)
        && b.get(5) == Some(&4);
    if !ok || b.len() < 28 {
        pkt.truncated = true;
        return;
    }
    let ip = |at: usize| Ipv4Addr::new(b[at], b[at + 1], b[at + 2], b[at + 3]);
    pkt.net = Net::Arp(Arp {
        op: u16::from_be_bytes([b[6], b[7]]),
        sender_mac: b[8..14].try_into().unwrap_or([0; 6]),
        sender_ip: ip(14),
        target_mac: b[18..24].try_into().unwrap_or([0; 6]),
        target_ip: ip(24),
    });
}

fn parse_ipv4<'a>(b: &'a [u8], pkt: &mut Packet<'a>) {
    let Some(&vihl) = b.first() else {
        pkt.truncated = true;
        return;
    };
    let ihl = ((vihl & 0x0f) as usize) * 4;
    if vihl >> 4 != 4 || ihl < 20 || b.len() < ihl {
        pkt.truncated = true;
        return;
    }
    let total = be16(b, 2).unwrap_or(0) as usize;
    // total == 0 happens with segmentation offload; fall back to the frame length. A total smaller than the header
    // is a lie; trust the frame.
    let end = if total >= ihl {
        total.min(b.len())
    } else {
        b.len()
    };
    if total > b.len() {
        pkt.truncated = true;
    }
    let frag = be16(b, 6).unwrap_or(0);
    let frag_off = frag & 0x1fff;
    let more = frag & 0x2000 != 0;
    let proto = b[9];
    pkt.net = Net::Ip(Ip {
        version: 4,
        src: IpAddr::V4(Ipv4Addr::new(b[12], b[13], b[14], b[15])),
        dst: IpAddr::V4(Ipv4Addr::new(b[16], b[17], b[18], b[19])),
        proto,
        ttl: b[8],
        fragmented: frag_off != 0 || more,
    });
    if frag_off != 0 {
        // A later fragment has no L4 header to decode.
        pkt.payload = &b[ihl.min(end)..end];
        return;
    }
    parse_l4(proto, &b[ihl.min(end)..end], pkt);
}

fn parse_ipv6<'a>(b: &'a [u8], pkt: &mut Packet<'a>) {
    if b.len() < 40 || b[0] >> 4 != 6 {
        pkt.truncated = true;
        return;
    }
    let plen = be16(b, 4).unwrap_or(0) as usize;
    let end = if plen == 0 {
        b.len()
    } else {
        (40 + plen).min(b.len())
    };
    if plen != 0 && 40 + plen > b.len() {
        pkt.truncated = true;
    }
    let addr = |at: usize| -> Ipv6Addr {
        let mut o = [0u8; 16];
        o.copy_from_slice(&b[at..at + 16]);
        Ipv6Addr::from(o)
    };
    let mut next = b[6];
    let mut off = 40usize;
    let mut fragmented = false;
    let mut later_fragment = false;
    // Walk the extension-header chain, bounded so a crafted chain cannot spin.
    for _ in 0..8 {
        match next {
            0 | 43 | 60 => {
                // Hop-by-hop, routing, destination options: next, hdr-ext-len (8-byte units, excluding first 8).
                let (Some(&n), Some(&l)) = (b.get(off), b.get(off + 1)) else {
                    pkt.truncated = true;
                    break;
                };
                next = n;
                off += 8 + (l as usize) * 8;
            }
            44 => {
                let (Some(&n), Some(fo)) = (b.get(off), be16(b, off + 2)) else {
                    pkt.truncated = true;
                    break;
                };
                fragmented = true;
                if fo >> 3 != 0 {
                    later_fragment = true;
                }
                next = n;
                off += 8;
            }
            51 => {
                // Authentication header: length in 4-byte units minus 2.
                let (Some(&n), Some(&l)) = (b.get(off), b.get(off + 1)) else {
                    pkt.truncated = true;
                    break;
                };
                next = n;
                off += (l as usize + 2) * 4;
            }
            _ => break,
        }
        if off > end {
            pkt.truncated = true;
            off = end;
            break;
        }
    }
    pkt.net = Net::Ip(Ip {
        version: 6,
        src: IpAddr::V6(addr(8)),
        dst: IpAddr::V6(addr(24)),
        proto: next,
        ttl: b[7],
        fragmented,
    });
    if later_fragment {
        pkt.payload = &b[off.min(end)..end];
        return;
    }
    parse_l4(next, &b[off.min(end)..end], pkt);
}

fn parse_l4<'a>(proto: u8, b: &'a [u8], pkt: &mut Packet<'a>) {
    match proto {
        IPPROTO_TCP => {
            if b.len() < 20 {
                pkt.truncated = true;
                return;
            }
            let doff = ((b[12] >> 4) as usize) * 4;
            if doff < 20 || doff > b.len() {
                pkt.truncated = true;
                return;
            }
            let (mut mss, mut wscale) = (None, None);
            let opts = &b[20..doff];
            let mut i = 0;
            while i < opts.len() {
                match opts[i] {
                    0 => break,
                    1 => i += 1,
                    kind => {
                        let Some(&len) = opts.get(i + 1) else { break };
                        let len = len as usize;
                        if len < 2 || i + len > opts.len() {
                            break;
                        }
                        match (kind, len) {
                            (2, 4) => mss = be16(opts, i + 2),
                            (3, 3) => wscale = Some(opts[i + 2]),
                            _ => {}
                        }
                        i += len;
                    }
                }
            }
            let payload = &b[doff..];
            pkt.l4 = L4::Tcp(Tcp {
                sport: u16::from_be_bytes([b[0], b[1]]),
                dport: u16::from_be_bytes([b[2], b[3]]),
                seq: be32(b, 4).unwrap_or(0),
                ack: be32(b, 8).unwrap_or(0),
                flags: b[13],
                window: u16::from_be_bytes([b[14], b[15]]),
                mss,
                wscale,
                payload_len: payload.len(),
            });
            pkt.payload = payload;
        }
        IPPROTO_UDP => {
            if b.len() < 8 {
                pkt.truncated = true;
                return;
            }
            let len = u16::from_be_bytes([b[4], b[5]]);
            // Honour the UDP length when plausible, so Ethernet padding is not mistaken for payload.
            let end = if (len as usize) >= 8 {
                (len as usize).min(b.len())
            } else {
                b.len()
            };
            pkt.l4 = L4::Udp(Udp {
                sport: u16::from_be_bytes([b[0], b[1]]),
                dport: u16::from_be_bytes([b[2], b[3]]),
                len,
            });
            pkt.payload = &b[8..end];
        }
        IPPROTO_ICMP | IPPROTO_ICMPV6 => {
            if b.len() < 4 {
                pkt.truncated = true;
                return;
            }
            let v6 = proto == IPPROTO_ICMPV6;
            let mut icmp = Icmp {
                v6,
                kind: b[0],
                code: b[1],
                id: 0,
                seq: 0,
            };
            if icmp.is_echo_request() || icmp.is_echo_reply() {
                match (be16(b, 4), be16(b, 6)) {
                    (Some(id), Some(seq)) => {
                        icmp.id = id;
                        icmp.seq = seq;
                    }
                    _ => pkt.truncated = true,
                }
            }
            pkt.l4 = L4::Icmp(icmp);
            pkt.payload = b.get(8..).unwrap_or(&[]);
        }
        _ => {
            pkt.payload = b;
        }
    }
}

/// Frame builders for tests and tools (IPv4 only; checksums are left zero).
pub mod build {
    use super::*;

    pub const GUEST_MAC: Mac = [2, 0, 0, 0, 0, 1];
    pub const PEER_MAC: Mac = [2, 0, 0, 0, 0, 2];

    pub fn eth(dst: Mac, src: Mac, ethertype: u16, body: &[u8]) -> Vec<u8> {
        let mut f = Vec::with_capacity(14 + body.len());
        f.extend_from_slice(&dst);
        f.extend_from_slice(&src);
        f.extend_from_slice(&ethertype.to_be_bytes());
        f.extend_from_slice(body);
        f
    }

    fn ipv4_header(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, l4_len: usize) -> Vec<u8> {
        let mut h = vec![0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, proto, 0, 0];
        h[2..4].copy_from_slice(&((20 + l4_len) as u16).to_be_bytes());
        h.extend_from_slice(&src.octets());
        h.extend_from_slice(&dst.octets());
        h
    }

    pub fn tcp_v4(
        src: (Ipv4Addr, u16),
        dst: (Ipv4Addr, u16),
        seq: u32,
        ack: u32,
        flags: u8,
        window: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut t = Vec::new();
        t.extend_from_slice(&src.1.to_be_bytes());
        t.extend_from_slice(&dst.1.to_be_bytes());
        t.extend_from_slice(&seq.to_be_bytes());
        t.extend_from_slice(&ack.to_be_bytes());
        t.push(5 << 4);
        t.push(flags);
        t.extend_from_slice(&window.to_be_bytes());
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend_from_slice(payload);
        let mut ip = ipv4_header(src.0, dst.0, IPPROTO_TCP, t.len());
        ip.extend_from_slice(&t);
        eth(PEER_MAC, GUEST_MAC, ETH_P_IPV4, &ip)
    }

    pub fn udp_v4(src: (Ipv4Addr, u16), dst: (Ipv4Addr, u16), payload: &[u8]) -> Vec<u8> {
        let mut u = Vec::new();
        u.extend_from_slice(&src.1.to_be_bytes());
        u.extend_from_slice(&dst.1.to_be_bytes());
        u.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]);
        u.extend_from_slice(payload);
        let mut ip = ipv4_header(src.0, dst.0, IPPROTO_UDP, u.len());
        ip.extend_from_slice(&u);
        eth(PEER_MAC, GUEST_MAC, ETH_P_IPV4, &ip)
    }

    pub fn icmp_echo_v4(src: Ipv4Addr, dst: Ipv4Addr, reply: bool, id: u16, seq: u16) -> Vec<u8> {
        let mut m = vec![if reply { 0 } else { 8 }, 0, 0, 0];
        m.extend_from_slice(&id.to_be_bytes());
        m.extend_from_slice(&seq.to_be_bytes());
        let mut ip = ipv4_header(src, dst, IPPROTO_ICMP, m.len());
        ip.extend_from_slice(&m);
        eth(PEER_MAC, GUEST_MAC, ETH_P_IPV4, &ip)
    }
}

#[cfg(test)]
mod tests {
    use super::build::*;
    use super::*;

    const A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);

    #[test]
    fn tcp_syn_with_options() {
        let mut t = vec![
            0x30,
            0x39,
            0x01,
            0xbb,
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            0,
            7 << 4,
            TCP_SYN,
            0xff,
            0xff,
            0,
            0,
            0,
            0,
        ];
        t.extend_from_slice(&[2, 4, 0x05, 0xb4, 1, 3, 3, 7]);
        let mut ip = vec![
            0x45, 0, 0, 0, 0, 0, 0, 0, 64, 6, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2,
        ];
        let total = (20 + t.len()) as u16;
        ip[2..4].copy_from_slice(&total.to_be_bytes());
        ip.extend_from_slice(&t);
        let f = eth([1; 6], [2; 6], ETH_P_IPV4, &ip);
        let p = parse(&f).unwrap();
        let L4::Tcp(t) = &p.l4 else { panic!("not tcp") };
        assert_eq!((t.sport, t.dport), (12345, 443));
        assert_eq!(t.mss, Some(1460));
        assert_eq!(t.wscale, Some(7));
        assert!(t.has(TCP_SYN) && !t.has(TCP_ACK));
        assert_eq!(t.payload_len, 0);
        assert!(!p.truncated);
    }

    #[test]
    fn udp_payload_ignores_ethernet_padding() {
        let mut f = udp_v4((A, 5000), (B, 53), b"abc");
        f.extend_from_slice(&[0u8; 20]);
        let p = parse(&f).unwrap();
        assert_eq!(p.payload, b"abc");
        assert_eq!(p.ports(), (5000, 53));
    }

    #[test]
    fn vlan_and_qinq() {
        let inner = udp_v4((A, 1), (B, 2), b"x");
        let body = &inner[14..];
        let mut tagged = Vec::new();
        tagged.extend_from_slice(&[0; 12]);
        tagged.extend_from_slice(&ETH_P_QINQ.to_be_bytes());
        tagged.extend_from_slice(&[0x00, 0x64]);
        tagged.extend_from_slice(&ETH_P_VLAN.to_be_bytes());
        tagged.extend_from_slice(&[0x00, 0xc8]);
        tagged.extend_from_slice(&ETH_P_IPV4.to_be_bytes());
        tagged.extend_from_slice(body);
        let p = parse(&tagged).unwrap();
        assert_eq!(p.vlan, Some(100));
        assert_eq!(p.ethertype, ETH_P_IPV4);
        assert!(matches!(p.l4, L4::Udp(_)));
    }

    #[test]
    fn arp_request() {
        let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1];
        a.extend_from_slice(&[2, 0, 0, 0, 0, 1]);
        a.extend_from_slice(&[10, 0, 0, 1]);
        a.extend_from_slice(&[0; 6]);
        a.extend_from_slice(&[10, 0, 0, 2]);
        let f = eth([0xff; 6], [2, 0, 0, 0, 0, 1], ETH_P_ARP, &a);
        let p = parse(&f).unwrap();
        match p.net {
            Net::Arp(a) => {
                assert_eq!(a.op, 1);
                assert_eq!(a.target_ip, B);
            }
            _ => panic!("not arp"),
        }
    }

    #[test]
    fn ipv6_with_hop_by_hop_and_tcp() {
        let mut t = vec![
            0x1f,
            0x90,
            0x00,
            0x50,
            0,
            0,
            0,
            9,
            0,
            0,
            0,
            0,
            5 << 4,
            TCP_ACK,
            0,
            10,
            0,
            0,
            0,
            0,
        ];
        t.extend_from_slice(b"hi");
        let mut ip = vec![0x60, 0, 0, 0, 0, 0, 0, 64];
        ip.extend_from_slice(&[0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        ip.extend_from_slice(&[0xfd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
        let hbh = [6u8, 0, 1, 4, 0, 0, 0, 0];
        let plen = (hbh.len() + t.len()) as u16;
        ip[4..6].copy_from_slice(&plen.to_be_bytes());
        ip[6] = 0; // next header: hop-by-hop
        ip.extend_from_slice(&hbh);
        ip.extend_from_slice(&t);
        let f = eth([1; 6], [2; 6], ETH_P_IPV6, &ip);
        let p = parse(&f).unwrap();
        assert_eq!(p.ip().unwrap().proto, IPPROTO_TCP);
        assert_eq!(p.ports(), (8080, 80));
        assert_eq!(p.payload, b"hi");
    }

    #[test]
    fn icmp_echo() {
        let f = icmp_echo_v4(A, B, false, 7, 9);
        let p = parse(&f).unwrap();
        let L4::Icmp(i) = p.l4 else { panic!() };
        assert!(i.is_echo_request());
        assert_eq!((i.id, i.seq), (7, 9));
    }

    #[test]
    fn truncated_frames_never_panic() {
        let good = [
            tcp_v4(
                (A, 1000),
                (B, 80),
                1,
                0,
                TCP_SYN,
                100,
                b"GET / HTTP/1.1\r\n",
            ),
            udp_v4((A, 1000), (B, 53), &[0u8; 40]),
            icmp_echo_v4(A, B, false, 1, 1),
        ];
        for f in &good {
            for n in 0..=f.len() {
                let _ = parse(&f[..n]);
            }
        }
    }

    #[test]
    fn random_bytes_never_panic() {
        // xorshift: deterministic fuzz without a dependency.
        let mut s = 0x9e3779b97f4a7c15u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..20_000 {
            let len = (next() % 200) as usize;
            let mut f: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Bias toward plausible headers so deeper layers are exercised.
            if len > 14 && next() % 2 == 0 {
                let et = [ETH_P_IPV4, ETH_P_IPV6, ETH_P_ARP, ETH_P_VLAN][(next() % 4) as usize];
                f[12..14].copy_from_slice(&et.to_be_bytes());
                if et == ETH_P_IPV4 {
                    f[14] = 0x45 | ((next() as u8) & 0x0a);
                }
                if et == ETH_P_IPV6 {
                    f[14] = 0x60;
                }
            }
            let _ = parse(&f);
        }
    }

    #[test]
    fn lying_lengths_are_survivable() {
        let mut f = tcp_v4((A, 1), (B, 2), 0, 0, TCP_ACK, 1, b"data");
        f[16..18].copy_from_slice(&5000u16.to_be_bytes());
        let p = parse(&f).unwrap();
        assert!(p.truncated);
        assert_eq!(p.payload, b"data");
        let mut f = tcp_v4((A, 1), (B, 2), 0, 0, TCP_ACK, 1, b"");
        f[14 + 20 + 12] = 15 << 4;
        let p = parse(&f).unwrap();
        assert!(p.truncated);
        assert_eq!(p.l4, L4::None);
    }
}
