// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Per-connection stream inspection: feeds the reassembled first bytes of each direction of a TCP connection to
//! the TLS hello parsers and the HTTP/2 frame decoder.
//!
//! A [`FlowStreams`] exists only for connections that looked like TLS or cleartext HTTP/2 at their first bytes
//! (or whose first data segment arrived out of order, so that nothing could be judged yet), and is dropped as
//! soon as both directions have yielded what they can: a ClientHello and a ServerHello for TLS, the first
//! [`ReasmConfig::max_bytes_per_stream`] bytes for HTTP/2.

use crate::h2::{self, H2Dir, H2Message};
use crate::l7::{self, ClientHello, Hs, ServerHello};
use crate::reasm::{DropWhy, Push, ReasmConfig, Stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Unknown,
    Tls,
    H2,
    Done,
}

#[derive(Debug, Clone)]
struct DirStream {
    stream: Stream,
    kind: Kind,
    h2: Option<Box<H2Dir>>,
}

/// Something a stream yielded.
#[derive(Debug, Clone)]
pub enum StreamOut {
    ClientHello(Box<ClientHello>),
    ServerHello(Box<ServerHello>),
    H2(H2Message),
}

/// What feeding one segment did besides producing output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FeedStats {
    /// The segment (or the whole stream) was dropped over a bound.
    pub dropped: Option<DropWhy>,
    /// Overlaps whose bytes disagreed with those already kept.
    pub conflicts: u32,
    /// The direction gave up before a message completed (prefix budget exhausted).
    pub gave_up: bool,
}

/// Both directions of one connection.
#[derive(Debug, Clone)]
pub struct FlowStreams {
    dirs: [DirStream; 2],
    h2_connection: bool,
}

impl Default for FlowStreams {
    fn default() -> Self {
        Self::new()
    }
}

impl FlowStreams {
    pub fn new() -> Self {
        let d = || DirStream { stream: Stream::new(), kind: Kind::Unknown, h2: None };
        Self { dirs: [d(), d()], h2_connection: false }
    }

    /// Heap bytes held (counted against the observer-wide budget).
    pub fn mem(&self) -> usize {
        self.dirs
            .iter()
            .map(|d| d.stream.mem() + d.h2.as_ref().map_or(0, |h| h.mem()))
            .sum()
    }

    /// Both directions have nothing more to give.
    pub fn all_done(&self) -> bool {
        self.dirs.iter().all(|d| d.kind == Kind::Done)
    }

    pub fn is_h2(&self) -> bool {
        self.h2_connection
    }

    fn finish(d: &mut DirStream) {
        d.kind = Kind::Done;
        d.stream.release();
        d.h2 = None;
    }

    /// Offers one TCP segment's payload. `dir` is 0 for client-to-server and 1 for server-to-client; `isn` is the
    /// direction's SYN sequence number when it was seen. Output is appended to `out`. `room` is the remaining
    /// observer-wide byte budget.
    #[allow(clippy::too_many_arguments)]
    pub fn feed(
        &mut self,
        dir: usize,
        seq: u32,
        payload: &[u8],
        isn: Option<u32>,
        cfg: &ReasmConfig,
        room: usize,
        out: &mut Vec<StreamOut>,
    ) -> FeedStats {
        let mut stats = FeedStats::default();
        let d = &mut self.dirs[dir];
        if d.kind == Kind::Done {
            return stats;
        }
        if let Some(i) = isn {
            d.stream.ensure_base(i.wrapping_add(1));
        }
        let before = d.stream.conflicts;
        let limit = cfg.max_bytes_per_stream;
        match d.stream.push(seq, payload, limit, cfg.max_pending_segments, room) {
            Push::Dropped(DropWhy::Budget) => {
                stats.dropped = Some(DropWhy::Budget);
                Self::finish(d);
                return stats;
            }
            Push::Dropped(DropWhy::PendingFull) => stats.dropped = Some(DropWhy::PendingFull),
            Push::Appended(_) | Push::Buffered | Push::Ignored => {}
        }
        stats.conflicts = d.stream.conflicts.saturating_sub(before);
        let h2_connection = self.h2_connection;
        let d = &mut self.dirs[dir];
        if d.stream.contiguous().is_empty() {
            return stats;
        }
        // Decide what this direction carries from its first bytes.
        if d.kind == Kind::Unknown {
            let b = d.stream.contiguous();
            d.kind = if dir == 0 {
                if b[0] == 0x16 {
                    Kind::Tls
                } else if h2::has_preface(b) {
                    Kind::H2
                } else if h2::is_preface_prefix(b) {
                    return stats; // could still become the preface
                } else {
                    Kind::Done
                }
            } else if h2_connection {
                Kind::H2
            } else if b[0] == 0x16 {
                Kind::Tls
            } else {
                Kind::Done
            };
            if d.kind == Kind::H2 && dir == 0 {
                self.h2_connection = true;
            }
            if d.kind == Kind::Done {
                Self::finish(d);
                return stats;
            }
        }
        let d = &mut self.dirs[dir];
        match d.kind {
            Kind::Tls => {
                let done = match l7::handshake_message(d.stream.contiguous()) {
                    Hs::Complete(ty, body) => {
                        if dir == 0 && ty == 1 {
                            if let Some(h) = l7::parse_client_hello_body(&body) {
                                out.push(StreamOut::ClientHello(Box::new(h)));
                            }
                        } else if dir == 1
                            && ty == 2
                            && let Some(h) = l7::parse_server_hello_body(&body)
                        {
                            out.push(StreamOut::ServerHello(Box::new(h)));
                        }
                        true
                    }
                    Hs::Need => {
                        if d.stream.contiguous().len() >= limit {
                            stats.gave_up = true;
                            true
                        } else {
                            false
                        }
                    }
                    Hs::Not => true,
                };
                if done {
                    Self::finish(d);
                }
            }
            Kind::H2 => {
                let h = d.h2.get_or_insert_with(|| Box::new(H2Dir::new(dir == 0)));
                let mut msgs = Vec::new();
                h.advance(d.stream.contiguous(), &mut msgs);
                out.extend(msgs.into_iter().map(StreamOut::H2));
                let dead = h.dead;
                if dead || d.stream.contiguous().len() >= limit {
                    Self::finish(d);
                }
            }
            Kind::Unknown | Kind::Done => {}
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h2::fixtures as h2f;
    use crate::l7::fixtures as tls;

    const ROOM: usize = usize::MAX;

    fn hello() -> Vec<u8> {
        tls::client_hello(
            0x0303,
            &[0x1301, 0x1302, 0xc02f],
            &[
                tls::ext_sni("split.example.com"),
                tls::ext_alpn(&["h2", "http/1.1"]),
                tls::ext_groups(&[29, 23]),
                tls::ext_supported_versions(&[0x0304, 0x0303]),
                tls::ext(0x0015, &vec![0u8; 600]), // padding pushes it past one MSS
            ],
        )
    }

    fn feed_all(fs: &mut FlowStreams, dir: usize, base: u32, chunks: &[(usize, &[u8])]) -> Vec<StreamOut> {
        let mut out = Vec::new();
        for (off, c) in chunks {
            fs.feed(dir, base.wrapping_add(*off as u32), c, Some(base.wrapping_sub(1)), &ReasmConfig::default(), ROOM, &mut out);
        }
        out
    }

    #[test]
    fn client_hello_split_across_three_segments() {
        let h = hello();
        let (a, rest) = h.split_at(100);
        let (b, c) = rest.split_at(300);
        let mut fs = FlowStreams::new();
        let out = feed_all(&mut fs, 0, 1000, &[(0, a), (100, b), (400, c)]);
        assert_eq!(out.len(), 1);
        let StreamOut::ClientHello(ch) = &out[0] else { panic!() };
        assert_eq!(ch.sni.as_deref(), Some("split.example.com"));
        assert_eq!(ch.alpn, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
        assert!(!fs.all_done(), "the server direction is still open");
        assert_eq!(fs.dirs[0].stream.mem(), 0, "released after the hello");
    }

    #[test]
    fn out_of_order_overlapping_and_retransmitted_segments() {
        let h = hello();
        let n = h.len();
        let chunks: Vec<(usize, &[u8])> = vec![
            (300, &h[300..]),     // tail first
            (0, &h[..120]),       // head
            (0, &h[..120]),       // retransmit
            (100, &h[100..320]),  // overlaps head and tail
            (150, &h[150..200]),  // fully inside
        ];
        let mut fs = FlowStreams::new();
        let out = feed_all(&mut fs, 0, 0xffff_ff00, &chunks); // also wraps the sequence space
        assert_eq!(out.len(), 1, "n = {n}");
        let StreamOut::ClientHello(ch) = &out[0] else { panic!() };
        assert_eq!(ch.sni.as_deref(), Some("split.example.com"));
    }

    #[test]
    fn exhaustive_two_way_splits_and_orders() {
        let h = hello();
        for cut in (1..h.len()).step_by(7) {
            for reversed in [false, true] {
                let mut chunks: Vec<(usize, &[u8])> = vec![(0, &h[..cut]), (cut, &h[cut..])];
                if reversed {
                    chunks.reverse();
                }
                let mut fs = FlowStreams::new();
                let out = feed_all(&mut fs, 0, 5000, &chunks);
                assert_eq!(out.len(), 1, "cut {cut} reversed {reversed}");
            }
        }
    }

    #[test]
    fn server_hello_metadata_without_certificates() {
        // TLS 1.3 ServerHello: supported_versions=1.3, key_share x25519.
        let sh = tls::server_hello(0x0303, 0x1301, &[tls::ext(43, &[0x03, 0x04]), tls::ext(51, &[0x00, 0x1d, 0x00, 0x00])]);
        // Followed by an opaque record, as a real server would send.
        let mut wire = sh.clone();
        wire.extend_from_slice(&[0x14, 0x03, 0x03, 0x00, 0x01, 0x01]);
        wire.extend_from_slice(&[0x17, 0x03, 0x03, 0x00, 0x10]);
        wire.extend_from_slice(&[0xaa; 16]);
        let mut fs = FlowStreams::new();
        let (a, b) = wire.split_at(30);
        let out = feed_all(&mut fs, 1, 7000, &[(0, a), (30, b)]);
        assert_eq!(out.len(), 1);
        let StreamOut::ServerHello(s) = &out[0] else { panic!() };
        assert_eq!(s.negotiated_version(), 0x0304);
        assert_eq!(s.cipher, 0x1301);
        assert_eq!(s.key_share_group, Some(29));
        assert!(!s.hello_retry);
    }

    #[test]
    fn non_tls_streams_do_not_hold_memory() {
        let mut fs = FlowStreams::new();
        let mut out = Vec::new();
        fs.feed(0, 1, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n", Some(0), &ReasmConfig::default(), ROOM, &mut out);
        assert!(out.is_empty());
        assert_eq!(fs.dirs[0].kind, Kind::Done);
        assert_eq!(fs.dirs[0].stream.mem(), 0);
    }

    #[test]
    fn budget_exhaustion_drops_and_abandons() {
        let h = hello();
        let mut fs = FlowStreams::new();
        let mut out = Vec::new();
        let s = fs.feed(0, 1, &h[..200], Some(0), &ReasmConfig::default(), 50, &mut out);
        assert_eq!(s.dropped, Some(DropWhy::Budget));
        assert_eq!(fs.dirs[0].kind, Kind::Done);
        assert!(out.is_empty());
    }

    #[test]
    fn pending_overflow_is_counted_and_memory_bounded() {
        let h = hello();
        let cfg = ReasmConfig { max_pending_segments: 2, ..ReasmConfig::default() };
        let mut fs = FlowStreams::new();
        let mut out = Vec::new();
        let mut dropped = 0;
        for i in 1..6usize {
            let s = fs.feed(0, (i * 50) as u32 + 1, &h[i * 50..i * 50 + 40], Some(0), &cfg, ROOM, &mut out);
            if s.dropped == Some(DropWhy::PendingFull) {
                dropped += 1;
            }
        }
        // First segment (offset 0) never arrived; the gap segments are pending-bounded.
        assert_eq!(dropped, 3);
        assert!(fs.mem() <= 2 * 40, "two pending segments of 40 bytes at most, got {}", fs.mem());
    }

    #[test]
    fn hello_larger_than_the_prefix_gives_up() {
        let big = tls::client_hello(0x0303, &[0x1301], &[tls::ext_sni("big.test"), tls::ext(0x0015, &vec![0u8; 3000])]);
        let cfg = ReasmConfig { max_bytes_per_stream: 1024, ..ReasmConfig::default() };
        let mut fs = FlowStreams::new();
        let mut out = Vec::new();
        let mut gave_up = false;
        let mut at = 0;
        while at < big.len() {
            let n = 500.min(big.len() - at);
            let s = fs.feed(0, at as u32 + 1, &big[at..at + n], Some(0), &cfg, ROOM, &mut out);
            gave_up |= s.gave_up;
            at += n;
        }
        assert!(out.is_empty());
        assert!(gave_up);
        assert_eq!(fs.dirs[0].kind, Kind::Done);
    }

    #[test]
    fn h2c_connection_both_directions() {
        let mut c2s = h2f::client_preface();
        c2s.extend(h2f::headers_frame(1, true, &h2f::request_block("GET", "http", "/secret", "mesh.svc:8080")));
        let mut s2c = h2f::settings();
        s2c.extend(h2f::headers_frame(1, true, &h2f::response_block(200, Some("application/grpc"))));
        let mut fs = FlowStreams::new();
        let mut out = Vec::new();
        // Split the client bytes mid-frame and deliver the response between the pieces.
        let (a, b) = c2s.split_at(40);
        fs.feed(0, 1, a, Some(0), &ReasmConfig::default(), ROOM, &mut out);
        assert!(out.is_empty());
        fs.feed(1, 1, &s2c, Some(0), &ReasmConfig::default(), ROOM, &mut out);
        fs.feed(0, 41, b, Some(0), &ReasmConfig::default(), ROOM, &mut out);
        assert!(fs.is_h2());
        let mut msgs: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                StreamOut::H2(m) => Some(m.clone()),
                _ => None,
            })
            .collect();
        msgs.sort_by_key(|m| !m.from_client);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].headers.authority.as_deref(), Some("mesh.svc:8080"));
        assert_eq!(msgs[1].headers.status, Some(200));
        assert_eq!(msgs[1].headers.content_type.as_deref(), Some("application/grpc"));
        assert!(!format!("{out:?}").contains("secret"));
    }

    #[test]
    fn hostile_segments_never_panic_and_stay_bounded() {
        let mut st = 0xabcdu64;
        let mut next = move || {
            st = st.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (st >> 20) as u32
        };
        let seeds: Vec<Vec<u8>> = vec![hello(), h2f::client_preface(), {
            let mut v = h2f::client_preface();
            v.extend(h2f::headers_frame(1, true, &h2f::request_block("GET", "http", "/", "a.test")));
            v
        }];
        let cfg = ReasmConfig { max_bytes_per_stream: 2048, max_pending_segments: 4, ..ReasmConfig::default() };
        for _ in 0..400 {
            let mut fs = FlowStreams::new();
            let mut out = Vec::new();
            let base = &seeds[(next() % 3) as usize];
            for _ in 0..30 {
                let off = (next() as usize) % (base.len() + 50);
                let len = ((next() as usize) % 200).min(base.len().saturating_sub(off).max(1));
                let mut chunk: Vec<u8> = base.get(off..(off + len).min(base.len())).unwrap_or(&[]).to_vec();
                if next() % 3 == 0 && !chunk.is_empty() {
                    let at = next() as usize % chunk.len();
                    chunk[at] = next() as u8;
                }
                let dir = (next() % 2) as usize;
                fs.feed(dir, next() % 4000, &chunk, None, &cfg, 1 << 20, &mut out);
                assert!(fs.mem() < 64 * 1024);
            }
        }
    }
}
