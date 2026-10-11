// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Minimal, conservative ext4 reader + in-place writer.
//!
//! Design rules:
//! * Everything is validated up front; anything not understood is refused.
//! * All modifications go into an in-memory overlay of filesystem blocks and
//!   the superblock. Nothing touches the disk until [`Fs::commit`], so any
//!   error before that leaves the image byte-for-byte unchanged.
//! * Only free blocks / free inodes found through the on-disk bitmaps are
//!   used; group descriptors, bitmaps checksums, inode checksums, directory
//!   block checksums and the superblock counters/checksum are kept coherent.

use crate::crc::crc32c_raw;
use crate::partition::Candidate;
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::os::unix::fs::FileExt;

pub const ROOT_INO: u32 = 2;

// compat
const COMPAT_HAS_JOURNAL: u32 = 0x4;
const COMPAT_ORPHAN_FILE: u32 = 0x1000;
const COMPAT_KNOWN_OK: u32 =
    0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x200 | 0x800 | COMPAT_ORPHAN_FILE;
// incompat
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_OK: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_RECOVER
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_FLEX_BG
    | INCOMPAT_CSUM_SEED;
// ro_compat
const RO_METADATA_CSUM: u32 = 0x400;
const RO_GDT_CSUM: u32 = 0x10;
// RO_GDT_CSUM is listed so the dedicated, clearer refusal below fires.
const RO_OK: u32 = 0x1 | 0x2 | 0x8 | 0x20 | 0x40 | RO_METADATA_CSUM | RO_GDT_CSUM | 0x2000;

// group flags
const BG_INODE_UNINIT: u16 = 0x1;
const BG_BLOCK_UNINIT: u16 = 0x2;

// inode flags
const EXTENTS_FL: u32 = 0x80000;
const INDEX_FL: u32 = 0x1000;
const BAD_FLAGS: u32 = 0x10 /*IMMUTABLE*/
    | 0x20 /*APPEND*/
    | 0x800 /*ENCRYPT*/
    | 0x40000 /*HUGE_FILE*/
    | 0x100000 /*VERITY*/
    | 0x200000 /*EA_INODE*/
    | 0x1000_0000 /*INLINE_DATA*/
    | 0x4000_0000 /*CASEFOLD*/;

const S_IFMT: u16 = 0xF000;
const S_IFDIR: u16 = 0x4000;
const S_IFREG: u16 = 0x8000;
const S_IFLNK: u16 = 0xA000;

const MAX_READ: u64 = 64 << 20;
const MAX_EXTENT_LEN: u32 = 32768;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn p16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn p32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn round4(n: usize) -> usize {
    (n + 3) & !3
}
fn bit(bm: &[u8], i: usize) -> bool {
    bm[i / 8] & (1 << (i % 8)) != 0
}
fn set_bit(bm: &mut [u8], i: usize) {
    bm[i / 8] |= 1 << (i % 8);
}
fn clear_bit(bm: &mut [u8], i: usize) {
    bm[i / 8] &= !(1 << (i % 8));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub lblock: u32,
    pub len: u32,
    pub phys: u64,
    pub uninit: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub inodes_allocated: u32,
    pub blocks_allocated: u64,
    pub blocks_freed: u64,
    pub dirs_created: u32,
}

pub struct Fs {
    file: File,
    base: u64,
    bs: usize,
    sb: Vec<u8>,
    csum: bool,
    seed: u32,
    is64: bool,
    blocks_count: u64,
    first_data_block: u64,
    bpg: usize,
    ipg: usize,
    inode_size: usize,
    desc_size: usize,
    groups: usize,
    gdt_block: u64,
    gdt_blocks: u64,
    first_ino: u32,
    now: u32,
    overlay: BTreeMap<u64, Vec<u8>>,
    pub stats: Stats,
}

impl Fs {
    /// Probe and validate the ext4 filesystem at `cand`. Refuses anything
    /// this writer does not fully understand.
    pub fn open(file: File, cand: &Candidate) -> Result<Fs> {
        let base = cand.offset;
        if cand.len < 4096 {
            return Err(Error::Invalid("partition too small".into()));
        }
        let mut sb = vec![0u8; 1024];
        file.read_exact_at(&mut sb, base + 1024)?;
        if le16(&sb, 0x38) != 0xEF53 {
            return Err(Error::Unsupported("no ext2/3/4 superblock magic".into()));
        }
        let rev = le32(&sb, 0x4C);
        if rev != 1 {
            return Err(Error::Unsupported(format!(
                "ext revision {rev} (need dynamic rev 1)"
            )));
        }
        let compat = le32(&sb, 0x5C);
        let incompat = le32(&sb, 0x60);
        let ro = le32(&sb, 0x64);
        if incompat & INCOMPAT_RECOVER != 0 {
            return Err(Error::Unsupported(
                "filesystem journal needs recovery (needs_recovery set); boot/fsck it first".into(),
            ));
        }
        if incompat & !INCOMPAT_OK != 0 {
            return Err(Error::Unsupported(format!(
                "unsupported incompat features 0x{:x}",
                incompat & !INCOMPAT_OK
            )));
        }
        if ro & !RO_OK != 0 {
            return Err(Error::Unsupported(format!(
                "unsupported ro_compat features 0x{:x}",
                ro & !RO_OK
            )));
        }
        if compat & !COMPAT_KNOWN_OK != 0 {
            return Err(Error::Unsupported(format!(
                "unsupported compat features 0x{:x}",
                compat & !COMPAT_KNOWN_OK
            )));
        }
        if incompat & INCOMPAT_EXTENTS == 0 {
            return Err(Error::Unsupported(
                "filesystem has no extents feature (ext2/3)".into(),
            ));
        }
        if incompat & INCOMPAT_FILETYPE == 0 {
            return Err(Error::Unsupported(
                "filesystem has no filetype feature".into(),
            ));
        }
        let csum = ro & RO_METADATA_CSUM != 0;
        if ro & RO_GDT_CSUM != 0 {
            return Err(Error::Unsupported(
                "uninit_bg/gdt_csum (crc16) group checksums are not supported".into(),
            ));
        }
        if le16(&sb, 0x3A) != 1 {
            return Err(Error::Unsupported(format!(
                "filesystem state 0x{:x} is not clean/valid; run fsck first",
                le16(&sb, 0x3A)
            )));
        }
        if le32(&sb, 0xE8) != 0 {
            return Err(Error::Unsupported("orphan inode list is not empty".into()));
        }
        for (off, what) in [
            (0x240usize, "user quota"),
            (0x244, "group quota"),
            (0x26C, "project quota"),
            (0x254, "snapshot"),
        ] {
            if le32(&sb, off) != 0 {
                return Err(Error::Unsupported(format!("{what} inode in use")));
            }
        }
        let log_bs = le32(&sb, 0x18);
        if log_bs > 2 {
            return Err(Error::Unsupported(
                "block size above 4096 not supported".into(),
            ));
        }
        let bs = 1024usize << log_bs;
        if le32(&sb, 0x1C) != log_bs {
            return Err(Error::Unsupported(
                "bigalloc/cluster size differs from block size".into(),
            ));
        }
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let mut blocks_count = le32(&sb, 0x4) as u64;
        if is64 {
            blocks_count |= (le32(&sb, 0x150) as u64) << 32;
        }
        let first_data_block = le32(&sb, 0x14) as u64;
        let bpg = le32(&sb, 0x20) as usize;
        let ipg = le32(&sb, 0x28) as usize;
        let inode_size = le16(&sb, 0x58) as usize;
        let first_ino = le32(&sb, 0x54);
        if !(128..=1024).contains(&inode_size) || !inode_size.is_power_of_two() || inode_size > bs {
            return Err(Error::Corrupt(format!("bad inode size {inode_size}")));
        }
        if bpg == 0 || bpg > bs * 8 || ipg == 0 || ipg > bs * 8 || !ipg.is_multiple_of(8) {
            return Err(Error::Corrupt("bad blocks/inodes per group".into()));
        }
        if first_data_block != if bs == 1024 { 1 } else { 0 } {
            return Err(Error::Corrupt("unexpected first_data_block".into()));
        }
        if first_ino < 11 {
            return Err(Error::Corrupt("first_ino < 11".into()));
        }
        if blocks_count <= first_data_block || blocks_count.checked_mul(bs as u64).is_none() {
            return Err(Error::Corrupt("bad blocks_count".into()));
        }
        if blocks_count * bs as u64 > cand.len {
            return Err(Error::Corrupt(
                "filesystem is larger than its partition (truncated image?)".into(),
            ));
        }
        let groups = ((blocks_count - first_data_block) as usize).div_ceil(bpg);
        if le32(&sb, 0x0) as u64 != groups as u64 * ipg as u64 {
            return Err(Error::Corrupt(
                "inodes_count != groups * inodes_per_group".into(),
            ));
        }
        let desc_size = if is64 { le16(&sb, 0xFE) as usize } else { 32 };
        if is64 && desc_size != 64 {
            return Err(Error::Unsupported(format!(
                "64bit descriptor size {desc_size}"
            )));
        }
        let gdt_block = first_data_block + 1;
        let gdt_blocks = ((groups * desc_size) as u64).div_ceil(bs as u64);
        let seed = if incompat & INCOMPAT_CSUM_SEED != 0 {
            le32(&sb, 0x270)
        } else {
            crc32c_raw(!0, &sb[0x68..0x78])
        };
        if csum {
            if le32(&sb, 0x3FC) != crc32c_raw(!0, &sb[..0x3FC]) {
                return Err(Error::Corrupt("superblock checksum mismatch".into()));
            }
            if sb[0x175] != 1 {
                return Err(Error::Unsupported(
                    "unknown superblock checksum type".into(),
                ));
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        let mut fs = Fs {
            file,
            base,
            bs,
            sb,
            csum,
            seed,
            is64,
            blocks_count,
            first_data_block,
            bpg,
            ipg,
            inode_size,
            desc_size,
            groups,
            gdt_block,
            gdt_blocks,
            first_ino,
            now,
            overlay: BTreeMap::new(),
            stats: Stats::default(),
        };
        // Group descriptor checksums (all groups; cheap).
        for g in 0..groups {
            let d = fs.read_gd(g)?;
            if csum && le16(&d, 0x1E) != fs.gd_csum(g, &d) {
                return Err(Error::Corrupt(format!(
                    "group descriptor {g} checksum mismatch"
                )));
            }
            let it = fs.gd_inode_table(&d);
            let itb = (ipg * inode_size).div_ceil(bs) as u64;
            if it == 0 || it + itb > blocks_count {
                return Err(Error::Corrupt(format!(
                    "group {g} inode table out of range"
                )));
            }
        }
        // Root inode must be a directory.
        let root = fs.read_inode(ROOT_INO)?;
        if le16(&root, 0) & S_IFMT != S_IFDIR {
            return Err(Error::Corrupt("root inode is not a directory".into()));
        }
        if compat & COMPAT_HAS_JOURNAL != 0 {
            fs.check_journal_clean()?;
        }
        if compat & COMPAT_ORPHAN_FILE != 0 {
            fs.check_orphan_file_empty()?;
        }
        Ok(fs)
    }

    // ------------------------------------------------------------ raw I/O

    fn read_block(&self, blk: u64) -> Result<Vec<u8>> {
        if blk >= self.blocks_count {
            return Err(Error::Corrupt(format!(
                "block {blk} beyond end of filesystem"
            )));
        }
        if let Some(b) = self.overlay.get(&blk) {
            return Ok(b.clone());
        }
        let mut v = vec![0u8; self.bs];
        self.file
            .read_exact_at(&mut v, self.base + blk * self.bs as u64)?;
        Ok(v)
    }

    fn write_block(&mut self, blk: u64, data: Vec<u8>) -> Result<()> {
        if blk >= self.blocks_count || data.len() != self.bs {
            return Err(Error::Corrupt(format!("invalid write to block {blk}")));
        }
        self.overlay.insert(blk, data);
        Ok(())
    }

    // --------------------------------------------------- group descriptors

    fn gd_pos(&self, g: usize) -> (u64, usize) {
        let byte = self.gdt_block * self.bs as u64 + (g * self.desc_size) as u64;
        (byte / self.bs as u64, (byte % self.bs as u64) as usize)
    }

    fn read_gd(&self, g: usize) -> Result<Vec<u8>> {
        let (blk, off) = self.gd_pos(g);
        let b = self.read_block(blk)?;
        Ok(b[off..off + self.desc_size].to_vec())
    }

    fn gd_csum(&self, g: usize, d: &[u8]) -> u16 {
        let mut c = crc32c_raw(self.seed, &(g as u32).to_le_bytes());
        c = crc32c_raw(c, &d[..0x1E]);
        c = crc32c_raw(c, &[0, 0]);
        if self.desc_size > 0x20 {
            c = crc32c_raw(c, &d[0x20..self.desc_size]);
        }
        (c & 0xFFFF) as u16
    }

    fn write_gd(&mut self, g: usize, d: &mut [u8]) -> Result<()> {
        if self.csum {
            let c = self.gd_csum(g, d);
            p16(d, 0x1E, c);
        }
        let (blk, off) = self.gd_pos(g);
        let mut b = self.read_block(blk)?;
        b[off..off + self.desc_size].copy_from_slice(d);
        self.write_block(blk, b)
    }

    fn gd64(&self, d: &[u8], lo: usize, hi: usize) -> u64 {
        let mut v = le32(d, lo) as u64;
        if self.is64 {
            v |= (le32(d, hi) as u64) << 32;
        }
        v
    }
    fn gd16(&self, d: &[u8], lo: usize, hi: usize) -> u32 {
        let mut v = le16(d, lo) as u32;
        if self.is64 {
            v |= (le16(d, hi) as u32) << 16;
        }
        v
    }
    fn set_gd16(&self, d: &mut [u8], lo: usize, hi: usize, v: u32) {
        p16(d, lo, v as u16);
        if self.is64 {
            p16(d, hi, (v >> 16) as u16);
        }
    }
    fn gd_block_bitmap(&self, d: &[u8]) -> u64 {
        self.gd64(d, 0, 0x20)
    }
    fn gd_inode_bitmap(&self, d: &[u8]) -> u64 {
        self.gd64(d, 4, 0x24)
    }
    fn gd_inode_table(&self, d: &[u8]) -> u64 {
        self.gd64(d, 8, 0x28)
    }
    fn gd_free_blocks(&self, d: &[u8]) -> u32 {
        self.gd16(d, 0xC, 0x2C)
    }
    fn gd_free_inodes(&self, d: &[u8]) -> u32 {
        self.gd16(d, 0xE, 0x2E)
    }
    fn gd_used_dirs(&self, d: &[u8]) -> u32 {
        self.gd16(d, 0x10, 0x30)
    }
    fn gd_itable_unused(&self, d: &[u8]) -> u32 {
        self.gd16(d, 0x1C, 0x32)
    }
    fn gd_flags(&self, d: &[u8]) -> u16 {
        le16(d, 0x12)
    }

    fn bitmap_csum_set(&self, d: &mut [u8], lo: usize, hi: usize, bm: &[u8]) {
        if !self.csum {
            return;
        }
        let c = crc32c_raw(self.seed, bm);
        p16(d, lo, c as u16);
        if self.desc_size >= 0x3C {
            p16(d, hi, (c >> 16) as u16);
        }
    }
    fn bitmap_csum_ok(&self, d: &[u8], lo: usize, hi: usize, bm: &[u8]) -> bool {
        if !self.csum {
            return true;
        }
        let c = crc32c_raw(self.seed, bm);
        if self.desc_size >= 0x3C {
            le16(d, lo) as u32 | (le16(d, hi) as u32) << 16 == c
        } else {
            le16(d, lo) == c as u16
        }
    }

    // ----------------------------------------------------------- superblock

    fn sb_free_blocks(&self) -> u64 {
        let mut v = le32(&self.sb, 0xC) as u64;
        if self.is64 {
            v |= (le32(&self.sb, 0x158) as u64) << 32;
        }
        v
    }
    fn set_sb_free_blocks(&mut self, v: u64) {
        p32(&mut self.sb, 0xC, v as u32);
        if self.is64 {
            p32(&mut self.sb, 0x158, (v >> 32) as u32);
        }
    }

    fn check_journal_clean(&mut self) -> Result<()> {
        let jino = le32(&self.sb, 0xE0);
        if jino == 0 {
            return Err(Error::Unsupported(
                "has_journal set but no internal journal inode (external journal?)".into(),
            ));
        }
        let raw = self.read_inode(jino)?;
        let (ext, _) = self.extents_of(jino, &raw)?;
        let first = ext
            .iter()
            .find(|e| e.lblock == 0 && !e.uninit)
            .ok_or_else(|| Error::Corrupt("journal inode has no block 0".into()))?;
        let jb = self.read_block(first.phys)?;
        let magic = u32::from_be_bytes(jb[0..4].try_into().unwrap());
        if magic != 0xC03B_3998 {
            return Err(Error::Corrupt("bad jbd2 superblock magic".into()));
        }
        let start = u32::from_be_bytes(jb[0x1C..0x20].try_into().unwrap());
        if start != 0 {
            return Err(Error::Unsupported(
                "journal contains unreplayed transactions; boot/fsck it first".into(),
            ));
        }
        Ok(())
    }

    fn check_orphan_file_empty(&mut self) -> Result<()> {
        let ino = le32(&self.sb, 0x280);
        if ino == 0 {
            return Err(Error::Corrupt(
                "orphan_file feature without orphan file inode".into(),
            ));
        }
        let raw = self.read_inode(ino)?;
        let (ext, _) = self.extents_of(ino, &raw)?;
        for e in ext {
            if e.uninit {
                continue;
            }
            for k in 0..e.len as u64 {
                let b = self.read_block(e.phys + k)?;
                if b[..self.bs - 8].iter().any(|&x| x != 0) {
                    return Err(Error::Unsupported(
                        "orphan file lists pending orphans; boot/fsck it first".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    // --------------------------------------------------------------- inodes

    fn inode_pos(&self, ino: u32) -> Result<(u64, usize)> {
        if ino == 0 || ino as usize > self.groups * self.ipg {
            return Err(Error::Corrupt(format!("inode {ino} out of range")));
        }
        let g = (ino as usize - 1) / self.ipg;
        let idx = (ino as usize - 1) % self.ipg;
        let d = self.read_gd(g)?;
        let table = self.gd_inode_table(&d);
        let byte = idx * self.inode_size;
        Ok((table + (byte / self.bs) as u64, byte % self.bs))
    }

    fn has_csum_hi(&self, raw: &[u8]) -> bool {
        self.inode_size > 128 && le16(raw, 0x80) >= 4
    }

    fn inode_csum(&self, ino: u32, raw: &[u8]) -> u32 {
        let mut b = raw.to_vec();
        b[0x7C] = 0;
        b[0x7D] = 0;
        if self.has_csum_hi(raw) {
            b[0x82] = 0;
            b[0x83] = 0;
        }
        let mut c = crc32c_raw(self.seed, &ino.to_le_bytes());
        c = crc32c_raw(c, &raw[0x64..0x68]);
        crc32c_raw(c, &b)
    }

    fn inode_seed(&self, ino: u32, raw: &[u8]) -> u32 {
        let c = crc32c_raw(self.seed, &ino.to_le_bytes());
        crc32c_raw(c, &raw[0x64..0x68])
    }

    pub fn read_inode(&self, ino: u32) -> Result<Vec<u8>> {
        let (blk, off) = self.inode_pos(ino)?;
        let b = self.read_block(blk)?;
        let raw = b[off..off + self.inode_size].to_vec();
        if raw.iter().all(|&x| x == 0) {
            return Err(Error::Corrupt(format!("inode {ino} is all zero")));
        }
        if self.csum {
            let c = self.inode_csum(ino, &raw);
            let ok = if self.has_csum_hi(&raw) {
                le16(&raw, 0x7C) as u32 | (le16(&raw, 0x82) as u32) << 16 == c
            } else {
                le16(&raw, 0x7C) == c as u16
            };
            if !ok {
                return Err(Error::Corrupt(format!("inode {ino} checksum mismatch")));
            }
        }
        Ok(raw)
    }

    fn write_inode(&mut self, ino: u32, raw: &mut [u8]) -> Result<()> {
        if self.csum {
            let c = self.inode_csum(ino, raw);
            p16(raw, 0x7C, c as u16);
            if self.has_csum_hi(raw) {
                p16(raw, 0x82, (c >> 16) as u16);
            }
        }
        let (blk, off) = self.inode_pos(ino)?;
        let mut b = self.read_block(blk)?;
        b[off..off + self.inode_size].copy_from_slice(raw);
        self.write_block(blk, b)
    }

    fn inode_size_bytes(raw: &[u8]) -> u64 {
        le32(raw, 0x4) as u64 | (le32(raw, 0x6C) as u64) << 32
    }

    // -------------------------------------------------------------- extents

    /// All extents plus the on-disk blocks of any extent-tree index/leaf nodes.
    pub fn extents_of(&self, ino: u32, raw: &[u8]) -> Result<(Vec<Extent>, Vec<u64>)> {
        let flags = le32(raw, 0x20);
        if flags & EXTENTS_FL == 0 {
            return Err(Error::Unsupported(format!(
                "inode {ino} is block-mapped (no EXTENTS flag)"
            )));
        }
        let iseed = self.inode_seed(ino, raw);
        let mut out = Vec::new();
        let mut tree = Vec::new();
        self.parse_node(
            &raw[0x28..0x28 + 60],
            true,
            iseed,
            None,
            &mut out,
            &mut tree,
        )?;
        out.sort_by_key(|e| e.lblock);
        Ok((out, tree))
    }

    fn parse_node(
        &self,
        node: &[u8],
        inline: bool,
        iseed: u32,
        expect_depth: Option<u16>,
        out: &mut Vec<Extent>,
        tree: &mut Vec<u64>,
    ) -> Result<()> {
        if le16(node, 0) != 0xF30A {
            return Err(Error::Corrupt("bad extent header magic".into()));
        }
        let entries = le16(node, 2) as usize;
        let max = le16(node, 4) as usize;
        let depth = le16(node, 6);
        if depth > 5 {
            return Err(Error::Corrupt("extent tree too deep".into()));
        }
        if expect_depth.is_some_and(|d| d != depth) {
            return Err(Error::Corrupt("extent tree depth mismatch".into()));
        }
        if entries > max || 12 + max * 12 > node.len() {
            return Err(Error::Corrupt("bad extent header counts".into()));
        }
        if !inline && self.csum {
            let end = 12 + max * 12;
            if end + 4 > node.len() || le32(node, end) != crc32c_raw(iseed, &node[..end]) {
                return Err(Error::Corrupt("extent node checksum mismatch".into()));
            }
        }
        for i in 0..entries {
            let e = &node[12 + i * 12..24 + i * 12];
            if depth == 0 {
                let raw_len = le16(e, 4) as u32;
                let (len, uninit) = if raw_len > MAX_EXTENT_LEN {
                    (raw_len - MAX_EXTENT_LEN, true)
                } else {
                    (raw_len, false)
                };
                let phys = le32(e, 8) as u64 | (le16(e, 6) as u64) << 32;
                if len == 0 || phys + len as u64 > self.blocks_count || phys < self.first_data_block
                {
                    return Err(Error::Corrupt("extent out of range".into()));
                }
                out.push(Extent {
                    lblock: le32(e, 0),
                    len,
                    phys,
                    uninit,
                });
            } else {
                let child = le32(e, 4) as u64 | (le16(e, 8) as u64) << 32;
                tree.push(child);
                let blk = self.read_block(child)?;
                self.parse_node(&blk, false, iseed, Some(depth - 1), out, tree)?;
            }
        }
        Ok(())
    }

    fn read_data(&self, ino: u32, raw: &[u8]) -> Result<Vec<u8>> {
        let size = Self::inode_size_bytes(raw);
        if size > MAX_READ {
            return Err(Error::Unsupported(format!(
                "inode {ino} too large to read ({size} bytes)"
            )));
        }
        let (ext, _) = self.extents_of(ino, raw)?;
        let mut out = vec![0u8; size as usize];
        for e in ext {
            if e.uninit {
                continue;
            }
            for k in 0..e.len as u64 {
                let lb = e.lblock as u64 + k;
                let start = lb * self.bs as u64;
                if start >= size {
                    break;
                }
                let blk = self.read_block(e.phys + k)?;
                let n = std::cmp::min(self.bs as u64, size - start) as usize;
                out[start as usize..start as usize + n].copy_from_slice(&blk[..n]);
            }
        }
        Ok(out)
    }

    fn read_symlink(&self, ino: u32, raw: &[u8]) -> Result<String> {
        if le32(raw, 0x20) & BAD_FLAGS & 0x800 != 0 {
            return Err(Error::Unsupported("encrypted symlink".into()));
        }
        let size = Self::inode_size_bytes(raw) as usize;
        let bytes = if le32(raw, 0x20) & EXTENTS_FL == 0 && size < 60 {
            raw[0x28..0x28 + size].to_vec()
        } else {
            self.read_data(ino, raw)?
        };
        String::from_utf8(bytes).map_err(|_| Error::Unsupported("non-UTF-8 symlink target".into()))
    }

    // ------------------------------------------------------------ directory

    fn dir_blocks(&self, ino: u32, raw: &[u8]) -> Result<Vec<u64>> {
        if le16(raw, 0) & S_IFMT != S_IFDIR {
            return Err(Error::Invalid(format!("inode {ino} is not a directory")));
        }
        let flags = le32(raw, 0x20);
        if flags & BAD_FLAGS != 0 {
            return Err(Error::Unsupported(format!(
                "directory inode {ino} has unsupported flags 0x{flags:x}"
            )));
        }
        let (ext, _) = self.extents_of(ino, raw)?;
        let nblocks = Self::inode_size_bytes(raw).div_ceil(self.bs as u64);
        let mut v = Vec::new();
        for e in ext {
            if e.uninit {
                return Err(Error::Unsupported(
                    "directory with uninitialised extent".into(),
                ));
            }
            for k in 0..e.len as u64 {
                if (e.lblock as u64 + k) < nblocks {
                    v.push(e.phys + k);
                }
            }
        }
        Ok(v)
    }

    fn lookup(&self, dir_ino: u32, dir_raw: &[u8], name: &str) -> Result<Option<u32>> {
        let nb = name.as_bytes();
        for blk in self.dir_blocks(dir_ino, dir_raw)? {
            let b = self.read_block(blk)?;
            let mut off = 0usize;
            while off + 8 <= self.bs {
                let ino = le32(&b, off);
                let rec = le16(&b, off + 4) as usize;
                let nl = b[off + 6] as usize;
                if rec < 8 || !rec.is_multiple_of(4) || off + rec > self.bs || 8 + nl > rec {
                    return Err(Error::Corrupt(format!(
                        "malformed directory entry in block {blk}"
                    )));
                }
                if ino != 0 && nl == nb.len() && &b[off + 8..off + 8 + nl] == nb {
                    return Ok(Some(ino));
                }
                off += rec;
            }
        }
        Ok(None)
    }

    /// Insert `name -> ino` into the directory. Reuses slack in an existing
    /// block; otherwise appends one block (inline extent tree only).
    fn add_dirent(&mut self, dir_ino: u32, name: &str, ino: u32, ftype: u8) -> Result<()> {
        if name.is_empty() || name.len() > 255 || name.contains('/') || name.contains('\0') {
            return Err(Error::Invalid(format!("bad file name {name:?}")));
        }
        let mut raw = self.read_inode(dir_ino)?;
        let blocks = self.dir_blocks(dir_ino, &raw)?;
        if le32(&raw, 0x20) & INDEX_FL != 0 {
            // An htree directory cannot take a linear insert without
            // updating the hash index. Convert it to a plain linear
            // directory first (root + interior nodes become ordinary
            // dirent blocks), which is always valid.
            self.dx_to_linear(dir_ino, &mut raw, &blocks)?;
        }
        let need = round4(8 + name.len());
        let usable = if self.csum { self.bs - 12 } else { self.bs };
        let iseed = self.inode_seed(dir_ino, &raw);

        for blk in &blocks {
            let mut b = self.read_block(*blk)?;
            if self.csum {
                let t = self.bs - 12;
                let is_tail =
                    le32(&b, t) == 0 && le16(&b, t + 4) == 12 && b[t + 6] == 0 && b[t + 7] == 0xDE;
                if !is_tail {
                    continue; // htree interior node or foreign shape: leave alone
                }
                if le32(&b, t + 8) != crc32c_raw(iseed, &b[..t]) {
                    return Err(Error::Corrupt(format!(
                        "directory block {blk} checksum mismatch"
                    )));
                }
            }
            let mut off = 0usize;
            let mut place: Option<(usize, usize, bool)> = None; // (off, new_rec, split)
            let mut ok = true;
            while off < usable {
                if off + 8 > usable {
                    ok = false;
                    break;
                }
                let e_ino = le32(&b, off);
                let rec = le16(&b, off + 4) as usize;
                let nl = b[off + 6] as usize;
                if rec < 8 || !rec.is_multiple_of(4) || off + rec > usable || 8 + nl > rec {
                    ok = false;
                    break;
                }
                if place.is_none() {
                    if e_ino == 0 {
                        if rec >= need {
                            place = Some((off, rec, false));
                        }
                    } else {
                        let nm = &b[off + 8..off + 8 + nl];
                        if nm != b"." && nm != b".." {
                            let actual = round4(8 + nl);
                            if rec - actual >= need {
                                place = Some((off, rec - actual, true));
                            }
                        }
                    }
                }
                off += rec;
            }
            if !ok || off != usable {
                return Err(Error::Corrupt(format!("malformed directory block {blk}")));
            }
            if let Some((poff, space, split)) = place {
                let at = if split {
                    let actual = round4(8 + b[poff + 6] as usize);
                    p16(&mut b, poff + 4, actual as u16);
                    poff + actual // new entry takes the remainder
                } else {
                    poff
                };
                self.put_dirent(&mut b, at, space, name, ino, ftype);
                if self.csum {
                    let t = self.bs - 12;
                    let c = crc32c_raw(iseed, &b[..t]);
                    p32(&mut b, t + 8, c);
                }
                self.write_block(*blk, b)?;
                let flags = le32(&raw, 0x20);
                if flags & INDEX_FL != 0 {
                    // The htree no longer describes this directory; fall back
                    // to a linear directory (always valid).
                    p32(&mut raw, 0x20, flags & !INDEX_FL);
                }
                p32(&mut raw, 0x0C, self.now);
                p32(&mut raw, 0x10, self.now);
                return self.write_inode(dir_ino, &mut raw);
            }
        }

        // Need a new directory block.
        let flags = le32(&raw, 0x20);
        let (mut ext, tree) = self.extents_of(dir_ino, &raw)?;
        let depth = le16(&raw, 0x28 + 6);
        let leaf = match (depth, tree.len()) {
            (0, 0) => None,
            (1, 1) => Some(tree[0]),
            _ => {
                return Err(Error::Unsupported(
                    "directory is full and its extent tree is too deep to grow".into(),
                ));
            }
        };
        let size = Self::inode_size_bytes(&raw);
        if size % self.bs as u64 != 0 {
            return Err(Error::Corrupt("directory size not block aligned".into()));
        }
        let new_l = (size / self.bs as u64) as u32;
        let goal_g = (dir_ino as usize - 1) / self.ipg;
        let runs = self.alloc_blocks(goal_g, 1)?;
        let (pb, _) = runs[0];
        let mut nb = vec![0u8; self.bs];
        let space = if self.csum { self.bs - 12 } else { self.bs };
        self.put_dirent(&mut nb, 0, space, name, ino, ftype);
        if self.csum {
            let t = self.bs - 12;
            p16(&mut nb, t + 4, 12);
            nb[t + 7] = 0xDE;
            let c = crc32c_raw(iseed, &nb[..t]);
            p32(&mut nb, t + 8, c);
        }
        self.write_block(pb, nb)?;
        let mut merged = false;
        if let Some(last) = ext.last_mut() {
            if last.uninit {
                return Err(Error::Unsupported(
                    "directory ends in uninitialised extent".into(),
                ));
            }
            if last.lblock + last.len == new_l
                && last.phys + last.len as u64 == pb
                && last.len < MAX_EXTENT_LEN
            {
                last.len += 1;
                merged = true;
            }
        }
        if !merged {
            ext.push(Extent {
                lblock: new_l,
                len: 1,
                phys: pb,
                uninit: false,
            });
        }
        let extra = self.write_extents(dir_ino, &mut raw, &ext, leaf, goal_g)?;
        let nsize = size + self.bs as u64;
        p32(&mut raw, 0x4, nsize as u32);
        p32(&mut raw, 0x6C, (nsize >> 32) as u32);
        let blocks512 = self.i_blocks(&raw) + (1 + extra) * (self.bs / 512) as u64;
        self.set_i_blocks(&mut raw, blocks512);
        p32(&mut raw, 0x20, flags & !INDEX_FL);
        p32(&mut raw, 0x0C, self.now);
        p32(&mut raw, 0x10, self.now);
        self.write_inode(dir_ino, &mut raw)
    }

    fn dx_to_linear(&mut self, dir_ino: u32, raw: &mut [u8], blocks: &[u64]) -> Result<()> {
        let iseed = self.inode_seed(dir_ino, raw);
        let tail = if self.csum { 12 } else { 0 };
        let t = self.bs - 12;
        let seal = |b: &mut Vec<u8>| {
            if tail != 0 {
                p32(b, t, 0);
                p16(b, t + 4, 12);
                b[t + 6] = 0;
                b[t + 7] = 0xDE;
                let c = crc32c_raw(iseed, &b[..t]);
                p32(b, t + 8, c);
            }
        };
        for (i, blk) in blocks.iter().enumerate() {
            let mut b = self.read_block(*blk)?;
            if i == 0 {
                let dot_ok = le32(&b, 0) == dir_ino
                    && le16(&b, 4) == 12
                    && b[6] == 1
                    && b[8] == b'.'
                    && b[12 + 6] == 2
                    && b[20] == b'.'
                    && b[21] == b'.';
                let dd_rec = le16(&b, 12 + 4) as usize;
                if !dot_ok || dd_rec != self.bs - 12 {
                    return Err(Error::Corrupt(
                        "indexed directory has unexpected root block".into(),
                    ));
                }
                // drop dx_root_info / dx entries; shrink ".." to leave room for the tail
                b[24..].fill(0);
                p16(&mut b, 12 + 4, (self.bs - 12 - tail) as u16);
                seal(&mut b);
            } else if le32(&b, 0) == 0 && le16(&b, 4) as usize == self.bs {
                // interior dx node: replace with an empty leaf block
                b.fill(0);
                p16(&mut b, 4, (self.bs - tail) as u16);
                seal(&mut b);
            } else {
                continue;
            }
            self.write_block(*blk, b)?;
        }
        let flags = le32(raw, 0x20);
        p32(raw, 0x20, flags & !INDEX_FL);
        Ok(())
    }

    fn put_dirent(&self, b: &mut [u8], at: usize, rec: usize, name: &str, ino: u32, ftype: u8) {
        p32(b, at, ino);
        p16(b, at + 4, rec as u16);
        b[at + 6] = name.len() as u8;
        b[at + 7] = ftype;
        b[at + 8..at + 8 + name.len()].copy_from_slice(name.as_bytes());
    }

    fn i_blocks(&self, raw: &[u8]) -> u64 {
        le32(raw, 0x1C) as u64 | (le16(raw, 0x74) as u64) << 32
    }
    fn set_i_blocks(&self, raw: &mut [u8], v: u64) {
        p32(raw, 0x1C, v as u32);
        p16(raw, 0x74, (v >> 32) as u16);
    }

    /// Store `ext` into the inode's extent root. Up to four extents live
    /// inline; more use one depth-1 leaf block (`leaf` = existing leaf).
    /// Returns the number of extra tree blocks newly allocated (0 or 1).
    fn write_extents(
        &mut self,
        ino: u32,
        raw: &mut [u8],
        ext: &[Extent],
        leaf: Option<u64>,
        goal_g: usize,
    ) -> Result<u64> {
        if ext.len() <= 4 && leaf.is_none() {
            raw[0x28..0x28 + 60].copy_from_slice(&Self::build_extent_root(ext));
            return Ok(0);
        }
        let max = (self.bs - 12) / 12;
        if ext.len() > max {
            return Err(Error::Unsupported(
                "too many extents for one leaf block".into(),
            ));
        }
        let (lb, newly) = match leaf {
            Some(l) => (l, 0),
            None => (self.alloc_blocks(goal_g, 1)?[0].0, 1),
        };
        let mut node = vec![0u8; self.bs];
        p16(&mut node, 0, 0xF30A);
        p16(&mut node, 2, ext.len() as u16);
        p16(&mut node, 4, max as u16);
        p16(&mut node, 6, 0);
        for (i, e) in ext.iter().enumerate() {
            let o = 12 + i * 12;
            p32(&mut node, o, e.lblock);
            let l = if e.uninit {
                e.len + MAX_EXTENT_LEN
            } else {
                e.len
            };
            p16(&mut node, o + 4, l as u16);
            p16(&mut node, o + 6, (e.phys >> 32) as u16);
            p32(&mut node, o + 8, e.phys as u32);
        }
        if self.csum {
            let end = 12 + max * 12;
            let c = crc32c_raw(self.inode_seed(ino, raw), &node[..end]);
            p32(&mut node, end, c);
        }
        self.write_block(lb, node)?;
        let mut root = [0u8; 60];
        p16(&mut root, 0, 0xF30A);
        p16(&mut root, 2, 1);
        p16(&mut root, 4, 4);
        p16(&mut root, 6, 1);
        p32(&mut root, 12, ext[0].lblock);
        p32(&mut root, 16, lb as u32);
        p16(&mut root, 20, (lb >> 32) as u16);
        raw[0x28..0x28 + 60].copy_from_slice(&root);
        Ok(newly)
    }

    fn build_extent_root(ext: &[Extent]) -> [u8; 60] {
        let mut r = [0u8; 60];
        p16(&mut r, 0, 0xF30A);
        p16(&mut r, 2, ext.len() as u16);
        p16(&mut r, 4, 4);
        p16(&mut r, 6, 0);
        for (i, e) in ext.iter().enumerate() {
            let o = 12 + i * 12;
            p32(&mut r, o, e.lblock);
            let l = if e.uninit {
                e.len + MAX_EXTENT_LEN
            } else {
                e.len
            };
            p16(&mut r, o + 4, l as u16);
            p16(&mut r, o + 6, (e.phys >> 32) as u16);
            p32(&mut r, o + 8, e.phys as u32);
        }
        r
    }

    // ------------------------------------------------------------ allocation

    fn group_blocks(&self, g: usize) -> (u64, usize) {
        let start = self.first_data_block + (g * self.bpg) as u64;
        let n = std::cmp::min(self.bpg as u64, self.blocks_count - start) as usize;
        (start, n)
    }

    /// Loads and sanity-checks the block bitmap of group `g`.
    fn load_block_bitmap(&self, g: usize, d: &[u8]) -> Result<Vec<u8>> {
        let bmb = self.gd_block_bitmap(d);
        let bm = self.read_block(bmb)?;
        if !self.bitmap_csum_ok(d, 0x18, 0x38, &bm[..self.bpg / 8]) {
            return Err(Error::Corrupt(format!(
                "group {g} block bitmap checksum mismatch"
            )));
        }
        let (start, n) = self.group_blocks(g);
        let rel = |blk: u64| -> Option<usize> {
            if blk >= start && blk < start + n as u64 {
                Some((blk - start) as usize)
            } else {
                None
            }
        };
        let itb = (self.ipg * self.inode_size).div_ceil(self.bs) as u64;
        let mut metas = vec![self.gd_block_bitmap(d), self.gd_inode_bitmap(d)];
        let it = self.gd_inode_table(d);
        metas.extend(it..it + itb);
        for m in metas {
            if let Some(i) = rel(m)
                && !bit(&bm, i)
            {
                return Err(Error::Corrupt(format!(
                    "group {g}: metadata block {m} not marked in-use in bitmap"
                )));
            }
        }
        let free = (0..n).filter(|&i| !bit(&bm, i)).count() as u32;
        if free != self.gd_free_blocks(d) {
            return Err(Error::Corrupt(format!(
                "group {g}: free block count {} disagrees with bitmap ({free})",
                self.gd_free_blocks(d)
            )));
        }
        Ok(bm)
    }

    /// Allocate `count` blocks as up to several runs, preferring group `goal`.
    fn alloc_blocks(&mut self, goal: usize, count: u64) -> Result<Vec<(u64, u32)>> {
        let mut need = count;
        let mut runs: Vec<(u64, u32)> = Vec::new();
        for k in 0..self.groups {
            if need == 0 {
                break;
            }
            let g = (goal + k) % self.groups;
            let mut d = self.read_gd(g)?;
            if self.gd_flags(&d) & BG_BLOCK_UNINIT != 0 || self.gd_free_blocks(&d) == 0 {
                continue;
            }
            let mut bm = self.load_block_bitmap(g, &d)?;
            let (start, n) = self.group_blocks(g);
            let mut taken = 0u32;
            let mut i = 0;
            while i < n && need > 0 {
                if bit(&bm, i) {
                    i += 1;
                    continue;
                }
                let run_start = i;
                while i < n
                    && !bit(&bm, i)
                    && (i - run_start) < need as usize
                    && (i - run_start) < MAX_EXTENT_LEN as usize
                {
                    set_bit(&mut bm, i);
                    i += 1;
                }
                let len = (i - run_start) as u32;
                taken += len;
                need -= len as u64;
                let phys = start + run_start as u64;
                match runs.last_mut() {
                    Some(l) if l.0 + l.1 as u64 == phys && l.1 + len <= MAX_EXTENT_LEN => {
                        l.1 += len
                    }
                    _ => runs.push((phys, len)),
                }
            }
            if taken == 0 {
                continue;
            }
            self.bitmap_csum_set(&mut d, 0x18, 0x38, &bm[..self.bpg / 8]);
            let nf = self.gd_free_blocks(&d) - taken;
            self.set_gd16(&mut d, 0xC, 0x2C, nf);
            let bmb = self.gd_block_bitmap(&d);
            self.write_block(bmb, {
                let mut full = self.read_block(bmb)?;
                full[..self.bpg / 8].copy_from_slice(&bm[..self.bpg / 8]);
                full
            })?;
            self.write_gd(g, &mut d)?;
            let sf = self.sb_free_blocks();
            self.set_sb_free_blocks(sf - taken as u64);
            self.stats.blocks_allocated += taken as u64;
        }
        if need > 0 {
            return Err(Error::NoSpace(format!(
                "not enough free blocks (short by {need}; BLOCK_UNINIT groups are not used)"
            )));
        }
        Ok(runs)
    }

    fn is_fs_metadata(&self, blk: u64, d: &[u8]) -> bool {
        let itb = (self.ipg * self.inode_size).div_ceil(self.bs) as u64;
        let it = self.gd_inode_table(d);
        blk < self.gdt_block + self.gdt_blocks
            || blk == self.gd_block_bitmap(d)
            || blk == self.gd_inode_bitmap(d)
            || (blk >= it && blk < it + itb)
    }

    fn free_blocks(&mut self, start: u64, len: u32) -> Result<()> {
        for blk in start..start + len as u64 {
            let g = ((blk - self.first_data_block) / self.bpg as u64) as usize;
            let mut d = self.read_gd(g)?;
            if self.gd_flags(&d) & BG_BLOCK_UNINIT != 0 {
                return Err(Error::Corrupt(format!(
                    "freeing block {blk} in BLOCK_UNINIT group"
                )));
            }
            if self.is_fs_metadata(blk, &d) {
                return Err(Error::Corrupt(format!(
                    "refusing to free metadata block {blk}"
                )));
            }
            let mut bm = self.load_block_bitmap(g, &d)?;
            let (gs, _) = self.group_blocks(g);
            let i = (blk - gs) as usize;
            if !bit(&bm, i) {
                return Err(Error::Corrupt(format!(
                    "block {blk} to free is not allocated"
                )));
            }
            clear_bit(&mut bm, i);
            self.bitmap_csum_set(&mut d, 0x18, 0x38, &bm[..self.bpg / 8]);
            let nf = self.gd_free_blocks(&d) + 1;
            self.set_gd16(&mut d, 0xC, 0x2C, nf);
            let bmb = self.gd_block_bitmap(&d);
            let mut full = self.read_block(bmb)?;
            full[..self.bpg / 8].copy_from_slice(&bm[..self.bpg / 8]);
            self.write_block(bmb, full)?;
            self.write_gd(g, &mut d)?;
            let sf = self.sb_free_blocks();
            self.set_sb_free_blocks(sf + 1);
            self.stats.blocks_freed += 1;
        }
        Ok(())
    }

    fn alloc_inode(&mut self, goal: usize, is_dir: bool) -> Result<u32> {
        for k in 0..self.groups {
            let g = (goal + k) % self.groups;
            let mut d = self.read_gd(g)?;
            if self.gd_flags(&d) & BG_INODE_UNINIT != 0 || self.gd_free_inodes(&d) == 0 {
                continue;
            }
            let ibm = self.gd_inode_bitmap(&d);
            let full = self.read_block(ibm)?;
            let nbytes = self.ipg / 8;
            if !self.bitmap_csum_ok(&d, 0x1A, 0x3A, &full[..nbytes]) {
                return Err(Error::Corrupt(format!(
                    "group {g} inode bitmap checksum mismatch"
                )));
            }
            let mut bm = full.clone();
            let free = (0..self.ipg).filter(|&i| !bit(&bm, i)).count() as u32;
            if free != self.gd_free_inodes(&d) {
                return Err(Error::Corrupt(format!(
                    "group {g}: free inode count disagrees with bitmap"
                )));
            }
            let unused = self.gd_itable_unused(&d) as usize;
            if unused > self.ipg {
                return Err(Error::Corrupt("itable_unused > inodes per group".into()));
            }
            let initialized = self.ipg - unused;
            if (initialized..self.ipg).any(|i| bit(&bm, i)) {
                return Err(Error::Corrupt(format!(
                    "group {g}: inode in use beyond itable_unused boundary"
                )));
            }
            let min_idx = if g == 0 {
                self.first_ino as usize - 1
            } else {
                0
            };
            let Some(idx) = (min_idx..self.ipg).find(|&i| !bit(&bm, i)) else {
                continue;
            };
            set_bit(&mut bm, idx);
            let ino = (g * self.ipg + idx + 1) as u32;
            if idx >= initialized {
                // gap inodes (idx > initialized) would be left uninitialised
                for gap in initialized..idx {
                    let gi = (g * self.ipg + gap + 1) as u32;
                    let (blk, off) = self.inode_pos(gi)?;
                    let mut b = self.read_block(blk)?;
                    b[off..off + self.inode_size].fill(0);
                    self.write_block(blk, b)?;
                }
                self.set_gd16(&mut d, 0x1C, 0x32, (self.ipg - (idx + 1)) as u32);
            }
            self.bitmap_csum_set(&mut d, 0x1A, 0x3A, &bm[..nbytes]);
            let nf = self.gd_free_inodes(&d) - 1;
            self.set_gd16(&mut d, 0xE, 0x2E, nf);
            if is_dir {
                let u = self.gd_used_dirs(&d) + 1;
                self.set_gd16(&mut d, 0x10, 0x30, u);
            }
            let mut out = full;
            out[..nbytes].copy_from_slice(&bm[..nbytes]);
            self.write_block(ibm, out)?;
            self.write_gd(g, &mut d)?;
            let fi = le32(&self.sb, 0x10);
            p32(&mut self.sb, 0x10, fi - 1);
            self.stats.inodes_allocated += 1;
            return Ok(ino);
        }
        Err(Error::NoSpace(
            "no free inode in an initialised group (INODE_UNINIT groups are not used)".into(),
        ))
    }

    // ---------------------------------------------------------- path walking

    fn new_inode_raw(
        &self,
        mode: u16,
        uid: u32,
        gid: u32,
        links: u16,
        size: u64,
        flags: u32,
    ) -> Vec<u8> {
        let mut r = vec![0u8; self.inode_size];
        p16(&mut r, 0, mode);
        p16(&mut r, 2, uid as u16);
        p16(&mut r, 0x78, (uid >> 16) as u16);
        p16(&mut r, 0x18, gid as u16);
        p16(&mut r, 0x7A, (gid >> 16) as u16);
        p16(&mut r, 0x1A, links);
        p32(&mut r, 0x4, size as u32);
        p32(&mut r, 0x6C, (size >> 32) as u32);
        p32(&mut r, 0x8, self.now);
        p32(&mut r, 0xC, self.now);
        p32(&mut r, 0x10, self.now);
        p32(&mut r, 0x20, flags);
        let generation = self
            .now
            .wrapping_mul(2654435761)
            .wrapping_add((self.stats.inodes_allocated + 1).wrapping_mul(40503));
        p32(&mut r, 0x64, generation);
        if self.inode_size > 128 {
            let want = le16(&self.sb, 0x15E) as usize;
            let mut ei = if want >= 4 { want } else { 32 };
            ei = ei.min(self.inode_size - 128) & !3;
            p16(&mut r, 0x80, ei as u16);
            if ei >= 24 {
                p32(&mut r, 0x90, self.now);
            }
        }
        r
    }

    fn mkdir(&mut self, parent: u32, name: &str, mode: u32, uid: u32, gid: u32) -> Result<u32> {
        let pg = (parent as usize - 1) / self.ipg;
        let ino = self.alloc_inode(pg, true)?;
        let g = (ino as usize - 1) / self.ipg;
        let runs = self.alloc_blocks(g, 1)?;
        let (pb, _) = runs[0];
        let mut raw = self.new_inode_raw(
            S_IFDIR | (mode & 0o7777) as u16,
            uid,
            gid,
            2,
            self.bs as u64,
            EXTENTS_FL,
        );
        let ext = [Extent {
            lblock: 0,
            len: 1,
            phys: pb,
            uninit: false,
        }];
        raw[0x28..0x28 + 60].copy_from_slice(&Self::build_extent_root(&ext));
        self.set_i_blocks(&mut raw, (self.bs / 512) as u64);
        let iseed = self.inode_seed(ino, &raw);
        let mut b = vec![0u8; self.bs];
        let tail = if self.csum { 12 } else { 0 };
        self.put_dirent(&mut b, 0, 12, ".", ino, 2);
        self.put_dirent(&mut b, 12, self.bs - 12 - tail, "..", parent, 2);
        if self.csum {
            let t = self.bs - 12;
            p16(&mut b, t + 4, 12);
            b[t + 7] = 0xDE;
            let c = crc32c_raw(iseed, &b[..t]);
            p32(&mut b, t + 8, c);
        }
        self.write_block(pb, b)?;
        self.write_inode(ino, &mut raw)?;
        self.add_dirent(parent, name, ino, 2)?;
        let mut praw = self.read_inode(parent)?;
        let links = le16(&praw, 0x1A);
        if links >= 65000 {
            return Err(Error::Unsupported(
                "parent directory link count too high".into(),
            ));
        }
        if links > 1 {
            p16(&mut praw, 0x1A, links + 1);
            self.write_inode(parent, &mut praw)?;
        }
        self.stats.dirs_created += 1;
        Ok(ino)
    }

    /// Resolve `comps` as a directory path from `/`, following symlinks in
    /// intermediate components. With `create`, missing directories are made.
    /// Returns the directory inode, or `None` if missing and not creating.
    pub fn walk_dir(
        &mut self,
        comps: &[&str],
        create: Option<(u32, u32, u32)>,
        created: &mut Vec<String>,
    ) -> Result<Option<u32>> {
        let mut stack = vec![ROOT_INO];
        let mut pending: Vec<String> = comps.iter().rev().map(|s| s.to_string()).collect();
        let mut hops = 0;
        let mut trail = String::new();
        while let Some(c) = pending.pop() {
            match c.as_str() {
                "" | "." => continue,
                ".." => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                    continue;
                }
                _ => {}
            }
            let cur = *stack.last().unwrap();
            let cur_raw = self.read_inode(cur)?;
            match self.lookup(cur, &cur_raw, &c)? {
                Some(ino) => {
                    let raw = self.read_inode(ino)?;
                    match le16(&raw, 0) & S_IFMT {
                        S_IFDIR => {
                            stack.push(ino);
                            trail.push('/');
                            trail.push_str(&c);
                        }
                        S_IFLNK => {
                            hops += 1;
                            if hops > 8 {
                                return Err(Error::Invalid("too many symlink hops".into()));
                            }
                            let t = self.read_symlink(ino, &raw)?;
                            if t.starts_with('/') {
                                stack.truncate(1);
                            }
                            for part in t.split('/').rev() {
                                pending.push(part.to_string());
                            }
                        }
                        _ => {
                            return Err(Error::Invalid(format!(
                                "path component {c:?} exists and is not a directory"
                            )));
                        }
                    }
                }
                None => match create {
                    Some((mode, uid, gid)) => {
                        let n = self.mkdir(cur, &c, mode, uid, gid)?;
                        stack.push(n);
                        trail.push('/');
                        trail.push_str(&c);
                        created.push(trail.clone());
                    }
                    None => return Ok(None),
                },
            }
        }
        Ok(stack.last().copied())
    }

    /// True if `path` (absolute) resolves to a directory.
    pub fn dir_exists(&mut self, path: &str) -> bool {
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        matches!(self.walk_dir(&comps, None, &mut Vec::new()), Ok(Some(_)))
    }

    /// True if `path` (absolute) names an existing non-directory-or-directory entry.
    pub fn exists(&mut self, path: &str) -> bool {
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let Some((name, parent)) = comps.split_last() else {
            return true;
        };
        match self.walk_dir(parent, None, &mut Vec::new()) {
            Ok(Some(p)) => self
                .read_inode(p)
                .ok()
                .and_then(|r| self.lookup(p, &r, name).ok().flatten())
                .is_some(),
            _ => false,
        }
    }

    /// Reads an existing regular file, or `None` if it does not exist.
    pub fn read_file(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let Some((name, parent)) = comps.split_last() else {
            return Err(Error::Invalid("empty path".into()));
        };
        let Some(p) = self.walk_dir(parent, None, &mut Vec::new())? else {
            return Ok(None);
        };
        let praw = self.read_inode(p)?;
        let Some(ino) = self.lookup(p, &praw, name)? else {
            return Ok(None);
        };
        let raw = self.read_inode(ino)?;
        if le16(&raw, 0) & S_IFMT != S_IFREG {
            return Err(Error::Invalid(format!("{path} is not a regular file")));
        }
        Ok(Some(self.read_data(ino, &raw)?))
    }

    // ---------------------------------------------------------- file writes

    /// Create or overwrite regular file `path`. Returns `(inode, created)`.
    #[allow(clippy::too_many_arguments)]
    pub fn put_file(
        &mut self,
        path: &str,
        data: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
        dir_mode: u32,
        enforce_parent_mode: bool,
        created_dirs: &mut Vec<String>,
    ) -> Result<(u32, bool)> {
        if !path.starts_with('/') {
            return Err(Error::Invalid(format!("path {path:?} must be absolute")));
        }
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let Some((name, parent_comps)) = comps.split_last() else {
            return Err(Error::Invalid("empty path".into()));
        };
        if *name == "." || *name == ".." {
            return Err(Error::Invalid(format!("bad file name in {path:?}")));
        }
        if data.len() as u64 > 16 << 20 {
            return Err(Error::Unsupported("file larger than 16 MiB".into()));
        }
        let parent = self
            .walk_dir(parent_comps, Some((dir_mode, uid, gid)), created_dirs)?
            .expect("create mode always yields a directory");
        if enforce_parent_mode && parent != ROOT_INO {
            let mut praw = self.read_inode(parent)?;
            let m = (le16(&praw, 0) & S_IFMT) | (dir_mode & 0o7777) as u16;
            p16(&mut praw, 0, m);
            self.write_inode(parent, &mut praw)?;
        }
        let praw = self.read_inode(parent)?;
        let existing = self.lookup(parent, &praw, name)?;
        let nblocks = (data.len() as u64).div_ceil(self.bs as u64);
        let goal_g;
        let (ino, mut raw, created) = match existing {
            Some(ino) => {
                let raw = self.read_inode(ino)?;
                if le16(&raw, 0) & S_IFMT != S_IFREG {
                    return Err(Error::Invalid(format!(
                        "{path} exists and is not a regular file; refusing to replace"
                    )));
                }
                let fl = le32(&raw, 0x20);
                if fl & BAD_FLAGS != 0 {
                    return Err(Error::Unsupported(format!(
                        "{path} has unsupported inode flags 0x{fl:x}"
                    )));
                }
                let (ext, tree) = self.extents_of(ino, &raw)?;
                for e in &ext {
                    self.free_blocks(e.phys, e.len)?;
                }
                for t in &tree {
                    self.free_blocks(*t, 1)?;
                }
                goal_g = (ino as usize - 1) / self.ipg;
                (ino, raw, false)
            }
            None => {
                let pg = (parent as usize - 1) / self.ipg;
                let ino = self.alloc_inode(pg, false)?;
                goal_g = (ino as usize - 1) / self.ipg;
                let raw = self.new_inode_raw(
                    S_IFREG | (mode & 0o7777) as u16,
                    uid,
                    gid,
                    1,
                    0,
                    EXTENTS_FL,
                );
                (ino, raw, true)
            }
        };
        let runs = if nblocks > 0 {
            self.alloc_blocks(goal_g, nblocks)?
        } else {
            vec![]
        };
        // write data
        let mut ext = Vec::new();
        let mut l = 0u32;
        let mut pos = 0usize;
        for (start, len) in &runs {
            ext.push(Extent {
                lblock: l,
                len: *len,
                phys: *start,
                uninit: false,
            });
            for k in 0..*len as u64 {
                let mut b = vec![0u8; self.bs];
                let n = std::cmp::min(self.bs, data.len() - pos);
                b[..n].copy_from_slice(&data[pos..pos + n]);
                pos += n;
                self.write_block(start + k, b)?;
            }
            l += len;
        }
        let extra = self.write_extents(ino, &mut raw, &ext, None, goal_g)?;
        let acl = le32(&raw, 0x68) as u64 | (le16(&raw, 0x76) as u64) << 32;
        let blocks512 = (nblocks + extra + if acl != 0 { 1 } else { 0 }) * (self.bs / 512) as u64;
        self.set_i_blocks(&mut raw, blocks512);
        p32(&mut raw, 0x4, data.len() as u32);
        p32(&mut raw, 0x6C, (data.len() as u64 >> 32) as u32);
        let m = (le16(&raw, 0) & S_IFMT) | (mode & 0o7777) as u16;
        p16(&mut raw, 0, m);
        p16(&mut raw, 2, uid as u16);
        p16(&mut raw, 0x78, (uid >> 16) as u16);
        p16(&mut raw, 0x18, gid as u16);
        p16(&mut raw, 0x7A, (gid >> 16) as u16);
        p32(&mut raw, 0xC, self.now);
        p32(&mut raw, 0x10, self.now);
        p32(&mut raw, 0x14, 0);
        self.write_inode(ino, &mut raw)?;
        if created {
            self.add_dirent(parent, name, ino, 1)?;
        }
        Ok((ino, created))
    }

    // ---------------------------------------------------------------- commit

    /// Flush every pending change to the image: data/metadata blocks first,
    /// then group descriptors, then the superblock last.
    pub fn commit(mut self) -> Result<()> {
        if self.overlay.is_empty() && self.stats.inodes_allocated == 0 {
            return Ok(());
        }
        // superblock into its block
        if self.csum {
            let c = crc32c_raw(!0, &self.sb[..0x3FC]);
            p32(&mut self.sb, 0x3FC, c);
        }
        let sb_blk = 1024 / self.bs as u64;
        let sb_off = 1024 % self.bs;
        let mut sbb = self.read_block(sb_blk)?;
        sbb[sb_off..sb_off + 1024].copy_from_slice(&self.sb);
        let gdt_range = self.gdt_block..self.gdt_block + self.gdt_blocks;
        let mut late: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut early: Vec<(u64, Vec<u8>)> = Vec::new();
        for (blk, data) in std::mem::take(&mut self.overlay) {
            if blk == sb_blk {
                continue;
            }
            if gdt_range.contains(&blk) {
                late.push((blk, data));
            } else {
                early.push((blk, data));
            }
        }
        for (blk, data) in early
            .into_iter()
            .chain(late)
            .chain(std::iter::once((sb_blk, sbb)))
        {
            self.file
                .write_all_at(&data, self.base + blk * self.bs as u64)?;
        }
        self.file.sync_all()?;
        Ok(())
    }
}
