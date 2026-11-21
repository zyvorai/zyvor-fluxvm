// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl kube apple`: a persistent ARM64 Linux guest (Apple Virtualization) running k3s, one VM per node, driven from
//! the Mac. Unlike `kube up`, which provisions everything through cloud-init and the installer script, this workflow
//! installs a *checksum-verified* k3s binary over a ssh connection whose host key was pinned before the guest booted, keeps a
//! journal of what it created (so an interrupted run can be repaired from the VMs' ownership labels), and never prints
//! or passes the cluster token on a command line. See `docs/apple-k3s.md`.

mod host;
mod lifecycle;
mod model;
mod script;
#[cfg(test)]
mod tests;

use crate::run::VmApi;
use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use lifecycle::Install;
use model::*;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct BinaryArgs {
    /// The official k3s-arm64 release binary, downloaded on the Mac. Its SHA-256 is checked here and again in the guest.
    #[arg(long, value_name = "FILE")]
    binary: Option<PathBuf>,
    /// The binary's published SHA-256 (64 hexadecimal characters).
    #[arg(long, value_name = "HEX")]
    sha256: Option<String>,
    /// A `sha256sum-arm64.txt` from the release; the entry named like the binary is used.
    #[arg(long, value_name = "FILE")]
    sha256_file: Option<PathBuf>,
    /// The image already has k3s and its systemd service; install nothing.
    #[arg(long, conflicts_with_all = ["binary", "sha256", "sha256_file"])]
    preinstalled: bool,
}

#[derive(Args, Debug, Clone)]
pub struct VmArgs {
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    #[arg(long, default_value_t = 4096)]
    memory_mib: u64,
    #[arg(long, default_value_t = 30)]
    disk_gib: u64,
    /// An ARM64 Linux image with systemd, cloud-init, sshd, sha256sum and passwordless sudo for the user.
    #[arg(long, default_value = "debian-13")]
    image: String,
    /// Login user in the guests (default: your user name).
    #[arg(long, short = 'l')]
    user: Option<String>,
}

impl VmArgs {
    fn params(&self) -> Params {
        Params {
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            disk_gib: self.disk_gib,
            image: self.image.clone(),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum PersistCommand {
    /// Write a canary into the datastore and the guest's disk and remember the boot id.
    Mark { name: String },
    /// After a stop/start: the guest rebooted and the canary is still there.
    Verify {
        name: String,
        /// Do not require a changed boot id (for a restart of the service only).
        #[arg(long)]
        allow_same_boot: bool,
    },
    /// mark, stop, start, verify, in one go.
    Cycle { name: String },
}

#[derive(Subcommand, Debug)]
pub enum SnapshotCommand {
    /// Take an etcd snapshot now (default name `fluxvm-<time>`; k3s appends the node and a timestamp).
    Save { name: String, label: Option<String> },
    /// List the snapshots on the server (k3s's own list, including scheduled ones).
    Ls { name: String },
    /// Delete one snapshot, by the name `ls` shows.
    Delete { name: String, snapshot: String },
    /// Keep the newest N snapshots whose name starts with PREFIX.
    Prune {
        name: String,
        #[arg(long, default_value = "on-demand")]
        prefix: String,
        #[arg(long, default_value_t = 5)]
        retention: u32,
    },
    /// Copy a snapshot and the token it needs to this Mac (private file plus .sha256); `restore FILE` accepts it.
    Pull {
        name: String,
        snapshot: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Restore a snapshot that is on the server (cluster-reset). Other servers must be wiped and rejoined.
    Restore {
        name: String,
        snapshot: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum AppleCommand {
    /// etcd snapshots (clusters created with --embedded-etcd): save, ls, delete, prune, pull, restore.
    Snapshot {
        #[command(subcommand)]
        command: SnapshotCommand,
    },
    /// Create a single-server cluster in its own VM: pinned SSH host key, checksum-verified k3s, API readiness, kubeconfig.
    /// The cluster is journalled before its VM exists; if a step fails, run the same command again to resume.
    Create {
        name: String,
        #[command(flatten)]
        vm: VmArgs,
        #[command(flatten)]
        binary: BinaryArgs,
        /// Where this Mac reaches the API (127.0.0.1:PORT). Use a different port per cluster.
        #[arg(long, default_value_t = DEFAULT_API_PORT)]
        api_port: u16,
        /// Use embedded etcd (`--cluster-init`), which allows more servers and etcd snapshots. Default: SQLite.
        #[arg(long)]
        embedded_etcd: bool,
        /// An extra name or address for the API certificate. Repeatable. 127.0.0.1 is always included.
        #[arg(long, value_name = "NAME")]
        tls_san: Vec<String>,
        /// Where to write the kubeconfig (default: ~/.kube/fluxvm-apple-NAME.yaml).
        #[arg(long)]
        kubeconfig: Option<PathBuf>,
    },
    /// Add a node, in a VM of its own on the cluster's private network. The token is read on the server and goes to the
    /// new node on stdin; it is never printed or put in an argument.
    /// With `--url` and `--token-file` the node joins a cluster that runs on another Mac.
    Join {
        /// The cluster (with --url: the local name under which the node is recorded).
        cluster: String,
        #[command(flatten)]
        vm: VmArgs,
        #[command(flatten)]
        binary: BinaryArgs,
        /// Node name (default: CLUSTER-a1, CLUSTER-a2, …).
        #[arg(long)]
        node: Option<String>,
        #[arg(long, value_parser = ["agent", "server"], default_value = "agent")]
        role: String,
        /// https://HOST[:PORT] of a server on another Mac. Needs --token-file.
        #[arg(long, requires = "token_file")]
        url: Option<String>,
        /// A file (mode 600) holding the cluster token, for --url.
        #[arg(long, requires = "url")]
        token_file: Option<PathBuf>,
        /// Keep a node that failed to join instead of deleting it again.
        #[arg(long)]
        keep_failed: bool,
    },
    /// Write the kubeconfig, pointed at 127.0.0.1 (the tunnel), mode 600.
    Kubeconfig {
        name: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// The SSH tunnel that publishes the API on 127.0.0.1 (host key pinned, loopback only).
    Tunnel {
        name: String,
        /// Close the tunnel.
        #[arg(long, conflicts_with = "status")]
        stop: bool,
        /// Only say whether it is up.
        #[arg(long)]
        status: bool,
    },
    /// Every VM, ssh, the k3s service, node readiness and version skew, as JSON. Exits non-zero when unhealthy.
    Status { name: String },
    /// The clusters this Mac knows.
    List,
    /// Stop k3s and the VMs (agents first). Data is kept.
    Stop { name: String },
    /// Start the VMs (server first) and wait until every node is Ready again.
    Start {
        name: String,
        /// Also open the API tunnel.
        #[arg(long)]
        tunnel: bool,
    },
    /// Check that data survives a stop and start.
    Persist {
        #[command(subcommand)]
        command: PersistCommand,
    },
    /// Replace the k3s binary node by node, servers first, with a datastore backup first and a rollback of a node that
    /// does not come back Ready.
    Upgrade {
        name: String,
        #[command(flatten)]
        binary: BinaryArgs,
        #[arg(long)]
        allow_downgrade: bool,
        /// Skip the automatic backup.
        #[arg(long)]
        no_backup: bool,
        /// Drain each agent before restarting it.
        #[arg(long)]
        drain: bool,
    },
    /// Save the datastore (and the token it needs) as a private file with a `.sha256` next to it. SQLite is stopped for the
    /// copy (seconds); embedded etcd uses a snapshot and keeps running. The file contains the cluster token.
    Backup {
        name: String,
        /// Default: ~/.fluxvm/apple-k3s/backups/NAME/NAME-<time>.tar.gz. Never overwritten.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Replace the datastore from a backup. The checksum is verified before anything is stopped.
    Restore {
        name: String,
        file: PathBuf,
        /// Expected SHA-256 (default: the .sha256 file next to the backup).
        #[arg(long)]
        sha256: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Repair the journal from the VMs' ownership labels after a crash: adopt guests created but not recorded, restore
    /// lost labels, report orphans and guests that belong to someone else (which are never touched).
    Recover {
        name: String,
        /// Also adopt a VM with the expected name and no labels (the crash came before they were written).
        #[arg(long)]
        adopt_unlabelled: bool,
        /// Delete VMs that carry this cluster's labels but are not in the journal.
        #[arg(long)]
        delete_orphans: bool,
    },
    /// Delete the agents, the server, the journal and the kubeconfig.
    Delete {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// The k3s service log of a node.
    Logs {
        name: String,
        #[arg(long)]
        node: Option<String>,
        #[arg(long, default_value_t = 100)]
        lines: u32,
    },
}

fn json<T: serde::Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Turns the binary flags into an install method. `default` is what no flag means: an error for the first server (it must
/// say where k3s comes from), a copy of the server's verified binary for a node that joins.
pub fn resolve_install(b: &BinaryArgs, default: Option<Install>) -> Result<Install> {
    if b.preinstalled {
        return Ok(Install::Preinstalled);
    }
    match &b.binary {
        Some(path) => {
            let entry = path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("k3s-arm64");
            let Some(sha) = expected_sha(b.sha256.as_deref(), b.sha256_file.as_deref(), entry)?
            else {
                bail!("--binary requires its published --sha256 (or --sha256-file)");
            };
            let data = read_verified(path, &sha)?;
            Ok(Install::Binary { data, sha256: sha })
        }
        None => {
            if b.sha256.is_some() || b.sha256_file.is_some() {
                bail!("--sha256 needs --binary");
            }
            default.ok_or_else(|| {
                anyhow::anyhow!(
                    "say where k3s comes from: --binary FILE with --sha256, or --preinstalled"
                )
            })
        }
    }
}

pub async fn run<A: VmApi>(api: &A, command: AppleCommand) -> Result<()> {
    let store = Store::new(default_root()?);
    // The first connection to a guest happens through `RealHost`; reading the key and user fails early and clearly.
    let user = match &command {
        AppleCommand::Create { vm, .. } | AppleCommand::Join { vm, .. } => vm.user.clone(),
        _ => None,
    };
    let h = host::RealHost::new(api, &store, user)?;
    use host::Host;
    match command {
        AppleCommand::Create { name, vm, binary, api_port, embedded_etcd, tls_san, kubeconfig } => {
            let install = resolve_install(&binary, None)?;
            let req = lifecycle::CreateReq {
                name,
                params: vm.params(),
                api_port,
                datastore: if embedded_etcd { Datastore::Etcd } else { Datastore::Sqlite },
                tls_sans: tls_san,
                install,
                kubeconfig,
                user: h.user(),
            };
            json(&lifecycle::create(&h, &store, &req).await?)
        }
        AppleCommand::Join { cluster, vm, binary, node, role, url, token_file, keep_failed } => {
            let external = url.zip(token_file);
            let default = external.is_none().then_some(Install::FromServer);
            let install = resolve_install(&binary, default)?;
            let req = lifecycle::JoinReq {
                cluster,
                node,
                role: if role == "server" { Role::Server } else { Role::Agent },
                install,
                external,
                params: vm.params(),
                user: h.user(),
                keep_failed,
            };
            json(&lifecycle::join(&h, &store, &req).await?)
        }
        AppleCommand::Kubeconfig { name, output } => {
            let p = lifecycle::kubeconfig(&h, &store, &name, output.as_deref()).await?;
            println!("{}", p.display());
            Ok(())
        }
        AppleCommand::Tunnel { name, stop, status } => {
            let st = store.require(&name)?;
            if status {
                return json(&serde_json::json!({"up": h.tunnel_up(&st), "port": st.api_port}));
            }
            if stop {
                return json(&serde_json::json!({"stopped": h.stop_tunnel(&st)}));
            }
            let pid = h.start_tunnel(&st).await?;
            json(&serde_json::json!({"pid": pid, "listen": format!("127.0.0.1:{}", st.api_port)}))
        }
        AppleCommand::Status { name } => {
            let s = lifecycle::status(&h, &store, &name).await?;
            json(&s)?;
            if !s.healthy {
                bail!("{name} is not healthy");
            }
            Ok(())
        }
        AppleCommand::List => json(
            &store
                .list()?
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name, "phase": s.phase, "datastore": s.datastore, "api_port": s.api_port,
                        "k3s_version": s.k3s_version,
                        "nodes": s.nodes.iter().map(|n| serde_json::json!({"name": n.name, "role": n.role, "joined": n.joined})).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>(),
        ),
        AppleCommand::Stop { name } => json(&lifecycle::stop(&h, &store, &name).await?),
        AppleCommand::Start { name, tunnel } => json(&lifecycle::start(&h, &store, &name, tunnel).await?),
        AppleCommand::Persist { command } => match command {
            PersistCommand::Mark { name } => json(&lifecycle::persist_mark(&h, &store, &name).await?),
            PersistCommand::Verify { name, allow_same_boot } => {
                let r = lifecycle::persist_verify(&h, &store, &name, allow_same_boot).await?;
                json(&r)?;
                if !r.ok {
                    bail!("persistence check failed");
                }
                Ok(())
            }
            PersistCommand::Cycle { name } => {
                let r = lifecycle::persist_cycle(&h, &store, &name).await?;
                json(&r)?;
                if !r.ok {
                    bail!("persistence check failed");
                }
                Ok(())
            }
        },
        AppleCommand::Upgrade { name, binary, allow_downgrade, no_backup, drain } => {
            let Install::Binary { data, sha256 } = resolve_install(&binary, None)? else {
                bail!("upgrade needs --binary with its checksum");
            };
            let req = lifecycle::UpgradeReq { binary: data, sha256, allow_downgrade, backup: !no_backup, drain };
            json(&lifecycle::upgrade(&h, &store, &name, &req).await?)
        }
        AppleCommand::Snapshot { command } => match command {
            SnapshotCommand::Save { name, label } => {
                println!("{}", lifecycle::snapshot_save(&h, &store, &name, label.as_deref()).await?);
                Ok(())
            }
            SnapshotCommand::Ls { name } => {
                println!("{}", lifecycle::snapshot_ls(&h, &store, &name).await?);
                Ok(())
            }
            SnapshotCommand::Delete { name, snapshot } => {
                lifecycle::snapshot_delete(&h, &store, &name, &snapshot).await
            }
            SnapshotCommand::Prune { name, prefix, retention } => {
                println!("{}", lifecycle::snapshot_prune(&h, &store, &name, &prefix, retention).await?);
                Ok(())
            }
            SnapshotCommand::Pull { name, snapshot, output } => {
                json(&lifecycle::snapshot_pull(&h, &store, &name, &snapshot, output).await?)
            }
            SnapshotCommand::Restore { name, snapshot, yes } => {
                lifecycle::snapshot_restore(&h, &store, &name, &snapshot, yes).await
            }
        },
        AppleCommand::Backup { name, output } => json(&lifecycle::backup(&h, &store, &name, output).await?),
        AppleCommand::Restore { name, file, sha256, yes } => {
            json(&lifecycle::restore(&h, &store, &name, &file, sha256.as_deref(), yes).await?)
        }
        AppleCommand::Recover { name, adopt_unlabelled, delete_orphans } => json(
            &lifecycle::recover(&h, &store, &name, &lifecycle::RecoverOpts { adopt_unlabelled, delete_orphans }).await?,
        ),
        AppleCommand::Delete { name, yes } => {
            let r = lifecycle::delete(&h, &store, &name, yes).await?;
            json(&r)?;
            if !r.failed.is_empty() {
                bail!("some nodes could not be deleted; run the command again to retry them");
            }
            Ok(())
        }
        AppleCommand::Logs { name, node, lines } => {
            println!("{}", lifecycle::logs(&h, &store, &name, node.as_deref(), lines).await?);
            Ok(())
        }
    }
}
