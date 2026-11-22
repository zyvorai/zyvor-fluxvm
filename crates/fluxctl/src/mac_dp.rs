// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Clap surface for `fluxctl mac login|logout|logins` (registry credentials, `mac_login`) and `fluxctl mac fork` (`mac_fork`).

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum MacDp {
    #[command(flatten)]
    Auth(crate::mac_login::LoginCommand),
    /// Create N instances from a STOPPED instance's disk by copy-on-write clone: each is a cold boot of the clone (a running state is
    /// never carried over), with its own name and the label `fluxvm.mac.forked-from`. Created stopped unless `--start`. Needs the daemon.
    Fork(crate::mac_fork::ForkArgs),
}

impl MacDp {
    /// Login and logout never contact the daemon; fork does.
    pub fn is_client_side(&self) -> bool {
        matches!(self, Self::Auth(_))
    }
}

pub async fn run_client(cmd: MacDp) -> Result<()> {
    match cmd {
        MacDp::Auth(a) => crate::mac_login::run(a).await,
        MacDp::Fork(_) => {
            anyhow::bail!("`mac fork` needs the daemon: pass --server (or select a context)")
        }
    }
}

pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacDp) -> Result<()> {
    match cmd {
        MacDp::Fork(a) => crate::mac_fork::run(r, a).await,
        MacDp::Auth(_) => anyhow::bail!("not a daemon command"),
    }
}
