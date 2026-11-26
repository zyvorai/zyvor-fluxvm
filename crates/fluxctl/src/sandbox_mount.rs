// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Controlled host-path virtiofs mounts for sandboxes.
//!
//! Off by default. Paths must be under a configured allowlist.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BindMount {
    pub host: PathBuf,
    pub guest: String,
    pub read_only: bool,
}

pub fn parse_mount(spec: &str) -> Result<BindMount> {
    // type=bind,src=/host/path,dst=/guest/path[,ro]
    let mut host = None;
    let mut guest = None;
    let mut read_only = true;

    for part in spec.split(',') {
        if let Some(rest) = part.strip_prefix("src=") {
            host = Some(PathBuf::from(rest));
        } else if let Some(rest) = part.strip_prefix("dst=") {
            guest = Some(rest.to_string());
        } else if part == "ro" {
            read_only = true;
        } else if part == "rw" {
            read_only = false;
        }
    }

    let host = host.context("missing src=")?;
    let guest = guest.context("missing dst=")?;
    Ok(BindMount { host, guest, read_only })
}

pub fn check_allowlist(path: &Path, allowlist: &[PathBuf]) -> Result<()> {
    if allowlist.is_empty() {
        bail!("host mounts disabled: no allowlist configured");
    }
    let canon = path.canonicalize().context("canonicalize host path")?;
    for allowed in allowlist {
        if canon.starts_with(allowed) {
            return Ok(());
        }
    }
    bail!("path {} is not under any configured allowlist", path.display());
}
