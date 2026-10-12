// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! REST routes for `vz` backend state the runner serves but that had no HTTP surface. Merged into the main router by
//! `router()`.
//!
//! * `GET  /v1/host/apple`
//! * `GET  /v1/host/apple/capabilities`
//! * `GET  /v1/host/overview`
//! * `GET  /v1/stats`
//! * `GET|POST /v1/host/apple/ipsw`
//! * `GET  /v1/vms/{id}/vz/secure-boot`
//! * `GET  /v1/vms/{id}/vz/custom-virtio`
//! * `POST /v1/vms/{id}/vz/display`
//! * `GET  /v1/vms/{id}/vz/attestation`
//! * `POST /v1/vms/{id}/vz/custom-virtio/reset`
//! * `GET  /v1/vms/{id}/vz/usb`
//! * `GET|POST /v1/vms/{id}/vz/usb/physical`

use super::{ApiError, ApiResult, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use fluxvm_core::config::Role;
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// When the router was built, for the overview's daemon uptime.
static DAEMON_STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    DAEMON_STARTED.get_or_init(std::time::Instant::now);
    Router::new()
        .route("/v1/host/apple", get(host_apple))
        .route("/v1/host/apple/capabilities", get(host_capabilities))
        .route("/v1/host/overview", get(host_overview))
        .route("/v1/host/history", get(host_history))
        .route("/v1/stats", get(node_stats))
        .route("/v1/host/storage", get(host_storage))
        .route(
            "/v1/vms/{id}/usernet/policy",
            get(usernet_policy).put(usernet_policy_set),
        )
        .route("/v1/host/apple/ipsw", get(ipsw_status).post(ipsw_download))
        .route("/v1/vms/{id}/vz/secure-boot", get(secure_boot))
        .route("/v1/vms/{id}/vz/custom-virtio", get(custom_virtio))
        .route(
            "/v1/vms/{id}/vz/custom-virtio/reset",
            post(custom_virtio_reset),
        )
        .route("/v1/vms/{id}/vz/display", post(display_resize))
        .route("/v1/vms/{id}/vz/attestation", get(attestation))
        .route("/v1/vms/{id}/vz/usb", get(usb_list))
        .route(
            "/v1/vms/{id}/vz/usb/physical",
            get(usb_physical_list).post(usb_physical_attach),
        )
}

async fn host_apple() -> ApiResult<Json<Value>> {
    let caps = fluxvm_scheduler::vz_devices::apple_host_capabilities()
        .await
        .map_err(|e| ApiError {
            status: StatusCode::NOT_IMPLEMENTED,
            message: format!("{e:#}"),
        })?;
    Ok(Json(caps))
}

/// The capability table judged against this Mac: for each feature whether it is available here and, when it is not, why
/// (macOS version, chip generation, missing entitlement). The table itself is static; the verdicts come from the same
/// host probe admission uses.
async fn host_capabilities() -> ApiResult<Json<Value>> {
    Ok(Json(
        fluxvm_scheduler::vz_devices::apple_capabilities_json().await,
    ))
}

/// Everything a dashboard needs in one cheap call: daemon, host, VM counts, per-VM usage rows and helper availability.
/// CPU is cumulative seconds (take two samples for a rate); a VM with no usable runner pid has no usage row.
async fn host_overview(State(m): State<Arc<VmManager>>) -> ApiResult<Json<Value>> {
    let vms = m.list().await;
    let branches = m.list_branches(None).await.map(|b| b.len()).unwrap_or(0);
    let uptime = DAEMON_STARTED.get().map(|t| t.elapsed().as_secs());
    let mut body = fluxvm_scheduler::vz_devices::apple_host_overview_json(vms, branches, uptime)
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{e:#}"),
        })?;
    body["networking"]["netobs_enabled"] = json!(fluxvm_netobs::global().is_some());
    Ok(Json(body))
}

/// Where disk space goes: the volume's free space and what each VM, branch and the image cache occupy (allocated bytes,
/// which count blocks shared with copy-on-write clones in every clone; free space is the truth about what remains).
async fn host_storage(State(m): State<Arc<VmManager>>) -> ApiResult<Json<Value>> {
    let r = m.storage_report().await.map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: format!("{e:#}"),
    })?;
    Ok(Json(json!(r)))
}

#[derive(Deserialize)]
struct HistoryQuery {
    /// RFC 3339; only samples strictly after it are returned.
    since: Option<chrono::DateTime<chrono::Utc>>,
}

/// The last hour of host memory and per-VM runner usage at five-second spacing, for a dashboard that connects late.
/// In memory only: it starts empty when the daemon starts.
async fn host_history(Query(q): Query<HistoryQuery>) -> Json<Value> {
    let items = fluxvm_scheduler::history::global().since(q.since);
    Json(json!({
        "interval_seconds": fluxvm_scheduler::history::INTERVAL_SECONDS,
        "capacity": fluxvm_scheduler::history::CAPACITY,
        "items": items,
    }))
}

/// One sample of this host and every VM on it (CPU, memory, disk and network counters) for the fleet's metrics
/// collector; see `fluxvm_scheduler::node_stats` for what each backend can report.
async fn node_stats(State(m): State<Arc<VmManager>>) -> ApiResult<Json<Value>> {
    let vms = m.list().await;
    let dir = m.cfg.state_dir.clone();
    let s = tokio::task::spawn_blocking(move || fluxvm_scheduler::node_stats::sample(&vms, &dir))
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("stats sampler failed: {e}"),
        })?;
    Ok(Json(json!(s)))
}

/// The egress policy of a usernet guest as stored.
async fn usernet_policy(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(m.vz_usernet_policy(id).await?))
}

/// Changes a usernet guest's egress policy; applied to the running gateway at once (open connections are not cut) and
/// stored, so restarts, forks and branch starts use it. Absent fields keep their value.
async fn usernet_policy_set(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    actor: Option<Extension<crate::AuditActor>>,
    Json(patch): Json<fluxvm_scheduler::vz_devices::PolicyPatch>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let actor_name = actor.as_ref().map(|a| a.0.0.as_str());
    Ok(Json(m.vz_usernet_policy_set(id, patch, actor_name).await?))
}

async fn secure_boot(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(m.vz_secure_boot_status(id).await?))
}

async fn custom_virtio(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(m.vz_custom_virtio_status(id).await?))
}

async fn custom_virtio_reset(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    m.vz_custom_virtio_reset(id).await?;
    Ok(Json(json!({"ok": true})))
}

/// The signed boot manifest of the VM's last launch and the host public key (hex), for an out-of-band verifier. Read-only;
/// the tenant guard scopes `{id}`. A host claim, not hardware attestation.
async fn attestation(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(m.vz_attestation(id).await?))
}

#[derive(Deserialize)]
struct DisplayResize {
    width: u32,
    height: u32,
    #[serde(default)]
    display: u32,
    #[serde(default)]
    ppi: Option<u32>,
}

/// Resizes a display of a running guest (admin). The tenant guard has already scoped `{id}` to the token's tenant.
async fn display_resize(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(b): Json<DisplayResize>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    m.vz_display_resize(id, b.width, b.height, b.display, b.ppi)
        .await?;
    Ok(Json(fluxvm_scheduler::vz_devices::display_resize_reply(
        b.width, b.height, b.display, b.ppi,
    )))
}

async fn usb_list(State(m): State<Arc<VmManager>>, Path(id): Path<Uuid>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"items": m.vz_usb_list(id).await?})))
}

async fn usb_physical_list(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"items": m.vz_usb_physical_list(id).await?})))
}

#[derive(Deserialize)]
struct PhysicalAttach {
    registry_id: u64,
}

async fn usb_physical_attach(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(body): Json<PhysicalAttach>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let uuid = m.vz_usb_physical_attach(id, body.registry_id).await?;
    Ok(Json(json!({"uuid": uuid})))
}

fn ipsw_error(e: anyhow::Error) -> ApiError {
    ApiError {
        status: StatusCode::BAD_GATEWAY,
        message: format!("{e:#}"),
    }
}

/// The newest macOS restore image and whether it is cached; downloads nothing.
async fn ipsw_status(State(m): State<Arc<VmManager>>) -> ApiResult<Json<Value>> {
    Ok(Json(m.macos_ipsw(false).await.map_err(ipsw_error)?))
}

/// Downloads the newest macOS restore image into the cache (about 25 GB, resumable; admin-only).
async fn ipsw_download(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    Ok(Json(m.macos_ipsw(true).await.map_err(ipsw_error)?))
}
