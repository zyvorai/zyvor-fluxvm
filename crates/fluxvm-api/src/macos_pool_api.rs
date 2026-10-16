// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! REST routes for the macOS warm pool (`fluxvm_scheduler::macos_pool`).
//!
//! * `GET  /v1/macos-pool`: the enrolled slots
//! * `GET  /v1/macos-pool/status`: whether a checkout can restore (the Mac's login session) and the slot counts
//! * `POST /v1/macos-pool/checkout`: restore a free slot
//! * `POST /v1/macos-pool/{id}/enroll`: turn a running, set-up macOS guest into a slot
//! * `POST /v1/macos-pool/{id}/return`: stop a leased slot

use super::{ApiError, ApiResult, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use fluxvm_core::config::Role;
use fluxvm_core::model::VmRecord;
use fluxvm_scheduler::VmManager;
use fluxvm_scheduler::macos_pool::{NoFreeSlot, SessionBlocksRestore};
use std::sync::Arc;
use uuid::Uuid;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new()
        .route("/v1/macos-pool", get(list))
        .route("/v1/macos-pool/status", get(status))
        .route("/v1/macos-pool/checkout", post(checkout))
        .route("/v1/macos-pool/{id}/enroll", post(enroll))
        .route("/v1/macos-pool/{id}/return", post(give_back))
}

/// No free slot is a state conflict; everything else keeps the default mapping.
fn pool_error(e: anyhow::Error) -> ApiError {
    if e.downcast_ref::<NoFreeSlot>().is_some() {
        return ApiError {
            status: StatusCode::CONFLICT,
            message: format!("{e:#}"),
        };
    }
    if e.downcast_ref::<SessionBlocksRestore>().is_some() {
        return ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: format!("{e:#}"),
        };
    }
    ApiError::from(e)
}

async fn status(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    Ok(Json(m.macos_pool_status().await))
}

async fn list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<Vec<VmRecord>>> {
    require_admin(role)?;
    Ok(Json(m.macos_pool_list().await))
}

async fn checkout(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<VmRecord>> {
    require_admin(role)?;
    m.macos_pool_checkout().await.map(Json).map_err(pool_error)
}

async fn enroll(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<VmRecord>> {
    require_admin(role)?;
    m.macos_pool_enroll(id).await.map(Json).map_err(pool_error)
}

async fn give_back(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<VmRecord>> {
    require_admin(role)?;
    m.macos_pool_return(id).await.map(Json).map_err(pool_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_locked_session_is_a_503_and_a_full_pool_stays_a_409() {
        let blocked = anyhow::Error::from(SessionBlocksRestore {
            slot: Uuid::nil(),
            hint: "the Mac's screen is locked".into(),
            cause: "invalid argument".into(),
        });
        let e = pool_error(blocked);
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(e.message.contains("screen is locked") && e.message.contains("invalid argument"));
        assert_eq!(
            pool_error(NoFreeSlot { enrolled: 2 }.into()).status,
            StatusCode::CONFLICT
        );
    }
}
