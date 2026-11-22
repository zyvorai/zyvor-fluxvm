// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! What `fluxctl up` does with the cluster lifecycle: look at the default cluster, create it with the same code as
//! `fluxctl kube cluster create` (the flags are parsed by the same clap definition, so there is one create path), start
//! it again when it is stopped, and say whether it is healthy. The decision from the cluster's record and status is a
//! pure function ([`state_of`]) with tests; the rest needs a daemon and is not run in unit tests.

use super::host::RealHost;
use super::lifecycle;
use super::model::{Phase, Store, default_root};
use super::{AppleCommand, create_value};
use crate::run::VmApi;
use anyhow::{Context, Result};
use clap::Parser;
use fluxvm_core::up::{CLUSTER_NAME, ClusterState};
use std::path::{Path, PathBuf};

/// What the cluster's record and (when it is Ready) its status say.
pub struct Seen {
    /// `None`: no record of the cluster.
    pub phase: Option<Phase>,
    /// The VM status of each node (`running`, `stopped`, `missing`, ...), when the status could be read.
    pub node_vms: Option<Vec<String>>,
    pub healthy: bool,
}

/// The pure decision.
pub fn state_of(s: &Seen) -> ClusterState {
    match s.phase {
        None => ClusterState::Missing,
        Some(Phase::Creating | Phase::Bootstrapping) => ClusterState::Incomplete,
        Some(Phase::Ready) => match &s.node_vms {
            _ if s.healthy => ClusterState::Healthy,
            Some(vms) if vms.iter().any(|v| v != "running") => ClusterState::Stopped,
            Some(_) => ClusterState::NotAnswering,
            None => ClusterState::Unknown,
        },
    }
}

#[derive(Parser)]
struct CreateOnly {
    #[command(subcommand)]
    command: AppleCommand,
}

/// The flags of `kube cluster create NAME --binary FILE --sha256-file FILE`, parsed like the real command line.
pub fn create_command(name: &str, binary: &Path, sums: &Path) -> Result<AppleCommand> {
    let args = [
        "up".to_owned(),
        "create".to_owned(),
        name.to_owned(),
        "--binary".to_owned(),
        binary.display().to_string(),
        "--sha256-file".to_owned(),
        sums.display().to_string(),
        // `up` already asked once, up front; the gateway can be added later with `kube cluster platform envoy-gateway`.
        "--no-gateway".to_owned(),
    ];
    Ok(CreateOnly::try_parse_from(args)
        .context("building the default cluster's create request")?
        .command)
}

fn cache_root() -> Result<PathBuf> {
    Ok(crate::docker_host::home()?.join(".fluxvm").join("up-cache"))
}

/// Look at the default cluster. Read-only.
pub async fn probe<A: VmApi>(api: &A) -> ClusterState {
    let Ok(root) = default_root() else {
        return ClusterState::Unknown;
    };
    let store = Store::new(root);
    let phase = match store.load(CLUSTER_NAME) {
        Ok(Some(s)) => Some(s.phase),
        Ok(None) => None,
        Err(_) => return ClusterState::Unknown,
    };
    if phase != Some(Phase::Ready) {
        return state_of(&Seen {
            phase,
            node_vms: None,
            healthy: false,
        });
    }
    let Ok(h) = RealHost::new(api, &store, None) else {
        return ClusterState::Unknown;
    };
    match lifecycle::status(&h, &store, CLUSTER_NAME).await {
        Ok(st) => state_of(&Seen {
            phase,
            node_vms: Some(st.nodes.iter().map(|n| n.vm.clone()).collect()),
            healthy: st.healthy,
        }),
        Err(_) => ClusterState::Unknown,
    }
}

/// Whether the default cluster is healthy right now.
pub async fn healthy<A: VmApi>(api: &A) -> bool {
    probe(api).await == ClusterState::Healthy
}

/// Create (or resume) the default cluster: download and verify k3s, then run `kube cluster create default` with it.
pub async fn create<A: VmApi>(api: &A) -> Result<String> {
    let backend = crate::vm_net::Backend::host();
    let root = cache_root()?;
    let (bin, sums, version) = tokio::task::block_in_place(|| {
        crate::kube_host::fetch_k3s(&root, backend.k3s_binary(), backend.k3s_sums_file())
    })
    .context("could not get k3s. Next: check the internet connection and run `fluxctl up` again; offline, download k3s and its sha256sum file yourself and run `fluxctl kube cluster create default --binary FILE --sha256-file FILE`")?;
    let store = Store::new(default_root()?);
    let h = RealHost::new(api, &store, None)?;
    let cmd = create_command(CLUSTER_NAME, &bin, &sums)?;
    let report = create_value(&h, &store, cmd).await?;
    let kubeconfig = report["kubeconfig"].as_str().unwrap_or("");
    Ok(format!(
        "cluster {CLUSTER_NAME} created with k3s {version} (kubeconfig {kubeconfig})"
    ))
}

/// Start the stopped default cluster and wait until its nodes are Ready (`kube cluster start default --tunnel`).
pub async fn start<A: VmApi>(api: &A) -> Result<String> {
    let store = Store::new(default_root()?);
    let h = RealHost::new(api, &store, None)?;
    lifecycle::start(&h, &store, CLUSTER_NAME, true).await?;
    Ok(format!("cluster {CLUSTER_NAME} started"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(phase: Option<Phase>, vms: Option<&[&str]>, healthy: bool) -> Seen {
        Seen {
            phase,
            node_vms: vms.map(|v| v.iter().map(|s| (*s).to_owned()).collect()),
            healthy,
        }
    }

    #[test]
    fn the_state_follows_the_record_and_the_status() {
        assert_eq!(state_of(&seen(None, None, false)), ClusterState::Missing);
        assert_eq!(
            state_of(&seen(Some(Phase::Creating), None, false)),
            ClusterState::Incomplete
        );
        assert_eq!(
            state_of(&seen(Some(Phase::Bootstrapping), None, false)),
            ClusterState::Incomplete
        );
        assert_eq!(
            state_of(&seen(Some(Phase::Ready), Some(&["running"]), true)),
            ClusterState::Healthy
        );
        assert_eq!(
            state_of(&seen(Some(Phase::Ready), Some(&["stopped"]), false)),
            ClusterState::Stopped
        );
        assert_eq!(
            state_of(&seen(Some(Phase::Ready), Some(&["running"]), false)),
            ClusterState::NotAnswering
        );
        assert_eq!(
            state_of(&seen(Some(Phase::Ready), None, false)),
            ClusterState::Unknown
        );
    }

    #[test]
    fn the_create_request_is_the_real_create_command_with_defaults() {
        let c = create_command("default", Path::new("/c/k3s"), Path::new("/c/sums")).unwrap();
        let AppleCommand::Create {
            name,
            servers,
            embedded_etcd,
            api_port,
            no_gateway,
            ..
        } = c
        else {
            panic!("not a Create command");
        };
        assert_eq!(name, "default");
        assert_eq!(servers, 1);
        assert!(!embedded_etcd);
        assert_eq!(api_port, fluxvm_core::up::CLUSTER_API_PORT);
        assert!(no_gateway, "up must not ask a second question mid-setup");
    }

    #[test]
    fn the_cluster_name_matches_the_vm_the_daemon_lists() {
        assert_eq!(
            super::super::model::vm_name(CLUSTER_NAME),
            fluxvm_core::up::CLUSTER_VM
        );
    }
}
