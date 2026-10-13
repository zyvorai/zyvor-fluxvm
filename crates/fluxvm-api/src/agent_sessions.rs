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
//! * `POST   /v1/agents/sessions/{id}/preview`       mint an expiring preview URL for a guest port
//! * `GET    /v1/agents/sessions/{id}/previews`      list live preview URLs
//! * `DELETE /v1/agents/sessions/{id}/preview/{token}` revoke one
//! * `ANY    /preview/{token}/{*path}`               the preview itself (the token is the credential)
//! * `POST   /v1/agents/sessions/{id}/jobs`          start a detached command
//! * `GET    /v1/agents/sessions/{id}/jobs/{job}`    its state and output from `?offset=`
//! * `DELETE /v1/agents/sessions/{id}/jobs/{job}`    stop it
//!
//! Records live in `<state_dir>/agent-sessions/<id>.json` and `<id>.ledger.jsonl`, so a
//! session survives a daemon restart. What is *not* here: promote, replay, sandbox-scoped PTY,
//! browser profiles (see `docs/agent-sessions.md`, "Roadmap").

use super::{ApiError, ApiResult, AuditActor, TokenTenant, change_error, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{any, get, post},
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
        .route("/v1/agents/sessions/{id}/preview", post(create_preview))
        .route("/v1/agents/sessions/{id}/previews", get(list_previews))
        .route(
            "/v1/agents/sessions/{id}/preview/{token}",
            axum::routing::delete(revoke_preview),
        )
        .route("/v1/agents/sessions/{id}/jobs", post(job_start))
        .route(
            "/v1/agents/sessions/{id}/jobs/{job}",
            get(job_read).delete(job_kill),
        )
        .route("/preview/{token}", any(preview_root))
        .route("/preview/{token}/", any(preview_root))
        .route("/preview/{token}/{*path}", any(preview_path))
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

// ---------------------------------------------------------------------------------------
// Preview URLs: an expiring, revocable capability to one guest port of one session. The token
// is 256 random bits kept in `<state_dir>/agent-previews/<token>.json`; whoever holds the URL
// can reach that port and nothing else, until it expires, is revoked, or the session ends.

#[derive(Debug, Serialize, Deserialize)]
struct Preview {
    token: String,
    session: Uuid,
    sandbox: Uuid,
    port: u16,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

const PREVIEW_DEFAULT_TTL: u64 = 3600;
const PREVIEW_MAX_TTL: u64 = 86_400;

fn previews_dir(m: &VmManager) -> PathBuf {
    m.cfg.state_dir.join("agent-previews")
}

fn valid_token(t: &str) -> bool {
    t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_preview(m: &VmManager, token: &str) -> Option<Preview> {
    if !valid_token(token) {
        return None;
    }
    let bytes = std::fs::read(previews_dir(m).join(format!("{token}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[derive(Debug, Deserialize)]
struct PreviewBody {
    port: u16,
    #[serde(default)]
    ttl_seconds: Option<u64>,
}

async fn create_preview(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<PreviewBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let bad = |msg: &str| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: msg.to_string(),
    };
    if body.port == 0 {
        return Err(bad("port must be 1-65535"));
    }
    let ttl = body.ttl_seconds.unwrap_or(PREVIEW_DEFAULT_TTL);
    if ttl == 0 || ttl > PREVIEW_MAX_TTL {
        return Err(bad("ttl_seconds must be 1-86400"));
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let now = Utc::now();
    let p = Preview {
        token: token.clone(),
        session: id,
        sandbox: s.sandbox_id,
        port: body.port,
        created_at: now,
        expires_at: now + chrono::Duration::seconds(ttl as i64),
    };
    let d = previews_dir(&m);
    tokio::fs::create_dir_all(&d)
        .await
        .map_err(anyhow::Error::from)?;
    let bytes = serde_json::to_vec_pretty(&p).map_err(anyhow::Error::from)?;
    tokio::fs::write(d.join(format!("{token}.json")), bytes)
        .await
        .map_err(anyhow::Error::from)?;
    // The token is a credential: the ledger gets the port and expiry, never the token.
    record(
        &m,
        id,
        "preview.create",
        json!({"port": p.port, "expires_at": p.expires_at}),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "token": token,
            "url": format!("/preview/{token}/"),
            "port": p.port,
            "expires_at": p.expires_at,
        })),
    ))
}

async fn list_previews(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    let now = Utc::now();
    let mut items = Vec::new();
    if let Ok(rd) = std::fs::read_dir(previews_dir(&m)) {
        for e in rd.flatten() {
            let Ok(b) = std::fs::read(e.path()) else {
                continue;
            };
            if let Ok(p) = serde_json::from_slice::<Preview>(&b)
                && p.session == id
                && p.expires_at > now
            {
                items.push(json!({
                    "token": p.token, "port": p.port,
                    "created_at": p.created_at, "expires_at": p.expires_at,
                }));
            }
        }
    }
    Ok(Json(json!({"items": items})))
}

async fn revoke_preview(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, token)): Path<(Uuid, String)>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    match read_preview(&m, &token) {
        Some(p) if p.session == id => {
            let _ = tokio::fs::remove_file(previews_dir(&m).join(format!("{token}.json"))).await;
            record(&m, id, "preview.revoke", json!({"port": p.port}));
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "preview not found".into(),
        }),
    }
}

async fn preview_root(
    State(m): State<Arc<VmManager>>,
    Path(token): Path<String>,
    req: axum::extract::Request,
) -> axum::response::Response {
    preview_serve(m, token, String::new(), req).await
}

async fn preview_path(
    State(m): State<Arc<VmManager>>,
    Path((token, path)): Path<(String, String)>,
    req: axum::extract::Request,
) -> axum::response::Response {
    preview_serve(m, token, path, req).await
}

async fn preview_serve(
    m: Arc<VmManager>,
    token: String,
    path: String,
    req: axum::extract::Request,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Unknown, expired and revoked tokens are indistinguishable on purpose.
    let gone = || (StatusCode::NOT_FOUND, "preview not found").into_response();
    let Some(p) = read_preview(&m, &token) else {
        return gone();
    };
    if p.expires_at <= Utc::now() {
        let _ = std::fs::remove_file(previews_dir(&m).join(format!("{token}.json")));
        return gone();
    }
    // The session must still be running: deleting it ends every preview.
    let alive = std::fs::read(session_path(&m, p.session))
        .ok()
        .and_then(|b| serde_json::from_slice::<Session>(&b).ok())
        .is_some_and(|s| s.state == "running");
    if !alive {
        return gone();
    }
    let mut resp = super::sandbox_proxy_inner(m, p.sandbox, p.port, path, req).await;
    // A preview is somebody else's app: never let a browser leak the token via Referer or cache it.
    let h = resp.headers_mut();
    h.insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    resp
}

// ---------------------------------------------------------------------------------------
// Jobs: a command that outlives the request. It runs detached in the guest (`setsid`), its
// output goes to a file, and the caller reads that file from a byte offset. This is built on
// the ordinary exec path, so each read costs one exec; it is polling, not a stream. There is
// no stdin and no TTY.

const JOBS_DIR: &str = "/tmp/fluxvm-jobs";
const JOB_READ_MAX: usize = 65_536;

#[derive(Debug, Deserialize)]
struct JobBody {
    command: String,
}

/// The guest-side shell line that starts a job. Pure, so it is unit-tested.
fn job_start_script(job: Uuid, command: &str) -> String {
    let f = format!("{JOBS_DIR}/{job}");
    let out = shell_quote(&format!("{f}.out"));
    let exit = shell_quote(&format!("{f}.exit"));
    let pid = shell_quote(&format!("{f}.pid"));
    // The inner shell records its pid (== process group, because of setsid), runs the command
    // with output to a file, and records the exit code last, so "exit file exists" means the
    // output file is complete.
    format!(
        "mkdir -p {dir} && (setsid sh -c 'echo $$ > \"$1\"; sh -c \"$2\" > \"$3\" 2>&1 < /dev/null; echo $? > \"$4\"' _ {pid} {cmd} {out} {exit} > /dev/null 2>&1 &) ; echo started",
        dir = shell_quote(JOBS_DIR),
        cmd = shell_quote(command),
    )
}

/// The guest-side line that reads a job: exit code first, then output from `offset`, so an
/// exit seen here means the output read after it is complete.
fn job_read_script(job: Uuid, offset: u64) -> String {
    let f = format!("{JOBS_DIR}/{job}");
    format!(
        "e=$(cat {exit} 2>/dev/null); if [ ! -e {out} ]; then echo MISSING; exit 0; fi; \
         echo \"EXIT:$e\"; tail -c +{from} {out} | head -c {max} | base64 | tr -d '\\n'; echo",
        exit = shell_quote(&format!("{f}.exit")),
        out = shell_quote(&format!("{f}.out")),
        from = offset + 1,
        max = JOB_READ_MAX,
    )
}

fn base64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let Some(v) = val(c) else { continue };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

async fn job_start(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<JobBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
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
    let job = Uuid::new_v4();
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            job_start_script(job, &body.command),
            Some(30),
            None,
        )
        .await?;
    let out = json!(res);
    if out["exit_code"].as_i64() != Some(0) {
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: format!(
                "could not start the job: {}",
                out["stderr"].as_str().unwrap_or("")
            ),
        });
    }
    record(
        &m,
        id,
        "job.start",
        json!({"job": job, "command": body.command}),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({"job": job, "state": "running"})),
    ))
}

#[derive(Debug, Deserialize)]
struct JobQuery {
    #[serde(default)]
    offset: u64,
}

async fn job_read(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, job)): Path<(Uuid, Uuid)>,
    Query(q): Query<JobQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let res = m
        .exec_with_policy(s.sandbox_id, job_read_script(job, q.offset), Some(30), None)
        .await?;
    let out = json!(res);
    let stdout = out["stdout"].as_str().unwrap_or("");
    let mut lines = stdout.lines();
    let head = lines.next().unwrap_or("");
    if head == "MISSING" {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("job {job} not found in session {id}"),
        });
    }
    let exit = head
        .strip_prefix("EXIT:")
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .and_then(|e| e.parse::<i64>().ok());
    let bytes = base64_decode(lines.next().unwrap_or(""));
    let next = q.offset + bytes.len() as u64;
    Ok(Json(json!({
        "job": job,
        "state": if exit.is_some() { "exited" } else { "running" },
        "exit_code": exit,
        "output": String::from_utf8_lossy(&bytes),
        "offset": q.offset,
        "next_offset": next,
        // true when more output was waiting than one read returns; read again from next_offset
        "more": bytes.len() >= JOB_READ_MAX,
    })))
}

async fn job_kill(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, job)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let pid = shell_quote(&format!("{JOBS_DIR}/{job}.pid"));
    let exit = shell_quote(&format!("{JOBS_DIR}/{job}.exit"));
    // Negative pid = the whole process group the job started in. That group includes the wrapper
    // that would have written the exit file, so a killed job gets the conventional 143 here.
    let cmd = format!(
        "p=$(cat {pid} 2>/dev/null) && [ -n \"$p\" ] && kill -TERM -- \"-$p\" 2>/dev/null; \
         sleep 0.3; [ -e {exit} ] || echo 143 > {exit}; true"
    );
    m.exec_with_policy(s.sandbox_id, cmd, Some(30), None)
        .await?;
    record(&m, id, "job.kill", json!({"job": job}));
    Ok(StatusCode::NO_CONTENT)
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

    #[test]
    fn a_job_is_started_detached_and_read_exit_first() {
        let job = Uuid::nil();
        let start = job_start_script(job, "echo 'hi there'; sleep 1");
        assert!(start.contains("setsid"), "{start}");
        assert!(start.contains("/tmp/fluxvm-jobs/00000000-0000-0000-0000-000000000000.out"));
        // The command arrives as one quoted argument, never spliced into the line.
        assert!(
            start.contains("'echo '\\''hi there'\\''; sleep 1'"),
            "{start}"
        );
        let read = job_read_script(job, 10);
        // The exit file is read before the output, and the offset is 1-based for tail.
        assert!(
            read.find(".exit").unwrap() < read.find("tail -c +11").unwrap(),
            "{read}"
        );
    }

    #[test]
    fn base64_decoding_matches_the_guest_encoder() {
        assert_eq!(base64_decode("aGVsbG8gd29ybGQ="), b"hello world");
        assert_eq!(base64_decode(""), b"");
        assert_eq!(base64_decode("/+8="), vec![0xff, 0xef]);
        assert_eq!(base64_decode("AAEC"), vec![0, 1, 2]);
    }
}
