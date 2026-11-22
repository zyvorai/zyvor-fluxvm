// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/cluster/health`: this node's health in plain English, from the same model as `fluxctl cluster status`
//! (`fluxvm_core::health`). Read-only. The Studio Home page shows it. It covers what the daemon can see itself (VMs,
//! macOS slots, memory, disk, backups, version); the host-setting checks of `fluxctl doctor` run in the CLI.

use axum::{Json, extract::State};
use fluxvm_core::health::{self, ClusterHealth, NodeFacts, Platform};
use fluxvm_scheduler::VmManager;
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new().route("/v1/cluster/health", axum::routing::get(cluster_health))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn cluster_health(State(m): State<Arc<VmManager>>) -> Json<ClusterHealth> {
    let vms = serde_json::json!({"items": m.list().await});
    let state_dir = m.cfg.state_dir.clone();
    let pressure = tokio::task::spawn_blocking(fluxvm_core::pressure_admission::sample_host)
        .await
        .unwrap_or_default();
    let facts = tokio::task::spawn_blocking(move || {
        let mut f = NodeFacts {
            name: health::hostname(),
            platform: if cfg!(target_os = "macos") {
                Platform::Mac
            } else if cfg!(target_os = "linux") {
                Platform::Linux
            } else {
                Platform::Unknown
            },
            local: true,
            reachable: true,
            installed: Some(true),
            daemon_up: Some(true),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            mem_total_mib: pressure.mem_total_mib,
            mem_free_mib: pressure.mem_available_mib,
            disk_free_gib: health::free_gib(&state_dir),
            backups_known: true,
            last_backup_epoch: health::newest_mtime(&state_dir.join("backups")),
            ..Default::default()
        };
        let cpus = std::thread::available_parallelism().ok().and_then(|n| u32::try_from(n.get()).ok());
        health::apply_vm_list(&mut f, &vms, cpus);
        f
    })
    .await
    .unwrap_or_default();
    Json(health::assess(&[facts], now()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_is_registered_under_v1() {
        let _ = routes();
    }
}
