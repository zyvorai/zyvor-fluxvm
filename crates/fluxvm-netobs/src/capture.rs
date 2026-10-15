// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A per-VM packet capture ring. Capture is **off by default**: nothing is retained until a session is enabled
//! for a VM with an explicit filter and a time-to-live, and it stops by itself when the TTL passes. A session is
//! bounded by packet count and total bytes (oldest frames are overwritten) and truncates frames to `snaplen`.
//!
//! Unlike the rest of this crate, a capture keeps packet bytes. That is the point of the feature; it is why it
//! needs an explicit enable, a filter, a TTL, and (at the API) the admin role. The default snaplen of 128 keeps
//! headers and little payload.

use crate::flow::proto_name;
use crate::packet::{Net, Packet};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::net::IpAddr;

pub const DEFAULT_SNAPLEN: usize = 128;
pub const MAX_SNAPLEN: usize = 65_535;
pub const MAX_TTL_US: u64 = 3_600 * 1_000_000;
/// Captured frames stay downloadable this long after the session stops capturing.
pub const RETAIN_AFTER_STOP_US: u64 = 300 * 1_000_000;

/// All set fields must match; `host` matches either endpoint and `port` either port.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureFilter {
    pub host: Option<IpAddr>,
    pub port: Option<u16>,
    /// "tcp", "udp", "icmp", "icmpv6" or "protoN".
    pub proto: Option<String>,
}

impl CaptureFilter {
    pub fn is_empty(&self) -> bool {
        self.host.is_none() && self.port.is_none() && self.proto.is_none()
    }

    pub fn matches(&self, p: &Packet<'_>) -> bool {
        let Net::Ip(ip) = &p.net else { return false };
        if let Some(h) = self.host
            && ip.src != h
            && ip.dst != h
        {
            return false;
        }
        if let Some(port) = self.port {
            let (s, d) = p.ports();
            if s != port && d != port {
                return false;
            }
        }
        if let Some(pr) = &self.proto {
            match crate::flow::parse_proto(pr) {
                Some(n) if n == ip.proto => {}
                _ => return false,
            }
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureRequest {
    pub filter: CaptureFilter,
    pub ttl_secs: u64,
    pub snaplen: usize,
    pub max_packets: usize,
    pub max_bytes: usize,
}

impl Default for CaptureRequest {
    fn default() -> Self {
        Self {
            filter: CaptureFilter::default(),
            ttl_secs: 60,
            snaplen: DEFAULT_SNAPLEN,
            max_packets: 4096,
            max_bytes: 8 << 20,
        }
    }
}

#[derive(Debug)]
pub enum CaptureError {
    /// A capture needs at least one of host/port/proto: whole-NIC capture is refused.
    UnfilteredRefused,
    TooManySessions(usize),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::UnfilteredRefused => {
                f.write_str("capture needs a filter: set at least one of host, port, proto")
            }
            CaptureError::TooManySessions(n) => {
                write!(f, "at most {n} concurrent capture sessions")
            }
        }
    }
}

impl std::error::Error for CaptureError {}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub ts_us: u64,
    /// Length on the wire.
    pub orig_len: u32,
    pub data: Vec<u8>,
}

struct Session {
    filter: CaptureFilter,
    snaplen: usize,
    max_packets: usize,
    max_bytes: usize,
    started_us: u64,
    expires_us: u64,
    ring: VecDeque<CapturedFrame>,
    bytes: usize,
    seen: u64,
    overwritten: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureStatus {
    pub vm: String,
    pub filter: CaptureFilter,
    pub active: bool,
    pub started_us: u64,
    pub expires_us: u64,
    pub snaplen: usize,
    pub packets: usize,
    pub bytes: usize,
    pub matched: u64,
    pub overwritten: u64,
}

pub struct CaptureManager {
    sessions: HashMap<String, Session>,
    max_sessions: usize,
}

impl Default for CaptureManager {
    fn default() -> Self {
        Self::new(8)
    }
}

impl CaptureManager {
    pub fn new(max_sessions: usize) -> Self {
        Self {
            sessions: HashMap::new(),
            max_sessions,
        }
    }

    pub fn has_active(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Starts (or replaces) the capture for `vm`.
    pub fn enable(
        &mut self,
        vm: &str,
        req: CaptureRequest,
        now_us: u64,
    ) -> Result<(), CaptureError> {
        if req.filter.is_empty() {
            return Err(CaptureError::UnfilteredRefused);
        }
        self.prune(now_us);
        if !self.sessions.contains_key(vm) && self.sessions.len() >= self.max_sessions {
            return Err(CaptureError::TooManySessions(self.max_sessions));
        }
        let ttl = (req.ttl_secs.max(1) * 1_000_000).min(MAX_TTL_US);
        self.sessions.insert(
            vm.to_string(),
            Session {
                filter: req.filter,
                snaplen: req.snaplen.clamp(14, MAX_SNAPLEN),
                max_packets: req.max_packets.clamp(1, 65_536),
                max_bytes: req.max_bytes.clamp(1024, 64 << 20),
                started_us: now_us,
                expires_us: now_us.saturating_add(ttl),
                ring: VecDeque::new(),
                bytes: 0,
                seen: 0,
                overwritten: 0,
            },
        );
        Ok(())
    }

    /// Ends capture for `vm` and discards what it held. Returns whether a session existed.
    pub fn disable(&mut self, vm: &str) -> bool {
        self.sessions.remove(vm).is_some()
    }

    fn prune(&mut self, now_us: u64) {
        self.sessions
            .retain(|_, s| now_us < s.expires_us.saturating_add(RETAIN_AFTER_STOP_US));
    }

    /// Offers a parsed frame. Cheap when no session exists for `vm`.
    pub fn offer(&mut self, vm: &str, now_us: u64, frame: &[u8], pkt: &Packet<'_>) {
        let Some(s) = self.sessions.get_mut(vm) else {
            return;
        };
        if now_us >= s.expires_us || !s.filter.matches(pkt) {
            return;
        }
        s.seen += 1;
        let keep = frame.len().min(s.snaplen);
        s.bytes += keep;
        s.ring.push_back(CapturedFrame {
            ts_us: now_us,
            orig_len: frame.len() as u32,
            data: frame[..keep].to_vec(),
        });
        while s.ring.len() > s.max_packets || s.bytes > s.max_bytes {
            match s.ring.pop_front() {
                Some(f) => {
                    s.bytes -= f.data.len();
                    s.overwritten += 1;
                }
                None => break,
            }
        }
    }

    pub fn status(&mut self, now_us: u64) -> Vec<CaptureStatus> {
        self.prune(now_us);
        let mut v: Vec<CaptureStatus> = self
            .sessions
            .iter()
            .map(|(vm, s)| CaptureStatus {
                vm: vm.clone(),
                filter: s.filter.clone(),
                active: now_us < s.expires_us,
                started_us: s.started_us,
                expires_us: s.expires_us,
                snaplen: s.snaplen,
                packets: s.ring.len(),
                bytes: s.bytes,
                matched: s.seen,
                overwritten: s.overwritten,
            })
            .collect();
        v.sort_by(|a, b| a.vm.cmp(&b.vm));
        v
    }

    /// The captured frames for `vm`, oldest first, with the snaplen they were cut to.
    pub fn frames(&mut self, vm: &str, now_us: u64) -> Option<(usize, Vec<CapturedFrame>)> {
        self.prune(now_us);
        let s = self.sessions.get(vm)?;
        Some((s.snaplen, s.ring.iter().cloned().collect()))
    }

    /// The pcap file for `vm`'s ring.
    pub fn pcap(&mut self, vm: &str, now_us: u64) -> Option<Vec<u8>> {
        let (snaplen, frames) = self.frames(vm, now_us)?;
        let mut out = Vec::with_capacity(24 + frames.len() * 64);
        write_pcap(&mut out, snaplen as u32, &frames).ok()?;
        Some(out)
    }

    pub fn filter_description(f: &CaptureFilter) -> String {
        let mut parts = Vec::new();
        if let Some(h) = f.host {
            parts.push(format!("host {h}"));
        }
        if let Some(p) = f.port {
            parts.push(format!("port {p}"));
        }
        if let Some(p) = &f.proto {
            parts.push(
                crate::flow::parse_proto(p)
                    .map(proto_name)
                    .unwrap_or_else(|| p.clone()),
            );
        }
        parts.join(" and ")
    }
}

/// Writes a classic little-endian, microsecond-resolution pcap file with LINKTYPE_ETHERNET.
pub fn write_pcap<W: Write>(w: &mut W, snaplen: u32, frames: &[CapturedFrame]) -> io::Result<()> {
    w.write_all(&0xa1b2_c3d4u32.to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?;
    w.write_all(&4u16.to_le_bytes())?;
    w.write_all(&0i32.to_le_bytes())?; // thiszone
    w.write_all(&0u32.to_le_bytes())?; // sigfigs
    w.write_all(&snaplen.to_le_bytes())?;
    w.write_all(&1u32.to_le_bytes())?; // LINKTYPE_ETHERNET
    for f in frames {
        w.write_all(&((f.ts_us / 1_000_000) as u32).to_le_bytes())?;
        w.write_all(&((f.ts_us % 1_000_000) as u32).to_le_bytes())?;
        w.write_all(&(f.data.len() as u32).to_le_bytes())?;
        w.write_all(&f.orig_len.to_le_bytes())?;
        w.write_all(&f.data)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (ts_sec, ts_usec, orig_len, bytes)
    type PcapRecord = (u32, u32, u32, Vec<u8>);
    use crate::packet::{self, TCP_ACK, build};
    use std::net::Ipv4Addr;

    const A: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const B: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 2);
    const C: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 3);

    /// A minimal in-test pcap reader: independent of the writer's code path.
    fn read_pcap(b: &[u8]) -> (u32, u32, Vec<PcapRecord>) {
        assert_eq!(&b[0..4], &[0xd4, 0xc3, 0xb2, 0xa1], "little-endian magic");
        let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 2);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 4);
        let snaplen = u32at(16);
        let linktype = u32at(20);
        let mut o = 24;
        let mut recs = Vec::new();
        while o < b.len() {
            let (s, us, incl, orig) = (u32at(o), u32at(o + 4), u32at(o + 8), u32at(o + 12));
            let data = b[o + 16..o + 16 + incl as usize].to_vec();
            recs.push((s, us, orig, data));
            o += 16 + incl as usize;
        }
        assert_eq!(o, b.len(), "no trailing bytes");
        (snaplen, linktype, recs)
    }

    fn req(filter: CaptureFilter) -> CaptureRequest {
        CaptureRequest {
            filter,
            ..CaptureRequest::default()
        }
    }

    #[test]
    fn off_by_default_and_unfiltered_is_refused() {
        let mut m = CaptureManager::default();
        let f = build::udp_v4((A, 1), (B, 2), b"x");
        let p = packet::parse(&f).unwrap();
        m.offer("vm", 1, &f, &p);
        assert!(m.pcap("vm", 1).is_none());
        assert!(matches!(
            m.enable("vm", req(CaptureFilter::default()), 0),
            Err(CaptureError::UnfilteredRefused)
        ));
    }

    #[test]
    fn filter_host_port_proto() {
        let mut m = CaptureManager::default();
        let filter = CaptureFilter {
            host: Some(A.into()),
            port: Some(53),
            proto: Some("udp".into()),
        };
        m.enable("vm", req(filter), 0).unwrap();
        let yes = build::udp_v4((A, 4000), (B, 53), b"q");
        let wrong_port = build::udp_v4((A, 4000), (B, 80), b"q");
        let wrong_host = build::udp_v4((C, 4000), (B, 53), b"q");
        let wrong_proto = build::tcp_v4((A, 4000), (B, 53), 0, 0, TCP_ACK, 1, b"");
        for f in [&yes, &wrong_port, &wrong_host, &wrong_proto] {
            let p = packet::parse(f).unwrap();
            m.offer("vm", 10, f, &p);
        }
        let (_, frames) = m.frames("vm", 10).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, yes);
    }

    #[test]
    fn pcap_round_trip_with_snaplen() {
        let mut m = CaptureManager::default();
        m.enable(
            "vm",
            CaptureRequest {
                filter: CaptureFilter {
                    port: Some(9),
                    ..Default::default()
                },
                snaplen: 40,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        let big = build::udp_v4((A, 9), (B, 9), &[7u8; 200]);
        let small = build::udp_v4((B, 9), (A, 9), b"hey");
        for (t, f) in [(1_500_000u64, &big), (2_000_007, &small)] {
            let p = packet::parse(f).unwrap();
            m.offer("vm", t, f, &p);
        }
        let file = m.pcap("vm", 3_000_000).unwrap();
        let (snaplen, link, recs) = read_pcap(&file);
        assert_eq!((snaplen, link), (40, 1));
        assert_eq!(recs.len(), 2);
        assert_eq!((recs[0].0, recs[0].1), (1, 500_000));
        assert_eq!(recs[0].2 as usize, big.len(), "original length preserved");
        assert_eq!(recs[0].3, big[..40]);
        assert_eq!((recs[1].0, recs[1].1), (2, 7));
        assert_eq!(recs[1].3, small[..40]);
    }

    #[test]
    fn ring_is_bounded_and_counts_overwrites() {
        let mut m = CaptureManager::default();
        m.enable(
            "vm",
            CaptureRequest {
                filter: CaptureFilter {
                    port: Some(9),
                    ..Default::default()
                },
                max_packets: 3,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        for i in 0..10u8 {
            let f = build::udp_v4((A, 9), (B, 9), &[i]);
            let p = packet::parse(&f).unwrap();
            m.offer("vm", 1 + i as u64, &f, &p);
        }
        let st = &m.status(100)[0];
        assert_eq!((st.packets, st.matched, st.overwritten), (3, 10, 7));
        let (_, frames) = m.frames("vm", 100).unwrap();
        assert_eq!(
            *frames[0].data.last().unwrap(),
            7,
            "oldest retained is the 8th frame"
        );
    }

    #[test]
    fn ttl_stops_capture_then_data_expires() {
        let mut m = CaptureManager::default();
        m.enable(
            "vm",
            CaptureRequest {
                filter: CaptureFilter {
                    port: Some(9),
                    ..Default::default()
                },
                ttl_secs: 1,
                ..Default::default()
            },
            0,
        )
        .unwrap();
        let f = build::udp_v4((A, 9), (B, 9), b"a");
        let p = packet::parse(&f).unwrap();
        m.offer("vm", 500_000, &f, &p);
        m.offer("vm", 1_000_000, &f, &p); // at expiry: not captured
        assert_eq!(m.status(1_000_001)[0].packets, 1);
        assert!(!m.status(1_000_001)[0].active);
        assert!(
            m.pcap("vm", 1_000_001 + RETAIN_AFTER_STOP_US).is_none(),
            "gone after the retention window"
        );
    }

    #[test]
    fn session_count_is_bounded() {
        let mut m = CaptureManager::new(1);
        let flt = CaptureFilter {
            port: Some(1),
            ..Default::default()
        };
        m.enable("a", req(flt.clone()), 0).unwrap();
        assert!(matches!(
            m.enable("b", req(flt.clone()), 0),
            Err(CaptureError::TooManySessions(1))
        ));
        m.enable("a", req(flt), 0).unwrap(); // replacing is fine
    }
}
