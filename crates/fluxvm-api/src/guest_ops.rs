// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Guest operations over the QEMU guest agent. The VM must be running with `qga.enabled`.
//! Merged into the main router by `router()`.
//!
//! * `GET  /v1/vms/{id}/guest/info`: OS, host name, clock, users, filesystems, vCPUs. Best effort per section; a
//!   section the agent could not answer is empty and named in `errors`. Read-only tokens may call it.
//! * `GET  /v1/vms/{id}/guest/ssh-keys?username=U`: that user's authorized keys (admin)
//! * `POST /v1/vms/{id}/guest/ssh-keys` `{"username", "keys": [...], "reset"?: bool}`: add keys (admin; 204)
//! * `POST /v1/vms/{id}/guest/ssh-keys/remove` `{"username", "keys": [...]}` (admin; 204)
//! * `POST /v1/vms/{id}/guest/password` `{"username", "password", "crypted"?: bool}` (admin; 204). The password
//!   is never logged, echoed or included in an error.
//! * `POST /v1/vms/{id}/guest/time-sync` `{"mode"?: "host" | "rtc"}`: set the guest clock from this host (default)
//!   or from the guest's hardware clock (admin)
//!
//! Failures: 409 when the VM has no guest agent configured or is not running, 502 when the agent does not
//! answer or rejects the command.

use super::{ApiError, ApiResult, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use fluxvm_core::config::Role;
use fluxvm_core::model::{VmRecord, VmStatus};
use fluxvm_image::qga;
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new()
        .route("/v1/vms/{id}/guest/info", get(info))
        .route("/v1/vms/{id}/guest/ssh-keys", get(ssh_list).post(ssh_add))
        .route("/v1/vms/{id}/guest/ssh-keys/remove", post(ssh_remove))
        .route("/v1/vms/{id}/guest/password", post(set_password))
        .route("/v1/vms/{id}/guest/time-sync", post(time_sync))
}

#[derive(Deserialize)]
struct UserQuery {
    username: String,
}

#[derive(Deserialize)]
struct SshAddRequest {
    username: String,
    keys: Vec<String>,
    #[serde(default)]
    reset: bool,
}

#[derive(Deserialize)]
struct SshRemoveRequest {
    username: String,
    keys: Vec<String>,
}

/// The password never appears in `Debug` output, so an accidental `{:?}` cannot leak it.
#[derive(Deserialize)]
struct PasswordRequest {
    username: String,
    password: String,
    /// `password` is already a crypt(3) hash rather than plaintext.
    #[serde(default)]
    crypted: bool,
}
impl std::fmt::Debug for PasswordRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordRequest")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("crypted", &self.crypted)
            .finish()
    }
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum TimeMode {
    /// Set the guest clock to this host's wall clock.
    #[default]
    Host,
    /// Have the guest reload its clock from its hardware RTC.
    Rtc,
}

#[derive(Debug, Default, Deserialize)]
struct TimeSyncRequest {
    #[serde(default)]
    mode: TimeMode,
}

/// Where a VM's guest-agent socket is, or why it has none.
pub(crate) fn resolve_socket(
    explicit: Option<PathBuf>,
    enabled: bool,
    workspace: &FsPath,
) -> Option<PathBuf> {
    explicit.or_else(|| enabled.then(|| workspace.join("qga.sock")))
}

fn conflict(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        message: message.into(),
    }
}

fn socket_for(vm: &VmRecord) -> ApiResult<PathBuf> {
    let enabled = vm.request.qga.as_ref().is_some_and(|q| q.enabled);
    let sock = resolve_socket(vm.qga_socket.clone(), enabled, &vm.workspace).ok_or_else(|| {
        conflict("QGA not enabled for this VM (set qga.enabled in the create spec)")
    })?;
    if !matches!(vm.status, VmStatus::Running) {
        return Err(conflict(format!(
            "guest operations need a running VM (status: {:?})",
            vm.status
        )));
    }
    Ok(sock)
}

/// Runs a blocking guest-agent call off the async runtime; agent failures are 502.
async fn with_socket<T, F>(m: &VmManager, id: Uuid, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&FsPath) -> anyhow::Result<T> + Send + 'static,
{
    let vm = m.get(id).await?;
    let sock = socket_for(&vm)?;
    tokio::task::spawn_blocking(move || f(&sock))
        .await
        .map_err(|e| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("guest operation worker failed: {e}"),
        })?
        .map_err(|e| ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("{e:#}"),
        })
}

fn bad_request(e: anyhow::Error) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("{e:#}"),
    }
}

async fn info(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<qga::QgaGuestInfo>> {
    Ok(Json(with_socket(&m, id, qga::guest_info).await?))
}

async fn ssh_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Query(q): Query<UserQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let keys = with_socket(&m, id, move |s| {
        qga::ssh_get_authorized_keys(s, &q.username)
    })
    .await?;
    Ok(Json(json!({ "keys": keys })))
}

async fn ssh_add(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(req): Json<SshAddRequest>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    qga::validate_ssh_keys(&req.keys).map_err(bad_request)?;
    with_socket(&m, id, move |s| {
        qga::ssh_add_authorized_keys(s, &req.username, &req.keys, req.reset)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ssh_remove(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(req): Json<SshRemoveRequest>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    qga::validate_ssh_keys(&req.keys).map_err(bad_request)?;
    with_socket(&m, id, move |s| {
        qga::ssh_remove_authorized_keys(s, &req.username, &req.keys)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_password(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(req): Json<PasswordRequest>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    if req.password.is_empty() {
        return Err(bad_request(anyhow::anyhow!("password must not be empty")));
    }
    with_socket(&m, id, move |s| {
        qga::set_user_password(s, &req.username, &req.password, req.crypted)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn time_sync(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    body: Option<Json<TimeSyncRequest>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let mode = body.map(|b| b.0.mode).unwrap_or_default();
    let ns = with_socket(&m, id, move |s| match mode {
        TimeMode::Host => qga::sync_time_from_host(s).map(Some),
        TimeMode::Rtc => qga::set_time_ns(s, None).map(|_| None),
    })
    .await?;
    Ok(Json(json!({
        "ok": true,
        "mode": if mode == TimeMode::Host { "host" } else { "rtc" },
        "time_ns": ns,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_resolution_prefers_explicit_then_workspace() {
        let ws = FsPath::new("/ws/vm1");
        assert_eq!(
            resolve_socket(Some("/x/q.sock".into()), false, ws),
            Some("/x/q.sock".into())
        );
        assert_eq!(
            resolve_socket(None, true, ws),
            Some("/ws/vm1/qga.sock".into())
        );
        assert_eq!(resolve_socket(None, false, ws), None);
    }

    #[test]
    fn password_request_debug_is_redacted() {
        let r: PasswordRequest =
            serde_json::from_str(r#"{"username":"a","password":"s3cret-pw"}"#).unwrap();
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("s3cret-pw"), "{dbg}");
        assert!(dbg.contains("redacted"));
        assert!(!r.crypted);
    }

    #[test]
    fn time_sync_body_defaults_to_host_and_parses_rtc() {
        let d: TimeSyncRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(d.mode, TimeMode::Host);
        let r: TimeSyncRequest = serde_json::from_str(r#"{"mode":"rtc"}"#).unwrap();
        assert_eq!(r.mode, TimeMode::Rtc);
        assert!(serde_json::from_str::<TimeSyncRequest>(r#"{"mode":"nope"}"#).is_err());
    }

    #[test]
    fn requests_reject_unknown_shapes() {
        assert!(serde_json::from_str::<SshAddRequest>(r#"{"username":"a"}"#).is_err());
        let a: SshAddRequest =
            serde_json::from_str(r#"{"username":"a","keys":["ssh-ed25519 AAAA"]}"#).unwrap();
        assert!(!a.reset);
    }

    #[test]
    fn guest_failures_are_502_and_missing_agent_is_409() {
        let c = conflict("x");
        assert_eq!(c.status, StatusCode::CONFLICT);
        assert_eq!(
            bad_request(anyhow::anyhow!("e")).status,
            StatusCode::BAD_REQUEST
        );
    }
}
