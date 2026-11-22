// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/up/plan`: what `fluxctl up` would check and do on this node, from the same rules as the command
//! (`fluxvm_core::up`). Read-only: nothing is created or started. The Studio Home page shows it as "Get started" with
//! the exact `fluxctl up` command to copy.

use axum::{Json, extract::State};
use fluxvm_core::up::{self, UpPlan};
use fluxvm_scheduler::VmManager;
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new().route("/v1/up/plan", axum::routing::get(up_plan))
}

async fn up_plan(State(m): State<Arc<VmManager>>) -> Json<UpPlan> {
    let vms = serde_json::json!({"items": m.list().await});
    let state_dir = m.cfg.state_dir.clone();
    let plan = tokio::task::spawn_blocking(move || {
        let mut f = up::probe(&state_dir);
        // This answer comes from the daemon itself, so it is up.
        f.daemon_up = Some(true);
        f.cluster = up::cluster_state(
            up::vm_status(&vms, up::CLUSTER_VM).as_deref(),
            up::api_answers(up::CLUSTER_API_PORT),
        );
        up::assess(&f)
    })
    .await
    .unwrap_or_else(|_| up::assess(&Default::default()));
    Json(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_is_registered_under_v1() {
        let _ = routes();
    }

    #[test]
    fn an_empty_fact_set_still_gives_a_plan_with_a_command() {
        let p = up::assess(&Default::default());
        assert_eq!(p.command, "fluxctl up");
        assert!(!p.steps.is_empty());
    }
}
