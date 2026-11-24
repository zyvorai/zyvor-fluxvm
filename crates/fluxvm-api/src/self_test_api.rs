// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `POST /v1/self-test` runs the quick smoke check of this install (`fluxvm_core::self_test`, the same probes as
//! `fluxctl self-test`) and returns the report; `GET /v1/self-test` returns the last report this daemon produced, or
//! `null` before the first run. Read-only apart from a throw-away file written to and removed from the state folder.
//! The Studio Home page has a "Self-test" card with a Run button.

use axum::{Json, extract::State};
use fluxvm_core::self_test::{self, DaemonProbe, Inputs, Report};
use fluxvm_scheduler::VmManager;
use std::sync::{Arc, Mutex};

static LAST: Mutex<Option<Report>> = Mutex::new(None);

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new().route("/v1/self-test", axum::routing::get(last).post(run))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn last() -> Json<Option<Report>> {
    Json(LAST.lock().map(|g| g.clone()).unwrap_or(None))
}

async fn run(State(m): State<Arc<VmManager>>) -> Json<Report> {
    // This handler is the daemon answering, and `list` is the scheduler call.
    let vms = m.list().await.len();
    let inp = Inputs {
        state_dir: m.cfg.state_dir.clone(),
        docker_sock: m.cfg.docker_sock_path(),
        agent_label: "dev.zyvor.fluxvm".into(),
        daemon: DaemonProbe {
            reachable: Ok(()),
            scheduler: Some(Ok(vms)),
        },
    };
    let report = tokio::task::spawn_blocking(move || self_test::run(&inp, now()))
        .await
        .unwrap_or_else(|e| {
            let c = self_test::Check {
                id: "self_test".into(),
                name: "Self-test".into(),
                status: self_test::Status::Fail,
                detail: format!("the probes crashed: {e}"),
                what_to_do: Some(
                    "Run `fluxctl self-test` from a terminal and send the output to support."
                        .into(),
                ),
                explain: None,
            };
            self_test::summarize(vec![c], now())
        });
    if let Ok(mut g) = LAST.lock() {
        *g = Some(report.clone());
    }
    Json(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_is_registered_under_v1() {
        let _ = routes();
    }
}
