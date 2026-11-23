// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/mac/disk` (admin): what `fluxctl mac system df` and `mac system prune --dry-run` report, for Studio. Read-only: nothing is
//! deleted through the API, and the answer carries the `fluxctl` commands that do it ("Copy as fluxctl").
//!
//! It reads the instance records of this daemon and the two folders `fluxctl mac` keeps on this host: the image store
//! (`FLUXVM_MAC_STORE`, default `~/.fluxvm/mac-images`) and the named volumes (`FLUXVM_MAC_VOLUMES`, default `~/.fluxvm/mac-volumes`).
//! Sizes are allocated bytes; APFS clones share blocks, so `would_free_bytes` is an upper bound. Not run against a real Mac.

use super::{ApiResult, require_admin};
use axum::{Extension, Json, Router, extract::State, routing::get};
use fluxvm_core::config::Role;
use fluxvm_scheduler::VmManager;
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new().route("/v1/mac/disk", get(disk))
}

fn home_dir(var: &str, leaf: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(var).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".fluxvm")
            .join(leaf),
    )
}

fn tree_alloc(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    let mut n = m.blocks() * 512;
    if m.is_dir()
        && let Ok(rd) = std::fs::read_dir(p)
    {
        n += rd.flatten().map(|e| tree_alloc(&e.path())).sum::<u64>();
    }
    n
}

/// Free and total bytes of the filesystem holding `p` (its nearest existing parent).
fn disk_of(p: &Path) -> Option<(u64, u64)> {
    let mut p = p;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.to_str()?).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` a zeroed statvfs the call fills in.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let frag = s.f_frsize as u64;
    Some((s.f_bavail as u64 * frag, s.f_blocks as u64 * frag))
}

/// Pure part: the report from instance records (as JSON) and the two folders.
pub(crate) fn report(vms: &[Value], store: Option<&Path>, volumes: Option<&Path>) -> Value {
    let instances: Vec<&Value> = vms
        .iter()
        .filter(|v| v["labels"]["fluxvm.mac"].as_str() == Some("instance"))
        .collect();
    let mut stopped = Vec::new();
    let mut inst_bytes = 0u64;
    for v in &instances {
        let b = v["disk"].as_str().map_or(0, |d| {
            std::fs::metadata(d).map_or(0, |m| m.blocks() * 512)
        });
        inst_bytes += b;
        if matches!(v["status"].as_str(), Some("stopped") | Some("failed")) {
            stopped.push(json!({"name": v["name"], "bytes": b}));
        }
    }
    let mut vols = Vec::new();
    if let Some(root) = volumes
        && let Ok(rd) = std::fs::read_dir(root)
    {
        for e in rd.flatten() {
            let data = e.path().join("data");
            if !e.path().join("volume.json").is_file() {
                continue;
            }
            let users: Vec<&str> = instances
                .iter()
                .filter(|v| {
                    v["request"]["shared_folders"].as_array().is_some_and(|a| {
                        a.iter()
                            .any(|f| f["host_path"].as_str().map(Path::new) == Some(data.as_path()))
                    })
                })
                .filter_map(|v| v["name"].as_str())
                .collect();
            vols.push(json!({
                "name": e.file_name().to_string_lossy(),
                "bytes": tree_alloc(&data),
                "used_by": users,
            }));
        }
    }
    vols.sort_by_key(|v| v["name"].as_str().unwrap_or("").to_string());
    let unused_vol_bytes: u64 = vols
        .iter()
        .filter(|v| v["used_by"].as_array().is_some_and(|a| a.is_empty()))
        .map(|v| v["bytes"].as_u64().unwrap_or(0))
        .sum();
    let stopped_bytes: u64 = stopped.iter().map(|s| s["bytes"].as_u64().unwrap_or(0)).sum();
    let images_bytes = store.map_or(0, tree_alloc);
    let probe = store.or(volumes);
    let disk = probe.and_then(disk_of);
    json!({
        "schema": 1,
        "disk": {"free_bytes": disk.map(|d| d.0), "total_bytes": disk.map(|d| d.1)},
        "images_bytes": images_bytes,
        "instances": {"count": instances.len(), "bytes": inst_bytes, "stopped": stopped},
        "volumes": vols,
        "prune_preview": {
            "would_free_bytes": stopped_bytes,
            "with_volumes_would_free_bytes": stopped_bytes + unused_vol_bytes,
            "note": "stopped instances (and unused volumes with --volumes); `--all` also removes unused images, which this view does not count. Upper bound: APFS clones share blocks",
        },
        "fluxctl": {
            "df": "fluxctl mac system df",
            "prune_preview": "fluxctl mac system prune --all --volumes --dry-run",
            "prune": "fluxctl mac system prune --all",
        },
    })
}

async fn disk(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let vms: Vec<Value> = m
        .list()
        .await
        .iter()
        .filter_map(|v| serde_json::to_value(v).ok())
        .collect();
    let store = home_dir("FLUXVM_MAC_STORE", "mac-images");
    let vols = home_dir("FLUXVM_MAC_VOLUMES", "mac-volumes");
    // Walking big directories blocks, so keep it off the async threads.
    let out = tokio::task::spawn_blocking(move || report(&vms, store.as_deref(), vols.as_deref()))
        .await
        .unwrap_or_else(|_| json!({"schema": 1, "error": "the disk scan was interrupted"}));
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_counts_stopped_instances_and_unused_volumes() {
        let d = tempfile::tempdir().unwrap();
        let vols = d.path().join("vols");
        for n in ["held", "idle"] {
            std::fs::create_dir_all(vols.join(n).join("data")).unwrap();
            std::fs::write(vols.join(n).join("volume.json"), "{}").unwrap();
            std::fs::write(vols.join(n).join("data/f"), vec![1u8; 30_000]).unwrap();
        }
        let disk = d.path().join("old.raw");
        std::fs::write(&disk, vec![2u8; 50_000]).unwrap();
        let held = vols.join("held").join("data");
        let vms = vec![
            json!({"name": "old", "status": "stopped", "disk": disk, "labels": {"fluxvm.mac": "instance"}}),
            json!({"name": "live", "status": "running", "labels": {"fluxvm.mac": "instance"},
                   "request": {"shared_folders": [{"host_path": held}]}}),
            json!({"name": "other", "status": "stopped", "labels": {}}),
        ];
        let r = report(&vms, None, Some(&vols));
        assert_eq!(r["instances"]["count"], 2);
        assert_eq!(r["instances"]["stopped"][0]["name"], "old");
        let v = r["volumes"].as_array().unwrap();
        assert_eq!(v[0]["name"], "held");
        assert_eq!(v[0]["used_by"][0], "live");
        assert!(v[1]["used_by"].as_array().unwrap().is_empty());
        let a = r["prune_preview"]["would_free_bytes"].as_u64().unwrap();
        let b = r["prune_preview"]["with_volumes_would_free_bytes"].as_u64().unwrap();
        assert!(a >= 50_000 && b >= a + 30_000);
        assert!(r["fluxctl"]["prune_preview"].as_str().unwrap().contains("--dry-run"));
    }

    #[test]
    fn a_missing_folder_is_an_empty_report_not_an_error() {
        let r = report(&[], Some(Path::new("/nonexistent/x")), Some(Path::new("/nonexistent/y")));
        assert_eq!(r["volumes"].as_array().unwrap().len(), 0);
        assert_eq!(r["images_bytes"], 0);
    }
}
