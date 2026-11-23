// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A view of the `fluxctl mac` instances on this host, for the Studio Mac page, with start, stop, restart, kill and remove.
//!
//! * `GET /v1/mac` (admin) lists the instances (VMs labelled `fluxvm.mac=instance`), the compose projects they belong to, the images
//!   of the local template store, and which registries have a saved login (user names only, never a password).
//! * `POST /v1/mac/instances/{id}/{start|stop|restart|kill|rm}` does what `fluxctl mac start|stop|restart|kill|rm` does, including the
//!   labels those commands keep (`fluxvm.mac.stopped`, the health start time), so the CLI and Studio never disagree.
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
/// Apple's licence limit: two macOS guests per Mac.
const MAC_LIMIT: usize = 2;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new()
        .route("/v1/mac", axum::routing::get(get_mac))
        .route(
            "/v1/mac/instances/{id}/{action}",
            axum::routing::post(instance_action),
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
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
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
    fn the_routes_are_registered() {
        let _ = routes();
    }
}
