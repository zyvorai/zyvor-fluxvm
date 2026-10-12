// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl cloud`: one-command FluxMesh personal-cloud UX.

use anyhow::Result;
use clap::{Args, Subcommand};
use fluxvm_mesh::cloud::{
    CloudPaths, accept_join_token, issue_join_token, issue_open_join_token, status, up,
};
use fluxvm_mesh::enroll_tls;
use fluxvm_mesh::reconcile::{rotate_wireguard_key, wireguard_public_key};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args)]
pub struct CloudArgs {
    #[command(subcommand)]
    command: CloudCommand,
}

#[derive(Subcommand)]
enum CloudCommand {
    /// Bootstrap this host as a personal-cloud node. Idempotent if already initialized.
    Up {
        node: String,
        hostname: String,
        #[arg(long)]
        pool: Option<String>,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Show the local overlay, dual-stack address and cloud inventory.
    Status {
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Create a short-lived one-time invitation. By default the joining host
    /// creates its own WireGuard key; --public-key can bind the invite to a
    /// pre-provisioned key instead.
    Invite {
        node: String,
        hostname: String,
        #[arg(long)]
        public_key: Option<String>,
        #[arg(long, default_value_t = 900)]
        ttl_seconds: u64,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Consume an invitation locally. This is useful for air-gapped/manual
    /// enrollment where the token was bound to a known public key.
    Accept {
        token: String,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Serve the verified-TLS zero-touch enrollment endpoint. A joining host
    /// presents only its one-time invitation and self-generated public key.
    ServeEnrollment {
        #[arg(long, default_value = "0.0.0.0:7443")]
        listen: SocketAddr,
        #[arg(long)]
        cert: PathBuf,
        #[arg(long)]
        tls_key: PathBuf,
        #[command(flatten)]
        paths: PathArgs,
    },
    /// Join an existing personal cloud over verified TLS. Generates a local
    /// WireGuard key if needed and atomically installs the returned mesh state.
    Join {
        controller: SocketAddr,
        #[arg(long)]
        server_name: String,
        #[arg(long)]
        ca: PathBuf,
        token: String,
        #[command(flatten)]
        paths: PathArgs,
    },
}

#[derive(Args, Clone, Default)]
struct PathArgs {
    #[arg(long, env = "FLUXMESH_STATE")]
    state: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_CATALOG")]
    catalog: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_KEY")]
    key: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_ENROLL_SECRET")]
    enroll_secret: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_ENROLL_NONCES")]
    used_nonces: Option<PathBuf>,
    #[arg(long, env = "FLUXMESH_KEYDIST")]
    keydist: Option<PathBuf>,
}

impl PathArgs {
    fn resolve(self) -> CloudPaths {
        let mut p = CloudPaths::default();
        if let Some(v) = self.state {
            p.state = v;
        }
        if let Some(v) = self.catalog {
            p.catalog = v;
        }
        if let Some(v) = self.key {
            p.key = v;
        }
        if let Some(v) = self.enroll_secret {
            p.enroll_secret = v;
        }
        if let Some(v) = self.used_nonces {
            p.used_nonces = v;
        }
        if let Some(v) = self.keydist {
            p.keydist = v;
        }
        p
    }
}

pub async fn run(args: CloudArgs) -> Result<()> {
    match args.command {
        CloudCommand::Up {
            node,
            hostname,
            pool,
            paths,
        } => {
            let status = up(&paths.resolve(), &node, &hostname, pool.as_deref())?;
            println!(
                "cloud={} ipv4={} ipv6={} nodes={} services={} relays={}",
                status.local_node,
                status.ipv4,
                status.ipv6,
                status.nodes,
                status.services,
                status.relays
            );
        }
        CloudCommand::Status { paths } => {
            let status = status(&paths.resolve())?;
            println!(
                "cloud={} ipv4={} ipv6={} nodes={} services={} relays={} pending_keys={}",
                status.local_node,
                status.ipv4,
                status.ipv6,
                status.nodes,
                status.services,
                status.relays,
                status.pending_key_rotations
            );
        }
        CloudCommand::Invite {
            node,
            hostname,
            public_key,
            ttl_seconds,
            paths,
        } => {
            let paths = paths.resolve();
            let ttl = Duration::from_secs(ttl_seconds);
            let token = match public_key {
                Some(public_key) => issue_join_token(&paths, &node, &hostname, &public_key, ttl)?,
                None => issue_open_join_token(&paths, &node, &hostname, ttl)?,
            };
            println!("{token}");
        }
        CloudCommand::Accept { token, paths } => {
            println!(
                "enrolled ipv4={}",
                accept_join_token(&paths.resolve(), &token)?
            );
        }
        CloudCommand::ServeEnrollment {
            listen,
            cert,
            tls_key,
            paths,
        } => {
            let paths = paths.resolve();
            enroll_tls::serve(
                listen,
                &cert,
                &tls_key,
                paths.state,
                paths.enroll_secret,
                paths.used_nonces,
            )
            .await?;
        }
        CloudCommand::Join {
            controller,
            server_name,
            ca,
            token,
            paths,
        } => {
            let paths = paths.resolve();
            let public_key = if paths.key.exists() {
                wireguard_public_key(&paths.key)?
            } else {
                rotate_wireguard_key(&paths.key)?
            };
            enroll_tls::join(
                controller,
                &server_name,
                &ca,
                &token,
                &public_key,
                &paths.state,
            )
            .await?;
            let joined = status(&paths)?;
            println!(
                "joined cloud={} ipv4={} ipv6={} public_key={}",
                joined.local_node, joined.ipv4, joined.ipv6, public_key
            );
        }
    }
    Ok(())
}
