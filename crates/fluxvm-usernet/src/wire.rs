// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Ethernet / ARP / IPv4 / IPv6 / UDP / TCP parsing and building. No checksum verification on input (a guest's
//! virtio-net may hand us partially checksummed frames); every frame we build carries correct checksums.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub type Mac = [u8; 6];
pub const BROADCAST: Mac = [0xff; 6];
pub const ETH_IPV4: u16 = 0x0800;
pub const ETH_ARP: u16 = 0x0806;
pub const ETH_IPV6: u16 = 0x86dd;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;
pub const PROTO_ICMPV6: u8 = 58;

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

fn sum_words(data: &[u8], mut sum: u32) -> u32 {
    let (words, rem) = data.as_chunks::<2>();
    for w in words {
        sum += u32::from(u16::from_be_bytes(*w));
    }
    if let [b] = rem {
        sum += u32::from(*b) << 8;
    }
    sum
}

fn fold(mut sum: u32) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

pub fn checksum(data: &[u8]) -> u16 {
    fold(sum_words(data, 0))
}

/// Internet checksum of an L4 segment including the IPv4 or IPv6 pseudo header.
pub fn l4_checksum(src: impl Into<IpAddr>, dst: impl Into<IpAddr>, proto: u8, l4: &[u8]) -> u16 {
    match (src.into(), dst.into()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            let mut p = [0u8; 12];
            p[0..4].copy_from_slice(&s.octets());
            p[4..8].copy_from_slice(&d.octets());
            p[9] = proto;
            p[10..12].copy_from_slice(&(l4.len() as u16).to_be_bytes());
            fold(sum_words(l4, sum_words(&p, 0)))
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            let mut p = [0u8; 40];
            p[0..16].copy_from_slice(&s.octets());
            p[16..32].copy_from_slice(&d.octets());
            p[32..36].copy_from_slice(&(l4.len() as u32).to_be_bytes());
            p[39] = proto;
            fold(sum_words(l4, sum_words(&p, 0)))
        }
        _ => 0xffff,
    }
}

pub struct Eth<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn parse_eth(f: &[u8]) -> Option<Eth<'_>> {
    if f.len() < 14 {
        return None;
    }
    let mut ethertype = u16::from_be_bytes([f[12], f[13]]);
    let mut off = 14;
    if ethertype == 0x8100 && f.len() >= 18 {
        ethertype = u16::from_be_bytes([f[16], f[17]]);
        off = 18;
    }
    Some(Eth {
        dst: f[0..6].try_into().ok()?,
        src: f[6..12].try_into().ok()?,
        ethertype,
        payload: &f[off..],
    })
}

pub fn build_eth(dst: Mac, src: Mac, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(14 + payload.len());
    v.extend_from_slice(&dst);
    v.extend_from_slice(&src);
    v.extend_from_slice(&ethertype.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub struct Ipv4<'a> {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub proto: u8,
    pub total_len: usize,
    /// More-fragments set or a non-zero fragment offset.
    pub fragment: bool,
    pub id: u16,
    pub df: bool,
    pub mf: bool,
    /// Fragment offset in bytes.
    pub frag_off: usize,
    pub payload: &'a [u8],
}

pub fn parse_ipv4(p: &[u8]) -> Option<Ipv4<'_>> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = usize::from(p[0] & 0xf) * 4;
    let total = usize::from(u16::from_be_bytes([p[2], p[3]]));
    if ihl < 20 || total < ihl || total > p.len() {
        return None;
    }
    let ff = u16::from_be_bytes([p[6], p[7]]);
    Some(Ipv4 {
        src: Ipv4Addr::new(p[12], p[13], p[14], p[15]),
        dst: Ipv4Addr::new(p[16], p[17], p[18], p[19]),
        proto: p[9],
        total_len: total,
        fragment: ff & 0x3fff != 0,
        id: u16::from_be_bytes([p[4], p[5]]),
        df: ff & 0x4000 != 0,
        mf: ff & 0x2000 != 0,
        frag_off: usize::from(ff & 0x1fff) * 8,
        payload: &p[ihl..total],
    })
}

/// IPv4 header (no options) plus payload with an explicit identification and DF bit.
pub fn build_ipv4_with(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    proto: u8,
    payload: &[u8],
    id: u16,
    df: bool,
) -> Vec<u8> {
    let mut v = vec![0u8; 20];
    v[0] = 0x45;
    v[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    v[4..6].copy_from_slice(&id.to_be_bytes());
    v[6] = if df { 0x40 } else { 0 };
    v[8] = 64;
    v[9] = proto;
    v[12..16].copy_from_slice(&src.octets());
    v[16..20].copy_from_slice(&dst.octets());
    let c = checksum(&v);
    v[10..12].copy_from_slice(&c.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn build_ipv4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
    build_ipv4_with(src, dst, proto, payload, 0, true)
}

/// Split an option-less IPv4 packet into fragments that each fit `mtu`. `None` when the packet has options or
/// `mtu` leaves no room for 8 payload bytes; a packet that already fits comes back as itself.
pub fn fragment_ipv4(packet: &[u8], mtu: usize) -> Option<Vec<Vec<u8>>> {
    if packet.len() < 20 || packet[0] != 0x45 {
        return None;
    }
    if packet.len() <= mtu {
        return Some(vec![packet.to_vec()]);
    }
    let per = mtu.checked_sub(20)? & !7;
    if per == 0 {
        return None;
    }
    let ff = u16::from_be_bytes([packet[6], packet[7]]);
    let base_off = usize::from(ff & 0x1fff) * 8;
    let orig_mf = ff & 0x2000 != 0;
    let body = &packet[20..];
    let mut out = Vec::new();
    let mut off = 0;
    while off < body.len() {
        let n = per.min(body.len() - off);
        let last = off + n == body.len();
        let mut h = packet[..20].to_vec();
        h[2..4].copy_from_slice(&((20 + n) as u16).to_be_bytes());
        let mut f = ((base_off + off) / 8) as u16 & 0x1fff;
        f |= ff & 0x4000;
        if !last || orig_mf {
            f |= 0x2000;
        }
        h[6..8].copy_from_slice(&f.to_be_bytes());
        h[10] = 0;
        h[11] = 0;
        let c = checksum(&h);
        h[10..12].copy_from_slice(&c.to_be_bytes());
        h.extend_from_slice(&body[off..off + n]);
        out.push(h);
        off += n;
    }
    Some(out)
}

pub struct Ipv6<'a> {
    pub src: Ipv6Addr,
    pub dst: Ipv6Addr,
    pub next_header: u8,
    pub hop_limit: u8,
    /// Total packet length (header + payload).
    pub total_len: usize,
    pub payload: &'a [u8],
}

pub fn parse_ipv6(p: &[u8]) -> Option<Ipv6<'_>> {
    if p.len() < 40 || p[0] >> 4 != 6 {
        return None;
    }
    let plen = usize::from(u16::from_be_bytes([p[4], p[5]]));
    if 40 + plen > p.len() {
        return None;
    }
    let a = |o: usize| -> [u8; 16] { p[o..o + 16].try_into().unwrap() };
    Some(Ipv6 {
        src: Ipv6Addr::from(a(8)),
        dst: Ipv6Addr::from(a(24)),
        next_header: p[6],
        hop_limit: p[7],
        total_len: 40 + plen,
        payload: &p[40..40 + plen],
    })
}

pub fn build_ipv6(
    src: Ipv6Addr,
    dst: Ipv6Addr,
    next_header: u8,
    hop_limit: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut v = vec![0u8; 40];
    v[0] = 0x60;
    v[4..6].copy_from_slice(&(payload.len() as u16).to_be_bytes());
    v[6] = next_header;
    v[7] = hop_limit;
    v[8..24].copy_from_slice(&src.octets());
    v[24..40].copy_from_slice(&dst.octets());
    v.extend_from_slice(payload);
    v
}

/// An ICMPv6 message `[type, code, csum, body...]` with its pseudo-header checksum filled in.
pub fn build_icmpv6(src: Ipv6Addr, dst: Ipv6Addr, ty: u8, code: u8, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + body.len());
    v.extend_from_slice(&[ty, code, 0, 0]);
    v.extend_from_slice(body);
    let c = l4_checksum(src, dst, PROTO_ICMPV6, &v);
    v[2..4].copy_from_slice(&c.to_be_bytes());
    v
}

/// Ethernet multicast MAC for an IPv6 multicast address (`33:33:` + last 4 octets).
pub fn v6_multicast_mac(a: Ipv6Addr) -> Mac {
    let o = a.octets();
    [0x33, 0x33, o[12], o[13], o[14], o[15]]
}

pub struct Udp<'a> {
    pub sport: u16,
    pub dport: u16,
    pub payload: &'a [u8],
}

pub fn parse_udp(p: &[u8]) -> Option<Udp<'_>> {
    if p.len() < 8 {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([p[4], p[5]]));
    if len < 8 || len > p.len() {
        return None;
    }
    Some(Udp {
        sport: u16::from_be_bytes([p[0], p[1]]),
        dport: u16::from_be_bytes([p[2], p[3]]),
        payload: &p[8..len],
    })
}

pub fn build_udp(
    src: impl Into<IpAddr>,
    dst: impl Into<IpAddr>,
    sport: u16,
    dport: u16,
    payload: &[u8],
) -> Vec<u8> {
    let (src, dst) = (src.into(), dst.into());
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&sport.to_be_bytes());
    v.extend_from_slice(&dport.to_be_bytes());
    v.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(payload);
    let mut c = l4_checksum(src, dst, PROTO_UDP, &v);
    if c == 0 {
        c = 0xffff;
    }
    v[6..8].copy_from_slice(&c.to_be_bytes());
    v
}

#[derive(Debug, Clone)]
pub struct Tcp<'a> {
    pub sport: u16,
    pub dport: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    /// Window-scale shift (RFC 7323), SYN only.
    pub wscale: Option<u8>,
    /// SACK-permitted option present (RFC 2018), SYN only.
    pub sack_ok: bool,
    /// SACK blocks (left edge, right edge), at most four.
    pub sack: Vec<(u32, u32)>,
    pub payload: &'a [u8],
}

pub fn parse_tcp(p: &[u8]) -> Option<Tcp<'_>> {
    if p.len() < 20 {
        return None;
    }
    let off = usize::from(p[12] >> 4) * 4;
    if off < 20 || off > p.len() {
        return None;
    }
    let (mut mss, mut wscale, mut sack_ok, mut sack) = (None, None, false, Vec::new());
    let mut o = &p[20..off];
    while let Some(&k) = o.first() {
        match k {
            0 => break,
            1 => o = &o[1..],
            _ => {
                let l = usize::from(*o.get(1)?);
                if l < 2 || l > o.len() {
                    break;
                }
                match (k, l) {
                    (2, 4) => mss = Some(u16::from_be_bytes([o[2], o[3]])),
                    (3, 3) => wscale = Some(o[2].min(14)),
                    (4, 2) => sack_ok = true,
                    (5, l) if l >= 10 && (l - 2) % 8 == 0 => {
                        for b in o[2..l].as_chunks::<8>().0.iter().take(4) {
                            sack.push((
                                u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
                                u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
                            ));
                        }
                    }
                    _ => {}
                }
                o = &o[l..];
            }
        }
    }
    Some(Tcp {
        sport: u16::from_be_bytes([p[0], p[1]]),
        dport: u16::from_be_bytes([p[2], p[3]]),
        seq: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        ack: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
        flags: p[13],
        window: u16::from_be_bytes([p[14], p[15]]),
        mss,
        wscale,
        sack_ok,
        sack,
        payload: &p[off..],
    })
}

/// A TCP segment to emit (ports are supplied when it is wrapped).
#[derive(Debug, Clone, Default)]
pub struct TcpOut {
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub mss: Option<u16>,
    pub wscale: Option<u8>,
    pub sack_ok: bool,
    pub sack: Vec<(u32, u32)>,
    pub payload: Vec<u8>,
}

pub fn build_tcp(
    src: impl Into<IpAddr>,
    dst: impl Into<IpAddr>,
    sport: u16,
    dport: u16,
    o: &TcpOut,
) -> Vec<u8> {
    let (src, dst) = (src.into(), dst.into());
    let mut opts: Vec<u8> = Vec::new();
    if let Some(m) = o.mss {
        opts.extend_from_slice(&[2, 4]);
        opts.extend_from_slice(&m.to_be_bytes());
    }
    if o.sack_ok {
        opts.extend_from_slice(&[4, 2]);
    }
    if let Some(s) = o.wscale {
        opts.extend_from_slice(&[1, 3, 3, s]);
    }
    if !o.sack.is_empty() {
        let n = o.sack.len().min(4);
        opts.extend_from_slice(&[1, 1, 5, (2 + 8 * n) as u8]);
        for (l, r) in &o.sack[..n] {
            opts.extend_from_slice(&l.to_be_bytes());
            opts.extend_from_slice(&r.to_be_bytes());
        }
    }
    while !opts.len().is_multiple_of(4) {
        opts.push(1);
    }
    let hlen = 20 + opts.len();
    let mut v = Vec::with_capacity(hlen + o.payload.len());
    v.extend_from_slice(&sport.to_be_bytes());
    v.extend_from_slice(&dport.to_be_bytes());
    v.extend_from_slice(&o.seq.to_be_bytes());
    v.extend_from_slice(&o.ack.to_be_bytes());
    v.push(((hlen / 4) as u8) << 4);
    v.push(o.flags);
    v.extend_from_slice(&o.window.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]);
    v.extend_from_slice(&opts);
    v.extend_from_slice(&o.payload);
    let c = l4_checksum(src, dst, PROTO_TCP, &v);
    v[16..18].copy_from_slice(&c.to_be_bytes());
    v
}

pub fn build_arp(op: u16, smac: Mac, sip: Ipv4Addr, tmac: Mac, tip: Ipv4Addr) -> Vec<u8> {
    let mut v = Vec::with_capacity(28);
    v.extend_from_slice(&[0, 1, 8, 0, 6, 4]);
    v.extend_from_slice(&op.to_be_bytes());
    v.extend_from_slice(&smac);
    v.extend_from_slice(&sip.octets());
    v.extend_from_slice(&tmac);
    v.extend_from_slice(&tip.octets());
    v
}

/// (op, sender mac, sender ip, target ip) of an Ethernet/IPv4 ARP packet.
pub fn parse_arp(p: &[u8]) -> Option<(u16, Mac, Ipv4Addr, Ipv4Addr)> {
    if p.len() < 28 || p[0..6] != [0, 1, 8, 0, 6, 4] {
        return None;
    }
    Some((
        u16::from_be_bytes([p[6], p[7]]),
        p[8..14].try_into().ok()?,
        Ipv4Addr::new(p[14], p[15], p[16], p[17]),
        Ipv4Addr::new(p[24], p[25], p[26], p[27]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_roundtrip() {
        let a = Ipv4Addr::new(10, 0, 2, 15);
        let b = Ipv4Addr::new(1, 2, 3, 4);
        let ip = build_ipv4(a, b, PROTO_UDP, &build_udp(a, b, 1, 2, b"hi"));
        assert_eq!(checksum(&ip[..20]), 0);
        let p = parse_ipv4(&ip).unwrap();
        assert_eq!(l4_checksum(a, b, PROTO_UDP, p.payload), 0);
        let u = parse_udp(p.payload).unwrap();
        assert_eq!(u.payload, b"hi");
        let t = build_tcp(
            a,
            b,
            1,
            2,
            &TcpOut {
                seq: 5,
                flags: SYN,
                mss: Some(1460),
                payload: b"abc".to_vec(),
                ..Default::default()
            },
        );
        assert_eq!(l4_checksum(a, b, PROTO_TCP, &t), 0);
        let pt = parse_tcp(&t).unwrap();
        assert_eq!((pt.mss, pt.payload), (Some(1460), &b"abc"[..]));
    }

    #[test]
    fn tcp_options_roundtrip() {
        let a = Ipv4Addr::new(10, 0, 2, 15);
        let b = Ipv4Addr::new(1, 2, 3, 4);
        let t = build_tcp(
            a,
            b,
            1,
            2,
            &TcpOut {
                flags: SYN,
                mss: Some(1400),
                wscale: Some(7),
                sack_ok: true,
                ..Default::default()
            },
        );
        assert_eq!(l4_checksum(a, b, PROTO_TCP, &t), 0);
        let p = parse_tcp(&t).unwrap();
        assert_eq!((p.mss, p.wscale, p.sack_ok), (Some(1400), Some(7), true));
        let t = build_tcp(
            a,
            b,
            1,
            2,
            &TcpOut {
                flags: ACK,
                sack: vec![(10, 20), (30, 40), (50, 60)],
                ..Default::default()
            },
        );
        assert_eq!(l4_checksum(a, b, PROTO_TCP, &t), 0);
        assert_eq!(parse_tcp(&t).unwrap().sack, vec![(10, 20), (30, 40), (50, 60)]);
    }

    #[test]
    fn ipv6_roundtrip_and_checksum() {
        let a: Ipv6Addr = "fd00::15".parse().unwrap();
        let b: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let u = build_udp(a, b, 1000, 53, b"hello");
        let ip = build_ipv6(a, b, PROTO_UDP, 64, &u);
        let p = parse_ipv6(&ip).unwrap();
        assert_eq!((p.src, p.dst, p.next_header), (a, b, PROTO_UDP));
        assert_eq!(l4_checksum(a, b, PROTO_UDP, p.payload), 0);
        let ic = build_icmpv6(a, b, 128, 0, &[0, 1, 0, 2]);
        assert_eq!(l4_checksum(a, b, PROTO_ICMPV6, &ic), 0);
        assert_eq!(
            v6_multicast_mac("ff02::1:ff00:1".parse().unwrap()),
            [0x33, 0x33, 0xff, 0, 0, 1]
        );
    }

    #[test]
    fn fragmentation_roundtrips() {
        let a = Ipv4Addr::new(10, 0, 2, 2);
        let b = Ipv4Addr::new(10, 0, 2, 15);
        let body: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let ip = build_ipv4_with(a, b, PROTO_UDP, &body, 77, false);
        let frags = fragment_ipv4(&ip, 1500).unwrap();
        assert_eq!(frags.len(), 3);
        let mut joined = vec![0u8; 3000];
        for (i, f) in frags.iter().enumerate() {
            assert!(f.len() <= 1500);
            assert_eq!(checksum(&f[..20]), 0);
            let p = parse_ipv4(f).unwrap();
            assert_eq!((p.id, p.mf, p.fragment), (77, i + 1 < frags.len(), true));
            joined[p.frag_off..p.frag_off + p.payload.len()].copy_from_slice(p.payload);
        }
        assert_eq!(joined, body);
        assert_eq!(fragment_ipv4(&ip, 4000).unwrap().len(), 1);
    }
}
