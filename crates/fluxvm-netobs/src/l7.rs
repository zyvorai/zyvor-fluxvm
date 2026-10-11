// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Application-layer metadata extraction. Everything here is bounds-checked, allocation-light and reads only the
//! first segment of a message; nothing is reassembled, decrypted or exported as payload.
//!
//! * DNS over UDP and TCP/53: query name, type, response code, answer addresses.
//! * TLS ClientHello: SNI, ALPN, JA3 and JA4 fingerprints (single-segment hellos only).
//! * HTTP/1.x: request method, `Host`, version, and response status. The request target (path and query) is never
//!   read into a record.
//! * QUIC: Initial packet detection (version, connection-id lengths). Initial packets are encrypted; their
//!   contents (and so SNI) are not decoded.
//!
//! [`Redaction`] is applied by the observer before any name leaves this layer's output.

use md5::{Digest as _, Md5};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------------------------------------------

/// What to do with one kind of name before it is stored or exported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Redact {
    /// Record the name as seen (lower-cased).
    #[default]
    Keep,
    /// Keep only the last two labels (`a.b.example.com` becomes `example.com`). A heuristic: it does not consult
    /// the public-suffix list.
    Registrable,
    /// Replace with a salted SHA-256 prefix: stable for correlation, not reversible by reading it.
    Hash,
    /// Do not record the name at all.
    Drop,
}

/// Which L7 fields may be recorded and in what form. Applied before storage, so a dropped name is never held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Redaction {
    pub dns_qname: Redact,
    pub tls_sni: Redact,
    pub http_host: Redact,
    /// Keep addresses from DNS answers.
    pub dns_answers: bool,
    /// Keep JA3/JA4 fingerprints.
    pub fingerprints: bool,
    /// Salt for [`Redact::Hash`]. Set a per-host secret in production; the default is public.
    pub salt: String,
}

impl Default for Redaction {
    fn default() -> Self {
        Self {
            dns_qname: Redact::Keep,
            tls_sni: Redact::Keep,
            http_host: Redact::Keep,
            dns_answers: true,
            fingerprints: true,
            salt: "fluxvm-netobs".to_string(),
        }
    }
}

impl Redaction {
    /// A profile that records no names at all.
    pub fn strict() -> Self {
        Self {
            dns_qname: Redact::Drop,
            tls_sni: Redact::Drop,
            http_host: Redact::Drop,
            dns_answers: false,
            fingerprints: false,
            ..Self::default()
        }
    }

    pub fn apply(&self, mode: Redact, name: &str) -> Option<String> {
        match mode {
            Redact::Keep => Some(name.to_string()),
            Redact::Drop => None,
            Redact::Registrable => {
                let labels: Vec<&str> = name.trim_end_matches('.').split('.').collect();
                let n = labels.len();
                Some(if n <= 2 { labels.join(".") } else { labels[n - 2..].join(".") })
            }
            Redact::Hash => {
                let mut h = Sha256::new();
                h.update(self.salt.as_bytes());
                h.update([0]);
                h.update(name.as_bytes());
                Some(format!("h:{}", &hex(&h.finalize())[..16]))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// DNS
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsMessage {
    pub id: u16,
    pub response: bool,
    pub opcode: u8,
    pub rcode: u8,
    pub truncated: bool,
    pub qname: String,
    pub qtype: u16,
    pub qclass: u16,
    pub answer_count: u16,
    /// A/AAAA answer addresses (at most 8).
    pub addrs: Vec<IpAddr>,
}

/// Reads a (possibly compressed) name at `at`. Returns the name and the offset just past it in the *original*
/// position. Pointer chains are bounded.
fn dns_name(b: &[u8], at: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut pos = at;
    let mut end = None;
    let mut hops = 0;
    loop {
        let len = *b.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xc0 == 0xc0 {
            let lo = *b.get(pos + 1)? as usize;
            if end.is_none() {
                end = Some(pos + 2);
            }
            hops += 1;
            if hops > 16 {
                return None;
            }
            pos = ((len & 0x3f) << 8) | lo;
            continue;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        let label = b.get(pos + 1..pos + 1 + len)?;
        if !out.is_empty() {
            out.push('.');
        }
        if out.len() + len > 253 {
            return None;
        }
        for &c in label {
            out.push(if (0x21..0x7f).contains(&c) { (c as char).to_ascii_lowercase() } else { '?' });
        }
        pos += 1 + len;
    }
    Some((out, end.unwrap_or(pos)))
}

/// Parses a DNS message (without the TCP length prefix).
pub fn parse_dns(b: &[u8]) -> Option<DnsMessage> {
    if b.len() < 12 {
        return None;
    }
    let flags = be16(b, 2)?;
    let qd = be16(b, 4)?;
    let an = be16(b, 6)?;
    let opcode = ((flags >> 11) & 0x0f) as u8;
    if qd != 1 || opcode > 5 {
        return None;
    }
    let (qname, mut pos) = dns_name(b, 12)?;
    let qtype = be16(b, pos)?;
    let qclass = be16(b, pos + 2)?;
    pos += 4;
    let response = flags & 0x8000 != 0;
    let mut addrs = Vec::new();
    if response {
        for _ in 0..an.min(32) {
            let Some((_, p)) = dns_name(b, pos) else { break };
            let (Some(ty), Some(rdlen)) = (be16(b, p), be16(b, p + 8)) else { break };
            let rd = p + 10;
            let rdlen = rdlen as usize;
            let Some(data) = b.get(rd..rd + rdlen) else { break };
            if addrs.len() < 8 {
                match (ty, rdlen) {
                    (1, 4) => addrs.push(IpAddr::V4(Ipv4Addr::new(data[0], data[1], data[2], data[3]))),
                    (28, 16) => {
                        let mut o = [0u8; 16];
                        o.copy_from_slice(data);
                        addrs.push(IpAddr::V6(Ipv6Addr::from(o)));
                    }
                    _ => {}
                }
            }
            pos = rd + rdlen;
        }
    }
    Some(DnsMessage {
        id: be16(b, 0)?,
        response,
        opcode,
        rcode: (flags & 0x0f) as u8,
        truncated: flags & 0x0200 != 0,
        qname,
        qtype,
        qclass,
        answer_count: an,
        addrs,
    })
}

/// DNS over TCP: a two-byte length then the message. Only a complete message in this segment is decoded.
pub fn parse_dns_tcp(b: &[u8]) -> Option<DnsMessage> {
    let len = be16(b, 0)? as usize;
    parse_dns(b.get(2..2 + len)?)
}

pub fn dns_type_name(t: u16) -> String {
    match t {
        1 => "A".into(),
        2 => "NS".into(),
        5 => "CNAME".into(),
        6 => "SOA".into(),
        12 => "PTR".into(),
        15 => "MX".into(),
        16 => "TXT".into(),
        28 => "AAAA".into(),
        33 => "SRV".into(),
        64 => "SVCB".into(),
        65 => "HTTPS".into(),
        255 => "ANY".into(),
        n => format!("TYPE{n}"),
    }
}

pub fn dns_rcode_name(r: u8) -> String {
    match r {
        0 => "NOERROR".into(),
        1 => "FORMERR".into(),
        2 => "SERVFAIL".into(),
        3 => "NXDOMAIN".into(),
        4 => "NOTIMP".into(),
        5 => "REFUSED".into(),
        n => format!("RCODE{n}"),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// TLS ClientHello, JA3, JA4
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientHello {
    pub legacy_version: u16,
    pub ciphers: Vec<u16>,
    /// Extension types in wire order.
    pub extensions: Vec<u16>,
    pub sni: Option<String>,
    pub alpn: Vec<Vec<u8>>,
    pub groups: Vec<u16>,
    pub point_formats: Vec<u8>,
    pub sig_algs: Vec<u16>,
    pub supported_versions: Vec<u16>,
}

fn is_grease(v: u16) -> bool {
    v & 0x0f0f == 0x0a0a && (v >> 8) == (v & 0xff)
}

/// Parses a TLS ClientHello from the first bytes of a TCP stream. Returns `None` unless the whole handshake
/// message is present in `b` (a hello split across segments or records is not reassembled).
pub fn parse_client_hello(b: &[u8]) -> Option<ClientHello> {
    if *b.first()? != 0x16 || *b.get(1)? != 3 {
        return None;
    }
    let rec_len = be16(b, 3)? as usize;
    let rec = b.get(5..5 + rec_len.min(b.len().saturating_sub(5)))?;
    if *rec.first()? != 1 {
        return None;
    }
    let hs_len = ((*rec.get(1)? as usize) << 16) | ((*rec.get(2)? as usize) << 8) | *rec.get(3)? as usize;
    let body = rec.get(4..4 + hs_len)?;
    let mut hello = ClientHello { legacy_version: be16(body, 0)?, ..Default::default() };
    let mut p = 2 + 32;
    let sid = *body.get(p)? as usize;
    p += 1 + sid;
    let cs = be16(body, p)? as usize;
    p += 2;
    if cs % 2 != 0 {
        return None;
    }
    for c in body.get(p..p + cs)?.chunks_exact(2) {
        hello.ciphers.push(u16::from_be_bytes([c[0], c[1]]));
    }
    p += cs;
    let comp = *body.get(p)? as usize;
    p += 1 + comp;
    if p == body.len() {
        return Some(hello);
    }
    let ext_len = be16(body, p)? as usize;
    p += 2;
    let exts = body.get(p..p + ext_len)?;
    let mut e = 0;
    while e + 4 <= exts.len() {
        let ty = be16(exts, e)?;
        let len = be16(exts, e + 2)? as usize;
        let data = exts.get(e + 4..e + 4 + len)?;
        e += 4 + len;
        hello.extensions.push(ty);
        match ty {
            0 => {
                // server_name_list: u16 list length, then (type u8, len u16, name).
                if data.len() >= 5 && data[2] == 0 {
                    let n = be16(data, 3)? as usize;
                    if let Some(name) = data.get(5..5 + n) {
                        let s: String = name
                            .iter()
                            .map(|&c| if (0x21..0x7f).contains(&c) { (c as char).to_ascii_lowercase() } else { '?' })
                            .collect();
                        hello.sni = Some(s);
                    }
                }
            }
            10 => {
                let n = be16(data, 0)? as usize;
                for c in data.get(2..2 + n)?.chunks_exact(2) {
                    hello.groups.push(u16::from_be_bytes([c[0], c[1]]));
                }
            }
            11 => {
                let n = *data.first()? as usize;
                hello.point_formats = data.get(1..1 + n)?.to_vec();
            }
            13 => {
                let n = be16(data, 0)? as usize;
                for c in data.get(2..2 + n)?.chunks_exact(2) {
                    hello.sig_algs.push(u16::from_be_bytes([c[0], c[1]]));
                }
            }
            16 => {
                let n = be16(data, 0)? as usize;
                let list = data.get(2..2 + n)?;
                let mut i = 0;
                while i < list.len() {
                    let l = list[i] as usize;
                    let Some(v) = list.get(i + 1..i + 1 + l) else { break };
                    hello.alpn.push(v.to_vec());
                    i += 1 + l;
                }
            }
            43 => {
                let n = *data.first()? as usize;
                for c in data.get(1..1 + n)?.chunks_exact(2) {
                    hello.supported_versions.push(u16::from_be_bytes([c[0], c[1]]));
                }
            }
            _ => {}
        }
    }
    Some(hello)
}

fn join_dec<I: IntoIterator<Item = u32>>(it: I) -> String {
    it.into_iter().map(|v| v.to_string()).collect::<Vec<_>>().join("-")
}

impl ClientHello {
    /// The JA3 input string: `version,ciphers,extensions,curves,point-formats`, GREASE removed.
    pub fn ja3_string(&self) -> String {
        format!(
            "{},{},{},{},{}",
            self.legacy_version,
            join_dec(self.ciphers.iter().copied().filter(|c| !is_grease(*c)).map(u32::from)),
            join_dec(self.extensions.iter().copied().filter(|c| !is_grease(*c)).map(u32::from)),
            join_dec(self.groups.iter().copied().filter(|c| !is_grease(*c)).map(u32::from)),
            join_dec(self.point_formats.iter().map(|v| u32::from(*v))),
        )
    }

    /// MD5 of [`Self::ja3_string`] (the JA3 hash).
    pub fn ja3(&self) -> String {
        hex(&Md5::digest(self.ja3_string().as_bytes()))
    }

    /// The JA4 TLS-over-TCP fingerprint (`t` prefix), `a_b_c` form.
    pub fn ja4(&self) -> String {
        let version = {
            let best = self
                .supported_versions
                .iter()
                .copied()
                .filter(|v| !is_grease(*v))
                .max()
                .unwrap_or(self.legacy_version);
            match best {
                0x0304 => "13",
                0x0303 => "12",
                0x0302 => "11",
                0x0301 => "10",
                0x0300 => "s3",
                0x0002 => "s2",
                _ => "00",
            }
        };
        let ciphers: Vec<u16> = self.ciphers.iter().copied().filter(|c| !is_grease(*c)).collect();
        let exts: Vec<u16> = self.extensions.iter().copied().filter(|c| !is_grease(*c)).collect();
        let alpn = match self.alpn.first() {
            Some(v) if !v.is_empty() => {
                let (f, l) = (v[0], v[v.len() - 1]);
                if f.is_ascii_alphanumeric() && l.is_ascii_alphanumeric() {
                    format!("{}{}", f as char, l as char)
                } else {
                    let h = hex(v);
                    format!("{}{}", &h[..1], &h[h.len() - 1..])
                }
            }
            _ => "00".to_string(),
        };
        let a = format!(
            "t{}{}{:02}{:02}{}",
            version,
            if self.sni.is_some() { 'd' } else { 'i' },
            ciphers.len().min(99),
            exts.len().min(99),
            alpn
        );
        let hash12 = |s: &str| -> String {
            if s.is_empty() {
                "000000000000".to_string()
            } else {
                hex(&Sha256::digest(s.as_bytes()))[..12].to_string()
            }
        };
        let mut sorted = ciphers;
        sorted.sort_unstable();
        let b = hash12(&sorted.iter().map(|c| format!("{c:04x}")).collect::<Vec<_>>().join(","));
        let mut ext_sorted: Vec<u16> = exts.into_iter().filter(|e| *e != 0x0000 && *e != 0x0010).collect();
        ext_sorted.sort_unstable();
        let mut c_in = ext_sorted.iter().map(|c| format!("{c:04x}")).collect::<Vec<_>>().join(",");
        if !self.sig_algs.is_empty() {
            c_in.push('_');
            c_in.push_str(&self.sig_algs.iter().map(|c| format!("{c:04x}")).collect::<Vec<_>>().join(","));
        }
        let c = hash12(if ext_sorted.is_empty() && self.sig_algs.is_empty() { "" } else { &c_in });
        format!("{a}_{b}_{c}")
    }
}

// ---------------------------------------------------------------------------------------------------------------
// HTTP/1
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Http {
    Request { method: &'static str, host: Option<String>, version: String },
    Response { status: u16, version: String },
}

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "HEAD", "OPTIONS", "PATCH", "CONNECT", "TRACE"];

/// Recognises an HTTP/1.x request or response head at the start of `b`. The request target is deliberately not
/// returned.
pub fn parse_http(b: &[u8]) -> Option<Http> {
    let head_end = b.windows(2).take(4096).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&b[..head_end]).ok()?;
    if let Some(rest) = line.strip_prefix("HTTP/1.") {
        let mut it = rest.splitn(3, ' ');
        let minor = it.next()?;
        let status: u16 = it.next()?.parse().ok()?;
        if !(100..600).contains(&status) || minor.len() != 1 {
            return None;
        }
        return Some(Http::Response { status, version: format!("1.{minor}") });
    }
    let mut it = line.split(' ');
    let method = METHODS.iter().copied().find(|m| *m == it.next().unwrap_or(""))?;
    let target = it.next()?;
    let ver = it.next()?;
    let version = ver.strip_prefix("HTTP/1.")?;
    if version.len() != 1 || it.next().is_some() || target.is_empty() {
        return None;
    }
    let mut host = None;
    // Headers: scan line by line within the segment, bounded.
    let rest = &b[head_end + 2..];
    for raw in rest.split(|c| *c == b'\n').take(64) {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.is_empty() {
            break;
        }
        if raw.len() > 5 && raw[..5].eq_ignore_ascii_case(b"host:") {
            if let Ok(v) = std::str::from_utf8(&raw[5..]) {
                let v = v.trim();
                if !v.is_empty() && v.len() <= 255 {
                    host = Some(v.to_ascii_lowercase());
                }
            }
            break;
        }
    }
    if host.is_none() && method == "CONNECT" {
        host = Some(target.to_ascii_lowercase());
    }
    Some(Http::Request { method, host, version: format!("1.{version}") })
}

// ---------------------------------------------------------------------------------------------------------------
// QUIC
// ---------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuicInitial {
    pub version: u32,
    pub dcid_len: u8,
    pub scid_len: u8,
}

/// Detects a QUIC client Initial in a UDP payload: long header, a known version, an Initial packet type, sane
/// connection-id lengths, and the 1200-byte minimum datagram size the protocol requires of client Initials.
pub fn detect_quic_initial(b: &[u8]) -> Option<QuicInitial> {
    if b.len() < 1200 {
        return None;
    }
    let first = b[0];
    if first & 0xc0 != 0xc0 {
        return None;
    }
    let version = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
    let ptype = (first >> 4) & 0x03;
    let initial = match version {
        0x0000_0001 => ptype == 0,
        0x6b33_43cf => ptype == 1, // QUIC v2
        v if v & 0xffff_ff00 == 0xff00_0000 => ptype == 0, // IETF drafts
        _ => false,
    };
    if !initial {
        return None;
    }
    let dcid = *b.get(5)?;
    let scid = *b.get(6 + dcid as usize)?;
    if dcid > 20 || scid > 20 {
        return None;
    }
    Some(QuicInitial { version, dcid_len: dcid, scid_len: scid })
}

/// Builders for wire fixtures. Public so other crates' tests (and tools) can synthesise traffic.
pub mod fixtures {
    fn name(n: &str) -> Vec<u8> {
        let mut o = Vec::new();
        for l in n.split('.').filter(|l| !l.is_empty()) {
            o.push(l.len() as u8);
            o.extend_from_slice(l.as_bytes());
        }
        o.push(0);
        o
    }

    pub fn dns_query(id: u16, qname: &str, qtype: u16) -> Vec<u8> {
        let mut m = vec![(id >> 8) as u8, id as u8, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        m.extend(name(qname));
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        m
    }

    /// A response with `rcode` and one A record per address (using a compression pointer to the question).
    pub fn dns_response(id: u16, qname: &str, qtype: u16, rcode: u8, a: &[[u8; 4]]) -> Vec<u8> {
        let mut m = vec![(id >> 8) as u8, id as u8, 0x81, 0x80 | (rcode & 0x0f)];
        m.extend_from_slice(&[0, 1]);
        m.extend_from_slice(&(a.len() as u16).to_be_bytes());
        m.extend_from_slice(&[0, 0, 0, 0]);
        m.extend(name(qname));
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        for ip in a {
            m.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
            m.extend_from_slice(ip);
        }
        m
    }

    pub fn ext(ty: u16, body: &[u8]) -> (u16, Vec<u8>) {
        (ty, body.to_vec())
    }

    pub fn ext_sni(host: &str) -> (u16, Vec<u8>) {
        let mut b = Vec::new();
        b.extend_from_slice(&((host.len() + 3) as u16).to_be_bytes());
        b.push(0);
        b.extend_from_slice(&(host.len() as u16).to_be_bytes());
        b.extend_from_slice(host.as_bytes());
        (0, b)
    }

    fn u16_list(ty: u16, v: &[u16]) -> (u16, Vec<u8>) {
        let mut b = ((v.len() * 2) as u16).to_be_bytes().to_vec();
        for x in v {
            b.extend_from_slice(&x.to_be_bytes());
        }
        (ty, b)
    }

    pub fn ext_groups(v: &[u16]) -> (u16, Vec<u8>) {
        u16_list(10, v)
    }
    pub fn ext_sig_algs(v: &[u16]) -> (u16, Vec<u8>) {
        u16_list(13, v)
    }
    pub fn ext_point_formats(v: &[u8]) -> (u16, Vec<u8>) {
        let mut b = vec![v.len() as u8];
        b.extend_from_slice(v);
        (11, b)
    }
    pub fn ext_alpn(protos: &[&str]) -> (u16, Vec<u8>) {
        let mut list = Vec::new();
        for p in protos {
            list.push(p.len() as u8);
            list.extend_from_slice(p.as_bytes());
        }
        let mut b = (list.len() as u16).to_be_bytes().to_vec();
        b.extend(list);
        (16, b)
    }
    pub fn ext_supported_versions(v: &[u16]) -> (u16, Vec<u8>) {
        let mut b = vec![(v.len() * 2) as u8];
        for x in v {
            b.extend_from_slice(&x.to_be_bytes());
        }
        (43, b)
    }

    /// A TLS record carrying one ClientHello with the given fields, extensions in the order given.
    pub fn client_hello(legacy_version: u16, ciphers: &[u16], exts: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&legacy_version.to_be_bytes());
        body.extend_from_slice(&[0x42; 32]);
        body.push(0); // empty session id
        body.extend_from_slice(&((ciphers.len() * 2) as u16).to_be_bytes());
        for c in ciphers {
            body.extend_from_slice(&c.to_be_bytes());
        }
        body.extend_from_slice(&[1, 0]); // null compression
        let mut e = Vec::new();
        for (ty, d) in exts {
            e.extend_from_slice(&ty.to_be_bytes());
            e.extend_from_slice(&(d.len() as u16).to_be_bytes());
            e.extend_from_slice(d);
        }
        body.extend_from_slice(&(e.len() as u16).to_be_bytes());
        body.extend(e);
        let mut hs = vec![1, (body.len() >> 16) as u8, (body.len() >> 8) as u8, body.len() as u8];
        hs.extend(body);
        let mut rec = vec![0x16, 0x03, 0x01];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend(hs);
        rec
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn dns_query_and_response_roundtrip() {
        let q = parse_dns(&dns_query(0xbeef, "WWW.Example.COM", 28)).unwrap();
        assert!(!q.response);
        assert_eq!(q.id, 0xbeef);
        assert_eq!(q.qname, "www.example.com");
        assert_eq!(dns_type_name(q.qtype), "AAAA");

        let r = parse_dns(&dns_response(0xbeef, "www.example.com", 1, 0, &[[93, 184, 216, 34], [1, 2, 3, 4]]))
            .unwrap();
        assert!(r.response);
        assert_eq!(r.rcode, 0);
        assert_eq!(r.addrs, vec![IpAddr::from([93, 184, 216, 34]), IpAddr::from([1, 2, 3, 4])]);

        let nx = parse_dns(&dns_response(1, "nope.example", 1, 3, &[])).unwrap();
        assert_eq!(dns_rcode_name(nx.rcode), "NXDOMAIN");
    }

    #[test]
    fn dns_over_tcp_needs_full_message() {
        let m = dns_query(7, "a.b", 1);
        let mut t = (m.len() as u16).to_be_bytes().to_vec();
        t.extend(&m);
        assert!(parse_dns_tcp(&t).is_some());
        assert!(parse_dns_tcp(&t[..t.len() - 1]).is_none());
    }

    #[test]
    fn dns_compression_loop_is_rejected() {
        let mut m = vec![0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        m.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1]); // pointer to itself
        assert!(parse_dns(&m).is_none());
    }

    #[test]
    fn dns_garbage_never_panics() {
        let base = dns_response(9, "x.y.z", 1, 0, &[[1, 1, 1, 1]]);
        for n in 0..base.len() {
            let _ = parse_dns(&base[..n]);
        }
        let mut s = 12345u64;
        for _ in 0..5000 {
            let mut m = base.clone();
            for _ in 0..4 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let i = (s >> 33) as usize % m.len();
                m[i] = (s >> 20) as u8;
            }
            let _ = parse_dns(&m);
        }
    }

    // The example from the JA3 project's README: this fingerprint string and its MD5. The hash is quoted from
    // memory of that README; the string-to-MD5 step is also checked against a vector that does not depend on it.
    #[test]
    fn ja3_readme_example() {
        let hello = client_hello(
            769,
            &[47, 53, 5, 10, 49161, 49162, 49171, 49172, 50, 56, 19, 4],
            &[
                ext_sni("example.com"),
                ext_groups(&[23, 24, 25]),
                ext_point_formats(&[0]),
            ],
        );
        let ch = parse_client_hello(&hello).unwrap();
        assert_eq!(ch.ja3_string(), "769,47-53-5-10-49161-49162-49171-49172-50-56-19-4,0-10-11,23-24-25,0");
        assert_eq!(ch.ja3(), "ada70206e40642a3e4461f35503241d5");
        assert_eq!(ch.sni.as_deref(), Some("example.com"));
    }

    #[test]
    fn md5_known_vector() {
        assert_eq!(hex(&Md5::digest(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&Md5::digest(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn grease_is_ignored() {
        let hello = client_hello(
            0x0303,
            &[0x0a0a, 0x1301, 0xdada],
            &[ext(0x2a2a, &[0]), ext_sni("a.test"), ext_groups(&[0x4a4a, 29])],
        );
        let ch = parse_client_hello(&hello).unwrap();
        assert_eq!(ch.ja3_string(), "771,4865,0-10,29,");
        assert!(ch.ja4().starts_with("t12d0102"));
    }

    // JA4 cipher section for Chrome's TLS 1.3 cipher list (15 suites; GREASE excluded). The expected value
    // 8daaf6152771 is the widely published Chrome cipher hash as recalled by the author; treat as an
    // unverified-by-source vector. The structure of section a is verified exactly.
    #[test]
    fn ja4_chrome_cipher_section() {
        let ciphers = [
            0x1a1a, 0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc013, 0xc014,
            0x009c, 0x009d, 0x002f, 0x0035,
        ];
        let hello = client_hello(
            0x0303,
            &ciphers,
            &[
                ext(0x0a0a, &[]),
                ext_sni("www.example.com"),
                ext(0x0017, &[]),
                ext(0xff01, &[0]),
                ext_groups(&[0x2a2a, 0x001d, 0x0017, 0x0018]),
                ext_point_formats(&[0]),
                ext(0x0023, &[]),
                ext_alpn(&["h2", "http/1.1"]),
                ext_sig_algs(&[0x0403, 0x0804, 0x0401]),
                ext_supported_versions(&[0x2a2a, 0x0304, 0x0303]),
            ],
        );
        let ch = parse_client_hello(&hello).unwrap();
        let ja4 = ch.ja4();
        let parts: Vec<&str> = ja4.split('_').collect();
        assert_eq!(parts[0], "t13d1509h2");
        assert_eq!(parts[1], "8daaf6152771");
        assert_eq!(parts[2].len(), 12);
    }

    #[test]
    fn ja4_without_sni_or_alpn() {
        let hello = client_hello(0x0303, &[0x002f], &[ext_groups(&[23])]);
        let ja4 = parse_client_hello(&hello).unwrap().ja4();
        assert!(ja4.starts_with("t12i010100_"), "{ja4}");
    }

    #[test]
    fn partial_hello_is_none() {
        let hello = client_hello(0x0303, &[0x1301], &[ext_sni("x.test")]);
        for n in 0..hello.len() {
            assert!(parse_client_hello(&hello[..n]).is_none(), "len {n}");
        }
        assert!(parse_client_hello(&hello).is_some());
    }

    #[test]
    fn tls_garbage_never_panics() {
        let base = client_hello(0x0303, &[0x1301, 0x1302], &[ext_sni("x.test"), ext_alpn(&["h2"]), ext_groups(&[29])]);
        let mut s = 99u64;
        for _ in 0..5000 {
            let mut m = base.clone();
            for _ in 0..3 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let i = (s >> 33) as usize % m.len();
                m[i] = (s >> 20) as u8;
            }
            let _ = parse_client_hello(&m);
        }
    }

    #[test]
    fn http_request_without_path() {
        let r = parse_http(b"GET /secret/token?a=b HTTP/1.1\r\nUser-Agent: x\r\nHost: Api.Example.com:8080\r\n\r\n")
            .unwrap();
        assert_eq!(
            r,
            Http::Request { method: "GET", host: Some("api.example.com:8080".into()), version: "1.1".into() }
        );
        // The path must not appear anywhere in the record.
        assert!(!format!("{r:?}").contains("secret"));
    }

    #[test]
    fn http_response_and_non_http() {
        assert_eq!(
            parse_http(b"HTTP/1.0 503 Service Unavailable\r\nX: y\r\n\r\n"),
            Some(Http::Response { status: 503, version: "1.0".into() })
        );
        assert!(parse_http(b"SSH-2.0-OpenSSH_9.6\r\n").is_none());
        assert!(parse_http(b"GET / HTTP/2.0\r\n").is_none());
        assert!(parse_http(b"\x16\x03\x01\x02\x00\x01").is_none());
    }

    #[test]
    fn quic_initial_detection() {
        let mut p = vec![0u8; 1250];
        p[0] = 0xc3;
        p[4] = 1; // version 1
        p[5] = 8; // dcid len
        p[6 + 8] = 0; // scid len
        assert_eq!(detect_quic_initial(&p), Some(QuicInitial { version: 1, dcid_len: 8, scid_len: 0 }));
        p[0] = 0xe3; // type 2 = Handshake in v1
        assert!(detect_quic_initial(&p).is_none());
        assert!(detect_quic_initial(&p[..600]).is_none());
        p[0] = 0x43;
        assert!(detect_quic_initial(&p).is_none());
    }

    #[test]
    fn redaction_modes() {
        let r = Redaction::default();
        assert_eq!(r.apply(Redact::Keep, "a.b.example.com").as_deref(), Some("a.b.example.com"));
        assert_eq!(r.apply(Redact::Registrable, "a.b.example.com").as_deref(), Some("example.com"));
        assert_eq!(r.apply(Redact::Registrable, "localhost").as_deref(), Some("localhost"));
        assert_eq!(r.apply(Redact::Drop, "x"), None);
        let h1 = r.apply(Redact::Hash, "secret.example").unwrap();
        let h2 = r.apply(Redact::Hash, "secret.example").unwrap();
        assert_eq!(h1, h2);
        assert!(h1.starts_with("h:") && !h1.contains("secret"));
        let salted = Redaction { salt: "other".into(), ..Redaction::default() };
        assert_ne!(salted.apply(Redact::Hash, "secret.example").unwrap(), h1);
    }
}
