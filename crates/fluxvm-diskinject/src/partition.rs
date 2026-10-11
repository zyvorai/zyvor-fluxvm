// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! GPT / MBR parsing. Only locates candidate byte ranges; deciding which one
//! is the root filesystem is done by probing them (see `lib.rs`).

use crate::{Error, Result};
use std::fs::File;
use std::os::unix::fs::FileExt;

/// Linux root-partition type GUIDs (Discoverable Partitions Spec), used only
/// to rank candidates.
const ROOT_GUIDS: &[&str] = &[
    "4f68bce3-e8cd-4db1-96e7-fbcaf984b709", // x86-64
    "b921b045-1df0-41c3-af44-4c6f280d3fae", // aarch64
    "69dad710-2ce4-4e3c-b16c-21a1d49abed3", // arm32
    "44479540-f297-41b2-9af7-d131d5f0458a", // x86
];
const LINUX_DATA_GUID: &str = "0fc63daf-8483-4772-8e79-3d69d8477de4";
const ESP_GUID: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const BIOS_BOOT_GUID: &str = "21686148-6449-6e6f-744e-656564454649";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 1-based partition number as listed in the table; 0 for a bare
    /// filesystem with no partition table.
    pub index: u32,
    pub offset: u64,
    pub len: u64,
    /// Partition type is an explicit "Linux root" type / MBR 0x83 / GPT Linux data.
    pub root_rank: u8,
}

fn guid_string(b: &[u8]) -> String {
    // GPT GUIDs are mixed endian.
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u32::from_le_bytes(b[0..4].try_into().unwrap()),
        u16::from_le_bytes(b[4..6].try_into().unwrap()),
        u16::from_le_bytes(b[6..8].try_into().unwrap()),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn read_at(f: &File, off: u64, len: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    f.read_exact_at(&mut v, off)?;
    Ok(v)
}

/// Lists candidate filesystem regions of `file` (length `file_len`).
/// A disk with no recognisable table is returned as a single whole-disk
/// candidate (index 0).
pub fn candidates(file: &File, file_len: u64) -> Result<Vec<Candidate>> {
    if file_len < 4096 {
        return Err(Error::Invalid("disk image too small".into()));
    }
    let head = read_at(file, 0, 4)?;
    if &head == b"QFI\xfb" {
        return Err(Error::Unsupported(
            "disk is qcow2; convert to raw first (this injector only edits raw images)".into(),
        ));
    }
    // Sector size: 512, or 4096 for native-4K GPT disks.
    for ss in [512u64, 4096] {
        if file_len < ss * 2 {
            continue;
        }
        let hdr = read_at(file, ss, 92)?;
        if &hdr[0..8] == b"EFI PART" {
            return parse_gpt(file, file_len, ss, &hdr);
        }
    }
    let mbr = read_at(file, 0, 512)?;
    if mbr[510] == 0x55 && mbr[511] == 0xAA {
        let mut out = Vec::new();
        let mut any_part = false;
        for i in 0..4 {
            let e = &mbr[446 + i * 16..446 + (i + 1) * 16];
            let ty = e[4];
            let lba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
            let cnt = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
            if ty == 0 || cnt == 0 {
                continue;
            }
            any_part = true;
            if ty == 0xEE {
                // protective MBR without a readable GPT header
                continue;
            }
            // Skip extended containers (0x05/0x0f/0x85) and clearly non-Linux
            // types we would never want to probe.
            if matches!(ty, 0x05 | 0x0f | 0x85 | 0x82 | 0xef) {
                continue;
            }
            let off = lba * 512;
            let len = cnt * 512;
            if off + len > file_len {
                return Err(Error::Corrupt(format!(
                    "MBR partition {} extends past end of image",
                    i + 1
                )));
            }
            out.push(Candidate {
                index: i as u32 + 1,
                offset: off,
                len,
                root_rank: if ty == 0x83 { 1 } else { 0 },
            });
        }
        // A bare ext4 filesystem can have boot-code bytes that look like a
        // signature; only trust the MBR if it yielded something.
        if any_part && !out.is_empty() {
            return Ok(out);
        }
    }
    // No table: the whole image may itself be a filesystem.
    Ok(vec![Candidate {
        index: 0,
        offset: 0,
        len: file_len,
        root_rank: 0,
    }])
}

fn parse_gpt(file: &File, file_len: u64, ss: u64, hdr: &[u8]) -> Result<Vec<Candidate>> {
    let entries_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let count = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as u64;
    let esize = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as u64;
    if !(128..=4096).contains(&esize) || count == 0 || count > 1024 {
        return Err(Error::Corrupt("implausible GPT entry array".into()));
    }
    let off = entries_lba
        .checked_mul(ss)
        .ok_or_else(|| Error::Corrupt("GPT entry LBA overflow".into()))?;
    if off + count * esize > file_len {
        return Err(Error::Corrupt("GPT entry array past end of image".into()));
    }
    let arr = read_at(file, off, (count * esize) as usize)?;
    let mut out = Vec::new();
    for i in 0..count as usize {
        let e = &arr[i * esize as usize..(i + 1) * esize as usize];
        if e[0..16].iter().all(|&b| b == 0) {
            continue;
        }
        let ty = guid_string(&e[0..16]);
        let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
        let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
        if last < first {
            return Err(Error::Corrupt(format!(
                "GPT partition {} has last < first",
                i + 1
            )));
        }
        if ty == ESP_GUID || ty == BIOS_BOOT_GUID {
            continue;
        }
        let o = first * ss;
        let l = (last - first + 1) * ss;
        if o + l > file_len {
            return Err(Error::Corrupt(format!(
                "GPT partition {} extends past end of image",
                i + 1
            )));
        }
        let rank = if ROOT_GUIDS.contains(&ty.as_str()) {
            2
        } else if ty == LINUX_DATA_GUID {
            1
        } else {
            0
        };
        out.push(Candidate {
            index: i as u32 + 1,
            offset: o,
            len: l,
            root_rank: rank,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn mbr_single_linux_partition() {
        let mut img = vec![0u8; 4 << 20];
        let e = 446;
        img[e + 4] = 0x83;
        img[e + 8..e + 12].copy_from_slice(&2048u32.to_le_bytes());
        img[e + 12..e + 16].copy_from_slice(&1024u32.to_le_bytes());
        img[510] = 0x55;
        img[511] = 0xAA;
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&img).unwrap();
        let c = candidates(&f, img.len() as u64).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].offset, 2048 * 512);
        assert_eq!(c[0].len, 1024 * 512);
        assert_eq!(c[0].root_rank, 1);
    }

    #[test]
    fn gpt_partitions_and_esp_skipped() {
        let mut img = vec![0u8; 4 << 20];
        img[512..520].copy_from_slice(b"EFI PART");
        img[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
        img[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes());
        img[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
        let put = |img: &mut Vec<u8>, slot: usize, g: [u8; 16], first: u64, last: u64| {
            let o = 1024 + slot * 128;
            img[o..o + 16].copy_from_slice(&g);
            img[o + 32..o + 40].copy_from_slice(&first.to_le_bytes());
            img[o + 40..o + 48].copy_from_slice(&last.to_le_bytes());
        };
        // ESP c12a7328-f81f-11d2-ba4b-00a0c93ec93b (mixed endian)
        let esp = [
            0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
            0xc9, 0x3b,
        ];
        // aarch64 root b921b045-1df0-41c3-af44-4c6f280d3fae
        let root = [
            0x45, 0xb0, 0x21, 0xb9, 0xf0, 0x1d, 0xc3, 0x41, 0xaf, 0x44, 0x4c, 0x6f, 0x28, 0x0d,
            0x3f, 0xae,
        ];
        put(&mut img, 0, esp, 64, 127);
        put(&mut img, 1, root, 128, 4095);
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&img).unwrap();
        let c = candidates(&f, img.len() as u64).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].index, 2);
        assert_eq!(c[0].offset, 128 * 512);
        assert_eq!(c[0].root_rank, 2);
    }

    #[test]
    fn qcow2_refused() {
        let mut img = vec![0u8; 8192];
        img[0..4].copy_from_slice(b"QFI\xfb");
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&img).unwrap();
        assert!(matches!(candidates(&f, 8192), Err(Error::Unsupported(_))));
    }
}
