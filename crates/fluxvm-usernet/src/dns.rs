// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Just enough DNS wire handling for a policy-checking forwarder.

use std::net::Ipv4Addr;

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
mod tests {
    use super::*;

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
