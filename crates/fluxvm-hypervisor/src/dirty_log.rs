// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! KVM dirty-log incremental snapshots (design in `docs/ROADMAP-DENSITY.md`).
//!
//! A delta (`FLUXKVMD`) holds only the guest pages written since its parent
//! snapshot. The dirty set is the union of:
//!
//! - KVM's dirty log for the RAM memslot (`KVM_MEM_LOG_DIRTY_PAGES` +
//!   `KVM_GET_DIRTY_LOG`, see `KvmVm::get_dirty_log`), which sees guest
//!   (vCPU) writes, and
//! - [`SoftwareDirtyBitmap`], set by `GuestMemory::write_at` /
//!   `advise_dontneed` for host-side writes KVM never sees (virtio
//!   completions, block reads, balloon discards).
//!
//! Writers that bypass both (kernel vhost-net, vhost-user backends) make a
//! delta unsafe; `kvm_snap` refuses to write one while they are active.
//!
//! On-disk layout (little endian):
//!
//! ```text
//! [8]  magic "FLUXKVMD"
//! [4]  delta format version (1)
//! [4]  page size (4096)
//! [8]  guest RAM length in bytes
//! [16] snapshot id (matches the FLUXKVM1 v6 vmstate lineage)
//! [16] parent snapshot id
//! [4]  parent memory path length N
//! [N]  parent memory path (UTF-8; relative paths resolve against the
//!      directory holding the delta)
//! [8]  range count R
//! R x { [8] first gfn, [8] page count }
//! the pages of every range, packed, in table order
//! ```
//!
//! Restore loads the parent (a full `snap.mem`, privately mapped, or another
//! delta) and then copies these pages over it.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Magic for a dirty-log delta file.
pub const DELTA_MAGIC: &[u8; 8] = b"FLUXKVMD";
/// Current delta format version.
pub const DELTA_VERSION: u32 = 1;
/// Guest page size tracked by the KVM dirty log on x86_64.
pub const PAGE_SIZE: u64 = 4096;
/// Longest parent path accepted when reading a delta.
const MAX_PARENT_PATH: u32 = 4096;
/// Deepest delta chain restore follows before giving up.
pub const MAX_CHAIN_DEPTH: usize = 64;

/// Number of 4 KiB pages covering `mem_len` bytes.
pub fn page_count(mem_len: u64) -> u64 {
    mem_len.div_ceil(PAGE_SIZE)
}

/// u64 words in a dirty bitmap for `mem_len` bytes (KVM's layout: one bit
/// per page, bit `n % 64` of word `n / 64`, rounded up to whole words).
pub fn bitmap_words(mem_len: u64) -> usize {
    page_count(mem_len).div_ceil(64) as usize
}

/// OR `src` into `dst` word by word (lengths may differ; extra words of
/// `src` are ignored).
pub fn merge_bitmap(dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d |= *s;
    }
}

/// Collapse a dirty bitmap into `(first_gfn, page_count)` runs, in
/// ascending order, ignoring bits at or past `pages`.
pub fn bitmap_to_ranges(bitmap: &[u64], pages: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (w, &word) in bitmap.iter().enumerate() {
        let mut bits = word;
        while bits != 0 {
            let b = bits.trailing_zeros() as u64;
            bits &= bits - 1;
            let gfn = w as u64 * 64 + b;
            if gfn >= pages {
                return out;
            }
            match out.last_mut() {
                Some((start, len)) if *start + *len == gfn => *len += 1,
                _ => out.push((gfn, 1)),
            }
        }
    }
    out
}

/// Total pages covered by `ranges`.
pub fn ranges_pages(ranges: &[(u64, u64)]) -> u64 {
    ranges.iter().map(|(_, n)| n).sum()
}

/// Everything in a delta except the page data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaHeader {
    pub mem_len: u64,
    pub snapshot_id: [u8; 16],
    pub parent_id: [u8; 16],
    pub parent_mem: PathBuf,
    pub ranges: Vec<(u64, u64)>,
}

impl DeltaHeader {
    /// Parent memory path, resolved against the delta's directory when
    /// it is relative.
    pub fn resolve_parent(&self, delta_path: &Path) -> PathBuf {
        if self.parent_mem.is_absolute() {
            self.parent_mem.clone()
        } else {
            delta_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(&self.parent_mem)
        }
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Write a delta of `mem` (the whole guest RAM) holding the pages in
/// `header.ranges`. Fsyncs before returning.
pub fn write_delta(path: &Path, header: &DeltaHeader, mem: &[u8]) -> io::Result<()> {
    if header.mem_len != mem.len() as u64 {
        return Err(invalid("delta header RAM length does not match guest RAM"));
    }
    let pages = page_count(header.mem_len);
    for &(gfn, n) in &header.ranges {
        if n == 0 || gfn.checked_add(n).is_none_or(|end| end > pages) {
            return Err(invalid(format!("dirty range {gfn}+{n} outside guest RAM")));
        }
    }
    let parent = header
        .parent_mem
        .to_str()
        .ok_or_else(|| invalid("parent path not UTF-8"))?;
    let file = File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    w.write_all(DELTA_MAGIC)?;
    w.write_all(&DELTA_VERSION.to_le_bytes())?;
    w.write_all(&(PAGE_SIZE as u32).to_le_bytes())?;
    w.write_all(&header.mem_len.to_le_bytes())?;
    w.write_all(&header.snapshot_id)?;
    w.write_all(&header.parent_id)?;
    w.write_all(&(parent.len() as u32).to_le_bytes())?;
    w.write_all(parent.as_bytes())?;
    w.write_all(&(header.ranges.len() as u64).to_le_bytes())?;
    for &(gfn, n) in &header.ranges {
        w.write_all(&gfn.to_le_bytes())?;
        w.write_all(&n.to_le_bytes())?;
    }
    for &(gfn, n) in &header.ranges {
        let start = (gfn * PAGE_SIZE) as usize;
        let end = ((gfn + n) * PAGE_SIZE).min(header.mem_len) as usize;
        w.write_all(&mem[start..end])?;
        // A RAM length that is not page aligned ends with a partial page;
        // pad it so every record is whole pages.
        let pad = ((gfn + n) * PAGE_SIZE) as usize - end;
        if pad > 0 {
            w.write_all(&vec![0u8; pad])?;
        }
    }
    let file = w.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()
}

/// True when `path` starts with [`DELTA_MAGIC`].
pub fn is_delta(path: &Path) -> bool {
    let mut magic = [0u8; 8];
    File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == DELTA_MAGIC
}

fn read_u32<R: Read>(r: &mut R) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// Read and validate a delta header, leaving `r` at the first page byte.
pub fn read_header<R: Read>(r: &mut R) -> io::Result<DeltaHeader> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != DELTA_MAGIC {
        return Err(invalid("not a FLUXKVMD delta"));
    }
    let version = read_u32(r)?;
    if version != DELTA_VERSION {
        return Err(invalid(format!("unsupported delta version {version}")));
    }
    let page = read_u32(r)?;
    if page as u64 != PAGE_SIZE {
        return Err(invalid(format!("unsupported delta page size {page}")));
    }
    let mem_len = read_u64(r)?;
    let mut snapshot_id = [0u8; 16];
    r.read_exact(&mut snapshot_id)?;
    let mut parent_id = [0u8; 16];
    r.read_exact(&mut parent_id)?;
    let plen = read_u32(r)?;
    if plen > MAX_PARENT_PATH {
        return Err(invalid(format!("parent path length {plen} too long")));
    }
    let mut pbytes = vec![0u8; plen as usize];
    r.read_exact(&mut pbytes)?;
    let parent_mem =
        PathBuf::from(String::from_utf8(pbytes).map_err(|_| invalid("parent path not UTF-8"))?);
    let nranges = read_u64(r)?;
    let pages = page_count(mem_len);
    if nranges > pages {
        return Err(invalid(format!("{nranges} ranges for {pages} pages")));
    }
    let mut ranges = Vec::with_capacity(nranges as usize);
    let mut prev_end = 0u64;
    for _ in 0..nranges {
        let gfn = read_u64(r)?;
        let n = read_u64(r)?;
        let end = gfn
            .checked_add(n)
            .ok_or_else(|| invalid("dirty range overflows"))?;
        if n == 0 || end > pages || gfn < prev_end {
            return Err(invalid(format!("bad dirty range {gfn}+{n}")));
        }
        prev_end = end;
        ranges.push((gfn, n));
    }
    Ok(DeltaHeader {
        mem_len,
        snapshot_id,
        parent_id,
        parent_mem,
        ranges,
    })
}

/// Read a delta's header from `path`.
pub fn read_header_from(path: &Path) -> io::Result<DeltaHeader> {
    read_header(&mut BufReader::new(File::open(path)?))
}

/// Copy the pages of the delta at `path` over `mem` (the parent must
/// already be loaded). Returns the header.
pub fn apply_delta(path: &Path, mem: &mut [u8]) -> io::Result<DeltaHeader> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let header = read_header(&mut r)?;
    if header.mem_len != mem.len() as u64 {
        return Err(invalid(format!(
            "delta RAM length {} does not match guest RAM {}",
            header.mem_len,
            mem.len()
        )));
    }
    for &(gfn, n) in &header.ranges {
        let start = (gfn * PAGE_SIZE) as usize;
        let full_end = ((gfn + n) * PAGE_SIZE) as usize;
        let end = full_end.min(mem.len());
        r.read_exact(&mut mem[start..end])?;
        if full_end > end {
            let mut pad = vec![0u8; full_end - end];
            r.read_exact(&mut pad)?;
        }
    }
    Ok(header)
}

/// Bitmap of guest pages written from the host (device emulation, balloon)
/// since the last [`SoftwareDirtyBitmap::take`]. KVM's dirty log only sees
/// guest writes, so incremental snapshots OR this into it.
///
/// Lock free: every `GuestMemory` clone shares one instance, and setting a
/// bit is a relaxed `fetch_or` that is skipped entirely while tracking is
/// off (the default), so VMs that never take a delta pay one atomic load
/// per host write.
#[derive(Debug)]
pub struct SoftwareDirtyBitmap {
    enabled: AtomicBool,
    bits: Vec<AtomicU64>,
    page_count: usize,
}

impl SoftwareDirtyBitmap {
    pub fn new(guest_ram_bytes: usize) -> Self {
        let page_count = guest_ram_bytes.div_ceil(PAGE_SIZE as usize);
        let words = page_count.div_ceil(64);
        Self {
            enabled: AtomicBool::new(false),
            bits: (0..words).map(|_| AtomicU64::new(0)).collect(),
            page_count,
        }
    }

    /// Start (or stop) recording; enabling clears any stale bits.
    pub fn set_enabled(&self, on: bool) {
        if on {
            self.clear();
        }
        self.enabled.store(on, Ordering::SeqCst);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set(&self, gfn: usize) {
        if gfn < self.page_count {
            self.bits[gfn / 64].fetch_or(1u64 << (gfn % 64), Ordering::Relaxed);
        }
    }

    /// Record a host write of `len` bytes at guest physical `gpa`, if
    /// tracking is on.
    pub fn mark_range(&self, gpa: u64, len: usize) {
        if len == 0 || !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let first = gpa / PAGE_SIZE;
        let last = (gpa.saturating_add(len as u64) - 1) / PAGE_SIZE;
        for gfn in first..=last.min(self.page_count as u64 - 1) {
            self.set(gfn as usize);
        }
    }

    pub fn is_dirty(&self, gfn: usize) -> bool {
        if gfn >= self.page_count {
            return false;
        }
        (self.bits[gfn / 64].load(Ordering::Relaxed) & (1u64 << (gfn % 64))) != 0
    }

    pub fn clear(&self) {
        for w in &self.bits {
            w.store(0, Ordering::Relaxed);
        }
    }

    /// Return the current bits (KVM bitmap layout) and clear them.
    pub fn take(&self) -> Vec<u64> {
        self.bits
            .iter()
            .map(|w| w.swap(0, Ordering::AcqRel))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_bitmap_roundtrip() {
        let bm = SoftwareDirtyBitmap::new(4096 * 128);
        bm.set(0);
        bm.set(63);
        bm.set(64);
        assert!(bm.is_dirty(0));
        assert!(bm.is_dirty(63));
        assert!(bm.is_dirty(64));
        assert!(!bm.is_dirty(1));
        bm.clear();
        assert!(!bm.is_dirty(0));
    }

    #[test]
    fn software_bitmap_marks_only_when_enabled() {
        let bm = SoftwareDirtyBitmap::new(4096 * 8);
        bm.mark_range(0, 10);
        assert!(!bm.is_dirty(0));
        bm.set_enabled(true);
        // Straddles pages 1 and 2.
        bm.mark_range(4096 + 4000, 200);
        // Past the end of RAM: clamped, no panic.
        bm.mark_range(7 * 4096, 3 * 4096);
        let words = bm.take();
        assert_eq!(words, vec![0b1000_0110]);
        assert_eq!(bm.take(), vec![0]);
    }

    #[test]
    fn bitmap_to_ranges_merges_runs_and_crosses_words() {
        let mut bm = vec![0u64; 3];
        for gfn in [0u64, 1, 2, 5, 62, 63, 64, 65, 130] {
            bm[(gfn / 64) as usize] |= 1 << (gfn % 64);
        }
        assert_eq!(
            bitmap_to_ranges(&bm, 192),
            vec![(0, 3), (5, 1), (62, 4), (130, 1)]
        );
        // Bits past the page count (KVM pads to whole words) are ignored.
        assert_eq!(bitmap_to_ranges(&bm, 130), vec![(0, 3), (5, 1), (62, 4)]);
        assert_eq!(bitmap_to_ranges(&[0, 0], 128), vec![]);
        assert_eq!(bitmap_to_ranges(&[u64::MAX], 64), vec![(0, 64)]);
        assert_eq!(ranges_pages(&[(0, 3), (5, 1), (62, 4)]), 8);
    }

    #[test]
    fn merge_and_sizes() {
        let mut a = vec![0b0101u64, 0];
        merge_bitmap(&mut a, &[0b0010, 1 << 63, 7]);
        assert_eq!(a, vec![0b0111, 1 << 63]);
        assert_eq!(page_count(4096 * 3 + 1), 4);
        assert_eq!(bitmap_words(4096 * 64), 1);
        assert_eq!(bitmap_words(4096 * 65), 2);
    }

    fn sample_mem(pages: usize) -> Vec<u8> {
        (0..pages * 4096)
            .map(|i| (i / 4096) as u8 ^ (i as u8))
            .collect()
    }

    #[test]
    fn delta_write_read_apply_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delta.mem");
        let mut mem = sample_mem(16);
        let header = DeltaHeader {
            mem_len: mem.len() as u64,
            snapshot_id: [1; 16],
            parent_id: [2; 16],
            parent_mem: PathBuf::from("base.mem"),
            ranges: vec![(0, 2), (7, 1), (15, 1)],
        };
        write_delta(&path, &header, &mem).unwrap();
        assert!(is_delta(&path));
        assert_eq!(read_header_from(&path).unwrap(), header);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            8 + 4 + 4 + 8 + 16 + 16 + 4 + 8 + 8 + 3 * 16 + 4 * 4096
        );
        assert_eq!(header.resolve_parent(&path), dir.path().join("base.mem"));

        let expected = mem.clone();
        // Scribble over everything, then apply: dirty pages come back,
        // clean pages keep whatever the parent had (here: 0xee).
        mem.fill(0xee);
        apply_delta(&path, &mut mem).unwrap();
        for gfn in 0..16usize {
            let page = &mem[gfn * 4096..(gfn + 1) * 4096];
            if [0, 1, 7, 15].contains(&gfn) {
                assert_eq!(page, &expected[gfn * 4096..(gfn + 1) * 4096], "gfn {gfn}");
            } else {
                assert!(page.iter().all(|b| *b == 0xee), "gfn {gfn}");
            }
        }
    }

    #[test]
    fn delta_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delta.mem");
        let mem = sample_mem(4);
        let mut header = DeltaHeader {
            mem_len: mem.len() as u64,
            snapshot_id: [0; 16],
            parent_id: [0; 16],
            parent_mem: PathBuf::from("/abs/base.mem"),
            ranges: vec![(3, 2)],
        };
        assert!(write_delta(&path, &header, &mem).is_err(), "range past RAM");
        header.ranges = vec![(1, 1)];
        write_delta(&path, &header, &mem).unwrap();
        assert_eq!(header.resolve_parent(&path), PathBuf::from("/abs/base.mem"));
        // Wrong RAM size on apply.
        let mut small = vec![0u8; 2 * 4096];
        assert!(apply_delta(&path, &mut small).is_err());
        // Truncated page data.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        let mut full = vec![0u8; 4 * 4096];
        assert!(apply_delta(&path, &mut full).is_err());
        // Not a delta.
        std::fs::write(&path, b"FLUXKVM1........").unwrap();
        assert!(!is_delta(&path));
        assert!(read_header_from(&path).is_err());
        // Overlapping / unordered ranges are rejected on read.
        let mut raw = Vec::new();
        raw.extend_from_slice(DELTA_MAGIC);
        raw.extend_from_slice(&DELTA_VERSION.to_le_bytes());
        raw.extend_from_slice(&4096u32.to_le_bytes());
        raw.extend_from_slice(&(4u64 * 4096).to_le_bytes());
        raw.extend_from_slice(&[0u8; 32]);
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&2u64.to_le_bytes());
        for (g, n) in [(2u64, 1u64), (1, 1)] {
            raw.extend_from_slice(&g.to_le_bytes());
            raw.extend_from_slice(&n.to_le_bytes());
        }
        std::fs::write(&path, &raw).unwrap();
        assert!(read_header_from(&path).is_err());
    }
}
