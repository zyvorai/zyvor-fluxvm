// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl sandbox build` — build an OCI image inside a FluxVM builder microVM.
//!
//! Reuses the existing rootfs unpack builder path. Full Dockerfile / BuildKit
//! support is a follow-up; this lands the command, flags, and the call into
//! the scheduler.

use anyhow::{Context, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Tag for the resulting image (e.g. myapp:1.0).
    #[arg(short, long)]
    pub tag: String,

    /// Build context directory (defaults to `.`).
    #[arg(default_value = ".")]
    pub context: PathBuf,

    /// Dockerfile path relative to context.
    #[arg(short, long, default_value = "Dockerfile")]
    pub file: PathBuf,

    /// Platform (linux/arm64 or linux/amd64).
    #[arg(long, default_value = "linux/arm64")]
    pub platform: String,

    /// Do not use the layer cache.
    #[arg(long)]
    pub no_cache: bool,
}

pub async fn run(args: BuildArgs) -> Result<()> {
    // TODO: wire to VmManager / builder microVM.
    // 1. Resolve context + Dockerfile
    // 2. Start or claim a builder VM (reuse oci unpack builder)
    // 3. Share context via virtiofs
    // 4. Run build inside guest
    // 5. Register resulting image under `args.tag`
    // 6. Print image id

    println!(
        "sandbox build: tag={} context={} file={} platform={}",
        args.tag,
        args.context.display(),
        args.file.display(),
        args.platform
    );
    println!("TODO: full builder VM integration (see docs/sandbox-build.md)");
    Ok(())
}
