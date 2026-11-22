// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! A small mDNS / DNS-SD announcer and browser, just enough for a machine to find the fleet it should ask to join
//! (`_fluxvm-pair._tcp.local`). No new dependency: the repo had no mDNS crate, and this needs one service type.
//!
//! The wire codec ([`build_query`], [`build_response`], [`parse_response`]) is pure and unit-tested, and the network is
//! behind the [`Browser`] trait so pairing is tested with [`FakeBrowser`]. The UDP implementations ([`UdpBrowser`],
//! [`Announcer`]) have been exercised only on loopback in tests; behaviour on a real LAN, with a router that filters
//! multicast, a firewall, or another responder holding port 5353, is not verified (docs/cluster-pairing.md).
//!
//! The browser sends a *legacy unicast* query (from an ephemeral port, RFC 6762 section 6.7), so it never has to bind
//! 5353 and the answer comes straight back to it.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const PAIR_SERVICE: &str = "_fluxvm-pair._tcp.local";
pub const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
pub const MDNS_PORT: u16 = 5353;

const T_A: u16 = 1;
const T_PTR: u16 = 12;
const T_TXT: u16 = 16;
const T_SRV: u16 = 33;
const CLASS_IN: u16 = 1;
const FLUSH: u16 = 0x8000;

/// What a responder announces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Human label, for example the fleet's name. One DNS label (at most 63 bytes, no dots).
    pub instance: String,
    /// `host.local`
    pub host: String,
    pub port: u16,
    pub addrs: Vec<Ipv4Addr>,
    pub txt: BTreeMap<String, String>,
}

/// What a browser found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Found {
    pub instance: String,
    pub host: String,
    pub port: u16,
    pub addrs: Vec<Ipv4Addr>,
    pub txt: BTreeMap<String, String>,
}

/// Looks for services on the network. Implemented by [`UdpBrowser`] and, in tests, [`FakeBrowser`].
pub trait Browser {
    fn browse(&self, service: &str, wait: Duration) -> Vec<Found>;
}

/// A browser that returns what it was given.
pub struct FakeBrowser(pub Vec<Found>);
impl Browser for FakeBrowser {
    fn browse(&self, _service: &str, _wait: Duration) -> Vec<Found> {
        self.0.clone()
    }
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() {
            continue;
        }
        let b = label.as_bytes();
        out.push(b.len().min(63) as u8);
        out.extend_from_slice(&b[..b.len().min(63)]);
    }
    out.push(0);
}

fn header(out: &mut Vec<u8>, id: u16, flags: u16, qd: u16, an: u16) {
    for v in [id, flags, qd, an, 0, 0] {
        out.extend_from_slice(&v.to_be_bytes());
    }
}

fn record(out: &mut Vec<u8>, name: &str, typ: u16, class: u16, ttl: u32, data: &[u8]) {
    put_name(out, name);
    out.extend_from_slice(&typ.to_be_bytes());
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

/// A PTR query for `service` (for example [`PAIR_SERVICE`]).
pub fn build_query(service: &str) -> Vec<u8> {
    let mut out = Vec::new();
    header(&mut out, 0, 0, 1, 0);
    put_name(&mut out, service);
    out.extend_from_slice(&T_PTR.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out
}

/// The answer to a PTR query: PTR, SRV, TXT and one A record per address.
pub fn build_response(svc: &Service, service: &str, ttl: u32, id: u16) -> Vec<u8> {
    let full = format!("{}.{service}", svc.instance);
    let mut out = Vec::new();
    header(&mut out, id, 0x8400, 0, 3 + svc.addrs.len() as u16);
    let mut ptr = Vec::new();
    put_name(&mut ptr, &full);
    record(&mut out, service, T_PTR, CLASS_IN, ttl, &ptr);
    let mut srv = vec![0, 0, 0, 0];
    srv.extend_from_slice(&svc.port.to_be_bytes());
    put_name(&mut srv, &svc.host);
    record(&mut out, &full, T_SRV, CLASS_IN | FLUSH, ttl, &srv);
    let mut txt = Vec::new();
    for (k, v) in &svc.txt {
        let kv = format!("{k}={v}");
        let b = kv.as_bytes();
        txt.push(b.len().min(255) as u8);
        txt.extend_from_slice(&b[..b.len().min(255)]);
    }
    if txt.is_empty() {
        txt.push(0);
    }
    record(&mut out, &full, T_TXT, CLASS_IN | FLUSH, ttl, &txt);
    for a in &svc.addrs {
        record(&mut out, &svc.host, T_A, CLASS_IN | FLUSH, ttl, &a.octets());
    }
    out
}

struct Reader<'a> {
    b: &'a [u8],
}

impl Reader<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        Some(u16::from_be_bytes(self.b.get(at..at + 2)?.try_into().ok()?))
    }

    /// The name at `at`, following compression pointers, and the offset after it.
    fn name(&self, at: usize) -> Option<(String, usize)> {
        let (mut pos, mut end, mut hops) = (at, None, 0);
        let mut labels: Vec<String> = Vec::new();
        loop {
            let len = *self.b.get(pos)? as usize;
            if len == 0 {
                return Some((labels.join("."), end.unwrap_or(pos + 1)));
            }
            if len & 0xC0 == 0xC0 {
                let ptr = ((len & 0x3F) << 8) | *self.b.get(pos + 1)? as usize;
                end.get_or_insert(pos + 2);
                hops += 1;
                if hops > 16 || ptr >= self.b.len() {
                    return None;
                }
                pos = ptr;
                continue;
            }
            if len > 63 {
                return None;
            }
            labels.push(String::from_utf8_lossy(self.b.get(pos + 1..pos + 1 + len)?).into_owned());
            pos += 1 + len;
        }
    }
}

struct Rr {
    name: String,
    typ: u16,
    data: usize,
    len: usize,
}

fn records(b: &[u8]) -> Option<(Reader<'_>, u16, Vec<Rr>)> {
    let r = Reader { b };
    let flags = r.u16(2)?;
    let (qd, an) = (
        r.u16(4)?,
        r.u16(6)? as usize + r.u16(8)? as usize + r.u16(10)? as usize,
    );
    let mut pos = 12;
    for _ in 0..qd {
        pos = r.name(pos)?.1 + 4;
    }
    let mut v = Vec::new();
    for _ in 0..an {
        let (name, p) = r.name(pos)?;
        let typ = r.u16(p)?;
        let len = r.u16(p + 8)? as usize;
        v.push(Rr {
            name,
            typ,
            data: p + 10,
            len,
        });
        pos = p + 10 + len;
        if pos > b.len() {
            return None;
        }
    }
    Some((r, flags, v))
}

/// Everything announced for `service` in a response packet. Garbage and truncated packets give an empty list.
pub fn parse_response(b: &[u8], service: &str) -> Vec<Found> {
    let Some((r, flags, rrs)) = records(b) else {
        return vec![];
    };
    if flags & 0x8000 == 0 {
        return vec![];
    }
    let mut out = Vec::new();
    for p in rrs
        .iter()
        .filter(|x| x.typ == T_PTR && x.name.eq_ignore_ascii_case(service))
    {
        let Some((inst_full, _)) = r.name(p.data) else {
            continue;
        };
        let suffix = format!(".{service}");
        let Some(instance) = inst_full.strip_suffix(&suffix) else {
            continue;
        };
        let mut f = Found {
            instance: instance.to_owned(),
            ..Default::default()
        };
        for s in rrs
            .iter()
            .filter(|x| x.name.eq_ignore_ascii_case(&inst_full))
        {
            if s.typ == T_SRV && s.len >= 7 {
                f.port = r.u16(s.data + 4).unwrap_or(0);
                f.host = r.name(s.data + 6).map(|n| n.0).unwrap_or_default();
            } else if s.typ == T_TXT {
                let (mut i, end) = (s.data, s.data + s.len);
                while i < end {
                    let l = b[i] as usize;
                    if l == 0 || i + 1 + l > end {
                        break;
                    }
                    let kv = String::from_utf8_lossy(&b[i + 1..i + 1 + l]).into_owned();
                    if let Some((k, v)) = kv.split_once('=') {
                        f.txt.insert(k.to_owned(), v.to_owned());
                    }
                    i += 1 + l;
                }
            }
        }
        for a in rrs
            .iter()
            .filter(|x| x.typ == T_A && x.len == 4 && x.name.eq_ignore_ascii_case(&f.host))
        {
            f.addrs.push(Ipv4Addr::new(
                b[a.data],
                b[a.data + 1],
                b[a.data + 2],
                b[a.data + 3],
            ));
        }
        if f.port != 0 {
            out.push(f);
        }
    }
    out
}

/// Whether `b` is a query asking for `service`; the query id is returned so the answer can echo it.
pub fn query_for(b: &[u8], service: &str) -> Option<u16> {
    let r = Reader { b };
    if r.u16(2)? & 0x8000 != 0 || r.u16(4)? == 0 {
        return None;
    }
    let mut pos = 12;
    for _ in 0..r.u16(4)? {
        let (name, p) = r.name(pos)?;
        let typ = r.u16(p)?;
        if name.eq_ignore_ascii_case(service) && (typ == T_PTR || typ == 255) {
            return r.u16(0);
        }
        pos = p + 4;
    }
    None
}

/// The address other machines would use to reach this one: the source address the OS would pick for a route off the
/// machine. Sends nothing.
pub fn local_ipv4() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_unspecified() && !a.ip().is_loopback() => Some(*a.ip()),
        _ => None,
    }
}

/// Asks the network over multicast, from an ephemeral port, and collects answers for `wait`.
pub struct UdpBrowser {
    /// Where to send the query; the multicast group unless a test points it at loopback.
    pub target: SocketAddr,
}

impl Default for UdpBrowser {
    fn default() -> Self {
        UdpBrowser {
            target: SocketAddr::V4(SocketAddrV4::new(MDNS_GROUP, MDNS_PORT)),
        }
    }
}

impl Browser for UdpBrowser {
    fn browse(&self, service: &str, wait: Duration) -> Vec<Found> {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else {
            return vec![];
        };
        let _ = sock.set_multicast_ttl_v4(255);
        let q = build_query(service);
        let deadline = Instant::now() + wait;
        let mut found: Vec<Found> = Vec::new();
        let mut last_send: Option<Instant> = None;
        let mut buf = [0u8; 1500];
        while Instant::now() < deadline {
            // Ask again every second: multicast is lossy.
            if last_send.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                let _ = sock.send_to(&q, self.target);
                last_send = Some(Instant::now());
            }
            let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
            if let Ok((n, _)) = sock.recv_from(&mut buf) {
                for f in parse_response(&buf[..n], service) {
                    if !found.contains(&f) {
                        found.push(f);
                    }
                }
            }
        }
        found
    }
}

/// Answers queries for one service until dropped. Binds 5353 with address reuse so it coexists with the system's own
/// responder where the OS allows it.
pub struct Announcer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Announcer {
    pub fn start(svc: Service, service: &'static str) -> std::io::Result<Announcer> {
        let sock = Self::bind(MDNS_PORT)?;
        let _ = sock.join_multicast_v4(&MDNS_GROUP, &Ipv4Addr::UNSPECIFIED);
        Ok(Self::run(sock, svc, service))
    }

    /// For tests: answer on an ordinary socket.
    pub fn start_on(sock: UdpSocket, svc: Service, service: &'static str) -> Announcer {
        Self::run(sock, svc, service)
    }

    fn bind(port: u16) -> std::io::Result<UdpSocket> {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_reuse_address(true)?;
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "freebsd"))]
        s.set_reuse_port(true)?;
        s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
        Ok(s.into())
    }

    fn run(sock: UdpSocket, svc: Service, service: &'static str) -> Announcer {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let _ = sock.set_read_timeout(Some(Duration::from_millis(250)));
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            while !flag.load(Ordering::Relaxed) {
                let Ok((n, from)) = sock.recv_from(&mut buf) else {
                    continue;
                };
                let Some(id) = query_for(&buf[..n], service) else {
                    continue;
                };
                // A legacy unicast query (not from 5353) gets its id echoed and a direct answer.
                let legacy = from.port() != MDNS_PORT;
                let resp = build_response(&svc, service, 120, if legacy { id } else { 0 });
                let _ = sock.send_to(&resp, from);
            }
        });
        Announcer {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Announcer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> Service {
        Service {
            instance: "Home Lab".into(),
            host: "mini-1.local".into(),
            port: 7799,
            addrs: vec![Ipv4Addr::new(192, 168, 1, 20), Ipv4Addr::new(10, 0, 0, 5)],
            txt: [
                ("v".to_owned(), "1".to_owned()),
                ("scheme".to_owned(), "http".to_owned()),
            ]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn a_response_round_trips() {
        let b = build_response(&svc(), PAIR_SERVICE, 120, 0);
        let f = parse_response(&b, PAIR_SERVICE);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].instance, "Home Lab");
        assert_eq!(f[0].host, "mini-1.local");
        assert_eq!(f[0].port, 7799);
        assert_eq!(f[0].addrs, svc().addrs);
        assert_eq!(f[0].txt, svc().txt);
    }

    #[test]
    fn a_query_is_recognised_and_a_response_is_not_a_query() {
        let q = build_query(PAIR_SERVICE);
        assert_eq!(query_for(&q, PAIR_SERVICE), Some(0));
        assert_eq!(query_for(&q, "_other._tcp.local"), None);
        assert_eq!(
            query_for(&build_response(&svc(), PAIR_SERVICE, 1, 0), PAIR_SERVICE),
            None
        );
        assert!(parse_response(&q, PAIR_SERVICE).is_empty());
    }

    #[test]
    fn garbage_and_truncated_packets_are_ignored_not_fatal() {
        let good = build_response(&svc(), PAIR_SERVICE, 120, 0);
        for n in 0..good.len() {
            let _ = parse_response(&good[..n], PAIR_SERVICE);
            let _ = query_for(&good[..n], PAIR_SERVICE);
        }
        assert!(parse_response(&[0xff; 64], PAIR_SERVICE).is_empty());
        // A compression pointer loop must terminate.
        let mut evil = good.clone();
        evil[12] = 0xC0;
        evil[13] = 12;
        assert!(parse_response(&evil, PAIR_SERVICE).is_empty());
    }

    #[test]
    fn another_service_is_not_reported() {
        let b = build_response(&svc(), "_other._tcp.local", 120, 0);
        assert!(parse_response(&b, PAIR_SERVICE).is_empty());
    }

    #[test]
    fn the_fake_browser_returns_what_it_holds() {
        let f = Found {
            instance: "x".into(),
            port: 1,
            ..Default::default()
        };
        assert_eq!(
            FakeBrowser(vec![f.clone()]).browse(PAIR_SERVICE, Duration::ZERO),
            vec![f]
        );
    }

    #[test]
    fn a_browser_finds_an_announcer_over_loopback() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        let _a = Announcer::start_on(sock, svc(), PAIR_SERVICE);
        let b = UdpBrowser { target: addr };
        let found = b.browse(PAIR_SERVICE, Duration::from_millis(700));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].port, 7799);
        assert_eq!(found[0].txt.get("v").map(String::as_str), Some("1"));
    }
}
