// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Agent-mode sandboxes.

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct AgentRunArgs {
    #[arg(long)]
    pub image: String,

    #[arg(long)]
    pub name: Option<String>,

    /// Hard wall-clock budget in seconds.
    #[arg(long)]
    pub budget_seconds: Option<u64>,

    /// Memory budget in MiB.
    #[arg(long)]
    pub budget_mib: Option<u64>,

    /// Hosts the agent may reach.
    #[arg(long)]
    pub allow_host: Vec<String>,

    /// Token scope, e.g. "exec,read:/workspace".
    #[arg(long)]
    pub token_scope: Option<String>,
}

pub async fn run(args: AgentRunArgs) -> Result<()> {
    println!(
        "agent sandbox: image={} budget_seconds={:?} scope={:?}",
        args.image, args.budget_seconds, args.token_scope
    );
    // TODO: create sandbox + scoped token + budget enforcer
    Ok(())
}
