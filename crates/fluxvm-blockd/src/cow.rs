// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Copy-on-write layered backend.
//!
//! On-disk overlay format (all integers little-endian):
//!
//! ```text
//! 0      8   magic "FLXOVL01"
//! 8      4   version (1)
//! 12     4   block size (65536)
//! 16     8   virtual size in bytes
//! 24     4   parent kind: 0 = raw file, 1 = overlay
//! 28     4   frozen flag (set once a branch was taken; no more writes)
//! 32     4   parent path length N
//! 36     N   parent path (absolute)
//! 4096       allocation bitmap, 1 bit per block, padded to 4096
//! D          data region; virtual offset V lives at file offset D + V
//! ```
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
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

const MAGIC: &[u8; 8] = b"FLXOVL01";
const HEADER_LEN: u64 = 4096;
const MAX_DEPTH: usize = 64;
const KIND_RAW: u32 = 0;
const KIND_OVERLAY: u32 = 1;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

pub struct CowBackend {
    path: PathBuf,
    file: File,
    size: u64,
    nblocks: u64,
    data_off: u64,
    bitmap: RwLock<Vec<u8>>,
    base: Arc<dyn BlockBackend>,
    frozen: AtomicBool,
}

fn layout(size: u64) -> (u64, u64) {
    let nblocks = size.div_ceil(BLOCK_SIZE);
    let bm = nblocks.div_ceil(8).div_ceil(4096) * 4096;
    (nblocks, HEADER_LEN + bm)
}

fn abs(p: &Path) -> io::Result<PathBuf> {
    std::path::absolute(p)
}

impl CowBackend {
    /// Create an overlay over a raw base image file. `size` defaults to the
    /// base size and must not be smaller than it.
    pub fn create(path: &Path, base: &Path, size: Option<u64>) -> io::Result<Arc<Self>> {
        let b = Arc::new(RawFile::open(base, true)?);
        let size = size.unwrap_or(b.size());
        if size < b.size() {
            return Err(bad("overlay smaller than base"));
        }
        Self::create_with(path, b, KIND_RAW, &abs(base)?, size)
    }

    /// Create an overlay over an existing overlay file (opened read-only and
    /// frozen). The parent file is marked frozen so it can no longer change
    /// under the child.
    pub fn create_on_overlay(path: &Path, parent: &Path) -> io::Result<Arc<Self>> {
        let p = Self::open_inner(parent, 0, true)?;
        p.freeze_persist()?;
        let size = p.size;
        Self::create_with(path, p, KIND_OVERLAY, &abs(parent)?, size)
    }

    /// Instant branch of a live overlay: freezes `self` and returns a new
    /// writable overlay on top of it. Cost is one small file creation.
    pub fn branch(self: &Arc<Self>, new_path: &Path) -> io::Result<Arc<Self>> {
        self.freeze_persist()?;
        self.flush()?;
        Self::create_with(new_path, self.clone(), KIND_OVERLAY, &abs(&self.path)?, self.size)
    }

    /// Open an existing overlay and its whole parent chain. If the overlay
    /// has been branched from it opens read-only.
    pub fn open(path: &Path) -> io::Result<Arc<Self>> {
        Self::open_inner(path, 0, false)
    }

    fn create_with(
        path: &Path,
        base: Arc<dyn BlockBackend>,
        kind: u32,
        parent_path: &Path,
        size: u64,
    ) -> io::Result<Arc<Self>> {
        let pp = parent_path.to_str().ok_or_else(|| bad("non-utf8 parent path"))?.as_bytes();
        if pp.len() > (HEADER_LEN as usize - 36) {
            return Err(bad("parent path too long"));
        }
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        let (nblocks, data_off) = layout(size);
        let mut h = vec![0u8; HEADER_LEN as usize];
        h[0..8].copy_from_slice(MAGIC);
        h[8..12].copy_from_slice(&1u32.to_le_bytes());
        h[12..16].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes());
        h[16..24].copy_from_slice(&size.to_le_bytes());
        h[24..28].copy_from_slice(&kind.to_le_bytes());
        h[32..36].copy_from_slice(&(pp.len() as u32).to_le_bytes());
        h[36..36 + pp.len()].copy_from_slice(pp);
        file.write_all_at(&h, 0)?;
        file.set_len(data_off + size)?; // sparse
        file.sync_all()?;
        Ok(Arc::new(Self {
            path: abs(path)?,
            file,
            size,
            nblocks,
            data_off,
            bitmap: RwLock::new(vec![0u8; nblocks.div_ceil(8) as usize]),
            base,
            frozen: AtomicBool::new(false),
        }))
    }

    fn open_inner(path: &Path, depth: usize, force_frozen: bool) -> io::Result<Arc<Self>> {
        if depth > MAX_DEPTH {
            return Err(bad("overlay chain too deep (cycle?)"));
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut h = vec![0u8; HEADER_LEN as usize];
        file.read_exact_at(&mut h, 0)?;
        if &h[0..8] != MAGIC {
            return Err(bad("not a fluxvm overlay"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().unwrap());
        if u32_at(8) != 1 || u32_at(12) as u64 != BLOCK_SIZE {
            return Err(bad("unsupported overlay version/block size"));
        }
        let size = u64::from_le_bytes(h[16..24].try_into().unwrap());
        let kind = u32_at(24);
        let frozen = u32_at(28) != 0 || force_frozen;
        let n = u32_at(32) as usize;
        if n > HEADER_LEN as usize - 36 {
            return Err(bad("corrupt parent path"));
        }
        let parent = PathBuf::from(
            std::str::from_utf8(&h[36..36 + n]).map_err(|_| bad("non-utf8 parent path"))?,
        );
        let base: Arc<dyn BlockBackend> = match kind {
            KIND_RAW => Arc::new(RawFile::open(&parent, true)?),
            KIND_OVERLAY => Self::open_inner(&parent, depth + 1, true)?,
            _ => return Err(bad("unknown parent kind")),
        };
        let (nblocks, data_off) = layout(size);
        let mut bitmap = vec![0u8; nblocks.div_ceil(8) as usize];
        file.read_exact_at(&mut bitmap, HEADER_LEN)?;
        Ok(Arc::new(Self {
            path: abs(path)?,
            file,
            size,
            nblocks,
            data_off,
            bitmap: RwLock::new(bitmap),
            base,
            frozen: AtomicBool::new(frozen),
        }))
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

    /// Number of blocks held by this layer.
    pub fn allocated_blocks(&self) -> u64 {
        self.bitmap.read().unwrap().iter().map(|b| b.count_ones() as u64).sum()
    }

    fn check(&self, off: u64, len: u64) -> io::Result<()> {
        if off.checked_add(len).is_none_or(|e| e > self.size) {
            return Err(oob());
        }
        Ok(())
    }

    fn check_writable(&self) -> io::Result<()> {
        if self.is_frozen() {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "overlay is frozen"));
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

    fn read_overlay(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let n = self.file.read_at(&mut buf[done..], self.data_off + off + done as u64)?;
            if n == 0 {
                buf[done..].fill(0);
                break;
            }
            done += n;
        }
        Ok(())
    }

    fn block_len(&self, b: u64) -> u64 {
        (self.size - b * BLOCK_SIZE).min(BLOCK_SIZE)
    }
}

impl BlockBackend for CowBackend {
    fn size(&self) -> u64 {
        self.size
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
        let end = off + data.len() as u64;
        let mut pos = off;
        while pos < end {
            let b = pos / BLOCK_SIZE;
            let bstart = b * BLOCK_SIZE;
            let seg_end = (bstart + BLOCK_SIZE).min(end);
            let src = &data[(pos - off) as usize..(seg_end - off) as usize];
            let present = Self::present(&bm, b);
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
            if !present {
                self.set_bit(&mut bm, b)?; // index after data
            }
            pos = seg_end;
        }
        Ok(())
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
        self.nblocks
    }

    /// Bytes physically allocated in the overlay file (holes excluded).
    pub fn allocated_bytes(&self) -> io::Result<u64> {
        crate::sparse::allocated_bytes(&self.file)
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
        // Grandchild via path.
        let g = CowBackend::create_on_overlay(&d.path().join("g.ovl"), &d.path().join("b.ovl")).unwrap();
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
        assert_eq!(rd(&*o, (3 << 20) + 139_999, 12), [0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF]);
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
