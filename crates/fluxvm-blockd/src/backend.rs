// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

/// A fixed-size random-access block store. Methods are synchronous; the NBD
/// server runs them on the blocking pool.
///
/// Extension seams (encryption, lazy pull) are expressed as wrappers
/// implementing this trait; see [`crate::seams`].
pub trait BlockBackend: Send + Sync {
    fn size(&self) -> u64;
    /// Fill `buf` from `off`. Caller guarantees `off + buf.len() <= size()`.
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&self, off: u64, data: &[u8]) -> io::Result<()>;
    /// Make completed writes durable.
    fn flush(&self) -> io::Result<()>;
    /// Make the range read back as zeros (and release space where possible).
    fn write_zeroes(&self, off: u64, len: u64) -> io::Result<()>;
    /// NBD_CMD_TRIM. Contents afterwards are unspecified by the protocol;
    /// the default zeroes the range.
    fn trim(&self, off: u64, len: u64) -> io::Result<()> {
        self.write_zeroes(off, len)
    }
    /// True when the backend can no longer accept writes (a frozen overlay that has children, a read-only lazy
    /// base). An export of it is then served read-only, so the guest sees a read-only disk instead of EIO.
    fn is_read_only(&self) -> bool {
        false
    }
}

pub(crate) fn oob() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "range outside device")
}

/// Plain file used as a disk image.
pub struct RawFile {
    file: File,
    size: u64,
}

impl RawFile {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }
}

impl BlockBackend for RawFile {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        if off
            .checked_add(buf.len() as u64)
            .is_none_or(|e| e > self.size)
        {
            return Err(oob());
        }
        let mut done = 0;
        while done < buf.len() {
            let n = self.file.read_at(&mut buf[done..], off + done as u64)?;
            if n == 0 {
                buf[done..].fill(0);
                break;
            }
            done += n;
        }
        Ok(())
    }
    fn write_at(&self, off: u64, data: &[u8]) -> io::Result<()> {
        if off
            .checked_add(data.len() as u64)
            .is_none_or(|e| e > self.size)
        {
            return Err(oob());
        }
        self.file.write_all_at(data, off)
    }
    fn flush(&self) -> io::Result<()> {
        self.file.sync_all()
    }
    fn write_zeroes(&self, off: u64, len: u64) -> io::Result<()> {
        if off.checked_add(len).is_none_or(|e| e > self.size) {
            return Err(oob());
        }
        crate::sparse::zero_range(&self.file, off, len)
    }
}
