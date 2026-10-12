// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Persistent volumes on this host: sparse raw disk files under `<state_dir>/volumes/` that outlive the VMs using them.
//! `fluxvm-agent central` (`/fleet/volumes`) creates them here and attaches one to a `vz` Linux guest at create time as
//! an `apple.extra_disks` image disk on the virtio controller. Deleting the VM leaves the file alone. Merged into the
//! main router by `router()`; every route is admin-only.
//!
//! * `GET    /v1/volumes`
//! * `POST   /v1/volumes` `{"name", "size_gib"}`: a new sparse raw file (201; 409 when the name is taken)
//! * `GET    /v1/volumes/{name}`
//! * `DELETE /v1/volumes/{name}`: refused (409) while any VM record on this host references the file
//! * `POST   /v1/volumes/{name}/resize` `{"size_gib"}`: grow only, and only while no VM references it

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
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

/// Largest volume this host creates or grows to (the same bound as a `vz` extra disk).
pub(crate) const MAX_VOLUME_GIB: u64 = 16 * 1024;
/// Most volume files kept on one host.
const MAX_VOLUMES: usize = 1024;
const GIB: u64 = 1 << 30;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new()
        .route("/v1/volumes", get(list).post(create))
        .route("/v1/volumes/{name}", get(show).delete(remove))
        .route("/v1/volumes/{name}/resize", post(resize))
}

/// What a volume looks like on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct VolumeInfo {
    pub name: String,
    /// The raw file; what a VM's `apple.extra_disks[].path` names to attach it.
    pub path: PathBuf,
    pub format: &'static str,
    pub size_gib: u64,
    pub size_bytes: u64,
    /// Bytes actually written (the file is sparse).
    pub allocated_bytes: u64,
    /// VM ids whose record references the file.
    pub attached_to: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VolumeError {
    Invalid(String),
    NotFound(String),
    Conflict(String),
    Io(String),
}

fn api_err(e: VolumeError) -> ApiError {
    let (status, message) = match e {
        VolumeError::Invalid(m) => (StatusCode::BAD_REQUEST, m),
        VolumeError::NotFound(m) => (StatusCode::NOT_FOUND, m),
        VolumeError::Conflict(m) => (StatusCode::CONFLICT, m),
        VolumeError::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
    };
    ApiError { status, message }
}

/// `[a-z0-9-]`, 1-63 characters, starting with a letter or digit: safe as a file name.
pub(crate) fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && n.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check_name(n: &str) -> Result<(), VolumeError> {
    if valid_name(n) {
        Ok(())
    } else {
        Err(VolumeError::Invalid(format!(
            "bad volume name {n:?}: 1-63 lowercase letters, digits and '-', starting with a letter or digit"
        )))
    }
}

fn check_size(gib: u64) -> Result<(), VolumeError> {
    if (1..=MAX_VOLUME_GIB).contains(&gib) {
        Ok(())
    } else {
        Err(VolumeError::Invalid(format!(
            "size_gib must be 1-{MAX_VOLUME_GIB}"
        )))
    }
}

fn io(what: &str, path: &FsPath, e: std::io::Error) -> VolumeError {
    VolumeError::Io(format!("{what} {}: {e}", path.display()))
}

fn file_of(dir: &FsPath, name: &str) -> PathBuf {
    dir.join(format!("{name}.raw"))
}

/// Every image path a VM record on this host references, with the VM's id: extra disks and overlay files.
pub(crate) fn references(vms: &[VmRecord]) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for vm in vms {
        let Some(apple) = vm.request.apple.as_ref() else {
            continue;
        };
        for d in &apple.extra_disks {
            out.push((vm.id.to_string(), d.path.clone()));
            if let Some(o) = &d.overlay {
                out.push((vm.id.to_string(), o.clone()));
            }
        }
    }
    out
}

/// The VMs among `refs` that use `path` (compared as given and canonically, so a symlinked state dir still matches).
fn users(refs: &[(String, PathBuf)], path: &FsPath) -> Vec<String> {
    let canon = std::fs::canonicalize(path).ok();
    let mut ids: Vec<String> = refs
        .iter()
        .filter(|(_, p)| {
            p == path
                || canon
                    .as_ref()
                    .is_some_and(|c| std::fs::canonicalize(p).is_ok_and(|q| &q == c))
        })
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// One volume, read from disk. Only a regular file counts: a symlink planted in the directory is refused.
pub(crate) fn info(
    dir: &FsPath,
    name: &str,
    refs: &[(String, PathBuf)],
) -> Result<VolumeInfo, VolumeError> {
    use std::os::unix::fs::MetadataExt;
    check_name(name)?;
    let path = file_of(dir, name);
    let md = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(VolumeError::NotFound(format!("no volume {name:?}")));
        }
        Err(e) => return Err(io("reading", &path, e)),
    };
    if !md.file_type().is_file() {
        return Err(VolumeError::Conflict(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(VolumeInfo {
        name: name.to_owned(),
        attached_to: users(refs, &path),
        path,
        format: "raw",
        size_gib: md.len() / GIB,
        size_bytes: md.len(),
        allocated_bytes: md.blocks() * 512,
    })
}

/// Every volume in `dir`, by name.
pub(crate) fn list_dir(
    dir: &FsPath,
    refs: &[(String, PathBuf)],
) -> Result<Vec<VolumeInfo>, VolumeError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io("listing", dir, e)),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(name) = file.to_str().and_then(|f| f.strip_suffix(".raw")) else {
            continue;
        };
        if valid_name(name)
            && let Ok(v) = info(dir, name, refs)
        {
            out.push(v);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Creates volume `name` as a sparse raw file of `gib` GiB. Never overwrites an existing file.
pub(crate) fn create_in(dir: &FsPath, name: &str, gib: u64) -> Result<VolumeInfo, VolumeError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    check_name(name)?;
    check_size(gib)?;
    std::fs::create_dir_all(dir).map_err(|e| io("creating", dir, e))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    if list_dir(dir, &[])?.len() >= MAX_VOLUMES {
        return Err(VolumeError::Conflict(format!(
            "this host already keeps {MAX_VOLUMES} volumes"
        )));
    }
    let path = file_of(dir, name);
    let f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(VolumeError::Conflict(format!(
                "volume {name:?} already exists"
            )));
        }
        Err(e) => return Err(io("creating", &path, e)),
    };
    if let Err(e) = f.set_len(gib * GIB) {
        let _ = std::fs::remove_file(&path);
        return Err(io("sizing", &path, e));
    }
    info(dir, name, &[])
}

/// Deletes volume `name` unless a VM references it.
pub(crate) fn remove_in(
    dir: &FsPath,
    name: &str,
    refs: &[(String, PathBuf)],
) -> Result<(), VolumeError> {
    let v = info(dir, name, refs)?;
    if !v.attached_to.is_empty() {
        return Err(VolumeError::Conflict(format!(
            "volume {name:?} is attached to VM {}; delete the VM first",
            v.attached_to.join(", ")
        )));
    }
    std::fs::remove_file(&v.path).map_err(|e| io("deleting", &v.path, e))
}

/// Grows volume `name` to `gib` GiB. Shrinking would cut off the guest's file system, so it is refused, as is any
/// resize while a VM references the file.
pub(crate) fn grow_in(
    dir: &FsPath,
    name: &str,
    gib: u64,
    refs: &[(String, PathBuf)],
) -> Result<VolumeInfo, VolumeError> {
    check_size(gib)?;
    let v = info(dir, name, refs)?;
    if !v.attached_to.is_empty() {
        return Err(VolumeError::Conflict(format!(
            "volume {name:?} is attached to VM {}; resize it while detached",
            v.attached_to.join(", ")
        )));
    }
    if gib * GIB < v.size_bytes {
        return Err(VolumeError::Invalid(format!(
            "volume {name:?} is {} GiB; volumes only grow",
            v.size_gib
        )));
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&v.path)
        .map_err(|e| io("opening", &v.path, e))?;
    f.set_len(gib * GIB)
        .map_err(|e| io("growing", &v.path, e))?;
    info(dir, name, refs)
}

fn volume_dir(m: &VmManager) -> PathBuf {
    m.cfg.state_dir.join("volumes")
}

async fn refs_of(m: &VmManager) -> Vec<(String, PathBuf)> {
    references(&m.list().await)
}

async fn list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let refs = refs_of(&m).await;
    let dir = volume_dir(&m);
    let items = list_dir(&dir, &refs).map_err(api_err)?;
    Ok(Json(json!({"dir": dir, "items": items})))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    size_gib: u64,
}

async fn create(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Json(b): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<VolumeInfo>)> {
    require_admin(role)?;
    let v = create_in(&volume_dir(&m), &b.name, b.size_gib).map_err(api_err)?;
    tracing::info!(volume = %v.name, size_gib = v.size_gib, "volume created");
    Ok((StatusCode::CREATED, Json(v)))
}

async fn show(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<Json<VolumeInfo>> {
    require_admin(role)?;
    let refs = refs_of(&m).await;
    Ok(Json(info(&volume_dir(&m), &name, &refs).map_err(api_err)?))
}

async fn remove(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    let refs = refs_of(&m).await;
    remove_in(&volume_dir(&m), &name, &refs).map_err(api_err)?;
    tracing::info!(volume = %name, "volume deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ResizeBody {
    size_gib: u64,
}

async fn resize(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    Json(b): Json<ResizeBody>,
) -> ApiResult<Json<VolumeInfo>> {
    require_admin(role)?;
    let refs = refs_of(&m).await;
    Ok(Json(
        grow_in(&volume_dir(&m), &name, b.size_gib, &refs).map_err(api_err)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_file_safe() {
        let (long, too_long) = ("x".repeat(63), "x".repeat(64));
        for ok in ["vol-1a2b3c4d5e6f", "data", "a", long.as_str()] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-a", "A", "a/b", "..", "a.raw", too_long.as_str()] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn create_is_sparse_exclusive_and_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("volumes");
        let v = create_in(&dir, "vol-a", 4).unwrap();
        assert_eq!((v.size_gib, v.size_bytes), (4, 4 * GIB));
        assert!(v.allocated_bytes < 1024 * 1024, "sparse: {v:?}");
        assert_eq!(v.path, dir.join("vol-a.raw"));
        let mode = std::fs::metadata(&v.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(matches!(
            create_in(&dir, "vol-a", 1),
            Err(VolumeError::Conflict(_))
        ));
        assert!(matches!(
            create_in(&dir, "vol-b", 0),
            Err(VolumeError::Invalid(_))
        ));
        assert!(matches!(
            create_in(&dir, "vol-b", MAX_VOLUME_GIB + 1),
            Err(VolumeError::Invalid(_))
        ));
        assert!(matches!(
            create_in(&dir, "../x", 1),
            Err(VolumeError::Invalid(_))
        ));
        let all = list_dir(&dir, &[]).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "vol-a");
    }

    #[test]
    fn attached_volumes_are_neither_deleted_nor_resized() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("volumes");
        let v = create_in(&dir, "vol-a", 2).unwrap();
        let refs = vec![("vm-1".to_owned(), v.path.clone())];
        assert_eq!(info(&dir, "vol-a", &refs).unwrap().attached_to, ["vm-1"]);
        assert!(matches!(
            remove_in(&dir, "vol-a", &refs),
            Err(VolumeError::Conflict(_))
        ));
        assert!(matches!(
            grow_in(&dir, "vol-a", 3, &refs),
            Err(VolumeError::Conflict(_))
        ));
        // Detached: grows, never shrinks, then goes.
        assert_eq!(grow_in(&dir, "vol-a", 3, &[]).unwrap().size_gib, 3);
        assert!(matches!(
            grow_in(&dir, "vol-a", 1, &[]),
            Err(VolumeError::Invalid(_))
        ));
        remove_in(&dir, "vol-a", &[]).unwrap();
        assert!(matches!(
            info(&dir, "vol-a", &[]),
            Err(VolumeError::NotFound(_))
        ));
        assert!(list_dir(&dir, &[]).unwrap().is_empty());
    }

    #[test]
    fn a_planted_symlink_is_not_a_volume() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("volumes");
        std::fs::create_dir_all(&dir).unwrap();
        let target = d.path().join("secret");
        std::fs::write(&target, b"x").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("evil.raw")).unwrap();
        assert!(matches!(
            remove_in(&dir, "evil", &[]),
            Err(VolumeError::Conflict(_))
        ));
        assert!(target.exists());
        assert!(list_dir(&dir, &[]).unwrap().is_empty());
    }
}
