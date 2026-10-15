// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Central fleet registry + proxy. Deliberately doesn't depend on
//! `fluxvm-core` — it treats `CreateVmRequest`/`VmRecord` bodies as opaque
//! JSON and just forwards them to the right node's own `fluxctl serve`,
//! the same way `fluxvm-kube`'s client does. This keeps the binary small
//! and means it never goes stale against the request/record schema; the
//! cost is no server-side validation beyond "is this valid JSON" — a node's
//! own `fluxctl serve` still does the real validation.

#[path = "central_audit.rs"]
mod audit;
#[path = "central_cloud.rs"]
mod cloud;
#[path = "central_ha.rs"]
mod ha_rt;
#[path = "central_metrics.rs"]
mod metrics;
#[path = "central_notify.rs"]
mod notify;
#[path = "central_warmpool.rs"]
mod pools;
#[path = "central_signup.rs"]
mod signup;
#[path = "central_snapshots.rs"]
mod snapshots;
#[path = "central_sso.rs"]
mod sso;
#[path = "central_tunnel.rs"]
mod tunnel;
#[path = "central_volumes.rs"]
mod vols;

pub use ha_rt::HaHandle;
pub use metrics::{
    DEFAULT_INTERVAL as DEFAULT_METRICS_INTERVAL, MetricsSettings, configure as configure_metrics,
};
pub use signup::{MailConfig, configure_mail, configure_stripe};
pub use sso::{SsoArgs, SsoConfig};

use crate::tenancy::{Caller, TenancyState};
use axum::{
    Extension, Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use fluxvm_core::agent_density::DensityReport;
use fluxvm_core::pressure_admission::PressureLevel;
use fluxvm_core::security::{NodeSecurityCapabilities, SecurityProfile};
use fluxvm_scheduler::apple_placement::{
    AppleHostCaps, ApplePlacementRequest, score as apple_score,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::io::AsRawFd,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

/// A node is considered unhealthy (excluded from placement, and reported
/// but flagged in `GET /fleet/nodes`) once its last heartbeat is older
/// than this — generous relative to the node agent's own default 10s
/// heartbeat interval, so a couple of missed beats under load doesn't
/// falsely evict a node that's still fine.
const HEALTHY_WINDOW_SECS: i64 = 30;

/// Default vCPU/memory assumed per existing VM when estimating residual
/// capacity for placement (used when the create body omits sizes).
const DEFAULT_VM_VCPUS: u32 = 2;
const DEFAULT_VM_MEMORY_MIB: u64 = 2048;

/// Whether the node agent could reach its own FluxVM daemon on its last heartbeat. A node whose daemon
/// is down still heartbeats (so it is `healthy` in the freshness sense) but is not a placement target.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What the node's running VMs actually request, summed from its daemon's records (not a per-VM guess).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allocated {
    pub vcpus: u32,
    pub memory_mib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    pub fluxvm_url: String,
    pub vcpus_total: u32,
    pub memory_mib_total: u64,
    pub vm_count: usize,
    pub last_seen: DateTime<Utc>,
    /// Operator-set maintenance flag (`POST /fleet/nodes/{name}/cordon`):
    /// excludes this node from automatic (residual-capacity) placement in
    /// `pick_best_capacity` without deregistering it or touching any VM
    /// already on it — same "stop scheduling here, don't evict anything"
    /// semantics as `kubectl cordon`. `#[serde(default)]` so a
    /// `fleet-nodes.json` written before this field existed still loads
    /// cleanly (as `false`, the honest "never cordoned" value).
    #[serde(default)]
    pub cordoned: bool,
    /// Operator-defined labels this node reports on every heartbeat (e.g.
    /// `zone=us-east`, `gpu=true`), set via `fluxvm-agent node --label
    /// key=value` (repeatable). Purely a placement input: `POST /fleet/vms`
    /// can carry a `"nodeSelector"` object, and automatic placement only
    /// ever considers a node whose labels are a superset of it -- the same
    /// exact-match semantics as Kubernetes' own `spec.nodeSelector`.
    /// `#[serde(default)]` so a `fleet-nodes.json` written before this
    /// field existed still loads cleanly (as the honest "no labels" empty
    /// map).
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Host security capabilities from the node's last heartbeat.
    #[serde(default)]
    pub security: NodeSecurityCapabilities,
    /// The node's `GET /v1/sandboxes/density` from its last heartbeat: memory-pressure level, warm slots, sandbox counts.
    /// `None` from a node agent that predates it, or a daemon that does not serve it.
    #[serde(default)]
    pub density: Option<DensityReport>,
    /// Apple VZ placement capabilities; None for Linux/non-VZ nodes and old agents.
    #[serde(default)]
    pub apple: Option<AppleHostCaps>,
    /// `None` from a node agent that predates it (treated as reachable).
    #[serde(default)]
    pub daemon: Option<DaemonStatus>,
    /// Summed CPU and memory the node's active VMs request; `None` from an older agent, which falls
    /// back to the per-VM estimate.
    #[serde(default)]
    pub allocated: Option<Allocated>,
}

impl NodeInfo {
    /// False when the node agent reported that it cannot reach its FluxVM daemon.
    fn daemon_reachable(&self) -> bool {
        self.daemon.as_ref().is_none_or(|d| d.reachable)
    }

    fn healthy(&self) -> bool {
        (Utc::now() - self.last_seen).num_seconds() < HEALTHY_WINDOW_SECS
    }

    /// True iff every key/value pair in `selector` is present, with an
    /// exact-match value, in this node's own labels. An empty selector
    /// always matches -- "no selector" means no placement constraint,
    /// same as omitting `nodeSelector` entirely.
    fn matches_selector(&self, selector: &HashMap<String, String>) -> bool {
        selector.iter().all(|(k, v)| self.labels.get(k) == Some(v))
    }

    fn estimated_used_vcpus(&self) -> u32 {
        match self.allocated {
            Some(a) => a.vcpus,
            None => (self.vm_count as u32).saturating_mul(DEFAULT_VM_VCPUS),
        }
    }

    fn estimated_used_memory_mib(&self) -> u64 {
        match self.allocated {
            Some(a) => a.memory_mib,
            None => (self.vm_count as u64).saturating_mul(DEFAULT_VM_MEMORY_MIB),
        }
    }

    fn free_vcpus(&self) -> u32 {
        self.vcpus_total.saturating_sub(self.estimated_used_vcpus())
    }

    fn free_memory_mib(&self) -> u64 {
        self.memory_mib_total
            .saturating_sub(self.estimated_used_memory_mib())
    }

    /// Higher is better. Prefer residual capacity fraction; tie-break fewer
    /// VMs then lexicographic name (stable, deterministic).
    fn capacity_score(
        &self,
        request_vcpus: u32,
        request_mem: u64,
    ) -> Option<(i64, i64, i64, String)> {
        if !self.healthy()
            || !self.daemon_reachable()
            || self.cordoned
            || self.pressure() == Some(PressureLevel::Critical)
        {
            return None;
        }
        if self.free_vcpus() < request_vcpus || self.free_memory_mib() < request_mem {
            // Still eligible if totals are tiny / estimates pessimistic —
            // fall through with low score rather than hard-exclude when
            // vm_count estimate overshoots; only exclude if totals themselves
            // cannot fit a single request.
            if self.vcpus_total < request_vcpus || self.memory_mib_total < request_mem {
                return None;
            }
        }
        let vcpu_frac = if self.vcpus_total == 0 {
            0
        } else {
            (self.free_vcpus() as i64 * 10_000) / self.vcpus_total as i64
        };
        let mem_frac = if self.memory_mib_total == 0 {
            0
        } else {
            (self.free_memory_mib() as i64 * 10_000) / self.memory_mib_total as i64
        };
        let residual = vcpu_frac.min(mem_frac);
        // Sort key for max: calmer memory pressure, residual desc, -vm_count, -name (via reverse min)
        let calm = match self.pressure() {
            Some(PressureLevel::Warn) => -1,
            _ => 0,
        };
        Some((residual, -(self.vm_count as i64), calm, self.name.clone()))
    }

    /// The memory-pressure level the node last reported (macOS hosts report one; Linux hosts do not).
    fn pressure(&self) -> Option<PressureLevel> {
        self.density.as_ref().and_then(|d| d.host_pressure_level)
    }
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub name: String,
    pub fluxvm_url: String,
    pub vcpus_total: u32,
    pub memory_mib_total: u64,
    pub vm_count: usize,
    /// See `NodeInfo::labels`. `#[serde(default)]` so an older node agent
    /// that predates labels can still heartbeat successfully.
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(default)]
    pub security: NodeSecurityCapabilities,
    #[serde(default)]
    pub density: Option<DensityReport>,
    #[serde(default)]
    pub apple: Option<AppleHostCaps>,
    #[serde(default)]
    pub daemon: Option<DaemonStatus>,
    #[serde(default)]
    pub allocated: Option<Allocated>,
}

#[derive(Debug)]
struct AppError(StatusCode, String);
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
impl<E: std::fmt::Display> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(StatusCode::BAD_GATEWAY, e.to_string())
    }
}

#[derive(Clone)]
struct Fleet {
    nodes: Arc<Mutex<HashMap<String, NodeInfo>>>,
    http: reqwest::Client,
    persist_path: Option<PathBuf>,
    lock_path: Option<PathBuf>,
    /// Shared secret; when `Some`, all `/fleet/*` routes require Bearer.
    token: Option<String>,
    /// Tenants, leases, the VM ledger and drain state (`fleet-tenancy.json`). Lock order when both are needed:
    /// `tenancy` first, then `nodes`.
    tenancy: Arc<Mutex<TenancyState>>,
    tenancy_path: Option<PathBuf>,
    /// Leader election when several centrals share a store (`--ha-redis`); `None` is a single central.
    ha: Option<Arc<ha_rt::HaRuntime>>,
    /// The activity log (`GET /fleet/events`).
    audit: Arc<audit::AuditLog>,
    /// Notifications and webhook deliveries.
    notify: Arc<notify::Notifier>,
    /// Usage history and counters (`/fleet/metrics/*`, `GET /metrics`).
    metrics: Arc<metrics::MetricsStore>,
    /// Open tunnels to VMs (`/fleet/tunnels`), in memory only.
    tunnels: Arc<tunnel::Tunnels>,
    /// Console single sign-on (OIDC) and its in-flight sign-ins.
    sso: Arc<sso::Sso>,
    /// Self-service sign-up and Stripe (rate limits, the Stripe config).
    signup: Arc<signup::Runtime>,
}

impl Fleet {
    /// Writes `fleet-tenancy.json` (no-op for an in-memory test fleet).
    fn persist_tenancy(&self, state: &TenancyState) {
        if let (Some(path), Some(lock)) = (&self.tenancy_path, &self.lock_path) {
            persist_json(path, lock, state);
        }
        if let Some(ha) = &self.ha
            && let Ok(json) = serde_json::to_string(state)
        {
            ha.mirror("tenancy", json);
        }
    }

    /// Whether this central serves fleet requests (always, without HA).
    fn leading(&self) -> bool {
        self.ha.as_ref().is_none_or(|h| h.is_leader())
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The tenant's named API key behind a request (`None`: its primary token, or the operator). `auth_middleware` puts
/// it next to the [`Caller`]; the key routes check it themselves too, so no path spelling gets a key past them.
#[derive(Debug, Clone, Default)]
struct KeyUsed(Option<String>);

/// Resolves the caller from the bearer token and stores it as an `Extension<Caller>` for the handlers.
///
/// - The fleet token (`--token`) is the operator ([`Caller::Admin`]); node heartbeats use it too.
/// - A tenant token (see `POST /fleet/tenants`) is [`Caller::Tenant`], limited to [`tenant_route_allowed`].
/// - Without a fleet token (lab mode) every caller that is not a tenant is the operator, as before tenants existed,
///   but only while no tenant exists: once one does, anonymous callers and unknown tokens get a 401. Central refuses
///   to create tenants without `--token`.
async fn auth_middleware(State(fleet): State<Fleet>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == "/healthz"
        || path == "/fleet/ha"
        || path == "/fleet/ha/leader"
        || CONSOLE_PATHS.contains(&path)
    {
        return next.run(req).await;
    }
    if let Some(ha) = &fleet.ha
        && !ha.is_leader()
    {
        return ha_rt::not_leader(ha);
    }
    // ---- sso ---- (the sign-in routes are public; they check the session cookie themselves)
    if path.starts_with("/auth/") {
        return next.run(req).await;
    }
    // ---- signup ---- (sign-up and the Stripe webhook, which is signed)
    if signup::PUBLIC_PATHS.contains(&path) {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_owned);
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "missing or invalid bearer token"})),
        )
            .into_response()
    };
    // The tenant's named API key, when one was used, and whether it may only read.
    let mut key_used: Option<String> = None;
    let mut read_only = false;
    // ---- sso ---- a console session cookie, used only when no bearer token was sent.
    let mut user: Option<sso::SessionUser> = None;
    let mut session_caller: Option<Caller> = None;
    if presented.is_none() && fleet.sso.enabled() {
        let (method, headers) = (req.method().clone(), req.headers().clone());
        match sso::caller_from_cookie(&fleet, &method, &headers).await {
            sso::FromCookie::Absent => {}
            sso::FromCookie::Reject(resp) => return resp,
            sso::FromCookie::User(c, u) => {
                session_caller = Some(c);
                user = Some(u);
            }
        }
    }
    let caller = match (fleet.token.as_deref(), presented.as_deref()) {
        _ if session_caller.is_some() => session_caller.clone().unwrap_or(Caller::Admin),
        (Some(expected), Some(t)) if constant_time_eq(t, expected) => Caller::Admin,
        (expected, Some(t)) => {
            let mut tenancy = fleet.tenancy.lock().await;
            match tenancy.authenticate_key(t, Utc::now()) {
                Some(m) if m.disabled => {
                    let id = m.tenant.clone();
                    if audit::audited(req.method(), req.uri().path()) {
                        fleet.audit.record(audit::event(
                            &Caller::Tenant(id.clone()),
                            req.method().to_string(),
                            req.uri().path().to_owned(),
                            None,
                            StatusCode::FORBIDDEN,
                            m.key.clone(),
                        ));
                    }
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": format!("tenant '{id}' is disabled")})),
                    )
                        .into_response();
                }
                Some(m) => {
                    key_used = m.key;
                    read_only = m.read_only;
                    Caller::Tenant(m.tenant)
                }
                // Lab mode keeps ignoring stray tokens, but only until tenants exist: after that an unknown token is
                // a mistyped tenant token and must not quietly become the operator.
                None if expected.is_none() && !tenancy.enabled() => Caller::Admin,
                None => return unauthorized(),
            }
        }
        (None, None) => {
            // Lab mode, but tenants exist (the token was removed after they were created): an anonymous caller could
            // be a tenant that dropped its token, so nobody is the operator until central runs with --token again.
            if fleet.tenancy.lock().await.enabled() {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "tenants exist but central runs without --token; restart it with --token"})),
                )
                    .into_response();
            }
            Caller::Admin
        }
        (Some(_), None) => return unauthorized(),
    };
    // A tenant reaching for an operator route is worth knowing about, so it is recorded; unauthenticated callers are not
    // (anyone could fill the log).
    if let Caller::Tenant(t) = &caller
        && !tenant_route_allowed(req.method(), req.uri().path())
    {
        if audit::audited(req.method(), req.uri().path()) {
            fleet.audit.record(audit::event(
                &caller,
                req.method().to_string(),
                req.uri().path().to_owned(),
                None,
                StatusCode::FORBIDDEN,
                key_used.clone(),
            ));
        }
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": format!("tenant '{t}' may not call {} {}", req.method(), req.uri().path())})),
        )
            .into_response();
    }
    // A read-only key only reads, and only the primary token manages keys (a leaked CI key must not mint more).
    if let Some(name) = &key_used {
        let changes = audit::audited(req.method(), req.uri().path());
        let keys_route = req.uri().path().starts_with("/fleet/tenants/me/keys");
        let refusal = if read_only && changes {
            Some(format!("API key '{name}' is read-only"))
        } else if keys_route && changes {
            Some("API keys are created and revoked with the tenant's primary token".to_owned())
        } else {
            None
        };
        if let Some(msg) = refusal {
            fleet.audit.record(audit::event(
                &caller,
                req.method().to_string(),
                req.uri().path().to_owned(),
                None,
                StatusCode::FORBIDDEN,
                key_used.clone(),
            ));
            return (StatusCode::FORBIDDEN, Json(json!({"error": msg}))).into_response();
        }
    }
    // ---- sso ---- a member's role decides what they may change.
    if let Some(u) = &user
        && let Some(role) = u.role
        && let Some(msg) = sso::role_refusal(role, req.method(), req.uri().path())
    {
        let mut ev = audit::event(
            &caller,
            req.method().to_string(),
            req.uri().path().to_owned(),
            None,
            StatusCode::FORBIDDEN,
            None,
        );
        ev.user = Some(u.email.clone());
        fleet.audit.record(ev);
        return (StatusCode::FORBIDDEN, Json(json!({"error": msg}))).into_response();
    }
    // ---- signup ----
    if let Caller::Tenant(t) = &caller
        && let Some(refusal) = signup::payment_gate(&fleet, t, req.method(), req.uri().path()).await
    {
        return refusal;
    }
    req.extensions_mut().insert(caller.clone());
    req.extensions_mut().insert(KeyUsed(key_used.clone()));
    req.extensions_mut().insert(sso::Who(user.clone()));
    if !audit::audited(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    let method = req.method().to_string();
    let path = req.uri().path().to_owned();
    let (req, target) = match audit::target_of(req).await {
        Ok(x) => x,
        Err(resp) => return resp,
    };
    let resp = next.run(req).await;
    // Screen input (every click and keystroke) is not recorded one by one; who may control a screen is.
    if path.ends_with("/input") {
        return resp;
    }
    let mut event = audit::event(&caller, method, path, target, resp.status(), key_used);
    event.user = user.map(|u| u.email);
    notify::from_change(&fleet, &event).await;
    fleet.audit.record(event);
    resp
}

/// The routes a tenant token may call; everything else (node registration, cordon, drain, scrub, tenant admin,
/// policy) is operator-only.
fn tenant_route_allowed(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    let get = *method == Method::GET;
    let post = *method == Method::POST;
    let delete = *method == Method::DELETE;
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    match seg.as_slice() {
        ["fleet", "capacity"] | ["fleet", "usage"] | ["fleet", "tenants", "me"] => get,
        ["fleet", "tenants", "me", "keys"] => get || post,
        ["fleet", "tenants", "me", "keys", _] => delete,
        ["fleet", "prices"] | ["fleet", "billing"] | ["fleet", "notifications"] => get,
        ["fleet", "webhooks"] => get || post,
        ["fleet", "webhooks", _] => delete,
        ["fleet", "webhooks", _, "test"] => post,
        ["fleet", "tenants", "me", "budget"] => *method == Method::PUT,
        ["fleet", "vms"] | ["fleet", "leases"] => get || post,
        ["fleet", "vms", _, _] => delete,
        ["fleet", "vms", _, _, "exec"] => post,
        [
            "fleet",
            "vms",
            _,
            _,
            "start" | "stop" | "restart" | "pause" | "resume" | "input",
        ] => post,
        ["fleet", "vms", _, _, "logs"] => get,
        ["fleet", "vms", _, _, "screen"] | ["fleet", "events"] => get,
        ["fleet", "leases", _] => get,
        ["fleet", "leases", _, "release"] => post,
        ["fleet", "images"] => get,
        ["fleet", "nodes", _] => get,
        ["fleet", "overlays"] => get || post,
        ["fleet", "overlays", _] => get || delete,
        ["fleet", "overlays", _, "members"] | ["fleet", "overlays", _, "rotate"] => post,
        ["fleet", "overlays", _, "members", _] => delete,
        // ---- metrics ----
        ["fleet", "metrics", "summary"] | ["fleet", "metrics", "vms"] => get,
        ["fleet", "metrics", "vms", _, _] | ["fleet", "metrics", "nodes", _] => get,
        // ---- snapshots ----
        ["fleet", "vms", _, _, "snapshots"] => get || post,
        ["fleet", "vms", _, _, "snapshots", _] => delete,
        ["fleet", "vms", _, _, "snapshots", _, "restore"] | ["fleet", "vms", _, _, "clone"] => post,
        // ---- volumes ----
        ["fleet", "volumes"] => get || post,
        ["fleet", "volumes", _] => get || delete,
        ["fleet", "volumes", _, "resize"] => post,
        // ---- tunnel ----
        ["fleet", "vms", _, _, "tunnel"] | ["fleet", "tunnels"] => get,
        ["fleet", "tunnels", _] => delete,
        // ---- warmpool ----
        ["fleet", "pools"] => get || post,
        ["fleet", "pools", _] => get || delete || *method == Method::PATCH,
        ["fleet", "pools", _, "status"] => get,
        // ---- sso ----
        ["fleet", "tenants", "me", "members"] => get || post,
        ["fleet", "tenants", "me", "members", _] => *method == Method::PATCH || delete,
        // ---- signup ----
        ["fleet", "billing", "invoices"] => get,
        ["fleet", "billing", "payment-method"] => post,
        _ => false,
    }
}

fn load_persisted(path: &FsPath) -> HashMap<String, NodeInfo> {
    if !path.exists() {
        return HashMap::new();
    }
    match fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => HashMap::new(),
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, path = %path.display(), "failed to parse fleet registry; starting empty");
            HashMap::new()
        }),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "failed to read fleet registry");
            HashMap::new()
        }
    }
}

fn persist_nodes(path: &FsPath, lock_path: &FsPath, nodes: &HashMap<String, NodeInfo>) {
    persist_json(path, lock_path, nodes)
}

fn load_tenancy(path: &FsPath) -> TenancyState {
    match fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => TenancyState::default(),
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            // Refuse to silently start with no tenants: that would turn every tenant token into "unknown" and,
            // without a fleet token, every caller into the operator.
            panic!(
                "failed to parse {}: {e}; fix or move the file before starting central",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => TenancyState::default(),
        Err(e) => panic!("failed to read {}: {e}", path.display()),
    }
}

/// Writes `value` as pretty JSON to `path` atomically (tmp + rename) under an exclusive `flock` on `lock_path`.
fn persist_json<T: Serialize + ?Sized>(path: &FsPath, lock_path: &FsPath, value: &T) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let lock_file = match fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "opening fleet lock");
            return;
        }
    };
    if unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        tracing::warn!(
            error = %std::io::Error::last_os_error(),
            "locking fleet registry"
        );
        return;
    }
    let tmp = path.with_extension("json.tmp");
    match serde_json::to_vec_pretty(value) {
        Ok(bytes) => {
            if let Err(e) = fs::write(&tmp, &bytes) {
                tracing::warn!(error = %e, "writing fleet registry tmp");
                return;
            }
            // The tenancy state holds overlay network keys: owner-only.
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)) {
                    tracing::warn!(error = %e, "restricting fleet registry tmp");
                    return;
                }
            }
            if let Err(e) = fs::rename(&tmp, path) {
                tracing::warn!(error = %e, "renaming fleet registry");
            }
        }
        Err(e) => tracing::warn!(error = %e, "serializing fleet registry"),
    }
}

pub struct CentralConfig {
    pub state_dir: PathBuf,
    pub token: Option<String>,
    /// How often to reconcile the VM ledger against every node and end expired leases (`None`: only on
    /// `POST /fleet/reconcile`). Needs a Tokio runtime when `router` is called.
    pub reconcile_interval: Option<std::time::Duration>,
    /// Run as one of several centrals sharing a store; see [`crate::ha`].
    pub ha: Option<crate::ha::HaConfig>,
}

pub fn router(cfg: CentralConfig) -> Router {
    router_with_handle(cfg).0
}

/// [`router`], plus a handle on this central's leader election when HA is on.
pub fn router_with_handle(cfg: CentralConfig) -> (Router, Option<HaHandle>) {
    router_with_sso(cfg, None)
}

/// [`router_with_handle`] with console single sign-on (`None`: off).
pub fn router_with_sso(
    cfg: CentralConfig,
    sso_cfg: Option<SsoConfig>,
) -> (Router, Option<HaHandle>) {
    let persist_path = cfg.state_dir.join("fleet-nodes.json");
    let lock_path = cfg.state_dir.join("fleet-nodes.lock");
    let tenancy_path = cfg.state_dir.join("fleet-tenancy.json");
    let _ = fs::create_dir_all(&cfg.state_dir);
    let initial = load_persisted(&persist_path);
    tracing::info!(
        path = %persist_path.display(),
        nodes = initial.len(),
        "loaded fleet registry"
    );
    let tenancy = load_tenancy(&tenancy_path);
    if tenancy.enabled() && cfg.token.is_none() {
        tracing::warn!(
            tenants = tenancy.tenants.len(),
            "tenants exist but central has no --token: any caller without a tenant token is treated as the operator"
        );
    }
    let fleet = Fleet {
        nodes: Arc::new(Mutex::new(initial)),
        http: reqwest::Client::new(),
        persist_path: Some(persist_path),
        lock_path: Some(lock_path),
        token: cfg.token,
        tenancy: Arc::new(Mutex::new(tenancy)),
        tenancy_path: Some(tenancy_path),
        ha: None,
        audit: Arc::new(audit::AuditLog::open(
            cfg.state_dir.join("fleet-audit.jsonl"),
        )),
        notify: Arc::default(),
        metrics: Arc::new(metrics::MetricsStore::configured()),
        tunnels: Arc::default(),
        sso: Arc::new(sso::Sso::new(sso_cfg)),
        signup: Arc::new(
            signup::Runtime::new(signup::boot_config()).with_mail(signup::boot_mail()),
        ),
    };
    let mut fleet = fleet;
    let mut handle = None;
    if let Some(ha_cfg) = cfg.ha {
        let (rt, rx) = ha_rt::HaRuntime::new(ha_cfg);
        fleet.ha = Some(rt.clone());
        handle = Some(HaHandle(rt.clone()));
        if tokio::runtime::Handle::try_current().is_ok() {
            ha_rt::spawn(fleet.clone(), rt, rx);
        }
    }
    if let Some(every) = cfg.reconcile_interval
        && tokio::runtime::Handle::try_current().is_ok()
    {
        cloud::spawn_reconciler(fleet.clone(), every);
    }
    // ---- metrics ----
    if tokio::runtime::Handle::try_current().is_ok() {
        metrics::spawn_collector(fleet.clone());
    }
    // ---- warmpool ----
    if cfg.reconcile_interval.is_some() && tokio::runtime::Handle::try_current().is_ok() {
        pools::spawn_reconciler(fleet.clone(), pools::interval());
    }
    if tokio::runtime::Handle::try_current().is_ok() {
        signup::spawn_billing_job(fleet.clone());
    }
    let app = Router::new()
        .route("/console", get(console_html))
        .route("/console/", get(console_html))
        .route("/console/app.js", get(console_js))
        .route("/fleet/ha", get(ha_rt::ha_status))
        .route("/fleet/ha/leader", get(ha_rt::ha_leader_check))
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .route("/fleet/register", post(register))
        .route("/fleet/nodes", get(list_nodes))
        .route("/fleet/nodes/{name}", get(get_node).delete(deregister_node))
        .route("/fleet/nodes/{name}/cordon", post(cordon_node))
        .route("/fleet/nodes/{name}/uncordon", post(uncordon_node))
        .route(
            "/fleet/nodes/{name}/drain",
            post(cloud::drain_node).get(cloud::drain_status),
        )
        .route("/fleet/nodes/{name}/scrub", post(cloud::scrub_node))
        .route("/fleet/rebalance/plan", get(cloud::rebalance_plan))
        .route(
            "/fleet/macos-pool/checkout",
            post(cloud::macos_pool_checkout),
        )
        .route(
            "/fleet/macos-pool/{node}/{id}/return",
            post(cloud::macos_pool_return),
        )
        .route("/fleet/vms", post(create_vm_route).get(list_vms_route))
        .route("/fleet/nodes/{name}/vms", get(node_vms))
        .route(
            "/fleet/vms/{node}/{id}",
            axum::routing::delete(delete_vm_route),
        )
        .route("/fleet/vms/{node}/{id}/exec", post(cloud::exec_vm))
        .route("/fleet/vms/{node}/{id}/input", post(cloud::vm_input))
        .route("/fleet/vms/{node}/{id}/logs", get(cloud::vm_logs))
        .route("/fleet/vms/{node}/{id}/{action}", post(cloud::vm_power))
        .route("/fleet/vms/{node}/{id}/screen", get(audit::screen))
        .route("/fleet/events", get(audit::events))
        .route("/fleet/capacity", get(fleet_capacity))
        .route(
            "/fleet/tenants",
            get(cloud::list_tenants).post(cloud::create_tenant),
        )
        .route("/fleet/tenants/me", get(cloud::tenant_me))
        .route(
            "/fleet/tenants/{id}",
            get(cloud::get_tenant)
                .patch(cloud::update_tenant)
                .delete(cloud::delete_tenant),
        )
        .route(
            "/fleet/prices",
            get(cloud::get_prices).put(cloud::put_prices),
        )
        .route("/fleet/billing", get(cloud::billing))
        .route("/fleet/notifications", get(notify::notifications))
        .route(
            "/fleet/webhooks",
            get(notify::list_webhooks).post(notify::create_webhook),
        )
        .route(
            "/fleet/webhooks/{id}",
            axum::routing::delete(notify::delete_webhook),
        )
        .route("/fleet/webhooks/{id}/test", post(notify::test_webhook))
        .route(
            "/fleet/tenants/{id}/budget",
            axum::routing::put(cloud::put_budget),
        )
        .route(
            "/fleet/tenants/{id}/keys",
            get(cloud::list_keys).post(cloud::create_key),
        )
        .route(
            "/fleet/tenants/{id}/keys/{key}",
            axum::routing::delete(cloud::revoke_key),
        )
        .route(
            "/fleet/tenants/{id}/rotate-token",
            post(cloud::rotate_tenant_token),
        )
        .route(
            "/fleet/leases",
            get(cloud::list_leases).post(cloud::create_lease),
        )
        .route("/fleet/leases/{id}", get(cloud::get_lease))
        .route("/fleet/leases/{id}/release", post(cloud::release_lease))
        .route("/fleet/usage", get(cloud::usage))
        .route(
            "/fleet/overlays",
            get(cloud::list_overlays).post(cloud::create_overlay),
        )
        .route(
            "/fleet/overlays/{name}",
            get(cloud::get_overlay).delete(cloud::delete_overlay),
        )
        .route(
            "/fleet/overlays/{name}/members",
            post(cloud::add_overlay_member),
        )
        .route("/fleet/overlays/{name}/rotate", post(cloud::rotate_overlay))
        .route(
            "/fleet/overlays/{name}/members/{node}",
            axum::routing::delete(cloud::remove_overlay_member),
        )
        .route("/fleet/nodes/{name}/overlays", get(cloud::node_overlays))
        .route("/fleet/images", get(cloud::list_images))
        .route(
            "/fleet/images/{name}",
            axum::routing::put(cloud::put_image).delete(cloud::delete_image),
        )
        .route("/fleet/reconcile", post(cloud::reconcile))
        .route(
            "/fleet/policy",
            get(cloud::get_policy).put(cloud::put_policy),
        )
        // ---- metrics ----
        .route("/fleet/metrics/summary", get(metrics::summary))
        .route("/fleet/metrics/vms", get(metrics::vm_latest))
        .route("/fleet/metrics/vms/{node}/{id}", get(metrics::vm_series))
        .route("/fleet/metrics/nodes/{name}", get(metrics::node_series))
        .route("/fleet/metrics/collect", post(metrics::collect_now))
        // ---- snapshots ----
        .route(
            "/fleet/vms/{node}/{id}/snapshots",
            get(snapshots::list).post(snapshots::create),
        )
        .route(
            "/fleet/vms/{node}/{id}/snapshots/{snap}",
            axum::routing::delete(snapshots::delete),
        )
        .route(
            "/fleet/vms/{node}/{id}/snapshots/{snap}/restore",
            post(snapshots::restore),
        )
        .route("/fleet/vms/{node}/{id}/clone", post(snapshots::clone_vm))
        // ---- volumes ----
        .route(
            "/fleet/volumes",
            get(vols::list_volumes).post(vols::create_volume),
        )
        .route(
            "/fleet/volumes/{name}",
            get(vols::get_volume).delete(vols::delete_volume),
        )
        .route("/fleet/volumes/{name}/resize", post(vols::resize_volume))
        // ---- tunnel ----
        .route("/fleet/vms/{node}/{id}/tunnel", get(tunnel::open_tunnel))
        .route("/fleet/tunnels", get(tunnel::list_tunnels))
        .route(
            "/fleet/tunnels/{id}",
            axum::routing::delete(tunnel::close_tunnel),
        )
        // ---- warmpool ----
        .route(
            "/fleet/pools",
            get(pools::list_pools).post(pools::create_pool),
        )
        .route(
            "/fleet/pools/{name}",
            get(pools::get_pool)
                .patch(pools::patch_pool)
                .delete(pools::delete_pool),
        )
        .route("/fleet/pools/{name}/status", get(pools::pool_status))
        // ---- sso ----
        .route("/auth/config", get(sso::config))
        .route("/auth/login", get(sso::login))
        .route("/auth/callback", get(sso::callback))
        .route("/auth/logout", post(sso::logout))
        .route("/auth/session", get(sso::session))
        .route(
            "/fleet/tenants/{id}/members",
            get(sso::list_members).post(sso::add_member),
        )
        .route(
            "/fleet/tenants/{id}/members/{email}",
            axum::routing::patch(sso::set_member_role).delete(sso::remove_member),
        )
        // ---- signup ----
        .merge(signup::routes())
        .layer(middleware::from_fn_with_state(
            fleet.clone(),
            auth_middleware,
        ))
        // ---- metrics ---- (after the auth layer: `GET /metrics` checks its own tokens, and every request is counted)
        .route("/metrics", get(metrics::prometheus))
        .layer(middleware::from_fn_with_state(
            fleet.clone(),
            metrics::count_requests,
        ))
        .with_state(fleet);
    (app, handle)
}

// ---- the fleet console: two static files, no auth to fetch them (the page asks for a token and calls the API) ----

const CONSOLE_PATHS: [&str; 3] = ["/console", "/console/", "/console/app.js"];
const CONSOLE_HTML: &str = include_str!("fleet_console.html");
const CONSOLE_JS: &str = include_str!("fleet_console.js");
const CONSOLE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; \
     img-src 'self' data: blob:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

fn console_file(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_SECURITY_POLICY, CONSOLE_CSP),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn console_html() -> Response {
    console_file("text/html; charset=utf-8", CONSOLE_HTML)
}

async fn console_js() -> Response {
    console_file("text/javascript; charset=utf-8", CONSOLE_JS)
}

/// Insert/update a node from a heartbeat. A brand-new node starts
/// uncordoned; a node that's already registered keeps whatever `cordoned`
/// state an operator previously set on it — a node agent has no idea
/// cordoning exists and its heartbeat body carries no such field, so a
/// naive overwrite-on-every-beat would silently un-cordon a node the very
/// next time its own agent reported in, seconds after an operator cordoned
/// it for maintenance.
fn apply_register(nodes: &mut HashMap<String, NodeInfo>, req: RegisterRequest) {
    let cordoned = nodes.get(&req.name).map(|n| n.cordoned).unwrap_or(false);
    nodes.insert(
        req.name.clone(),
        NodeInfo {
            name: req.name,
            fluxvm_url: req.fluxvm_url,
            vcpus_total: req.vcpus_total,
            memory_mib_total: req.memory_mib_total,
            vm_count: req.vm_count,
            last_seen: Utc::now(),
            cordoned,
            labels: req.labels,
            security: req.security,
            density: req.density,
            apple: req.apple,
            daemon: req.daemon,
            allocated: req.allocated,
        },
    );
}

async fn register(State(fleet): State<Fleet>, Json(req): Json<RegisterRequest>) -> Json<Value> {
    let mut nodes = fleet.nodes.lock().await;
    apply_register(&mut nodes, req);
    if let (Some(path), Some(lock)) = (&fleet.persist_path, &fleet.lock_path) {
        persist_nodes(path, lock, &nodes);
    }
    Json(json!({"ok": true}))
}

fn node_json(n: &NodeInfo) -> Value {
    json!({
        "name": n.name,
        "fluxvm_url": n.fluxvm_url,
        "vcpus_total": n.vcpus_total,
        "memory_mib_total": n.memory_mib_total,
        "vm_count": n.vm_count,
        "last_seen": n.last_seen,
        "healthy": n.healthy(),
        "cordoned": n.cordoned,
        "labels": n.labels,
        "free_vcpus": n.free_vcpus(),
        "free_memory_mib": n.free_memory_mib(),
        "security": n.security,
        "density": n.density,
        "apple": n.apple,
        "daemon_reachable": n.daemon_reachable(),
        "daemon": n.daemon,
        "allocated": n.allocated,
    })
}

async fn list_nodes(State(fleet): State<Fleet>) -> Json<Value> {
    let nodes = fleet.nodes.lock().await;
    let items: Vec<Value> = nodes.values().map(node_json).collect();
    Json(json!({"items": items}))
}

/// `GET /fleet/nodes/{name}` — exactly one node's own record (the same
/// shape `GET /fleet/nodes`'s `"items"` entries already have), without a
/// caller having to fetch the whole fleet and filter client-side just to
/// check one node's `healthy`/`cordoned`/free-capacity state — the same
/// "answer this one question without needing the rest of the fleet up"
/// motivation `GET /fleet/nodes/{name}/vms` already established for VMs.
/// `404` for a name that was never registered, same status
/// `cordon`/`uncordon`/`deregister` already use for the identical case on
/// this same path segment — unlike `GET /fleet/nodes/{name}/vms`'s `400`,
/// since that route's unknown-name case is a bad *proxy target*, not a
/// missing *resource*.
async fn get_node(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    if let Caller::Tenant(tenant) = &caller {
        return cloud::tenant_node_view(&fleet, tenant, &name)
            .await
            .map(Json);
    }
    let nodes = fleet.nodes.lock().await;
    match nodes.get(&name) {
        Some(n) => Ok(Json(node_json(n))),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no registered node named '{name}'"),
        )),
    }
}

/// Set (or clear) a node's cordoned flag. Returns `None` if no node by
/// that name is registered — cordoning an unregistered name is a no-op
/// error, not a way to pre-seed a node record.
fn set_cordoned(nodes: &mut HashMap<String, NodeInfo>, name: &str, cordoned: bool) -> Option<()> {
    let node = nodes.get_mut(name)?;
    node.cordoned = cordoned;
    Some(())
}

async fn cordon_node(
    State(fleet): State<Fleet>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    set_node_cordoned(fleet, name, true).await
}

async fn uncordon_node(
    State(fleet): State<Fleet>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    set_node_cordoned(fleet, name, false).await
}

async fn set_node_cordoned(
    fleet: Fleet,
    name: String,
    cordoned: bool,
) -> Result<Json<Value>, AppError> {
    let updated = {
        let mut nodes = fleet.nodes.lock().await;
        if set_cordoned(&mut nodes, &name, cordoned).is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("no registered node named '{name}'"),
            ));
        }
        let updated = node_json(nodes.get(&name).expect("just set"));
        if let (Some(path), Some(lock)) = (&fleet.persist_path, &fleet.lock_path) {
            persist_nodes(path, lock, &nodes);
        }
        updated
    };
    // Uncordoning ends a drain too.
    if !cordoned {
        let mut tenancy = fleet.tenancy.lock().await;
        if tenancy.draining.remove(&name) {
            fleet.persist_tenancy(&tenancy);
        }
    }
    Ok(Json(updated))
}

/// Outcome of attempting to permanently remove a node's registry entry.
/// Kept as its own enum (rather than folding into `set_cordoned`'s
/// `Option<()>`) because deregistration has a second failure mode —
/// refusing to touch a node that's still heartbeating — that needs its own
/// HTTP status, not just "found or not".
enum DeregisterOutcome {
    Removed,
    NotFound,
    StillHealthy,
}

/// Remove a node's entry entirely, but only once its heartbeat has already
/// gone stale. Deliberately narrower than cordoning: cordoning suppresses
/// *future* automatic placement while leaving the record (and its
/// `cordoned` flag) intact forever; this erases the record outright, which
/// is only safe once nothing will resurrect it with the wrong state. If
/// the node were still healthy, `apply_register` would treat its very next
/// heartbeat as a brand-new node and re-insert it uncordoned (see
/// `apply_register`'s doc comment) — silently undoing any cordon an
/// operator had set and defeating the maintenance workflow cordon exists
/// for. Requiring staleness first means an operator decommissioning a live
/// host has one unambiguous path: stop that host's `fluxvm-agent node`
/// process (or otherwise let its heartbeat lapse) and wait past
/// `HEALTHY_WINDOW_SECS`, then deregister — never a race between "delete"
/// and "the next heartbeat wins".
fn deregister(nodes: &mut HashMap<String, NodeInfo>, name: &str) -> DeregisterOutcome {
    match nodes.get(name) {
        None => DeregisterOutcome::NotFound,
        Some(n) if n.healthy() => DeregisterOutcome::StillHealthy,
        Some(_) => {
            nodes.remove(name);
            DeregisterOutcome::Removed
        }
    }
}

/// `DELETE /fleet/nodes/{name}` — permanently forget a decommissioned or
/// retired node, so it stops appearing in `GET /fleet/nodes` forever (a
/// stale node otherwise sits in the registry indefinitely, excluded from
/// placement by its own staleness but never actually cleaned up). Returns
/// `404` for a name that was never registered, and `409` for a node whose
/// heartbeat is still fresh — see `deregister`'s doc comment for why a
/// live node can't be deregistered directly.
async fn deregister_node(
    State(fleet): State<Fleet>,
    Path(name): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut nodes = fleet.nodes.lock().await;
    match deregister(&mut nodes, &name) {
        DeregisterOutcome::Removed => {
            if let (Some(path), Some(lock)) = (&fleet.persist_path, &fleet.lock_path) {
                persist_nodes(path, lock, &nodes);
            }
            Ok(StatusCode::NO_CONTENT)
        }
        DeregisterOutcome::NotFound => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no registered node named '{name}'"),
        )),
        DeregisterOutcome::StillHealthy => Err(AppError(
            StatusCode::CONFLICT,
            format!(
                "node '{name}' is still heartbeating; stop its fluxvm-agent node process (or otherwise let its heartbeat lapse) and wait for it to go stale before deregistering, or its next heartbeat will simply re-register it"
            ),
        )),
    }
}

/// Residual-capacity placement: prefer the healthy node with the highest
/// free CPU/memory fraction that can fit the request; tie-break by fewest
/// VMs then name. No label constraint -- equivalent to an empty
/// `nodeSelector`. `#[cfg(test)]`: `resolve_target`/`create_vm` now call
/// `pick_best_capacity_excluding` directly (they always have a selector,
/// even if empty), but this 3-arg convenience form is still handy for
/// tests that don't care about selectors at all.
#[cfg(test)]
fn pick_best_capacity(
    nodes: &HashMap<String, NodeInfo>,
    request_vcpus: u32,
    request_mem: u64,
) -> Option<NodeInfo> {
    pick_best_capacity_excluding(
        nodes,
        request_vcpus,
        request_mem,
        &HashSet::new(),
        &HashMap::new(),
        SecurityProfile::Standard,
    )
}

/// Same as `pick_best_capacity`, but never considers a node whose name is
/// in `exclude`, and only ever considers a node whose labels satisfy
/// `selector` (see `NodeInfo::matches_selector`; an empty selector matches
/// every node). `exclude` is used by `create_vm`'s automatic-placement
/// failover loop: each candidate that turns out to be unreachable gets
/// added to `exclude` so the next iteration picks the next-best
/// *remaining* node instead of picking the same dead one again.
fn pick_best_capacity_excluding(
    nodes: &HashMap<String, NodeInfo>,
    request_vcpus: u32,
    request_mem: u64,
    exclude: &HashSet<String>,
    selector: &HashMap<String, String>,
    profile: SecurityProfile,
) -> Option<NodeInfo> {
    pick_best_capacity_excluding_apple(
        nodes,
        request_vcpus,
        request_mem,
        exclude,
        selector,
        profile,
        None,
    )
}

fn pick_best_capacity_excluding_apple(
    nodes: &HashMap<String, NodeInfo>,
    request_vcpus: u32,
    request_mem: u64,
    exclude: &HashSet<String>,
    selector: &HashMap<String, String>,
    profile: SecurityProfile,
    apple: Option<&ApplePlacementRequest>,
) -> Option<NodeInfo> {
    pick_best_capacity_admitted(
        nodes,
        request_vcpus,
        request_mem,
        exclude,
        selector,
        profile,
        apple,
        &|_| true,
    )
}

/// [`pick_best_capacity_excluding_apple`] plus an extra per-node admission check (tenancy: leases, scrub).
#[allow(clippy::too_many_arguments)]
fn pick_best_capacity_admitted(
    nodes: &HashMap<String, NodeInfo>,
    request_vcpus: u32,
    request_mem: u64,
    exclude: &HashSet<String>,
    selector: &HashMap<String, String>,
    profile: SecurityProfile,
    apple: Option<&ApplePlacementRequest>,
    admit: &dyn Fn(&NodeInfo) -> bool,
) -> Option<NodeInfo> {
    nodes
        .values()
        .filter(|n| !exclude.contains(&n.name))
        .filter(|n| admit(n))
        .filter(|n| n.matches_selector(selector))
        .filter(|n| n.security.supports(profile).is_ok())
        .filter_map(|n| {
            let generic = n.capacity_score(request_vcpus, request_mem)?;
            let ascore = match apple {
                Some(r) => apple_score(n.apple.as_ref()?, r)?,
                None => 0,
            };
            Some(((ascore, generic), n))
        })
        .max_by(|(a, _), (b, _)| {
            a.0.cmp(&b.0)
                .then(a.1.2.cmp(&b.1.2))
                .then(a.1.0.cmp(&b.1.0))
                .then(a.1.1.cmp(&b.1.1))
                .then(a.1.3.cmp(&b.1.3).reverse())
        })
        .map(|(_, n)| n.clone())
}

/// Renders a `nodeSelector` map deterministically (sorted keys) for an
/// error message -- a `HashMap`'s own iteration order is unspecified, and
/// an operator staring at an error needs the same selector spelled the
/// same way every time, not something that shuffles between requests.
fn format_selector(selector: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = selector.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    format!("{{{}}}", pairs.join(", "))
}

/// The error message for "automatic placement found no candidate",
/// distinguishing a plain capacity/health exhaustion from a `nodeSelector`
/// that simply doesn't match any registered node's labels -- otherwise an
/// operator who typo'd a label value sees the exact same unhelpful "no
/// healthy, uncordoned nodes registered" as a genuinely empty fleet.
fn no_candidate_message(selector: &HashMap<String, String>) -> String {
    if selector.is_empty() {
        "no healthy, uncordoned nodes registered".into()
    } else {
        format!(
            "no healthy, uncordoned node matches nodeSelector {}",
            format_selector(selector)
        )
    }
}

/// Pulls `"nodeSelector"` (a flat string->string object, e.g.
/// `{"zone": "us-east"}`) out of a `POST /fleet/vms` body, removing it in
/// the process -- like `"node"`, it's a routing instruction for `central`
/// itself, not part of the `CreateVmRequest` a node's own `fluxctl serve`
/// understands, so it must never be forwarded downstream. A non-object
/// value or a non-string entry is silently ignored rather than rejected,
/// matching `request_sizes`' own lenient-default posture elsewhere in this
/// same handler -- this module treats bodies as opaque JSON with minimal
/// validation by design (see this file's own doc comment).
fn extract_selector(body: &mut Value) -> HashMap<String, String> {
    body.as_object_mut()
        .and_then(|o| o.remove("nodeSelector"))
        .and_then(|v| v.as_object().cloned())
        .map(|obj| {
            obj.into_iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn apple_placement_request(
    body: &Value,
    vcpus: u32,
    memory_mib: u64,
) -> Option<ApplePlacementRequest> {
    if body.get("backend").and_then(Value::as_str) != Some("vz") {
        return None;
    }
    let apple = body.get("apple");
    Some(ApplePlacementRequest {
        vcpus,
        memory_mib,
        macos_guest: apple
            .and_then(|a| a.get("guest_os"))
            .and_then(Value::as_str)
            == Some("macos"),
        needs_nested: apple
            .and_then(|a| a.get("nested_virtualization"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        needs_vmnet: apple
            .and_then(|a| a.get("vmnet"))
            .is_some_and(|v| !v.is_null()),
        needs_custom_virtio: apple
            .and_then(|a| a.get("custom_virtio"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        needs_secure_boot: body.get("secure_boot").and_then(Value::as_bool) == Some(true),
        needs_rosetta: apple
            .and_then(|a| a.get("rosetta"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        bridge_interface: apple
            .and_then(|a| a.get("bridge_interface"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        // A bare name can be a built-in image; a path is the operator's own file and carries no bias.
        image: body
            .get("image")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && !s.contains('/'))
            .map(str::to_owned),
    })
}

fn request_sizes(body: &Value) -> (u32, u64) {
    let vcpus = body
        .get("vcpus")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_VM_VCPUS);
    let mem = body
        .get("memory_mib")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_VM_MEMORY_MIB);
    (vcpus, mem)
}

/// Resolve which node a `POST /fleet/vms` should land on. An explicit
/// `"node"` name bypasses placement entirely — including a cordoned node
/// AND `selector` (`nodeSelector`) — the same way a Kubernetes Pod with
/// `spec.nodeName` set bypasses the scheduler (and its own nodeSelector
/// check) and can still land on a cordoned node: cordoning and
/// `nodeSelector` only ever constrain *automatic* placement's candidate
/// set, neither is a per-node admission check. Automatic (no `"node"`
/// given) placement excludes cordoned and label-mismatched nodes via
/// `pick_best_capacity_excluding` -> `capacity_score`/`matches_selector`
/// regardless of how much free capacity they have.
fn resolve_target(
    nodes: &HashMap<String, NodeInfo>,
    requested_node: Option<String>,
    req_vcpus: u32,
    req_mem: u64,
    selector: &HashMap<String, String>,
    profile: SecurityProfile,
) -> Result<NodeInfo, AppError> {
    match requested_node {
        Some(name) => {
            let node = nodes.get(&name).cloned().ok_or_else(|| {
                AppError(
                    StatusCode::BAD_REQUEST,
                    format!("no registered node named '{name}'"),
                )
            })?;
            node.security.supports(profile).map_err(|e| {
                AppError(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "node '{name}' cannot satisfy security_profile {}: {e}",
                        profile.as_str()
                    ),
                )
            })?;
            Ok(node)
        }
        None => pick_best_capacity_excluding(
            nodes,
            req_vcpus,
            req_mem,
            &HashSet::new(),
            selector,
            profile,
        )
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                no_candidate_message(selector),
            )
        }),
    }
}

fn extract_security_profile(body: &serde_json::Value) -> Result<SecurityProfile, AppError> {
    match body.get("security_profile").and_then(|v| v.as_str()) {
        None | Some("") | Some("standard") => Ok(SecurityProfile::Standard),
        Some(raw) => {
            SecurityProfile::parse_fleet(raw).map_err(|e| AppError(StatusCode::BAD_REQUEST, e))
        }
    }
}

/// Outcome of trying to actually dispatch a create to one candidate node,
/// distinguished so `create_vm`'s automatic-placement loop knows whether
/// it's worth trying the next-best node or whether retrying anywhere would
/// just repeat the same failure.
enum DispatchError {
    /// Couldn't get a well-formed response out of the node at all —
    /// connection refused/reset/timed out, or it answered with a body that
    /// isn't valid JSON. This is exactly the "heartbeat was fresh but the
    /// node is actually down or wedged right now" case
    /// `HEALTHY_WINDOW_SECS` can't catch — the node hasn't missed enough
    /// beats yet to be marked unhealthy. Worth failing over.
    Unreachable(String),
    /// The node answered just fine but its own `fluxctl serve` rejected the
    /// request (e.g. bad `CreateVmRequest` fields). Every other node would
    /// reject the identical body for the identical reason, so failing over
    /// would just waste time re-discovering the same error — surface it
    /// immediately instead.
    Rejected(String),
}

async fn dispatch_create(
    fleet: &Fleet,
    target: &NodeInfo,
    body: &Value,
) -> Result<Value, DispatchError> {
    let resp = fleet
        .http
        .post(format!("{}/v1/vms", target.fluxvm_url))
        .json(body)
        .send()
        .await
        .map_err(|e| DispatchError::Unreachable(e.to_string()))?;
    let status = resp.status();
    let record: Value = resp
        .json()
        .await
        .map_err(|e| DispatchError::Unreachable(e.to_string()))?;
    if !status.is_success() {
        return Err(DispatchError::Rejected(record.to_string()));
    }
    Ok(record)
}

/// `POST /fleet/vms`. Body is a normal `CreateVmRequest` JSON, optionally
/// with a top-level `"node"` field naming an exact node to target, and/or a
/// `"nodeSelector"` object (flat string->string, e.g.
/// `{"zone": "us-east"}`) constraining automatic placement to a node whose
/// labels are a superset of it — when neither is given, residual-capacity
/// placement picks any healthy, uncordoned node. Both fields are stripped
/// before the body is forwarded to a node's own `fluxctl serve`, which knows
/// nothing about either.
///
/// An explicit `"node"` is a single, non-retried attempt that bypasses
/// `"nodeSelector"` entirely, same as it bypasses cordoning — see
/// `resolve_target`'s doc comment — so a caller that pinned a node gets an
/// honest failure if that exact node is down rather than a surprise landing
/// somewhere else.
///
/// Automatic placement (no `"node"`) instead fails over: if the node
/// `pick_best_capacity_excluding` picked turns out to be unreachable — its heartbeat
/// was fresh enough to look healthy, but it's actually down, wedged, or
/// network-partitioned right now — that node is excluded and the
/// next-best remaining candidate is tried, until one accepts the create or
/// every schedulable node has been tried. A node that reaches its own
/// `fluxctl serve` and gets an explicit rejection (bad request fields) is
/// never retried elsewhere, since every other node would reject the same
/// body identically.
///
/// The router serves [`create_vm_route`] (which knows the caller); this operator-only form is what the tests drive.
#[cfg(test)]
async fn create_vm(
    State(fleet): State<Fleet>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    create_vm_inner(&fleet, &Caller::Admin, body, false).await
}

/// The routed `POST /fleet/vms`: `create_vm` for whoever the auth middleware says is calling.
async fn create_vm_route(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    create_vm_inner(&fleet, &caller, body, false).await
}

fn tenancy_error(e: crate::tenancy::TenancyError) -> AppError {
    AppError(
        StatusCode::from_u16(e.status()).unwrap_or(StatusCode::CONFLICT),
        e.to_string(),
    )
}

/// Records a VM central just created in the ledger (quotas, ownership, metering, drain).
#[allow(clippy::too_many_arguments)]
async fn record_created_vm(
    fleet: &Fleet,
    node: &str,
    record: &Value,
    tenant: Option<String>,
    vcpus: u32,
    memory_mib: u64,
    macos_guest: bool,
    spec: Option<Value>,
) {
    let Some(vm_id) = record.get("id").and_then(Value::as_str) else {
        tracing::warn!(
            node,
            "created VM record has no id; not recorded in the ledger"
        );
        return;
    };
    let mut tenancy = fleet.tenancy.lock().await;
    tenancy.record_vm(crate::tenancy::VmEntry {
        node: node.to_owned(),
        vm_id: vm_id.to_owned(),
        name: record
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        tenant,
        vcpus,
        memory_mib,
        macos_guest,
        created_at: Utc::now(),
        ended_at: None,
        spec,
        warm_pool: None,
        claimed_at: None,
        expires_at: None,
        billed_from: None,
    });
    fleet.persist_tenancy(&tenancy);
}

/// `POST /fleet/vms` for `caller`. On top of the placement described on `create_vm`:
///
/// - A tenant's request is stamped with `"tenant": "<id>"` (whatever the body said) and checked against the tenant's
///   quota. An operator may set `"tenant"` to act for a tenant; placement then follows that tenant's leases.
/// - Automatic placement only considers nodes [`TenancyState::admit`] allows (leases, scrub) and skips draining
///   nodes; a tenant naming a node is checked the same way, an operator naming a node is not.
/// - `"reschedulable": true` (stripped before forwarding) keeps the create body in the ledger so a drain can
///   recreate the VM elsewhere. Leave it off for bodies that carry secrets.
/// - `skip_quota` is for a drain recreating a VM the tenant already has.
async fn create_vm_inner(
    fleet: &Fleet,
    caller: &Caller,
    mut body: Value,
    skip_quota: bool,
) -> Result<Json<Value>, AppError> {
    if !body.is_object() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "request body must be a JSON object".into(),
        ));
    }
    // ---- warmpool ---- (a launch a warm pool can answer takes a booted VM; a drain's recreate never does)
    if !skip_quota {
        body = match pools::claim_or_pass(fleet, caller, body).await? {
            pools::Launch::Claimed(v) => return Ok(Json(v)),
            pools::Launch::Create(b) => b,
        };
    }
    let mut requested_node = body
        .as_object_mut()
        .and_then(|o| o.remove("node"))
        .and_then(|v| v.as_str().map(str::to_string));
    let volumes = vols::take_volumes(&mut body)?;
    vols::refuse_tenant_extra_disks(caller, &body)?;
    let selector = extract_selector(&mut body);
    let reschedulable = body
        .as_object_mut()
        .and_then(|o| o.remove("reschedulable"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let tenant: Option<String> = match caller {
        Caller::Tenant(t) => {
            body["tenant"] = json!(t);
            Some(t.clone())
        }
        Caller::Admin => body
            .get("tenant")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
    };

    let (req_vcpus, req_mem) = request_sizes(&body);
    let profile = extract_security_profile(&body)?;

    // Who placement acts for: the tenant (own or named by the operator) once tenancy is on, else the caller. The
    // quota is reserved (not just checked) so parallel creates by one tenant cannot overshoot it.
    let (effective, reservation) = {
        let mut tenancy = fleet.tenancy.lock().await;
        match &tenant {
            Some(t) if tenancy.enabled() => {
                if *caller == Caller::Admin && !tenancy.tenants.contains_key(t) {
                    return Err(AppError(
                        StatusCode::BAD_REQUEST,
                        format!("no tenant '{t}'"),
                    ));
                }
                let reservation = if skip_quota {
                    None
                } else {
                    Some(
                        tenancy
                            .reserve(t, req_vcpus, req_mem)
                            .map_err(tenancy_error)?,
                    )
                };
                if wants_tenant_vmnet(&body) {
                    let had = tenancy.tenants.get(t).and_then(|x| x.network_index);
                    if let Some(net) = tenancy.tenant_vmnet(t) {
                        body["apple"]["vmnet"] = net;
                        if had.is_none() {
                            fleet.persist_tenancy(&tenancy);
                        }
                    }
                }
                (Caller::Tenant(t.clone()), reservation)
            }
            _ => (caller.clone(), None),
        }
    };
    // Released below once placement answered, or on drop if the request is cancelled mid-flight (client gone).
    let mut hold = ReservationHold {
        fleet,
        id: reservation,
    };
    // Volumes pin the launch to their Mac and become extra disks; settled (attached or freed) once placement answered.
    let claim = vols::claim_for_launch(
        fleet,
        &effective,
        &volumes,
        &mut body,
        &mut requested_node,
        reschedulable,
    )
    .await?;
    // After any tenant network was stamped, so placement asks for a vmnet-capable Mac.
    let apple_request = apple_placement_request(&body, req_vcpus, req_mem);
    let macos_guest = apple_request.as_ref().is_some_and(|r| r.macos_guest);
    let placed = place_and_dispatch(
        fleet,
        caller,
        &effective,
        body,
        Placement {
            requested_node,
            selector,
            reschedulable,
            tenant,
            vcpus: req_vcpus,
            memory_mib: req_mem,
            profile,
            apple: apple_request,
            macos_guest,
        },
    )
    .await;
    claim.settle(&placed).await;
    if let Some(id) = hold.id.take() {
        // A successful create is already in the ledger, so the quota stays held without a gap.
        fleet.tenancy.lock().await.release_reservation(id);
    }
    placed
}

/// A quota reservation that is released when dropped unreleased, so a create whose future is dropped (the client
/// disconnected while the node was still answering) does not hold the tenant's quota until central restarts.
struct ReservationHold<'a> {
    fleet: &'a Fleet,
    id: Option<u64>,
}

impl Drop for ReservationHold<'_> {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        if let Ok(mut tenancy) = self.fleet.tenancy.try_lock() {
            tenancy.release_reservation(id);
            return;
        }
        let tenancy = self.fleet.tenancy.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                tenancy.lock().await.release_reservation(id);
            });
        }
    }
}

/// Whether a create should join its tenant's isolated vmnet network: a `vz` VM that did not choose its own network
/// (vmnet, usernet, a bridge, or no network at all).
fn wants_tenant_vmnet(body: &Value) -> bool {
    if body.get("backend").and_then(Value::as_str) != Some("vz") {
        return false;
    }
    if body
        .get("network")
        .and_then(|n| n.get("mode"))
        .and_then(Value::as_str)
        == Some("none")
    {
        return false;
    }
    let apple = body.get("apple");
    // A malformed `apple` (not an object) is left for the node to reject; stamping into it would panic.
    if apple.is_some_and(|a| !a.is_object() && !a.is_null()) {
        return false;
    }
    let set = |k: &str| apple.and_then(|a| a.get(k)).is_some_and(|v| !v.is_null());
    !(set("vmnet") || set("usernet") || set("bridge_interface"))
}

/// What [`create_vm_inner`] worked out about a create before placing it.
struct Placement {
    requested_node: Option<String>,
    selector: HashMap<String, String>,
    reschedulable: bool,
    tenant: Option<String>,
    vcpus: u32,
    memory_mib: u64,
    profile: SecurityProfile,
    apple: Option<ApplePlacementRequest>,
    macos_guest: bool,
}

/// Places a create (named node or automatic, with failover), forwards it, and records it in the ledger.
async fn place_and_dispatch(
    fleet: &Fleet,
    caller: &Caller,
    effective: &Caller,
    body: Value,
    p: Placement,
) -> Result<Json<Value>, AppError> {
    let Placement {
        requested_node,
        selector,
        reschedulable,
        tenant,
        vcpus: req_vcpus,
        memory_mib: req_mem,
        profile,
        apple: apple_request,
        macos_guest,
    } = p;
    let spec = reschedulable.then(|| {
        let mut s = body.clone();
        if !selector.is_empty() {
            s["nodeSelector"] = json!(selector);
        }
        s
    });

    if let Some(name) = requested_node {
        let target = {
            let tenancy = fleet.tenancy.lock().await;
            let nodes = fleet.nodes.lock().await;
            if *caller != Caller::Admin {
                // A tenant naming a node gets the same checks as automatic placement: no cordoned or draining node.
                if tenancy.draining.contains(&name) || nodes.get(&name).is_some_and(|n| n.cordoned)
                {
                    return Err(AppError(
                        StatusCode::CONFLICT,
                        format!("node '{name}' is cordoned or draining"),
                    ));
                }
                tenancy
                    .admit(&name, effective, macos_guest, true)
                    .map_err(tenancy_error)?;
            }
            resolve_target(&nodes, Some(name), req_vcpus, req_mem, &selector, profile)?
        };
        return match dispatch_create(fleet, &target, &body).await {
            Ok(record) => {
                record_created_vm(
                    fleet,
                    &target.name,
                    &record,
                    tenant,
                    req_vcpus,
                    req_mem,
                    macos_guest,
                    spec,
                )
                .await;
                Ok(Json(json!({"node": target.name, "vm": record})))
            }
            Err(DispatchError::Unreachable(msg)) => Err(AppError(StatusCode::BAD_GATEWAY, msg)),
            Err(DispatchError::Rejected(msg)) => Err(AppError(
                StatusCode::BAD_GATEWAY,
                format!("node '{}' rejected create: {msg}", target.name),
            )),
        };
    }

    let mut tried: HashSet<String> = HashSet::new();
    let mut last_unreachable: Option<AppError> = None;
    loop {
        let (target, tenancy_on) = {
            let tenancy = fleet.tenancy.lock().await;
            let nodes = fleet.nodes.lock().await;
            let admit = |n: &NodeInfo| {
                !tenancy.draining.contains(&n.name)
                    && tenancy
                        .admit(&n.name, effective, macos_guest, false)
                        .is_ok()
            };
            (
                pick_best_capacity_admitted(
                    &nodes,
                    req_vcpus,
                    req_mem,
                    &tried,
                    &selector,
                    profile,
                    apple_request.as_ref(),
                    &admit,
                ),
                tenancy.enabled(),
            )
        };
        let Some(target) = target else {
            return Err(last_unreachable.unwrap_or_else(|| {
                let mut msg = no_candidate_message(&selector);
                if tenancy_on && macos_guest && *effective != Caller::Admin {
                    msg.push_str(
                        "; macOS guests run only on Macs the tenant leases: lease one with POST /fleet/leases",
                    );
                }
                AppError(StatusCode::SERVICE_UNAVAILABLE, msg)
            }));
        };
        tried.insert(target.name.clone());
        match dispatch_create(fleet, &target, &body).await {
            Ok(record) => {
                record_created_vm(
                    fleet,
                    &target.name,
                    &record,
                    tenant,
                    req_vcpus,
                    req_mem,
                    macos_guest,
                    spec,
                )
                .await;
                return Ok(Json(json!({"node": target.name, "vm": record})));
            }
            Err(DispatchError::Unreachable(msg)) => {
                tracing::warn!(
                    node = %target.name,
                    error = %msg,
                    "placement candidate unreachable, trying next best node"
                );
                last_unreachable = Some(AppError(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "node '{}' unreachable and no other schedulable node accepted the create: {msg}",
                        target.name
                    ),
                ));
                continue;
            }
            Err(DispatchError::Rejected(msg)) => {
                return Err(AppError(
                    StatusCode::BAD_GATEWAY,
                    format!("node '{}' rejected create: {msg}", target.name),
                ));
            }
        }
    }
}

/// `GET /fleet/vms` — the fleet-wide aggregate. Every node this couldn't
/// account for (excluded up front as unhealthy, or one whose query failed
/// partway through) is named in `"unreachable_nodes"` alongside a short
/// reason, instead of being silently absent from `"items"` with only a
/// server-side `tracing::warn!` the caller never sees — the same
/// surface-don't-hide fix `GET /fleet/nodes/{name}/vms` already applies to
/// the single-node case, extended to the aggregate. A caller that cares
/// whether the list it just got is complete can check this field is empty;
/// one that doesn't can ignore it exactly like before.
///
/// The router serves [`list_vms_route`] (which knows the caller); this operator-only form is what the tests drive.
#[cfg(test)]
async fn list_vms(State(fleet): State<Fleet>) -> Json<Value> {
    list_vms_as(&fleet, &Caller::Admin).await
}

/// The routed `GET /fleet/vms`. A tenant sees only its own VMs: the ones the ledger records as its own, or whose
/// record carries its tenant id.
async fn list_vms_route(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    list_vms_as(&fleet, &caller).await
}

async fn list_vms_as(fleet: &Fleet, caller: &Caller) -> Json<Value> {
    let Json(mut all) = list_vms_all(fleet).await;
    pools::annotate(fleet, &mut all).await; // ---- warmpool ----
    if let Caller::Tenant(t) = caller {
        let tenancy = fleet.tenancy.lock().await;
        if let Some(items) = all.get_mut("items").and_then(Value::as_array_mut) {
            items.retain(|vm| {
                let node = vm.get("node").and_then(Value::as_str).unwrap_or_default();
                let id = vm.get("id").and_then(Value::as_str).unwrap_or_default();
                let stamped = vm
                    .get("request")
                    .and_then(|r| r.get("tenant"))
                    .and_then(Value::as_str)
                    == Some(t.as_str());
                stamped || tenancy.owns(caller, node, id)
            });
        }
        // Other tenants' Macs are none of this tenant's business, and a node's error text can carry its address.
        if let Some(down) = all
            .get_mut("unreachable_nodes")
            .and_then(Value::as_array_mut)
        {
            down.retain(|u| {
                let node = u.get("node").and_then(Value::as_str).unwrap_or_default();
                tenancy
                    .active_lease_for_node(node)
                    .is_some_and(|l| l.tenant == *t)
                    || tenancy.vms.iter().any(|v| {
                        v.active() && v.node == node && v.tenant.as_deref() == Some(t.as_str())
                    })
            });
            for u in down.iter_mut() {
                u["reason"] = json!("did not answer");
            }
        }
    }
    Json(all)
}

async fn list_vms_all(fleet: &Fleet) -> Json<Value> {
    let (targets, mut unreachable): (Vec<NodeInfo>, Vec<Value>) = {
        let nodes = fleet.nodes.lock().await;
        let mut targets = Vec::new();
        let mut unreachable = Vec::new();
        for node in nodes.values() {
            if node.healthy() {
                targets.push(node.clone());
            } else {
                unreachable.push(json!({
                    "node": node.name,
                    "reason": "unhealthy (stale heartbeat)",
                }));
            }
        }
        (targets, unreachable)
    };
    let mut items = Vec::new();
    for node in targets {
        match fleet
            .http
            .get(format!("{}/v1/vms", node.fluxvm_url))
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<Value>().await {
                    Ok(body) => {
                        if !status.is_success() {
                            tracing::warn!(node = %node.name, %status, body = %body, "node rejected fleet-wide list");
                            unreachable.push(json!({
                                "node": node.name,
                                "reason": format!("node rejected list ({status}): {body}"),
                            }));
                            continue;
                        }
                        if let Some(node_items) = body.get("items").and_then(|v| v.as_array()) {
                            for mut vm in node_items.clone() {
                                if let Some(obj) = vm.as_object_mut() {
                                    obj.insert("node".into(), json!(node.name));
                                }
                                items.push(vm);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(node = %node.name, error = %e, "failed to parse node's VM list");
                        unreachable.push(json!({
                            "node": node.name,
                            "reason": format!("failed to parse node's VM list: {e}"),
                        }));
                    }
                }
            }
            Err(e) => {
                tracing::warn!(node = %node.name, error = %e, "failed to reach node for fleet-wide list");
                unreachable.push(json!({
                    "node": node.name,
                    "reason": format!("unreachable: {e}"),
                }));
            }
        }
    }
    Json(json!({"items": items, "unreachable_nodes": unreachable}))
}

/// `GET /fleet/nodes/{name}/vms` — the VMs on exactly one node, queried
/// directly against that node's own `fluxctl serve` rather than the
/// fleet-wide aggregate `GET /fleet/vms` produces. Two reasons this earns
/// its own route instead of leaving callers to filter the fleet-wide list
/// client-side: it works even when *other* nodes in the fleet are
/// unreachable or stale — the fleet-wide list no longer silently drops a
/// node it can't reach (it names every such node in its own
/// `"unreachable_nodes"` field), but still requires every *other* node to
/// answer too just to learn about this one; this route needs none of them
/// — and it surfaces a failure to reach *this* node as a hard `502` instead
/// of a field in an otherwise-200 response the caller has to remember to
/// check. This is the natural "what's actually running here?" check before
/// cordoning a node for maintenance (or before deciding a cordon is safe to
/// lift) — cordoning alone only tells you whether a node *is* excluded from
/// new placement, not what's already on it.
async fn node_vms(
    State(fleet): State<Fleet>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let target = {
        let nodes = fleet.nodes.lock().await;
        nodes.get(&name).cloned().ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                format!("no registered node named '{name}'"),
            )
        })?
    };
    let resp = fleet
        .http
        .get(format!("{}/v1/vms", target.fluxvm_url))
        .send()
        .await?;
    let status = resp.status();
    let body: Value = resp.json().await?;
    if !status.is_success() {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{name}' rejected list: {body}"),
        ));
    }
    let mut items: Vec<Value> = body
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for vm in items.iter_mut() {
        if let Some(obj) = vm.as_object_mut() {
            obj.insert("node".into(), json!(target.name));
        }
    }
    Ok(Json(json!({"node": target.name, "items": items})))
}

/// The routed `DELETE /fleet/vms/{node}/{id}`. A tenant may delete only its own VMs (anything else is a 404, so a
/// tenant learns nothing about other tenants' VMs).
async fn delete_vm_route(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    delete_vm_as(&fleet, &caller, node, id).await
}

async fn delete_vm_as(
    fleet: &Fleet,
    caller: &Caller,
    node: String,
    id: String,
) -> Result<StatusCode, AppError> {
    if !fleet.tenancy.lock().await.owns(caller, &node, &id) {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            format!("no VM '{id}' on node '{node}'"),
        ));
    }
    let status = delete_vm_on_node(fleet, &node, &id).await?;
    let mut tenancy = fleet.tenancy.lock().await;
    if tenancy.end_vm(&node, &id, Utc::now()) {
        fleet.persist_tenancy(&tenancy);
    }
    Ok(status)
}

/// Deletes one VM on one node through that node's own API; ledger bookkeeping is the caller's.
async fn delete_vm_on_node(fleet: &Fleet, node: &str, id: &str) -> Result<StatusCode, AppError> {
    let target = {
        let nodes = fleet.nodes.lock().await;
        nodes.get(node).cloned().ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                format!("no registered node named '{node}'"),
            )
        })?
    };
    let resp = fleet
        .http
        .delete(format!("{}/v1/vms/{}", target.fluxvm_url, id))
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        Ok(StatusCode::NO_CONTENT)
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{node}' rejected delete: {body}"),
        ))
    }
}

/// `GET /fleet/capacity` — a fleet-wide capacity summary, computed purely
/// from each node's own last-reported heartbeat already sitting in the
/// registry (no proxy calls to any node's `fluxctl serve`). Unlike
/// `GET /fleet/vms`, this never blocks on or is degraded by a node being
/// unreachable right now — a stale node is simply excluded from every
/// total below, the same "excluded, not silently guessed" posture
/// `GET /fleet/vms`'s own `unreachable_nodes` field documents for the
/// harder, per-node-proxied aggregate. Answers, in one cheap call, what an
/// operator or an autoscaler would otherwise have to derive by fetching
/// `GET /fleet/nodes` and summing every node's `free_vcpus`/
/// `free_memory_mib` client-side themselves.
///
/// `vcpus_total`/`memory_mib_total` sum every *healthy* node regardless of
/// `cordoned` — cordoned hardware still physically exists and still counts
/// as real fleet capacity, it's just not accepting new placements right
/// now. `vcpus_free`/`memory_mib_free` sum only the *schedulable* subset
/// (healthy AND not cordoned) — exactly the node set `pick_best_capacity`
/// itself draws from for an unaddressed `POST /fleet/vms`, so this free
/// figure answers "would an unaddressed create fit right now" precisely,
/// not approximately. `vcpus_used`/`memory_mib_used` reuse the same
/// per-node capacity estimate `capacity_score` already applies
/// (`vm_count * DEFAULT_VM_VCPUS`/`DEFAULT_VM_MEMORY_MIB`) — an estimate
/// for the same reason it is there: neither central nor this route ever
/// asks a node for each VM's real configured size, only its count.
async fn fleet_capacity(State(fleet): State<Fleet>) -> Json<Value> {
    let nodes = fleet.nodes.lock().await;

    let mut nodes_total = 0usize;
    let mut nodes_healthy = 0usize;
    let mut nodes_cordoned = 0usize;
    let mut nodes_schedulable = 0usize;
    let mut vm_count = 0usize;
    let mut vcpus_total = 0u64;
    let mut memory_mib_total = 0u64;
    let mut vcpus_used = 0u64;
    let mut memory_mib_used = 0u64;
    let mut vcpus_free = 0u64;
    let mut memory_mib_free = 0u64;

    for n in nodes.values() {
        nodes_total += 1;
        if n.cordoned {
            nodes_cordoned += 1;
        }
        if !n.healthy() {
            continue;
        }
        nodes_healthy += 1;
        vm_count += n.vm_count;
        vcpus_total += u64::from(n.vcpus_total);
        memory_mib_total += n.memory_mib_total;
        vcpus_used += u64::from(n.estimated_used_vcpus());
        memory_mib_used += n.estimated_used_memory_mib();
        if n.cordoned {
            continue;
        }
        nodes_schedulable += 1;
        vcpus_free += u64::from(n.free_vcpus());
        memory_mib_free += n.free_memory_mib();
    }

    Json(json!({
        "nodes_total": nodes_total,
        "nodes_healthy": nodes_healthy,
        "nodes_cordoned": nodes_cordoned,
        "nodes_schedulable": nodes_schedulable,
        "vm_count": vm_count,
        "vcpus_total": vcpus_total,
        "vcpus_used": vcpus_used,
        "vcpus_free": vcpus_free,
        "memory_mib_total": memory_mib_total,
        "memory_mib_used": memory_mib_used,
        "memory_mib_free": memory_mib_free,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, vm_count: usize, age_secs: i64) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            fluxvm_url: format!("http://{name}"),
            vcpus_total: 8,
            memory_mib_total: 16384,
            vm_count,
            last_seen: Utc::now() - chrono::Duration::seconds(age_secs),
            cordoned: false,
            labels: HashMap::new(),
            security: NodeSecurityCapabilities::default(),
            density: None,
            daemon: None,
            allocated: None,
            apple: None,
        }
    }

    fn node_under(name: &str, vm_count: usize, level: Option<PressureLevel>) -> NodeInfo {
        NodeInfo {
            density: Some(DensityReport {
                host_pressure_level: level,
                ..DensityReport::default()
            }),
            ..node(name, vm_count, 0)
        }
    }

    #[test]
    fn a_node_that_cannot_reach_its_daemon_takes_no_work_but_is_still_listed() {
        let mut nodes = HashMap::new();
        let mut a = node("a", 0, 0);
        a.daemon = Some(DaemonStatus {
            reachable: false,
            error: Some("connection refused".into()),
        });
        nodes.insert("a".into(), a.clone());
        nodes.insert("b".into(), node("b", 5, 0));
        // `a` is emptier but its daemon is down: `b` is chosen.
        assert_eq!(pick_best_capacity(&nodes, 1, 512).unwrap().name, "b");
        assert!(a.healthy(), "it is still heartbeating, so not stale");
        let j = node_json(&a);
        assert_eq!(j["daemon_reachable"], false);
        assert_eq!(j["daemon"]["error"], "connection refused");
        // An agent that predates the field reads as reachable.
        assert!(node("c", 0, 0).daemon_reachable());
    }

    #[test]
    fn reported_allocation_replaces_the_per_vm_estimate() {
        let mut n = node("a", 4, 0);
        let estimated = (n.free_vcpus(), n.free_memory_mib());
        n.allocated = Some(Allocated {
            vcpus: 1,
            memory_mib: 1024,
        });
        assert_eq!((n.free_vcpus(), n.free_memory_mib()), (7, 15360));
        assert_ne!(estimated, (n.free_vcpus(), n.free_memory_mib()));
        // Four large VMs: a node the estimate called nearly empty is in fact full.
        n.allocated = Some(Allocated {
            vcpus: 8,
            memory_mib: 16384,
        });
        assert_eq!((n.free_vcpus(), n.free_memory_mib()), (0, 0));
    }

    #[test]
    fn placement_skips_critical_pressure_and_prefers_calm_nodes() {
        let mut nodes = HashMap::new();
        // The emptiest node is under critical pressure: never picked.
        nodes.insert(
            "a".into(),
            node_under("a", 0, Some(PressureLevel::Critical)),
        );
        // Warn is picked only when nothing calmer fits, even with more room.
        nodes.insert("b".into(), node_under("b", 1, Some(PressureLevel::Warn)));
        nodes.insert("c".into(), node_under("c", 3, Some(PressureLevel::Normal)));
        assert_eq!(pick_best_capacity(&nodes, 1, 512).unwrap().name, "c");
        nodes.remove("c");
        assert_eq!(pick_best_capacity(&nodes, 1, 512).unwrap().name, "b");
        nodes.remove("b");
        assert!(pick_best_capacity(&nodes, 1, 512).is_none());
        // A node that reports no level (Linux, or an older agent) counts as calm.
        nodes.insert("d".into(), node("d", 2, 0));
        assert_eq!(pick_best_capacity(&nodes, 1, 512).unwrap().name, "d");
    }

    #[test]
    fn heartbeat_density_is_kept_and_optional() {
        let mut nodes = HashMap::new();
        let old: RegisterRequest = serde_json::from_value(json!({
            "name": "m", "fluxvm_url": "http://m", "vcpus_total": 10, "memory_mib_total": 65536, "vm_count": 0,
        }))
        .unwrap();
        apply_register(&mut nodes, old);
        assert!(nodes["m"].density.is_none());
        let new: RegisterRequest = serde_json::from_value(json!({
            "name": "m", "fluxvm_url": "http://m", "vcpus_total": 10, "memory_mib_total": 65536, "vm_count": 0,
            "density": {"warm_slots_ready": 3, "host_pressure_level": "warn"},
        }))
        .unwrap();
        apply_register(&mut nodes, new);
        let d = nodes["m"].density.as_ref().unwrap();
        assert_eq!(d.warm_slots_ready, 3);
        assert_eq!(
            node_json(&nodes["m"])["density"]["host_pressure_level"],
            "warn"
        );
    }

    fn labeled_node(name: &str, vm_count: usize, labels: &[(&str, &str)]) -> NodeInfo {
        NodeInfo {
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..node(name, vm_count, 0)
        }
    }

    fn node_cap(name: &str, vm_count: usize, vcpus: u32, mem: u64) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            fluxvm_url: format!("http://{name}"),
            vcpus_total: vcpus,
            memory_mib_total: mem,
            vm_count,
            last_seen: Utc::now(),
            cordoned: false,
            labels: HashMap::new(),
            security: NodeSecurityCapabilities::default(),
            density: None,
            daemon: None,
            allocated: None,
            apple: None,
        }
    }

    fn node_with_security(name: &str, security: NodeSecurityCapabilities) -> NodeInfo {
        NodeInfo {
            security,
            ..node(name, 0, 0)
        }
    }

    #[test]
    fn picks_node_with_more_residual_capacity() {
        let mut nodes = HashMap::new();
        // a: 1 VM → ~2/8 vCPU used; b: 3 VMs → ~6/8 used
        nodes.insert("a".to_string(), node("a", 1, 0));
        nodes.insert("b".to_string(), node("b", 3, 0));
        let picked = pick_best_capacity(&nodes, 2, 2048).unwrap();
        assert_eq!(picked.name, "a");
    }

    #[test]
    fn prefers_host_that_can_fit_when_other_is_saturated() {
        let mut nodes = HashMap::new();
        // small: 2 vCPU total, already 1 VM (est. 2 used) → can't fit another 2-vCPU request by totals
        // large: plenty of room
        nodes.insert("small".to_string(), node_cap("small", 1, 2, 2048));
        nodes.insert("large".to_string(), node_cap("large", 1, 64, 131072));
        let picked = pick_best_capacity(&nodes, 2, 2048).unwrap();
        assert_eq!(picked.name, "large");
    }

    #[test]
    fn skips_stale_nodes() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "stale".to_string(),
            node("stale", 0, HEALTHY_WINDOW_SECS + 5),
        );
        nodes.insert("fresh".to_string(), node("fresh", 5, 0));
        let picked = pick_best_capacity(&nodes, 2, 2048).unwrap();
        assert_eq!(picked.name, "fresh");
    }

    #[test]
    fn returns_none_when_no_node_is_healthy() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "stale".to_string(),
            node("stale", 0, HEALTHY_WINDOW_SECS + 5),
        );
        assert!(pick_best_capacity(&nodes, 2, 2048).is_none());
    }

    #[test]
    fn ties_break_by_name_for_determinism() {
        let mut nodes = HashMap::new();
        nodes.insert("z".to_string(), node("z", 1, 0));
        nodes.insert("a".to_string(), node("a", 1, 0));
        let picked = pick_best_capacity(&nodes, 2, 2048).unwrap();
        assert_eq!(picked.name, "a");
    }

    #[test]
    fn request_sizes_defaults() {
        assert_eq!(request_sizes(&json!({})), (2, 2048));
        assert_eq!(
            request_sizes(&json!({"vcpus": 4, "memory_mib": 4096})),
            (4, 4096)
        );
    }

    #[test]
    fn cordoned_node_excluded_from_automatic_placement() {
        let mut nodes = HashMap::new();
        let mut a = node("a", 0, 0); // most free capacity, but cordoned
        a.cordoned = true;
        nodes.insert("a".to_string(), a);
        nodes.insert("b".to_string(), node("b", 3, 0));
        let picked = pick_best_capacity(&nodes, 2, 2048).unwrap();
        assert_eq!(picked.name, "b");
    }

    #[test]
    fn cordoning_the_only_node_leaves_nothing_placeable() {
        let mut nodes = HashMap::new();
        let mut a = node("a", 0, 0);
        a.cordoned = true;
        nodes.insert("a".to_string(), a);
        assert!(pick_best_capacity(&nodes, 2, 2048).is_none());
    }

    #[test]
    fn uncordoning_restores_eligibility() {
        let mut nodes = HashMap::new();
        let mut a = node("a", 0, 0);
        a.cordoned = true;
        nodes.insert("a".to_string(), a);
        assert!(pick_best_capacity(&nodes, 2, 2048).is_none());
        set_cordoned(&mut nodes, "a", false).unwrap();
        let picked = pick_best_capacity(&nodes, 2, 2048).unwrap();
        assert_eq!(picked.name, "a");
    }

    #[test]
    fn set_cordoned_reports_none_for_unknown_node() {
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), node("a", 0, 0));
        assert!(set_cordoned(&mut nodes, "nonexistent", true).is_none());
    }

    #[test]
    fn heartbeat_preserves_existing_cordoned_state() {
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), node("a", 0, 0));
        set_cordoned(&mut nodes, "a", true).unwrap();
        assert!(nodes["a"].cordoned);

        // A fresh heartbeat carries no cordon information at all — it must
        // not silently un-cordon the node.
        apply_register(
            &mut nodes,
            RegisterRequest {
                name: "a".into(),
                fluxvm_url: "http://a".into(),
                vcpus_total: 8,
                memory_mib_total: 16384,
                vm_count: 1,
                labels: HashMap::new(),
                security: NodeSecurityCapabilities::default(),
                density: None,
                daemon: None,
                allocated: None,
                apple: None,
            },
        );
        assert!(nodes["a"].cordoned);
        assert_eq!(nodes["a"].vm_count, 1); // other fields still update
    }

    #[test]
    fn first_ever_heartbeat_registers_uncordoned() {
        let mut nodes = HashMap::new();
        apply_register(
            &mut nodes,
            RegisterRequest {
                name: "new".into(),
                fluxvm_url: "http://new".into(),
                vcpus_total: 4,
                memory_mib_total: 8192,
                vm_count: 0,
                labels: HashMap::new(),
                security: NodeSecurityCapabilities::default(),
                density: None,
                daemon: None,
                allocated: None,
                apple: None,
            },
        );
        assert!(!nodes["new"].cordoned);
    }

    #[test]
    fn explicit_node_targeting_bypasses_cordon() {
        let mut nodes = HashMap::new();
        let mut a = node("a", 0, 0);
        a.cordoned = true;
        nodes.insert("a".to_string(), a);
        // Automatic placement finds nothing...
        assert!(pick_best_capacity(&nodes, 2, 2048).is_none());
        // ...but an explicit "node":"a" still resolves, same as a
        // Kubernetes Pod with spec.nodeName bypassing the scheduler.
        let target = resolve_target(
            &nodes,
            Some("a".to_string()),
            2,
            2048,
            &HashMap::new(),
            SecurityProfile::Standard,
        )
        .unwrap();
        assert_eq!(target.name, "a");
    }

    #[test]
    fn explicit_node_targeting_unknown_name_still_errors() {
        let nodes: HashMap<String, NodeInfo> = HashMap::new();
        let err = resolve_target(
            &nodes,
            Some("ghost".to_string()),
            2,
            2048,
            &HashMap::new(),
            SecurityProfile::Standard,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn automatic_placement_skips_cordoned_even_with_more_capacity() {
        let mut nodes = HashMap::new();
        let mut roomy = node_cap("roomy", 0, 64, 131072);
        roomy.cordoned = true;
        nodes.insert("roomy".to_string(), roomy);
        nodes.insert("tight".to_string(), node_cap("tight", 1, 4, 8192));
        let target = resolve_target(
            &nodes,
            None,
            2,
            2048,
            &HashMap::new(),
            SecurityProfile::Standard,
        )
        .unwrap();
        assert_eq!(target.name, "tight");
    }

    #[test]
    fn node_selector_restricts_automatic_placement_to_matching_labels() {
        let mut nodes = HashMap::new();
        // "roomy" has far more free capacity but the wrong zone label.
        let mut roomy = labeled_node("roomy", 0, &[("zone", "us-west")]);
        roomy.vcpus_total = 64;
        roomy.memory_mib_total = 131072;
        nodes.insert("roomy".to_string(), roomy);
        nodes.insert(
            "tight".to_string(),
            labeled_node("tight", 0, &[("zone", "us-east")]),
        );
        let selector: HashMap<String, String> = [("zone".to_string(), "us-east".to_string())]
            .into_iter()
            .collect();
        let target =
            resolve_target(&nodes, None, 2, 2048, &selector, SecurityProfile::Standard).unwrap();
        assert_eq!(target.name, "tight");
    }

    #[test]
    fn node_selector_matching_no_node_is_a_clear_error() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            labeled_node("a", 0, &[("zone", "us-east")]),
        );
        let selector: HashMap<String, String> = [("gpu".to_string(), "true".to_string())]
            .into_iter()
            .collect();
        let err = resolve_target(&nodes, None, 2, 2048, &selector, SecurityProfile::Standard)
            .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.contains("nodeSelector"), "message was: {}", err.1);
        assert!(err.1.contains("gpu=true"), "message was: {}", err.1);
    }

    #[test]
    fn empty_selector_matches_any_node() {
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), node("a", 0, 0));
        assert!(nodes["a"].matches_selector(&HashMap::new()));
    }

    #[test]
    fn explicit_node_targeting_bypasses_a_non_matching_selector() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            labeled_node("a", 0, &[("zone", "us-east")]),
        );
        let selector: HashMap<String, String> = [("zone".to_string(), "us-west".to_string())]
            .into_iter()
            .collect();
        // "a" doesn't match the selector at all, but naming it explicitly
        // still resolves -- same bypass as an explicit node vs. cordoning.
        let target = resolve_target(
            &nodes,
            Some("a".to_string()),
            2,
            2048,
            &selector,
            SecurityProfile::Standard,
        )
        .unwrap();
        assert_eq!(target.name, "a");
    }

    #[test]
    fn explicit_node_targeting_still_rejects_ineligible_confidential_profile() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            node_with_security("a", NodeSecurityCapabilities::default()),
        );
        let err = resolve_target(
            &nodes,
            Some("a".to_string()),
            2,
            2048,
            &HashMap::new(),
            SecurityProfile::ConfidentialSnp,
        )
        .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("confidential-snp"));
    }

    #[test]
    fn automatic_placement_skips_nodes_that_cannot_satisfy_snp() {
        let mut nodes = HashMap::new();
        let snp_cap = NodeSecurityCapabilities {
            snp: true,
            ..Default::default()
        };
        nodes.insert("snp".to_string(), node_with_security("snp", snp_cap));
        nodes.insert(
            "plain".to_string(),
            node_with_security("plain", NodeSecurityCapabilities::default()),
        );
        let target = resolve_target(
            &nodes,
            None,
            2,
            2048,
            &HashMap::new(),
            SecurityProfile::ConfidentialSnp,
        )
        .unwrap();
        assert_eq!(target.name, "snp");
    }

    #[test]
    fn extract_selector_pulls_flat_string_map_and_strips_it_from_body() {
        let mut body = json!({"vcpus": 2, "nodeSelector": {"zone": "us-east", "gpu": "true"}});
        let selector = extract_selector(&mut body);
        assert_eq!(selector.get("zone"), Some(&"us-east".to_string()));
        assert_eq!(selector.get("gpu"), Some(&"true".to_string()));
        assert!(body.get("nodeSelector").is_none());
        assert_eq!(body["vcpus"], 2); // everything else untouched
    }

    #[test]
    fn extract_selector_defaults_to_empty_when_absent_or_malformed() {
        assert!(extract_selector(&mut json!({})).is_empty());
        assert!(extract_selector(&mut json!({"nodeSelector": "not-an-object"})).is_empty());
        // Non-string values are dropped rather than rejected.
        let selector = extract_selector(&mut json!({"nodeSelector": {"a": 1, "b": "ok"}}));
        assert_eq!(selector.len(), 1);
        assert_eq!(selector.get("b"), Some(&"ok".to_string()));
    }

    // --- DELETE /fleet/nodes/{name} ---

    #[test]
    fn deregister_removes_a_stale_node() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "stale".to_string(),
            node("stale", 0, HEALTHY_WINDOW_SECS + 5),
        );
        assert!(matches!(
            deregister(&mut nodes, "stale"),
            DeregisterOutcome::Removed
        ));
        assert!(!nodes.contains_key("stale"));
    }

    #[test]
    fn deregister_refuses_a_healthy_node() {
        let mut nodes = HashMap::new();
        nodes.insert("fresh".to_string(), node("fresh", 0, 0));
        assert!(matches!(
            deregister(&mut nodes, "fresh"),
            DeregisterOutcome::StillHealthy
        ));
        // Refused — the record must still be there afterward.
        assert!(nodes.contains_key("fresh"));
    }

    #[test]
    fn deregister_unknown_node_is_not_found() {
        let mut nodes: HashMap<String, NodeInfo> = HashMap::new();
        assert!(matches!(
            deregister(&mut nodes, "ghost"),
            DeregisterOutcome::NotFound
        ));
    }

    #[test]
    fn deregister_then_new_heartbeat_recreates_uncordoned() {
        // Documents the exact hazard `deregister` refusing a healthy node
        // guards against: once a record is actually gone, the next
        // heartbeat for that name is indistinguishable from a brand-new
        // node and re-registers uncordoned, regardless of what the erased
        // record's `cordoned` flag used to say.
        let mut nodes = HashMap::new();
        let mut a = node("a", 0, HEALTHY_WINDOW_SECS + 5);
        a.cordoned = true;
        nodes.insert("a".to_string(), a);
        assert!(matches!(
            deregister(&mut nodes, "a"),
            DeregisterOutcome::Removed
        ));
        apply_register(
            &mut nodes,
            RegisterRequest {
                name: "a".into(),
                fluxvm_url: "http://a".into(),
                vcpus_total: 8,
                memory_mib_total: 16384,
                vm_count: 0,
                labels: HashMap::new(),
                security: NodeSecurityCapabilities::default(),
                density: None,
                daemon: None,
                allocated: None,
                apple: None,
            },
        );
        assert!(!nodes["a"].cordoned);
    }

    #[tokio::test]
    async fn deregister_node_http_removes_a_stale_node() {
        let mut nodes = HashMap::new();
        nodes.insert(
            "stale".to_string(),
            node("stale", 0, HEALTHY_WINDOW_SECS + 5),
        );
        let fleet = test_fleet(nodes);
        let status = deregister_node(State(fleet.clone()), Path("stale".to_string()))
            .await
            .unwrap();
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!fleet.nodes.lock().await.contains_key("stale"));
    }

    #[tokio::test]
    async fn deregister_node_http_conflicts_on_a_healthy_node() {
        let mut nodes = HashMap::new();
        nodes.insert("fresh".to_string(), node("fresh", 0, 0));
        let fleet = test_fleet(nodes);
        let err = deregister_node(State(fleet.clone()), Path("fresh".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(fleet.nodes.lock().await.contains_key("fresh"));
    }

    #[tokio::test]
    async fn deregister_node_http_not_found_for_unknown_node() {
        let fleet = test_fleet(HashMap::new());
        let err = deregister_node(State(fleet), Path("ghost".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    // --- GET /fleet/nodes/{name} ---

    #[tokio::test]
    async fn get_node_returns_the_named_nodes_own_record() {
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), node("a", 3, 0));
        nodes.insert("b".to_string(), node("b", 0, 0));
        let fleet = test_fleet(nodes);
        let Json(body) = get_node(
            State(fleet),
            Extension(Caller::Admin),
            Path("a".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(body["name"], "a");
        assert_eq!(body["vm_count"], 3);
        assert_eq!(body["healthy"], true);
        assert_eq!(body["cordoned"], false);
    }

    #[tokio::test]
    async fn get_node_not_found_for_unknown_node() {
        let fleet = test_fleet(HashMap::new());
        let err = get_node(
            State(fleet),
            Extension(Caller::Admin),
            Path("ghost".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(err.1.contains("ghost"));
    }

    // --- GET /fleet/nodes/{name}/vms ---

    fn test_fleet(nodes: HashMap<String, NodeInfo>) -> Fleet {
        Fleet {
            nodes: Arc::new(Mutex::new(nodes)),
            http: reqwest::Client::new(),
            persist_path: None,
            lock_path: None,
            token: None,
            tenancy: Arc::new(Mutex::new(TenancyState::default())),
            tenancy_path: None,
            ha: None,
            audit: Arc::new(audit::AuditLog::memory()),
            notify: Arc::default(),
            metrics: Arc::default(),
            tunnels: Arc::default(),
            sso: Arc::default(),
            signup: Arc::default(),
        }
    }

    /// [`spawn_fake_create_node`] for the cloud module's tests.
    pub(super) async fn spawn_fake_create_node_pub(
        status: StatusCode,
        body: Value,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        spawn_fake_create_node(status, body).await
    }

    /// Spawns a real, minimal HTTP server on an OS-assigned loopback port
    /// that answers `GET /v1/vms` with a fixed body/status, so `node_vms`
    /// (which proxies via a real `reqwest` call, not something mockable
    /// in-process) has a real node to talk to. Returns the `http://` base
    /// URL to hand to a `NodeInfo.fluxvm_url`.
    async fn spawn_fake_node(status: StatusCode, body: Value) -> String {
        let app = Router::new().route(
            "/v1/vms",
            get(move || {
                let body = body.clone();
                async move { (status, Json(body)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    /// Like `spawn_fake_node`, but answers `POST /v1/vms` (for `create_vm`
    /// dispatch tests) and counts how many times it was hit. The counter is
    /// what lets a test prove a node was *never contacted* — the only way
    /// to show create_vm's failover loop stopped after a rejection instead
    /// of trying every other node too.
    async fn spawn_fake_create_node(
        status: StatusCode,
        body: Value,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_for_route = hits.clone();
        let app = Router::new().route(
            "/v1/vms",
            post(move |Json(_req): Json<Value>| {
                let body = body.clone();
                let hits = hits_for_route.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (status, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    #[tokio::test]
    async fn node_vms_returns_only_that_nodes_vms_tagged_with_its_name() {
        let url = spawn_fake_node(
            StatusCode::OK,
            json!({"items": [{"id": "vm-1"}, {"id": "vm-2"}]}),
        )
        .await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: url,
                ..node("a", 2, 0)
            },
        );
        nodes.insert("b".to_string(), node("b", 5, 0)); // never contacted
        let fleet = test_fleet(nodes);

        let Json(resp) = node_vms(State(fleet), Path("a".to_string())).await.unwrap();
        assert_eq!(resp["node"], "a");
        let items = resp["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], "vm-1");
        assert_eq!(items[0]["node"], "a");
        assert_eq!(items[1]["node"], "a");
    }

    #[tokio::test]
    async fn node_vms_unknown_node_is_bad_request() {
        let fleet = test_fleet(HashMap::new());
        let err = node_vms(State(fleet), Path("ghost".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn node_vms_surfaces_unreachable_node_as_an_error_instead_of_hiding_it() {
        // Bind then immediately drop a listener: nothing is listening on
        // this port any more, so a connection to it fails fast — unlike
        // the fleet-wide list, which would just warn server-side and
        // silently omit this node.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);

        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: format!("http://{dead_addr}"),
                ..node("a", 1, 0)
            },
        );
        let fleet = test_fleet(nodes);
        let err = node_vms(State(fleet), Path("a".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn node_vms_propagates_the_nodes_own_error_status() {
        let url = spawn_fake_node(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "local fluxctl serve is unhappy"}),
        )
        .await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: url,
                ..node("a", 1, 0)
            },
        );
        let fleet = test_fleet(nodes);
        let err = node_vms(State(fleet), Path("a".to_string()))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(err.1.contains("local fluxctl serve is unhappy"));
    }

    #[tokio::test]
    async fn node_vms_handles_a_node_with_no_vms() {
        let url = spawn_fake_node(StatusCode::OK, json!({"items": []})).await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: url,
                ..node("a", 0, 0)
            },
        );
        let fleet = test_fleet(nodes);
        let Json(resp) = node_vms(State(fleet), Path("a".to_string())).await.unwrap();
        assert_eq!(resp["items"].as_array().unwrap().len(), 0);
    }

    // --- GET /fleet/vms (fleet-wide aggregate) ---

    #[tokio::test]
    async fn list_vms_all_healthy_and_reachable_reports_no_unreachable_nodes() {
        let url = spawn_fake_node(StatusCode::OK, json!({"items": [{"id": "vm-1"}]})).await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: url,
                ..node("a", 1, 0)
            },
        );
        let fleet = test_fleet(nodes);
        let Json(resp) = list_vms(State(fleet)).await;
        let items = resp["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["node"], "a");
        assert_eq!(resp["unreachable_nodes"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_vms_names_a_node_it_cannot_connect_to_instead_of_silently_dropping_it() {
        // Same dead-listener trick as node_vms's unreachable test: bind then
        // drop, so nothing answers and the connection fails fast.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);

        let good_url = spawn_fake_node(StatusCode::OK, json!({"items": [{"id": "vm-ok"}]})).await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "dead".to_string(),
            NodeInfo {
                fluxvm_url: format!("http://{dead_addr}"),
                ..node("dead", 3, 0)
            },
        );
        nodes.insert(
            "up".to_string(),
            NodeInfo {
                fluxvm_url: good_url,
                ..node("up", 1, 0)
            },
        );
        let fleet = test_fleet(nodes);
        let Json(resp) = list_vms(State(fleet)).await;

        // The reachable node's VM still comes through...
        let items = resp["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "vm-ok");

        // ...and the unreachable one is named, not just missing.
        let unreachable = resp["unreachable_nodes"].as_array().unwrap();
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0]["node"], "dead");
        assert!(
            unreachable[0]["reason"]
                .as_str()
                .unwrap()
                .contains("unreachable")
        );
    }

    #[tokio::test]
    async fn list_vms_names_a_stale_heartbeat_node_as_unreachable_without_contacting_it() {
        // 300s old is well past HEALTHY_WINDOW_SECS (30s) — excluded from
        // placement the same way, but previously vanished from the
        // fleet-wide list with no trace at all.
        let mut nodes = HashMap::new();
        nodes.insert("stale".to_string(), node("stale", 2, 300));
        let fleet = test_fleet(nodes);
        let Json(resp) = list_vms(State(fleet)).await;

        assert_eq!(resp["items"].as_array().unwrap().len(), 0);
        let unreachable = resp["unreachable_nodes"].as_array().unwrap();
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0]["node"], "stale");
        assert!(
            unreachable[0]["reason"]
                .as_str()
                .unwrap()
                .contains("unhealthy")
        );
    }

    #[tokio::test]
    async fn list_vms_names_a_node_that_rejects_the_list_call_as_unreachable() {
        let url = spawn_fake_node(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "local fluxctl serve is unhappy"}),
        )
        .await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "cranky".to_string(),
            NodeInfo {
                fluxvm_url: url,
                ..node("cranky", 1, 0)
            },
        );
        let fleet = test_fleet(nodes);
        let Json(resp) = list_vms(State(fleet)).await;

        assert_eq!(resp["items"].as_array().unwrap().len(), 0);
        let unreachable = resp["unreachable_nodes"].as_array().unwrap();
        assert_eq!(unreachable.len(), 1);
        assert_eq!(unreachable[0]["node"], "cranky");
        assert!(
            unreachable[0]["reason"]
                .as_str()
                .unwrap()
                .contains("local fluxctl serve is unhappy")
        );
    }

    // --- GET /fleet/capacity ---

    #[tokio::test]
    async fn capacity_is_all_zero_for_an_empty_fleet() {
        let fleet = test_fleet(HashMap::new());
        let Json(resp) = fleet_capacity(State(fleet)).await;
        assert_eq!(resp["nodes_total"], 0);
        assert_eq!(resp["nodes_healthy"], 0);
        assert_eq!(resp["nodes_schedulable"], 0);
        assert_eq!(resp["vcpus_total"], 0);
        assert_eq!(resp["vcpus_free"], 0);
        assert_eq!(resp["memory_mib_total"], 0);
        assert_eq!(resp["memory_mib_free"], 0);
    }

    #[tokio::test]
    async fn capacity_sums_healthy_uncordoned_nodes() {
        // a: 8 vCPU/16Gi, 1 VM (~2 used, 6 free); b: 8 vCPU/16Gi, 3 VMs (~6 used, 2 free)
        let mut nodes = HashMap::new();
        nodes.insert("a".to_string(), node("a", 1, 0));
        nodes.insert("b".to_string(), node("b", 3, 0));
        let fleet = test_fleet(nodes);
        let Json(resp) = fleet_capacity(State(fleet)).await;

        assert_eq!(resp["nodes_total"], 2);
        assert_eq!(resp["nodes_healthy"], 2);
        assert_eq!(resp["nodes_schedulable"], 2);
        assert_eq!(resp["nodes_cordoned"], 0);
        assert_eq!(resp["vm_count"], 4);
        assert_eq!(resp["vcpus_total"], 16);
        assert_eq!(resp["memory_mib_total"], 32768);
        assert_eq!(resp["vcpus_used"], 8); // (1+3) VMs * DEFAULT_VM_VCPUS(2)
        assert_eq!(resp["memory_mib_used"], 8192); // (1+3) VMs * 2048
        assert_eq!(resp["vcpus_free"], 8);
        assert_eq!(resp["memory_mib_free"], 24576);
    }

    #[tokio::test]
    async fn capacity_counts_a_stale_node_toward_nothing() {
        // Well past HEALTHY_WINDOW_SECS (30s) -- excluded from every total,
        // not just from placement, exactly like GET /fleet/vms's own
        // unreachable_nodes handling excludes it from "items".
        let mut nodes = HashMap::new();
        nodes.insert("fresh".to_string(), node("fresh", 1, 0));
        nodes.insert("stale".to_string(), node("stale", 5, 300));
        let fleet = test_fleet(nodes);
        let Json(resp) = fleet_capacity(State(fleet)).await;

        assert_eq!(resp["nodes_total"], 2);
        assert_eq!(resp["nodes_healthy"], 1);
        assert_eq!(resp["vm_count"], 1); // stale node's 5 VMs are not counted
        assert_eq!(resp["vcpus_total"], 8);
        assert_eq!(resp["memory_mib_total"], 16384);
    }

    #[tokio::test]
    async fn capacity_reports_cordoned_hardware_as_real_but_unschedulable() {
        // Cordoned hardware still exists (counts toward *_total) but is
        // excluded from pick_best_capacity's own candidate set, so it must
        // also be excluded from *_free -- otherwise this endpoint would
        // promise free capacity an unaddressed create could never actually
        // land on.
        let mut nodes = HashMap::new();
        let mut a = node("a", 0, 0);
        a.cordoned = true;
        nodes.insert("a".to_string(), a);
        nodes.insert("b".to_string(), node("b", 0, 0));
        let fleet = test_fleet(nodes);
        let Json(resp) = fleet_capacity(State(fleet)).await;

        assert_eq!(resp["nodes_total"], 2);
        assert_eq!(resp["nodes_healthy"], 2);
        assert_eq!(resp["nodes_cordoned"], 1);
        assert_eq!(resp["nodes_schedulable"], 1);
        // Both nodes' hardware counts toward the fleet's real totals...
        assert_eq!(resp["vcpus_total"], 16);
        assert_eq!(resp["memory_mib_total"], 32768);
        // ...but only "b" (uncordoned) contributes to what's actually free.
        assert_eq!(resp["vcpus_free"], 8);
        assert_eq!(resp["memory_mib_free"], 16384);
    }

    // --- POST /fleet/vms (create) ---

    #[tokio::test]
    async fn automatic_placement_fails_over_when_the_best_pick_is_unreachable() {
        // "roomy" has more free capacity (0 VMs) than "tight" (2 VMs) so
        // pick_best_capacity would choose it first -- but nothing is
        // listening on its port. Automatic placement must notice the
        // dead connection and land on "tight" instead of just failing.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);

        let (good_url, hits) = spawn_fake_create_node(StatusCode::OK, json!({"id": "vm-1"})).await;

        let mut nodes = HashMap::new();
        nodes.insert(
            "roomy".to_string(),
            NodeInfo {
                fluxvm_url: format!("http://{dead_addr}"),
                ..node("roomy", 0, 0)
            },
        );
        nodes.insert(
            "tight".to_string(),
            NodeInfo {
                fluxvm_url: good_url,
                ..node("tight", 2, 0)
            },
        );
        let fleet = test_fleet(nodes);

        let Json(resp) = create_vm(State(fleet), Json(json!({})))
            .await
            .expect("should fail over to the reachable node");
        assert_eq!(resp["node"], "tight");
        assert_eq!(resp["vm"]["id"], "vm-1");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn automatic_placement_errors_once_every_candidate_is_unreachable() {
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead1 = l1.local_addr().unwrap();
        drop(l1);
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead2 = l2.local_addr().unwrap();
        drop(l2);

        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: format!("http://{dead1}"),
                ..node("a", 0, 0)
            },
        );
        nodes.insert(
            "b".to_string(),
            NodeInfo {
                fluxvm_url: format!("http://{dead2}"),
                ..node("b", 0, 0)
            },
        );
        let fleet = test_fleet(nodes);

        let err = create_vm(State(fleet), Json(json!({}))).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(err.1.contains("unreachable"));
    }

    #[tokio::test]
    async fn automatic_placement_does_not_fail_over_on_an_explicit_rejection() {
        // "picked" has more free capacity so it's chosen first. It answers
        // (unlike the unreachable case) but its own fluxctl serve rejects
        // the body -- every other node would reject the identical body
        // identically, so "spare" must never even be contacted.
        let (picked_url, picked_hits) = spawn_fake_create_node(
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid disk size"}),
        )
        .await;
        let (spare_url, spare_hits) =
            spawn_fake_create_node(StatusCode::OK, json!({"id": "vm-1"})).await;

        let mut nodes = HashMap::new();
        nodes.insert(
            "picked".to_string(),
            NodeInfo {
                fluxvm_url: picked_url,
                ..node("picked", 0, 0)
            },
        );
        nodes.insert(
            "spare".to_string(),
            NodeInfo {
                fluxvm_url: spare_url,
                ..node("spare", 2, 0)
            },
        );
        let fleet = test_fleet(nodes);

        let err = create_vm(State(fleet), Json(json!({}))).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert!(err.1.contains("invalid disk size"));
        assert_eq!(picked_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(spare_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn explicit_node_target_does_not_fail_over_when_that_node_is_down() {
        // Pinning "node":"a" bypasses the scheduler entirely -- even though
        // "b" is up and would happily accept the create, an explicit
        // target must fail honestly rather than silently landing elsewhere.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = listener.local_addr().unwrap();
        drop(listener);
        let (good_url, hits) = spawn_fake_create_node(StatusCode::OK, json!({"id": "vm-1"})).await;

        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: format!("http://{dead_addr}"),
                ..node("a", 0, 0)
            },
        );
        nodes.insert(
            "b".to_string(),
            NodeInfo {
                fluxvm_url: good_url,
                ..node("b", 0, 0)
            },
        );
        let fleet = test_fleet(nodes);

        let err = create_vm(State(fleet), Json(json!({"node": "a"})))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_GATEWAY);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn create_vm_routes_to_the_node_matching_node_selector() {
        // "roomy" has vastly more free capacity, but only "picky" carries
        // the requested label -- the selector must win over raw capacity.
        let (roomy_url, roomy_hits) =
            spawn_fake_create_node(StatusCode::OK, json!({"id": "vm-1"})).await;
        let (picky_url, picky_hits) =
            spawn_fake_create_node(StatusCode::OK, json!({"id": "vm-2"})).await;

        let mut nodes = HashMap::new();
        nodes.insert(
            "roomy".to_string(),
            NodeInfo {
                fluxvm_url: roomy_url,
                vcpus_total: 64,
                memory_mib_total: 131072,
                ..node("roomy", 0, 0)
            },
        );
        nodes.insert(
            "picky".to_string(),
            NodeInfo {
                fluxvm_url: picky_url,
                ..labeled_node("picky", 0, &[("gpu", "true")])
            },
        );
        let fleet = test_fleet(nodes);

        let Json(resp) = create_vm(State(fleet), Json(json!({"nodeSelector": {"gpu": "true"}})))
            .await
            .unwrap();
        assert_eq!(resp["node"], "picky");
        assert_eq!(picky_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(roomy_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn create_vm_with_unmatched_node_selector_fails_without_contacting_any_node() {
        let (url, hits) = spawn_fake_create_node(StatusCode::OK, json!({"id": "vm-1"})).await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: url,
                ..labeled_node("a", 0, &[("zone", "us-east")])
            },
        );
        let fleet = test_fleet(nodes);

        let err = create_vm(
            State(fleet),
            Json(json!({"nodeSelector": {"zone": "us-west"}})),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.contains("nodeSelector"), "message was: {}", err.1);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn create_vm_strips_node_selector_before_forwarding_to_the_node() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_for_route = hits.clone();
        let app = Router::new().route(
            "/v1/vms",
            post(move |Json(req): Json<Value>| {
                let hits = hits_for_route.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // A real `fluxctl serve` would reject an unknown field --
                    // asserting it's simply absent proves central() actually
                    // strips "nodeSelector" rather than forwarding it as-is.
                    assert!(req.get("nodeSelector").is_none());
                    assert_eq!(req["name"], "test-vm");
                    (StatusCode::OK, Json(json!({"id": "vm-1"})))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: format!("http://{addr}"),
                ..labeled_node("a", 0, &[("zone", "us-east")])
            },
        );
        let fleet = test_fleet(nodes);

        let Json(resp) = create_vm(
            State(fleet),
            Json(json!({"name": "test-vm", "nodeSelector": {"zone": "us-east"}})),
        )
        .await
        .unwrap();
        assert_eq!(resp["node"], "a");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn create_vm_explicit_node_ignores_a_non_matching_node_selector() {
        let (url, hits) = spawn_fake_create_node(StatusCode::OK, json!({"id": "vm-1"})).await;
        let mut nodes = HashMap::new();
        nodes.insert(
            "a".to_string(),
            NodeInfo {
                fluxvm_url: url,
                ..labeled_node("a", 0, &[("zone", "us-east")])
            },
        );
        let fleet = test_fleet(nodes);

        // "node":"a" bypasses placement -- an unrelated nodeSelector must
        // never block it, same as it never blocks a cordoned target.
        let Json(resp) = create_vm(
            State(fleet),
            Json(json!({"node": "a", "nodeSelector": {"zone": "us-west"}})),
        )
        .await
        .unwrap();
        assert_eq!(resp["node"], "a");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn apple_node(name: &str, vm_count: usize, caps: AppleHostCaps) -> NodeInfo {
        NodeInfo {
            apple: Some(caps),
            ..node(name, vm_count, 0)
        }
    }

    fn caps(free_cpu: u32, free_memory_mib: u64) -> AppleHostCaps {
        AppleHostCaps {
            free_cpu,
            free_memory_mib,
            ..AppleHostCaps::default()
        }
    }

    #[test]
    fn apple_request_carries_a_bare_image_name_but_not_a_path() {
        let named = json!({"backend": "vz", "image": "debian-13"});
        let r = apple_placement_request(&named, 1, 512).unwrap();
        assert_eq!(r.image.as_deref(), Some("debian-13"));
        let path = json!({"backend": "vz", "image": "/Users/x/disk.raw"});
        assert_eq!(apple_placement_request(&path, 1, 512).unwrap().image, None);
        // A node that already has the image is preferred by the apple score.
        let has = AppleHostCaps {
            images: vec!["debian-13".into()],
            ..caps(8, 16 * 1024)
        };
        let lacks = caps(8, 16 * 1024);
        assert!(apple_score(&has, &r).unwrap() > apple_score(&lacks, &r).unwrap());
    }

    #[test]
    fn apple_request_only_exists_for_vz_and_reads_the_apple_options() {
        let linux = json!({"backend": "qemu", "apple": {"custom_virtio": true}});
        assert!(apple_placement_request(&linux, 1, 512).is_none());
        let no_backend = json!({"vcpus": 1});
        assert!(apple_placement_request(&no_backend, 1, 512).is_none());

        let plain = apple_placement_request(&json!({"backend": "vz"}), 2, 1024).unwrap();
        assert_eq!((plain.vcpus, plain.memory_mib), (2, 1024));
        assert!(!plain.macos_guest && !plain.needs_nested && !plain.needs_vmnet);
        assert!(!plain.needs_custom_virtio && plain.bridge_interface.is_none());

        let full = apple_placement_request(
            &json!({"backend": "vz", "apple": {
                "guest_os": "macos", "nested_virtualization": true,
                "vmnet": {"mode": "shared"}, "custom_virtio": true,
                "bridge_interface": "en0"}}),
            4,
            8192,
        )
        .unwrap();
        assert!(full.macos_guest && full.needs_nested && full.needs_vmnet);
        assert!(full.needs_custom_virtio);
        assert_eq!(full.bridge_interface.as_deref(), Some("en0"));

        // A null vmnet is "not asked for".
        let null_vmnet =
            apple_placement_request(&json!({"backend": "vz", "apple": {"vmnet": null}}), 1, 512)
                .unwrap();
        assert!(!null_vmnet.needs_vmnet);
    }

    #[test]
    fn vz_placement_skips_nodes_without_apple_capabilities() {
        let mut nodes = HashMap::new();
        // A big Linux node (no `apple`) and a small Mac: only the Mac may take a vz VM.
        nodes.insert("linux".into(), node("linux", 0, 0));
        nodes.insert("mac".into(), apple_node("mac", 3, caps(8, 8192)));
        let req = apple_placement_request(&json!({"backend": "vz"}), 1, 512).unwrap();
        let pick = |req: Option<&ApplePlacementRequest>| {
            pick_best_capacity_excluding_apple(
                &nodes,
                1,
                512,
                &HashSet::new(),
                &HashMap::new(),
                SecurityProfile::default(),
                req,
            )
            .map(|n| n.name)
        };
        assert_eq!(pick(Some(&req)).as_deref(), Some("mac"));
        // Without an Apple request (any other backend) the Linux node is still eligible.
        assert!(pick(None).is_some());
    }

    #[test]
    fn vz_placement_requires_the_features_the_request_needs() {
        let mut nodes = HashMap::new();
        let mut cv = caps(8, 8192);
        cv.custom_virtio = true;
        nodes.insert("capable".into(), apple_node("capable", 5, cv));
        // Far more room, but no custom Virtio.
        nodes.insert("roomy".into(), apple_node("roomy", 0, caps(64, 262144)));
        let pick = |body: Value| {
            let req = apple_placement_request(&body, 1, 512).unwrap();
            pick_best_capacity_excluding_apple(
                &nodes,
                1,
                512,
                &HashSet::new(),
                &HashMap::new(),
                SecurityProfile::default(),
                Some(&req),
            )
            .map(|n| n.name)
        };
        assert_eq!(
            pick(json!({"backend": "vz", "apple": {"custom_virtio": true}})).as_deref(),
            Some("capable")
        );
        // Nothing offers nested virtualization: no placement at all.
        assert!(pick(json!({"backend": "vz", "apple": {"nested_virtualization": true}})).is_none());
        // A bridge interface the hosts do not have.
        assert!(pick(json!({"backend": "vz", "apple": {"bridge_interface": "en9"}})).is_none());
    }

    #[test]
    fn vz_placement_enforces_the_two_macos_guest_limit() {
        let mut nodes = HashMap::new();
        let mut full = caps(8, 16384);
        full.macos_guests = 2;
        nodes.insert("full".into(), apple_node("full", 2, full));
        let mut one = caps(8, 16384);
        one.macos_guests = 1;
        nodes.insert("one".into(), apple_node("one", 1, one));
        let req = apple_placement_request(
            &json!({"backend": "vz", "apple": {"guest_os": "macos"}}),
            2,
            4096,
        )
        .unwrap();
        let picked = pick_best_capacity_excluding_apple(
            &nodes,
            2,
            4096,
            &HashSet::new(),
            &HashMap::new(),
            SecurityProfile::default(),
            Some(&req),
        )
        .map(|n| n.name);
        assert_eq!(picked.as_deref(), Some("one"));
        nodes.remove("one");
        assert!(
            pick_best_capacity_excluding_apple(
                &nodes,
                2,
                4096,
                &HashSet::new(),
                &HashMap::new(),
                SecurityProfile::default(),
                Some(&req),
            )
            .is_none()
        );
    }
}
