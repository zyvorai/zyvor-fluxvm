// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Backups of macOS instances and named volumes, for the Studio Mac page: the same files and rules as `fluxctl mac backup`
//! (`fluxvm_core::mac_backup`).
//!
//! * `GET    /v1/mac/backups`              (admin) backups, free room on the backups disk, schedules, what can be backed up;
//! * `POST   /v1/mac/backups`              (admin) `{kind: "instance"|"volume", name, keep?}` starts a backup in the background and
//!   answers 202 with its id; a low-disk refusal comes back at once as 409 with the amount needed;
//! * `POST   /v1/mac/backups/{id}/verify`  (admin) checks the archive against its sha256;
//! * `DELETE /v1/mac/backups/{id}`         (admin);
//! * `POST   /v1/mac/backup-schedules`     (admin) `{kind, name, every: "24h", keep?, off?}`.
//!
//! Restoring is not offered here: it creates an instance through the template store of the account that runs `fluxctl`, so Studio
//! shows the exact `fluxctl mac backup restore ID` command to copy. The daemon also takes the due scheduled backups every minute.
//! Not run against a real Mac.

use super::{ApiError, ApiResult, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use fluxvm_core::config::Role;
use fluxvm_core::mac_backup as bk;
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new()
        .route("/v1/mac/backups", get(list).post(create))
        .route("/v1/mac/backups/{id}/verify", post(verify))
        .route("/v1/mac/backups/{id}", delete(remove))
        .route("/v1/mac/backup-schedules", post(schedule))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn status_of(code: &str) -> StatusCode {
    match code {
        "MAC_BACKUP_NOT_FOUND" => StatusCode::NOT_FOUND,
        "MAC_BACKUP_IO" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::CONFLICT,
    }
}

fn api_err(e: bk::BackupError) -> ApiError {
    ApiError {
        status: status_of(e.code),
        message: format!("{} (more: fluxctl explain {})", e.message, e.code),
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        message: msg.into(),
    }
}

fn root() -> Result<PathBuf, ApiError> {
    bk::default_root().ok_or_else(|| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "the daemon has no HOME: set FLUXVM_MAC_BACKUPS to the backups folder".into(),
    })
}

fn volumes_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_VOLUMES").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".fluxvm")
            .join("mac-volumes"),
    )
}

fn valid_volume_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.starts_with(|c: char| c.is_ascii_alphanumeric())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The volume's data folder and labels, read like `fluxctl mac volume` does (`NAME/volume.json`, `NAME/data`).
fn volume_plan(name: &str) -> bk::Result<bk::Plan> {
    let missing = || bk::BackupError {
        code: "MAC_BACKUP_SOURCE",
        message: format!("no volume named {name:?} (see `fluxctl mac volume ls`)"),
    };
    if !valid_volume_name(name) {
        return Err(missing());
    }
    let dir = volumes_root().ok_or_else(missing)?.join(name);
    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join("volume.json")).map_err(|_| missing())?)
            .map_err(|_| missing())?;
    let labels = meta["labels"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    bk::volume_plan(name, &dir.join("data"), &labels)
}

fn volume_names() -> Vec<String> {
    let Some(rd) = volumes_root().and_then(|r| std::fs::read_dir(r).ok()) else {
        return vec![];
    };
    let mut v: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().join("volume.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_volume_name(n))
        .collect();
    v.sort();
    v
}

async fn instance_records(m: &VmManager) -> Vec<Value> {
    m.list()
        .await
        .iter()
        .filter_map(|v| serde_json::to_value(v).ok())
        .filter(|v| v["labels"]["fluxvm.mac"].as_str() == Some("instance"))
        .collect()
}

fn plan_in(instances: &[Value], kind: bk::Kind, name: &str) -> bk::Result<bk::Plan> {
    match kind {
        bk::Kind::Instance => match instances.iter().find(|v| v["name"].as_str() == Some(name)) {
            Some(vm) => bk::instance_plan(vm),
            None => Err(bk::BackupError {
                code: "MAC_BACKUP_SOURCE",
                message: format!(
                    "no instance named {name:?} on this daemon (see `fluxctl mac ps -a`)"
                ),
            }),
        },
        bk::Kind::Volume => volume_plan(name),
    }
}

fn parse_kind(k: &str) -> Result<bk::Kind, ApiError> {
    match k {
        "instance" => Ok(bk::Kind::Instance),
        "volume" => Ok(bk::Kind::Volume),
        _ => Err(bad("kind must be \"instance\" or \"volume\"")),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e.to_string(),
    })
}

fn entry_json(e: &bk::Entry, t: u64) -> Value {
    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
    let m = e.manifest.as_ref();
    v["kind"] = json!(m.map(|m| m.kind.word()));
    v["name"] = json!(m.map(|m| m.name.clone()));
    v["age_seconds"] = json!(m.map(|m| t.saturating_sub(m.created_unix)));
    v["restore"] = json!(format!("fluxctl mac backup restore {}", e.id));
    v["verify"] = json!(format!("fluxctl mac backup verify {}", e.id));
    v
}

/// `GET /v1/mac/backups`
async fn list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let root = root()?;
    let instances = instance_records(&m).await;
    let stopped: Vec<Value> = instances
        .iter()
        .map(|v| json!({"name": v["name"], "status": v["status"]}))
        .collect();
    let t = now();
    let (entries, schedules, free) = blocking({
        let root = root.clone();
        move || {
            (
                bk::list(&root, t),
                bk::load_schedules(&root),
                bk::free_bytes(&root),
            )
        }
    })
    .await?;
    let rows: Vec<Value> = entries.iter().map(|e| entry_json(e, t)).collect();
    Ok(Json(json!({
        "schema": 1,
        "supported": cfg!(target_os = "macos"),
        "root": root,
        "free_bytes": free,
        "reserve_bytes": bk::RESERVE_BYTES,
        "default_keep": bk::DEFAULT_KEEP,
        "backups": rows,
        "schedules": schedules,
        "instances": stopped,
        "volumes": volume_names(),
        "excluded": bk::EXCLUDED,
        "fluxctl": "fluxctl mac backup ls",
    })))
}

#[derive(Deserialize)]
struct CreateBody {
    kind: String,
    name: String,
    keep: Option<u32>,
    note: Option<String>,
}

/// `POST /v1/mac/backups`
async fn create(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Json(b): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let kind = parse_kind(&b.kind)?;
    let root = root()?;
    let instances = instance_records(&m).await;
    let name = b.name.clone();
    let plan = blocking(move || plan_in(&instances, kind, &name))
        .await?
        .map_err(api_err)?;
    // The disk-room refusal is the one people hit: answer it now, not as a failure a minute later.
    bk::check_room(bk::free_bytes(&root), bk::needed_bytes(&plan)).map_err(api_err)?;
    let t = now();
    let id = bk::new_id(&root, kind, &b.name, t);
    bk::record_started(&root, &id);
    let keep = b.keep.unwrap_or(bk::DEFAULT_KEEP).clamp(1, 1000);
    let cmd = format!(
        "fluxctl mac backup create {}{}",
        if kind == bk::Kind::Volume {
            "--volume "
        } else {
            ""
        },
        b.name
    );
    let (root2, id2, note) = (root.clone(), id.clone(), b.note);
    tokio::task::spawn_blocking(move || {
        let r = bk::create(
            &root2,
            bk::Request {
                plan,
                keep,
                automatic: false,
                note,
                now: t,
                id: Some(id2.clone()),
            },
            &bk::free_bytes,
        );
        if let Err(e) = r {
            tracing::warn!(backup = %id2, "mac backup failed: {}", e.message);
            bk::record_failure(
                &root2,
                &id2,
                &format!("{} (more: fluxctl explain {})", e.message, e.code),
            );
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"ok": true, "id": id, "started": true, "fluxctl": cmd})),
    ))
}

/// `POST /v1/mac/backups/{id}/verify`
async fn verify(
    Extension(role): Extension<Role>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let root = root()?;
    let t = now();
    let r = blocking(move || bk::resolve(&root, &id, t).and_then(|m| bk::verify(&root, &m)))
        .await?
        .map_err(api_err)?;
    Ok(Json(json!({"ok": true, "report": r})))
}

/// `DELETE /v1/mac/backups/{id}`
async fn remove(
    Extension(role): Extension<Role>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let root = root()?;
    let id2 = id.clone();
    blocking(move || bk::remove(&root, &id2))
        .await?
        .map_err(api_err)?;
    Ok(Json(
        json!({"ok": true, "id": id, "fluxctl": format!("fluxctl mac backup rm {id}")}),
    ))
}

#[derive(Deserialize)]
struct ScheduleBody {
    kind: String,
    name: String,
    every: Option<String>,
    keep: Option<u32>,
    #[serde(default)]
    off: bool,
}

/// `POST /v1/mac/backup-schedules`
async fn schedule(
    Extension(role): Extension<Role>,
    Json(b): Json<ScheduleBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let kind = parse_kind(&b.kind)?;
    let root = root()?;
    let t = now();
    if b.off {
        let (r2, n) = (root.clone(), b.name.clone());
        let was = blocking(move || bk::clear_schedule(&r2, kind, &n))
            .await?
            .map_err(api_err)?;
        return Ok(Json(json!({"ok": true, "removed": was})));
    }
    let every = b
        .every
        .ok_or_else(|| bad("say how often: every is like 24h"))?;
    let secs = bk::parse_every(&every).map_err(api_err)?;
    let keep = b.keep.unwrap_or(bk::DEFAULT_KEEP);
    let name = b.name.clone();
    let e = blocking(move || bk::set_schedule(&root, kind, &name, secs, keep, t))
        .await?
        .map_err(api_err)?;
    Ok(Json(json!({
        "ok": true, "schedule": e,
        "fluxctl": format!("fluxctl mac backup schedule {}{} --every {every}", if kind == bk::Kind::Volume { "--volume " } else { "" }, b.name),
    })))
}

/// The daemon's own loop: every minute, take the scheduled backups that are due. Cheap when there are none.
pub fn spawn_scheduler(m: Arc<VmManager>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(root) = bk::default_root() else {
                continue;
            };
            let t = now();
            if !bk::load_schedules(&root).iter().any(|s| bk::is_due(s, t)) {
                continue;
            }
            let instances = instance_records(&m).await;
            let r = tokio::task::spawn_blocking(move || {
                bk::run_due(&root, t, &bk::free_bytes, &|k, n| plan_in(&instances, k, n))
            })
            .await;
            for x in r.unwrap_or_default() {
                match x.outcome {
                    Ok(c) => tracing::info!(backup = %c.manifest.id, "scheduled mac backup taken"),
                    Err(e) => {
                        tracing::warn!(name = %x.name, "scheduled mac backup failed: {}", e.message)
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_messages_name_the_fix() {
        assert_eq!(status_of("MAC_BACKUP_NOT_FOUND"), StatusCode::NOT_FOUND);
        assert_eq!(status_of("MAC_BACKUP_NO_ROOM"), StatusCode::CONFLICT);
        let e = api_err(bk::check_room(Some(1 << 30), 1 << 30).unwrap_err());
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert!(
            e.message.contains("fluxctl explain MAC_BACKUP_NO_ROOM")
                && e.message.contains("only 1.0 GiB is free")
        );
    }

    #[test]
    fn plan_in_finds_instances_by_name_and_refuses_running_ones() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        for f in ["disk.raw", "hardware.bin", "auxiliary.bin"] {
            std::fs::write(ws.join(f), b"x").unwrap();
        }
        let vm = |status: &str| json!({"name": "ci", "status": status, "disk": ws.join("disk.raw"), "workspace": ws, "labels": {"fluxvm.mac": "instance"}});
        assert!(plan_in(&[vm("stopped")], bk::Kind::Instance, "ci").is_ok());
        assert_eq!(
            plan_in(&[vm("running")], bk::Kind::Instance, "ci")
                .unwrap_err()
                .code,
            "MAC_BACKUP_RUNNING"
        );
        assert_eq!(
            plan_in(&[vm("stopped")], bk::Kind::Instance, "nope")
                .unwrap_err()
                .code,
            "MAC_BACKUP_SOURCE"
        );
    }

    #[test]
    fn volume_names_cannot_climb_out_of_the_volumes_folder() {
        assert!(
            !valid_volume_name("../x")
                && !valid_volume_name("a/b")
                && !valid_volume_name(".hidden")
                && !valid_volume_name("")
        );
        assert!(valid_volume_name("build-cache_1.2"));
        assert_eq!(volume_plan("../etc").unwrap_err().code, "MAC_BACKUP_SOURCE");
    }

    #[test]
    fn kinds_parse() {
        assert!(parse_kind("instance").is_ok() && parse_kind("volume").is_ok());
        assert_eq!(
            parse_kind("vm").unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }
}
