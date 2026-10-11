// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Application-layer metadata extraction. Everything here is bounds-checked and allocation-light, and works on
//! byte slices; reassembling a stream into such a slice is the job of [`crate::reasm`] / [`crate::streams`], and
//! QUIC and HTTP/2 have their own modules ([`crate::quic`], [`crate::h2`]). Payload is never exported.
//!
//! * DNS over UDP and TCP/53: query name, type, response code (including the EDNS-extended bits), answer
//!   addresses, CNAME targets, answer record types, HTTPS service bindings and the EDNS(0) header.
//! * TLS ClientHello: SNI, ALPN, JA3 and JA4 fingerprints; handshake messages split over several TLS records are
//!   stitched ([`handshake_message`]).
//! * TLS ServerHello: negotiated version and cipher, JA3S and JA4S. Certificates are never read.
//! * HTTP/1.x: request method, `Host`, version, and response status. The request target (path and query) is never
//!   read into a record.
//! * QUIC: Initial packet detection (version, connection-id lengths); the ClientHello inside it is decoded by
//!   [`crate::quic`].
//! * Encrypted-DNS recognition helpers (DoH host matching, see [`sni_is_doh`]).
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
    /// CNAME targets from the answer section, in order (at most 4).
    pub cnames: Vec<String>,
    /// Distinct record types present in the answer section, in order of first appearance (at most 8).
    pub answer_types: Vec<u16>,
    /// HTTPS (type 65) service bindings (at most 4).
    pub https: Vec<HttpsRr>,
    /// The EDNS(0) OPT pseudo-record, if any. Its options (client subnet, cookies, padding) are not read.
    pub edns: Option<Edns>,
}

impl DnsMessage {
    /// The rcode including the EDNS extended bits: `(edns.ext_rcode << 4) | rcode`.
    pub fn full_rcode(&self) -> u16 {
        let ext = self.edns.as_ref().map_or(0, |e| e.ext_rcode as u16);
        (ext << 4) | self.rcode as u16
    }
}

/// EDNS(0) header fields from the OPT record (RFC 6891).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edns {
    pub udp_size: u16,
    pub version: u8,
    pub ext_rcode: u8,
    /// DNSSEC OK.
    pub dnssec_ok: bool,
}

/// An HTTPS resource record (RFC 9460) reduced to what is useful for observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpsRr {
    pub priority: u16,
    /// The target name; `"."` means the owner name itself.
    pub target: String,
    pub alpn: Vec<String>,
    pub port: Option<u16>,
    /// An Encrypted ClientHello config is advertised.
    pub ech: bool,
}

/// One resource record header: `(type, class, ttl, rdata start, rdata length, offset after the record)`.
fn dns_rr(b: &[u8], at: usize) -> Option<(u16, u16, u32, usize, usize, usize)> {
    let (_, p) = dns_name(b, at)?;
    let ty = be16(b, p)?;
    let class = be16(b, p + 2)?;
    let ttl = u32::from_be_bytes([*b.get(p + 4)?, *b.get(p + 5)?, *b.get(p + 6)?, *b.get(p + 7)?]);
    let rdlen = be16(b, p + 8)? as usize;
    let rd = p + 10;
    b.get(rd..rd + rdlen)?;
    Some((ty, class, ttl, rd, rdlen, rd + rdlen))
}

fn parse_https_rr(b: &[u8], rd: usize, rdlen: usize) -> Option<HttpsRr> {
    let data = b.get(rd..rd + rdlen)?;
    let priority = be16(data, 0)?;
    let (target, mut p) = dns_name(b, rd + 2)?;
    let end = rd + rdlen;
    let mut rr = HttpsRr {
        priority,
        target: if target.is_empty() { ".".to_string() } else { target },
        alpn: Vec::new(),
        port: None,
        ech: false,
    };
    for _ in 0..16 {
        if p + 4 > end {
            break;
        }
        let key = be16(b, p)?;
        let len = be16(b, p + 2)? as usize;
        if p + 4 + len > end {
            break;
        }
        let v = &b[p + 4..p + 4 + len];
        match key {
            1 => {
                let mut i = 0;
                while i < v.len() && rr.alpn.len() < 8 {
                    let l = v[i] as usize;
                    let Some(id) = v.get(i + 1..i + 1 + l) else { break };
                    rr.alpn.push(String::from_utf8_lossy(id).chars().filter(|c| !c.is_control()).take(32).collect());
                    i += 1 + l;
                }
            }
            3 if len == 2 => rr.port = Some(u16::from_be_bytes([v[0], v[1]])),
            5 => rr.ech = true,
            _ => {}
        }
        p += 4 + len;
    }
    Some(rr)
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
    let ns = be16(b, 8)?;
    let ar = be16(b, 10)?;
    let opcode = ((flags >> 11) & 0x0f) as u8;
    if qd != 1 || opcode > 5 {
        return None;
    }
    let (qname, mut pos) = dns_name(b, 12)?;
    let qtype = be16(b, pos)?;
    let qclass = be16(b, pos + 2)?;
    pos += 4;
    let response = flags & 0x8000 != 0;
    let mut msg = DnsMessage {
        id: be16(b, 0)?,
        response,
        opcode,
        rcode: (flags & 0x0f) as u8,
        truncated: flags & 0x0200 != 0,
        qname,
        qtype,
        qclass,
        answer_count: an,
        addrs: Vec::new(),
        cnames: Vec::new(),
        answer_types: Vec::new(),
        https: Vec::new(),
        edns: None,
    };
    // Answers, then authority (skipped), then additional (only the OPT record is read). Each section is bounded
    // and a malformed record ends parsing with whatever was decoded so far.
    'sections: for (section, count, cap) in [(0u8, an, 32u16), (1, ns, 16), (2, ar, 16)] {
        for _ in 0..count.min(cap) {
            let Some((ty, class, ttl, rd, rdlen, next)) = dns_rr(b, pos) else { break 'sections };
            pos = next;
            let data = &b[rd..rd + rdlen];
            match section {
                0 => {
                    if msg.answer_types.len() < 8 && !msg.answer_types.contains(&ty) {
                        msg.answer_types.push(ty);
                    }
                    match (ty, rdlen) {
                        (1, 4) if msg.addrs.len() < 8 => {
                            msg.addrs.push(IpAddr::V4(Ipv4Addr::new(data[0], data[1], data[2], data[3])));
                        }
                        (28, 16) if msg.addrs.len() < 8 => {
                            let mut o = [0u8; 16];
                            o.copy_from_slice(data);
                            msg.addrs.push(IpAddr::V6(Ipv6Addr::from(o)));
                        }
                        (5, _) if msg.cnames.len() < 4 => {
                            if let Some((n, _)) = dns_name(b, rd) {
                                msg.cnames.push(n);
                            }
                        }
                        (65, _) if msg.https.len() < 4 => {
                            if let Some(h) = parse_https_rr(b, rd, rdlen) {
                                msg.https.push(h);
                            }
                        }
                        _ => {}
                    }
                }
                2 if ty == 41 && msg.edns.is_none() => {
                    msg.edns = Some(Edns {
                        udp_size: class,
                        ext_rcode: (ttl >> 24) as u8,
                        version: (ttl >> 16) as u8,
                        dnssec_ok: ttl & 0x8000 != 0,
                    });
                }
                _ => {}
            }
        }
    }
    Some(msg)
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
    dns_rcode_name_full(u16::from(r))
}

/// Names a full (EDNS-extended) rcode.
pub fn dns_rcode_name_full(r: u16) -> String {
    match r {
        0 => "NOERROR".into(),
        1 => "FORMERR".into(),
        2 => "SERVFAIL".into(),
        3 => "NXDOMAIN".into(),
        4 => "NOTIMP".into(),
        5 => "REFUSED".into(),
        16 => "BADVERS".into(),
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

/// The largest handshake message body accepted (a ClientHello with large post-quantum key shares is ~2 KiB).
const MAX_HANDSHAKE: usize = 65_535;

/// The outcome of looking for the first handshake message in the start of a TLS byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hs<'a> {
    /// The first handshake message is complete: its type and body (the 4-byte handshake header removed). The body
    /// is borrowed when it sits inside one record and owned when it was stitched from several.
    Complete(u8, std::borrow::Cow<'a, [u8]>),
    /// A valid prefix: more bytes are needed.
    Need,
    /// Not a TLS handshake stream (or lengths that cannot be one).
    Not,
}

/// Finds the first handshake message at the start of `b`, a stream of TLS records. A message split across several
/// records (RFC 8446 section 5.1 permits fragmentation) is stitched together; up to 8 records are walked.
pub fn handshake_message(b: &[u8]) -> Hs<'_> {
    use std::borrow::Cow;
    // Whether `p` holds a whole message; `Err` for a length that cannot be right.
    fn whole(p: &[u8]) -> Result<Option<(u8, usize)>, ()> {
        if p.len() < 4 {
            return Ok(None);
        }
        let len = ((p[1] as usize) << 16) | ((p[2] as usize) << 8) | p[3] as usize;
        if len > MAX_HANDSHAKE {
            return Err(());
        }
        Ok((p.len() >= 4 + len).then_some((p[0], len)))
    }
    let mut off = 0usize;
    let mut stitched: Vec<u8> = Vec::new();
    for rec_no in 0..8 {
        match b.get(off) {
            None => return Hs::Need,
            Some(0x16) => {}
            Some(_) => return Hs::Not,
        }
        match b.get(off + 1) {
            None => return Hs::Need,
            Some(3) => {}
            Some(_) => return Hs::Not,
        }
        let Some(len) = be16(b, off + 3) else { return Hs::Need };
        let len = len as usize;
        if len == 0 || len > 16_384 {
            return Hs::Not;
        }
        let end = (off + 5 + len).min(b.len());
        let rec = &b[(off + 5).min(end)..end];
        if rec_no == 0 {
            match whole(rec) {
                Err(()) => return Hs::Not,
                Ok(Some((ty, l))) => return Hs::Complete(ty, Cow::Borrowed(&rec[4..4 + l])),
                Ok(None) => {}
            }
        }
        stitched.extend_from_slice(rec);
        match whole(&stitched) {
            Err(()) => return Hs::Not,
            Ok(Some((ty, l))) => return Hs::Complete(ty, Cow::Owned(stitched[4..4 + l].to_vec())),
            Ok(None) => {}
        }
        if off + 5 + len > b.len() {
            return Hs::Need;
        }
        off += 5 + len;
    }
    Hs::Not
}

/// Parses a TLS ClientHello from the first bytes of a TCP stream. Returns `None` unless the whole handshake
/// message is present in `b` (a hello that continues in a later segment needs the reassembled stream).
pub fn parse_client_hello(b: &[u8]) -> Option<ClientHello> {
    match handshake_message(b) {
        Hs::Complete(1, body) => parse_client_hello_body(&body),
        _ => None,
    }
}

/// Parses a ClientHello handshake body (the bytes after the 4-byte handshake header).
pub fn parse_client_hello_body(body: &[u8]) -> Option<ClientHello> {
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
        self.ja4_for('t')
    }

    /// JA4 with an explicit transport prefix: `t` for TLS over TCP, `q` for QUIC.
    pub fn ja4_for(&self, transport: char) -> String {
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
            "{}{}{}{:02}{:02}{}",
            transport,
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
// TLS ServerHello, JA3S, JA4S
// ---------------------------------------------------------------------------------------------------------------

/// The `random` a server sends to mark a HelloRetryRequest (RFC 8446 section 4.1.3).
const HELLO_RETRY_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91, 0xc2, 0xa2,
    0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// What a ServerHello says in the clear. Only the hello itself is read: certificates (clear in TLS 1.2) and
/// everything after are never decoded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerHello {
    pub legacy_version: u16,
    pub cipher: u16,
    pub compression: u8,
    /// Extension types in wire order.
    pub extensions: Vec<u16>,
    /// The version from the `supported_versions` extension (TLS 1.3).
    pub selected_version: Option<u16>,
    /// The ALPN protocol selected (clear in a TLS 1.2 ServerHello; encrypted in 1.3).
    pub alpn: Option<Vec<u8>>,
    pub key_share_group: Option<u16>,
    /// This is a HelloRetryRequest rather than a real ServerHello.
    pub hello_retry: bool,
}

/// Parses a ServerHello from the first bytes of the server's side of a TCP stream.
pub fn parse_server_hello(b: &[u8]) -> Option<ServerHello> {
    match handshake_message(b) {
        Hs::Complete(2, body) => parse_server_hello_body(&body),
        _ => None,
    }
}

/// Parses a ServerHello handshake body (the bytes after the 4-byte handshake header).
pub fn parse_server_hello_body(body: &[u8]) -> Option<ServerHello> {
    let mut h = ServerHello { legacy_version: be16(body, 0)?, ..Default::default() };
    h.hello_retry = body.get(2..34)? == HELLO_RETRY_RANDOM;
    let mut p = 34;
    let sid = *body.get(p)? as usize;
    p += 1 + sid;
    h.cipher = be16(body, p)?;
    p += 2;
    h.compression = *body.get(p)?;
    p += 1;
    if p == body.len() {
        return Some(h);
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
        h.extensions.push(ty);
        match ty {
            43 if data.len() == 2 => h.selected_version = be16(data, 0),
            51 if data.len() >= 2 => h.key_share_group = be16(data, 0),
            16 => {
                // protocol_name_list with exactly one entry.
                let n = be16(data, 0)? as usize;
                let list = data.get(2..2 + n)?;
                let l = *list.first()? as usize;
                h.alpn = list.get(1..1 + l).map(<[u8]>::to_vec);
            }
            _ => {}
        }
    }
    Some(h)
}

pub fn tls_version_name(v: u16) -> String {
    match v {
        0x0304 => "TLS1.3".into(),
        0x0303 => "TLS1.2".into(),
        0x0302 => "TLS1.1".into(),
        0x0301 => "TLS1.0".into(),
        0x0300 => "SSL3".into(),
        n => format!("0x{n:04x}"),
    }
}

pub fn tls_cipher_name(c: u16) -> String {
    match c {
        0x1301 => "TLS_AES_128_GCM_SHA256".into(),
        0x1302 => "TLS_AES_256_GCM_SHA384".into(),
        0x1303 => "TLS_CHACHA20_POLY1305_SHA256".into(),
        0x1304 => "TLS_AES_128_CCM_SHA256".into(),
        0x1305 => "TLS_AES_128_CCM_8_SHA256".into(),
        0xc02b => "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256".into(),
        0xc02c => "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384".into(),
        0xc02f => "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256".into(),
        0xc030 => "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384".into(),
        0xcca8 => "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256".into(),
        0xcca9 => "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256".into(),
        0x009c => "TLS_RSA_WITH_AES_128_GCM_SHA256".into(),
        0x009d => "TLS_RSA_WITH_AES_256_GCM_SHA384".into(),
        0xc013 => "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA".into(),
        0xc014 => "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA".into(),
        0x002f => "TLS_RSA_WITH_AES_128_CBC_SHA".into(),
        0x0035 => "TLS_RSA_WITH_AES_256_CBC_SHA".into(),
        n => format!("0x{n:04x}"),
    }
}

impl ServerHello {
    /// The version actually negotiated: the `supported_versions` selection when present, else the legacy field.
    pub fn negotiated_version(&self) -> u16 {
        self.selected_version.unwrap_or(self.legacy_version)
    }

    /// The JA3S input string: `version,cipher,extensions` with the extension types in wire order.
    pub fn ja3s_string(&self) -> String {
        format!(
            "{},{},{}",
            self.legacy_version,
            self.cipher,
            join_dec(self.extensions.iter().map(|e| u32::from(*e)))
        )
    }

    /// MD5 of [`Self::ja3s_string`] (the JA3S hash).
    pub fn ja3s(&self) -> String {
        hex(&Md5::digest(self.ja3s_string().as_bytes()))
    }

    /// The JA4S fingerprint, `a_b_c`: protocol, version, extension count and ALPN; the chosen cipher; and a hash of
    /// the extension types in wire order. `transport` is `t` (TCP) or `q` (QUIC).
    pub fn ja4s_for(&self, transport: char) -> String {
        let version = match self.negotiated_version() {
            0x0304 => "13",
            0x0303 => "12",
            0x0302 => "11",
            0x0301 => "10",
            0x0300 => "s3",
            _ => "00",
        };
        let alpn = match &self.alpn {
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
        let a = format!("{transport}{version}{:02}{alpn}", self.extensions.len().min(99));
        let c = if self.extensions.is_empty() {
            "000000000000".to_string()
        } else {
            let s = self.extensions.iter().map(|e| format!("{e:04x}")).collect::<Vec<_>>().join(",");
            hex(&Sha256::digest(s.as_bytes()))[..12].to_string()
        };
        format!("{a}_{:04x}_{c}", self.cipher)
    }

    pub fn ja4s(&self) -> String {
        self.ja4s_for('t')
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Encrypted DNS recognition
// ---------------------------------------------------------------------------------------------------------------

/// Public DNS-over-HTTPS resolvers' names, matched against the SNI of a TLS or QUIC hello. Deployments extend
/// this through `ObserverConfig::doh_hosts`.
pub const DEFAULT_DOH_HOSTS: &[&str] = &[
    "dns.google",
    "dns.google.com",
    "cloudflare-dns.com",
    "one.one.one.one",
    "dns.quad9.net",
    "dns9.quad9.net",
    "dns10.quad9.net",
    "dns11.quad9.net",
    "doh.opendns.com",
    "dns.nextdns.io",
    "dns.adguard-dns.com",
    "doh.cleanbrowsing.org",
    "doh.mullvad.net",
    "doh.libredns.gr",
];

/// True when `sni` is one of `hosts` or a subdomain of one (case-insensitive, trailing dot ignored).
pub fn sni_is_doh(sni: &str, hosts: &[String]) -> bool {
    let n = sni.trim_end_matches('.').to_ascii_lowercase();
    hosts.iter().any(|h| {
        let h = h.trim_end_matches('.').to_ascii_lowercase();
        !h.is_empty() && (n == h || (n.len() > h.len() && n.ends_with(&h) && n.as_bytes()[n.len() - h.len() - 1] == b'.'))
    })
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

    /// An answer record for [`dns_response_rr`].
    pub enum Rr<'a> {
        A([u8; 4]),
        Aaaa([u8; 16]),
        Cname(&'a str),
        Https { priority: u16, target: &'a str, alpn: &'a [&'a str], port: Option<u16>, ech: bool },
        /// Any other type with opaque rdata.
        Raw(u16, &'a [u8]),
    }

    fn opt_rr(udp: u16, ext_rcode: u8, do_bit: bool) -> Vec<u8> {
        let mut r = vec![0u8, 0, 41];
        r.extend_from_slice(&udp.to_be_bytes());
        r.push(ext_rcode);
        r.push(0); // version
        r.extend_from_slice(&(if do_bit { 0x8000u16 } else { 0 }).to_be_bytes());
        r.extend_from_slice(&[0, 0]); // no options
        r
    }

    /// A query carrying an EDNS(0) OPT record.
    pub fn dns_query_edns(id: u16, qname: &str, qtype: u16, udp: u16, do_bit: bool) -> Vec<u8> {
        let mut m = dns_query(id, qname, qtype);
        m[10..12].copy_from_slice(&1u16.to_be_bytes());
        m.extend(opt_rr(udp, 0, do_bit));
        m
    }

    /// A response with typed answers (names in rdata are uncompressed) and an optional OPT record
    /// `(udp size, DO bit, extended rcode)`.
    pub fn dns_response_rr(
        id: u16,
        qname: &str,
        qtype: u16,
        rcode: u8,
        answers: &[Rr<'_>],
        edns: Option<(u16, bool, u8)>,
    ) -> Vec<u8> {
        let mut m = vec![(id >> 8) as u8, id as u8, 0x81, 0x80 | (rcode & 0x0f)];
        m.extend_from_slice(&[0, 1]);
        m.extend_from_slice(&(answers.len() as u16).to_be_bytes());
        m.extend_from_slice(&[0, 0]);
        m.extend_from_slice(&(edns.is_some() as u16).to_be_bytes());
        m.extend(name(qname));
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        for a in answers {
            let (ty, rd): (u16, Vec<u8>) = match a {
                Rr::A(ip) => (1, ip.to_vec()),
                Rr::Aaaa(ip) => (28, ip.to_vec()),
                Rr::Cname(t) => (5, name(t)),
                Rr::Https { priority, target, alpn, port, ech } => {
                    let mut r = priority.to_be_bytes().to_vec();
                    r.extend(name(if *target == "." { "" } else { target }));
                    if !alpn.is_empty() {
                        let mut list = Vec::new();
                        for p in *alpn {
                            list.push(p.len() as u8);
                            list.extend_from_slice(p.as_bytes());
                        }
                        r.extend_from_slice(&[0, 1]);
                        r.extend_from_slice(&(list.len() as u16).to_be_bytes());
                        r.extend(list);
                    }
                    if let Some(p) = port {
                        r.extend_from_slice(&[0, 3, 0, 2]);
                        r.extend_from_slice(&p.to_be_bytes());
                    }
                    if *ech {
                        r.extend_from_slice(&[0, 5, 0, 3, 1, 2, 3]);
                    }
                    (65, r)
                }
                Rr::Raw(t, d) => (*t, d.to_vec()),
            };
            m.extend_from_slice(&[0xc0, 0x0c]);
            m.extend_from_slice(&ty.to_be_bytes());
            m.extend_from_slice(&[0, 1, 0, 0, 0, 60]);
            m.extend_from_slice(&(rd.len() as u16).to_be_bytes());
            m.extend(rd);
        }
        if let Some((udp, do_bit, ext)) = edns {
            m.extend(opt_rr(udp, ext, do_bit));
        }
        m
    }

    /// A TLS record carrying one ServerHello (session id empty, no compression).
    pub fn server_hello(legacy_version: u16, cipher: u16, exts: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&legacy_version.to_be_bytes());
        body.extend_from_slice(&[0x24; 32]);
        body.push(0);
        body.extend_from_slice(&cipher.to_be_bytes());
        body.push(0);
        let mut e = Vec::new();
        for (ty, d) in exts {
            e.extend_from_slice(&ty.to_be_bytes());
            e.extend_from_slice(&(d.len() as u16).to_be_bytes());
            e.extend_from_slice(d);
        }
        body.extend_from_slice(&(e.len() as u16).to_be_bytes());
        body.extend(e);
        let mut hs = vec![2, (body.len() >> 16) as u8, (body.len() >> 8) as u8, body.len() as u8];
        hs.extend(body);
        let mut rec = vec![0x16, 0x03, 0x03];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend(hs);
        rec
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
    fn handshake_message_stitches_records_and_reports_needs() {
        let rec = client_hello(0x0303, &[0x1301], &[ext_sni("frag.example.com"), ext_alpn(&["h2"])]);
        let hs = &rec[5..];
        // Re-frame the same handshake message as three TLS records.
        let mut frag = Vec::new();
        for part in [&hs[..10], &hs[10..40], &hs[40..]] {
            frag.extend_from_slice(&[0x16, 0x03, 0x01]);
            frag.extend_from_slice(&(part.len() as u16).to_be_bytes());
            frag.extend_from_slice(part);
        }
        match handshake_message(&frag) {
            Hs::Complete(1, body) => {
                assert!(matches!(body, std::borrow::Cow::Owned(_)));
                assert_eq!(parse_client_hello_body(&body).unwrap().sni.as_deref(), Some("frag.example.com"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(parse_client_hello(&frag).unwrap().alpn, vec![b"h2".to_vec()]);
        // A single record borrows.
        assert!(matches!(handshake_message(&rec), Hs::Complete(1, std::borrow::Cow::Borrowed(_))));
        // Every proper prefix is "need more", never "complete" and never a panic.
        for n in 1..frag.len() {
            assert_eq!(handshake_message(&frag[..n]), Hs::Need, "prefix {n}");
        }
        assert_eq!(handshake_message(b""), Hs::Need);
        assert_eq!(handshake_message(b"GET /"), Hs::Not);
        assert_eq!(handshake_message(&[0x16, 0x03, 0x01, 0x00, 0x00]), Hs::Not, "zero-length record");
        assert_eq!(handshake_message(&[0x16, 0x03, 0x01, 0xff, 0xff]), Hs::Not, "record over 2^14");
        // A handshake header claiming more than the cap.
        let mut huge = vec![0x16, 0x03, 0x01, 0x00, 0x08, 0x01, 0xff, 0xff, 0xff];
        huge.extend_from_slice(&[0; 4]);
        assert_eq!(handshake_message(&huge), Hs::Not);
    }

    #[test]
    fn server_hello_parsing_and_fingerprints() {
        let tls13 = server_hello(0x0303, 0x1301, &[ext(43, &[3, 4]), ext(51, &[0, 0x1d, 0, 0])]);
        let h = parse_server_hello(&tls13).unwrap();
        assert_eq!((h.legacy_version, h.cipher, h.selected_version), (0x0303, 0x1301, Some(0x0304)));
        assert_eq!(h.negotiated_version(), 0x0304);
        assert_eq!(h.extensions, vec![43, 51]);
        assert!(!h.hello_retry);
        assert_eq!(tls_version_name(h.negotiated_version()), "TLS1.3");
        assert_eq!(tls_cipher_name(h.cipher), "TLS_AES_128_GCM_SHA256");
        assert_eq!(h.ja3s_string(), "771,4865,43-51");
        assert_eq!(h.ja3s(), hex(&Md5::digest(b"771,4865,43-51")));
        // JA4S for a TLS 1.3 hello with extensions 002b,0033 is the FoxIO documentation example
        // `t130200_1301_a56c5b993250`; the third part is SHA-256("002b,0033")[..12], checked here by computation.
        assert_eq!(h.ja4s(), "t130200_1301_a56c5b993250");
        assert_eq!(h.ja4s_for('q'), "q130200_1301_a56c5b993250");

        // TLS 1.2 with a clear ALPN and the classic JA3S shape.
        let tls12 = server_hello(0x0303, 0xc02f, &[ext(0xff01, &[0]), ext(0, &[]), ext(11, &[1, 0]), ext_alpn(&["http/1.1"])]);
        let h = parse_server_hello(&tls12).unwrap();
        assert_eq!(h.ja3s_string(), "771,49199,65281-0-11-16");
        assert_eq!(h.alpn.as_deref(), Some(&b"http/1.1"[..]));
        assert!(h.ja4s().starts_with("t120411_c02f_") || h.ja4s().starts_with("t1204h1_c02f_"), "{}", h.ja4s());
        assert_eq!(tls_version_name(h.negotiated_version()), "TLS1.2");

        // No extensions at all.
        let bare = parse_server_hello(&server_hello(0x0303, 0x009c, &[])).unwrap();
        assert_eq!(bare.ja3s_string(), "771,156,");
        assert_eq!(bare.ja4s(), "t120000_009c_000000000000");
    }

    #[test]
    fn hello_retry_request_is_flagged() {
        let mut rec = server_hello(0x0303, 0x1301, &[ext(43, &[3, 4])]);
        // The random sits after record header (5), handshake header (4) and version (2).
        rec[11..43].copy_from_slice(&HELLO_RETRY_RANDOM);
        assert!(parse_server_hello(&rec).unwrap().hello_retry);
    }

    #[test]
    fn server_hello_garbage_never_panics() {
        let base = server_hello(0x0303, 0x1301, &[ext(43, &[3, 4]), ext(51, &[0, 0x1d, 0, 0]), ext_alpn(&["h2"])]);
        for n in 0..base.len() {
            let _ = parse_server_hello(&base[..n]);
        }
        let mut s = 7u64;
        for _ in 0..5000 {
            let mut m = base.clone();
            for _ in 0..3 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let i = (s >> 33) as usize % m.len();
                m[i] = (s >> 20) as u8;
            }
            let _ = parse_server_hello(&m);
            let _ = handshake_message(&m);
        }
    }

    #[test]
    fn dns_answer_types_https_and_edns() {
        use fixtures::Rr;
        let m = parse_dns(&dns_response_rr(
            3,
            "svc.example.com",
            65,
            0,
            &[
                Rr::Cname("t.example.net"),
                Rr::Https { priority: 0, target: "alt.example.com", alpn: &[], port: None, ech: false },
                Rr::A([1, 2, 3, 4]),
                Rr::Raw(99, &[1, 2, 3]),
            ],
            Some((1400, true, 0)),
        ))
        .unwrap();
        assert_eq!(m.answer_types, vec![5, 65, 1, 99]);
        assert_eq!(m.cnames, vec!["t.example.net"]);
        assert_eq!(m.https[0].priority, 0);
        assert_eq!(m.https[0].target, "alt.example.com");
        assert_eq!(m.addrs, vec![IpAddr::from([1, 2, 3, 4])]);
        assert_eq!(m.edns, Some(Edns { udp_size: 1400, version: 0, ext_rcode: 0, dnssec_ok: true }));
        // Truncated anywhere: whatever parsed so far is returned or None; never a panic.
        let full = dns_response_rr(3, "x.test", 1, 0, &[Rr::Cname("y.test"), Rr::A([9, 9, 9, 9])], Some((512, false, 0)));
        for n in 0..full.len() {
            let _ = parse_dns(&full[..n]);
        }
        // A bad record in the middle keeps the answers before it.
        let mut bad = full.clone();
        let at = bad.len() - 8; // inside the OPT record
        bad[at] = 0xff;
        let p = parse_dns(&bad).unwrap();
        assert_eq!(p.addrs, vec![IpAddr::from([9, 9, 9, 9])]);
    }

    #[test]
    fn doh_host_matching() {
        let hosts: Vec<String> = ["dns.google", "Cloudflare-DNS.com."].iter().map(|s| s.to_string()).collect();
        assert!(sni_is_doh("dns.google", &hosts));
        assert!(sni_is_doh("DNS.GOOGLE.", &hosts));
        assert!(sni_is_doh("mozilla.cloudflare-dns.com", &hosts));
        assert!(!sni_is_doh("notdns.google", &hosts), "label boundary");
        assert!(!sni_is_doh("google", &hosts));
        assert!(!sni_is_doh("example.com", &hosts));
        assert!(!sni_is_doh("anything", &["".to_string()]));
        assert!(DEFAULT_DOH_HOSTS.contains(&"dns.google"));
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
