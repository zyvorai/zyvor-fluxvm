// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/alerts`: what needs attention now. Built from data that already exists (`fluxvm_scheduler::alerts`): warn and
//! error lines of the merged event feed, backups and schedule runs that failed, Mac instances that are unhealthy, and
//! the problems of the cluster health model. `?since=` (default `24h`) sets how far back events count and
//! `?include_snoozed=1` lists snoozed ones too (flagged with `snoozed_until`).
//!
//! `POST /v1/alerts/snooze` (`{"key", "hours"?}`, default 24 hours) and `POST /v1/alerts/unsnooze` (`{"key"}`) store the
//! snooze on the server, so every browser and the CLI agree. Both need an admin token; a read-only token can read the list.
//! The list covers the whole host, so a tenant-scoped token is refused in words. The audit log is never read here.

use crate::{ApiResult, Role, TokenTenant, require_admin};
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::Utc;
use fluxvm_core::health::{ClusterHealth, Severity};
use fluxvm_core::model::{VmRecord, VmStatus};
use fluxvm_scheduler::VmManager;
use fluxvm_scheduler::alerts::{self, Alert, Report, SnoozeError};
use fluxvm_scheduler::event_feed::Inputs;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

pub(crate) fn routes() -> axum::Router<Arc<VmManager>> {
    axum::Router::new()
        .route("/v1/alerts", axum::routing::get(list))
        .route("/v1/alerts/snooze", axum::routing::post(snooze))
        .route("/v1/alerts/unsnooze", axum::routing::post(unsnooze))
}

#[derive(Deserialize, Default)]
struct ListQuery {
    since: Option<String>,
    #[serde(default)]
    include_snoozed: Option<String>,
}

#[derive(Deserialize)]
struct SnoozeBody {
    key: String,
    #[serde(default)]
    hours: Option<u32>,
}

fn host_wide(tenant: &Option<Extension<TokenTenant>>) -> ApiResult<()> {
    if tenant.is_some() {
        return Err(crate::kube::err(
            StatusCode::FORBIDDEN,
            "Alerts cover the whole host, so a tenant token cannot read them. Use an admin or read-only token, or look at your own machines with `fluxctl events --source vm`.",
        ));
    }
    Ok(())
}

/// `machine/code/3` -> `machine/code`: the trailing number only says where it sat in this run.
fn stable_id(id: &str) -> &str {
    match id.rsplit_once('/') {
        Some((head, tail)) if head.contains('/') && tail.chars().all(|c| c.is_ascii_digit()) => {
            head
        }
        _ => id,
    }
}

/// A message as part of a key: letters only, so a changing number does not change the key and no shell quoting is needed.
fn slug(message: &str) -> String {
    let words: Vec<String> = message
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    words.join("-").chars().take(80).collect()
}

/// Running instances whose healthcheck label says so, as `(name, health)`. A stopped one keeps its last label but no
/// healthcheck is failing any more.
fn mac_health(vms: &[VmRecord]) -> Vec<(String, String)> {
    vms.iter()
        .filter(|v| v.status == VmStatus::Running)
        .filter_map(|v| Some((v.name.clone(), v.labels.get("fluxvm.mac.health")?.clone())))
        .collect()
}

/// Findings of the health model that need a person, as alerts. A fix with a command or steps becomes the next step.
fn health_alerts(h: &ClusterHealth, now: chrono::DateTime<Utc>) -> Vec<Alert> {
    let found: Vec<_> = h
        .all_items()
        .into_iter()
        .filter(|i| i.severity != Severity::Ok)
        .collect();
    // Some codes legitimately repeat on one machine (two security settings): each keeps a key of its own.
    let mut per_id: HashMap<&str, usize> = HashMap::new();
    for i in &found {
        *per_id.entry(stable_id(&i.id)).or_default() += 1;
    }
    found
        .iter()
        .map(|i| {
            let step = i
                .fix
                .as_ref()
                .and_then(|f| {
                    f.command
                        .as_ref()
                        .map(|c| format!("Run `{c}`."))
                        .or_else(|| f.steps.clone())
                })
                .unwrap_or_else(|| "Run `fluxctl cluster status` to see what to do.".into());
            let id = stable_id(&i.id);
            let key = if per_id[id] > 1 {
                format!("health:{id}:{}", slug(&i.message))
            } else {
                format!("health:{id}")
            };
            Alert {
                key,
                level: if i.severity == Severity::Fail {
                    "error"
                } else {
                    "warn"
                }
                .into(),
                source: "health".into(),
                summary: i.message.clone(),
                next_step: step,
                explain: i.explain.clone(),
                target: Some(i.machine.clone()),
                last_seen: now,
                first_seen: now,
                count: 1,
                snoozed_until: None,
            }
        })
        .collect()
}

async fn list(
    State(m): State<Arc<VmManager>>,
    tenant: Option<Extension<TokenTenant>>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Report>> {
    host_wide(&tenant)?;
    let since = q
        .since
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| alerts::DEFAULT_SINCE.to_string());
    let now = Utc::now();
    alerts::check_since(&since, now).map_err(|e| crate::kube::err(StatusCode::BAD_REQUEST, e))?;
    let mut extra = alerts::unhealthy_macs(&mac_health(&m.list().await), now);
    extra.extend(health_alerts(
        &crate::cluster_health::assess_now(&m).await,
        now,
    ));
    let dir = m.cfg.state_dir.clone();
    let since2 = since.clone();
    let (all, notes) = tokio::task::spawn_blocking(move || {
        let none = |_: Uuid| None;
        let inp = Inputs {
            state_dir: &dir,
            container: &none,
            schedules: fluxvm_core::mac_schedule::default_path(),
            backups: fluxvm_core::mac_backup::default_root(),
        };
        alerts::gather(&inp, &since2, extra, now).map(|all| (all, alerts::notes(&inp)))
    })
    .await
    .map_err(|e| anyhow::anyhow!("alerts read task failed: {e}"))??;
    let include = q
        .include_snoozed
        .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes"));
    Ok(Json(Report::new(&since, all, include, notes)))
}

fn not_snoozed(e: SnoozeError) -> crate::ApiError {
    let status = match e {
        SnoozeError::Request(_) => StatusCode::BAD_REQUEST,
        SnoozeError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    crate::kube::err(status, e.to_string())
}

async fn snooze(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    tenant: Option<Extension<TokenTenant>>,
    Json(b): Json<SnoozeBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    host_wide(&tenant)?;
    let dir = m.cfg.state_dir.clone();
    let hours = b.hours.unwrap_or(alerts::DEFAULT_SNOOZE_HOURS);
    let key = b.key;
    let until = tokio::task::spawn_blocking(move || alerts::snooze(&dir, &key, hours, Utc::now()))
        .await
        .map_err(|e| anyhow::anyhow!("snooze task failed: {e}"))?
        .map_err(not_snoozed)?;
    Ok(Json(json!({"snoozed_until": until})))
}

async fn unsnooze(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    tenant: Option<Extension<TokenTenant>>,
    Json(b): Json<SnoozeBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    host_wide(&tenant)?;
    let dir = m.cfg.state_dir.clone();
    let key = b.key;
    let was = tokio::task::spawn_blocking(move || alerts::unsnooze(&dir, &key, Utc::now()))
        .await
        .map_err(|e| anyhow::anyhow!("snooze task failed: {e}"))?
        .map_err(not_snoozed)?;
    Ok(Json(json!({"was_snoozed": was})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::health::{self, Item};

    #[test]
    fn the_routes_are_registered_once_each() {
        let _ = routes();
    }

    #[test]
    fn two_findings_with_one_code_keep_their_own_keys() {
        let item = |n: u32, message: &str| Item {
            id: format!("mini-01/security_setting/{n}"),
            machine: "mini-01".into(),
            source: "host".into(),
            severity: Severity::Warn,
            message: message.into(),
            cause: None,
            fix: None,
            explain: Some("SECURITY_SETTING".into()),
        };
        let mut h = health::assess(&[], 0);
        h.items = vec![
            item(1, "FileVault is on"),
            item(2, "The screen locks after 5 minutes"),
        ];
        let keys: Vec<_> = health_alerts(&h, Utc::now())
            .into_iter()
            .map(|a| a.key)
            .collect();
        assert_eq!(
            keys,
            [
                "health:mini-01/security_setting:filevault-is-on",
                "health:mini-01/security_setting:the-screen-locks-after-minutes"
            ]
        );
    }

    #[test]
    fn health_findings_become_alerts_with_their_fix_and_code() {
        let item = |sev, id: &str, fix: Option<health::Fix>| Item {
            id: id.into(),
            machine: "mini-01".into(),
            source: "daemon".into(),
            severity: sev,
            message: "FluxVM is not running".into(),
            cause: None,
            fix,
            explain: Some("DAEMON_DOWN".into()),
        };
        let mut h = health::assess(&[], 0);
        h.items = vec![
            item(Severity::Ok, "m/ok/1", None),
            item(
                Severity::Fail,
                "mini-01/daemon_down/2",
                Some(health::Fix {
                    command: Some("fluxctl service start".into()),
                    ..Default::default()
                }),
            ),
            item(Severity::Warn, "mini-01/low_disk/3", None),
        ];
        let a = health_alerts(&h, Utc::now());
        assert_eq!(a.len(), 2);
        let down = a.iter().find(|x| x.level == "error").unwrap();
        assert_eq!(down.key, "health:mini-01/daemon_down");
        assert_eq!(down.next_step, "Run `fluxctl service start`.");
        assert_eq!(down.explain.as_deref(), Some("DAEMON_DOWN"));
        assert!(
            a.iter()
                .any(|x| x.level == "warn" && x.next_step.contains("cluster status"))
        );
    }
}
