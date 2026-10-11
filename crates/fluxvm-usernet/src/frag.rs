// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! IPv4 fragment reassembly for datagrams the guest sends. Bounded in datagram count and size, with a timeout.
//! Overlapping fragments make the whole datagram be discarded (RFC 5722 / RFC 1858 style hardening); an exact
//! duplicate of a held fragment is ignored.

use crate::wire::{self, Ipv4};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Datagrams being reassembled at once.
pub const MAX_DATAGRAMS: usize = 32;
/// Largest reassembled payload (IPv4 total length is 16 bits, minus a 20-byte header).
pub const MAX_PAYLOAD: usize = 65535 - 20;
pub const TIMEOUT: Duration = Duration::from_secs(15);

type Key = (Ipv4Addr, Ipv4Addr, u8, u16);

struct Entry {
    /// (offset, bytes), unordered, pairwise disjoint.
    frags: Vec<(usize, Vec<u8>)>,
    /// Known once the fragment without MF arrives.
    total: Option<usize>,
    got: usize,
    first: Instant,
}

#[derive(Default)]
pub struct Reassembler {
    map: HashMap<Key, Entry>,
}

impl Reassembler {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Add one fragment. Returns the reassembled, header-rebuilt IPv4 packet once complete.
    pub fn add(&mut self, ip: &Ipv4<'_>, now: Instant) -> Option<Vec<u8>> {
        let key: Key = (ip.src, ip.dst, ip.proto, ip.id);
        let off = ip.frag_off;
        let data = ip.payload;
        if data.is_empty()
            || (ip.mf && !data.len().is_multiple_of(8))
            || off + data.len() > MAX_PAYLOAD
        {
            self.map.remove(&key);
            return None;
        }
        if !self.map.contains_key(&key) && self.map.len() >= MAX_DATAGRAMS {
            return None;
        }
        let e = self.map.entry(key).or_insert_with(|| Entry {
            frags: Vec::new(),
            total: None,
            got: 0,
            first: now,
        });
        let end = off + data.len();
        let mut bad = false;
        for (o, d) in &e.frags {
            if *o == off && d.len() == data.len() {
                return None; // retransmitted fragment
            }
            if *o < end && off < *o + d.len() {
                bad = true;
                break;
            }
        }
        if !ip.mf {
            if e.total.is_some_and(|t| t != end) || e.frags.iter().any(|(o, d)| o + d.len() > end) {
                bad = true;
            }
            e.total = Some(end);
        } else if e.total.is_some_and(|t| end > t) {
            bad = true;
        }
        if bad {
            self.map.remove(&key);
            return None;
        }
        e.frags.push((off, data.to_vec()));
        e.got += data.len();
        if e.total == Some(e.got) {
            let mut e = self.map.remove(&key)?;
            e.frags.sort_by_key(|(o, _)| *o);
            let mut body = Vec::with_capacity(e.got);
            for (_, d) in e.frags {
                body.extend_from_slice(&d);
            }
            return Some(wire::build_ipv4_with(
                ip.src, ip.dst, ip.proto, &body, ip.id, false,
            ));
        }
        None
    }

    /// Drop datagrams older than [`TIMEOUT`]; returns how many.
    pub fn expire(&mut self, now: Instant) -> usize {
        let n = self.map.len();
        self.map
            .retain(|_, e| now.duration_since(e.first) <= TIMEOUT);
        n - self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frags(id: u16, len: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
        let body: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
        let ip = wire::build_ipv4_with(
            Ipv4Addr::new(10, 0, 2, 15),
            Ipv4Addr::new(1, 1, 1, 1),
            17,
            &body,
            id,
            false,
        );
        (body, wire::fragment_ipv4(&ip, 600).unwrap())
    }

    #[test]
    fn reassembles_out_of_order() {
        let (body, mut fr) = frags(5, 2000);
        assert!(fr.len() >= 4);
        fr.reverse();
        let mut r = Reassembler::default();
        let now = Instant::now();
        let mut done = None;
        for f in &fr {
            let p = wire::parse_ipv4(f).unwrap();
            if let Some(d) = r.add(&p, now) {
                done = Some(d);
            }
        }
        let d = done.expect("complete");
        let p = wire::parse_ipv4(&d).unwrap();
        assert_eq!((p.payload, p.fragment, p.id), (&body[..], false, 5));
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn bounded_and_times_out() {
        let mut r = Reassembler::default();
        let now = Instant::now();
        for id in 0..(MAX_DATAGRAMS as u16 + 10) {
            let (_, fr) = frags(id, 2000);
            let p = wire::parse_ipv4(&fr[0]).unwrap();
            assert!(r.add(&p, now).is_none());
        }
        assert_eq!(r.len(), MAX_DATAGRAMS);
        assert_eq!(
            r.expire(now + TIMEOUT + Duration::from_secs(1)),
            MAX_DATAGRAMS
        );
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn overlap_discards_the_datagram() {
        let (_, fr) = frags(9, 2000);
        let mut r = Reassembler::default();
        let now = Instant::now();
        let p0 = wire::parse_ipv4(&fr[0]).unwrap();
        assert!(r.add(&p0, now).is_none());
        // forge a fragment overlapping the first: same id, offset 8
        let mut forged = wire::build_ipv4_with(p0.src, p0.dst, p0.proto, &[1u8; 64], 9, false);
        forged[6..8].copy_from_slice(&(0x2000u16 | 1).to_be_bytes());
        let c = {
            forged[10] = 0;
            forged[11] = 0;
            wire::checksum(&forged[..20])
        };
        forged[10..12].copy_from_slice(&c.to_be_bytes());
        let pf = wire::parse_ipv4(&forged).unwrap();
        assert!(r.add(&pf, now).is_none());
        assert_eq!(r.len(), 0);
    }
}
