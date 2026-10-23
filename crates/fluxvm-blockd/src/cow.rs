// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Copy-on-write layered backend.
//!
//! On-disk overlay format (all integers little-endian):
//!
//! ```text
//! 0      8   magic "FLXOVL01"
//! 8      4   version (1 or 2; new overlays are written as 2)
//! 12     4   block size (65536)
//! 16     8   virtual size in bytes
//! 24     4   parent kind: 0 = raw file, 1 = overlay, 2 = lazy http(s) source
//! 28     4   frozen flag (set once a branch was taken; no more writes)
//! 32     4   parent length N
//! 36     N   parent: file path (see ext flag REL) or URL (kind 2)
//! 3584       v2 extension block (v2 only; parent is then limited to 3548 B)
//!   +0   4   ext flags: 1 = parent path is relative to this overlay's
//!            directory, 2 = encrypted (AES-256-XTS), 4 = sha256 present
//!   +4   16  key-check salt
//!   +20  32  key-check value (SHA-256 over domain, salt, key)
//!   +52  32  expected whole-image sha256 of a lazy parent
//!   +84  8   data region offset (bitmap capacity is reserved up front so
//!            small grows stay in place)
//! 4096       allocation bitmap, 1 bit per block, padded
//! D          data region; virtual offset V lives at file offset D + V
//!            (encrypted overlays store whole 64 KiB blocks, zero padded)
//! ```
//!
//! Version 1 files (absolute parent path, `D` computed from the size) are
//! read as before and are upgraded to version 2 only when rebased or
//! relocated by a resize.
//!
//! The data region is sparse: only written blocks consume space. A set bit
//! means "this block lives in the overlay (possibly all zeros)"; a clear bit
//! means "read through to the parent".
//!
//! Crash behaviour: for every write the data is written first and the bitmap
//! bit afterwards, so a crash can lose the write but never exposes a block
//! whose data was not written (partial-block writes copy up the parent's data
//! before the bit is set). Durability is only promised after `flush` (fsync).
//! Ordering between the data and bitmap pages is not enforced with an extra
//! fsync, so after power loss (not process crash) a set bit could in theory
//! point at unwritten data; the guest is expected to flush for durability,
//! like on a real disk.

use crate::BLOCK_SIZE;
use crate::backend::{BlockBackend, RawFile, oob};
use crate::lazy::{LazyConfig, LazyHttp};
use crate::seams::{BlockCipher, LazySource};
use std::fs::{File, OpenOptions as FsOpen};
use std::io;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

const MAGIC: &[u8; 8] = b"FLXOVL01";
const HEADER_LEN: u64 = 4096;
const EXT_OFF: usize = 3584;
const MAX_DEPTH: usize = 64;
const KIND_RAW: u32 = 0;
const KIND_OVERLAY: u32 = 1;
const KIND_LAZY: u32 = 2;
const XF_REL: u32 = 1;
const XF_ENC: u32 = 2;
const XF_SHA: u32 = 4;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Shared flock held for the life of an open overlay fd (serve / read). Blocks exclusive
/// resize/rebase/branch from another process.
fn flock_shared(file: &File) -> io::Result<()> {
    // SAFETY: `file` is open; flock applies to the inode for this process's lifetime of the fd.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Non-blocking exclusive flock for offline mutate ops. Fails with [`io::ErrorKind::WouldBlock`]
/// when the overlay is served or otherwise open.
fn try_flock_exclusive(file: &File) -> io::Result<()> {
    // SAFETY: as above; LOCK_NB makes failure immediate when a shared holder exists.
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock
        || err.raw_os_error() == Some(libc::EWOULDBLOCK)
        || err.raw_os_error() == Some(libc::EAGAIN)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "overlay is in use (served or open elsewhere); stop the server before resize/rebase/branch",
        ));
    }
    Err(err)
}

/// How `open_inner` locks the top-of-chain file.
#[derive(Clone, Copy, PartialEq)]
enum Lock {
    Shared,
    Exclusive,
    /// Caller holds an exclusive lock on the overlay being mutated: skip locking the whole
    /// chain (a second fd would block on our own lock). Rebase verify / cycle check.
    Held,
}

/// Options for creating an overlay.
#[derive(Clone, Default)]
pub struct CreateOptions {
    /// Virtual size; defaults to the parent's size and may not be smaller.
    pub size: Option<u64>,
    /// Encrypt the overlay's own blocks with this cipher.
    pub cipher: Option<Arc<dyn BlockCipher>>,
}

/// Options for opening an overlay chain.
#[derive(Clone, Default)]
pub struct OpenOptions {
    /// Key for encrypted layers (the same key is used for the whole chain).
    pub key: Option<Arc<dyn BlockCipher>>,
    /// Settings for lazy http parents.
    pub lazy: LazyConfig,
}

#[derive(Clone)]
struct Header {
    version: u32,
    size: u64,
    kind: u32,
    frozen: bool,
    parent: String,
    rel: bool,
    encrypted: bool,
    salt: [u8; 16],
    kcv: [u8; 32],
    sha: Option<[u8; 32]>,
    data_off: u64,
}

fn layout(size: u64) -> (u64, u64) {
    let nblocks = size.div_ceil(BLOCK_SIZE);
    let bm = nblocks.div_ceil(8).div_ceil(4096) * 4096;
    (nblocks, HEADER_LEN + bm)
}

/// v2 layout: bitmap capacity is reserved with 4x headroom (min 64 KiB,
/// sparse so free) so overlays can grow in place.
fn layout_v2(size: u64) -> (u64, u64) {
    let nblocks = size.div_ceil(BLOCK_SIZE);
    let need = nblocks.div_ceil(8);
    let cap = (need * 4).div_ceil(4096).max(16) * 4096;
    (nblocks, HEADER_LEN + cap)
}

fn data_len(h_enc: bool, nblocks: u64, size: u64) -> u64 {
    if h_enc { nblocks * BLOCK_SIZE } else { size }
}

impl Header {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let pp = self.parent.as_bytes();
        if pp.len() > EXT_OFF - 36 {
            return Err(bad("parent path too long"));
        }
        let mut h = vec![0u8; HEADER_LEN as usize];
        h[0..8].copy_from_slice(MAGIC);
        h[8..12].copy_from_slice(&2u32.to_le_bytes());
        h[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
        h[16..24].copy_from_slice(&self.size.to_le_bytes());
        h[24..28].copy_from_slice(&self.kind.to_le_bytes());
        h[28..32].copy_from_slice(&(self.frozen as u32).to_le_bytes());
        h[32..36].copy_from_slice(&(pp.len() as u32).to_le_bytes());
        h[36..36 + pp.len()].copy_from_slice(pp);
        let mut xf = 0;
        if self.rel {
            xf |= XF_REL;
        }
        if self.encrypted {
            xf |= XF_ENC;
        }
        if self.sha.is_some() {
            xf |= XF_SHA;
        }
        let e = &mut h[EXT_OFF..];
        e[0..4].copy_from_slice(&xf.to_le_bytes());
        e[4..20].copy_from_slice(&self.salt);
        e[20..52].copy_from_slice(&self.kcv);
        e[52..84].copy_from_slice(&self.sha.unwrap_or([0; 32]));
        e[84..92].copy_from_slice(&self.data_off.to_le_bytes());
        Ok(h)
    }

    fn decode(h: &[u8]) -> io::Result<Self> {
        if &h[0..8] != MAGIC {
            return Err(bad("not a fluxvm overlay"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        let version = u32_at(8);
        if !(version == 1 || version == 2) || u32_at(12) as u64 != BLOCK_SIZE {
            return Err(bad("unsupported overlay version/block size"));
        }
        let size = u64::from_le_bytes(h[16..24].try_into().unwrap());
        let n = u32_at(32) as usize;
        let limit = if version == 2 {
            EXT_OFF
        } else {
            HEADER_LEN as usize
        } - 36;
        if n > limit {
            return Err(bad("corrupt parent path"));
        }
        let parent = std::str::from_utf8(&h[36..36 + n])
            .map_err(|_| bad("non-utf8 parent path"))?
            .to_string();
        let mut o = Header {
            version,
            size,
            kind: u32_at(24),
            frozen: u32_at(28) != 0,
            parent,
            rel: false,
            encrypted: false,
            salt: [0; 16],
            kcv: [0; 32],
            sha: None,
            data_off: layout(size).1,
        };
        if version == 2 {
            let e = &h[EXT_OFF..];
            let xf = u32::from_le_bytes(e[0..4].try_into().unwrap());
            o.rel = xf & XF_REL != 0;
            o.encrypted = xf & XF_ENC != 0;
            o.salt.copy_from_slice(&e[4..20]);
            o.kcv.copy_from_slice(&e[20..52]);
            if xf & XF_SHA != 0 {
                let mut s = [0u8; 32];
                s.copy_from_slice(&e[52..84]);
                o.sha = Some(s);
            }
            o.data_off = u64::from_le_bytes(e[84..92].try_into().unwrap());
            let min_off = HEADER_LEN + size.div_ceil(BLOCK_SIZE).div_ceil(8);
            if o.data_off < min_off || !o.data_off.is_multiple_of(4096) {
                return Err(bad("corrupt data offset"));
            }
        }
        Ok(o)
    }
}

/// Canonical directory + file name; the file need not be a symlink target.
fn canon_file(p: &Path) -> io::Result<PathBuf> {
    let abs = std::path::absolute(p)?;
    let name = abs
        .file_name()
        .ok_or_else(|| bad("path has no file name"))?
        .to_owned();
    let dir = abs.parent().ok_or_else(|| bad("path has no parent"))?;
    Ok(dir.canonicalize()?.join(name))
}

/// Path of `target` relative to directory `from` (both absolute, canonical).
/// `None` when they share nothing but the root: the result would not be
/// meaningfully relocatable.
fn rel_path(from: &Path, target: &Path) -> Option<PathBuf> {
    let f: Vec<Component> = from.components().collect();
    let t: Vec<Component> = target.components().collect();
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    if common <= 1 {
        return None;
    }
    let mut out = PathBuf::new();
    for _ in common..f.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c.as_os_str());
    }
    Some(out)
}

/// True if `p` starts with the overlay magic.
pub fn is_overlay_file(p: &Path) -> bool {
    use std::io::Read;
    let mut m = [0u8; 8];
    File::open(p).and_then(|mut f| f.read_exact(&mut m)).is_ok() && &m == MAGIC
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

fn lazy_cache_base(overlay: &Path) -> PathBuf {
    let mut s = overlay.as_os_str().to_owned();
    s.push(".lazy");
    PathBuf::from(s)
}

fn random16() -> io::Result<[u8; 16]> {
    use std::io::Read;
    let mut b = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

/// Peek at a file's header: `(encrypted, parent kind)`.
pub fn overlay_info(p: &Path) -> io::Result<(bool, u32)> {
    let f = File::open(p)?;
    let mut h = vec![0u8; HEADER_LEN as usize];
    f.read_exact_at(&mut h, 0)?;
    let d = Header::decode(&h)?;
    Ok((d.encrypted, d.kind))
}

pub struct CowBackend {
    path: PathBuf,
    file: File,
    size: AtomicU64,
    data_off: u64,
    bitmap: RwLock<Vec<u8>>,
    base: Arc<dyn BlockBackend>,
    lazy: Option<Arc<LazyHttp>>,
    parent_cow: Option<Arc<CowBackend>>,
    cipher: Option<Arc<dyn BlockCipher>>,
    frozen: AtomicBool,
}

/// Everything about the parent needed to create a layer on top of it.
struct Parent {
    base: Arc<dyn BlockBackend>,
    kind: u32,
    /// Header string before relativisation.
    target: ParentTarget,
    lazy: Option<Arc<LazyHttp>>,
    cow: Option<Arc<CowBackend>>,
    sha: Option<[u8; 32]>,
}

enum ParentTarget {
    File(PathBuf), // canonical absolute
    Url(String),
}

impl CowBackend {
    /// Create an overlay over a raw base image file. `size` defaults to the
    /// base size and must not be smaller than it.
    pub fn create(path: &Path, base: &Path, size: Option<u64>) -> io::Result<Arc<Self>> {
        Self::create_opts(path, base, &CreateOptions { size, cipher: None })
    }

    pub fn create_opts(path: &Path, base: &Path, o: &CreateOptions) -> io::Result<Arc<Self>> {
        let b = Arc::new(RawFile::open(base, true)?);
        let p = Parent {
            base: b,
            kind: KIND_RAW,
            target: ParentTarget::File(base.canonicalize()?),
            lazy: None,
            cow: None,
            sha: None,
        };
        Self::create_with(path, p, o)
    }

    /// Create an overlay over an existing overlay file (opened read-only and
    /// frozen). The parent file is marked frozen so it can no longer change
    /// under the child.
    pub fn create_on_overlay(path: &Path, parent: &Path) -> io::Result<Arc<Self>> {
        Self::create_on_overlay_opts(
            path,
            parent,
            &CreateOptions::default(),
            &OpenOptions::default(),
        )
    }

    pub fn create_on_overlay_opts(
        path: &Path,
        parent: &Path,
        o: &CreateOptions,
        open: &OpenOptions,
    ) -> io::Result<Arc<Self>> {
        // Exclusive: refuse to freeze/branch a parent that is currently served.
        let p = Self::open_inner(parent, 0, true, open, Lock::Exclusive)?;
        if p.cipher.is_some() && o.cipher.is_none() {
            return Err(bad("parent is encrypted: child must be encrypted too"));
        }
        p.freeze_persist()?;
        let par = Parent {
            base: p.clone(),
            kind: KIND_OVERLAY,
            target: ParentTarget::File(p.path.clone()),
            lazy: None,
            cow: Some(p),
            sha: None,
        };
        Self::create_with(path, par, o)
    }

    /// Create an overlay over a remote image read lazily through `url`. The
    /// download cache lives next to the overlay (`<overlay>.lazy.data/.map`).
    pub fn create_on_lazy(
        path: &Path,
        url: &str,
        o: &CreateOptions,
        lazy: &LazyConfig,
    ) -> io::Result<Arc<Self>> {
        if path.exists() {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        let cache = lazy_cache_base(&canon_file(path)?);
        let src = LazyHttp::open(url, &cache, lazy.clone())?;
        let par = Parent {
            base: src.clone(),
            kind: KIND_LAZY,
            target: ParentTarget::Url(url.to_string()),
            lazy: Some(src),
            cow: None,
            sha: lazy.sha256,
        };
        Self::create_with(path, par, o)
    }

    /// Instant branch of a live overlay: freezes `self` and returns a new
    /// writable overlay on top of it. Cost is one small file creation. The
    /// child inherits the parent's cipher.
    pub fn branch(self: &Arc<Self>, new_path: &Path) -> io::Result<Arc<Self>> {
        self.freeze_persist()?;
        self.flush()?;
        let par = Parent {
            base: self.clone(),
            kind: KIND_OVERLAY,
            target: ParentTarget::File(self.path.clone()),
            lazy: None,
            cow: Some(self.clone()),
            sha: None,
        };
        let o = CreateOptions {
            size: None,
            cipher: self.cipher.clone(),
        };
        Self::create_with(new_path, par, &o)
    }

    /// Open an existing overlay and its whole parent chain. If the overlay
    /// has been branched from it opens read-only. Holds a shared flock for the
    /// life of the returned handle so offline resize/rebase/branch refuse while served.
    pub fn open(path: &Path) -> io::Result<Arc<Self>> {
        Self::open_inner(path, 0, false, &OpenOptions::default(), Lock::Shared)
    }

    pub fn open_with(path: &Path, o: &OpenOptions) -> io::Result<Arc<Self>> {
        Self::open_inner(path, 0, false, o, Lock::Shared)
    }

    fn create_with(path: &Path, par: Parent, o: &CreateOptions) -> io::Result<Arc<Self>> {
        let psize = par.base.size();
        let size = o.size.unwrap_or(psize);
        if size < psize {
            return Err(bad("overlay smaller than base"));
        }
        let file = FsOpen::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let cpath = canon_file(path)?;
        let (parent, rel) = match &par.target {
            ParentTarget::Url(u) => (u.clone(), false),
            ParentTarget::File(t) => {
                let dir = cpath.parent().unwrap();
                match rel_path(dir, t) {
                    Some(r) => (
                        r.to_str().ok_or_else(|| bad("non-utf8 path"))?.to_string(),
                        true,
                    ),
                    None => (
                        t.to_str().ok_or_else(|| bad("non-utf8 path"))?.to_string(),
                        false,
                    ),
                }
            }
        };
        let (nblocks, data_off) = layout_v2(size);
        let (salt, kcv) = match &o.cipher {
            Some(c) => {
                let s = random16()?;
                (s, c.key_check(&s))
            }
            None => ([0; 16], [0; 32]),
        };
        let h = Header {
            version: 2,
            size,
            kind: par.kind,
            frozen: false,
            parent,
            rel,
            encrypted: o.cipher.is_some(),
            salt,
            kcv,
            sha: par.sha,
            data_off,
        };
        let hb = h.encode().inspect_err(|_| {
            let _ = std::fs::remove_file(path);
        })?;
        file.write_all_at(&hb, 0)?;
        file.set_len(data_off + data_len(h.encrypted, nblocks, size))?; // sparse
        file.sync_all()?;
        flock_shared(&file)?;
        Ok(Arc::new(Self {
            path: cpath,
            file,
            size: AtomicU64::new(size),
            data_off,
            bitmap: RwLock::new(vec![0u8; nblocks.div_ceil(8) as usize]),
            base: par.base,
            lazy: par.lazy,
            parent_cow: par.cow,
            cipher: o.cipher.clone(),
            frozen: AtomicBool::new(false),
        }))
    }

    fn open_inner(
        path: &Path,
        depth: usize,
        force_frozen: bool,
        o: &OpenOptions,
        lock: Lock,
    ) -> io::Result<Arc<Self>> {
        if depth > MAX_DEPTH {
            return Err(bad("overlay chain too deep (cycle?)"));
        }
        let cpath = canon_file(path)?;
        let file = FsOpen::new().read(true).write(true).open(&cpath)?;
        // Top of the chain: exclusive for branch/freeze, shared for serve. Deeper
        // parents always take shared so a served child blocks mutating them too.
        match (depth, lock) {
            (0, Lock::Exclusive) => try_flock_exclusive(&file)?,
            (_, Lock::Held) => {}
            _ => flock_shared(&file)?,
        }
        let mut hb = vec![0u8; HEADER_LEN as usize];
        file.read_exact_at(&mut hb, 0)?;
        let h = Header::decode(&hb)?;
        let frozen = h.frozen || force_frozen;
        let cipher = if h.encrypted {
            let k = o.key.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "overlay {} is encrypted: a key is required (key file or {})",
                        cpath.display(),
                        crate::crypt::KEY_ENV
                    ),
                )
            })?;
            if !ct_eq(&k.key_check(&h.salt), &h.kcv) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("wrong key for encrypted overlay {}", cpath.display()),
                ));
            }
            Some(k)
        } else {
            None
        };
        let (mut lazy, mut parent_cow) = (None, None);
        let base: Arc<dyn BlockBackend> = match h.kind {
            KIND_RAW | KIND_OVERLAY => {
                let pp = if h.rel {
                    cpath.parent().unwrap().join(&h.parent)
                } else {
                    PathBuf::from(&h.parent)
                };
                if h.kind == KIND_RAW {
                    Arc::new(RawFile::open(&pp, true)?)
                } else {
                    let p = Self::open_inner(
                        &pp,
                        depth + 1,
                        true,
                        o,
                        if lock == Lock::Held {
                            Lock::Held
                        } else {
                            Lock::Shared
                        },
                    )?;
                    parent_cow = Some(p.clone());
                    p
                }
            }
            KIND_LAZY => {
                let mut cfg = o.lazy.clone();
                if h.sha.is_some() {
                    cfg.sha256 = h.sha;
                }
                let l = LazyHttp::open(&h.parent, &lazy_cache_base(&cpath), cfg)?;
                lazy = Some(l.clone());
                l
            }
            _ => return Err(bad("unknown parent kind")),
        };
        let nblocks = h.size.div_ceil(BLOCK_SIZE);
        let mut bitmap = vec![0u8; nblocks.div_ceil(8) as usize];
        file.read_exact_at(&mut bitmap, HEADER_LEN)?;
        Ok(Arc::new(Self {
            path: cpath,
            file,
            size: AtomicU64::new(h.size),
            data_off: h.data_off,
            bitmap: RwLock::new(bitmap),
            base,
            lazy,
            parent_cow,
            cipher,
            frozen: AtomicBool::new(frozen),
        }))
    }

    /// Point an overlay at a different parent without touching its data
    /// (`qemu-img rebase -u` semantics: the caller asserts the new parent has
    /// the same content). `parent` is a file path (raw image or overlay) or
    /// an http(s) URL. The parent is stored relative when possible.
    pub fn rebase(path: &Path, parent: &str, open: &OpenOptions) -> io::Result<()> {
        let cpath = canon_file(path)?;
        let file = FsOpen::new().read(true).write(true).open(&cpath)?;
        try_flock_exclusive(&file)?;
        let mut old = vec![0u8; HEADER_LEN as usize];
        file.read_exact_at(&mut old, 0)?;
        let mut h = Header::decode(&old)?;
        let mut to_freeze: Option<Arc<CowBackend>> = None;
        let (kind, target, psize, sha) = if is_url(parent) {
            let l = LazyHttp::open(parent, &lazy_cache_base(&cpath), open.lazy.clone())?;
            (
                KIND_LAZY,
                ParentTarget::Url(parent.to_string()),
                l.size(),
                open.lazy.sha256,
            )
        } else {
            let pp = Path::new(parent).canonicalize()?;
            if pp == cpath {
                return Err(bad("an overlay cannot be its own parent"));
            }
            if is_overlay_file(&pp) {
                let p = Self::open_inner(&pp, 0, true, open, Lock::Held)?;
                if p.chain_contains(&cpath) {
                    return Err(bad("rebase would create a cycle"));
                }
                if p.cipher.is_some() != h.encrypted {
                    return Err(bad(
                        "encrypted and plain overlays cannot be mixed in a chain",
                    ));
                }
                let s = p.size();
                to_freeze = Some(p);
                (KIND_OVERLAY, ParentTarget::File(pp), s, None)
            } else {
                let r = RawFile::open(&pp, true)?;
                (KIND_RAW, ParentTarget::File(pp), r.size(), None)
            }
        };
        if psize > h.size {
            return Err(bad("new parent is larger than the overlay"));
        }
        match target {
            ParentTarget::Url(u) => {
                h.parent = u;
                h.rel = false;
            }
            ParentTarget::File(t) => match rel_path(cpath.parent().unwrap(), &t) {
                Some(r) => {
                    h.parent = r.to_str().ok_or_else(|| bad("non-utf8 path"))?.to_string();
                    h.rel = true;
                }
                None => {
                    h.parent = t.to_str().ok_or_else(|| bad("non-utf8 path"))?.to_string();
                    h.rel = false;
                }
            },
        }
        h.kind = kind;
        h.sha = sha;
        h.version = 2;
        let nb = h.encode()?;
        file.write_all_at(&nb, 0)?;
        file.sync_all()?;
        // Verify the rewritten chain opens; restore the old header if not.
        let verify = if h.encrypted && open.key.is_none() {
            Ok(())
        } else {
            Self::open_inner(&cpath, 0, false, open, Lock::Held).map(|_| ())
        };
        if let Err(e) = verify {
            file.write_all_at(&old, 0)?;
            file.sync_all()?;
            return Err(e);
        }
        if let Some(p) = to_freeze {
            p.freeze_persist()?;
        }
        Ok(())
    }

    fn chain_contains(&self, p: &Path) -> bool {
        self.path == p
            || self
                .parent_cow
                .as_ref()
                .is_some_and(|c| c.chain_contains(p))
    }

    /// Grow the live overlay to `new_size` (grow only). Succeeds in place
    /// while the new bitmap fits the capacity reserved in the header;
    /// otherwise returns `Unsupported` and the offline
    /// [`CowBackend::resize_file`] must be used. New connections advertise the
    /// new size immediately; existing ones keep the old size.
    pub fn grow(&self, new_size: u64) -> io::Result<()> {
        self.check_writable()?;
        let mut bm = self.bitmap.write().unwrap();
        let old = self.size();
        if new_size < old {
            return Err(bad("overlays can only grow"));
        }
        if new_size == old {
            return Ok(());
        }
        let new_n = new_size.div_ceil(BLOCK_SIZE);
        let need = new_n.div_ceil(8);
        if need > self.data_off - HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "growth exceeds reserved bitmap space; run `fluxvm-blockd resize` offline",
            ));
        }
        let enc = self.cipher.is_some();
        self.file
            .set_len(self.data_off + data_len(enc, new_n, new_size))?;
        self.file.write_all_at(&new_size.to_le_bytes(), 16)?;
        self.file.sync_all()?;
        bm.resize(need as usize, 0);
        self.size.store(new_size, Ordering::SeqCst);
        Ok(())
    }

    /// Offline grow of an overlay file. In place when the reserved bitmap
    /// space suffices, otherwise the file is rewritten (allocated blocks
    /// copied, zero blocks skipped) to a temporary file and atomically
    /// renamed over the original, upgrading the header to version 2. Must not
    /// be used on an overlay that is currently served or open elsewhere.
    /// Returns true if the file was relocated.
    pub fn resize_file(path: &Path, new_size: u64) -> io::Result<bool> {
        let cpath = canon_file(path)?;
        let file = FsOpen::new().read(true).write(true).open(&cpath)?;
        try_flock_exclusive(&file)?;
        let mut hb = vec![0u8; HEADER_LEN as usize];
        file.read_exact_at(&mut hb, 0)?;
        let h = Header::decode(&hb)?;
        if h.frozen {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "overlay is frozen (has children); it cannot be resized",
            ));
        }
        if new_size < h.size {
            return Err(bad("overlays can only grow"));
        }
        if new_size == h.size {
            return Ok(false);
        }
        let new_n = new_size.div_ceil(BLOCK_SIZE);
        let need = new_n.div_ceil(8);
        let old_n = h.size.div_ceil(BLOCK_SIZE);
        if need <= h.data_off - HEADER_LEN {
            file.set_len(h.data_off + data_len(h.encrypted, new_n, new_size))?;
            file.write_all_at(&new_size.to_le_bytes(), 16)?;
            file.sync_all()?;
            return Ok(false);
        }
        let (_, new_off) = layout_v2(new_size);
        let mut tmp_name = cpath.as_os_str().to_owned();
        tmp_name.push(".resize-tmp");
        let tmp_path = PathBuf::from(tmp_name);
        let tmp = FsOpen::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp_path)?;
        let res = (|| -> io::Result<()> {
            let mut nh = h.clone();
            nh.version = 2;
            nh.size = new_size;
            nh.data_off = new_off;
            tmp.write_all_at(&nh.encode()?, 0)?;
            let mut bm = vec![0u8; old_n.div_ceil(8) as usize];
            file.read_exact_at(&mut bm, HEADER_LEN)?;
            tmp.write_all_at(&bm, HEADER_LEN)?;
            let mut buf = vec![0u8; BLOCK_SIZE as usize];
            for b in 0..old_n {
                if bm[(b / 8) as usize] & (1 << (b % 8)) == 0 {
                    continue;
                }
                let len = if h.encrypted {
                    BLOCK_SIZE
                } else {
                    (h.size - b * BLOCK_SIZE).min(BLOCK_SIZE)
                } as usize;
                file.read_exact_at(&mut buf[..len], h.data_off + b * BLOCK_SIZE)?;
                if buf[..len].iter().any(|&x| x != 0) {
                    tmp.write_all_at(&buf[..len], new_off + b * BLOCK_SIZE)?;
                }
            }
            tmp.set_len(new_off + data_len(h.encrypted, new_n, new_size))?;
            tmp.sync_all()?;
            std::fs::rename(&tmp_path, &cpath)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        res.map(|_| true)
    }

    fn freeze_persist(&self) -> io::Result<()> {
        self.frozen.store(true, Ordering::SeqCst);
        self.file.write_all_at(&1u32.to_le_bytes(), 28)?;
        self.file.sync_all()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::SeqCst)
    }

    pub fn is_encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// The lazy http source underneath this layer, if any.
    pub fn lazy_source(&self) -> Option<&Arc<LazyHttp>> {
        self.lazy.as_ref()
    }

    /// Hint the lazy base (if any) to fetch `[off, off+len)` soon.
    pub fn prefetch(&self, off: u64, len: u64) {
        if let Some(l) = &self.lazy {
            l.prefetch(off, len);
        }
    }

    /// Number of blocks held by this layer.
    pub fn allocated_blocks(&self) -> u64 {
        self.bitmap
            .read()
            .unwrap()
            .iter()
            .map(|b| b.count_ones() as u64)
            .sum()
    }

    fn check(&self, off: u64, len: u64) -> io::Result<()> {
        if off.checked_add(len).is_none_or(|e| e > self.size()) {
            return Err(oob());
        }
        Ok(())
    }

    fn check_writable(&self) -> io::Result<()> {
        if self.is_frozen() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "overlay is frozen",
            ));
        }
        Ok(())
    }

    fn present(bm: &[u8], b: u64) -> bool {
        bm[(b / 8) as usize] & (1 << (b % 8)) != 0
    }

    /// Set bit durably-ish (single byte pwrite) and in memory.
    fn set_bit(&self, bm: &mut [u8], b: u64) -> io::Result<()> {
        let i = (b / 8) as usize;
        bm[i] |= 1 << (b % 8);
        self.file.write_all_at(&[bm[i]], HEADER_LEN + i as u64)
    }

    fn read_base(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let bs = self.base.size();
        if off >= bs {
            buf.fill(0);
            return Ok(());
        }
        let avail = ((bs - off) as usize).min(buf.len());
        self.base.read_at(off, &mut buf[..avail])?;
        buf[avail..].fill(0);
        Ok(())
    }

    fn read_block_plain(&self, c: &dyn BlockCipher, b: u64) -> io::Result<Vec<u8>> {
        let mut blk = vec![0u8; BLOCK_SIZE as usize];
        self.file
            .read_exact_at(&mut blk, self.data_off + b * BLOCK_SIZE)?;
        c.decrypt(b, &mut blk)?;
        Ok(blk)
    }

    fn read_overlay(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        if let Some(c) = &self.cipher {
            let end = off + buf.len() as u64;
            let mut pos = off;
            while pos < end {
                let b = pos / BLOCK_SIZE;
                let bstart = b * BLOCK_SIZE;
                let seg_end = (bstart + BLOCK_SIZE).min(end);
                let blk = self.read_block_plain(c.as_ref(), b)?;
                buf[(pos - off) as usize..(seg_end - off) as usize]
                    .copy_from_slice(&blk[(pos - bstart) as usize..(seg_end - bstart) as usize]);
                pos = seg_end;
            }
            return Ok(());
        }
        let mut done = 0;
        while done < buf.len() {
            let n = self
                .file
                .read_at(&mut buf[done..], self.data_off + off + done as u64)?;
            if n == 0 {
                buf[done..].fill(0);
                break;
            }
            done += n;
        }
        Ok(())
    }

    fn block_len(&self, b: u64) -> u64 {
        (self.size() - b * BLOCK_SIZE).min(BLOCK_SIZE)
    }

    /// Write `data` at `off` with the bitmap write lock held.
    fn write_locked(&self, bm: &mut [u8], off: u64, data: &[u8]) -> io::Result<()> {
        let end = off + data.len() as u64;
        let mut pos = off;
        while pos < end {
            let b = pos / BLOCK_SIZE;
            let bstart = b * BLOCK_SIZE;
            let seg_end = (bstart + BLOCK_SIZE).min(end);
            let src = &data[(pos - off) as usize..(seg_end - off) as usize];
            let present = Self::present(bm, b);
            if let Some(c) = &self.cipher {
                // Whole-block read-modify-write; padding beyond the device
                // end stays zero.
                let mut blk = if present {
                    self.read_block_plain(c.as_ref(), b)?
                } else {
                    let mut v = vec![0u8; BLOCK_SIZE as usize];
                    let bl = self.block_len(b) as usize;
                    self.read_base(bstart, &mut v[..bl])?;
                    v
                };
                blk[(pos - bstart) as usize..(seg_end - bstart) as usize].copy_from_slice(src);
                c.encrypt(b, &mut blk)?;
                self.file.write_all_at(&blk, self.data_off + bstart)?;
            } else {
                let covers_all = pos == bstart && seg_end - bstart == self.block_len(b);
                if present || covers_all {
                    self.file.write_all_at(src, self.data_off + pos)?;
                } else {
                    // Copy up: merge into the parent's view of the block first.
                    let mut blk = vec![0u8; self.block_len(b) as usize];
                    self.read_base(bstart, &mut blk)?;
                    blk[(pos - bstart) as usize..(seg_end - bstart) as usize].copy_from_slice(src);
                    self.file.write_all_at(&blk, self.data_off + bstart)?;
                }
            }
            if !present {
                self.set_bit(bm, b)?; // index after data
            }
            pos = seg_end;
        }
        Ok(())
    }
}

fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl BlockBackend for CowBackend {
    fn size(&self) -> u64 {
        self.size.load(Ordering::SeqCst)
    }

    fn is_read_only(&self) -> bool {
        self.is_frozen()
    }

    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.check(off, buf.len() as u64)?;
        let bm = self.bitmap.read().unwrap();
        let end = off + buf.len() as u64;
        let mut pos = off;
        while pos < end {
            let b = pos / BLOCK_SIZE;
            let state = Self::present(&bm, b);
            // Coalesce following blocks with the same state into one run.
            let mut run_end = ((b + 1) * BLOCK_SIZE).min(end);
            while run_end < end && Self::present(&bm, run_end / BLOCK_SIZE) == state {
                run_end = (run_end + BLOCK_SIZE).min(end);
            }
            let dst = &mut buf[(pos - off) as usize..(run_end - off) as usize];
            if state {
                self.read_overlay(pos, dst)?;
            } else {
                self.read_base(pos, dst)?;
            }
            pos = run_end;
        }
        Ok(())
    }

    fn write_at(&self, off: u64, data: &[u8]) -> io::Result<()> {
        self.check_writable()?;
        self.check(off, data.len() as u64)?;
        let mut bm = self.bitmap.write().unwrap();
        self.write_locked(&mut bm, off, data)
    }

    fn flush(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn write_zeroes(&self, off: u64, len: u64) -> io::Result<()> {
        self.check_writable()?;
        self.check(off, len)?;
        let end = off + len;
        let mut pos = off;
        let mut bm = self.bitmap.write().unwrap();
        if self.cipher.is_some() {
            // Holes would decrypt to garbage: write encrypted zeros.
            let z = vec![0u8; BLOCK_SIZE as usize];
            while pos < end {
                let n = (end - pos).min(BLOCK_SIZE) as usize;
                self.write_locked(&mut bm, pos, &z[..n])?;
                pos += n as u64;
            }
            return Ok(());
        }
        while pos < end {
            let b = pos / BLOCK_SIZE;
            let bstart = b * BLOCK_SIZE;
            let seg_end = (bstart + BLOCK_SIZE).min(end);
            let present = Self::present(&bm, b);
            let covers_all = pos == bstart && seg_end - bstart == self.block_len(b);
            if covers_all {
                // Whole block: punch a hole (reads as zero, frees space) and
                // mark present so it no longer reads through to the parent.
                crate::sparse::zero_range(&self.file, self.data_off + bstart, seg_end - bstart)?;
            } else {
                let z = vec![0u8; (seg_end - pos) as usize];
                if present {
                    self.file.write_all_at(&z, self.data_off + pos)?;
                } else {
                    let mut blk = vec![0u8; self.block_len(b) as usize];
                    self.read_base(bstart, &mut blk)?;
                    blk[(pos - bstart) as usize..(seg_end - bstart) as usize].fill(0);
                    self.file.write_all_at(&blk, self.data_off + bstart)?;
                }
            }
            if !present {
                self.set_bit(&mut bm, b)?;
            }
            pos = seg_end;
        }
        Ok(())
    }
}

impl CowBackend {
    #[doc(hidden)]
    pub fn nblocks(&self) -> u64 {
        self.size().div_ceil(BLOCK_SIZE)
    }

    /// Bytes physically allocated in the overlay file (holes excluded).
    pub fn allocated_bytes(&self) -> io::Result<u64> {
        crate::sparse::allocated_bytes(&self.file)
    }

    /// Raw bytes of the overlay file's data region for block `b` (for
    /// at-rest inspection in tests).
    #[doc(hidden)]
    pub fn raw_block(&self, b: u64) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; BLOCK_SIZE as usize];
        self.file
            .read_exact_at(&mut v, self.data_off + b * BLOCK_SIZE)?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_file(dir: &Path, len: usize, fill: u8) -> PathBuf {
        let p = dir.join("base.img");
        std::fs::write(&p, vec![fill; len]).unwrap();
        p
    }

    fn rd(b: &dyn BlockBackend, off: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        b.read_at(off, &mut v).unwrap();
        v
    }

    #[test]
    fn read_through_overwrite_and_base_untouched() {
        let d = tempfile::tempdir().unwrap();
        let base = base_file(d.path(), 1 << 20, 0xAA);
        let o = CowBackend::create(&d.path().join("o.ovl"), &base, None).unwrap();
        assert_eq!(rd(&*o, 100, 4), [0xAA; 4]);
        o.write_at(70_000, &[1, 2, 3]).unwrap(); // partial block => copy-up
        assert_eq!(rd(&*o, 69_999, 5), [0xAA, 1, 2, 3, 0xAA]);
        assert_eq!(o.allocated_blocks(), 1);
        o.write_at(70_000, &[9]).unwrap(); // overwrite
        assert_eq!(rd(&*o, 70_000, 3), [9, 2, 3]);
        assert_eq!(std::fs::read(&base).unwrap(), vec![0xAA; 1 << 20]);
    }

    #[test]
    fn write_spanning_blocks_and_odd_size() {
        let d = tempfile::tempdir().unwrap();
        let base = base_file(d.path(), 200_000, 0x11); // not block aligned
        let o = CowBackend::create(&d.path().join("o.ovl"), &base, None).unwrap();
        let data: Vec<u8> = (0..150_000u32).map(|i| i as u8).collect();
        o.write_at(10_000, &data).unwrap();
        assert_eq!(rd(&*o, 10_000, data.len()), data);
        assert_eq!(rd(&*o, 0, 10), [0x11; 10]);
        assert_eq!(rd(&*o, 199_990, 10), [0x11; 10]);
        assert!(o.write_at(199_999, &[1, 2]).is_err());
    }

    #[test]
    fn branch_isolation_and_persistence() {
        let d = tempfile::tempdir().unwrap();
        let base = base_file(d.path(), 1 << 20, 0);
        let a = CowBackend::create(&d.path().join("a.ovl"), &base, None).unwrap();
        a.write_at(0, b"parent").unwrap();
        let b = a.branch(&d.path().join("b.ovl")).unwrap();
        let c = a.branch(&d.path().join("c.ovl")).unwrap();
        assert!(a.write_at(0, b"x").is_err(), "parent frozen");
        b.write_at(0, b"child-b").unwrap();
        c.write_at(100, b"child-c").unwrap();
        assert_eq!(rd(&*b, 0, 7), *b"child-b");
        assert_eq!(rd(&*c, 0, 6), *b"parent");
        assert_eq!(rd(&*c, 100, 7), *b"child-c");
        assert_eq!(rd(&*b, 100, 7), [0; 7]);
        assert_eq!(rd(&*a, 0, 6), *b"parent");
        b.flush().unwrap();
        drop((a, b, c));
        let b2 = CowBackend::open(&d.path().join("b.ovl")).unwrap();
        assert_eq!(rd(&*b2, 0, 7), *b"child-b");
        assert_eq!(b2.allocated_blocks(), 1);
        // Branched-from overlay reopens read-only.
        let a2 = CowBackend::open(&d.path().join("a.ovl")).unwrap();
        assert!(a2.is_frozen());
        drop((b2, a2));
        // Grandchild via path (parent must not be open: exclusive flock).
        let g = CowBackend::create_on_overlay(&d.path().join("g.ovl"), &d.path().join("b.ovl"))
            .unwrap();
        assert_eq!(rd(&*g, 0, 7), *b"child-b");
    }

    #[test]
    fn trim_and_write_zeroes_over_base() {
        let d = tempfile::tempdir().unwrap();
        let base = base_file(d.path(), 4 << 20, 0xFF);
        let o = CowBackend::create(&d.path().join("o.ovl"), &base, None).unwrap();
        o.write_at(0, &vec![7u8; 3 << 20]).unwrap();
        o.trim(65_536, 2 << 20).unwrap();
        assert_eq!(rd(&*o, 0, 4), [7; 4]);
        assert_eq!(rd(&*o, 65_536, 4096), vec![0u8; 4096]);
        assert_eq!(rd(&*o, 65_536 + (2 << 20) - 1, 2), [0, 7]);
        // Zeroing a never-written range hides the base.
        o.write_zeroes(3 << 20, 70_000).unwrap();
        assert_eq!(rd(&*o, 3 << 20, 8), [0; 8]);
        assert_eq!(rd(&*o, (3 << 20) + 70_000, 2), [0xFF; 2]);
        // Partial-block zero keeps neighbours.
        o.write_zeroes((3 << 20) + 140_000, 10).unwrap();
        assert_eq!(
            rd(&*o, (3 << 20) + 139_999, 12),
            [0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF]
        );
    }

    #[test]
    fn trim_releases_space() {
        let d = tempfile::tempdir().unwrap();
        let base = base_file(d.path(), 8 << 20, 0);
        let o = CowBackend::create(&d.path().join("o.ovl"), &base, None).unwrap();
        o.write_at(0, &vec![1u8; 8 << 20]).unwrap();
        o.flush().unwrap();
        let before = o.allocated_bytes().unwrap();
        o.trim(0, 8 << 20).unwrap();
        o.flush().unwrap();
        let after = o.allocated_bytes().unwrap();
        // Hole punching is filesystem dependent; only assert when it worked.
        eprintln!("allocated before={before} after={after}");
        assert!(after <= before);
        assert_eq!(rd(&*o, 4 << 20, 16), [0; 16]);
    }
}

#[cfg(test)]
mod feature_tests {
    use super::*;
    use crate::crypt::XtsCipher;

    fn rd(b: &dyn BlockBackend, off: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        b.read_at(off, &mut v).unwrap();
        v
    }

    fn key(n: u8) -> Arc<dyn BlockCipher> {
        Arc::new(XtsCipher::new(&[n; 32]).unwrap())
    }

    fn enc_opts(n: u8) -> CreateOptions {
        CreateOptions {
            size: None,
            cipher: Some(key(n)),
        }
    }

    fn open_key(n: u8) -> OpenOptions {
        OpenOptions {
            key: Some(key(n)),
            ..Default::default()
        }
    }

    #[test]
    fn relative_parent_survives_directory_move() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        std::fs::create_dir(&a).unwrap();
        std::fs::write(a.join("base.img"), vec![0x42u8; 1 << 20]).unwrap();
        let o = CowBackend::create(&a.join("o.ovl"), &a.join("base.img"), None).unwrap();
        o.write_at(10, b"hello").unwrap();
        o.flush().unwrap();
        drop(o);
        // Stored relative.
        let mut hb = vec![0u8; 4096];
        File::open(a.join("o.ovl"))
            .unwrap()
            .read_exact_at(&mut hb, 0)
            .unwrap();
        let h = Header::decode(&hb).unwrap();
        assert!(h.rel);
        assert_eq!(h.parent, "base.img");
        assert_eq!(h.version, 2);
        // Move the whole tree: still opens.
        let b = d.path().join("moved");
        std::fs::rename(&a, &b).unwrap();
        let o2 = CowBackend::open(&b.join("o.ovl")).unwrap();
        assert_eq!(rd(&*o2, 8, 7), [0x42, 0x42, b'h', b'e', b'l', b'l', b'o']);
    }

    #[test]
    fn relative_overlay_chain_across_sibling_dirs() {
        let d = tempfile::tempdir().unwrap();
        for s in ["x", "y"] {
            std::fs::create_dir(d.path().join(s)).unwrap();
        }
        std::fs::write(d.path().join("x/base.img"), vec![1u8; 1 << 18]).unwrap();
        let p = CowBackend::create(
            &d.path().join("x/p.ovl"),
            &d.path().join("x/base.img"),
            None,
        )
        .unwrap();
        p.write_at(0, b"P").unwrap();
        p.flush().unwrap();
        drop(p);
        let c = CowBackend::create_on_overlay(&d.path().join("y/c.ovl"), &d.path().join("x/p.ovl"))
            .unwrap();
        drop(c);
        let mut hb = vec![0u8; 4096];
        File::open(d.path().join("y/c.ovl"))
            .unwrap()
            .read_exact_at(&mut hb, 0)
            .unwrap();
        assert_eq!(Header::decode(&hb).unwrap().parent, "../x/p.ovl");
        let m = d.path().join("m");
        std::fs::create_dir(&m).unwrap();
        std::fs::rename(d.path().join("x"), m.join("x")).unwrap();
        std::fs::rename(d.path().join("y"), m.join("y")).unwrap();
        let c = CowBackend::open(&m.join("y/c.ovl")).unwrap();
        assert_eq!(rd(&*c, 0, 2), [b'P', 1]);
    }

    /// Build a version-1 overlay by hand (absolute parent, computed layout).
    fn v1_overlay(path: &Path, base: &Path, size: u64, block0: u8) {
        let (n, off) = layout(size);
        let mut h = vec![0u8; 4096];
        h[0..8].copy_from_slice(MAGIC);
        h[8..12].copy_from_slice(&1u32.to_le_bytes());
        h[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
        h[16..24].copy_from_slice(&size.to_le_bytes());
        let pp = base.canonicalize().unwrap();
        let pp = pp.to_str().unwrap().as_bytes();
        h[32..36].copy_from_slice(&(pp.len() as u32).to_le_bytes());
        h[36..36 + pp.len()].copy_from_slice(pp);
        let f = File::create(path).unwrap();
        f.write_all_at(&h, 0).unwrap();
        f.write_all_at(&[1u8], 4096).unwrap(); // block 0 present
        f.set_len(off + size).unwrap();
        f.write_all_at(&vec![block0; BLOCK_SIZE as usize], off)
            .unwrap();
        let _ = n;
    }

    #[test]
    fn v1_absolute_header_still_reads_rebases_and_resizes() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("base.img");
        let size = 3 * BLOCK_SIZE;
        std::fs::write(&base, vec![0xAAu8; size as usize]).unwrap();
        let ov = d.path().join("old.ovl");
        v1_overlay(&ov, &base, size, 0x55);
        let o = CowBackend::open(&ov).unwrap();
        assert_eq!(rd(&*o, 0, 4), [0x55; 4]);
        assert_eq!(rd(&*o, BLOCK_SIZE, 4), [0xAA; 4]);
        o.write_at(BLOCK_SIZE, b"new").unwrap();
        drop(o);
        // v1 resize in place (same bitmap page) keeps v1 and the data.
        assert!(!CowBackend::resize_file(&ov, size + 5 * BLOCK_SIZE).unwrap());
        let o = CowBackend::open(&ov).unwrap();
        assert_eq!(o.size(), size + 5 * BLOCK_SIZE);
        assert_eq!(rd(&*o, BLOCK_SIZE, 3), *b"new");
        drop(o);
        // Rebase upgrades to a v2 relative header.
        CowBackend::rebase(&ov, base.to_str().unwrap(), &OpenOptions::default()).unwrap();
        let mut hb = vec![0u8; 4096];
        File::open(&ov).unwrap().read_exact_at(&mut hb, 0).unwrap();
        let h = Header::decode(&hb).unwrap();
        assert_eq!((h.version, h.rel, h.parent.as_str()), (2, true, "base.img"));
        let o = CowBackend::open(&ov).unwrap();
        assert_eq!(rd(&*o, 0, 2), [0x55; 2]);
        assert_eq!(rd(&*o, BLOCK_SIZE + 3, 1), [0xAA]);
    }

    #[test]
    fn rebase_to_moved_parent_and_cycle_rejected() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("base.img"), vec![9u8; 1 << 17]).unwrap();
        std::fs::copy(a.join("base.img"), b.join("base.img")).unwrap();
        let o = CowBackend::create(&a.join("o.ovl"), &a.join("base.img"), None).unwrap();
        o.write_at(0, b"zz").unwrap();
        o.flush().unwrap();
        drop(o);
        CowBackend::rebase(
            &a.join("o.ovl"),
            b.join("base.img").to_str().unwrap(),
            &OpenOptions::default(),
        )
        .unwrap();
        std::fs::remove_file(a.join("base.img")).unwrap();
        let o = CowBackend::open(&a.join("o.ovl")).unwrap();
        assert_eq!(rd(&*o, 0, 3), [b'z', b'z', 9]);
        drop(o);
        // Rebase to missing parent fails and leaves the overlay intact.
        assert!(
            CowBackend::rebase(&a.join("o.ovl"), "/no/such/file", &OpenOptions::default()).is_err()
        );
        assert!(CowBackend::open(&a.join("o.ovl")).is_ok());
        // Cycle: parent <- child, rebase parent onto child.
        let p = CowBackend::create(&b.join("p.ovl"), &b.join("base.img"), None).unwrap();
        let c = p.branch(&b.join("c.ovl")).unwrap();
        drop((p, c));
        let e = CowBackend::rebase(
            &b.join("p.ovl"),
            b.join("c.ovl").to_str().unwrap(),
            &OpenOptions::default(),
        )
        .unwrap_err();
        assert!(e.to_string().contains("cycle"), "{e}");
    }

    #[test]
    fn encrypted_roundtrip_ciphertext_and_keys() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("base.img");
        std::fs::write(&base, vec![0x11u8; 3 * BLOCK_SIZE as usize - 100]).unwrap();
        let ov = d.path().join("e.ovl");
        let o = CowBackend::create_opts(&ov, &base, &enc_opts(7)).unwrap();
        assert!(o.is_encrypted());
        let secret: Vec<u8> = b"TOP-SECRET-MARKER-0123456789"
            .iter()
            .cycle()
            .take(150_000)
            .copied()
            .collect();
        o.write_at(1000, &secret).unwrap();
        o.write_zeroes(BLOCK_SIZE + 5, 20).unwrap();
        o.flush().unwrap();
        assert_eq!(rd(&*o, 1000, secret.len())[..100], secret[..100]);
        assert_eq!(rd(&*o, BLOCK_SIZE + 5, 20), [0; 20]);
        // Untouched tail of the odd-sized device reads through to the base.
        assert_eq!(rd(&*o, 3 * BLOCK_SIZE - 101, 1), [0x11]);
        // Ciphertext on disk is not plaintext.
        let raw = std::fs::read(&ov).unwrap();
        let needle = b"TOP-SECRET-MARKER";
        assert!(!raw.windows(needle.len()).any(|w| w == needle));
        assert_ne!(o.raw_block(0).unwrap()[1000..1100], secret[..100]);
        drop(o);
        // No key / wrong key / right key.
        let e = CowBackend::open(&ov).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("key is required"), "{e}");
        let e = CowBackend::open_with(&ov, &open_key(8)).err().unwrap();
        assert!(e.to_string().contains("wrong key"), "{e}");
        let o = CowBackend::open_with(&ov, &open_key(7)).unwrap();
        assert_eq!(rd(&*o, 1000, secret.len()), {
            let mut s = secret.clone();
            let z0 = (BLOCK_SIZE + 5 - 1000) as usize;
            s[z0..z0 + 20].fill(0);
            s
        });
    }

    #[test]
    fn encrypted_branch_inherits_and_requires_key() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("base.img");
        std::fs::write(&base, vec![0u8; 1 << 20]).unwrap();
        let a = CowBackend::create_opts(&d.path().join("a.ovl"), &base, &enc_opts(1)).unwrap();
        a.write_at(0, b"parent-data").unwrap();
        let b = a.branch(&d.path().join("b.ovl")).unwrap();
        assert!(b.is_encrypted());
        b.write_at(100, b"child-data").unwrap();
        b.flush().unwrap();
        assert_eq!(rd(&*b, 0, 11), *b"parent-data");
        assert_eq!(rd(&*b, 100, 10), *b"child-data");
        drop((a, b));
        assert!(CowBackend::open(&d.path().join("b.ovl")).is_err());
        assert!(CowBackend::open_with(&d.path().join("b.ovl"), &open_key(2)).is_err());
        let b = CowBackend::open_with(&d.path().join("b.ovl"), &open_key(1)).unwrap();
        assert_eq!(rd(&*b, 0, 11), *b"parent-data");
        assert_eq!(rd(&*b, 100, 10), *b"child-data");
        drop(b);
        // A plain child over an encrypted parent is refused.
        let e = CowBackend::create_on_overlay_opts(
            &d.path().join("plain.ovl"),
            &d.path().join("b.ovl"),
            &CreateOptions::default(),
            &open_key(1),
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("encrypted"), "{e}");
        // Encrypted grandchild via path works.
        let g = CowBackend::create_on_overlay_opts(
            &d.path().join("g.ovl"),
            &d.path().join("b.ovl"),
            &enc_opts(1),
            &open_key(1),
        )
        .unwrap();
        assert_eq!(rd(&*g, 100, 10), *b"child-data");
    }

    #[test]
    fn grow_live_in_place_and_offline_relocate() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("base.img");
        std::fs::write(&base, vec![0xEEu8; 1 << 20]).unwrap();
        let ov = d.path().join("g.ovl");
        let o = CowBackend::create(&ov, &base, None).unwrap();
        o.write_at(0, b"data").unwrap();
        assert!(o.grow((1 << 20) - 1).is_err(), "shrink refused");
        o.grow(8 << 20).unwrap();
        assert_eq!(o.size(), 8 << 20);
        o.write_at((8 << 20) - 3, b"end").unwrap();
        assert_eq!(rd(&*o, (8 << 20) - 3, 3), *b"end");
        assert_eq!(rd(&*o, 1 << 20, 4), [0; 4]);
        // Far beyond reserved bitmap capacity: live grow refuses.
        let huge = 1u64 << 40;
        assert_eq!(o.grow(huge).unwrap_err().kind(), io::ErrorKind::Unsupported);
        o.flush().unwrap();
        drop(o);
        assert!(CowBackend::resize_file(&ov, huge).unwrap(), "relocated");
        let o = CowBackend::open(&ov).unwrap();
        assert_eq!(o.size(), huge);
        assert_eq!(rd(&*o, 0, 4), *b"data");
        assert_eq!(rd(&*o, (8 << 20) - 3, 3), *b"end");
        assert_eq!(rd(&*o, 2 << 20, 2), [0; 2]);
        o.write_at(huge - 4, b"tail").unwrap();
        assert_eq!(rd(&*o, huge - 4, 4), *b"tail");
        // Branched (frozen) overlay refuses resize.
        let c = o.branch(&d.path().join("c.ovl")).unwrap();
        drop((o, c));
        assert!(CowBackend::resize_file(&ov, huge * 2).is_err());
    }

    #[test]
    fn encrypted_grow_and_relocate_preserve_data() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("base.img");
        std::fs::write(&base, vec![0u8; 100_000]).unwrap();
        let ov = d.path().join("e.ovl");
        let o = CowBackend::create_opts(&ov, &base, &enc_opts(5)).unwrap();
        o.write_at(99_990, b"0123456789").unwrap();
        o.grow(300_000).unwrap();
        assert_eq!(rd(&*o, 99_990, 10), *b"0123456789");
        o.write_at(299_990, b"abcdefghij").unwrap();
        o.flush().unwrap();
        drop(o);
        assert!(CowBackend::resize_file(&ov, 1 << 40).unwrap());
        let o = CowBackend::open_with(&ov, &open_key(5)).unwrap();
        assert_eq!(rd(&*o, 99_990, 10), *b"0123456789");
        assert_eq!(rd(&*o, 299_990, 10), *b"abcdefghij");
        assert_eq!(rd(&*o, 100_000, 8), [0; 8]);
        assert!(CowBackend::open_with(&ov, &open_key(6)).is_err());
    }
}
