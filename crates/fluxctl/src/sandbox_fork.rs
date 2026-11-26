// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Sandbox fork and time-travel.

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct ForkArgs {
    /// Source sandbox name or id.
    pub source: String,

    /// How many copies to create.
    #[arg(long, default_value = "1")]
    pub count: u32,

    /// Prefix for the new sandbox names.
    #[arg(long, default_value = "fork")]
    pub prefix: String,
}

pub async fn fork(args: ForkArgs) -> Result<()> {
    println!(
        "forking sandbox {} into {} copies (prefix {})",
        args.source, args.count, args.prefix
    );
    // TODO: call the existing vm_fork path
    Ok(())
}

#[derive(Args, Debug)]
pub struct CheckpointArgs {
    pub sandbox: String,
    #[arg(long)]
    pub name: String,
}

pub async fn checkpoint(args: CheckpointArgs) -> Result<()> {
    println!("checkpoint {} as {}", args.sandbox, args.name);
    Ok(())
}
