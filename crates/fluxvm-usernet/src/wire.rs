// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Ethernet / ARP / IPv4 / UDP / TCP parsing and building. No checksum verification on input (a guest's virtio-net
//! may hand us partially checksummed frames); every frame we build carries correct checksums.

use std::net::Ipv4Addr;

pub type Mac = [u8; 6];
pub const BROADCAST: Mac = [0xff; 6];
pub const ETH_IPV4: u16 = 0x0800;
pub const ETH_ARP: u16 = 0x0806;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;

fn sum_words(data: &[u8], mut sum: u32) -> u32 {
    let mut it = data.chunks_exact(2);
    for w in &mut it {
        sum += u32::from(u16::from_be_bytes([w[0], w[1]]));
    }
    if let [b] = it.remainder() {
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

pub fn l4_checksum(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, l4: &[u8]) -> u16 {
    let mut p = [0u8; 12];
    p[0..4].copy_from_slice(&src.octets());
    p[4..8].copy_from_slice(&dst.octets());
    p[9] = proto;
    p[10..12].copy_from_slice(&(l4.len() as u16).to_be_bytes());
    fold(sum_words(l4, sum_words(&p, 0)))
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
    pub fragment: bool,
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
        payload: &p[ihl..total],
    })
}

pub fn build_ipv4(src: Ipv4Addr, dst: Ipv4Addr, proto: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = vec![0u8; 20];
    v[0] = 0x45;
    v[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    v[6] = 0x40; // DF
    v[8] = 64;
    v[9] = proto;
    v[12..16].copy_from_slice(&src.octets());
    v[16..20].copy_from_slice(&dst.octets());
    let c = checksum(&v);
    v[10..12].copy_from_slice(&c.to_be_bytes());
    v.extend_from_slice(payload);
    v
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

pub fn build_udp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
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
    let mut mss = None;
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
                if k == 2 && l == 4 {
                    mss = Some(u16::from_be_bytes([o[2], o[3]]));
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
    pub payload: Vec<u8>,
}

pub fn build_tcp(src: Ipv4Addr, dst: Ipv4Addr, sport: u16, dport: u16, o: &TcpOut) -> Vec<u8> {
    let hlen = if o.mss.is_some() { 24 } else { 20 };
    let mut v = Vec::with_capacity(hlen + o.payload.len());
    v.extend_from_slice(&sport.to_be_bytes());
    v.extend_from_slice(&dport.to_be_bytes());
    v.extend_from_slice(&o.seq.to_be_bytes());
    v.extend_from_slice(&o.ack.to_be_bytes());
    v.push(((hlen / 4) as u8) << 4);
    v.push(o.flags);
    v.extend_from_slice(&o.window.to_be_bytes());
    v.extend_from_slice(&[0, 0, 0, 0]);
    if let Some(m) = o.mss {
        v.extend_from_slice(&[2, 4]);
        v.extend_from_slice(&m.to_be_bytes());
    }
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
}
