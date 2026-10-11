// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Cleartext HTTP/2 (prior-knowledge `h2c`) metadata: connection preface detection and HEADERS decoding.
//!
//! What is read: from each HEADERS (and PUSH_PROMISE / CONTINUATION) block, only `:method`, `:authority`,
//! `:scheme`, the response `:status` and `content-type`. The request path, query, cookies, authorization and every
//! other header are decoded *only as far as HPACK requires to stay in sync*: their values are never stored, and a
//! Huffman-coded value that is not wanted is merely counted, not materialised. DATA frames are skipped by length.
//!
//! Scope and limits: HTTP/2 inside TLS is encrypted and not visible here; this is for cleartext h2c (service
//! meshes, local gRPC). The input is the reassembled *prefix* of each direction (see [`crate::reasm`]), so only
//! the first requests and responses of a long connection are reported, and a decoding error ends decoding of that
//! direction for the rest of the connection (HPACK state cannot be resynchronised). Everything is bounded and
//! nothing panics on hostile input.

use crate::hpack_huffman::CODES;
use std::collections::VecDeque;
use std::sync::OnceLock;

/// The client connection preface (RFC 9113 section 3.4).
pub const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// `b` is a non-empty prefix of the client preface (so more bytes could still complete it).
pub fn is_preface_prefix(b: &[u8]) -> bool {
    !b.is_empty() && b.len() <= PREFACE.len() && PREFACE.starts_with(b)
}

/// `b` starts with the full client preface.
pub fn has_preface(b: &[u8]) -> bool {
    b.starts_with(PREFACE)
}

/// Largest frame payload accepted (the protocol default, RFC 9113 SETTINGS_MAX_FRAME_SIZE).
pub const MAX_FRAME_PAYLOAD: usize = 16_384;
/// Largest dynamic table size a peer may announce before decoding gives up.
pub const MAX_TABLE_SIZE: usize = 65_536;
const DEFAULT_TABLE_SIZE: usize = 4096;
const MAX_HEADERS_PER_BLOCK: usize = 256;
const MAX_FIELD_VALUE: usize = 256;

// ---------------------------------------------------------------------------------------------------------------
// Huffman
// ---------------------------------------------------------------------------------------------------------------

struct Trie {
    /// `[child on 0, child on 1]`: 0 = none, > 0 = node index, < 0 = leaf holding symbol `-(v + 1)`.
    nodes: Vec<[i32; 2]>,
}

fn trie() -> &'static Trie {
    static T: OnceLock<Trie> = OnceLock::new();
    T.get_or_init(|| {
        let mut nodes: Vec<[i32; 2]> = vec![[0, 0]];
        for (sym, &(len, code)) in CODES.iter().enumerate() {
            let mut at = 0usize;
            for i in (0..len).rev() {
                let bit = ((code >> i) & 1) as usize;
                if i == 0 {
                    nodes[at][bit] = -(sym as i32) - 1;
                } else {
                    if nodes[at][bit] == 0 {
                        nodes.push([0, 0]);
                        let n = (nodes.len() - 1) as i32;
                        nodes[at][bit] = n;
                    }
                    at = nodes[at][bit] as usize;
                }
            }
        }
        Trie { nodes }
    })
}

/// Huffman-decodes `input`. Returns the decoded length; decoded bytes are appended to `out` while it is shorter
/// than `cap` (so a caller can both measure and keep a bounded prefix). `None` on an invalid code, EOS in the
/// data, or invalid padding (RFC 7541 section 5.2).
pub fn huffman_decode(input: &[u8], mut out: Option<&mut Vec<u8>>, cap: usize) -> Option<usize> {
    let t = trie();
    let mut node = 0usize;
    let mut pad_bits = 0u32;
    let mut pad_ones = true;
    let mut count = 0usize;
    for &byte in input {
        for i in (0..8).rev() {
            let bit = ((byte >> i) & 1) as usize;
            let next = t.nodes[node][bit];
            if next == 0 {
                return None;
            }
            if next < 0 {
                let sym = (-(next + 1)) as usize;
                if sym == 256 {
                    return None;
                }
                count += 1;
                if let Some(o) = out.as_deref_mut()
                    && o.len() < cap
                {
                    o.push(sym as u8);
                }
                node = 0;
                pad_bits = 0;
                pad_ones = true;
            } else {
                node = next as usize;
                pad_bits += 1;
                pad_ones &= bit == 1;
            }
        }
    }
    if node != 0 && (pad_bits > 7 || !pad_ones) {
        return None;
    }
    Some(count)
}

// ---------------------------------------------------------------------------------------------------------------
// HPACK
// ---------------------------------------------------------------------------------------------------------------

/// RFC 7541 Appendix A.
const STATIC_TABLE: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

/// The only header fields whose values are ever kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Method,
    Authority,
    Scheme,
    Status,
    ContentType,
    Other,
}

impl Field {
    fn from_name(n: &[u8]) -> Field {
        match n {
            b":method" => Field::Method,
            b":authority" => Field::Authority,
            b":scheme" => Field::Scheme,
            b":status" => Field::Status,
            b"content-type" => Field::ContentType,
            _ => Field::Other,
        }
    }
}


#[derive(Debug, Clone)]
struct Entry {
    field: Field,
    /// Kept only for the five wanted fields (and only when short); `None` for everything else.
    value: Option<String>,
    /// Decoded octet counts, which RFC 7541 section 4.1 uses for the entry size.
    name_len: usize,
    value_len: usize,
}

impl Entry {
    fn size(&self) -> usize {
        self.name_len + self.value_len + 32
    }
}

/// The fields of one decoded header block that this module reports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    pub method: Option<String>,
    pub authority: Option<String>,
    pub scheme: Option<String>,
    pub status: Option<u16>,
    /// The media type only (parameters such as `charset` removed), lower-case, at most 64 bytes.
    pub content_type: Option<String>,
    /// Number of header fields in the block (all of them, wanted or not).
    pub fields: usize,
}

impl Headers {
    fn set(&mut self, f: Field, v: &str) {
        if !v.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return;
        }
        match f {
            Field::Method if self.method.is_none() && !v.is_empty() && v.len() <= 16 => {
                self.method = Some(v.to_ascii_uppercase());
            }
            Field::Authority if self.authority.is_none() && !v.is_empty() && v.len() <= 255 => {
                self.authority = Some(v.to_ascii_lowercase());
            }
            Field::Scheme if self.scheme.is_none() && !v.is_empty() && v.len() <= 16 => {
                self.scheme = Some(v.to_ascii_lowercase());
            }
            Field::Status if self.status.is_none() => {
                if let Ok(s) = v.parse::<u16>()
                    && (100..600).contains(&s)
                {
                    self.status = Some(s);
                }
            }
            Field::ContentType if self.content_type.is_none() => {
                let media = v.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
                if !media.is_empty() && media.len() <= 64 {
                    self.content_type = Some(media);
                }
            }
            _ => {}
        }
    }
}

/// Why a header block could not be decoded. The decoder is unusable afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HpackError {
    Truncated,
    BadInteger,
    BadIndex,
    BadHuffman,
    TableSize,
    TooManyFields,
}

/// An HPACK decoder (RFC 7541) for one direction of one connection.
#[derive(Debug, Clone)]
pub struct HpackDecoder {
    table: VecDeque<Entry>,
    size: usize,
    max_size: usize,
}

impl Default for HpackDecoder {
    fn default() -> Self {
        Self::new()
    }
}

struct StrRef<'a> {
    raw: &'a [u8],
    huffman: bool,
}

fn read_int(b: &[u8], pos: &mut usize, prefix: u32) -> Result<usize, HpackError> {
    let first = *b.get(*pos).ok_or(HpackError::Truncated)?;
    *pos += 1;
    let mask = (1usize << prefix) - 1;
    let mut v = usize::from(first) & mask;
    if v < mask {
        return Ok(v);
    }
    let mut shift = 0u32;
    loop {
        let byte = *b.get(*pos).ok_or(HpackError::Truncated)?;
        *pos += 1;
        if shift > 28 {
            return Err(HpackError::BadInteger);
        }
        v = v.checked_add(usize::from(byte & 0x7f) << shift).ok_or(HpackError::BadInteger)?;
        if byte & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

fn read_str<'a>(b: &'a [u8], pos: &mut usize) -> Result<StrRef<'a>, HpackError> {
    let first = *b.get(*pos).ok_or(HpackError::Truncated)?;
    let huffman = first & 0x80 != 0;
    let len = read_int(b, pos, 7)?;
    let end = pos.checked_add(len).ok_or(HpackError::Truncated)?;
    let raw = b.get(*pos..end).ok_or(HpackError::Truncated)?;
    *pos = end;
    Ok(StrRef { raw, huffman })
}

impl StrRef<'_> {
    /// Decoded length; decoded bytes (up to `cap` in total) are appended to `out`.
    fn decode(&self, out: Option<&mut Vec<u8>>, cap: usize) -> Result<usize, HpackError> {
        if self.huffman {
            huffman_decode(self.raw, out, cap).ok_or(HpackError::BadHuffman)
        } else {
            if let Some(o) = out {
                let room = cap.saturating_sub(o.len());
                o.extend_from_slice(&self.raw[..self.raw.len().min(room)]);
            }
            Ok(self.raw.len())
        }
    }
}

impl HpackDecoder {
    pub fn new() -> Self {
        Self::with_table_size(DEFAULT_TABLE_SIZE)
    }

    /// A decoder whose dynamic table starts at `size` bytes (a peer may raise it, up to [`MAX_TABLE_SIZE`], with
    /// a table size update).
    pub fn with_table_size(size: usize) -> Self {
        Self { table: VecDeque::new(), size: 0, max_size: size.min(MAX_TABLE_SIZE) }
    }

    /// Number of dynamic table entries.
    pub fn dynamic_len(&self) -> usize {
        self.table.len()
    }

    /// Bytes the dynamic table accounts for under RFC 7541 section 4.1.
    pub fn dynamic_size(&self) -> usize {
        self.size
    }

    fn evict_to(&mut self, limit: usize) {
        while self.size > limit {
            match self.table.pop_back() {
                Some(e) => self.size -= e.size(),
                None => {
                    self.size = 0;
                    break;
                }
            }
        }
    }

    fn insert(&mut self, e: Entry) {
        let size = e.size();
        if size > self.max_size {
            self.table.clear();
            self.size = 0;
            return;
        }
        self.evict_to(self.max_size - size);
        self.size += size;
        self.table.push_front(e);
    }

    /// The entry at 1-based table index `idx`.
    fn lookup(&self, idx: usize) -> Result<Entry, HpackError> {
        if idx == 0 {
            return Err(HpackError::BadIndex);
        }
        if idx <= STATIC_TABLE.len() {
            let (n, v) = STATIC_TABLE[idx - 1];
            let field = Field::from_name(n.as_bytes());
            let value = (field != Field::Other && !v.is_empty()).then(|| v.to_string());
            return Ok(Entry { field, value, name_len: n.len(), value_len: v.len() });
        }
        self.table.get(idx - STATIC_TABLE.len() - 1).cloned().ok_or(HpackError::BadIndex)
    }

    /// Decodes one complete header block, returning the fields of interest. Values of every other header are
    /// consumed (HPACK needs their lengths) but not retained.
    pub fn decode_block(&mut self, block: &[u8]) -> Result<Headers, HpackError> {
        let mut h = Headers::default();
        let mut pos = 0;
        while pos < block.len() {
            let b = block[pos];
            if b & 0x80 != 0 {
                // Indexed header field.
                let idx = read_int(block, &mut pos, 7)?;
                let e = self.lookup(idx)?;
                h.fields += 1;
                if h.fields > MAX_HEADERS_PER_BLOCK {
                    return Err(HpackError::TooManyFields);
                }
                if let Some(v) = &e.value {
                    h.set(e.field, v);
                }
            } else if b & 0xe0 == 0x20 {
                // Dynamic table size update.
                let n = read_int(block, &mut pos, 5)?;
                if n > MAX_TABLE_SIZE {
                    return Err(HpackError::TableSize);
                }
                self.max_size = n;
                self.evict_to(n);
            } else {
                let incremental = b & 0x40 != 0;
                let idx = read_int(block, &mut pos, if incremental { 6 } else { 4 })?;
                h.fields += 1;
                if h.fields > MAX_HEADERS_PER_BLOCK {
                    return Err(HpackError::TooManyFields);
                }
                let (field, name_len) = if idx == 0 {
                    let s = read_str(block, &mut pos)?;
                    let mut name = Vec::new();
                    let len = s.decode(Some(&mut name), 32)?;
                    (if len <= 32 { Field::from_name(&name) } else { Field::Other }, len)
                } else {
                    let e = self.lookup(idx)?;
                    (e.field, e.name_len)
                };
                let v = read_str(block, &mut pos)?;
                let (value, value_len) = if field == Field::Other {
                    (None, v.decode(None, 0)?)
                } else {
                    let mut buf = Vec::new();
                    let len = v.decode(Some(&mut buf), MAX_FIELD_VALUE + 1)?;
                    if len > MAX_FIELD_VALUE {
                        (None, len)
                    } else {
                        (String::from_utf8(buf).ok(), len)
                    }
                };
                if let Some(s) = &value {
                    h.set(field, s);
                }
                if incremental {
                    self.insert(Entry { field, value, name_len, value_len });
                }
            }
        }
        Ok(h)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------------------------

const FRAME_HEADERS: u8 = 1;
const FRAME_PUSH_PROMISE: u8 = 5;
const FRAME_CONTINUATION: u8 = 9;
const FLAG_END_HEADERS: u8 = 0x4;
const FLAG_PADDED: u8 = 0x8;
const FLAG_PRIORITY: u8 = 0x20;

/// One request or response head recovered from a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H2Message {
    /// Sent by the connection's client (a request) rather than the server (a response).
    pub from_client: bool,
    pub headers: Headers,
}

/// A header block spread over HEADERS + CONTINUATION frames.
#[derive(Debug, Clone)]
struct Pending {
    block: Vec<u8>,
    /// PUSH_PROMISE blocks keep HPACK in sync but are not reported.
    report: bool,
}

/// Incremental frame parser and HPACK state for one direction of an h2c connection.
#[derive(Debug, Clone)]
pub struct H2Dir {
    from_client: bool,
    hpack: HpackDecoder,
    consumed: usize,
    preface_seen: bool,
    pending: Option<Pending>,
    /// Decoding stopped for good (protocol error, HPACK error, or a frame larger than the limit).
    pub dead: bool,
    /// The reason decoding stopped, when it was an error rather than the end of the observed prefix.
    pub error: Option<&'static str>,
}

impl H2Dir {
    pub fn new(from_client: bool) -> Self {
        Self {
            from_client,
            hpack: HpackDecoder::new(),
            consumed: 0,
            preface_seen: !from_client,
            pending: None,
            dead: false,
            error: None,
        }
    }

    pub fn mem(&self) -> usize {
        self.pending.as_ref().map_or(0, |p| p.block.capacity()) + self.hpack.dynamic_len() * 64
    }

    fn fail(&mut self, why: &'static str) {
        self.dead = true;
        self.error = Some(why);
        self.pending = None;
    }

    /// Consumes whole frames from `buf` (the reassembled prefix of this direction) that have not been looked at
    /// yet, appending reportable messages to `out`. Safe to call again with a longer `buf`.
    pub fn advance(&mut self, buf: &[u8], out: &mut Vec<H2Message>) {
        if self.dead {
            return;
        }
        if !self.preface_seen {
            if buf.len() < PREFACE.len() {
                if !PREFACE.starts_with(buf) {
                    self.fail("bad client preface");
                }
                return;
            }
            if !has_preface(buf) {
                self.fail("bad client preface");
                return;
            }
            self.preface_seen = true;
            self.consumed = PREFACE.len();
        }
        loop {
            let at = self.consumed;
            let Some(hdr) = buf.get(at..at + 9) else { return };
            let len = (usize::from(hdr[0]) << 16) | (usize::from(hdr[1]) << 8) | usize::from(hdr[2]);
            let (ty, flags) = (hdr[3], hdr[4]);
            if len > MAX_FRAME_PAYLOAD {
                self.fail("frame larger than the default maximum");
                return;
            }
            let Some(payload) = buf.get(at + 9..at + 9 + len) else { return };
            self.consumed = at + 9 + len;
            if self.pending.is_some() && ty != FRAME_CONTINUATION {
                self.fail("frame interleaved in a header block");
                return;
            }
            match ty {
                FRAME_HEADERS | FRAME_PUSH_PROMISE => {
                    let Some(fragment) = header_fragment(ty, flags, payload) else {
                        self.fail("bad frame padding");
                        return;
                    };
                    let report = ty == FRAME_HEADERS;
                    if flags & FLAG_END_HEADERS != 0 {
                        self.decode(fragment, report, out);
                    } else {
                        self.pending = Some(Pending { block: fragment.to_vec(), report });
                    }
                }
                FRAME_CONTINUATION => {
                    let Some(mut p) = self.pending.take() else {
                        self.fail("unexpected CONTINUATION");
                        return;
                    };
                    if p.block.len() + payload.len() > MAX_FRAME_PAYLOAD {
                        self.fail("header block larger than the limit");
                        return;
                    }
                    p.block.extend_from_slice(payload);
                    if flags & FLAG_END_HEADERS != 0 {
                        let block = std::mem::take(&mut p.block);
                        self.decode(&block, p.report, out);
                    } else {
                        self.pending = Some(p);
                    }
                }
                _ => {}
            }
            if self.dead {
                return;
            }
        }
    }

    fn decode(&mut self, block: &[u8], report: bool, out: &mut Vec<H2Message>) {
        match self.hpack.decode_block(block) {
            Ok(headers) => {
                if report && (headers.method.is_some() || headers.status.is_some()) {
                    out.push(H2Message { from_client: self.from_client, headers });
                }
            }
            Err(_) => self.fail("HPACK decoding error"),
        }
    }
}

/// The header block fragment of a HEADERS or PUSH_PROMISE frame payload, with padding, priority and promised
/// stream id removed.
fn header_fragment(ty: u8, flags: u8, payload: &[u8]) -> Option<&[u8]> {
    let mut p = payload;
    let mut pad = 0usize;
    if flags & FLAG_PADDED != 0 {
        pad = usize::from(*p.first()?);
        p = &p[1..];
    }
    if ty == FRAME_PUSH_PROMISE {
        p = p.get(4..)?;
    } else if flags & FLAG_PRIORITY != 0 {
        p = p.get(5..)?;
    }
    let keep = p.len().checked_sub(pad)?;
    Some(&p[..keep])
}

/// Builders for h2c fixtures: connection preface, SETTINGS, and HEADERS frames with simple (literal,
/// non-indexed, non-Huffman) HPACK encodings. Public so other crates' tests can synthesise traffic.
pub mod fixtures {
    use super::*;

    pub fn frame(ty: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![(payload.len() >> 16) as u8, (payload.len() >> 8) as u8, payload.len() as u8, ty, flags];
        f.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
        f.extend_from_slice(payload);
        f
    }

    pub fn settings() -> Vec<u8> {
        frame(4, 0, 0, &[])
    }

    pub fn client_preface() -> Vec<u8> {
        let mut v = PREFACE.to_vec();
        v.extend(settings());
        v
    }

    pub fn headers_frame(stream: u32, end_stream: bool, block: &[u8]) -> Vec<u8> {
        frame(FRAME_HEADERS, FLAG_END_HEADERS | u8::from(end_stream), stream, block)
    }

    fn lit(out: &mut Vec<u8>, name_idx_prefix: &[u8], value: &[u8]) {
        out.extend_from_slice(name_idx_prefix);
        assert!(value.len() < 127);
        out.push(value.len() as u8);
        out.extend_from_slice(value);
    }

    /// An HPACK block for a request: `:method`, `:scheme`, `:path`, `:authority` (inserted into the dynamic
    /// table) and an extra `user-agent`.
    pub fn request_block(method: &str, scheme: &str, path: &str, authority: &str) -> Vec<u8> {
        let mut b = Vec::new();
        match method {
            "GET" => b.push(0x82),
            "POST" => b.push(0x83),
            m => lit(&mut b, &[0x02], m.as_bytes()), // :method literal, no indexing
        }
        match scheme {
            "http" => b.push(0x86),
            "https" => b.push(0x87),
            s => lit(&mut b, &[0x06], s.as_bytes()),
        }
        lit(&mut b, &[0x04], path.as_bytes()); // :path
        lit(&mut b, &[0x41], authority.as_bytes()); // :authority, incremental indexing
        lit(&mut b, &[0x0f, 0x2b], b"fixture-agent/1.0"); // user-agent (index 58 = 15 + 43)
        b
    }

    /// An HPACK block for a response.
    pub fn response_block(status: u16, content_type: Option<&str>) -> Vec<u8> {
        let mut b = Vec::new();
        match status {
            200 => b.push(0x88),
            204 => b.push(0x89),
            404 => b.push(0x8d),
            s => lit(&mut b, &[0x08], s.to_string().as_bytes()), // :status literal
        }
        if let Some(ct) = content_type {
            lit(&mut b, &[0x0f, 0x10], ct.as_bytes()); // content-type (index 31 = 15 + 16)
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn headers(d: &mut HpackDecoder, hex: &str) -> Headers {
        d.decode_block(&unhex(hex)).unwrap()
    }

    // RFC 7541 Appendix C.4 (requests, Huffman) and C.3 (requests, plain). Transcribed from memory of the RFC;
    // they are decoded by an implementation that shares nothing with that memory, so a mistake fails the test.
    #[test]
    fn rfc7541_c3_requests_without_huffman() {
        let mut d = HpackDecoder::new();
        let h = headers(&mut d, "8286 8441 0f77 7777 2e65 7861 6d70 6c65 2e63 6f6d");
        assert_eq!(h.method.as_deref(), Some("GET"));
        assert_eq!(h.scheme.as_deref(), Some("http"));
        assert_eq!(h.authority.as_deref(), Some("www.example.com"));
        assert_eq!(h.fields, 4);
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (1, 57));

        let h = headers(&mut d, "8286 84be 5808 6e6f 2d63 6163 6865");
        assert_eq!(h.authority.as_deref(), Some("www.example.com"), "from the dynamic table");
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (2, 110));

        let h = headers(
            &mut d,
            "8287 85bf 400a 6375 7374 6f6d 2d6b 6579 0c63 7573 746f 6d2d 7661 6c75 65",
        );
        assert_eq!(h.scheme.as_deref(), Some("https"));
        assert_eq!(h.authority.as_deref(), Some("www.example.com"));
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (3, 164));
    }

    #[test]
    fn rfc7541_c4_requests_with_huffman() {
        let mut d = HpackDecoder::new();
        let h = headers(&mut d, "8286 8441 8cf1 e3c2 e5f2 3a6b a0ab 90f4 ff");
        assert_eq!(h.method.as_deref(), Some("GET"));
        assert_eq!(h.authority.as_deref(), Some("www.example.com"));
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (1, 57));
        let h = headers(&mut d, "8286 84be 5886 a8eb 1064 9cbf");
        assert_eq!(h.authority.as_deref(), Some("www.example.com"));
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (2, 110));
        let h = headers(&mut d, "8287 85bf 4088 25a8 49e9 5ba9 7d7f 8925 a849 e95b b8e8 b4bf");
        assert_eq!(h.scheme.as_deref(), Some("https"));
        assert_eq!(h.authority.as_deref(), Some("www.example.com"));
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (3, 164));
    }

    // RFC 7541 Appendix C.5/C.6: responses with a 256-byte dynamic table, which forces evictions.
    #[test]
    fn rfc7541_c5_responses_evict_from_a_small_table() {
        let mut d = HpackDecoder::with_table_size(256);
        let h = headers(
            &mut d,
            "4803 3330 3258 0770 7269 7661 7465 611d 4d6f 6e2c 2032 3120 4f63 7420 3230 3133 \
             2032 303a 3133 3a32 3120 474d 546e 1768 7474 7073 3a2f 2f77 7777 2e65 7861 6d70 \
             6c65 2e63 6f6d",
        );
        assert_eq!(h.status, Some(302));
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (4, 222));
        let h = headers(&mut d, "4803 3330 37c1 c0bf");
        assert_eq!(h.status, Some(307));
        // :status 302 was evicted to make room for :status 307.
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (4, 222));
    }

    #[test]
    fn rfc7541_c6_huffman_responses() {
        let mut d = HpackDecoder::with_table_size(256);
        let h = headers(
            &mut d,
            "4882 6402 5885 aec3 771a 4b61 96d0 7abe 9410 54d4 44a8 2005 9504 0b81 66e0 82a6 \
             2d1b ff6e 919d 29ad 1718 63c7 8f0b 97c8 e9ae 82ae 43d3",
        );
        assert_eq!(h.status, Some(302));
        assert_eq!((d.dynamic_len(), d.dynamic_size()), (4, 222));
    }

    #[test]
    fn huffman_known_strings() {
        let mut out = Vec::new();
        let n = huffman_decode(&unhex("f1e3 c2e5 f23a 6ba0 ab90 f4ff"), Some(&mut out), 100).unwrap();
        assert_eq!(out, b"www.example.com");
        assert_eq!(n, 15);
        // Count-only mode agrees, and a cap truncates the copy but not the count.
        assert_eq!(huffman_decode(&unhex("f1e3 c2e5 f23a 6ba0 ab90 f4ff"), None, 0), Some(15));
        let mut small = Vec::new();
        assert_eq!(huffman_decode(&unhex("f1e3 c2e5 f23a 6ba0 ab90 f4ff"), Some(&mut small), 4), Some(15));
        assert_eq!(small, b"www.");
        // Padding that is not all ones is rejected; so is padding of 8 or more bits.
        assert!(huffman_decode(&unhex("f1e3 c2e5 f23a 6ba0 ab90 f4fe"), None, 0).is_none());
        assert!(huffman_decode(&unhex("f1e3 c2e5 f23a 6ba0 ab90 f4ff ff"), None, 0).is_none());
        // EOS (30 one-bits) inside the data is an error.
        assert!(huffman_decode(&[0xff, 0xff, 0xff, 0xff], None, 0).is_none());
    }

    #[test]
    fn static_table_spot_checks() {
        assert_eq!(STATIC_TABLE[1], (":method", "GET"));
        assert_eq!(STATIC_TABLE[7], (":status", "200"));
        assert_eq!(STATIC_TABLE[30], ("content-type", ""));
        assert_eq!(STATIC_TABLE[60], ("www-authenticate", ""));
        assert_eq!(STATIC_TABLE[14], ("accept-charset", ""));
    }

    #[test]
    fn path_and_other_values_are_never_kept() {
        let mut d = HpackDecoder::new();
        let h = d
            .decode_block(&request_block("GET", "https", "/very/secret/path?token=abc123", "Api.Example.com"))
            .unwrap();
        assert_eq!(h.authority.as_deref(), Some("api.example.com"));
        let dbg = format!("{h:?}{d:?}");
        assert!(!dbg.contains("secret") && !dbg.contains("abc123") && !dbg.contains("fixture-agent"), "{dbg}");
    }

    #[test]
    fn response_status_and_content_type() {
        let mut d = HpackDecoder::new();
        let h = d.decode_block(&response_block(200, Some("Application/JSON; charset=utf-8"))).unwrap();
        assert_eq!(h.status, Some(200));
        assert_eq!(h.content_type.as_deref(), Some("application/json"));
        let h = d.decode_block(&response_block(503, None)).unwrap();
        assert_eq!(h.status, Some(503));
        assert_eq!(h.content_type, None);
    }

    #[test]
    fn dynamic_table_size_update_and_limits() {
        let mut d = HpackDecoder::new();
        // size update to 0 empties the table
        d.decode_block(&request_block("GET", "http", "/", "a.test")).unwrap();
        assert_eq!(d.dynamic_len(), 1);
        d.decode_block(&[0x20]).unwrap();
        assert_eq!(d.dynamic_len(), 0);
        // Beyond the hard cap is an error (5-bit prefix: 31 + continuation).
        let mut big = vec![0x3f];
        big.extend_from_slice(&[0xe1, 0xff, 0x7f]);
        assert_eq!(d.decode_block(&big), Err(HpackError::TableSize));
        // Index 0 and out-of-range indexes.
        assert_eq!(HpackDecoder::new().decode_block(&[0x80]), Err(HpackError::BadIndex));
        assert_eq!(HpackDecoder::new().decode_block(&[0xff, 0x7f]), Err(HpackError::BadIndex));
        // Runaway integer continuation.
        assert!(HpackDecoder::new().decode_block(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).is_err());
    }

    #[test]
    fn dynamic_table_stays_bounded() {
        let mut d = HpackDecoder::with_table_size(512);
        for i in 0..2000 {
            let name = format!("x-header-{i}");
            let mut b = vec![0x40, name.len() as u8];
            b.extend_from_slice(name.as_bytes());
            b.push(10);
            b.extend_from_slice(b"0123456789");
            d.decode_block(&b).unwrap();
            assert!(d.dynamic_size() <= 512);
            assert!(d.dynamic_len() <= 512 / 32);
        }
    }

    #[test]
    fn too_many_fields_is_an_error() {
        let block = vec![0x82u8; MAX_HEADERS_PER_BLOCK + 1];
        assert_eq!(HpackDecoder::new().decode_block(&block), Err(HpackError::TooManyFields));
    }

    #[test]
    fn preface_detection() {
        assert!(has_preface(&client_preface()));
        assert!(is_preface_prefix(b"PRI * HT"));
        assert!(!is_preface_prefix(b"GET / HT"));
        assert!(!is_preface_prefix(b""));
    }

    #[test]
    fn frames_requests_and_responses() {
        let mut stream = client_preface();
        stream.extend(headers_frame(1, true, &request_block("POST", "http", "/upload/secret", "svc.local:8080")));
        stream.extend(frame(0, 0, 1, b"data data data")); // DATA is skipped
        stream.extend(headers_frame(3, true, &request_block("GET", "http", "/other", "svc.local:8080")));
        let mut c = H2Dir::new(true);
        let mut out = Vec::new();
        c.advance(&stream, &mut out);
        assert_eq!(out.len(), 2);
        assert!(out[0].from_client);
        assert_eq!(out[0].headers.method.as_deref(), Some("POST"));
        assert_eq!(out[0].headers.authority.as_deref(), Some("svc.local:8080"));
        assert_eq!(out[1].headers.method.as_deref(), Some("GET"));
        assert!(!format!("{out:?}").contains("secret"));

        let mut resp = settings();
        resp.extend(headers_frame(1, false, &response_block(200, Some("application/grpc"))));
        resp.extend(headers_frame(3, true, &response_block(404, None)));
        let mut s = H2Dir::new(false);
        let mut out = Vec::new();
        s.advance(&resp, &mut out);
        assert_eq!(out.len(), 2);
        assert!(!out[0].from_client);
        assert_eq!(out[0].headers.status, Some(200));
        assert_eq!(out[0].headers.content_type.as_deref(), Some("application/grpc"));
        assert_eq!(out[1].headers.status, Some(404));
    }

    #[test]
    fn incremental_advance_matches_one_shot() {
        let mut stream = client_preface();
        stream.extend(headers_frame(1, true, &request_block("GET", "http", "/", "a.test")));
        stream.extend(headers_frame(3, true, &request_block("GET", "http", "/x", "b.test")));
        let mut inc = H2Dir::new(true);
        let mut got = Vec::new();
        for n in 0..=stream.len() {
            inc.advance(&stream[..n], &mut got);
        }
        let mut one = H2Dir::new(true);
        let mut want = Vec::new();
        one.advance(&stream, &mut want);
        assert_eq!(got, want);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn continuation_padding_and_priority() {
        let block = request_block("GET", "https", "/", "split.test");
        let (a, b) = block.split_at(block.len() / 2);
        let mut stream = client_preface();
        // HEADERS (PADDED | PRIORITY, no END_HEADERS) + CONTINUATION (END_HEADERS)
        let mut p = vec![3u8]; // pad length
        p.extend_from_slice(&[0, 0, 0, 0, 16]); // priority
        p.extend_from_slice(a);
        p.extend_from_slice(&[0, 0, 0]);
        stream.extend(frame(FRAME_HEADERS, FLAG_PADDED | FLAG_PRIORITY | 1, 1, &p));
        stream.extend(frame(FRAME_CONTINUATION, FLAG_END_HEADERS, 1, b));
        let mut c = H2Dir::new(true);
        let mut out = Vec::new();
        c.advance(&stream, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].headers.authority.as_deref(), Some("split.test"));
        assert!(!c.dead);
    }

    #[test]
    fn push_promise_keeps_hpack_in_sync_without_reporting() {
        // Server side: PUSH_PROMISE inserts :authority into the dynamic table; a later response uses nothing from
        // it but the decoder must not desynchronise.
        let mut s = H2Dir::new(false);
        let mut pp = vec![0, 0, 0, 2];
        pp.extend(request_block("GET", "http", "/pushed", "push.test"));
        let mut stream = frame(FRAME_PUSH_PROMISE, FLAG_END_HEADERS, 1, &pp);
        stream.extend(headers_frame(2, false, &response_block(200, None)));
        let mut out = Vec::new();
        s.advance(&stream, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].headers.status, Some(200));
    }

    #[test]
    fn protocol_errors_stop_decoding() {
        let mut c = H2Dir::new(true);
        let mut out = Vec::new();
        c.advance(b"GET / HTTP/1.1\r\n\r\nxxxxxxxxxxxx", &mut out);
        assert!(c.dead && out.is_empty());

        let mut c = H2Dir::new(false);
        c.advance(&frame(FRAME_CONTINUATION, FLAG_END_HEADERS, 1, b"x"), &mut out);
        assert!(c.dead);

        let mut c = H2Dir::new(false);
        let mut huge = vec![0x01, 0x00, 0x00, FRAME_HEADERS, FLAG_END_HEADERS, 0, 0, 0, 1]; // 65536 bytes
        huge.extend_from_slice(&[0; 16]);
        c.advance(&huge, &mut out);
        assert!(c.dead);

        // A corrupt HPACK block poisons the direction.
        let mut c = H2Dir::new(false);
        c.advance(&headers_frame(1, true, &[0xff, 0x7f]), &mut out);
        assert!(c.dead);
        assert_eq!(c.error, Some("HPACK decoding error"));
    }

    fn rng(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed;
        move || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            s >> 16
        }
    }

    #[test]
    fn fuzz_hpack_and_frames_never_panic() {
        let mut next = rng(0x5eed);
        let mut seed_stream = client_preface();
        seed_stream.extend(headers_frame(1, true, &request_block("GET", "https", "/p", "fuzz.test")));
        seed_stream.extend(headers_frame(3, false, &response_block(200, Some("text/html"))));
        // Truncations.
        for n in 0..seed_stream.len() {
            let mut c = H2Dir::new(true);
            c.advance(&seed_stream[..n], &mut Vec::new());
        }
        // Mutations of a valid stream.
        for _ in 0..3000 {
            let mut m = seed_stream.clone();
            for _ in 0..(1 + next() % 6) {
                let at = (next() as usize) % m.len();
                m[at] = next() as u8;
            }
            let mut c = H2Dir::new(true);
            c.advance(&m, &mut Vec::new());
            let mut s = H2Dir::new(false);
            s.advance(&m[24.min(m.len())..], &mut Vec::new());
        }
        // Pure noise through HPACK and Huffman.
        for _ in 0..5000 {
            let len = (next() % 80) as usize;
            let b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = HpackDecoder::new().decode_block(&b);
            let _ = huffman_decode(&b, Some(&mut Vec::new()), 64);
            // Biased toward "literal with Huffman string" shapes.
            let mut lit = vec![0x40 | (next() as u8 & 0x3f), 0x80 | (next() as u8 & 0x1f)];
            lit.extend_from_slice(&b);
            let _ = HpackDecoder::new().decode_block(&lit);
        }
    }
}
