// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/audit`: the audit log (what was changed, by whom, when, and how it ended), newest first, paginated.
//!
//! Filters (all optional, combined with AND): `since`, `until` (RFC 3339), `action` (prefix: `mac.` or `mac.run`),
//! `target` (part of the name), `actor`, `result` (`ok`, `failed`, `denied`). Paging: `limit` (default 100, at most 1000)
//! and `offset`; a reply with `next_offset` has more. `notice` says in plain words when old records were removed to keep
//! the log within its size cap. Admin only: the log names every actor. The records are the same ones
//! `fluxctl audit ls` reads from the state directory; secrets are redacted before they are written (see
//! `fluxvm_scheduler::audit`).

use crate::{ApiResult, Role, TokenTenant, require_admin};
use axum::{
    Extension, Json,
    extract::{Query, State},
};
use fluxvm_scheduler::{VmManager, audit};
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new().route("/v1/audit", axum::routing::get(list))
}

async fn list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Query(mut q): Query<audit::Query>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    // A tenant-scoped token sees only what its own tenant did, never other tenants' actions.
    if let Some(Extension(TokenTenant(t))) = token_tenant {
        q.actor = Some(t);
    }
    let dir = m.cfg.state_dir.clone();
    let page = tokio::task::spawn_blocking(move || audit::AuditLog::new(&dir).query(&q))
        .await
        .map_err(|e| anyhow::anyhow!("audit read task failed: {e}"))??;
    Ok(Json(
        serde_json::to_value(page).map_err(anyhow::Error::from)?,
    ))
}
