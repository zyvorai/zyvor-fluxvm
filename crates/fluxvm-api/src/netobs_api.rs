// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `/v1/netobs/*`: guest-network observability (flows, DNS, roll-ups, capture) from the process-wide
//! `fluxvm_netobs` observer. When no observer is installed the read routes answer with `enabled: false` and an
//! empty result, so clients can probe without special-casing.
//!
//! Tenant-scoped tokens only see VMs of their tenant, as for events. Capture holds packet bytes, so enabling,
//! disabling and downloading it require the admin role.

use super::{ApiError, ApiResult, TokenTenant, require_admin};
use axum::{
    Extension, Json,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use fluxvm_core::config::Role;
use fluxvm_netobs::{CaptureFilter, CaptureRequest, FlowQuery, Observer};
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Builds the daemon's observer, installs it as the process-wide one the routes read and starts the pump that drains
/// the process-wide tap. Returns the pump handle (keep it for the daemon's lifetime), or `None` when disabled by
/// `netobs.enabled = false` / `FLUXVM_NETOBS=0`, or when an observer is already installed.
pub fn install_netobs(cfg: &fluxvm_core::config::Config) -> Option<fluxvm_netobs::PumpHandle> {
    if !cfg.netobs.effective_enabled() {
        tracing::info!("network observability disabled");
        return None;
    }
    let obs = Arc::new(Observer::new(Default::default()));
    if !fluxvm_netobs::install_global(obs.clone()) {
        return None;
    }
    let (tap, rx) = fluxvm_netobs::NetTap::new(cfg.netobs.queue_frames, cfg.netobs.snaplen);
    fluxvm_netobs::install_global_tap(tap);
    tracing::info!("network observability enabled");
    Some(fluxvm_netobs::spawn_pump(obs, rx))
}

fn observer() -> Option<Arc<Observer>> {
    fluxvm_netobs::global()
}

/// The VM ids a tenant-scoped token may see; `None` for unscoped tokens.
async fn scope(m: &VmManager, tenant: Option<Extension<TokenTenant>>) -> Option<HashSet<String>> {
    let Extension(TokenTenant(t)) = tenant?;
    Some(
        m.list()
            .await
            .into_iter()
            .filter(|vm| vm.request.tenant.as_deref() == Some(t.as_str()))
            .map(|vm| vm.id.to_string())
            .collect(),
    )
}

fn not_found(msg: &str) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        message: msg.to_string(),
    }
}

fn unavailable() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        message: "network observability is not enabled on this host".to_string(),
    }
}

fn check_vm(scope: &Option<HashSet<String>>, vm: &str) -> ApiResult<()> {
    match scope {
        Some(s) if !s.contains(vm) => Err(not_found("VM not found")),
        _ => Ok(()),
    }
}

pub(crate) async fn summary(
    State(m): State<Arc<VmManager>>,
    tenant: Option<Extension<TokenTenant>>,
) -> ApiResult<Json<serde_json::Value>> {
    let now = fluxvm_netobs::now_us();
    let Some(o) = observer() else {
        return Ok(Json(
            serde_json::to_value(fluxvm_netobs::Summary::disabled(now)).unwrap_or_default(),
        ));
    };
    let scope = scope(&m, tenant).await;
    let mut s = o.summary(now);
    if let Some(allowed) = &scope {
        // Global counters describe the whole host; hide them from a tenant-scoped token, keep only its VMs.
        s.counters = Default::default();
        s.per_vm.retain(|v| allowed.contains(&v.vm_id));
        s.top_talkers.retain(|r| allowed.contains(&r.vm_id));
        s.top_errors.retain(|r| allowed.contains(&r.vm_id));
        s.capture.retain(|c| allowed.contains(&c.vm));
        s.flows_active = s.per_vm.iter().map(|v| v.flows_active).sum();
    }
    Ok(Json(serde_json::to_value(s).unwrap_or_default()))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct FlowParams {
    vm: Option<String>,
    proto: Option<String>,
    port: Option<u16>,
    host: Option<std::net::IpAddr>,
    verdict: Option<String>,
    #[serde(default)]
    closed: bool,
    limit: Option<usize>,
}

pub(crate) async fn flows(
    State(m): State<Arc<VmManager>>,
    tenant: Option<Extension<TokenTenant>>,
    Query(p): Query<FlowParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(o) = observer() else {
        return Ok(Json(
            json!({"schema": fluxvm_netobs::SCHEMA_VERSION, "enabled": false, "items": []}),
        ));
    };
    let q = FlowQuery {
        vm: p.vm,
        proto: p.proto,
        port: p.port,
        host: p.host,
        verdict: p.verdict,
        include_closed: p.closed,
        limit: p.limit.unwrap_or(200),
        allowed_vms: scope(&m, tenant).await,
    };
    Ok(Json(json!({
        "schema": fluxvm_netobs::SCHEMA_VERSION,
        "enabled": true,
        "items": o.flows(&q),
    })))
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamParams {
    since: Option<u64>,
    vm: Option<String>,
}

/// Server-sent events of flow, DNS, TLS, HTTP and drop metadata. `id:` is the event sequence number; resume with
/// `?since=<id>` or the `Last-Event-ID` header.
pub(crate) async fn stream(
    State(m): State<Arc<VmManager>>,
    tenant: Option<Extension<TokenTenant>>,
    headers: HeaderMap,
    Query(p): Query<StreamParams>,
) -> ApiResult<Response> {
    let Some(o) = observer() else {
        return Err(unavailable());
    };
    let scope = scope(&m, tenant).await;
    if let (Some(vm), s) = (&p.vm, &scope) {
        check_vm(s, vm)?;
    }
    let resume = p.since.or_else(|| {
        headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
    });
    let vm = p.vm;
    let body = async_stream::stream! {
        let mut since = resume.unwrap_or_else(|| o.last_seq());
        yield Ok::<_, std::io::Error>(bytes::Bytes::from_static(b": fluxvm netobs\n\n"));
        let mut idle = 0u32;
        loop {
            let (events, next) = o.events_since(since, scope.as_ref(), vm.as_deref(), 512);
            since = next;
            if events.is_empty() {
                idle += 1;
                if idle.is_multiple_of(20) {
                    yield Ok(bytes::Bytes::from_static(b": keepalive\n\n"));
                }
            } else {
                idle = 0;
                for e in events {
                    yield Ok(bytes::Bytes::from(e.to_sse()));
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(body))?)
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DnsParams {
    vm: Option<String>,
    limit: Option<usize>,
}

pub(crate) async fn dns(
    State(m): State<Arc<VmManager>>,
    tenant: Option<Extension<TokenTenant>>,
    Query(p): Query<DnsParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(o) = observer() else {
        return Ok(Json(
            json!({"schema": fluxvm_netobs::SCHEMA_VERSION, "enabled": false, "items": []}),
        ));
    };
    let scope = scope(&m, tenant).await;
    Ok(Json(json!({
        "schema": fluxvm_netobs::SCHEMA_VERSION,
        "enabled": true,
        "items": o.dns(p.vm.as_deref(), scope.as_ref(), p.limit.unwrap_or(200)),
    })))
}

pub(crate) async fn history(
    State(m): State<Arc<VmManager>>,
    tenant: Option<Extension<TokenTenant>>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(o) = observer() else {
        return Ok(Json(
            json!({"schema": fluxvm_netobs::SCHEMA_VERSION, "enabled": false, "windows": []}),
        ));
    };
    let scope = scope(&m, tenant).await;
    let windows: Vec<_> = o
        .history(scope.as_ref())
        .into_iter()
        .map(|(start, end, rows)| json!({"start_us": start, "end_us": end, "rows": rows}))
        .collect();
    Ok(Json(
        json!({"schema": fluxvm_netobs::SCHEMA_VERSION, "enabled": true, "windows": windows}),
    ))
}

pub(crate) async fn capture_status(
    State(m): State<Arc<VmManager>>,
    tenant: Option<Extension<TokenTenant>>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(o) = observer() else {
        return Ok(Json(
            json!({"schema": fluxvm_netobs::SCHEMA_VERSION, "enabled": false, "sessions": []}),
        ));
    };
    let scope = scope(&m, tenant).await;
    let sessions: Vec<_> = o
        .capture_status()
        .into_iter()
        .filter(|c| scope.as_ref().is_none_or(|s| s.contains(&c.vm)))
        .collect();
    Ok(Json(
        json!({"schema": fluxvm_netobs::SCHEMA_VERSION, "enabled": true, "sessions": sessions}),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CaptureBody {
    vm: String,
    #[serde(flatten)]
    req: CaptureRequest,
}

pub(crate) async fn capture_enable(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    tenant: Option<Extension<TokenTenant>>,
    Json(body): Json<CaptureBody>,
) -> ApiResult<Json<serde_json::Value>> {
    require_admin(role)?;
    let Some(o) = observer() else {
        return Err(unavailable());
    };
    check_vm(&scope(&m, tenant).await, &body.vm)?;
    let ttl = body.req.ttl_secs;
    let filter: CaptureFilter = body.req.filter.clone();
    o.capture_enable(&body.vm, body.req).map_err(|e| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: e.to_string(),
    })?;
    Ok(Json(
        json!({"vm": body.vm, "enabled": true, "ttl_secs": ttl, "filter": filter}),
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct VmParam {
    vm: String,
}

pub(crate) async fn capture_disable(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    tenant: Option<Extension<TokenTenant>>,
    Query(p): Query<VmParam>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    let Some(o) = observer() else {
        return Err(unavailable());
    };
    check_vm(&scope(&m, tenant).await, &p.vm)?;
    if o.capture_disable(&p.vm) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found("no capture session for that VM"))
    }
}

pub(crate) async fn capture_pcap(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    tenant: Option<Extension<TokenTenant>>,
    Query(p): Query<VmParam>,
) -> ApiResult<Response> {
    require_admin(role)?;
    let Some(o) = observer() else {
        return Err(unavailable());
    };
    check_vm(&scope(&m, tenant).await, &p.vm)?;
    let Some(bytes) = o.capture_pcap(&p.vm) else {
        return Err(not_found("no capture session for that VM"));
    };
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.tcpdump.pcap".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"netobs-{}.pcap\"",
                    p.vm.replace(['"', '/', '\\'], "_")
                ),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// Prometheus text for the `/metrics` route; empty when no observer is installed.
pub(crate) fn prometheus() -> String {
    observer()
        .map(|o| fluxvm_netobs::render_prometheus(&o.summary(fluxvm_netobs::now_us())))
        .unwrap_or_default()
}
