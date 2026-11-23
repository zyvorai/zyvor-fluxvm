// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The Studio Images security view: `GET /v1/oci/security` (one verdict per cached OCI image, the policy, how old the
//! advisory data is, recent overrides) and `GET /v1/oci/images/{what}/sbom` (SPDX or CycloneDX JSON). Read-only; the
//! checks that change anything (`image verify`, `scan --update`, `policy init`) are `fluxctl` commands, which Studio
//! shows with Copy as fluxctl. Same code as the CLI: `fluxvm_image::{sbom, advisory, supply}`.

use crate::ApiError;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use fluxvm_image::sbom::{self, Format, Subject};
use fluxvm_image::{advisory, supply};
use fluxvm_scheduler::VmManager;
use fluxvm_scheduler::oci_images::{self, OciImageEntry};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new()
        .route("/v1/oci/security", axum::routing::get(security))
        .route("/v1/oci/images/{what}/sbom", axum::routing::get(sbom_of))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn subject_of(e: &OciImageEntry) -> Subject {
    Subject {
        reference: e
            .references
            .first()
            .cloned()
            .unwrap_or_else(|| e.manifest_digest.clone()),
        manifest_digest: e.manifest_digest.clone(),
        architecture: e.architecture.clone(),
        layers: e.layers.clone(),
    }
}

/// The last few lines of `policy-overrides.log`, newest last.
fn recent_overrides(state_dir: &std::path::Path, n: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(state_dir.join("oci").join(supply::OVERRIDE_LOG))
        .unwrap_or_default();
    let mut v: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let skip = v.len().saturating_sub(n);
    v.drain(..skip);
    v
}

async fn security(State(m): State<Arc<VmManager>>) -> Result<Json<Value>, ApiError> {
    let entries = m.oci_list()?;
    let state_dir = m.cfg.state_dir.clone();
    let out = tokio::task::spawn_blocking(move || {
        let t = now();
        let items: Vec<supply::Assessment> = entries
            .iter()
            .map(|e| supply::assess(&state_dir, &subject_of(e), t))
            .collect();
        let policy = supply::load_policy(&state_dir);
        let advisories = advisory::load(&state_dir);
        json!({
            "items": items,
            "policy": {
                "present": matches!(policy, Ok(Some(_))),
                "path": supply::policy_path(&state_dir),
                "rules": policy.as_ref().ok().and_then(|p| p.as_ref()),
                "error": policy.as_ref().err().map(|e| format!("{e:#}")),
            },
            "advisory": advisory::status(advisories.as_ref().ok().and_then(|a| a.as_ref()), t),
            "overrides": recent_overrides(&state_dir, 20),
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("security check worker failed: {e}"))?;
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
struct SbomQuery {
    /// `spdx` (default) or `cyclonedx`.
    format: Option<String>,
}

async fn sbom_of(
    State(m): State<Arc<VmManager>>,
    Path(what): Path<String>,
    Query(q): Query<SbomQuery>,
) -> Result<Json<Value>, ApiError> {
    let format = Format::parse(q.format.as_deref().unwrap_or("spdx"))?;
    let entry = oci_images::find(&m.cfg, &what)?;
    let state_dir = m.cfg.state_dir.clone();
    let doc = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let s = sbom::load_or_collect(&state_dir, &subject_of(&entry), now())?;
        Ok(sbom::render(&s, format, env!("CARGO_PKG_VERSION")))
    })
    .await
    .map_err(|e| anyhow::anyhow!("sbom worker failed: {e}"))??;
    Ok(Json(doc))
}
