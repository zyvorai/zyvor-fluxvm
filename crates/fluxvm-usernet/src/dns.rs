// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Just enough DNS wire handling for a policy-checking forwarder.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub struct Query {
    pub name: String,
    /// Offset one past the first question.
    pub question_end: usize,
}

/// Read a (possibly compressed) name at `off`; returns the name and the offset after it in the *original* position.
fn read_name(buf: &[u8], mut off: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut end = None;
    let mut hops = 0;
    loop {
        let len = *buf.get(off)?;
        if len & 0xc0 == 0xc0 {
            let lo = *buf.get(off + 1)?;
            if end.is_none() {
                end = Some(off + 2);
            }
            off = usize::from(len & 0x3f) << 8 | usize::from(lo);
            hops += 1;
            if hops > 16 {
                return None;
            }
            continue;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        off += 1;
        if len == 0 {
            return Some((name, end.unwrap_or(off)));
        }
        let l = usize::from(len);
        let label = buf.get(off..off + l)?;
        if !name.is_empty() {
            name.push('.');
        }
        for &b in label {
            name.push(char::from(b));
        }
        if name.len() > 255 {
            return None;
        }
        off += l;
    }
}

pub fn parse_query(buf: &[u8]) -> Option<Query> {
    if buf.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    let qd = u16::from_be_bytes([buf[4], buf[5]]);
    if flags & 0x8000 != 0 || qd == 0 {
        return None;
    }
    let (name, off) = read_name(buf, 12)?;
    buf.get(off..off + 4)?;
    Some(Query {
        name,
        question_end: off + 4,
    })
}

/// A header-plus-question reply with `rcode` (and the TC bit when `truncated`).
pub fn build_reply(query: &[u8], q: &Query, rcode: u8, truncated: bool) -> Vec<u8> {
    let mut v = query[..q.question_end].to_vec();
    let rd = query[2] & 0x01;
    v[2] = 0x80 | rd | if truncated { 0x02 } else { 0 };
    v[3] = 0x80 | (rcode & 0xf); // RA
    v[4..6].copy_from_slice(&1u16.to_be_bytes());
    v[6..12].fill(0);
    v
}

/// A records (address, ttl) in a response's answer section.
#[cfg(test)]
pub fn parse_a_records(buf: &[u8]) -> Vec<(Ipv4Addr, u32)> {
    let mut out = Vec::new();
    let mut parse = || -> Option<()> {
        if buf.len() < 12 || buf[2] & 0x80 == 0 {
            return None;
        }
        let qd = u16::from_be_bytes([buf[4], buf[5]]);
        let an = u16::from_be_bytes([buf[6], buf[7]]);
        let mut off = 12;
        for _ in 0..qd {
            off = read_name(buf, off)?.1 + 4;
        }
        for _ in 0..an {
            let (_, o) = read_name(buf, off)?;
            let h = buf.get(o..o + 10)?;
            let rtype = u16::from_be_bytes([h[0], h[1]]);
            let ttl = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
            let rdlen = usize::from(u16::from_be_bytes([h[8], h[9]]));
            let rd = buf.get(o + 10..o + 10 + rdlen)?;
            if rtype == 1 && rdlen == 4 {
                out.push((Ipv4Addr::new(rd[0], rd[1], rd[2], rd[3]), ttl));
            }
            off = o + 10 + rdlen;
        }
        Some(())
    };
    let _ = parse();
    out
}

/// Addresses (A and AAAA) with their TTLs in a response's answer section.
pub fn parse_addr_records(buf: &[u8]) -> Vec<(IpAddr, u32)> {
    let mut out = Vec::new();
    let mut parse = || -> Option<()> {
        if buf.len() < 12 || buf[2] & 0x80 == 0 {
            return None;
        }
        let qd = u16::from_be_bytes([buf[4], buf[5]]);
        let an = u16::from_be_bytes([buf[6], buf[7]]);
        let mut off = 12;
        for _ in 0..qd {
            off = read_name(buf, off)?.1 + 4;
        }
        for _ in 0..an {
            let (_, o) = read_name(buf, off)?;
            let h = buf.get(o..o + 10)?;
            let rtype = u16::from_be_bytes([h[0], h[1]]);
            let ttl = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
            let rdlen = usize::from(u16::from_be_bytes([h[8], h[9]]));
            let rd = buf.get(o + 10..o + 10 + rdlen)?;
            match (rtype, rdlen) {
                (1, 4) => out.push((Ipv4Addr::new(rd[0], rd[1], rd[2], rd[3]).into(), ttl)),
                (28, 16) => {
                    let a: [u8; 16] = rd.try_into().ok()?;
                    out.push((Ipv6Addr::from(a).into(), ttl));
                }
                _ => {}
            }
            off = o + 10 + rdlen;
        }
        Some(())
    };
    let _ = parse();
    out
}

/// The TC (truncated) bit of a response.
pub fn is_truncated(buf: &[u8]) -> bool {
    buf.len() >= 4 && buf[2] & 0x02 != 0
}

/// The largest UDP response the client accepts: its EDNS0 payload size when the query carries an OPT record,
/// otherwise 512 (RFC 1035).
pub fn udp_limit(query: &[u8]) -> usize {
    let f = || -> Option<usize> {
        if query.len() < 12 {
            return None;
        }
        let qd = u16::from_be_bytes([query[4], query[5]]);
        let an = u16::from_be_bytes([query[6], query[7]]);
        let ns = u16::from_be_bytes([query[8], query[9]]);
        let ar = u16::from_be_bytes([query[10], query[11]]);
        let mut off = 12;
        for _ in 0..qd {
            off = read_name(query, off)?.1 + 4;
        }
        for i in 0..(u32::from(an) + u32::from(ns) + u32::from(ar)) {
            let (_, o) = read_name(query, off)?;
            let h = query.get(o..o + 10)?;
            let rtype = u16::from_be_bytes([h[0], h[1]]);
            if rtype == 41 && i >= u32::from(an) + u32::from(ns) {
                return Some(usize::from(u16::from_be_bytes([h[2], h[3]])).max(512));
            }
            off = o + 10 + usize::from(u16::from_be_bytes([h[8], h[9]]));
        }
        None
    };
    f().unwrap_or(512)
}

/// A header-plus-question reply with TC=1 and no answers, telling the client to retry over TCP.
pub fn truncate_reply(answer: &[u8]) -> Option<Vec<u8>> {
    if answer.len() < 12 || u16::from_be_bytes([answer[4], answer[5]]) == 0 {
        return None;
    }
    let (_, off) = read_name(answer, 12)?;
    answer.get(off..off + 4)?;
    let mut v = answer[..off + 4].to_vec();
    v[2] |= 0x02;
    v[4..6].copy_from_slice(&1u16.to_be_bytes());
    v[6..12].fill(0);
    Some(v)
}

/// Prefix `msg` with its two-byte length (DNS over TCP, RFC 1035 section 4.2.2).
pub fn tcp_frame(msg: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(msg.len() + 2);
    v.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    v.extend_from_slice(msg);
    v
}

/// Pop one complete length-prefixed message from the front of `buf`.
pub fn tcp_take(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() < 2 {
        return None;
    }
    let n = usize::from(u16::from_be_bytes([buf[0], buf[1]]));
    if buf.len() < 2 + n {
        return None;
    }
    let m = buf[2..2 + n].to_vec();
    buf.drain(..2 + n);
    Some(m)
}

#[cfg(test)]
pub(crate) fn test_query(id: u16, name: &str) -> Vec<u8> {
    let mut v = vec![(id >> 8) as u8, id as u8, 0x01, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for l in name.split('.') {
        v.push(l.len() as u8);
        v.extend_from_slice(l.as_bytes());
    }
    v.extend_from_slice(&[0, 0, 1, 0, 1]);
    v
}

#[cfg(test)]
pub(crate) fn test_answer(query: &[u8], ip: Ipv4Addr, ttl: u32) -> Vec<u8> {
    let q = parse_query(query).unwrap();
    let mut v = build_reply(query, &q, 0, false);
    v[6..8].copy_from_slice(&1u16.to_be_bytes());
    v.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1]);
    v.extend_from_slice(&ttl.to_be_bytes());
    v.extend_from_slice(&[0, 4]);
    v.extend_from_slice(&ip.octets());
    v
}

#[cfg(test)]
pub(crate) fn test_query_type(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut v = test_query(id, name);
    let n = v.len();
    v[n - 4..n - 2].copy_from_slice(&qtype.to_be_bytes());
    v
}

/// An answer with `n` A records (to build large responses).
#[cfg(test)]
pub(crate) fn test_big_answer(query: &[u8], n: usize) -> Vec<u8> {
    let q = parse_query(query).unwrap();
    let mut v = build_reply(query, &q, 0, false);
    v[6..8].copy_from_slice(&(n as u16).to_be_bytes());
    for i in 0..n {
        v.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 1, 44, 0, 4, 10, 1, (i >> 8) as u8, i as u8]);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aaaa_edns_and_tcp_framing() {
        let q = test_query_type(7, "v6.test", 28);
        let mut a = build_reply(&q, &parse_query(&q).unwrap(), 0, false);
        a[6..8].copy_from_slice(&1u16.to_be_bytes());
        a.extend_from_slice(&[0xc0, 12, 0, 28, 0, 1, 0, 0, 0, 60, 0, 16]);
        a.extend_from_slice(&"2001:db8::7".parse::<Ipv6Addr>().unwrap().octets());
        assert_eq!(parse_addr_records(&a), vec![("2001:db8::7".parse().unwrap(), 60)]);

        assert_eq!(udp_limit(&q), 512);
        let mut e = q.clone();
        e[10..12].copy_from_slice(&1u16.to_be_bytes());
        e.extend_from_slice(&[0, 0, 41, 0x10, 0, 0, 0, 0, 0, 0, 0]); // root OPT, udp size 4096
        assert_eq!(udp_limit(&e), 4096);

        let big = test_big_answer(&q, 100);
        assert!(big.len() > 512);
        let t = truncate_reply(&big).unwrap();
        assert!(is_truncated(&t));
        assert_eq!(&t[..2], &q[..2]);
        assert_eq!(u16::from_be_bytes([t[6], t[7]]), 0);

        let mut buf = tcp_frame(b"abc");
        buf.extend_from_slice(&tcp_frame(b"defg")[..3]);
        assert_eq!(tcp_take(&mut buf).as_deref(), Some(&b"abc"[..]));
        assert!(tcp_take(&mut buf).is_none());
        buf.extend_from_slice(&tcp_frame(b"defg")[3..]);
        assert_eq!(tcp_take(&mut buf).as_deref(), Some(&b"defg"[..]));
    }

    #[test]
    fn query_and_answer() {
        let q = test_query(0x1234, "www.Example.com");
        let p = parse_query(&q).unwrap();
        assert_eq!(p.name, "www.Example.com");
        let a = test_answer(&q, Ipv4Addr::new(1, 2, 3, 4), 300);
        assert_eq!(parse_a_records(&a), vec![(Ipv4Addr::new(1, 2, 3, 4), 300)]);
        let nx = build_reply(&q, &p, 3, false);
        assert_eq!(nx[3] & 0xf, 3);
        assert!(parse_a_records(&nx).is_empty());
    }

    #[test]
    fn compression_loop_is_rejected() {
        let mut q = test_query(1, "a.b");
        q[12] = 0xc0;
        q[13] = 12;
        assert!(parse_query(&q).is_none());
    }
}
