// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! QUIC client Initial inspection.
//!
//! An Initial packet is "encrypted" with keys anyone can derive: they come from the Destination Connection ID
//! that is sent in the clear and a version-specific public salt (RFC 9001 section 5.2, RFC 9369 section 3.3).
//! That protection exists to stop off-path tampering, not to keep the contents secret, which is what makes the
//! TLS ClientHello (SNI, ALPN, fingerprint) observable here exactly as it is for TCP/TLS.
//!
//! Only client Initials of QUIC v1 and v2 are decoded. Everything after the hello is encrypted with secrets that
//! are not derivable from the wire, and nothing of the sort is attempted. All output goes through the same
//! redaction as TLS-over-TCP; this module only parses.
//!
//! Every read is bounds-checked and nothing here panics on hostile input.

use crate::l7::{ClientHello, parse_client_hello_body};
use crate::reasm::{Push, Stream};
use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes_gcm::Aes128Gcm;
use aes_gcm::aead::{Aead, Payload};
use hkdf::Hkdf;
use sha2::Sha256;

pub const VERSION_1: u32 = 0x0000_0001;
pub const VERSION_2: u32 = 0x6b33_43cf;

/// RFC 9001 section 5.2.
const SALT_V1: [u8; 20] = [
    0x38, 0x76, 0x2c, 0xf7, 0xf5, 0x59, 0x34, 0xb3, 0x4d, 0x17, 0x9a, 0xe6, 0xa4, 0xc8, 0x0c, 0xad,
    0xcc, 0xbb, 0x7f, 0x0a,
];
/// RFC 9369 section 3.3.1.
const SALT_V2: [u8; 20] = [
    0x0d, 0xed, 0xe3, 0xde, 0xf7, 0x00, 0xa6, 0xdb, 0x81, 0x93, 0x81, 0xbe, 0x6e, 0x26, 0x9d, 0xcb,
    0xf9, 0xbd, 0x2e, 0xd9,
];

/// Most Initial packets decoded per flow, and the CRYPTO bytes kept (a hello with post-quantum key shares is
/// about 2 KiB).
pub const MAX_PACKETS_PER_FLOW: u8 = 8;
pub const MAX_CRYPTO_BYTES: usize = 8 * 1024;
const MAX_CID: usize = 20;

/// The invariant header fields of a long-header packet that is an Initial of a version we can decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialHeader<'a> {
    pub version: u32,
    pub dcid: &'a [u8],
    pub scid: &'a [u8],
    /// Offset of the (protected) packet number field.
    pub pn_offset: usize,
    /// The Length field: packet number plus protected payload including the 16-byte tag.
    pub length: usize,
}

impl InitialHeader<'_> {
    /// Total bytes of this packet inside the datagram.
    pub fn packet_len(&self) -> usize {
        self.pn_offset + self.length
    }
}

fn varint(b: &[u8], at: usize) -> Option<(u64, usize)> {
    let first = *b.get(at)?;
    let len = 1usize << (first >> 6);
    let bytes = b.get(at..at + len)?;
    let mut v = u64::from(first & 0x3f);
    for &x in &bytes[1..] {
        v = (v << 8) | u64::from(x);
    }
    Some((v, len))
}

/// Parses the long header of a QUIC v1/v2 Initial packet at the start of `b`.
pub fn parse_initial_header(b: &[u8]) -> Option<InitialHeader<'_>> {
    let first = *b.first()?;
    if first & 0x80 == 0 {
        return None;
    }
    let version = u32::from_be_bytes([*b.get(1)?, *b.get(2)?, *b.get(3)?, *b.get(4)?]);
    let ptype = (first >> 4) & 3;
    let is_initial = match version {
        VERSION_1 => ptype == 0,
        VERSION_2 => ptype == 1,
        _ => false,
    };
    if !is_initial {
        return None;
    }
    let dcid_len = *b.get(5)? as usize;
    if dcid_len > MAX_CID {
        return None;
    }
    let dcid = b.get(6..6 + dcid_len)?;
    let mut p = 6 + dcid_len;
    let scid_len = *b.get(p)? as usize;
    if scid_len > MAX_CID {
        return None;
    }
    let scid = b.get(p + 1..p + 1 + scid_len)?;
    p += 1 + scid_len;
    let (token_len, n) = varint(b, p)?;
    p += n;
    let token_len = usize::try_from(token_len).ok()?;
    p = p.checked_add(token_len)?;
    let (length, n) = varint(b, p)?;
    p += n;
    let length = usize::try_from(length).ok()?;
    // The packet number is 1..=4 bytes and the tag is 16.
    if length < 1 + 16 || p.checked_add(length)? > b.len() {
        return None;
    }
    Some(InitialHeader {
        version,
        dcid,
        scid,
        pn_offset: p,
        length,
    })
}

/// The packet-protection material for client Initials.
#[derive(Clone, PartialEq, Eq)]
pub struct InitialKeys {
    pub key: [u8; 16],
    pub iv: [u8; 12],
    pub hp: [u8; 16],
}

impl std::fmt::Debug for InitialKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InitialKeys(..)")
    }
}

/// TLS 1.3 `HKDF-Expand-Label` with an empty context.
fn expand_label(prk: &Hkdf<Sha256>, label: &str, out: &mut [u8]) -> Option<()> {
    let full = format!("tls13 {label}");
    let mut info = Vec::with_capacity(4 + full.len());
    info.extend_from_slice(&(out.len() as u16).to_be_bytes());
    info.push(full.len() as u8);
    info.extend_from_slice(full.as_bytes());
    info.push(0);
    prk.expand(&info, out).ok()
}

/// Derives the client Initial keys for `version` from the Destination Connection ID of the client's first
/// Initial. `None` for versions this module does not decode.
pub fn client_initial_keys(version: u32, dcid: &[u8]) -> Option<InitialKeys> {
    let (salt, key_l, iv_l, hp_l) = match version {
        VERSION_1 => (SALT_V1, "quic key", "quic iv", "quic hp"),
        VERSION_2 => (SALT_V2, "quicv2 key", "quicv2 iv", "quicv2 hp"),
        _ => return None,
    };
    let (_, initial) = Hkdf::<Sha256>::extract(Some(&salt), dcid);
    let mut client_secret = [0u8; 32];
    expand_label(&initial, "client in", &mut client_secret)?;
    let client = Hkdf::<Sha256>::from_prk(&client_secret).ok()?;
    let mut k = InitialKeys {
        key: [0; 16],
        iv: [0; 12],
        hp: [0; 16],
    };
    expand_label(&client, key_l, &mut k.key)?;
    expand_label(&client, iv_l, &mut k.iv)?;
    expand_label(&client, hp_l, &mut k.hp)?;
    Some(k)
}

/// The header-protection mask: AES-ECB of a 16-byte sample (RFC 9001 section 5.4.3).
pub fn hp_mask(hp: &[u8; 16], sample: &[u8; 16]) -> [u8; 16] {
    let cipher = Aes128::new(GenericArray::from_slice(hp));
    let mut block = GenericArray::clone_from_slice(sample);
    cipher.encrypt_block(&mut block);
    block.into()
}

/// A decoded client Initial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedInitial {
    /// The packet number as transmitted (truncated; not expanded against a largest-acknowledged value).
    pub packet_number: u64,
    /// CRYPTO frames as `(stream offset, data)`.
    pub crypto: Vec<(u64, Vec<u8>)>,
    /// Bytes this packet occupied in the datagram (to find a coalesced packet after it).
    pub packet_len: usize,
}

/// Removes header and packet protection from the Initial packet at the start of `datagram`. `key_dcid` is the
/// connection ID the keys derive from when it differs from the packet's own Destination Connection ID (a client
/// keeps deriving from its first DCID after it has seen the server's).
pub fn decrypt_client_initial(
    datagram: &[u8],
    key_dcid: Option<&[u8]>,
) -> Option<DecryptedInitial> {
    let h = parse_initial_header(datagram)?;
    let keys = client_initial_keys(h.version, key_dcid.unwrap_or(h.dcid))?;
    let sample_at = h.pn_offset + 4;
    let sample: [u8; 16] = datagram.get(sample_at..sample_at + 16)?.try_into().ok()?;
    let mask = hp_mask(&keys.hp, &sample);
    let first = datagram[0] ^ (mask[0] & 0x0f);
    let pn_len = usize::from(first & 3) + 1;
    if h.length < pn_len + 16 {
        return None;
    }
    let mut header = datagram.get(..h.pn_offset + pn_len)?.to_vec();
    header[0] = first;
    let mut pn = 0u64;
    for i in 0..pn_len {
        header[h.pn_offset + i] ^= mask[1 + i];
        pn = (pn << 8) | u64::from(header[h.pn_offset + i]);
    }
    let ct = datagram.get(h.pn_offset + pn_len..h.packet_len())?;
    let mut nonce = keys.iv;
    for (i, b) in pn.to_be_bytes().iter().enumerate() {
        nonce[4 + i] ^= b;
    }
    let aead = Aes128Gcm::new_from_slice(&keys.key).ok()?;
    let plain = aead
        .decrypt(
            GenericArray::from_slice(&nonce),
            Payload {
                msg: ct,
                aad: &header,
            },
        )
        .ok()?;
    Some(DecryptedInitial {
        packet_number: pn,
        crypto: crypto_frames(&plain),
        packet_len: h.packet_len(),
    })
}

/// Walks the frames of an Initial packet and returns its CRYPTO frames. Stops at the first frame type that cannot
/// appear in an Initial (or is malformed).
fn crypto_frames(p: &[u8]) -> Vec<(u64, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    let rd = |i: &mut usize| -> Option<u64> {
        let (v, n) = varint(p, *i)?;
        *i += n;
        Some(v)
    };
    while i < p.len() {
        let ty = p[i];
        i += 1;
        match ty {
            0x00 | 0x01 => {}
            0x02 | 0x03 => {
                let step = (|| {
                    rd(&mut i)?; // largest acknowledged
                    rd(&mut i)?; // delay
                    let ranges = rd(&mut i)?;
                    rd(&mut i)?; // first range
                    for _ in 0..ranges.min(64) {
                        rd(&mut i)?;
                        rd(&mut i)?;
                    }
                    if ty == 0x03 {
                        for _ in 0..3 {
                            rd(&mut i)?;
                        }
                    }
                    Some(())
                })();
                if step.is_none() {
                    break;
                }
            }
            0x06 => {
                let (Some(off), Some(len)) = (rd(&mut i), rd(&mut i)) else {
                    break;
                };
                let Ok(len) = usize::try_from(len) else { break };
                let Some(data) = i.checked_add(len).and_then(|e| p.get(i..e)) else {
                    break;
                };
                if out.len() < 16 {
                    out.push((off, data.to_vec()));
                }
                i += len;
            }
            _ => break,
        }
    }
    out
}

/// What decoding the Initials of one flow has produced so far.
#[derive(Debug, Clone)]
pub struct QuicFlow {
    dcid: [u8; MAX_CID],
    dcid_len: u8,
    crypto: Stream,
    packets: u8,
    finished: bool,
}

impl Default for QuicFlow {
    fn default() -> Self {
        Self::new()
    }
}

impl QuicFlow {
    pub fn new() -> Self {
        Self {
            dcid: [0; MAX_CID],
            dcid_len: 0,
            crypto: Stream::with_base(0),
            packets: 0,
            finished: false,
        }
    }

    pub fn mem(&self) -> usize {
        self.crypto.mem()
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn finish(&mut self) {
        self.finished = true;
        self.crypto.release();
    }

    /// Feeds one client datagram. Returns the ClientHello once the CRYPTO stream holds all of it. `room` is the
    /// remaining observer-wide byte budget; the result's second value is true when a bound forced a drop.
    pub fn on_datagram(&mut self, datagram: &[u8], room: usize) -> (Option<ClientHello>, bool) {
        if self.finished {
            return (None, false);
        }
        let mut dropped = false;
        let mut at = 0;
        // An Initial may be followed by coalesced packets (Handshake, 0-RTT, or another Initial).
        for _ in 0..3 {
            let Some(rest) = datagram.get(at..) else {
                break;
            };
            let Some(h) = parse_initial_header(rest) else {
                break;
            };
            if self.packets >= MAX_PACKETS_PER_FLOW {
                self.finish();
                return (None, dropped);
            }
            if self.dcid_len == 0 {
                self.dcid[..h.dcid.len()].copy_from_slice(h.dcid);
                self.dcid_len = h.dcid.len() as u8;
            }
            self.packets += 1;
            let key_dcid = self.dcid[..self.dcid_len as usize].to_vec();
            let Some(d) = decrypt_client_initial(rest, Some(&key_dcid)) else {
                break;
            };
            at += d.packet_len;
            for (off, data) in &d.crypto {
                let Ok(off) = u32::try_from(*off) else {
                    continue;
                };
                match self.crypto.push(off, data, MAX_CRYPTO_BYTES, 8, room) {
                    Push::Dropped(_) => dropped = true,
                    Push::Appended(_) | Push::Buffered | Push::Ignored => {}
                }
            }
            if let Some(hello) = self.try_hello() {
                self.finish();
                return (Some(hello), dropped);
            }
            if dropped {
                self.finish();
                return (None, true);
            }
        }
        (None, dropped)
    }

    fn try_hello(&self) -> Option<ClientHello> {
        let b = self.crypto.contiguous();
        if b.len() < 4 || b[0] != 1 {
            return None;
        }
        let len = (usize::from(b[1]) << 16) | (usize::from(b[2]) << 8) | usize::from(b[3]);
        parse_client_hello_body(b.get(4..4 + len)?)
    }
}

/// Test helpers that build real, protected Initial packets. Public so other crates' tests can synthesise QUIC
/// traffic; it is the inverse of [`decrypt_client_initial`].
pub mod fixtures {
    use super::*;

    fn put_varint(out: &mut Vec<u8>, v: u64) {
        if v < 0x40 {
            out.push(v as u8);
        } else if v < 0x4000 {
            out.extend_from_slice(&((v as u16) | 0x4000).to_be_bytes());
        } else {
            out.extend_from_slice(&((v as u32) | 0x8000_0000).to_be_bytes());
        }
    }

    /// Builds a protected client Initial whose payload is `frames` padded with PADDING frames so the packet is
    /// `pad_to` bytes in total (use 1200 for a datagram that satisfies the client minimum). The packet number is
    /// encoded in 4 bytes.
    pub fn client_initial(
        version: u32,
        dcid: &[u8],
        scid: &[u8],
        pn: u32,
        frames: &[u8],
        pad_to: usize,
    ) -> Vec<u8> {
        client_initial_keyed(version, dcid, dcid, scid, pn, frames, pad_to)
    }

    /// Like [`client_initial`] but protected with keys derived from `key_dcid` while the header names `dcid` (a
    /// client's later Initials after it has adopted the server's connection ID).
    pub fn client_initial_keyed(
        version: u32,
        dcid: &[u8],
        key_dcid: &[u8],
        scid: &[u8],
        pn: u32,
        frames: &[u8],
        pad_to: usize,
    ) -> Vec<u8> {
        let keys = client_initial_keys(version, key_dcid).expect("supported version");
        let first_type: u8 = if version == VERSION_2 { 0b01 } else { 0b00 };
        let first = 0xc0 | (first_type << 4) | 0x03; // pn length 4
        let mut hdr = vec![first];
        hdr.extend_from_slice(&version.to_be_bytes());
        hdr.push(dcid.len() as u8);
        hdr.extend_from_slice(dcid);
        hdr.push(scid.len() as u8);
        hdr.extend_from_slice(scid);
        put_varint(&mut hdr, 0); // token length
        // The Length field is always encoded in two bytes here so the header size is predictable.
        let fixed = hdr.len() + 2 + 4; // length field + pn
        let plain_len = pad_to.saturating_sub(fixed + 16).max(frames.len());
        let mut plain = frames.to_vec();
        plain.resize(plain_len, 0);
        let length = 4 + plain.len() + 16;
        hdr.extend_from_slice(&((length as u16) | 0x4000).to_be_bytes());
        let pn_offset = hdr.len();
        hdr.extend_from_slice(&pn.to_be_bytes());
        let mut nonce = keys.iv;
        for (i, b) in u64::from(pn).to_be_bytes().iter().enumerate() {
            nonce[4 + i] ^= b;
        }
        let aead = Aes128Gcm::new_from_slice(&keys.key).unwrap();
        let ct = aead
            .encrypt(
                GenericArray::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: &hdr,
                },
            )
            .unwrap();
        let mut pkt = hdr;
        pkt.extend_from_slice(&ct);
        let sample: [u8; 16] = pkt[pn_offset + 4..pn_offset + 20].try_into().unwrap();
        let mask = hp_mask(&keys.hp, &sample);
        pkt[0] ^= mask[0] & 0x0f;
        for i in 0..4 {
            pkt[pn_offset + i] ^= mask[1 + i];
        }
        pkt
    }

    /// A CRYPTO frame carrying `data` at `offset`.
    pub fn crypto_frame(offset: u64, data: &[u8]) -> Vec<u8> {
        let mut f = vec![0x06];
        put_varint(&mut f, offset);
        put_varint(&mut f, data.len() as u64);
        f.extend_from_slice(data);
        f
    }

    /// A ClientHello as a bare handshake message (no TLS record header), as QUIC carries it in CRYPTO frames.
    pub fn handshake_client_hello(exts: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let rec = crate::l7::fixtures::client_hello(0x0303, &[0x1301, 0x1302], exts);
        rec[5..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::l7::fixtures as tls;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // ---- RFC 9001 Appendix A.1/A.2: the client Initial. -------------------------------------------------------
    // These values are the RFC's published ones. This repository has no network access to the RFC text, so they
    // are transcribed from memory; every one of them is *checked against the implementation*, which derives
    // them independently, so a transcription error would show up as a failure rather than a silent pass.

    const A_DCID: &str = "8394c8f03e515708";
    const A_KEY: &str = "1f369613dd76d5467730efcbe3b1a22d";
    const A_IV: &str = "fa044b2f42a3fd3b46fb255c";
    const A_HP: &str = "9f50449e04a0e810283a1e9933adedd2";
    const A_SAMPLE: &str = "d1b1c98dd7689fb8ec11d242b123dc9b";
    const A_MASK: &str = "437b9aec36";
    const A_PROTECTED_HEADER: &str = "c000000001088394c8f03e5157080000449e7b9aec34";
    // The CRYPTO frame of the A.2 client Initial: a ClientHello for example.com offering "alpn".
    const A_CRYPTO: &str = "060040f1010000ed0303ebf8fa56f12939b9584a3896472ec40bb863cfd3e86804fe3a47f06a2b69484c\
        00000413011302010000c000000010000e00000b6578616d706c652e636f6dff01000100000a0008000600\
        1d0017001800100007000504616c706e000500050100000000003300260024001d00209370b2c9caa47f\
        babaf4559fedba753de171fa71f50f1ce15d43e994ec74d748002b0003020304000d0010000e0403050306\
        030203080408050806002d00020101001c00024001003900320408ffffffffffffffff05048000ffff0704\
        8000ffff0801100104800075300901100f088394c8f03e51570806048000ffff";

    #[test]
    fn rfc9001_a1_initial_keys() {
        let k = client_initial_keys(VERSION_1, &unhex(A_DCID)).unwrap();
        assert_eq!(k.key.to_vec(), unhex(A_KEY));
        assert_eq!(k.iv.to_vec(), unhex(A_IV));
        assert_eq!(k.hp.to_vec(), unhex(A_HP));
    }

    #[test]
    fn rfc9001_a2_header_protection_and_packet() {
        let k = client_initial_keys(VERSION_1, &unhex(A_DCID)).unwrap();
        let mask = hp_mask(&k.hp, &unhex(A_SAMPLE).try_into().unwrap());
        assert_eq!(mask[..5].to_vec(), unhex(A_MASK));

        // Build the packet the RFC describes: DCID 8394c8f03e515708, empty SCID, packet number 2, the CRYPTO
        // frame above padded to a 1200-byte datagram.
        let crypto = unhex(A_CRYPTO);
        assert_eq!(
            crypto.len(),
            4 + 0xf1,
            "CRYPTO frame: type + offset + 2-byte length + 241 bytes"
        );
        let pkt = client_initial(VERSION_1, &unhex(A_DCID), &[], 2, &crypto, 1200);
        assert_eq!(pkt.len(), 1200);
        // The protected header and the sample (first ciphertext bytes) match the RFC.
        assert_eq!(pkt[..22].to_vec(), unhex(A_PROTECTED_HEADER));
        assert_eq!(pkt[22..38].to_vec(), unhex(A_SAMPLE));
        // And the last 16 bytes (the AEAD tag) are the RFC's.
        assert_eq!(
            pkt[1184..].to_vec(),
            unhex("e221af44860018ab0856972e194cd934")
        );

        // Decrypt it with the production path.
        let d = decrypt_client_initial(&pkt, None).unwrap();
        assert_eq!(d.packet_number, 2);
        assert_eq!(d.packet_len, 1200);
        assert_eq!(d.crypto.len(), 1);
        assert_eq!(d.crypto[0].0, 0);
        assert_eq!(d.crypto[0].1, crypto[4..].to_vec());

        // And the whole flow driver extracts SNI and ALPN.
        let mut q = QuicFlow::new();
        let (hello, dropped) = q.on_datagram(&pkt, usize::MAX);
        assert!(!dropped);
        let hello = hello.expect("ClientHello decoded");
        assert_eq!(hello.sni.as_deref(), Some("example.com"));
        assert_eq!(hello.alpn, vec![b"alpn".to_vec()]);
        // 2 cipher suites, 11 extensions, first ALPN "alpn" -> "an".
        assert!(
            hello.ja4_for('q').starts_with("q13d0211an_"),
            "{}",
            hello.ja4_for('q')
        );
    }

    // RFC 9369 Appendix A.1: QUIC v2 Initial keys for the same DCID.
    #[test]
    fn rfc9369_a1_v2_initial_keys() {
        let k = client_initial_keys(VERSION_2, &unhex(A_DCID)).unwrap();
        assert_eq!(k.key.to_vec(), unhex("8b1a0bc121284290a29e0971b5cd045d"));
        assert_eq!(k.iv.to_vec(), unhex("91f73e2351d8fa91660e909f"));
        assert_eq!(k.hp.to_vec(), unhex("45b95e15235d6f45a6b19cbcb0294ba9"));
    }

    #[test]
    fn v2_roundtrip_through_the_driver() {
        let hello =
            handshake_client_hello(&[tls::ext_sni("v2.example.org"), tls::ext_alpn(&["h3"])]);
        let pkt = client_initial(
            VERSION_2,
            &[1, 2, 3, 4, 5, 6, 7, 8],
            &[9, 9],
            0,
            &crypto_frame(0, &hello),
            1252,
        );
        let h = parse_initial_header(&pkt).unwrap();
        assert_eq!((h.version, h.dcid.len(), h.scid.len()), (VERSION_2, 8, 2));
        let (hello, _) = QuicFlow::new().on_datagram(&pkt, usize::MAX);
        let hello = hello.unwrap();
        assert_eq!(hello.sni.as_deref(), Some("v2.example.org"));
        assert_eq!(hello.alpn, vec![b"h3".to_vec()]);
    }

    #[test]
    fn hello_split_over_two_crypto_frames_and_two_packets() {
        let hello =
            handshake_client_hello(&[tls::ext_sni("split.example.net"), tls::ext_alpn(&["h3"])]);
        let cut = hello.len() / 2;
        let dcid = [7u8; 8];
        // Two CRYPTO frames in one packet, delivered out of order within it, plus the second half again in a
        // second packet: first the tail, then the head.
        let mut frames = crypto_frame(cut as u64, &hello[cut..]);
        frames.extend(crypto_frame(0, &hello[..cut]));
        let one = client_initial(VERSION_1, &dcid, &[], 0, &frames, 1200);
        let (h, _) = QuicFlow::new().on_datagram(&one, usize::MAX);
        assert_eq!(h.unwrap().sni.as_deref(), Some("split.example.net"));

        let p1 = client_initial(
            VERSION_1,
            &dcid,
            &[],
            0,
            &crypto_frame(cut as u64, &hello[cut..]),
            1200,
        );
        let p2 = client_initial(
            VERSION_1,
            &dcid,
            &[],
            1,
            &crypto_frame(0, &hello[..cut]),
            1200,
        );
        let mut q = QuicFlow::new();
        assert_eq!(q.on_datagram(&p1, usize::MAX).0, None);
        assert!(q.mem() > 0);
        let (h, _) = q.on_datagram(&p2, usize::MAX);
        assert_eq!(h.unwrap().sni.as_deref(), Some("split.example.net"));
        assert!(q.is_finished());
        assert_eq!(q.mem(), 0, "buffers are released once the hello is out");
    }

    #[test]
    fn client_keeps_deriving_from_its_first_dcid() {
        let hello = handshake_client_hello(&[tls::ext_sni("late.example.com")]);
        let cut = hello.len() / 2;
        let first_dcid = [3u8; 8];
        let p1 = client_initial(
            VERSION_1,
            &first_dcid,
            &[],
            0,
            &crypto_frame(0, &hello[..cut]),
            1200,
        );
        let mut q = QuicFlow::new();
        assert_eq!(q.on_datagram(&p1, usize::MAX).0, None);
        // The second packet names a different DCID (the server's chosen connection ID) but is still protected with
        // keys from the first one; the driver must keep using the DCID it stored.
        let p2 = client_initial_keyed(
            VERSION_1,
            &[0xaa; 8],
            &first_dcid,
            &[],
            1,
            &crypto_frame(cut as u64, &hello[cut..]),
            1200,
        );
        assert!(
            decrypt_client_initial(&p2, None).is_none(),
            "own DCID derives the wrong keys"
        );
        assert_eq!(
            q.on_datagram(&p2, usize::MAX).0.unwrap().sni.as_deref(),
            Some("late.example.com")
        );
    }

    #[test]
    fn tampering_and_wrong_keys_fail_cleanly() {
        let hello = handshake_client_hello(&[tls::ext_sni("t.example.com")]);
        let pkt = client_initial(VERSION_1, &[5u8; 8], &[], 0, &crypto_frame(0, &hello), 1200);
        assert!(decrypt_client_initial(&pkt, None).is_some());
        for at in [0usize, 10, 30, 100, 700, 1199] {
            let mut bad = pkt.clone();
            bad[at] ^= 0x55;
            // Either the header no longer parses or the AEAD tag fails; never a panic, never a hello.
            let mut q = QuicFlow::new();
            assert_eq!(q.on_datagram(&bad, usize::MAX).0, None, "byte {at}");
        }
        assert!(decrypt_client_initial(&pkt, Some(&[6u8; 8])).is_none());
    }

    #[test]
    fn unsupported_versions_and_non_initials_are_ignored() {
        let hello = handshake_client_hello(&[tls::ext_sni("t.example.com")]);
        let mut pkt = client_initial(VERSION_1, &[5u8; 8], &[], 0, &crypto_frame(0, &hello), 1200);
        pkt[4] = 0x1d; // a draft version
        assert!(parse_initial_header(&pkt).is_none());
        assert!(client_initial_keys(0xff00_001d, &[1]).is_none());
        let mut short = pkt.clone();
        short[0] = 0x40;
        assert!(parse_initial_header(&short).is_none());
    }

    #[test]
    fn per_flow_packet_cap_stops_work() {
        let mut q = QuicFlow::new();
        let junk = client_initial(VERSION_1, &[5u8; 8], &[], 0, &[0x01], 1200); // PING + padding: no CRYPTO
        for _ in 0..MAX_PACKETS_PER_FLOW + 2 {
            assert!(q.on_datagram(&junk, usize::MAX).0.is_none());
        }
        assert!(q.is_finished());
    }

    #[test]
    fn budget_overflow_is_reported_not_buffered() {
        let hello = handshake_client_hello(&[tls::ext_sni("big.example.com")]);
        let pkt = client_initial(VERSION_1, &[5u8; 8], &[], 0, &crypto_frame(5, &hello), 1200);
        let mut q = QuicFlow::new();
        let (h, dropped) = q.on_datagram(&pkt, 4);
        assert!(h.is_none());
        assert!(dropped);
        assert_eq!(q.mem(), 0);
    }

    #[test]
    fn garbage_never_panics() {
        let hello =
            handshake_client_hello(&[tls::ext_sni("fuzz.example.com"), tls::ext_alpn(&["h3"])]);
        let base = client_initial(
            VERSION_1,
            &[1, 2, 3, 4, 5, 6, 7, 8],
            &[1],
            0,
            &crypto_frame(0, &hello),
            1200,
        );
        let mut s = 0xfeed_u64;
        let mut next = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 24) as usize
        };
        for n in 0..base.len().min(80) {
            let _ = parse_initial_header(&base[..n]);
            let _ = QuicFlow::new().on_datagram(&base[..n], usize::MAX);
        }
        for _ in 0..600 {
            let mut m = base.clone();
            for _ in 0..(1 + next() % 5) {
                let at = next() % 60; // concentrate on the header
                m[at] = next() as u8;
            }
            let _ = QuicFlow::new().on_datagram(&m, usize::MAX);
        }
        // Frame-level garbage inside a validly protected packet.
        for _ in 0..400 {
            let junk: Vec<u8> = (0..(next() % 64)).map(|_| next() as u8).collect();
            let pkt = client_initial(VERSION_1, &[9u8; 8], &[], 1, &junk, 1200);
            let _ = QuicFlow::new().on_datagram(&pkt, usize::MAX);
        }
    }
}
