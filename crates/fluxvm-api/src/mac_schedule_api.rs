// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The `fluxctl mac schedule` list for the Studio Mac page (admin):
//!
//! * `GET    /v1/mac/schedules`: the schedules with next run and last result. A webhook URL is only ever shown as its host.
//! * `POST   /v1/mac/schedules`: add one (`name`, `cron`, `image`, optional `cmd`, `ttl`, `notify`, `webhook`, `enabled`).
//! * `DELETE /v1/mac/schedules/{name}`: remove it.
//! * `POST   /v1/mac/schedules/{name}/{enable|disable|run-now}`: `run-now` asks the next `fluxctl mac schedule tick` to run it once.
//!
//! It edits the same `mac-schedules.json` the CLI does (`fluxvm_core::mac_schedule`), in the account the daemon runs as; the answer
//! says where. The API starts no VM itself: schedules fire while `fluxctl mac schedule tick --loop` runs (the LaunchAgent that
//! `fluxctl mac schedule install` writes), and the answer says so when it does not look installed. Not run against a real Mac.

use crate::{ApiError, ApiResult, require_admin};
use axum::{
    Extension, Json, Router,
    extract::Path,
    http::StatusCode,
    routing::{delete, get, post},
};
use fluxvm_core::config::Role;
use fluxvm_core::mac_schedule as ms;
use fluxvm_scheduler::VmManager;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new()
        .route("/v1/mac/schedules", get(list).post(add))
        .route("/v1/mac/schedules/{name}", delete(remove))
        .route("/v1/mac/schedules/{name}/{action}", post(act))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn file() -> ApiResult<std::path::PathBuf> {
    ms::default_path().ok_or_else(|| ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "The daemon has no home directory, so it cannot find the schedule file. Set FLUXVM_MAC_SCHEDULES for the daemon, or use `fluxctl mac schedule ls`.".into(),
    })
}

fn bad(m: String) -> ApiError {
    ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("{m} (see: fluxctl explain MAC_SCHEDULE_INVALID)"),
    }
}

fn not_found(m: String) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        message: format!("{m} (see: fluxctl explain MAC_SCHEDULE_MISSING)"),
    }
}

fn server_error(m: String) -> ApiError {
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: m,
    }
}

fn answer(path: &std::path::Path) -> ApiResult<Value> {
    let f = ms::load(path).map_err(server_error)?;
    let n = now();
    Ok(json!({
        "path": path,
        "schedules": f.schedules.iter().map(|s| s.view(n)).collect::<Vec<_>>(),
        "note": "Schedules fire while `fluxctl mac schedule tick --loop` runs. `fluxctl mac schedule install` starts it at login.",
        "commands": {"add": "fluxctl mac schedule add NAME \"0 3 * * *\" IMAGE -- CMD", "list": "fluxctl mac schedule ls", "install": "fluxctl mac schedule install"},
    }))
}

async fn list(Extension(role): Extension<Role>) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    Ok(Json(answer(&file()?)?))
}

#[derive(serde::Deserialize)]
struct NewSchedule {
    name: String,
    cron: String,
    image: String,
    #[serde(default)]
    cmd: Vec<String>,
    ttl: Option<String>,
    #[serde(default)]
    notify: bool,
    webhook: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

async fn add(
    Extension(role): Extension<Role>,
    Json(b): Json<NewSchedule>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let p = file()?;
    let s = ms::Schedule {
        name: b.name,
        cron: b.cron,
        image: b.image,
        cmd: b.cmd,
        ttl: b.ttl.filter(|t| !t.trim().is_empty()),
        notify: b.notify,
        webhook: b.webhook.filter(|w| !w.trim().is_empty()),
        enabled: b.enabled,
        created_unix: 0,
        last_run_unix: None,
        last_status: None,
        last_detail: None,
        run_now: false,
    };
    ms::add(&p, s, now()).map_err(bad)?;
    Ok((StatusCode::CREATED, Json(answer(&p)?)))
}

async fn remove(
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let p = file()?;
    ms::remove(&p, &name).map_err(not_found)?;
    Ok(Json(answer(&p)?))
}

async fn act(
    Extension(role): Extension<Role>,
    Path((name, action)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let p = file()?;
    let change: fn(&mut ms::Schedule) = match action.as_str() {
        "enable" => |s| s.enabled = true,
        "disable" => |s| s.enabled = false,
        "run-now" => |s| s.run_now = true,
        other => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "{other:?} is not a schedule action: use enable, disable or run-now. From a terminal: `fluxctl mac schedule run-now {name}`"
                ),
            });
        }
    };
    ms::update(&p, &name, change).map_err(not_found)?;
    Ok(Json(answer(&p)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test: the schedule file path comes from a process-wide variable.
    #[tokio::test]
    async fn add_list_act_remove_and_never_leak_the_webhook() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        // SAFETY: only this test in this crate sets it.
        unsafe { std::env::set_var("FLUXVM_MAC_SCHEDULES", &p) };
        let admin = || Extension(Role::Admin);
        let new = |name: &str, cron: &str| NewSchedule {
            name: name.into(),
            cron: cron.into(),
            image: "macos-base:15".into(),
            cmd: vec!["true".into()],
            ttl: Some("1h".into()),
            notify: true,
            webhook: Some("https://hooks.example.com/T0K3N".into()),
            enabled: true,
        };
        let (st, Json(v)) = add(admin(), Json(new("nightly", "0 3 * * *")))
            .await
            .unwrap();
        assert_eq!(st, StatusCode::CREATED);
        assert_eq!(v["schedules"][0]["name"], "nightly");
        assert!(!v.to_string().contains("T0K3N"));
        assert!(v["schedules"][0]["next_run_unix"].is_number());
        // A bad cron is a 400 that names the fix; a duplicate too.
        let e = add(admin(), Json(new("x", "nope"))).await.unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        assert!(e.message.contains("MAC_SCHEDULE_INVALID"));
        assert!(
            add(admin(), Json(new("nightly", "0 3 * * *")))
                .await
                .is_err()
        );
        let Json(v) = act(admin(), Path(("nightly".into(), "run-now".into())))
            .await
            .unwrap();
        assert_eq!(v["schedules"][0]["run_now"], true);
        let Json(v) = act(admin(), Path(("nightly".into(), "disable".into())))
            .await
            .unwrap();
        assert_eq!(v["schedules"][0]["enabled"], false);
        let e = act(admin(), Path(("nightly".into(), "explode".into())))
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_REQUEST);
        let e = act(admin(), Path(("nope".into(), "enable".into())))
            .await
            .unwrap_err();
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        // Not an admin: refused before anything is read.
        assert!(list(Extension(Role::ReadOnly)).await.is_err());
        let Json(v) = remove(admin(), Path("nightly".into())).await.unwrap();
        assert!(v["schedules"].as_array().unwrap().is_empty());
        assert!(remove(admin(), Path("nightly".into())).await.is_err());
    }
}
