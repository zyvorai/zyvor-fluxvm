// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/doctor`: the "check my setup" report of `fluxctl doctor`, for the Studio Home page. Read-only.
//!
//! It is the same model: this node's [`ClusterHealth`] (what `GET /v1/cluster/health` returns) sorted into the short
//! green/yellow/red list of `fluxvm_core::setup_doctor`, plus the heat limit of a Mac. What lives in a person's home
//! directory or on other machines (the Docker context, registry logins, a fleet inventory, the gateway add-on of a
//! cluster) is not something the daemon can see, so those lines come back as "not checked" with the command to run.

use axum::{Json, extract::State};
use fluxvm_core::health;
use fluxvm_core::setup_doctor::{self, HostFacts, SetupCheck, SetupReport};
use fluxvm_scheduler::VmManager;
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new().route("/v1/doctor", axum::routing::get(doctor))
}

/// The lines only the CLI can fill in, said honestly instead of left out.
pub(crate) fn cli_only_lines() -> Vec<SetupCheck> {
    let u = |area, title, why: &str| SetupCheck::unchecked(area, title, why, "fluxctl doctor");
    vec![
        u(
            "docker",
            "Docker context",
            "The daemon cannot see your Docker settings, so this was not checked here.",
        ),
        u(
            "registry",
            "Registry logins",
            "Logins live in your home folder, so this was not checked here.",
        ),
        u(
            "gateway",
            "Gateway add-on",
            "Gateways are read with your kubeconfig, so this was not checked here.",
        ),
    ]
}

async fn doctor(State(m): State<Arc<VmManager>>) -> Json<SetupReport> {
    let h = crate::cluster_health::assess_now(&m).await;
    let state_dir = m.cfg.state_dir.clone();
    let host = tokio::task::spawn_blocking(move || HostFacts {
        thermal_limit: fluxvm_apple::host_health::probe_thermal_limit(),
        disk_free_gib: health::free_gib(&state_dir),
    })
    .await
    .unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Json(setup_doctor::build(&h, &host, cli_only_lines(), 0, now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::health::{NodeFacts, Platform, Severity};

    #[test]
    fn the_route_is_registered() {
        let _ = routes();
    }

    #[test]
    fn what_the_daemon_cannot_see_is_unchecked_with_a_command_and_never_colours_the_report() {
        let n = NodeFacts {
            name: "h".into(),
            platform: Platform::Linux,
            local: true,
            reachable: true,
            installed: Some(true),
            daemon_up: Some(true),
            ..Default::default()
        };
        let h = health::assess(&[n], 1);
        let r = setup_doctor::build(&h, &HostFacts::default(), cli_only_lines(), 0, 1);
        for a in ["docker", "registry", "gateway"] {
            let c = r.checks.iter().find(|c| c.area == a).unwrap();
            assert!(
                !c.checked && c.command.as_deref() == Some("fluxctl doctor"),
                "{a}"
            );
        }
        assert_eq!(r.light, Severity::Ok);
        assert_eq!(r.checks.iter().filter(|c| c.area == "gateway").count(), 1);
    }
}
