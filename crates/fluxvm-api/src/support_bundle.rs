// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `POST /v1/support-bundle`: the same archive `fluxctl support-bundle` writes, built by the daemon and sent back as a
//! download (the Studio Home page's "Download support bundle"). Admin role only, and refused for a tenant-bound token,
//! because the bundle describes the whole host. Secrets are removed and the finished archive is scanned again before a
//! byte is sent (`fluxvm_core::support_bundle`); a failed scan is a 500 that says what to do next.

use crate::{ApiError, TokenTenant, require_admin};
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use fluxvm_core::config::Role;
use fluxvm_core::support_collect::{self, Inputs};
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new().route("/v1/support-bundle", axum::routing::post(support_bundle))
}

#[derive(Deserialize, Default)]
struct Q {
    /// Lines kept from each log (bounded; default 1000).
    log_lines: Option<usize>,
}

async fn support_bundle(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Query(q): Query<Q>,
) -> Result<Response, ApiError> {
    require_admin(role)?;
    if token_tenant.is_some() {
        return Err(ApiError::forbidden(
            "a support bundle describes the whole host, so a token that is limited to one tenant cannot download it; use an admin token without a tenant, or run `fluxctl support-bundle` on the host",
        ));
    }
    let now = chrono::Utc::now();
    let health = crate::cluster_health::assess_now(&m).await;
    let vms: Vec<serde_json::Value> = m
        .list()
        .await
        .iter()
        .filter_map(|v| serde_json::to_value(v).ok())
        .collect();
    let clusters = crate::kube::list_clusters(State(m.clone()), None)
        .await
        .map(|Json(v)| v)
        .map_err(|e| e.message);
    let cfg = m.cfg.clone();
    let lines = q.log_lines.unwrap_or(0);
    let built = tokio::task::spawn_blocking(move || {
        let version = env!("CARGO_PKG_VERSION").to_owned();
        support_collect::build(&Inputs {
            cfg: &cfg,
            source: "daemon",
            fluxctl_version: version.clone(),
            daemon_version: Ok(version),
            health: serde_json::to_value(&health).map_err(|e| e.to_string()),
            vms: Ok(vms),
            clusters,
            log_lines: lines,
            now,
        })
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!(
            "the support bundle task stopped unexpectedly ({e}). Run `fluxctl support-bundle` on the host instead, or `fluxctl explain SUPPORT_BUNDLE_WRITE_FAILED`"
        ),
    })?
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e.to_string(),
    })?;
    let name = support_collect::default_file_name(now);
    let mut resp = (StatusCode::OK, built.bytes).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/gzip"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(
        HeaderName::from_static("x-fluxvm-support-skipped"),
        HeaderValue::from(built.skipped.len()),
    );
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_route_is_registered_under_v1() {
        let _ = routes();
    }
}
