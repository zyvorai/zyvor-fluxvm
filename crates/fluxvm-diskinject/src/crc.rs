// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! CRC32C (Castagnoli) in the "raw" form ext4 uses: caller-supplied seed, no
//! pre- or post-inversion.

const POLY: u32 = 0x82F6_3B78;

const fn make_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u32; 256] = make_table();

/// `crc32c(seed, data)` exactly as the Linux kernel's `ext4_chksum` computes it.
pub fn crc32c_raw(seed: u32, data: &[u8]) -> u32 {
    let mut c = seed;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_check_vector() {
        // Standard CRC-32C check value: init/final-xor 0xFFFFFFFF.
        assert_eq!(crc32c_raw(!0, b"123456789") ^ !0, 0xE306_9283);
    }

    #[test]
    fn rfc3720_vectors() {
        let std = |d: &[u8]| crc32c_raw(!0, d) ^ !0;
        assert_eq!(std(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(std(&[0xFFu8; 32]), 0x62A8_AB43);
        let asc: Vec<u8> = (0u8..32).collect();
        assert_eq!(std(&asc), 0x46DD_794E);
        let desc: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(std(&desc), 0x113F_DB5C);
    }

    #[test]
    fn chaining_equals_single_pass() {
        let data = b"ext4 superblock + inode + generation";
        let full = crc32c_raw(0x1234_5678, data);
        for split in 0..=data.len() {
            let (a, b) = data.split_at(split);
            assert_eq!(crc32c_raw(crc32c_raw(0x1234_5678, a), b), full);
        }
    }
}
