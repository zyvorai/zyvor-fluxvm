// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Reading Cirrus Labs' Tart VM images, so `fluxctl vm-image pull` can take the macOS and Linux images people already
//! publish for Tart (for example `ghcr.io/cirruslabs/macos-…`) and turn them into a FluxVM bundle.
//!
//! Format, from Tart's source (`Sources/tart/OCI/Manifest.swift`, `OCI/Layerizer/DiskV2.swift`, `VMConfig.swift`):
//!
//! - An OCI image manifest whose `config` descriptor is a Docker-compatibility stub; the real VM config is
//!   `layers[0]` ([`MT_CONFIG`], uncompressed JSON), the NVRAM is the last layer ([`MT_NVRAM`], uncompressed), and
//!   everything between is the disk ([`MT_DISK_V2`]): `disk.img` cut into 512 MiB slices, each compressed on its own
//!   with Apple's framed LZ4 (`COMPRESSION_LZ4`), annotated with its uncompressed size and digest. Slices carry no
//!   offset; they are strictly sequential.
//! - Stacked images (ASIF overlay layers), ASIF disks and the legacy `disk.v1` format are refused with a clear error.
//!
//! Mapping onto a FluxVM bundle: `disk.img` → `disk.raw`; macOS `nvram.bin` (`VZMacAuxiliaryStorage`) →
//! `auxiliary.bin`; `hardwareModel` (base64 of `VZMacHardwareModel.dataRepresentation`) → `hardware.bin`; Linux
//! `nvram.bin` (`VZEFIVariableStore`) → `efi.bin`. Tart's `ecid` (the machine identifier) is deliberately dropped:
//! every FluxVM clone gets its own identity.

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;

pub const MT_PREFIX: &str = "application/vnd.cirruslabs.tart.";
pub const MT_CONFIG: &str = "application/vnd.cirruslabs.tart.config.v1";
pub const MT_DISK_V2: &str = "application/vnd.cirruslabs.tart.disk.v2";
pub const MT_DISK_V1: &str = "application/vnd.cirruslabs.tart.disk.v1";
pub const MT_ASIF_OVERLAY: &str = "application/vnd.cirruslabs.tart.disk.asif.overlay.v1";
pub const MT_NVRAM: &str = "application/vnd.cirruslabs.tart.nvram.v1";
pub const ANN_UNCOMPRESSED_SIZE: &str = "org.cirruslabs.tart.uncompressed-size";
pub const ANN_UNCOMPRESSED_DIGEST: &str = "org.cirruslabs.tart.uncompressed-content-digest";
/// Tart's slice size; a slice claiming more is refused.
pub const MAX_SLICE: u64 = 512 * 1024 * 1024;
/// Largest NVRAM / config blob accepted.
pub const MAX_SMALL_BLOB: u64 = 64 * 1024 * 1024;

/// Whether a manifest with these layer media types is a Tart image.
pub fn is_tart<'a>(media_types: impl IntoIterator<Item = &'a str>) -> bool {
    media_types.into_iter().any(|m| m.starts_with(MT_PREFIX))
}

fn default_os() -> String {
    "darwin".into()
}

fn default_arch() -> String {
    "arm64".into()
}

/// The fields of Tart's `config.json` that matter here.
#[derive(Debug, Clone, Deserialize)]
pub struct TartConfig {
    pub version: u32,
    #[serde(default = "default_os")]
    pub os: String,
    #[serde(default = "default_arch")]
    pub arch: String,
    #[serde(rename = "cpuCount", default)]
    pub cpu_count: u32,
    /// Bytes.
    #[serde(rename = "memorySize", default)]
    pub memory_size: u64,
    #[serde(rename = "hardwareModel", default)]
    pub hardware_model: Option<String>,
    #[serde(rename = "diskFormat", default)]
    pub disk_format: Option<String>,
}

impl TartConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let c: TartConfig = serde_json::from_slice(bytes).context("parsing the Tart VM config")?;
        ensure!(
            c.version == 1,
            "unsupported Tart config version {}",
            c.version
        );
        ensure!(
            c.arch == "arm64",
            "this Tart image is {}; Apple silicon runs arm64 images only",
            c.arch
        );
        ensure!(
            c.os == "darwin" || c.os == "linux",
            "unknown Tart guest OS {:?}",
            c.os
        );
        if let Some(f) = &c.disk_format {
            ensure!(
                f == "raw",
                "this Tart image has an {f} disk; only raw disks can be converted"
            );
        }
        Ok(c)
    }

    /// FluxVM's name for the guest OS.
    pub fn fluxvm_os(&self) -> &'static str {
        if self.os == "darwin" {
            "macos"
        } else {
            "linux"
        }
    }

    /// `hardware.bin`: the raw `VZMacHardwareModel.dataRepresentation` (macOS guests only).
    pub fn hardware_model_bytes(&self) -> Result<Option<Vec<u8>>> {
        if self.os != "darwin" {
            return Ok(None);
        }
        use base64::Engine as _;
        let b64 = self
            .hardware_model
            .as_deref()
            .context("this macOS Tart image has no hardwareModel")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("hardwareModel is not base64")?;
        ensure!(!bytes.is_empty(), "hardwareModel is empty");
        Ok(Some(bytes))
    }

    /// The bundle file the NVRAM layer becomes.
    pub fn nvram_file(&self) -> &'static str {
        if self.os == "darwin" {
            "auxiliary.bin"
        } else {
            "efi.bin"
        }
    }
}

/// One disk slice of a Tart image, with the offset worked out from the slices before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice {
    /// Index into the manifest's layers.
    pub layer: usize,
    pub offset: u64,
    pub size: u64,
    /// `sha256:<hex>` of the uncompressed slice, when the image carries it.
    pub digest: Option<String>,
}

/// Checks the layer layout (config first, NVRAM last, raw disk slices between) and plans the disk. `layers` are
/// `(media type, annotations)`; returns the slices and the disk's total size.
pub fn plan(layers: &[(&str, &BTreeMap<String, String>)]) -> Result<(Vec<Slice>, u64)> {
    ensure!(
        layers.len() >= 2,
        "a Tart image needs at least a config and an NVRAM layer"
    );
    ensure!(
        layers[0].0 == MT_CONFIG,
        "the first layer of a Tart image must be its config, not {:?}",
        layers[0].0
    );
    ensure!(
        layers[layers.len() - 1].0 == MT_NVRAM,
        "the last layer of a Tart image must be its NVRAM, not {:?}",
        layers[layers.len() - 1].0
    );
    let mut slices = Vec::new();
    let mut offset = 0u64;
    for (i, (mt, ann)) in layers.iter().enumerate().take(layers.len() - 1).skip(1) {
        match *mt {
            MT_DISK_V2 => {}
            MT_ASIF_OVERLAY => {
                bail!("stacked Tart images (ASIF overlays) are not supported; pull a flat image")
            }
            MT_DISK_V1 => bail!(
                "Tart's legacy disk.v1 format is not supported; re-push the image with a current Tart"
            ),
            MT_CONFIG | MT_NVRAM => {
                bail!("a Tart image has exactly one config and one NVRAM layer")
            }
            other => bail!("unexpected layer {other:?} in a Tart image"),
        }
        let size: u64 = ann
            .get(ANN_UNCOMPRESSED_SIZE)
            .with_context(|| format!("disk layer {i} has no {ANN_UNCOMPRESSED_SIZE}"))?
            .parse()
            .with_context(|| format!("disk layer {i} has a bad uncompressed size"))?;
        ensure!(
            size > 0 && size <= MAX_SLICE,
            "disk layer {i} claims {size} bytes"
        );
        let digest = ann.get(ANN_UNCOMPRESSED_DIGEST).cloned();
        if let Some(d) = &digest {
            crate::oci_registry::validate_digest(d)?;
        }
        slices.push(Slice {
            layer: i,
            offset,
            size,
            digest,
        });
        offset = offset.checked_add(size).context("disk size overflows")?;
    }
    ensure!(!slices.is_empty(), "this Tart image has no disk layers");
    Ok((slices, offset))
}

/// Decodes one raw LZ4 block, appending to `out`. Matches may reach back into anything already in `out` (Apple's
/// framing lets a block refer to the previous block). Never grows `out` past `limit`.
pub fn lz4_block_decode(src: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<()> {
    let mut i = 0usize;
    let read_len = |i: &mut usize, base: usize| -> Result<usize> {
        let mut n = base;
        if base == 15 {
            loop {
                let b = *src.get(*i).context("truncated LZ4 length")?;
                *i += 1;
                n = n.checked_add(b as usize).context("LZ4 length overflow")?;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(n)
    };
    while i < src.len() {
        let token = src[i];
        i += 1;
        let lit = read_len(&mut i, (token >> 4) as usize)?;
        let end = i.checked_add(lit).context("LZ4 literal overflow")?;
        ensure!(end <= src.len(), "truncated LZ4 literals");
        ensure!(out.len() + lit <= limit, "LZ4 block decodes past its size");
        out.extend_from_slice(&src[i..end]);
        i = end;
        if i == src.len() {
            return Ok(());
        }
        ensure!(i + 2 <= src.len(), "truncated LZ4 match offset");
        let offset = u16::from_le_bytes([src[i], src[i + 1]]) as usize;
        i += 2;
        ensure!(
            offset != 0 && offset <= out.len(),
            "bad LZ4 match offset {offset}"
        );
        let len = read_len(&mut i, (token & 15) as usize)? + 4;
        ensure!(out.len() + len <= limit, "LZ4 block decodes past its size");
        let start = out.len() - offset;
        for k in 0..len {
            let b = out[start + k];
            out.push(b);
        }
    }
    Ok(())
}

/// Decodes Apple's framed LZ4 (`COMPRESSION_LZ4`): `bv41` compressed blocks, `bv4-` stored blocks, `bv4$` end. The
/// result must be exactly `expected` bytes.
pub fn apple_lz4_decode(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    let mut i = 0usize;
    let u32_at = |i: usize| -> Result<usize> {
        let b = data.get(i..i + 4).context("truncated LZ4 frame header")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    loop {
        let magic = data
            .get(i..i + 4)
            .context("LZ4 stream ends without its end marker")?;
        i += 4;
        match magic {
            b"bv41" => {
                let decoded = u32_at(i)?;
                let encoded = u32_at(i + 4)?;
                i += 8;
                let block = data.get(i..i + encoded).context("truncated LZ4 block")?;
                i += encoded;
                let before = out.len();
                let limit = before.checked_add(decoded).context("LZ4 size overflow")?;
                ensure!(
                    limit <= expected,
                    "LZ4 stream decodes past {expected} bytes"
                );
                lz4_block_decode(block, &mut out, limit)?;
                ensure!(
                    out.len() == limit,
                    "LZ4 block decoded to {} bytes, its header says {decoded}",
                    out.len() - before
                );
            }
            b"bv4-" => {
                let n = u32_at(i)?;
                i += 4;
                let raw = data.get(i..i + n).context("truncated stored block")?;
                i += n;
                ensure!(
                    out.len() + n <= expected,
                    "LZ4 stream decodes past {expected} bytes"
                );
                out.extend_from_slice(raw);
            }
            b"bv4$" => break,
            other => bail!("not an Apple LZ4 stream (block magic {other:?})"),
        }
    }
    ensure!(
        out.len() == expected,
        "LZ4 stream decoded to {} bytes, expected {expected}",
        out.len()
    );
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `abc` then a match of 9 at offset 3, then a literal-only `x`: "abcabcabcabcx".
    pub(crate) const BLOCK: &[u8] = &[0x35, b'a', b'b', b'c', 3, 0, 0x10, b'x'];

    pub(crate) fn frame(blocks: &[(&[u8], Option<usize>)]) -> Vec<u8> {
        let mut v = Vec::new();
        for (data, decoded) in blocks {
            match decoded {
                Some(d) => {
                    v.extend_from_slice(b"bv41");
                    v.extend_from_slice(&(*d as u32).to_le_bytes());
                    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
                }
                None => {
                    v.extend_from_slice(b"bv4-");
                    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
                }
            }
            v.extend_from_slice(data);
        }
        v.extend_from_slice(b"bv4$");
        v
    }

    #[test]
    fn lz4_blocks_decode_with_overlapping_matches() {
        let mut out = Vec::new();
        lz4_block_decode(BLOCK, &mut out, 100).unwrap();
        assert_eq!(out, b"abcabcabcabcx");
        // Long literal and match lengths use the 15 + 255… extension.
        let mut src = vec![0xF0 | 0x0F, 3];
        src.extend(std::iter::repeat_n(b'z', 18));
        src.extend_from_slice(&[1, 0, 0]); // offset 1, match length 15 + 0 + 4 = 19
        let mut out = Vec::new();
        lz4_block_decode(&src, &mut out, 100).unwrap();
        assert_eq!(out, vec![b'z'; 37]);
    }

    #[test]
    fn hostile_lz4_is_refused() {
        let mut out = Vec::new();
        assert!(
            lz4_block_decode(&[0x10, b'a', 5, 0], &mut out, 100).is_err(),
            "offset past output"
        );
        let mut out = Vec::new();
        assert!(
            lz4_block_decode(&[0x10, b'a', 0, 0], &mut out, 100).is_err(),
            "zero offset"
        );
        let mut out = Vec::new();
        assert!(
            lz4_block_decode(BLOCK, &mut out, 5).is_err(),
            "past the limit"
        );
        let mut out = Vec::new();
        assert!(
            lz4_block_decode(&[0x50, b'a'], &mut out, 100).is_err(),
            "truncated literals"
        );
    }

    #[test]
    fn apple_frames_decode_across_blocks() {
        // A stored block, then a compressed block whose match reaches back into the stored one.
        // Match length 15 + extension 0 + 4 = 19 at offset 3, then a literal "!".
        let second: &[u8] = &[0x0F, 3, 0, 0, 0x10, b'!'];
        let data = frame(&[(b"xyz".as_slice(), None), (second, Some(20))]);
        let out = apple_lz4_decode(&data, 23).unwrap();
        assert_eq!(&out[..], b"xyzxyzxyzxyzxyzxyzxyzx!");
        assert!(apple_lz4_decode(&data, 22).is_err());
        assert!(
            apple_lz4_decode(&data[..data.len() - 4], 23).is_err(),
            "no end marker"
        );
        assert!(
            apple_lz4_decode(b"\x04\x22\x4d\x18", 1).is_err(),
            "LZ4 frame format is not Apple's"
        );
    }

    fn ann(size: u64) -> BTreeMap<String, String> {
        [(ANN_UNCOMPRESSED_SIZE.to_owned(), size.to_string())].into()
    }

    #[test]
    fn layouts_are_planned_and_checked() {
        let none = BTreeMap::new();
        let (a, b) = (ann(512), ann(100));
        let layers = [
            (MT_CONFIG, &none),
            (MT_DISK_V2, &a),
            (MT_DISK_V2, &b),
            (MT_NVRAM, &none),
        ];
        let (slices, total) = plan(&layers).unwrap();
        assert_eq!(total, 612);
        assert_eq!(slices[1].offset, 512);
        assert_eq!(slices[1].layer, 2);

        let stacked = [
            (MT_CONFIG, &none),
            (MT_DISK_V2, &a),
            (MT_ASIF_OVERLAY, &b),
            (MT_NVRAM, &none),
        ];
        assert!(format!("{:#}", plan(&stacked).unwrap_err()).contains("stacked"));
        let v1 = [(MT_CONFIG, &none), (MT_DISK_V1, &a), (MT_NVRAM, &none)];
        assert!(plan(&v1).is_err());
        let no_disk = [(MT_CONFIG, &none), (MT_NVRAM, &none)];
        assert!(plan(&no_disk).is_err());
        let wrong_order = [(MT_DISK_V2, &a), (MT_CONFIG, &none), (MT_NVRAM, &none)];
        assert!(plan(&wrong_order).is_err());
        let huge = ann(MAX_SLICE + 1);
        assert!(plan(&[(MT_CONFIG, &none), (MT_DISK_V2, &huge), (MT_NVRAM, &none)]).is_err());
    }

    #[test]
    fn configs_map_to_fluxvm() {
        let mac = TartConfig::parse(
            br#"{"version":1,"os":"darwin","arch":"arm64","cpuCount":4,"memorySize":8589934592,
                 "hardwareModel":"aGFyZHdhcmU=","ecid":"aWQ=","macAddress":"aa:bb:cc:dd:ee:ff"}"#,
        )
        .unwrap();
        assert_eq!(mac.fluxvm_os(), "macos");
        assert_eq!(mac.nvram_file(), "auxiliary.bin");
        assert_eq!(mac.hardware_model_bytes().unwrap().unwrap(), b"hardware");
        let linux = TartConfig::parse(br#"{"version":1,"os":"linux","cpuCount":2,"memorySize":1}"#)
            .unwrap();
        assert_eq!(linux.fluxvm_os(), "linux");
        assert_eq!(linux.nvram_file(), "efi.bin");
        assert!(linux.hardware_model_bytes().unwrap().is_none());
        assert!(TartConfig::parse(br#"{"version":1,"arch":"amd64"}"#).is_err());
        assert!(TartConfig::parse(br#"{"version":1,"diskFormat":"asif"}"#).is_err());
        assert!(
            TartConfig::parse(br#"{"version":1,"os":"darwin"}"#)
                .unwrap()
                .hardware_model_bytes()
                .is_err()
        );
    }
}
