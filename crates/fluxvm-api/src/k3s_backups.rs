// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The Studio Backups card: backups of the FluxVM k3s clusters on this host.
//!
//! * `GET  /v1/k3s/backups` lists every cluster with its backups (age, size, kind), its schedule and the free disk,
//! * `POST /v1/k3s/{cluster}/backups` takes a backup now,
//! * `POST /v1/k3s/{cluster}/backups/{name}/verify` checks that a backup is whole and restorable,
//! * `POST /v1/k3s/{cluster}/backups/{name}/restore` with `{"confirm": "<name>"}` restores it; without the matching
//!   confirmation it answers 428 and changes nothing (the Studio asks twice before sending it),
//! * `POST /v1/k3s/{cluster}/backups/schedule` turns the automatic backups on or off and sets how often and how many.
//!
//! All of them are admin only. Listing, verifying and the schedule only read and write files
//! (`fluxvm_core::k3s_backups`, the same rules as `fluxctl kube backups`). Taking a backup and restoring need ssh into the
//! cluster's guests, which `fluxctl` does, so those run `fluxctl kube backup --json` and `fluxctl kube restore --json`
//! (found next to the daemon, else on PATH). The daemon also runs `fluxctl kube backup run-due` shortly after it starts
//! and every hour, which is what makes the default schedule happen.

use crate::kube::err;
use crate::{ApiResult, Role, require_admin};
use axum::{Extension, Json, extract::Path, http::StatusCode};
use fluxvm_core::k3s_backups::{self as kb, BackupEntry, Schedule, VerifyReport};
use fluxvm_scheduler::VmManager;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/k3s/backups", get(list))
        .route("/v1/k3s/{cluster}/backups", post(backup_now))
        .route("/v1/k3s/{cluster}/backups/schedule", post(set_schedule))
        .route("/v1/k3s/{cluster}/backups/{name}/verify", post(verify))
        .route("/v1/k3s/{cluster}/backups/{name}/restore", post(restore))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn root() -> ApiResult<PathBuf> {
    kb::store_root().ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the daemon has no home folder, so it cannot find the clusters' backups",
        )
    })
}

fn check(cluster: &str, name: Option<&str>) -> ApiResult<()> {
    if !kb::valid_name(cluster) || name.is_some_and(|n| !kb::valid_name(n)) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "that is not a valid cluster or backup name",
        ));
    }
    Ok(())
}

#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct ClusterBackups {
    name: String,
    schedule: Option<Schedule>,
    backups: Vec<BackupEntry>,
    total_bytes: u64,
    newest_age_secs: Option<u64>,
    disk_free_bytes: Option<u64>,
    /// What to type in a terminal for each action (the Studio's Copy-as-fluxctl).
    commands: Commands,
}

#[derive(Serialize, Debug, PartialEq)]
struct Commands {
    backup: String,
    list: String,
    verify: String,
    restore_latest: String,
}

#[derive(Serialize, Debug, PartialEq)]
pub(crate) struct Listing {
    schema: u32,
    clusters: Vec<ClusterBackups>,
}

/// Clusters with a journal or a backups folder under `store`, each with what is in it. Pure file reads.
pub(crate) fn collect(store: &std::path::Path, now: u64) -> Listing {
    let mut names = kb::clusters(store);
    if let Ok(rd) = std::fs::read_dir(store) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "json")
                && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                && kb::valid_name(stem)
                && !names.iter().any(|n| n == stem)
            {
                names.push(stem.to_owned());
            }
        }
    }
    names.sort();
    let clusters = names
        .into_iter()
        .map(|name| {
            let dir = kb::backups_dir(store, &name);
            let backups = kb::list(&dir, &name, now);
            let flag = if name == fluxvm_core::up::CLUSTER_NAME {
                String::new()
            } else {
                format!(" --cluster {name}")
            };
            ClusterBackups {
                schedule: kb::load_schedule(&dir),
                total_bytes: backups.iter().map(|b| b.bytes).sum(),
                newest_age_secs: backups.first().map(|b| b.age_secs),
                disk_free_bytes: kb::free_bytes(&dir),
                commands: Commands {
                    backup: format!("fluxctl kube backup{flag}"),
                    list: format!("fluxctl kube backups{flag}"),
                    verify: format!("fluxctl kube backup verify{flag}"),
                    restore_latest: format!(
                        "fluxctl kube restore --latest{} --yes",
                        flag.replace("--cluster", "--k3s-cluster")
                    ),
                },
                backups,
                name,
            }
        })
        .collect();
    Listing {
        schema: kb::SCHEMA,
        clusters,
    }
}

async fn list(Extension(role): Extension<Role>) -> ApiResult<Json<Listing>> {
    require_admin(role)?;
    let root = root()?;
    let l = tokio::task::spawn_blocking(move || collect(&root, now()))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(l))
}

/// `fluxctl`, next to the running daemon or else on PATH.
fn fluxctl() -> Option<PathBuf> {
    if let Some(p) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("fluxctl")))
        .filter(|p| p.is_file())
    {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join("fluxctl"))
            .find(|p| p.is_file())
    })
}

/// Runs `fluxctl ARGS` and returns its stdout, or the failure in words.
fn run_fluxctl(args: Vec<String>, limit: Duration) -> Result<String, (StatusCode, String)> {
    use std::io::Read;
    let Some(bin) = fluxctl() else {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            "fluxctl was not found next to the daemon or on its PATH, so it cannot do this; run the same command in a terminal (Copy as fluxctl)".into(),
        ));
    };
    let mut child = std::process::Command::new(bin)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not start fluxctl: {e}"),
            )
        })?;
    // Both pipes are drained on threads so a chatty command cannot block on a full pipe.
    let mut out = child.stdout.take().expect("piped");
    let mut errp = child.stderr.take().expect("piped");
    let ot = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let et = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = errp.read_to_string(&mut s);
        s
    });
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err((StatusCode::GATEWAY_TIMEOUT, "it took too long and was stopped; the cluster may be busy (check `fluxctl kube cluster status`)".into()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
        }
    };
    let stdout = ot.join().unwrap_or_default();
    let stderr = et.join().unwrap_or_default();
    if status.success() {
        return Ok(stdout);
    }
    let msg = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|_| stderr.trim().to_owned())
        .unwrap_or_else(|| "fluxctl failed without a message".into());
    // A failure with an explain code is the backup's fault (422), anything else ours (500).
    let code =
        if msg.contains("fluxctl explain BACKUP_") || msg.contains("fluxctl explain RESTORE_") {
            StatusCode::UNPROCESSABLE_ENTITY
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
    Err((code, msg))
}

async fn run_blocking(args: Vec<String>, limit: Duration) -> ApiResult<Json<Value>> {
    let r = tokio::task::spawn_blocking(move || run_fluxctl(args, limit))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match r {
        Ok(out) => Ok(Json(
            serde_json::from_str(&out).unwrap_or_else(|_| json!({"output": out.trim()})),
        )),
        Err((s, m)) => Err(err(s, m)),
    }
}

fn cluster_args(cluster: &str, flag: &str) -> Vec<String> {
    if cluster == fluxvm_core::up::CLUSTER_NAME {
        vec![]
    } else {
        vec![flag.into(), cluster.into()]
    }
}

async fn backup_now(
    Extension(role): Extension<Role>,
    Path(cluster): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    check(&cluster, None)?;
    let mut a: Vec<String> = vec!["kube".into(), "backup".into(), "--json".into()];
    a.extend(cluster_args(&cluster, "--cluster"));
    run_blocking(a, Duration::from_secs(20 * 60)).await
}

async fn verify(
    Extension(role): Extension<Role>,
    Path((cluster, name)): Path<(String, String)>,
) -> ApiResult<Json<VerifyReport>> {
    require_admin(role)?;
    check(&cluster, Some(&name))?;
    let file = kb::backups_dir(&root()?, &cluster).join(&name);
    if !file.is_file() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!(
                "there is no backup {name} for cluster {cluster} (more: fluxctl explain BACKUP_NOT_FOUND)"
            ),
        ));
    }
    let r = tokio::task::spawn_blocking(move || kb::verify_file(&file, None))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(r))
}

#[derive(Deserialize)]
struct RestoreBody {
    /// The backup's name, typed again.
    #[serde(default)]
    confirm: String,
}

async fn restore(
    Extension(role): Extension<Role>,
    Path((cluster, name)): Path<(String, String)>,
    body: Option<Json<RestoreBody>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    check(&cluster, Some(&name))?;
    let confirm = body.map(|Json(b)| b.confirm).unwrap_or_default();
    if confirm != name {
        return Err(err(
            StatusCode::PRECONDITION_REQUIRED,
            format!(
                "restoring replaces everything in cluster {cluster}; send {{\"confirm\": \"{name}\"}} to do it (more: fluxctl explain RESTORE_NEEDS_CONFIRM)"
            ),
        ));
    }
    if !kb::backups_dir(&root()?, &cluster).join(&name).is_file() {
        return Err(err(
            StatusCode::NOT_FOUND,
            format!(
                "there is no backup {name} for cluster {cluster} (more: fluxctl explain BACKUP_NOT_FOUND)"
            ),
        ));
    }
    let mut a: Vec<String> = vec![
        "kube".into(),
        "restore".into(),
        name,
        "--yes".into(),
        "--json".into(),
    ];
    a.extend(cluster_args(&cluster, "--k3s-cluster"));
    run_blocking(a, Duration::from_secs(30 * 60)).await
}

#[derive(Deserialize)]
struct ScheduleBody {
    enabled: Option<bool>,
    every_hours: Option<u32>,
    keep: Option<u32>,
    keep_pre_upgrade: Option<u32>,
}

/// Applies a change to a policy (an absent one starts from the default, switched off until the change says otherwise).
fn apply_schedule(cur: Option<Schedule>, b: &ScheduleBody) -> Result<Schedule, String> {
    let mut s = cur.unwrap_or(Schedule {
        enabled: false,
        ..Schedule::default()
    });
    s.enabled = b
        .enabled
        .unwrap_or(s.enabled || b.every_hours.is_some() || b.keep.is_some());
    s.every_hours = b.every_hours.unwrap_or(s.every_hours);
    s.keep = b.keep.unwrap_or(s.keep);
    s.keep_pre_upgrade = b.keep_pre_upgrade.unwrap_or(s.keep_pre_upgrade);
    s.valid()?;
    Ok(s)
}

async fn set_schedule(
    Extension(role): Extension<Role>,
    Path(cluster): Path<String>,
    Json(b): Json<ScheduleBody>,
) -> ApiResult<Json<Schedule>> {
    require_admin(role)?;
    check(&cluster, None)?;
    let dir = kb::backups_dir(&root()?, &cluster);
    let s =
        apply_schedule(kb::load_schedule(&dir), &b).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    kb::save_schedule(&dir, &s).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not save the schedule: {e}"),
        )
    })?;
    Ok(Json(s))
}

/// Starts the hourly check that makes scheduled backups happen (first one five minutes after start). Does nothing when
/// there are no clusters with a schedule, and never fails the daemon.
pub(crate) fn start_scheduler() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            let _ = tokio::task::spawn_blocking(|| {
                let has = kb::store_root().is_some_and(|r| !kb::clusters(&r).is_empty());
                if !has {
                    return;
                }
                let args = vec![
                    "kube".into(),
                    "backup".into(),
                    "run-due".into(),
                    "--json".into(),
                ];
                match run_fluxctl(args, Duration::from_secs(60 * 60)) {
                    Ok(out) if out.contains("\"backed up\"") => {
                        tracing::info!("scheduled k3s backup taken")
                    }
                    Ok(_) => {}
                    Err((_, m)) => tracing::warn!(error = %m, "scheduled k3s backup"),
                }
            })
            .await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routes_are_registered_under_v1() {
        let _ = routes();
    }

    #[test]
    fn listing_covers_clusters_with_a_journal_or_backups() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("default.json"), "{}").unwrap();
        let dir = kb::backups_dir(d.path(), "lab");
        std::fs::create_dir_all(&dir).unwrap();
        kb::save_schedule(&dir, &Schedule::default()).unwrap();
        std::fs::write(dir.join("lab-auto-20260101-000000.tar.gz"), vec![0u8; 100]).unwrap();
        let l = collect(d.path(), now());
        let names: Vec<&str> = l.clusters.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["default", "lab"]);
        assert_eq!(l.clusters[0].commands.backup, "fluxctl kube backup");
        assert_eq!(
            l.clusters[1].commands.backup,
            "fluxctl kube backup --cluster lab"
        );
        assert!(
            l.clusters[1]
                .commands
                .restore_latest
                .contains("--k3s-cluster lab")
        );
        assert_eq!(l.clusters[1].backups.len(), 1);
        assert!(l.clusters[1].schedule.as_ref().unwrap().enabled);
        assert!(l.clusters[0].schedule.is_none() && l.clusters[0].backups.is_empty());
    }

    #[test]
    fn names_that_could_escape_the_folder_are_refused() {
        assert!(check("default", Some("default-20260101-000000.tar.gz")).is_ok());
        for bad in ["..", "a/b", ".hidden", ""] {
            assert!(check("default", Some(bad)).is_err(), "{bad}");
            assert!(check(bad, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn schedule_changes_validate_and_turn_on_when_tuned() {
        let b = |e, h, k| ScheduleBody {
            enabled: e,
            every_hours: h,
            keep: k,
            keep_pre_upgrade: None,
        };
        let s = apply_schedule(None, &b(None, Some(6), None)).unwrap();
        assert!(s.enabled && s.every_hours == 6);
        let s = apply_schedule(Some(s), &b(Some(false), None, None)).unwrap();
        assert!(!s.enabled && s.every_hours == 6);
        assert!(apply_schedule(None, &b(Some(true), Some(0), None)).is_err());
        assert!(apply_schedule(None, &b(None, None, Some(0))).is_err());
    }

    #[tokio::test]
    async fn restore_without_the_matching_confirmation_changes_nothing() {
        let e = restore(
            Extension(Role::Admin),
            Path(("default".into(), "default-20260101-000000.tar.gz".into())),
            Some(Json(RestoreBody {
                confirm: "other".into(),
            })),
        )
        .await
        .unwrap_err();
        assert_eq!(e.status, StatusCode::PRECONDITION_REQUIRED);
        assert!(e.message.contains("RESTORE_NEEDS_CONFIRM"));
        let e = restore(
            Extension(Role::ReadOnly),
            Path(("default".into(), "x.tar.gz".into())),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(e.status, StatusCode::FORBIDDEN);
    }
}
