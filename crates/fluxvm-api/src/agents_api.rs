// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `GET /v1/agents` lists this Mac's FluxVM LaunchAgents with a plain-English health (`fluxvm_core::launch_agents`, the
//! same data as `fluxctl agents`). `POST /v1/agents/repair` loads the installed ones launchd does not have loaded
//! (`?dry_run=true` only says which); `POST /v1/agents/{label}/restart` restarts one. Nothing here edits or deletes a
//! plist. The Studio Home page has a "Background services" card. Empty on a machine without launchd.

use axum::{
    Json,
    extract::{Path, Query},
    http::StatusCode,
};
use fluxvm_core::launch_agents::{self as la, RepairResult, Report};
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new()
        .route("/v1/agents", axum::routing::get(list))
        .route("/v1/agents/repair", axum::routing::post(repair))
        .route("/v1/agents/{label}/restart", axum::routing::post(restart))
}

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(Into::into)
}

async fn list() -> Json<Report> {
    let r = tokio::task::spawn_blocking(|| match home() {
        Some(h) if cfg!(target_os = "macos") => la::list(&h, &la::System),
        _ => la::list(std::path::Path::new("/nonexistent"), &la::System),
    })
    .await
    .unwrap_or_else(|_| la::list(std::path::Path::new("/nonexistent"), &la::System));
    Json(r)
}

#[derive(Deserialize)]
struct RepairQuery {
    #[serde(default)]
    dry_run: bool,
}

async fn repair(Query(q): Query<RepairQuery>) -> Json<Vec<RepairResult>> {
    let out = tokio::task::spawn_blocking(move || match home() {
        Some(h) if cfg!(target_os = "macos") => la::repair(&h, &la::System, q.dry_run),
        _ => vec![],
    })
    .await
    .unwrap_or_default();
    Json(out)
}

async fn restart(
    Path(label): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = tokio::task::spawn_blocking(move || match home() {
        Some(h) if cfg!(target_os = "macos") => la::restart(&h, &la::System, &label),
        _ => Err("LaunchAgents exist only on macOS".to_owned()),
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match r {
        Ok(label) => Ok(Json(serde_json::json!({"ok": true, "label": label}))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routes_are_registered_under_v1() {
        let _ = routes();
    }
}
