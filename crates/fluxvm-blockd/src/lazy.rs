// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Read-through base image over HTTP(S) range requests with an on-disk cache.
//!
//! The remote image is fetched in 1 MiB chunks on first read and stored in a
//! sparse cache file (`<cache>.data`); a persisted bitmap (`<cache>.map`)
//! records which chunks are present so a restart keeps the cache. Chunks are
//! written and `fdatasync`ed before their bit is set, so a set bit always
//! points at complete data.
//!
//! Failure model: a failed or truncated range request (after the configured
//! retries) surfaces as an `io::Error` from `read_at`, which the NBD layer
//! maps to `EIO`. Requests carry a timeout, so a stalled server cannot hang a
//! reader forever.
//!
//! Integrity: if an expected whole-image SHA-256 is configured, it is checked
//! once every chunk is cached. On mismatch the source becomes permanently
//! failed (all reads return an error) rather than serving unverified bytes
//! after that point. Bytes read *before* the cache completed were not
//! verified; this is a completeness check, not per-read authentication.
//!
//! Assumes the remote image is immutable for the life of the cache and that
//! the server honours `Range` (206); servers answering 200 are rejected.

use crate::backend::{BlockBackend, oob};
use crate::seams::LazySource;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

/// Fetch and cache granularity.
pub const CHUNK: u64 = 1 << 20;
const MAP_MAGIC: &[u8; 8] = b"FLXLZY01";
const MAP_HDR: u64 = 64;
const STRIPES: usize = 64;
const MAX_QUEUE: usize = 4096;

#[derive(Clone, Debug)]
pub struct LazyConfig {
    /// Max parallel chunk fetches (per multi-chunk read and for prefetch).
    pub parallel: usize,
    /// Optional expected SHA-256 of the whole image.
    pub sha256: Option<[u8; 32]>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Extra attempts after a failed chunk request.
    pub retries: u32,
}

impl Default for LazyConfig {
    fn default() -> Self {
        Self { parallel: 4, sha256: None, timeout: Duration::from_secs(30), retries: 2 }
    }
}

fn other(m: impl Into<String>) -> io::Error {
    io::Error::other(m.into())
}

struct PrefetchQ {
    q: VecDeque<u64>,
    active: usize,
}

pub struct LazyHttp {
    url: String,
    size: u64,
    nchunks: u64,
    cfg: LazyConfig,
    client: Option<reqwest::blocking::Client>,
    data: File,
    map: File,
    bits: RwLock<Vec<u8>>,
    have: AtomicU64,
    stripes: Vec<Mutex<()>>,
    /// Set once the whole-image hash mismatched.
    failed: AtomicBool,
    verified: AtomicBool,
    me: Weak<LazyHttp>,
    pq: Mutex<PrefetchQ>,
    fetches: AtomicUsize,
}

impl Drop for LazyHttp {
    fn drop(&mut self) {
        // reqwest's blocking client must not be dropped on an async worker.
        if let Some(c) = self.client.take() {
            let _ = std::thread::spawn(move || drop(c)).join();
        }
    }
}

fn cache_paths(cache: &Path) -> (PathBuf, PathBuf) {
    let s = cache.as_os_str().to_owned();
    let (mut d, mut m) = (s.clone(), s);
    d.push(".data");
    m.push(".map");
    (PathBuf::from(d), PathBuf::from(m))
}

impl LazyHttp {
    /// Open (or initialise) the cache at `cache` (`.data`/`.map` suffixes are
    /// appended) and learn the remote size with a 1-byte range request.
    pub fn open(url: &str, cache: &Path, cfg: LazyConfig) -> io::Result<Arc<Self>> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(other("lazy source url must be http(s)"));
        }
        let timeout = cfg.timeout;
        let client = std::thread::spawn(move || {
            reqwest::blocking::Client::builder()
                .timeout(timeout)
                .connect_timeout(timeout)
                .build()
        })
        .join()
        .map_err(|_| other("http client init panicked"))?
        .map_err(|e| other(format!("http client: {e}")))?;
        let size = probe_size(&client, url)?;
        let nchunks = size.div_ceil(CHUNK);
        let (dp, mp) = cache_paths(cache);
        let rw = || {
            let mut o = OpenOptions::new();
            o.read(true).write(true).create(true);
            o
        };
        let data = rw().truncate(false).open(&dp)?;
        let map = rw().truncate(false).open(&mp)?;
        let bm_len = nchunks.div_ceil(8) as usize;
        let url_hash: [u8; 32] = Sha256::digest(url.as_bytes()).into();
        let mut hdr = [0u8; MAP_HDR as usize];
        hdr[0..8].copy_from_slice(MAP_MAGIC);
        hdr[8..16].copy_from_slice(&size.to_le_bytes());
        hdr[16..24].copy_from_slice(&CHUNK.to_le_bytes());
        hdr[24..56].copy_from_slice(&url_hash);
        let mut cur = vec![0u8; MAP_HDR as usize];
        let valid = map.read_exact_at(&mut cur, 0).is_ok()
            && cur == hdr
            && map.metadata()?.len() >= MAP_HDR + bm_len as u64
            && data.metadata()?.len() == size;
        let mut bits = vec![0u8; bm_len];
        if valid {
            map.read_exact_at(&mut bits, MAP_HDR)?;
        } else {
            // New or mismatching cache: start from scratch.
            data.set_len(0)?;
            data.set_len(size)?;
            map.set_len(0)?;
            map.write_all_at(&hdr, 0)?;
            map.write_all_at(&bits, MAP_HDR)?;
        }
        let have: u64 = (0..nchunks).filter(|c| bits[(c / 8) as usize] & (1 << (c % 8)) != 0).count() as u64;
        let this = Arc::new_cyclic(|me| Self {
            url: url.to_string(),
            size,
            nchunks,
            cfg: LazyConfig { parallel: cfg.parallel.max(1), ..cfg },
            client: Some(client),
            data,
            map,
            bits: RwLock::new(bits),
            have: AtomicU64::new(have),
            stripes: (0..STRIPES).map(|_| Mutex::new(())).collect(),
            failed: AtomicBool::new(false),
            verified: AtomicBool::new(false),
            me: me.clone(),
            pq: Mutex::new(PrefetchQ { q: VecDeque::new(), active: 0 }),
            fetches: AtomicUsize::new(0),
        });
        if have == nchunks {
            this.finish_if_complete()?;
        }
        Ok(this)
    }

    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn total_chunks(&self) -> u64 {
        self.nchunks
    }
    pub fn cached_chunks(&self) -> u64 {
        self.have.load(Ordering::SeqCst)
    }
    pub fn is_complete(&self) -> bool {
        self.cached_chunks() == self.nchunks
    }
    /// Number of HTTP chunk requests issued by this instance (incl. retries).
    pub fn http_fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
    /// `Some(true)` once the configured sha256 matched, `Some(false)` after a
    /// mismatch, `None` while incomplete or no hash was configured.
    pub fn verified(&self) -> Option<bool> {
        if self.failed.load(Ordering::SeqCst) {
            Some(false)
        } else if self.verified.load(Ordering::SeqCst) {
            Some(true)
        } else {
            None
        }
    }

    fn has(&self, c: u64) -> bool {
        self.bits.read().unwrap()[(c / 8) as usize] & (1 << (c % 8)) != 0
    }

    fn chunk_range(&self, c: u64) -> (u64, u64) {
        let s = c * CHUNK;
        (s, (s + CHUNK).min(self.size))
    }

    fn try_fetch(&self, c: u64) -> io::Result<Vec<u8>> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        let (s, e) = self.chunk_range(c);
        let client = self.client.as_ref().expect("client present");
        let resp = client
            .get(&self.url)
            .header("Range", format!("bytes={}-{}", s, e - 1))
            .send()
            .map_err(|e| other(format!("range request failed: {e}")))?;
        if resp.status().as_u16() != 206 {
            return Err(other(format!("range request returned HTTP {}", resp.status())));
        }
        let body = resp.bytes().map_err(|e| other(format!("body read failed: {e}")))?;
        if body.len() as u64 != e - s {
            return Err(other(format!("short body: {} of {} bytes", body.len(), e - s)));
        }
        Ok(body.to_vec())
    }

    fn ensure(&self, c: u64) -> io::Result<()> {
        if self.failed.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "lazy source sha256 mismatch"));
        }
        if self.has(c) {
            return Ok(());
        }
        let _g = self.stripes[(c as usize) % STRIPES].lock().unwrap();
        if self.has(c) {
            return Ok(());
        }
        let mut last = None;
        let mut body = None;
        for _ in 0..=self.cfg.retries {
            match self.try_fetch(c) {
                Ok(b) => {
                    body = Some(b);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let body = match body {
            Some(b) => b,
            None => return Err(last.unwrap_or_else(|| other("fetch failed"))),
        };
        let (s, _) = self.chunk_range(c);
        self.data.write_all_at(&body, s)?;
        self.data.sync_data()?;
        {
            let mut bm = self.bits.write().unwrap();
            let i = (c / 8) as usize;
            bm[i] |= 1 << (c % 8);
            self.map.write_all_at(&[bm[i]], MAP_HDR + i as u64)?;
        }
        let n = self.have.fetch_add(1, Ordering::SeqCst) + 1;
        drop(_g);
        if n == self.nchunks {
            self.finish_if_complete()?;
        }
        Ok(())
    }

    fn finish_if_complete(&self) -> io::Result<()> {
        let Some(want) = self.cfg.sha256 else {
            return Ok(());
        };
        let mut h = Sha256::new();
        let mut buf = vec![0u8; CHUNK as usize];
        let mut off = 0;
        while off < self.size {
            let n = ((self.size - off) as usize).min(buf.len());
            self.data.read_exact_at(&mut buf[..n], off)?;
            h.update(&buf[..n]);
            off += n as u64;
        }
        let got: [u8; 32] = h.finalize().into();
        if got == want {
            self.verified.store(true, Ordering::SeqCst);
            Ok(())
        } else {
            self.failed.store(true, Ordering::SeqCst);
            Err(io::Error::new(io::ErrorKind::InvalidData, "lazy source sha256 mismatch"))
        }
    }

    fn fetch_many(&self, missing: &[u64]) -> io::Result<()> {
        let workers = self.cfg.parallel.min(missing.len());
        if workers <= 1 {
            for &c in missing {
                self.ensure(c)?;
            }
            return Ok(());
        }
        let next = AtomicUsize::new(0);
        let err: Mutex<Option<io::Error>> = Mutex::new(None);
        std::thread::scope(|sc| {
            for _ in 0..workers {
                sc.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::SeqCst);
                        if i >= missing.len() || err.lock().unwrap().is_some() {
                            return;
                        }
                        if let Err(e) = self.ensure(missing[i]) {
                            err.lock().unwrap().get_or_insert(e);
                            return;
                        }
                    }
                });
            }
        });
        match err.into_inner().unwrap() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn probe_size(client: &reqwest::blocking::Client, url: &str) -> io::Result<u64> {
    let resp = client
        .get(url)
        .header("Range", "bytes=0-0")
        .send()
        .map_err(|e| other(format!("probe failed: {e}")))?;
    if resp.status().as_u16() != 206 {
        return Err(other(format!(
            "server does not support range requests (HTTP {})",
            resp.status()
        )));
    }
    let cr = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| other("missing Content-Range"))?;
    let total = cr
        .rsplit('/')
        .next()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .ok_or_else(|| other("unparseable Content-Range"))?;
    if total == 0 {
        return Err(other("remote image is empty"));
    }
    Ok(total)
}

impl BlockBackend for LazyHttp {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        if off.checked_add(buf.len() as u64).is_none_or(|e| e > self.size) {
            return Err(oob());
        }
        if buf.is_empty() {
            return Ok(());
        }
        let first = off / CHUNK;
        let last = (off + buf.len() as u64 - 1) / CHUNK;
        let missing: Vec<u64> = (first..=last).filter(|c| !self.has(*c)).collect();
        if self.failed.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "lazy source sha256 mismatch"));
        }
        if !missing.is_empty() {
            self.fetch_many(&missing)?;
        }
        self.data.read_exact_at(buf, off)
    }

    fn write_at(&self, _off: u64, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "lazy source is read-only"))
    }

    fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    fn write_zeroes(&self, _off: u64, _len: u64) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "lazy source is read-only"))
    }
}

impl LazySource for LazyHttp {
    fn prefetch(&self, off: u64, len: u64) {
        if len == 0 || off >= self.size {
            return;
        }
        let end = off.saturating_add(len).min(self.size);
        let (first, last) = (off / CHUNK, (end - 1) / CHUNK);
        let Some(me) = self.me.upgrade() else { return };
        let mut pq = self.pq.lock().unwrap();
        for c in first..=last {
            if !self.has(c) && pq.q.len() < MAX_QUEUE {
                pq.q.push_back(c);
            }
        }
        let want = self.cfg.parallel.saturating_sub(pq.active).min(pq.q.len());
        for _ in 0..want {
            pq.active += 1;
            let me = me.clone();
            std::thread::spawn(move || {
                loop {
                    let c = {
                        let mut pq = me.pq.lock().unwrap();
                        match pq.q.pop_front() {
                            Some(c) => c,
                            None => {
                                pq.active -= 1;
                                return;
                            }
                        }
                    };
                    let _ = me.ensure(c); // best effort
                }
            });
        }
    }
}
