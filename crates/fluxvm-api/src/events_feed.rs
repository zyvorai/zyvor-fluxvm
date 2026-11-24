// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/events?source=...` and `GET /v1/events/stream?source=...`: the merged feed (VM, Mac, Kubernetes, container,
//! backup and audit events in one list). The routes are the existing event routes; naming a `source` (`all` for every
//! one) switches the reply to the merged feed, so callers that do not ask keep getting the plain VM event log.
//!
//! Who sees what: an admin or read-only token sees every source except `audit` (the audit log names every actor, so it
//! stays admin only, like `/v1/audit`); a tenant-scoped token sees only events of its own tenant's VMs and containers.
//! A request naming a source the caller may not read is refused in words, not silently emptied.

use crate::{ApiResult, Role, TokenTenant};
use axum::{
    Json,
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use fluxvm_core::model::VmRecord;
use fluxvm_scheduler::VmManager;
use fluxvm_scheduler::event_feed::{self, Feed, FeedEvent, FeedQuery, Inputs};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// What the caller may read: which sources, and which VMs when it is scoped to a tenant.
struct Scope {
    vms: Option<HashSet<Uuid>>,
    admin: bool,
}

async fn scope(
    m: &VmManager,
    role: Option<Role>,
    tenant: Option<crate::TokenTenant>,
) -> (Scope, Vec<VmRecord>) {
    let all = m.list().await;
    let vms = tenant.map(|TokenTenant(t)| {
        all.iter()
            .filter(|v| v.request.tenant.as_deref() == Some(t.as_str()))
            .map(|v| v.id)
            .collect()
    });
    (
        Scope {
            vms,
            admin: matches!(role, Some(Role::Admin)),
        },
        all,
    )
}

fn bad(e: event_feed::FeedError) -> crate::ApiError {
    crate::kube::err(StatusCode::BAD_REQUEST, e.to_string())
}

/// The checked query, narrowed to what this caller may read.
fn feed_for(q: &FeedQuery, scope: &Scope) -> Result<Feed, event_feed::FeedError> {
    let mut feed = Feed::parse(q, chrono::Utc::now())?;
    let asked_all = q
        .source
        .as_deref()
        .is_none_or(|s| s.trim().is_empty() || s.split(',').any(|x| x.trim() == "all"));
    let allowed = |s: &str| match (&scope.vms, s) {
        (Some(_), "vm" | "docker") => true,
        (Some(_), _) => false,
        (None, "audit") => scope.admin,
        (None, _) => true,
    };
    if !asked_all {
        if let Some(s) = feed.sources.iter().find(|s| !allowed(s)) {
            return Err(event_feed::FeedError {
                code: "EVENTS_NOT_ALLOWED",
                message: if scope.vms.is_some() {
                    format!(
                        "This token can only read events of its own VMs and containers, not `{s}`. Ask for --source vm or --source docker, or use an admin token."
                    )
                } else {
                    format!(
                        "`{s}` shows who changed what, so it needs an admin token. Use an admin token, or leave `{s}` out of --source."
                    )
                },
            });
        }
    }
    feed.sources.retain(|s| allowed(s));
    Ok(feed)
}

fn containers(all: &[VmRecord]) -> HashMap<Uuid, (String, String)> {
    all.iter()
        .filter_map(|v| {
            Some((
                v.id,
                (
                    v.name.clone(),
                    v.labels
                        .get(fluxvm_scheduler::oci_sandbox::OCI_IMAGE_LABEL)?
                        .clone(),
                ),
            ))
        })
        .collect()
}

fn visible(scope: &Scope, e: &FeedEvent) -> bool {
    scope
        .vms
        .as_ref()
        .is_none_or(|ids| e.vm_id.is_some_and(|id| ids.contains(&id)))
}

pub(crate) async fn list(
    m: &Arc<VmManager>,
    role: Option<Role>,
    tenant: Option<axum::Extension<TokenTenant>>,
    q: FeedQuery,
) -> ApiResult<Json<Value>> {
    let (scope, all) = scope(m, role, tenant.map(|axum::Extension(t)| t)).await;
    let feed = feed_for(&q, &scope).map_err(bad)?;
    let names = containers(&all);
    let dir = m.cfg.state_dir.clone();
    let items = tokio::task::spawn_blocking(move || {
        let lookup = |id: Uuid| names.get(&id).cloned();
        let inp = Inputs {
            state_dir: &dir,
            container: &lookup,
            schedules: fluxvm_core::mac_schedule::default_path(),
            backups: fluxvm_core::mac_backup::default_root(),
        };
        event_feed::collect(&inp, &feed, chrono::Utc::now()).map(|v| (v, feed.sources.clone()))
    })
    .await
    .map_err(|e| anyhow::anyhow!("event read task failed: {e}"))??;
    let (mut items, sources) = items;
    items.retain(|e| visible(&scope, e));
    Ok(Json(json!({
        "items": items,
        "sources": sources,
        "available": event_feed::source_list(),
    })))
}

/// Server-sent events: one `data:` JSON line per new event, newest last, until the client goes away.
pub(crate) async fn stream(
    m: Arc<VmManager>,
    role: Option<Role>,
    tenant: Option<axum::Extension<TokenTenant>>,
    q: FeedQuery,
) -> ApiResult<Response> {
    let (scope, all) = scope(&m, role, tenant.map(|axum::Extension(t)| t)).await;
    let feed = feed_for(&q, &scope).map_err(bad)?;
    let dir = m.cfg.state_dir.clone();
    let mut names = containers(&all);
    let stream = async_stream::stream! {
        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b": fluxvm events\n\n"));
        let mut cursor = {
            let lookup = |id: Uuid| names.get(&id).cloned();
            let inp = Inputs { state_dir: &dir, container: &lookup, schedules: fluxvm_core::mac_schedule::default_path(), backups: fluxvm_core::mac_backup::default_root() };
            event_feed::Cursor::at_end(&inp, chrono::Utc::now())
        };
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            names = containers(&m.list().await);
            let lookup = |id: Uuid| names.get(&id).cloned();
            let inp = Inputs { state_dir: &dir, container: &lookup, schedules: fluxvm_core::mac_schedule::default_path(), backups: fluxvm_core::mac_backup::default_root() };
            match event_feed::poll(&inp, &feed, &mut cursor, chrono::Utc::now()) {
                Ok(events) => {
                    for ev in events.iter().filter(|e| visible(&scope, e)) {
                        if let Ok(s) = serde_json::to_string(ev) {
                            yield Ok(bytes::Bytes::from(format!("event: {}\ndata: {s}\n\n", ev.event)));
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("The event logs could not be read ({e}). Check that the state directory is readable, then run `fluxctl events --follow` again. more: fluxctl explain EVENTS_UNREADABLE");
                    yield Ok(bytes::Bytes::from(format!("event: error\ndata: {}\n\n", json!({"error": msg}))));
                    return;
                }
            }
        }
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(source: &str) -> FeedQuery {
        FeedQuery {
            source: Some(source.into()),
            ..Default::default()
        }
    }

    #[test]
    fn audit_needs_an_admin_and_tenants_see_only_their_own() {
        let admin = Scope {
            vms: None,
            admin: true,
        };
        assert_eq!(feed_for(&q("all"), &admin).unwrap().sources.len(), 6);
        let reader = Scope {
            vms: None,
            admin: false,
        };
        let f = feed_for(&q("all"), &reader).unwrap();
        assert!(!f.sources.contains("audit") && f.sources.len() == 5);
        let e = feed_for(&q("audit"), &reader).unwrap_err();
        assert_eq!(e.code, "EVENTS_NOT_ALLOWED");
        assert!(e.message.contains("admin token"));
        let tenant = Scope {
            vms: Some(HashSet::new()),
            admin: false,
        };
        let f = feed_for(&q("all"), &tenant).unwrap();
        assert_eq!(
            f.sources.iter().copied().collect::<Vec<_>>(),
            ["docker", "vm"]
        );
        assert_eq!(
            feed_for(&q("mac"), &tenant).unwrap_err().code,
            "EVENTS_NOT_ALLOWED"
        );
        assert_eq!(
            feed_for(&q("nope"), &admin).unwrap_err().code,
            "EVENTS_BAD_SOURCE"
        );
    }

    #[test]
    fn a_tenant_sees_only_lines_about_its_own_vms() {
        let mine = Uuid::new_v4();
        let ev = |id| FeedEvent {
            ts: chrono::Utc::now(),
            source: "vm".into(),
            event: "vm.start".into(),
            summary: String::new(),
            level: "info".into(),
            target: None,
            vm_id: id,
            fields: Default::default(),
        };
        let s = Scope {
            vms: Some(HashSet::from([mine])),
            admin: false,
        };
        assert!(visible(&s, &ev(Some(mine))));
        assert!(!visible(&s, &ev(Some(Uuid::new_v4()))));
        assert!(!visible(&s, &ev(None)));
        assert!(visible(
            &Scope {
                vms: None,
                admin: false
            },
            &ev(None)
        ));
    }
}
