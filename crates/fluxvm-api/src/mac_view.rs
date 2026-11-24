// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A view of the `fluxctl mac` instances on this host, for the Studio Mac page, with start, stop, restart, kill and remove.
//!
//! * `GET /v1/mac` (admin) lists the instances (VMs labelled `fluxvm.mac=instance`), the compose projects they belong to, the images
//!   of the local template store, and which registries have a saved login (user names only, never a password).
//! * `POST /v1/mac/instances/{id}/{start|stop|restart|kill|rm}` does what `fluxctl mac start|stop|restart|kill|rm` does, including the
//!   labels those commands keep (`fluxvm.mac.stopped`, the health start time), so the CLI and Studio never disagree.
//!
//! * `PATCH /v1/mac/instances/{id}` is `fluxctl mac update`: restart policy and labels on a stopped or running instance, CPUs and
//!   memory on a stopped one (they are read when the VM boots), or on a running one with `restart_now` (checkpoint, stop, apply,
//!   start). What needs a restart and was not asked for is listed in `pending_restart`, never silently dropped.
//!
//! Logs and inspect use the existing `GET /v1/vms/{id}/logs` and `GET /v1/vms/{id}`. The template store and the login file are read
//! from the account the daemon runs as (`FLUXVM_MAC_STORE`, `FLUXVM_MAC_REGISTRY_AUTH`); when the daemon is a service of another
//! user than the one running `fluxctl`, the answer says where it looked. Nothing here has run against a real Mac.

use crate::{ApiError, ApiResult, Role, require_admin};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

const INSTANCE_LABEL: &str = "fluxvm.mac";
const IMAGE_LABEL: &str = "fluxvm.mac.image";
const RESTART_LABEL: &str = "fluxvm.mac.restart";
const PROJECT_LABEL: &str = "fluxvm.mac.project";
const SERVICE_LABEL: &str = "fluxvm.mac.service";
const STOPPED_LABEL: &str = "fluxvm.mac.stopped";
const NOT_STARTED_LABEL: &str = "fluxvm.mac.notstarted";
const HEALTH_START_LABEL: &str = "fluxvm.mac.health.start";
const HEALTH_LABEL: &str = "fluxvm.mac.health";
const HEALTH_STREAK_LABEL: &str = "fluxvm.mac.health.streak";
const HEALTH_AT_LABEL: &str = "fluxvm.mac.health.checked";
/// Apple's licence limit: two macOS guests per Mac.
const MAC_LIMIT: usize = 2;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new()
        .route("/v1/mac", axum::routing::get(get_mac))
        .route(
            "/v1/mac/instances/{id}/{action}",
            axum::routing::post(instance_action),
        )
        .route(
            "/v1/mac/instances/{id}",
            axum::routing::patch(update_instance),
        )
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn is_internal(k: &str) -> bool {
    k == INSTANCE_LABEL || k.starts_with("fluxvm.mac.")
}

/// The last healthcheck result `mac supervise` / `mac health` recorded, or null when no probe ever ran.
fn health_of(vm: &Value) -> Value {
    let l = &vm["labels"];
    let Some(status) = l[HEALTH_LABEL]
        .as_str()
        .filter(|s| matches!(*s, "starting" | "healthy" | "unhealthy"))
    else {
        return Value::Null;
    };
    let num = |k: &str| {
        l[k].as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
    };
    json!({"status": status, "failing_streak": num(HEALTH_STREAK_LABEL), "last_check_unix": num(HEALTH_AT_LABEL)})
}

/// One instance as the page shows it, from the daemon's VM record.
fn instance_row(vm: &Value) -> Value {
    let ports: Vec<Value> = vm["request"]["network"]["forwards"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(json!({"host": f["host_port"].as_u64()?, "guest": f["guest_port"].as_u64()?}))
                })
                .collect()
        })
        .unwrap_or_default();
    let labels: Map<String, Value> = vm["labels"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !is_internal(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let name = vm["name"].as_str().unwrap_or_default();
    json!({
        "id": vm["id"],
        "name": name,
        "image": vm["labels"][IMAGE_LABEL],
        "status": vm["status"].as_str().unwrap_or("unknown"),
        "ip": vm["guest_ip"],
        "cpus": vm["request"]["vcpus"],
        "memory_mib": vm["request"]["memory_mib"],
        "created": vm["created_at"],
        "ports": ports,
        "restart_policy": vm["labels"][RESTART_LABEL].as_str().unwrap_or("no"),
        "health": health_of(vm),
        "project": vm["labels"][PROJECT_LABEL],
        "service": vm["labels"][SERVICE_LABEL],
        "error": vm["error"],
        "labels": labels,
    })
}

/// Compose projects (`fluxctl mac compose ls`): instances grouped by their project label.
fn projects(rows: &[Value]) -> Vec<Value> {
    let mut by: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for r in rows {
        if let Some(p) = r["project"].as_str().filter(|p| !p.is_empty()) {
            by.entry(p.to_owned()).or_default().push(r);
        }
    }
    by.into_iter()
        .map(|(name, members)| {
            let running = members
                .iter()
                .filter(|m| m["status"].as_str() == Some("running"))
                .count();
            json!({
                "name": name,
                "running": running,
                "total": members.len(),
                "services": members.iter().map(|m| json!({
                    "service": m["service"], "instance": m["name"], "status": m["status"],
                    "health": m["health"],
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// The LaunchAgent label of `fluxctl mac supervise --install`, the one thing that applies a restart policy.
const SUPERVISOR_LABEL: &str = "dev.zyvor.fluxvm.mac-supervise";

/// Whether the restarter is installed for the account the daemon runs as. Without it a restart policy is only a label.
fn supervisor_view(home: Option<&FsPath>) -> Value {
    let plist = home.map(|h| {
        h.join("Library/LaunchAgents")
            .join(format!("{SUPERVISOR_LABEL}.plist"))
    });
    json!({
        "installed": plist.as_ref().is_some_and(|p| p.exists()),
        "label": SUPERVISOR_LABEL,
        "install_command": "fluxctl mac supervise --install",
        "status_command": "fluxctl mac supervise --status",
    })
}

fn store_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_STORE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fluxvm").join("mac-images"))
}

fn auth_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_REGISTRY_AUTH").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/fluxvm/mac-registry-auth.json"))
}

/// The images of the template store (`fluxctl mac images`): reference, source, when added and the disk's apparent size.
fn images_view(root: Option<&FsPath>) -> Value {
    let Some(root) = root else {
        return json!({"path": null, "exists": false, "items": [],
            "note": "The daemon has no home directory, so it cannot find the image store. Set FLUXVM_MAC_STORE for the daemon, or run `fluxctl mac images`."});
    };
    let path = root.display().to_string();
    let text = match std::fs::read_to_string(root.join("index.json")) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({"path": path, "exists": false, "items": [],
                "note": "No images here yet. Pull or build one with `fluxctl mac pull REFERENCE`, then it shows in this list. If you use fluxctl as another user, run `fluxctl mac images` as that user."});
        }
        Err(e) => {
            return json!({"path": path, "exists": true, "items": [],
                "note": format!("The image index could not be read ({e}). Check that the daemon's account can read {path}.")});
        }
    };
    let Ok(index) = serde_json::from_str::<Value>(&text) else {
        return json!({"path": path, "exists": true, "items": [],
            "note": "The image index is not valid JSON. Run `fluxctl mac images` to see what fluxctl makes of it."});
    };
    let items: Vec<Value> = index["images"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(reference, e)| {
                    let dir = e["dir"].as_str().unwrap_or_default();
                    // The directory name is one path segment; refuse anything that could point elsewhere.
                    let safe = !dir.is_empty() && !dir.contains('/') && !dir.contains("..");
                    let disk = safe
                        .then(|| std::fs::metadata(root.join(dir).join("disk.raw")))
                        .and_then(Result::ok);
                    json!({
                        "reference": reference,
                        "digest": e["digest"],
                        "source": e["source"],
                        "added_unix": e["added_unix"],
                        "size_bytes": disk.as_ref().map(std::fs::Metadata::len),
                        "present": disk.is_some(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({"path": path, "exists": true, "items": items, "note": null})
}

/// Registries with a saved login: names only. The password is never read into the answer.
fn logins_view(path: Option<&FsPath>) -> Value {
    let Some(path) = path else {
        return json!({"path": null, "items": [], "problem": null});
    };
    let p = path.display().to_string();
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return json!({"path": p, "items": [], "problem": null});
        }
        Err(e) => {
            return json!({"path": p, "items": [], "problem": format!("{p} could not be read ({e}). Check that the daemon's account can read it.")});
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return json!({"path": p, "items": [], "problem":
                format!("{p} can be read by other users, so fluxctl will not use it. Run `chmod 600 {p}` (or delete it and log in again).")});
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    let parsed = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let Some(file) = parsed else {
        return json!({"path": p, "items": [], "problem":
            format!("{p} is not a valid login file. Delete it (`rm {p}`) and run `fluxctl mac login REGISTRY` again.")});
    };
    let items: Vec<Value> = file["registries"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(r, l)| json!({"registry": r, "username": l["username"]}))
                .collect()
        })
        .unwrap_or_default();
    json!({"path": p, "items": items, "problem": null})
}

/// `GET /v1/mac` (admin).
async fn get_mac(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let vms = m.list().await;
    let mut rows: Vec<Value> = vms
        .iter()
        .filter(|v| v.labels.get(INSTANCE_LABEL).map(String::as_str) == Some("instance"))
        .filter_map(|v| serde_json::to_value(v).ok())
        .map(|v| instance_row(&v))
        .collect();
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    // The licence limit counts every macOS guest on the host, not only the instances of this list.
    let macos_running = vms
        .iter()
        .filter(|v| {
            serde_json::to_value(v).is_ok_and(|j| {
                j["request"]["apple"]["guest_os"].as_str() == Some("macos")
                    && j["status"].as_str() == Some("running")
            })
        })
        .count();
    let (root, auth) = (store_root(), auth_path());
    let (images, logins) = tokio::task::spawn_blocking(move || {
        (images_view(root.as_deref()), logins_view(auth.as_deref()))
    })
    .await
    .map_err(|e| ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: e.to_string(),
    })?;
    Ok(Json(json!({
        "schema": 1,
        "supported": cfg!(target_os = "macos"),
        "macos_limit": MAC_LIMIT,
        "macos_running": macos_running,
        "projects": projects(&rows),
        "instances": rows,
        "images": images,
        "logins": logins,
        "supervisor": supervisor_view(std::env::var_os("HOME").map(PathBuf::from).as_deref()),
    })))
}

/// The labels `fluxctl mac start` and `restart` change: no longer "created only" or "stopped by you", and a new health start period.
fn start_patch(labels: &BTreeMap<String, String>, now: u64) -> BTreeMap<String, Option<String>> {
    let mut p = BTreeMap::new();
    for k in [STOPPED_LABEL, NOT_STARTED_LABEL] {
        if labels.contains_key(k) {
            p.insert(k.to_owned(), None);
        }
    }
    p.insert(HEALTH_START_LABEL.to_owned(), Some(now.to_string()));
    p
}

/// `POST /v1/mac/instances/{id}/{action}` (admin): action is start, stop, restart, kill or rm. The answer names the `fluxctl` command.
async fn instance_action(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path((id, action)): Path<(Uuid, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let vm = m.get(id).await?;
    if vm.labels.get(INSTANCE_LABEL).map(String::as_str) != Some("instance") {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!(
                "{} is not a `fluxctl mac` instance. Manage other machines on the Machines page, or with `fluxctl stop ID`.",
                vm.name
            ),
        });
    }
    let patch = |labels: BTreeMap<String, Option<String>>| fluxvm_core::model::VmPatch {
        name: None,
        labels,
    };
    match action.as_str() {
        "start" | "restart" => {
            if action == "start" {
                m.start(id).await?;
            } else {
                m.restart(id).await?;
            }
            m.patch(id, patch(start_patch(&vm.labels, now_unix())))
                .await?;
        }
        "stop" => {
            m.stop(id).await?;
            m.patch(
                id,
                patch(BTreeMap::from([(
                    STOPPED_LABEL.to_owned(),
                    Some("true".to_owned()),
                )])),
            )
            .await?;
        }
        "kill" => {
            m.kill(id).await?;
        }
        "rm" => {
            // Like `fluxctl mac rm`: a stop that fails because it is already stopped is fine.
            let _ = m.stop(id).await;
            m.delete(id).await?;
        }
        other => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "'{other}' is not an action: use start, stop, restart, kill or rm"
                ),
            });
        }
    }
    Ok(Json(json!({
        "ok": true,
        "action": action,
        "name": vm.name,
        "fluxctl": format!("fluxctl mac {action} {}", vm.name),
    })))
}

// ------------------------------------------------------------------ update

/// Body of `PATCH /v1/mac/instances/{id}`.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct UpdateRequest {
    cpus: Option<u8>,
    memory_mib: Option<u64>,
    /// `no`, `always`, `unless-stopped` or `on-failure`.
    restart: Option<String>,
    /// Labels to set.
    labels: BTreeMap<String, String>,
    /// Label keys to remove.
    rm_labels: Vec<String>,
    /// Refused: a macOS VM stores no environment.
    env: BTreeMap<String, String>,
    /// Stop, apply and start again when CPUs or memory change on a running instance.
    restart_now: bool,
    /// Save a disk checkpoint before a restart (default true).
    checkpoint: Option<bool>,
}

/// What the update needs to know about the instance.
struct Facts {
    name: String,
    live: bool,
    labels: BTreeMap<String, String>,
    cpus: u8,
    memory_mib: u64,
}

/// What the update does to the daemon; a fake in the tests.
#[allow(async_fn_in_trait)]
trait UpdateOps {
    async fn patch_labels(
        &self,
        id: Uuid,
        labels: BTreeMap<String, Option<String>>,
    ) -> anyhow::Result<()>;
    async fn set_resources(
        &self,
        id: Uuid,
        cpus: Option<u8>,
        memory_mib: Option<u64>,
    ) -> anyhow::Result<()>;
    async fn stop(&self, id: Uuid) -> anyhow::Result<()>;
    async fn start(&self, id: Uuid) -> anyhow::Result<()>;
    /// A disk-only checkpoint of the stopped instance.
    async fn checkpoint(&self, id: Uuid, tag: &str) -> anyhow::Result<()>;
}

impl UpdateOps for Arc<VmManager> {
    async fn patch_labels(
        &self,
        id: Uuid,
        labels: BTreeMap<String, Option<String>>,
    ) -> anyhow::Result<()> {
        self.patch(id, fluxvm_core::model::VmPatch { name: None, labels })
            .await
            .map(drop)
    }
    async fn set_resources(
        &self,
        id: Uuid,
        cpus: Option<u8>,
        memory_mib: Option<u64>,
    ) -> anyhow::Result<()> {
        VmManager::resize_stopped(self, id, cpus, memory_mib)
            .await
            .map(drop)
    }
    async fn stop(&self, id: Uuid) -> anyhow::Result<()> {
        VmManager::stop(self, id).await.map(drop)
    }
    async fn start(&self, id: Uuid) -> anyhow::Result<()> {
        VmManager::start(self, id).await.map(drop)
    }
    async fn checkpoint(&self, id: Uuid, tag: &str) -> anyhow::Result<()> {
        self.create_vm_snapshot_with(id, tag, false).await
    }
}

fn bad(message: String) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        message,
    }
}

fn restart_label_value(p: &str) -> Result<Option<&'static str>, ApiError> {
    // `on-failure:3` is Docker's retry count: accepted, count ignored.
    Ok(match p.split(':').next().unwrap_or("") {
        "no" | "" => None,
        "always" => Some("always"),
        "unless-stopped" => Some("unless-stopped"),
        "on-failure" => Some("on-failure"),
        _ => {
            return Err(bad(format!(
                "restart policy {p:?}: use no, always, unless-stopped or on-failure"
            )));
        }
    })
}

/// The `fluxctl` command that does what the request asked, for the answer and for Studio's copy button.
fn update_command(name: &str, r: &UpdateRequest) -> String {
    let mut c = format!("fluxctl mac update {name}");
    if let Some(n) = r.cpus {
        c += &format!(" --cpus {n}");
    }
    if let Some(m) = r.memory_mib {
        c += &if m % 1024 == 0 {
            format!(" --memory {}g", m / 1024)
        } else {
            format!(" --memory {m}m")
        };
    }
    if let Some(p) = &r.restart {
        c += &format!(" --restart {p}");
    }
    for (k, v) in &r.labels {
        c += &format!(" --label {k}={v}");
    }
    for k in &r.rm_labels {
        c += &format!(" --rm-label {k}");
    }
    if r.restart_now {
        c += " --restart-now";
    }
    if r.checkpoint == Some(false) {
        c += " --no-checkpoint";
    }
    c
}

/// The whole update. Validation first, so a refused request changes nothing; then labels, then (per the state) resources.
async fn apply_update<O: UpdateOps>(
    ops: &O,
    id: Uuid,
    vm: &Facts,
    r: &UpdateRequest,
    now: u64,
) -> Result<Value, ApiError> {
    let name = &vm.name;
    if !r.env.is_empty() {
        return Err(bad(format!(
            "environment variables cannot be stored on a macOS VM ({name}): the guest keeps no environment between runs, and `mac run` and `mac exec` send it over SSH each time. Pass it when you run something: `fluxctl mac exec {name} -e KEY=VALUE -- CMD`. Nothing was changed.\n  more: fluxctl explain MAC_UPDATE_ENV_NOT_STORED"
        )));
    }
    if r.cpus == Some(0) || r.memory_mib.is_some_and(|m| m < 512) {
        return Err(bad(format!(
            "a macOS VM needs at least 1 CPU and 512 MiB of memory (asked for {} CPUs, {} MiB). Nothing was changed.",
            r.cpus.map_or("unchanged".to_owned(), |c| c.to_string()),
            r.memory_mib
                .map_or("unchanged".to_owned(), |m| m.to_string())
        )));
    }
    let mut labels: BTreeMap<String, Option<String>> = BTreeMap::new();
    if let Some(p) = &r.restart {
        labels.insert(
            RESTART_LABEL.to_owned(),
            restart_label_value(p)?.map(str::to_owned),
        );
    }
    for (k, v) in &r.labels {
        if is_internal(k) {
            return Err(bad(format!(
                "label {k:?}: keys starting with fluxvm.mac are reserved"
            )));
        }
        labels.insert(k.clone(), Some(v.clone()));
    }
    for k in &r.rm_labels {
        if is_internal(k) {
            return Err(bad(format!(
                "label {k:?}: keys starting with fluxvm.mac are reserved"
            )));
        }
        if r.labels.contains_key(k) {
            return Err(bad(format!(
                "label {k:?} is both set and removed in one update; pick one"
            )));
        }
        labels.insert(k.clone(), None);
    }
    let cpus = r.cpus.filter(|c| *c != vm.cpus);
    let memory = r.memory_mib.filter(|m| *m != vm.memory_mib);
    let resize = cpus.is_some() || memory.is_some();
    if labels.is_empty() && r.cpus.is_none() && r.memory_mib.is_none() {
        return Err(bad(
            "nothing to update: pass cpus, memory_mib, restart, labels or rm_labels (see `fluxctl mac update --help`)".to_owned(),
        ));
    }

    let mut applied: Vec<String> = Vec::new();
    let mut pending: Vec<Value> = Vec::new();
    let mut restarted = false;
    let mut checkpoint: Option<String> = None;
    let mut next: Option<String> = None;
    let mut explain: Option<&str> = None;

    let size_words = |c: Option<u8>, m: Option<u64>| {
        let mut v = Vec::new();
        if let Some(c) = c {
            v.push(format!("cpus {} -> {c}", vm.cpus));
        }
        if let Some(m) = m {
            v.push(format!("memory {} MiB -> {m} MiB", vm.memory_mib));
        }
        v
    };

    if resize && vm.live && !r.restart_now {
        // Labels and policy apply now; the size waits for a restart, and the answer says so.
        for w in size_words(cpus, memory) {
            pending.push(json!({"change": w}));
        }
        explain = Some("MAC_UPDATE_NEEDS_RESTART");
        next = Some(format!(
            "{name} is running, and a macOS VM reads its CPUs and memory only when it boots. The size was NOT changed. To apply it now: `{}` (it saves a checkpoint, stops {name}, applies the change and starts it again; same name, same disk). Or change it on your own time: `fluxctl mac stop {name}`, then repeat this update.",
            update_command(
                name,
                &UpdateRequest {
                    restart_now: true,
                    ..clone_req(r)
                }
            )
        ));
    }
    if resize && vm.live && r.restart_now {
        if !labels.is_empty() {
            ops.patch_labels(id, std::mem::take(&mut labels)).await?;
            applied.push("labels and restart policy".to_owned());
        }
        // Like `mac stop`: marked stopped by you so nothing restarts it in between; start clears the mark again.
        ops.patch_labels(
            id,
            BTreeMap::from([(STOPPED_LABEL.to_owned(), Some("true".to_owned()))]),
        )
        .await?;
        ops.stop(id).await?;
        if r.checkpoint != Some(false) {
            let tag = format!("auto-before-update-{now}");
            if let Err(e) = ops.checkpoint(id, &tag).await {
                // Put it back as it was: the user asked for a change, not for an outage.
                let back = ops.start(id).await;
                let _ = ops
                    .patch_labels(id, start_patch(&vm.labels, now_unix()))
                    .await;
                return Err(ApiError {
                    status: StatusCode::CONFLICT,
                    message: format!(
                        "{name}: the checkpoint before the update could not be saved ({e:#}), so nothing was resized. {} To go on without a checkpoint: `{}`",
                        if back.is_ok() {
                            format!("{name} was started again as it was.")
                        } else {
                            format!("{name} is stopped; start it with `fluxctl mac start {name}`.")
                        },
                        update_command(
                            name,
                            &UpdateRequest {
                                checkpoint: Some(false),
                                ..clone_req(r)
                            }
                        )
                    ),
                });
            }
            checkpoint = Some(tag);
        }
        if let Err(e) = ops.set_resources(id, cpus, memory).await {
            let _ = ops.start(id).await;
            let _ = ops
                .patch_labels(id, start_patch(&vm.labels, now_unix()))
                .await;
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "{name}: the new size was refused ({e:#}); nothing was resized and {name} was started again as it was."
                ),
            });
        }
        applied.extend(size_words(cpus, memory));
        if let Err(e) = ops.start(id).await {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!(
                    "{name}: the new size is saved but {name} did not start ({e:#}). {}Try `fluxctl mac start {name}`; see `fluxctl explain MAC_SLOT_LIMIT` if both macOS slots are taken.",
                    checkpoint.as_ref().map_or(String::new(), |t| format!(
                        "To go back to how it was, restore the checkpoint: `fluxctl mac checkpoint restore {name} {t}`. "
                    ))
                ),
            });
        }
        let _ = ops
            .patch_labels(id, start_patch(&vm.labels, now_unix()))
            .await;
        restarted = true;
    } else {
        if resize && !vm.live {
            ops.set_resources(id, cpus, memory)
                .await
                .map_err(|e| ApiError {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("{name}: {e:#}. Nothing was changed."),
                })?;
            applied.extend(size_words(cpus, memory));
            next = Some(format!(
                "The new size is used the next time {name} boots: `fluxctl mac start {name}`."
            ));
        }
        if !labels.is_empty() {
            ops.patch_labels(id, labels).await?;
            applied.push("labels and restart policy".to_owned());
        }
    }
    if applied.is_empty() && pending.is_empty() {
        applied.push("nothing: the instance already has these values".to_owned());
    }
    Ok(json!({
        "ok": pending.is_empty(),
        "name": name,
        "applied": applied,
        "pending_restart": pending,
        "restarted": restarted,
        "checkpoint": checkpoint,
        "explain": explain,
        "next": next,
        "fluxctl": update_command(name, r),
    }))
}

fn clone_req(r: &UpdateRequest) -> UpdateRequest {
    UpdateRequest {
        cpus: r.cpus,
        memory_mib: r.memory_mib,
        restart: r.restart.clone(),
        labels: r.labels.clone(),
        rm_labels: r.rm_labels.clone(),
        env: BTreeMap::new(),
        restart_now: r.restart_now,
        checkpoint: r.checkpoint,
    }
}

/// `PATCH /v1/mac/instances/{id}` (admin): `fluxctl mac update`.
async fn update_instance(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateRequest>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let vm = m.get(id).await?;
    if vm.labels.get(INSTANCE_LABEL).map(String::as_str) != Some("instance") {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!(
                "{} is not a `fluxctl mac` instance. Manage other machines on the Machines page, or with `fluxctl stop ID`.",
                vm.name
            ),
        });
    }
    let facts = Facts {
        name: vm.name.clone(),
        live: matches!(
            vm.status,
            fluxvm_core::model::VmStatus::Running | fluxvm_core::model::VmStatus::Paused
        ),
        labels: vm.labels.clone(),
        cpus: vm.request.vcpus,
        memory_mib: vm.request.memory_mib,
    };
    Ok(Json(apply_update(&m, id, &facts, &body, now_unix()).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str, status: &str, labels: Value) -> Value {
        json!({"id": "11111111-2222-3333-4444-555555555555", "name": name, "status": status, "guest_ip": "192.168.64.5",
            "created_at": "2026-11-01T10:00:00Z", "labels": labels,
            "request": {"vcpus": 4, "memory_mib": 8192, "network": {"forwards": [{"host_port": 2222, "guest_port": 22}]}}})
    }

    #[test]
    fn a_row_has_the_fields_of_mac_ps_and_hides_internal_labels() {
        let r = instance_row(&vm(
            "ci",
            "running",
            json!({"fluxvm.mac": "instance", "fluxvm.mac.image": "xcode:16", "fluxvm.mac.restart": "always", "team": "ios"}),
        ));
        assert_eq!(r["image"], "xcode:16");
        assert_eq!(r["restart_policy"], "always");
        assert_eq!(r["ports"][0], json!({"host": 2222, "guest": 22}));
        assert_eq!(
            (r["cpus"].as_u64(), r["memory_mib"].as_u64()),
            (Some(4), Some(8192))
        );
        assert_eq!(r["labels"], json!({"team": "ios"}));
        let bare = instance_row(&vm("x", "stopped", json!({"fluxvm.mac": "instance"})));
        assert_eq!(bare["restart_policy"], "no");
        assert!(bare["project"].is_null());
    }

    #[test]
    fn projects_group_by_label_and_count_running() {
        let rows: Vec<Value> = [
            ("web", "running", "shop", "web"),
            ("db", "stopped", "shop", "db"),
            ("solo", "running", "", ""),
        ]
        .iter()
        .map(|(n, s, p, sv)| {
            let mut l = json!({"fluxvm.mac": "instance"});
            if !p.is_empty() {
                l[PROJECT_LABEL] = json!(p);
                l[SERVICE_LABEL] = json!(sv);
            }
            instance_row(&vm(n, s, l))
        })
        .collect();
        let p = projects(&rows);
        assert_eq!(p.len(), 1);
        assert_eq!(
            (
                p[0]["name"].as_str(),
                p[0]["running"].as_u64(),
                p[0]["total"].as_u64()
            ),
            (Some("shop"), Some(1), Some(2))
        );
        // Each service carries its health (null when no probe ran), which `depends_on: service_healthy` gates on.
        assert!(p[0]["services"][0].get("health").is_some());
    }

    #[test]
    fn start_clears_the_stopped_marks_and_starts_the_health_clock() {
        let labels = BTreeMap::from([(STOPPED_LABEL.to_owned(), "true".to_owned())]);
        let p = start_patch(&labels, 100);
        assert_eq!(p[STOPPED_LABEL], None);
        assert!(!p.contains_key(NOT_STARTED_LABEL));
        assert_eq!(p[HEALTH_START_LABEL].as_deref(), Some("100"));
    }

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxvm-mac-view-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn images_come_from_the_index_and_a_missing_store_says_what_to_do() {
        let d = scratch("img");
        let none = images_view(Some(&d));
        assert_eq!(none["exists"], false);
        assert!(none["note"].as_str().unwrap().contains("fluxctl mac pull"));
        std::fs::create_dir_all(d.join("abc")).unwrap();
        std::fs::write(d.join("abc/disk.raw"), vec![0u8; 10]).unwrap();
        std::fs::write(
            d.join("index.json"),
            r#"{"images":{"xcode:16":{"dir":"abc","digest":"sha256:abc","source":"ghcr.io/a/x:16","added_unix":5},"evil:1":{"dir":"../x"}}}"#,
        )
        .unwrap();
        let v = images_view(Some(&d));
        let items = v["items"].as_array().unwrap();
        let x = items.iter().find(|i| i["reference"] == "xcode:16").unwrap();
        assert_eq!(
            (x["size_bytes"].as_u64(), x["present"].as_bool()),
            (Some(10), Some(true))
        );
        let e = items.iter().find(|i| i["reference"] == "evil:1").unwrap();
        assert_eq!(
            e["present"], false,
            "a directory that leaves the store is not followed"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn logins_show_names_never_passwords_and_refuse_a_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let d = scratch("login");
        let f = d.join("auth.json");
        std::fs::write(
            &f,
            r#"{"registries":{"ghcr.io":{"username":"bot","password":"hunter2"}}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let bad = logins_view(Some(&f));
        assert!(bad["problem"].as_str().unwrap().contains("chmod 600"));
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ok = logins_view(Some(&f));
        assert_eq!(
            ok["items"][0],
            json!({"registry": "ghcr.io", "username": "bot"})
        );
        assert!(!ok.to_string().contains("hunter2"));
        assert_eq!(logins_view(Some(&d.join("none.json")))["items"], json!([]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn health_comes_from_the_labels_and_is_null_before_the_first_probe() {
        let r = instance_row(&vm(
            "a",
            "running",
            json!({"fluxvm.mac": "instance", "fluxvm.mac.health": "unhealthy", "fluxvm.mac.health.streak": "3", "fluxvm.mac.health.checked": "99"}),
        ));
        assert_eq!(
            r["health"],
            json!({"status": "unhealthy", "failing_streak": 3, "last_check_unix": 99})
        );
        assert_eq!(r["labels"], json!({}), "the health labels are internal");
        let none = instance_row(&vm("b", "running", json!({"fluxvm.mac": "instance"})));
        assert!(none["health"].is_null());
        let junk = instance_row(&vm("c", "running", json!({"fluxvm.mac.health": "weird"})));
        assert!(junk["health"].is_null());
    }

    #[test]
    fn the_supervisor_is_installed_when_its_launch_agent_exists() {
        let dir = scratch("supervisor");
        assert_eq!(supervisor_view(Some(&dir))["installed"], false);
        assert_eq!(supervisor_view(None)["installed"], false);
        let agents = dir.join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join(format!("{SUPERVISOR_LABEL}.plist")), "x").unwrap();
        let v = supervisor_view(Some(&dir));
        assert_eq!(v["installed"], true);
        assert_eq!(v["install_command"], "fluxctl mac supervise --install");
    }

    #[test]
    fn the_routes_are_registered() {
        let _ = routes();
    }

    // ---- update

    #[derive(Default)]
    struct Fake {
        calls: std::cell::RefCell<Vec<String>>,
        fail_checkpoint: bool,
        fail_resources: bool,
        fail_start: bool,
    }

    impl UpdateOps for Fake {
        async fn patch_labels(
            &self,
            _: Uuid,
            l: BTreeMap<String, Option<String>>,
        ) -> anyhow::Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("labels {}", serde_json::to_string(&l).unwrap()));
            Ok(())
        }
        async fn set_resources(
            &self,
            _: Uuid,
            c: Option<u8>,
            m: Option<u64>,
        ) -> anyhow::Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("resources {c:?} {m:?}"));
            if self.fail_resources {
                anyhow::bail!("the vz backend needs at least 1 vCPU and 512 MiB of memory");
            }
            Ok(())
        }
        async fn stop(&self, _: Uuid) -> anyhow::Result<()> {
            self.calls.borrow_mut().push("stop".into());
            Ok(())
        }
        async fn start(&self, _: Uuid) -> anyhow::Result<()> {
            self.calls.borrow_mut().push("start".into());
            if self.fail_start {
                anyhow::bail!("boom");
            }
            Ok(())
        }
        async fn checkpoint(&self, _: Uuid, tag: &str) -> anyhow::Result<()> {
            self.calls.borrow_mut().push(format!("checkpoint {tag}"));
            if self.fail_checkpoint {
                anyhow::bail!("not enough free disk space");
            }
            Ok(())
        }
    }

    fn facts(live: bool) -> Facts {
        Facts {
            name: "ci".into(),
            live,
            labels: BTreeMap::from([(STOPPED_LABEL.to_owned(), "true".to_owned())]),
            cpus: 4,
            memory_mib: 8192,
        }
    }

    fn req(v: Value) -> UpdateRequest {
        serde_json::from_value(v).unwrap()
    }

    fn calls(f: &Fake) -> Vec<String> {
        f.calls.borrow().clone()
    }

    #[tokio::test]
    async fn a_stopped_instance_takes_a_new_size_and_labels_at_once() {
        let f = Fake::default();
        let r = req(
            json!({"cpus": 8, "memory_mib": 16384, "restart": "always", "labels": {"team": "ios"}, "rm_labels": ["old"]}),
        );
        let a = apply_update(&f, Uuid::nil(), &facts(false), &r, 100)
            .await
            .unwrap();
        assert_eq!(a["ok"], true);
        assert_eq!(a["restarted"], false);
        assert_eq!(a["pending_restart"], json!([]));
        let c = calls(&f);
        assert_eq!(c[0], "resources Some(8) Some(16384)");
        assert!(
            c[1].contains("fluxvm.mac.restart\":\"always") && c[1].contains("\"old\":null"),
            "{c:?}"
        );
        assert!(a["next"].as_str().unwrap().contains("fluxctl mac start ci"));
        assert_eq!(
            a["fluxctl"],
            "fluxctl mac update ci --cpus 8 --memory 16g --restart always --label team=ios --rm-label old"
        );
    }

    #[tokio::test]
    async fn a_running_instance_applies_labels_and_says_the_size_needs_a_restart() {
        let f = Fake::default();
        let r = req(json!({"cpus": 8, "restart": "unless-stopped"}));
        let a = apply_update(&f, Uuid::nil(), &facts(true), &r, 100)
            .await
            .unwrap();
        assert_eq!(a["ok"], false);
        assert_eq!(a["explain"], "MAC_UPDATE_NEEDS_RESTART");
        assert_eq!(a["pending_restart"][0]["change"], "cpus 4 -> 8");
        let n = a["next"].as_str().unwrap();
        assert!(
            n.contains("--restart-now") && n.contains("fluxctl mac stop ci"),
            "{n}"
        );
        let c = calls(&f);
        assert_eq!(c.len(), 1, "only the label call: {c:?}");
        assert!(c[0].starts_with("labels"));
    }

    #[tokio::test]
    async fn restart_now_checkpoints_stops_resizes_and_starts_in_that_order() {
        let f = Fake::default();
        let r = req(json!({"memory_mib": 16384, "restart_now": true, "labels": {"a": "b"}}));
        let a = apply_update(&f, Uuid::nil(), &facts(true), &r, 1234)
            .await
            .unwrap();
        assert_eq!(
            (a["ok"].as_bool(), a["restarted"].as_bool()),
            (Some(true), Some(true))
        );
        assert_eq!(a["checkpoint"], "auto-before-update-1234");
        let c = calls(&f);
        let kinds: Vec<&str> = c.iter().map(|s| s.split(' ').next().unwrap()).collect();
        assert_eq!(
            kinds,
            [
                "labels",
                "labels",
                "stop",
                "checkpoint",
                "resources",
                "start",
                "labels"
            ],
            "{c:?}"
        );
        assert!(
            c[1].contains("fluxvm.mac.stopped\":\"true"),
            "marked stopped so a supervisor leaves it alone"
        );
        assert!(
            c[6].contains("fluxvm.mac.stopped\":null") && c[6].contains("fluxvm.mac.health.start"),
            "start clears the mark"
        );
    }

    #[tokio::test]
    async fn no_checkpoint_skips_it() {
        let f = Fake::default();
        let r = req(json!({"cpus": 2, "restart_now": true, "checkpoint": false}));
        let a = apply_update(&f, Uuid::nil(), &facts(true), &r, 1)
            .await
            .unwrap();
        assert!(a["checkpoint"].is_null());
        assert!(!calls(&f).iter().any(|c| c.starts_with("checkpoint")));
        assert!(
            a["fluxctl"]
                .as_str()
                .unwrap()
                .ends_with("--restart-now --no-checkpoint")
        );
    }

    #[tokio::test]
    async fn a_failed_checkpoint_changes_nothing_and_starts_the_vm_again() {
        let f = Fake {
            fail_checkpoint: true,
            ..Fake::default()
        };
        let r = req(json!({"cpus": 2, "restart_now": true}));
        let e = apply_update(&f, Uuid::nil(), &facts(true), &r, 1)
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::CONFLICT);
        assert!(
            e.message.contains("nothing was resized")
                && e.message.contains("started again")
                && e.message.contains("--no-checkpoint"),
            "{}",
            e.message
        );
        let c = calls(&f);
        assert!(!c.iter().any(|c| c.starts_with("resources")), "{c:?}");
        assert!(c.iter().any(|c| c == "start"));
    }

    #[tokio::test]
    async fn a_refused_size_after_the_stop_puts_the_vm_back() {
        let f = Fake {
            fail_resources: true,
            ..Fake::default()
        };
        let r = req(json!({"cpus": 2, "restart_now": true, "checkpoint": false}));
        let e = apply_update(&f, Uuid::nil(), &facts(true), &r, 1)
            .await
            .unwrap_err();
        assert!(
            e.message.contains("started again as it was"),
            "{}",
            e.message
        );
        assert_eq!(calls(&f).iter().filter(|c| *c == "start").count(), 1);
    }

    #[tokio::test]
    async fn a_start_that_fails_names_the_checkpoint_to_go_back_to() {
        let f = Fake {
            fail_start: true,
            ..Fake::default()
        };
        let r = req(json!({"cpus": 2, "restart_now": true}));
        let e = apply_update(&f, Uuid::nil(), &facts(true), &r, 77)
            .await
            .unwrap_err();
        assert!(
            e.message
                .contains("fluxctl mac checkpoint restore ci auto-before-update-77"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn refusals_change_nothing_and_say_what_to_do() {
        for (body, needle) in [
            (json!({"env": {"A": "b"}}), "MAC_UPDATE_ENV_NOT_STORED"),
            (json!({}), "nothing to update"),
            (json!({"cpus": 0}), "at least 1 CPU"),
            (json!({"memory_mib": 64}), "512 MiB"),
            (json!({"restart": "sometimes"}), "unless-stopped"),
            (json!({"labels": {"fluxvm.mac.x": "1"}}), "reserved"),
            (json!({"rm_labels": ["fluxvm.mac"]}), "reserved"),
            (
                json!({"labels": {"a": "1"}, "rm_labels": ["a"]}),
                "both set and removed",
            ),
        ] {
            let f = Fake::default();
            let e = apply_update(&f, Uuid::nil(), &facts(false), &req(body.clone()), 1)
                .await
                .unwrap_err();
            assert_eq!(e.status, StatusCode::BAD_REQUEST, "{body}");
            assert!(e.message.contains(needle), "{body}: {}", e.message);
            assert!(calls(&f).is_empty(), "{body}: {:?}", calls(&f));
        }
    }

    #[tokio::test]
    async fn the_same_size_is_not_a_change() {
        let f = Fake::default();
        let a = apply_update(
            &f,
            Uuid::nil(),
            &facts(true),
            &req(json!({"cpus": 4, "memory_mib": 8192})),
            1,
        )
        .await
        .unwrap();
        assert_eq!(a["ok"], true);
        assert!(calls(&f).is_empty());
        assert!(a["applied"][0].as_str().unwrap().contains("already"));
    }

    #[test]
    fn unknown_fields_are_rejected_and_the_route_does_not_clash() {
        assert!(serde_json::from_value::<UpdateRequest>(json!({"gpus": 1})).is_err());
        // Building the router panics on a duplicate path + method.
        let _ = routes();
    }
}
