// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Hole punching with a write-zeros fallback.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

const ALIGN: u64 = 4096;

/// Try to deallocate `[off, off+len)`; must be filesystem-block aligned.
fn punch(file: &File, off: u64, len: u64) -> io::Result<()> {
    let fd = file.as_raw_fd();
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct FPunchHole {
            fp_flags: u32,
            reserved: u32,
            fp_offset: libc::off_t,
            fp_length: libc::off_t,
        }
        const F_PUNCHHOLE: libc::c_int = 99;
        let arg = FPunchHole {
            fp_flags: 0,
            reserved: 0,
            fp_offset: off as _,
            fp_length: len as _,
        };
        let r = unsafe { libc::fcntl(fd, F_PUNCHHOLE, &arg) };
        if r == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(target_os = "linux")]
    {
        let r = unsafe {
            libc::fallocate(
                fd,
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                off as libc::off_t,
                len as libc::off_t,
            )
        };
        if r == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (fd, off, len);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

fn write_zeros(file: &File, mut off: u64, mut len: u64) -> io::Result<()> {
    let z = [0u8; 64 * 1024];
    while len > 0 {
        let n = len.min(z.len() as u64) as usize;
        file.write_all_at(&z[..n], off)?;
        off += n as u64;
        len -= n as u64;
    }
    Ok(())
}

/// Make `[off, off+len)` read as zeros, punching holes for the aligned core
/// and writing zeros for ragged edges or if punching is unsupported.
pub fn zero_range(file: &File, off: u64, len: u64) -> io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    let end = off + len;
    let a_start = off.div_ceil(ALIGN) * ALIGN;
    let a_end = end / ALIGN * ALIGN;
    if a_start >= a_end {
        return write_zeros(file, off, len);
    }
    write_zeros(file, off, a_start - off)?;
    if punch(file, a_start, a_end - a_start).is_err() {
        write_zeros(file, a_start, a_end - a_start)?;
    }
    write_zeros(file, a_end, end - a_end)
}

/// Bytes actually allocated on disk (st_blocks * 512).
pub fn allocated_bytes(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.blocks() * 512)
}
