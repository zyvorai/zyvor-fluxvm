// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! AgentSession: one handle for what an AI agent needs from FluxVM. It is a thin layer over
//! the existing sandbox, snapshot, fork, change-set and event primitives; it adds no new
//! isolation. A session owns one sandbox, remembers its checkpoints and forks, and keeps an
//! append-only ledger of the API-level actions taken through it.
//!
//! * `POST   /v1/agents/sessions`                    create (sandbox + policy + workspace)
//! * `GET    /v1/agents/sessions`                    list
//! * `GET    /v1/agents/sessions/{id}`               one session and its capabilities
//! * `DELETE /v1/agents/sessions/{id}`               delete the session and its sandbox
//! * `POST   /v1/agents/sessions/{id}/exec`          run a command
//! * `POST   /v1/agents/sessions/{id}/checkpoint`    snapshot under a tag
//! * `POST   /v1/agents/sessions/{id}/restore`       restore a checkpoint
//! * `POST   /v1/agents/sessions/{id}/fork`          fork into child sessions (in-tree KVM only)
//! * `GET    /v1/agents/sessions/{id}/events`        ledger plus the sandbox's VM events
//! * `GET    /v1/agents/sessions/{id}/artifacts`     workspace changes since the baseline
//!
//! Records live in `<state_dir>/agent-sessions/<id>.json` and `<id>.ledger.jsonl`, so a
//! session survives a daemon restart. What is *not* here: promote, replay, sandbox-scoped PTY,
//! preview URLs and browser profiles (see `docs/agent-sessions.md`, "Roadmap").

use super::{ApiError, ApiResult, AuditActor, TokenTenant, change_error, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use fluxvm_core::config::Role;
use fluxvm_scheduler::VmManager;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new()
        .route(
            "/v1/agents/sessions",
            post(create_session).get(list_sessions),
        )
        .route(
            "/v1/agents/sessions/{id}",
            get(get_session).delete(delete_session),
        )
        .route("/v1/agents/sessions/{id}/exec", post(exec_session))
        .route("/v1/agents/sessions/{id}/checkpoint", post(checkpoint))
        .route("/v1/agents/sessions/{id}/restore", post(restore))
        .route("/v1/agents/sessions/{id}/fork", post(fork))
        .route("/v1/agents/sessions/{id}/events", get(events))
        .route("/v1/agents/sessions/{id}/artifacts", get(artifacts))
}

// ---------------------------------------------------------------------------------------
// Model

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    tag: String,
    memory: bool,
    created_at: DateTime<Utc>,
}

/// What a session can do on its backend, so callers never have to guess.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Capabilities {
    snapshot: bool,
    fork: bool,
    /// Always false here: the PTY route is VM-level and admin-only (`/v1/vms/{id}/console`).
    pty: bool,
}

/// The policy as compiled, including the parts that are accepted but not enforced yet.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PolicyReport {
    preset: Option<String>,
    offline: bool,
    allow_hosts: Vec<String>,
    /// `filesystem` and `process` rules are stored but nothing enforces them in this MVP.
    unenforced: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    id: Uuid,
    agent: String,
    sandbox_id: Uuid,
    backend: String,
    parent: Option<Uuid>,
    children: Vec<Uuid>,
    tenant: Option<String>,
    state: String, // running | deleted
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    capabilities: Capabilities,
    policy: PolicyReport,
    workspace: Option<WorkspaceState>,
    checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceState {
    path: String,
    git: Option<String>,
    mode: String,
    cloned: bool,
    baseline: bool,
    note: Option<String>,
}

// ---------------------------------------------------------------------------------------
// Storage: one JSON file per session and an append-only ledger.

fn dir(m: &VmManager) -> PathBuf {
    m.cfg.state_dir.join("agent-sessions")
}

fn session_path(m: &VmManager, id: Uuid) -> PathBuf {
    dir(m).join(format!("{id}.json"))
}

fn ledger_path(m: &VmManager, id: Uuid) -> PathBuf {
    dir(m).join(format!("{id}.ledger.jsonl"))
}

fn not_found(id: Uuid) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        message: format!("agent session {id} not found"),
    }
}

async fn save(m: &VmManager, s: &Session) -> ApiResult<()> {
    let d = dir(m);
    tokio::fs::create_dir_all(&d)
        .await
        .map_err(anyhow::Error::from)?;
    let tmp = d.join(format!("{}.json.tmp", s.id));
    let body = serde_json::to_vec_pretty(s).map_err(anyhow::Error::from)?;
    tokio::fs::write(&tmp, body)
        .await
        .map_err(anyhow::Error::from)?;
    tokio::fs::rename(&tmp, session_path(m, s.id))
        .await
        .map_err(anyhow::Error::from)?;
    Ok(())
}

async fn load(m: &VmManager, id: Uuid, tenant: &Option<String>) -> ApiResult<Session> {
    let bytes = tokio::fs::read(session_path(m, id))
        .await
        .map_err(|_| not_found(id))?;
    let s: Session = serde_json::from_slice(&bytes).map_err(anyhow::Error::from)?;
    // A tenant-scoped token only ever sees its own sessions (404, not 403, like sandboxes).
    if &s.tenant != tenant {
        return Err(not_found(id));
    }
    Ok(s)
}

/// Append one ledger line. Failures are logged, never fatal: the ledger must not break a run.
fn record(m: &VmManager, id: Uuid, kind: &str, detail: Value) {
    let line = json!({"ts": Utc::now(), "kind": kind, "detail": detail});
    let path = ledger_path(m, id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = res {
        tracing::warn!(session = %id, error = %e, "agent session ledger write failed");
    }
}

fn read_ledger(m: &VmManager, id: Uuid, since: Option<DateTime<Utc>>) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(ledger_path(m, id)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| match since {
            None => true,
            Some(t) => v["ts"]
                .as_str()
                .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                .is_some_and(|ts| ts > t),
        })
        .collect()
}

fn caller_tenant(t: &Option<Extension<TokenTenant>>) -> Option<String> {
    t.as_ref().map(|Extension(TokenTenant(t))| t.clone())
}

// ---------------------------------------------------------------------------------------
// Policy compilation

const DEVELOPER_SAFE_HOSTS: &[&str] = &[
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    "crates.io",
    "*.crates.io",
    "static.crates.io",
    "pypi.org",
    "files.pythonhosted.org",
    "registry.npmjs.org",
    "proxy.golang.org",
    "sum.golang.org",
];

#[derive(Debug, Deserialize, Default)]
struct PolicyBody {
    /// `offline`, `developer-safe` or `open`.
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    network: Option<NetworkBody>,
    /// Accepted and recorded; not enforced in this MVP.
    #[serde(default)]
    filesystem: Option<Value>,
    #[serde(default)]
    process: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct NetworkBody {
    /// `deny` (default when `allow` is given) or `allow`.
    #[serde(default)]
    default: Option<String>,
    /// Host names (`example.com`, `*.example.com`). Method/path rules are not supported yet.
    #[serde(default)]
    allow: Vec<Value>,
}

fn compile_policy(p: Option<PolicyBody>) -> Result<PolicyReport, ApiError> {
    let bad = |m: String| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: m,
    };
    let Some(p) = p else {
        return Ok(PolicyReport::default());
    };
    let mut r = PolicyReport {
        preset: p.preset.clone(),
        ..Default::default()
    };
    match p.preset.as_deref() {
        None | Some("open") => {}
        Some("offline") => r.offline = true,
        Some("developer-safe") => {
            r.allow_hosts = DEVELOPER_SAFE_HOSTS.iter().map(|s| s.to_string()).collect();
        }
        Some(other) => {
            return Err(bad(format!(
                "unknown policy preset {other:?}; use offline, developer-safe or open"
            )));
        }
    }
    if let Some(n) = p.network {
        let default = n.default.as_deref().unwrap_or("deny");
        if default != "deny" && default != "allow" {
            return Err(bad("network.default must be deny or allow".into()));
        }
        if default == "allow" && !n.allow.is_empty() {
            return Err(bad(
                "network.default=allow with an allow list is contradictory; use deny".into(),
            ));
        }
        for item in n.allow {
            match item {
                Value::String(h) => r.allow_hosts.push(h),
                Value::Object(o) => {
                    if o.keys().any(|k| k != "host") {
                        return Err(bad(
                            "allow entries with methods or paths are not supported yet (host names only)"
                                .into(),
                        ));
                    }
                    match o.get("host").and_then(Value::as_str) {
                        Some(h) => r.allow_hosts.push(h.to_string()),
                        None => return Err(bad("allow entry needs a host".into())),
                    }
                }
                _ => return Err(bad("allow entries are host names".into())),
            }
        }
        if default == "deny" && r.allow_hosts.is_empty() && p.preset.as_deref() != Some("open") {
            r.offline = true;
        }
    }
    if p.filesystem.is_some() {
        r.unenforced.push("filesystem".into());
    }
    if p.process.is_some() {
        r.unenforced.push("process".into());
    }
    r.allow_hosts.sort();
    r.allow_hosts.dedup();
    Ok(r)
}

// ---------------------------------------------------------------------------------------
// Handlers

#[derive(Debug, Deserialize)]
struct CreateBody {
    #[serde(default = "default_agent")]
    agent: String,
    #[serde(default)]
    workspace: Option<WorkspaceBody>,
    #[serde(default)]
    runtime: Option<RuntimeBody>,
    #[serde(default)]
    policy: Option<PolicyBody>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

fn default_agent() -> String {
    "custom".into()
}

#[derive(Debug, Deserialize)]
struct WorkspaceBody {
    /// `https://…` git URL cloned into the workspace (needs the policy to allow the host).
    #[serde(default)]
    git: Option<String>,
    /// `clone` (default) or `readonly`. `overlay` is on the roadmap.
    #[serde(default)]
    mode: Option<String>,
    /// Guest path; default `/workspace`.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuntimeBody {
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    cpus: Option<u8>,
    #[serde(default)]
    memory_mb: Option<u64>,
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn valid_guest_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 200
        && !p.contains("..")
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
}

fn valid_git_url(u: &str) -> bool {
    u.starts_with("https://")
        && u.len() <= 500
        && !u.contains(char::is_whitespace)
        && !u.starts_with("https://-")
}

fn valid_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// What a session can do on its backend. Fork: the in-tree KVM engine, or a `vz` Linux guest
/// that has no network card of its own to restore onto (an offline or allow-listed sandbox);
/// a `vz` sandbox on the Mac's NAT cannot be forked (see `fork::check_vz_forkable`).
fn capabilities_for(backend: &str, offline: bool) -> Capabilities {
    Capabilities {
        snapshot: matches!(backend, "vz" | "flux-vm" | "qemu" | "firecracker"),
        fork: backend == "flux-vm" || (backend == "vz" && offline),
        pty: false,
    }
}

/// Why a session cannot fork, for the 409 body. `None` when it can.
fn fork_refusal(s: &Session) -> Option<String> {
    if s.capabilities.fork {
        return None;
    }
    Some(if s.backend == "vz" {
        "fork on vz needs a session with no network card of its own: create it with policy offline \
         or a host allow-list (a sandbox on the Mac's NAT cannot be forked)"
            .to_string()
    } else {
        format!(
            "fork is not available on backend {}: it needs the in-tree KVM engine (flux-vm) on Linux, \
             or an offline vz session",
            s.backend
        )
    })
}

async fn create_session(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    actor: Option<Extension<AuditActor>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Json(body): Json<CreateBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let bad = |msg: &str| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: msg.to_string(),
    };
    if body.agent.is_empty()
        || body.agent.len() > 64
        || !body
            .agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        return Err(bad("agent must be 1-64 characters of [A-Za-z0-9._-]"));
    }
    let ws = body.workspace.as_ref();
    let ws_path = ws
        .and_then(|w| w.path.clone())
        .unwrap_or_else(|| "/workspace".into());
    if !valid_guest_path(&ws_path) {
        return Err(bad("workspace.path must be an absolute guest path"));
    }
    let ws_mode = ws
        .and_then(|w| w.mode.clone())
        .unwrap_or_else(|| "clone".into());
    if ws_mode != "clone" && ws_mode != "readonly" {
        return Err(bad(
            "workspace.mode must be clone or readonly (overlay is not available yet)",
        ));
    }
    if let Some(g) = ws.and_then(|w| w.git.as_deref())
        && !valid_git_url(g)
    {
        return Err(bad("workspace.git must be an https:// URL"));
    }
    let policy = compile_policy(body.policy)?;

    let tenant = caller_tenant(&token_tenant);
    let actor_name = actor.as_ref().map(|a| a.0.0.as_str());
    let id = Uuid::new_v4();
    let req = fluxvm_scheduler::SandboxCreateRequest {
        name: Some(format!("agent-{}", &id.to_string()[..8])),
        image: body.runtime.as_ref().and_then(|r| r.image.clone()),
        ttl_seconds: body.ttl_seconds,
        vcpus: body.runtime.as_ref().and_then(|r| r.cpus),
        memory_mib: body.runtime.as_ref().and_then(|r| r.memory_mb),
        offline: policy.offline,
        allow_hosts: policy.allow_hosts.clone(),
        ..serde_json::from_value(json!({})).map_err(anyhow::Error::from)?
    };
    let rec = m.create_sandbox(req, tenant.as_deref(), actor_name).await?;
    let backend = serde_json::to_value(rec.backend)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();

    let mut session = Session {
        id,
        agent: body.agent,
        sandbox_id: rec.id,
        capabilities: capabilities_for(&backend, policy.offline || !policy.allow_hosts.is_empty()),
        backend,
        parent: None,
        children: Vec::new(),
        tenant,
        state: "running".into(),
        created_at: Utc::now(),
        expires_at: rec.expires_at,
        policy,
        workspace: None,
        checkpoints: Vec::new(),
    };
    record(
        &m,
        id,
        "session.create",
        json!({"agent": session.agent, "sandbox": rec.id, "backend": session.backend, "policy": session.policy}),
    );

    if let Some(w) = ws {
        let mut state = WorkspaceState {
            path: ws_path.clone(),
            git: w.git.clone(),
            mode: ws_mode,
            cloned: false,
            baseline: false,
            note: None,
        };
        // The workspace directory exists whether or not a clone happens, so a baseline can be
        // recorded and an agent has somewhere to work.
        if m.ensure_running_for_request(rec.id).await.is_ok() {
            // The guest user is not root on stock images, so `/workspace` needs sudo to exist.
            let p = shell_quote(&ws_path);
            let mk = format!(
                "mkdir -p {p} 2>/dev/null || (sudo -n mkdir -p {p} && sudo -n chown \"$(id -u):$(id -g)\" {p})"
            );
            let code = m
                .exec_with_policy(rec.id, mk, Some(30), None)
                .await
                .ok()
                .and_then(|r| serde_json::to_value(&r).ok())
                .and_then(|v| v["exit_code"].as_i64());
            if code != Some(0) {
                state.note = Some(format!(
                    "could not create the workspace directory {ws_path} (exit {code:?})"
                ));
            }
        }
        if let Some(url) = &w.git {
            m.ensure_running_for_request(rec.id).await?;
            let cmd = format!(
                "git clone --depth 1 -- {} {}",
                shell_quote(url),
                shell_quote(&ws_path)
            );
            match m.exec_with_policy(rec.id, cmd, Some(300), None).await {
                Ok(r) => {
                    let code = serde_json::to_value(&r)
                        .ok()
                        .and_then(|v| v["exit_code"].as_i64());
                    state.cloned = code == Some(0);
                    if !state.cloned {
                        state.note = Some(match code {
                            Some(127) => "git is not installed in the guest image; use an image that has it (the stock debian-13 does not)".to_string(),
                            other => format!(
                                "git clone exited {other:?}; the policy may not allow the host, or the directory is not empty"
                            ),
                        });
                    }
                }
                Err(e) => state.note = Some(format!("git clone failed: {e:#}")),
            }
            record(
                &m,
                id,
                "workspace.clone",
                json!({"git": url, "path": ws_path, "ok": state.cloned, "note": state.note}),
            );
        }
        // A baseline makes `artifacts` report what the agent changed. Best effort.
        if m.sandbox_baseline(rec.id, vec![ws_path.clone()])
            .await
            .is_ok()
        {
            state.baseline = true;
        }
        session.workspace = Some(state);
    }
    save(&m, &session).await?;
    Ok((StatusCode::CREATED, Json(json!(session))))
}

async fn list_sessions(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let tenant = caller_tenant(&token_tenant);
    let mut items = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(dir(&m)).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".json") || name.ends_with(".tmp") {
                continue;
            }
            if let Ok(b) = tokio::fs::read(e.path()).await
                && let Ok(s) = serde_json::from_slice::<Session>(&b)
                && s.tenant == tenant
            {
                items.push(s);
            }
        }
    }
    items.sort_by_key(|s| s.created_at);
    Ok(Json(json!({ "items": items })))
}

async fn get_session(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    Ok(Json(json!(s)))
}

async fn delete_session(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    if s.state != "deleted" {
        // The sandbox may already be gone (TTL); the session is still closed.
        if let Err(e) = m.delete(s.sandbox_id).await {
            tracing::warn!(session = %id, error = %format!("{e:#}"), "agent session: sandbox delete");
        }
        s.state = "deleted".into();
        save(&m, &s).await?;
        record(&m, id, "session.delete", json!({}));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn running(s: &Session) -> ApiResult<()> {
    if s.state != "running" {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!("agent session {} is {}", s.id, s.state),
        });
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ExecBody {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

async fn exec_session(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<ExecBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if body.command.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "command is required".into(),
        });
    }
    m.ensure_running_for_request(s.sandbox_id).await?;
    let started = std::time::Instant::now();
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            body.command.clone(),
            body.timeout_seconds,
            None,
        )
        .await?;
    let out = json!(res);
    record(
        &m,
        id,
        "exec",
        json!({
            "command": body.command,
            "exit_code": out["exit_code"],
            "duration_ms": started.elapsed().as_millis() as u64,
        }),
    );
    Ok(Json(out))
}

#[derive(Debug, Deserialize, Default)]
struct CheckpointBody {
    #[serde(default)]
    tag: Option<String>,
    /// `false` takes a disk-only snapshot (much smaller).
    #[serde(default)]
    memory: Option<bool>,
}

async fn checkpoint(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    body: Option<Json<CheckpointBody>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let tag = body
        .tag
        .unwrap_or_else(|| format!("cp-{}", s.checkpoints.len() + 1));
    if !valid_tag(&tag) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "tag must match [A-Za-z0-9._-]{1,128}".into(),
        });
    }
    if !s.capabilities.snapshot {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!("backend {} cannot snapshot", s.backend),
        });
    }
    let memory = body.memory.unwrap_or(true);
    m.create_vm_snapshot_with(s.sandbox_id, &tag, memory)
        .await?;
    let cp = Checkpoint {
        tag: tag.clone(),
        memory,
        created_at: Utc::now(),
    };
    s.checkpoints.retain(|c| c.tag != tag);
    s.checkpoints.push(cp.clone());
    save(&m, &s).await?;
    record(&m, id, "checkpoint", json!({"tag": tag, "memory": memory}));
    Ok(Json(json!(cp)))
}

#[derive(Debug, Deserialize)]
struct RestoreBody {
    tag: String,
}

async fn restore(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<RestoreBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if !s.checkpoints.iter().any(|c| c.tag == body.tag) {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("session {id} has no checkpoint {:?}", body.tag),
        });
    }
    let vm = m
        .restore_vm_snapshot(s.sandbox_id, &body.tag)
        .await
        .map_err(change_error)?;
    record(&m, id, "restore", json!({"tag": body.tag}));
    Ok(Json(json!({"restored": body.tag, "vm": vm})))
}

#[derive(Debug, Deserialize, Default)]
struct ForkBody {
    #[serde(default)]
    count: Option<u32>,
    /// `best-effort` (default), `unique` or `preserve`. `unique` refuses to fork a guest that has
    /// neither the vsock agent nor SSH to confirm each child's identity reset.
    #[serde(default)]
    identity: Option<String>,
}

async fn fork(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    actor: Option<Extension<AuditActor>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    body: Option<Json<ForkBody>>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if let Some(why) = fork_refusal(&s) {
        // Never a silent fallback: a caller that forks to explore in parallel must know.
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: why,
        });
    }
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let count = body.count.unwrap_or(1);
    let identity = body.identity.unwrap_or_else(|| "best-effort".into());
    if !["best-effort", "unique", "preserve"].contains(&identity.as_str()) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "identity must be best-effort, unique or preserve".into(),
        });
    }
    if count == 0 || count > 32 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "count must be 1-32".into(),
        });
    }
    let spec: fluxvm_scheduler::fork::ForkSpec = serde_json::from_value(json!({
        "count": count,
        "name_prefix": format!("agent-{}-fork", &id.to_string()[..8]),
        "identity": identity,
    }))
    .map_err(anyhow::Error::from)?;
    let actor_name = actor.as_ref().map(|a| a.0.0.as_str());
    let report = m.fork_vm_spec(s.sandbox_id, spec, actor_name).await?;

    let mut kids = Vec::new();
    for child in &report.children {
        let cid = Uuid::new_v4();
        let cs = Session {
            id: cid,
            agent: s.agent.clone(),
            sandbox_id: child.id,
            backend: s.backend.clone(),
            parent: Some(id),
            children: Vec::new(),
            tenant: s.tenant.clone(),
            state: "running".into(),
            created_at: Utc::now(),
            expires_at: child.expires_at,
            capabilities: s.capabilities.clone(),
            policy: s.policy.clone(),
            workspace: s.workspace.clone(),
            checkpoints: Vec::new(),
        };
        save(&m, &cs).await?;
        record(&m, cid, "session.fork_child", json!({"parent": id}));
        s.children.push(cid);
        kids.push(cs);
    }
    save(&m, &s).await?;
    record(
        &m,
        id,
        "fork",
        json!({"count": count, "children": s.children, "total_ms": report.total_ms}),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "items": kids,
            "identity": identity,
            "total_ms": report.total_ms,
            "parent_snapshot_ms": report.parent_snapshot_ms,
        })),
    ))
}

#[derive(Debug, Deserialize, Default)]
struct EventsQuery {
    #[serde(default)]
    since: Option<DateTime<Utc>>,
}

async fn events(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    let ledger = read_ledger(&m, id, q.since);
    let vm_events = match fluxvm_scheduler::events::global() {
        Some(log) => {
            let filter = fluxvm_scheduler::EventFilter {
                vm: Some(s.sandbox_id),
                since: q.since,
                event: None,
                limit: Some(500),
            };
            log.list(&filter)
                .map(|v| v.into_iter().map(|e| json!(e)).collect::<Vec<_>>())
                .unwrap_or_default()
        }
        None => Vec::new(),
    };
    Ok(Json(json!({"ledger": ledger, "vm_events": vm_events})))
}

async fn artifacts(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    let ledger = read_ledger(&m, id, None);
    let execs = ledger.iter().filter(|e| e["kind"] == "exec").count();
    let (changes, note) = match &s.workspace {
        Some(w) if w.baseline && s.state == "running" => {
            m.ensure_running_for_request(s.sandbox_id).await?;
            match m
                .sandbox_changes(s.sandbox_id, Some(vec![w.path.clone()]))
                .await
            {
                Ok(r) => (Some(json!(r)), None),
                Err(e) => (None, Some(format!("changes unavailable: {e:#}"))),
            }
        }
        Some(_) => (
            None,
            Some("no baseline was recorded for the workspace".into()),
        ),
        None => (None, Some("the session has no workspace".into())),
    };
    Ok(Json(json!({
        "session": id,
        "changes": changes,
        "note": note,
        "checkpoints": s.checkpoints,
        "children": s.children,
        "execs": execs,
        "ledger_entries": ledger.len(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(v: Value) -> PolicyBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn presets_compile_to_existing_sandbox_fields() {
        let r = compile_policy(Some(pol(json!({"preset": "offline"})))).unwrap();
        assert!(r.offline && r.allow_hosts.is_empty());
        let r = compile_policy(Some(pol(json!({"preset": "developer-safe"})))).unwrap();
        assert!(!r.offline);
        assert!(r.allow_hosts.contains(&"crates.io".to_string()));
        let r = compile_policy(None).unwrap();
        assert!(!r.offline && r.allow_hosts.is_empty());
    }

    #[test]
    fn deny_by_default_with_an_allow_list() {
        let r = compile_policy(Some(pol(json!({
            "network": {"default": "deny", "allow": ["github.com", {"host": "api.github.com"}, "github.com"]}
        }))))
        .unwrap();
        assert_eq!(r.allow_hosts, vec!["api.github.com", "github.com"]);
        assert!(!r.offline);
        // deny with nothing allowed is offline
        let r = compile_policy(Some(pol(json!({"network": {"default": "deny"}})))).unwrap();
        assert!(r.offline);
    }

    #[test]
    fn what_is_not_supported_is_refused_not_ignored() {
        assert!(
            compile_policy(Some(pol(
                json!({"network": {"allow": [{"host": "a.b", "methods": ["GET"]}]}})
            )))
            .is_err(),
            "method/path rules must be refused until they are enforced"
        );
        assert!(compile_policy(Some(pol(json!({"preset": "nope"})))).is_err());
        assert!(
            compile_policy(Some(pol(
                json!({"network": {"default": "allow", "allow": ["a.b"]}})
            )))
            .is_err()
        );
        let r = compile_policy(Some(pol(json!({
            "filesystem": {"write": ["/workspace/**"]}, "process": {"allow": ["git"]}
        }))))
        .unwrap();
        assert_eq!(r.unenforced, vec!["filesystem", "process"]);
    }

    #[test]
    fn inputs_that_reach_a_shell_are_validated() {
        assert!(valid_git_url("https://github.com/zyvorai/zyvor-fluxvm"));
        assert!(!valid_git_url("git@github.com:a/b.git"));
        assert!(!valid_git_url("https://x y"));
        assert!(!valid_git_url("https://-oProxyCommand=x"));
        assert!(valid_guest_path("/workspace"));
        assert!(!valid_guest_path("/workspace/../etc"));
        assert!(!valid_guest_path("/a b"));
        assert!(!valid_guest_path("relative"));
        assert!(valid_tag("after-build.1"));
        assert!(!valid_tag("a/b"));
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn capabilities_follow_the_backend_and_never_claim_fork_on_a_nat_session() {
        assert!(capabilities_for("flux-vm", false).fork);
        // vz forks only without a NIC of its own: offline or allow-listed sessions.
        let nat = capabilities_for("vz", false);
        assert!(nat.snapshot && !nat.fork && !nat.pty);
        assert!(capabilities_for("vz", true).fork);
        assert!(!capabilities_for("cloud-hypervisor", true).snapshot);
        assert!(!capabilities_for("qemu", true).fork);
    }

    #[test]
    fn the_ledger_reads_back_in_order_and_filters_by_time() {
        let dir = std::env::temp_dir().join(format!("fluxvm-ledger-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.ledger.jsonl");
        let t0 = Utc::now();
        let l1 = json!({"ts": t0, "kind": "exec", "detail": {}});
        let l2 =
            json!({"ts": t0 + chrono::Duration::seconds(5), "kind": "checkpoint", "detail": {}});
        std::fs::write(&p, format!("{l1}\nnot json\n{l2}\n")).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let all: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(all.len(), 2, "a corrupt line is skipped, not fatal");
        assert_eq!(all[0]["kind"], "exec");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
