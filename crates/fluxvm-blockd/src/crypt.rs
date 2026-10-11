// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Encryption at rest: AES-256-XTS per 64 KiB overlay block.
//!
//! The tweak is the block index (little-endian, as in IEEE 1619 sector
//! numbers), so every block of an overlay is encrypted independently and can
//! be rewritten in place. Each overlay stores a salted key-check value (a
//! SHA-256 over a domain string, the salt and the key) so a wrong key is
//! rejected at open instead of silently decrypting to garbage.
//!
//! Limits (inherent to XTS, documented rather than hidden): no
//! authentication (an attacker with write access can flip/replace blocks
//! undetected), and the same plaintext at the same block index under the same
//! key yields the same ciphertext (e.g. a branch and its parent share keys and
//! tweaks). Key material is never logged and `Debug` is not implemented.

use crate::seams::BlockCipher;
use aes::Aes256;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;

/// Environment variable consulted for a hex-encoded key.
pub const KEY_ENV: &str = "FLUXVM_BLOCKD_KEY";

pub struct XtsCipher {
    k1: Aes256,
    k2: Aes256,
    /// SHA-256 state input for key-check values (full 64-byte XTS key).
    material: [u8; 64],
}

impl Drop for XtsCipher {
    fn drop(&mut self) {
        // Best effort scrub of the raw key copy (the AES schedules are not
        // reachable). Volatile so the compiler cannot elide it.
        for b in self.material.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

fn invalid(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, m.to_string())
}

fn mul_alpha(t: &mut [u8; 16]) {
    let carry = t[15] >> 7;
    for i in (1..16).rev() {
        t[i] = (t[i] << 1) | (t[i - 1] >> 7);
    }
    t[0] <<= 1;
    if carry != 0 {
        t[0] ^= 0x87;
    }
}

impl XtsCipher {
    /// `key` is 64 bytes (two AES-256 keys, used as-is) or 32 bytes (expanded
    /// to 64 with SHA-256 domain separation).
    pub fn new(key: &[u8]) -> io::Result<Self> {
        let mut material = [0u8; 64];
        match key.len() {
            64 => material.copy_from_slice(key),
            32 => {
                for (i, half) in material.chunks_mut(32).enumerate() {
                    let mut h = Sha256::new();
                    h.update(b"fluxvm-blockd xts key expand v1");
                    h.update([i as u8 + 1]);
                    h.update(key);
                    half.copy_from_slice(&h.finalize());
                }
            }
            _ => return Err(invalid("encryption key must be 32 or 64 bytes")),
        }
        if material[..32] == material[32..] {
            return Err(invalid("XTS key halves must differ"));
        }
        Ok(Self {
            k1: Aes256::new(GenericArray::from_slice(&material[..32])),
            k2: Aes256::new(GenericArray::from_slice(&material[32..])),
            material,
        })
    }

    /// Parse key bytes from text (hex, 64 or 128 digits) or raw bytes.
    pub fn from_bytes_or_hex(raw: &[u8]) -> io::Result<Self> {
        let trimmed: &[u8] = {
            let mut t = raw;
            while let [rest @ .., b'\n' | b'\r' | b' ' | b'\t'] = t {
                t = rest;
            }
            t
        };
        if (trimmed.len() == 64 || trimmed.len() == 128)
            && trimmed.iter().all(|b| b.is_ascii_hexdigit())
        {
            return Self::new(&unhex(trimmed)?);
        }
        Self::new(raw)
    }

    /// Read a key file: 32/64 raw bytes or 64/128 hex digits.
    pub fn from_file(path: &Path) -> io::Result<Self> {
        let raw = std::fs::read(path)?;
        Self::from_bytes_or_hex(&raw)
            .map_err(|e| io::Error::new(e.kind(), format!("key file {}: {e}", path.display())))
    }

    /// Read hex key from the environment variable `var`.
    pub fn from_env(var: &str) -> io::Result<Option<Self>> {
        match std::env::var(var) {
            Ok(v) => {
                let t = v.trim().as_bytes();
                if !(t.len() == 64 || t.len() == 128) {
                    return Err(invalid("key env var must hold 64 or 128 hex digits"));
                }
                Ok(Some(Self::new(&unhex(t)?)?))
            }
            Err(_) => Ok(None),
        }
    }

    fn crypt(&self, block_index: u64, data: &mut [u8], enc: bool) -> io::Result<()> {
        if data.is_empty() || !data.len().is_multiple_of(16) {
            return Err(invalid("XTS data must be a non-empty multiple of 16 bytes"));
        }
        let mut t = [0u8; 16];
        t[..8].copy_from_slice(&block_index.to_le_bytes());
        self.k2.encrypt_block(GenericArray::from_mut_slice(&mut t));
        for chunk in data.as_chunks_mut::<16>().0 {
            for (c, k) in chunk.iter_mut().zip(t.iter()) {
                *c ^= k;
            }
            let b = GenericArray::from_mut_slice(&mut chunk[..]);
            if enc {
                self.k1.encrypt_block(b);
            } else {
                self.k1.decrypt_block(b);
            }
            for (c, k) in chunk.iter_mut().zip(t.iter()) {
                *c ^= k;
            }
            mul_alpha(&mut t);
        }
        Ok(())
    }
}

fn unhex(s: &[u8]) -> io::Result<Vec<u8>> {
    let v = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    s.chunks(2)
        .map(
            |p| match (p.first().and_then(|c| v(*c)), p.get(1).and_then(|c| v(*c))) {
                (Some(a), Some(b)) => Ok(a << 4 | b),
                _ => Err(invalid("invalid hex key")),
            },
        )
        .collect()
}

impl BlockCipher for XtsCipher {
    fn encrypt(&self, block_index: u64, data: &mut [u8]) -> io::Result<()> {
        self.crypt(block_index, data, true)
    }
    fn decrypt(&self, block_index: u64, data: &mut [u8]) -> io::Result<()> {
        self.crypt(block_index, data, false)
    }
    fn key_check(&self, salt: &[u8; 16]) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(b"fluxvm-blockd kcv v1");
        h.update(salt);
        h.update(self.material);
        h.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_vector() {
        // Generated with python `cryptography` (OpenSSL) AES-256-XTS,
        // tweak = 5 little-endian, key = 0..64, plaintext = (i*3) & 255.
        let key: Vec<u8> = (0..64).collect();
        let c = XtsCipher::new(&key).unwrap();
        let pt: Vec<u8> = (0..64u32).map(|i| (i * 3) as u8).collect();
        let mut buf = pt.clone();
        c.encrypt(5, &mut buf).unwrap();
        let expect = "ffa5dbcb22deb09f40b04a480e964847109dc74995ca2a58ff4979e2a22ead82\
                      0ef7d89b96f0799fbb41b9c38b27b58286c3980d58ed6103d02038816153b277";
        assert_eq!(buf, unhex(expect.as_bytes()).unwrap());
        c.decrypt(5, &mut buf).unwrap();
        assert_eq!(buf, pt);
    }

    #[test]
    fn key_forms_and_errors() {
        assert!(XtsCipher::new(&[1u8; 16]).is_err());
        assert!(XtsCipher::new(&[7u8; 64]).is_err(), "equal halves");
        let hex = "ab".repeat(32);
        let a = XtsCipher::from_bytes_or_hex(hex.as_bytes()).unwrap();
        let b = XtsCipher::new(&[0xab; 32]).unwrap();
        assert_eq!(a.key_check(&[0; 16]), b.key_check(&[0; 16]));
        assert_ne!(a.key_check(&[0; 16]), a.key_check(&[1; 16]));
        let mut x = [0u8; 17];
        assert!(a.encrypt(0, &mut x).is_err());
    }

    #[test]
    fn tweak_changes_ciphertext() {
        let c = XtsCipher::new(&[3u8; 32]).unwrap();
        let mut a = vec![0u8; 4096];
        let mut b = a.clone();
        c.encrypt(1, &mut a).unwrap();
        c.encrypt(2, &mut b).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, vec![0u8; 4096]);
    }
}
