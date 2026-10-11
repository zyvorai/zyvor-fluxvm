// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Client-side layer encryption: AES-256-GCM over 1 MiB frames, so a
//! multi-GiB layer streams through constant memory.
//!
//! Layout: an 8-byte magic, an 8-byte random nonce prefix, then frames of
//! `u32 BE ciphertext length || ciphertext+tag`. Frame `i` uses the nonce
//! `prefix || i (u32 BE)` and authenticates `magic || prefix || i || last`,
//! so frames can't be reordered, dropped, or the stream truncated (the
//! final frame, possibly empty, is the only one with `last = 1`).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

pub const ALGORITHM: &str = "aes256-gcm";
const MAGIC: &[u8; 8] = b"FVCBTE1\0";
const FRAME: usize = 1 << 20;
const TAG: usize = 16;

/// A 256-bit key from its base64 form.
pub fn parse_key(b64: &str) -> Result<[u8; 32]> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .context("encryption key is not base64")?;
    raw.try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("encryption key is {} bytes, want 32", v.len()))
}

fn aad(prefix: &[u8; 8], index: u32, last: bool) -> [u8; 21] {
    let mut a = [0u8; 21];
    a[..8].copy_from_slice(MAGIC);
    a[8..16].copy_from_slice(prefix);
    a[16..20].copy_from_slice(&index.to_be_bytes());
    a[20] = last as u8;
    a
}

fn nonce(prefix: &[u8; 8], index: u32) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(prefix);
    n[8..].copy_from_slice(&index.to_be_bytes());
    n
}

/// Reads up to `buf.len()` bytes, short only at end of input.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Encrypts `src` into `dst`. Blocking; run it off the async runtime.
pub fn encrypt_file(src: &Path, dst: &Path, key: &[u8; 32]) -> Result<()> {
    let cipher = Aes256Gcm::new(key.into());
    let mut prefix = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut prefix);
    let mut r =
        BufReader::new(File::open(src).with_context(|| format!("opening {}", src.display()))?);
    let mut w =
        BufWriter::new(File::create(dst).with_context(|| format!("creating {}", dst.display()))?);
    w.write_all(MAGIC)?;
    w.write_all(&prefix)?;
    let mut cur = vec![0u8; FRAME];
    let mut next = vec![0u8; FRAME];
    let mut cur_len = fill(&mut r, &mut cur)?;
    let mut index: u32 = 0;
    loop {
        // Look ahead one frame so the final one is known when sealed.
        let next_len = if cur_len == FRAME {
            fill(&mut r, &mut next)?
        } else {
            0
        };
        let last = next_len == 0;
        let ct = cipher
            .encrypt(
                Nonce::from_slice(&nonce(&prefix, index)),
                Payload {
                    msg: &cur[..cur_len],
                    aad: &aad(&prefix, index, last),
                },
            )
            .map_err(|_| anyhow::anyhow!("encrypting frame {index}"))?;
        w.write_all(&(ct.len() as u32).to_be_bytes())?;
        w.write_all(&ct)?;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
        index = index.checked_add(1).context("layer too large to encrypt")?;
    }
    w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(())
}

/// The next frame's ciphertext, or `None` at a clean end of input.
fn read_frame(r: &mut impl Read, index: u32) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match fill(r, &mut len)? {
        0 => return Ok(None),
        4 => {}
        _ => bail!("encrypted layer frame {index} is truncated"),
    }
    let n = u32::from_be_bytes(len) as usize;
    if !(TAG..=FRAME + TAG).contains(&n) {
        bail!("encrypted layer frame {index} has an invalid length {n}");
    }
    let mut ct = vec![0u8; n];
    if fill(r, &mut ct)? != n {
        bail!("encrypted layer frame {index} is truncated");
    }
    Ok(Some(ct))
}

/// Decrypts `src` into `dst`, failing on a wrong key, tampering, reordered
/// frames or truncation. Blocking; run it off the async runtime.
pub fn decrypt_file(src: &Path, dst: &Path, key: &[u8; 32]) -> Result<()> {
    let cipher = Aes256Gcm::new(key.into());
    let mut r =
        BufReader::new(File::open(src).with_context(|| format!("opening {}", src.display()))?);
    let mut head = [0u8; 16];
    if fill(&mut r, &mut head)? != 16 || &head[..8] != MAGIC {
        bail!("{} is not an encrypted FluxVM backup layer", src.display());
    }
    let prefix: [u8; 8] = head[8..].try_into().unwrap();
    let mut w =
        BufWriter::new(File::create(dst).with_context(|| format!("creating {}", dst.display()))?);
    let mut index: u32 = 0;
    let mut cur = read_frame(&mut r, index)?.context("encrypted layer has no frames")?;
    loop {
        // Read one frame ahead: the current frame is the last exactly when
        // nothing follows, and its tag only verifies with that flag set.
        let next = read_frame(&mut r, index.saturating_add(1))?;
        let last = next.is_none();
        let pt = cipher
            .decrypt(
                Nonce::from_slice(&nonce(&prefix, index)),
                Payload { msg: &cur, aad: &aad(&prefix, index, last) },
            )
            .map_err(|_| {
                anyhow::anyhow!(
                    "encrypted layer frame {index} failed authentication (wrong key, tampered or truncated data)"
                )
            })?;
        w.write_all(&pt)?;
        match next {
            None => break,
            Some(ct) => {
                cur = ct;
                index = index
                    .checked_add(1)
                    .context("encrypted layer has too many frames")?;
            }
        }
    }
    w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(())
}

/// SHA-256 of a file, hex. Blocking.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut r =
        BufReader::new(File::open(path).with_context(|| format!("opening {}", path.display()))?);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; FRAME];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(len: usize) {
        let dir = tempfile::tempdir().unwrap();
        let (plain, enc, out) = (
            dir.path().join("p"),
            dir.path().join("e"),
            dir.path().join("o"),
        );
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        std::fs::write(&plain, &data).unwrap();
        let key = [7u8; 32];
        encrypt_file(&plain, &enc, &key).unwrap();
        decrypt_file(&enc, &out, &key).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data, "len {len}");
        assert_eq!(sha256_file(&plain).unwrap(), sha256_file(&out).unwrap());
    }

    #[test]
    fn round_trips_empty_partial_and_exact_frame_sizes() {
        for len in [0, 1, FRAME - 1, FRAME, FRAME + 1, 3 * FRAME] {
            roundtrip(len);
        }
    }

    fn encrypted(len: usize) -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let (plain, enc) = (dir.path().join("p"), dir.path().join("e"));
        std::fs::write(&plain, vec![9u8; len]).unwrap();
        encrypt_file(&plain, &enc, &[1u8; 32]).unwrap();
        let bytes = std::fs::read(&enc).unwrap();
        (dir, enc, bytes)
    }

    #[test]
    fn rejects_wrong_key_tampering_and_truncation() {
        let (dir, enc, bytes) = encrypted(2 * FRAME + 10);
        let out = dir.path().join("o");
        assert!(decrypt_file(&enc, &out, &[2u8; 32]).is_err(), "wrong key");

        let mut flipped = bytes.clone();
        flipped[40] ^= 1;
        std::fs::write(&enc, &flipped).unwrap();
        assert!(decrypt_file(&enc, &out, &[1u8; 32]).is_err(), "bit flip");

        // Drop the final frame: what remains ends on a whole frame that was
        // sealed as non-final, so it must not verify as complete.
        let first_two = 16 + 2 * (4 + FRAME + TAG);
        std::fs::write(&enc, &bytes[..first_two]).unwrap();
        let err = decrypt_file(&enc, &out, &[1u8; 32]).unwrap_err();
        assert!(err.to_string().contains("authentication"), "{err}");

        std::fs::write(&enc, &bytes[..bytes.len() - 3]).unwrap();
        assert!(
            decrypt_file(&enc, &out, &[1u8; 32]).is_err(),
            "cut mid-frame"
        );
    }

    #[test]
    fn parses_base64_keys_of_the_right_length() {
        let k = base64::engine::general_purpose::STANDARD.encode([5u8; 32]);
        assert_eq!(parse_key(&k).unwrap(), [5u8; 32]);
        let short = base64::engine::general_purpose::STANDARD.encode([5u8; 16]);
        assert!(parse_key(&short).is_err());
        assert!(parse_key("not base64!").is_err());
    }
}
