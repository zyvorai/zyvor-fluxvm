// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! KVM dirty-log incremental snapshot support (design from `docs/ROADMAP-DENSITY.md`).
//!
//! Not yet wired into the live snapshot path. This module holds the data
//! structures and the planned layout for FLUXKVM1 v6 deltas so the rest of
//! the work can land incrementally.

use std::path::Path;

/// Magic for a dirty-log delta file.
pub const DELTA_MAGIC: &[u8; 8] = b"FLUXKVMD";

/// Planned on-disk layout for an incremental (delta) snapshot.
///
/// ```text
/// [8]  magic "FLUXKVMD"
/// [8]  parent snapshot id (or hash)
/// [8]  page count (u64)
/// then for each dirty page:
///   [8]  gfn (u64)
///   [4096] page bytes
/// ```
///
/// A v6 `FLUXKVM1` vmstate will carry a `parent` reference and a `delta` flag.
/// Restore applies the base `snap.mem` (privately mapped) then patches the
/// delta pages over it.
#[derive(Debug, Clone)]
pub struct DirtyDelta {
    pub parent_id: String,
    pub pages: Vec<(u64, [u8; 4096])>,
}

impl DirtyDelta {
    /// Write the delta to disk (skeleton).
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        // TODO: implement once the KVM dirty-log capture path exists.
        let _ = path;
        Ok(())
    }

    /// Read a delta from disk (skeleton).
    pub fn read_from(path: &Path) -> std::io::Result<Self> {
        let _ = path;
        Ok(Self {
            parent_id: String::new(),
            pages: Vec::new(),
        })
    }
}

/// Software bitmap for host-side device writes that KVM dirty log does not see.
///
/// Virtio queue completions and other host writes through `host_ptr()` must
/// also set bits here (or the delta must conservatively include every page
/// the queue service touched since the last snapshot).
pub struct SoftwareDirtyBitmap {
    bits: Vec<u64>,
    page_count: usize,
}

impl SoftwareDirtyBitmap {
    pub fn new(guest_ram_bytes: usize) -> Self {
        let page_count = guest_ram_bytes.div_ceil(4096);
        let words = page_count.div_ceil(64);
        Self {
            bits: vec![0u64; words],
            page_count,
        }
    }

    pub fn set(&mut self, gfn: usize) {
        if gfn < self.page_count {
            let word = gfn / 64;
            let bit = gfn % 64;
            self.bits[word] |= 1u64 << bit;
        }
    }

    pub fn is_dirty(&self, gfn: usize) -> bool {
        if gfn >= self.page_count {
            return false;
        }
        let word = gfn / 64;
        let bit = gfn % 64;
        (self.bits[word] & (1u64 << bit)) != 0
    }

    pub fn clear(&mut self) {
        for w in &mut self.bits {
            *w = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_bitmap_roundtrip() {
        let mut bm = SoftwareDirtyBitmap::new(4096 * 128);
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
}
