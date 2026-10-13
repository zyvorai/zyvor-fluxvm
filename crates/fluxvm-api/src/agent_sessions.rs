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
//! * `GET    /v1/agents/sessions/{id}/pty`           WebSocket: an interactive shell on a pseudo-terminal
//! * `GET    /v1/agents/sessions/{id}/trajectory`    ledger and VM events merged into one ordered list
//! * `POST   /v1/agents/sessions/{id}/preview`       mint an expiring preview URL for a guest port
//! * `GET    /v1/agents/sessions/{id}/previews`      list live preview URLs
//! * `DELETE /v1/agents/sessions/{id}/preview/{token}` revoke one
//! * `ANY    /preview/{token}/{*path}`               the preview itself (the token is the credential)
//! * `POST   /v1/agents/sessions/{id}/jobs`          start a detached command
//! * `GET    /v1/agents/sessions/{id}/jobs/{job}`    its state and output from `?offset=`
//! * `DELETE /v1/agents/sessions/{id}/jobs/{job}`    stop it
//! * `GET    /v1/agents/sessions/{id}/files?path=`   list a directory (or stat a file)
//! * `GET    /v1/agents/sessions/{id}/files/content?path=` read a file
//! * `PUT    /v1/agents/sessions/{id}/files/content` write a file
//! * `DELETE /v1/agents/sessions/{id}/files?path=`   remove a file or directory
//! * `GET    /v1/agents/sessions/{id}/git/status`    branch and changed paths of the workspace repo
//! * `GET    /v1/agents/sessions/{id}/git/diff`      the workspace diff
//! * `GET    /v1/agents/sessions/{id}/git/log`       recent commits
//! * `POST   /v1/agents/sessions/{id}/git/commit`    stage and commit
//! * `POST   /v1/agents/sessions/{id}/git/branch`    create (and switch to) a branch
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
        .route("/v1/agents/sessions/{id}/trajectory", get(trajectory))
        .route("/v1/agents/sessions/{id}/pty", get(pty))
        .route("/v1/agents/sessions/{id}/preview", post(create_preview))
        .route("/v1/agents/sessions/{id}/previews", get(list_previews))
        .route(
            "/v1/agents/sessions/{id}/preview/{token}",
            axum::routing::delete(revoke_preview),
        )
        .route(
            "/v1/agents/sessions/{id}/files",
            get(files_list).delete(files_delete),
        )
        .route(
            "/v1/agents/sessions/{id}/files/content",
            get(files_read).put(files_write),
        )
        .route("/v1/agents/sessions/{id}/git/status", get(git_status))
        .route("/v1/agents/sessions/{id}/git/diff", get(git_diff))
        .route("/v1/agents/sessions/{id}/git/log", get(git_log))
        .route("/v1/agents/sessions/{id}/git/commit", post(git_commit))
        .route("/v1/agents/sessions/{id}/git/branch", post(git_branch))
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
    /// `GET .../pty` works: vz Linux sessions (the guest is reached over SSH). A session with
    /// `policy.process` rules is refused at the route even where this is true.
    pty: bool,
}

/// The policy as compiled, including the parts that are accepted but not enforced yet.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PolicyReport {
    preset: Option<String>,
    offline: bool,
    allow_hosts: Vec<String>,
    /// Command rules enforced on exec and job start; `None` when the session has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    process: Option<CommandRules>,
    /// `filesystem` rules are stored but nothing enforces them yet.
    unenforced: Vec<String>,
}

/// Glob rules over the command strings an agent submits (`*` matches any run of characters).
/// A command is split on `;`, `&&`, `||`, `|`, `&` and newlines and every piece is checked: any
/// piece matching `deny` refuses the whole command; with a non-empty `allow` every piece must
/// match one of its patterns. This stops an agent running what you listed by accident; it is not
/// a security boundary, because a shell can build a command in ways the string never shows
/// (`sh -c`, `$(...)`, variables, scripts it wrote itself). Quote and backslash characters are
/// dropped before matching, so the CLI's quoted argv (`'touch' '/x'`) matches `touch *`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct CommandRules {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

fn glob_match(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn command_pieces(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in cmd.chars() {
        if matches!(c, ';' | '&' | '|' | '\n') {
            out.push(std::mem::take(&mut cur));
        } else if !matches!(c, '\'' | '"' | '\\') {
            // Quotes and backslashes are dropped so `'touch' '/x'` and `t"o"uch /x` match `touch *`.
            cur.push(c);
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

impl CommandRules {
    /// `Err(reason)` when the command is refused.
    fn check(&self, cmd: &str) -> Result<(), String> {
        for piece in command_pieces(cmd) {
            if let Some(d) = self.deny.iter().find(|d| glob_match(d, &piece)) {
                return Err(format!("`{piece}` matches the deny rule `{d}`"));
            }
            if !self.allow.is_empty() && !self.allow.iter().any(|a| glob_match(a, &piece)) {
                return Err(format!("`{piece}` matches no allow rule"));
            }
        }
        Ok(())
    }
}

fn enforce_rules(s: &Session, cmd: &str) -> Result<(), ApiError> {
    match s.policy.process.as_ref().map(|r| r.check(cmd)) {
        Some(Err(why)) => Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: format!("refused by the session's process policy: {why}"),
        }),
        _ => Ok(()),
    }
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
    #[serde(default)]
    volumes: Vec<VolumeMount>,
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
    if let Some(v) = p.process {
        let obj = v
            .as_object()
            .ok_or_else(|| bad("process must be an object with allow and/or deny".into()))?;
        let mut rules = CommandRules::default();
        for (k, val) in obj {
            let list = match k.as_str() {
                "allow" => &mut rules.allow,
                "deny" => &mut rules.deny,
                "ask" => {
                    return Err(bad(
                        "process.ask is not supported yet: there is no per-command approval flow"
                            .into(),
                    ));
                }
                other => return Err(bad(format!("unknown process rule `{other}`"))),
            };
            for e in val
                .as_array()
                .ok_or_else(|| bad(format!("process.{k} must be a list of patterns")))?
            {
                let pat = e
                    .as_str()
                    .map(str::trim)
                    .filter(|x| !x.is_empty() && x.len() <= 256)
                    .ok_or_else(|| bad(format!("process.{k} entries are non-empty strings")))?;
                list.push(pat.to_string());
            }
        }
        r.process = Some(rules);
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
    /// Named host volumes (`POST /v1/volumes`) attached as disks. vz only; see `docs/agent-sessions.md`.
    #[serde(default)]
    volumes: Vec<VolumeBody>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    cpus: Option<u8>,
    #[serde(default)]
    memory_mb: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct VolumeBody {
    name: String,
    /// Absolute guest mount point under /mnt, /data, /srv, /opt, /workspace, /home or /root.
    path: String,
}

/// A volume mounted into the session; recorded so fork is refused and the docs/ledger are honest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct VolumeMount {
    name: String,
    path: String,
}

const MOUNT_ROOTS: &[&str] = &["mnt", "data", "srv", "opt", "workspace", "home", "root"];
const MAX_SESSION_VOLUMES: usize = 2;

fn valid_mount_path(p: &str) -> bool {
    valid_guest_path(p)
        && !p.contains(|c: char| c.is_whitespace() || "'\"$`\\;&|<>*?".contains(c))
        && p.trim_start_matches('/')
            .split('/')
            .next()
            .is_some_and(|top| MOUNT_ROOTS.contains(&top))
        && p.trim_end_matches('/').len() > 1
}

/// Make the volume's disk usable: wait for the device, format it only when blkid says it has no
/// file system (exit 2; any other failure aborts rather than risk wiping data),
/// mount it, make the mount point the guest user's, and persist it in fstab (`nofail`).
fn mount_script(serial: &str, path: &str) -> String {
    let dev = format!("/dev/disk/by-id/virtio-{serial}");
    let (d, p) = (shell_quote(&dev), shell_quote(path));
    format!(
        "set -e; for i in $(seq 1 40); do [ -e {d} ] && break; sleep 0.25; done; [ -e {d} ] || {{ echo device missing >&2; exit 3; }}; \
         rc=0; sudo -n blkid {d} >/dev/null 2>&1 || rc=$?; \
         if [ $rc -eq 2 ]; then sudo -n mkfs.ext4 -q -F -L fluxvol {d}; elif [ $rc -ne 0 ]; then echo blkid failed >&2; exit 4; fi; \
         sudo -n mkdir -p {p}; mountpoint -q {p} || sudo -n mount {d} {p}; \
         sudo -n chown \"$(id -u):$(id -g)\" {p}; \
         grep -qF {d} /etc/fstab || printf '%s %s ext4 defaults,nofail 0 2\\n' {d} {p} | sudo -n tee -a /etc/fstab >/dev/null"
    )
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
        // `GET .../pty` reaches the guest over SSH, which only the vz path has.
        pty: backend == "vz",
    }
}

/// Why a session cannot fork, for the 409 body. `None` when it can.
fn fork_refusal(s: &Session) -> Option<String> {
    if !s.volumes.is_empty() {
        return Some(
            "a session with a volume cannot fork: the children would share one disk and corrupt it"
                .to_string(),
        );
    }
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
    let want_volumes = body
        .runtime
        .as_ref()
        .map(|r| r.volumes.as_slice())
        .unwrap_or_default();
    let mut extra_disks = Vec::new();
    let mut mounts: Vec<(String, VolumeMount)> = Vec::new();
    if !want_volumes.is_empty() {
        if !cfg!(target_os = "macos") {
            return Err(bad(
                "runtime.volumes are only supported on vz (macOS) sessions",
            ));
        }
        if tenant.is_some() {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                message: "volumes are host-wide and not tenant-scoped yet; a tenant token cannot attach one"
                    .into(),
            });
        }
        if want_volumes.len() > MAX_SESSION_VOLUMES {
            return Err(bad("at most 2 volumes per session"));
        }
        let refs = super::volumes_api::refs_of(&m).await;
        let vdir = super::volumes_api::volume_dir(&m);
        for (i, v) in want_volumes.iter().enumerate() {
            if !valid_mount_path(&v.path) {
                return Err(bad(
                    "volume path must be an absolute guest path under /mnt, /data, /srv, /opt, /workspace, /home or /root",
                ));
            }
            if want_volumes.iter().filter(|o| o.path == v.path).count() > 1
                || want_volumes.iter().filter(|o| o.name == v.name).count() > 1
            {
                return Err(bad("each volume name and path may be used once"));
            }
            let info = super::volumes_api::info(&vdir, &v.name, &refs)
                .map_err(super::volumes_api::api_err)?;
            if !info.attached_to.is_empty() {
                return Err(ApiError {
                    status: StatusCode::CONFLICT,
                    message: format!(
                        "volume {:?} is attached to VM {}; a volume is one disk and attaches to one VM at a time",
                        v.name,
                        info.attached_to.join(", ")
                    ),
                });
            }
            let serial = format!("fluxvol{i}");
            extra_disks.push(fluxvm_core::model::AppleDisk {
                name: Some(format!("vol-{}", v.name)),
                path: info.path,
                block_device_id: Some(serial.clone()),
                ..Default::default()
            });
            mounts.push((
                serial,
                VolumeMount {
                    name: v.name.clone(),
                    path: v.path.clone(),
                },
            ));
        }
    }
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
        extra_disks,
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
        volumes: Vec::new(),
    };
    record(
        &m,
        id,
        "session.create",
        json!({"agent": session.agent, "sandbox": rec.id, "backend": session.backend, "policy": session.policy}),
    );

    // Volumes first, so a workspace under a volume path lands on the volume.
    for (serial, mnt) in &mounts {
        let res = match m.ensure_running_for_request(rec.id).await {
            Ok(_) => m
                .exec_with_policy(rec.id, mount_script(serial, &mnt.path), Some(120), None)
                .await
                .map_err(|e| format!("{e:#}")),
            Err(e) => Err(format!("{e:#}")),
        };
        let code = res
            .as_ref()
            .ok()
            .and_then(|r| serde_json::to_value(r).ok())
            .and_then(|v| v["exit_code"].as_i64());
        if code != Some(0) {
            let why = match &res {
                Err(e) => e.clone(),
                Ok(r) => serde_json::to_value(r)
                    .ok()
                    .and_then(|v| v["stderr"].as_str().map(|s| s.trim().to_string()))
                    .unwrap_or_default(),
            };
            // Nothing was promised yet: remove the sandbox so the volume is free again.
            let _ = m.delete(rec.id).await;
            let _ = std::fs::remove_file(session_path(&m, id));
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!(
                    "could not mount volume {:?} at {} (exit {code:?}): {why}",
                    mnt.name, mnt.path
                ),
            });
        }
        record(
            &m,
            id,
            "volume.mount",
            json!({"volume": mnt.name, "path": mnt.path}),
        );
        session.volumes.push(mnt.clone());
    }

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
    if let Err(e) = enforce_rules(&s, &body.command) {
        record(
            &m,
            id,
            "exec.refused",
            json!({"command": body.command, "reason": e.message}),
        );
        return Err(e);
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
            volumes: Vec::new(),
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

/// Ledger entries and the sandbox's VM events as one list ordered by time, each step numbered
/// and tagged with where it came from. The ledger is what the API did; VM events are what the
/// hypervisor layer saw. Both are what we already record: nothing here is a transcript of the
/// agent's reasoning, which FluxVM never sees.
/// Order steps by time (stable, so equal timestamps keep their input order) and number them.
fn number_steps(mut steps: Vec<(DateTime<Utc>, Value)>) -> Vec<Value> {
    steps.sort_by_key(|(ts, _)| *ts);
    steps
        .into_iter()
        .enumerate()
        .map(|(i, (ts, mut v))| {
            v["seq"] = json!(i);
            v["ts"] = json!(ts);
            v
        })
        .collect()
}

async fn trajectory(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    let mut steps: Vec<(DateTime<Utc>, Value)> = Vec::new();
    for e in read_ledger(&m, id, None) {
        let Some(ts) = e["ts"]
            .as_str()
            .and_then(|t| t.parse::<DateTime<Utc>>().ok())
        else {
            continue;
        };
        steps.push((
            ts,
            json!({"source": "ledger", "kind": e["kind"], "detail": e["detail"]}),
        ));
    }
    if let Some(log) = fluxvm_scheduler::events::global() {
        let filter = fluxvm_scheduler::EventFilter {
            vm: Some(s.sandbox_id),
            since: None,
            event: None,
            limit: Some(5000),
        };
        for e in log.list(&filter).unwrap_or_default() {
            steps.push((
                e.ts,
                json!({"source": "vm", "kind": e.event, "detail": e.fields}),
            ));
        }
    }
    let steps = number_steps(steps);
    Ok(Json(json!({
        "session": {
            "id": s.id,
            "sandbox_id": s.sandbox_id,
            "agent": s.agent,
            "backend": s.backend,
            "state": s.state,
            "created_at": s.created_at,
        },
        "steps": steps,
    })))
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
    let is_ws = req
        .headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let mut resp = if is_ws {
        // The same token that gates HTTP gates the WebSocket (Chrome DevTools, dev-server HMR).
        use axum::extract::FromRequestParts as _;
        let (mut parts, _body) = req.into_parts();
        match axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(ws) => super::sandbox_ws_inner(m, p.sandbox, p.port, path, ws).await,
            Err(e) => e.into_response(),
        }
    } else {
        super::sandbox_proxy_inner(m, p.sandbox, p.port, path, req).await
    };
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
// PTY: an interactive shell over a WebSocket. The guest is reached with `ssh -tt`, so the guest
// sees a real terminal; frames carry raw bytes in both directions. The terminal size is set once
// at start (`cols`, `rows`); a later resize is not forwarded. Not available to a session with
// `policy.process` rules, because a shell would walk straight around them.

static OPEN_PTYS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_OPEN_PTYS: usize = 16;
const PTY_MAX_SECONDS: u64 = 3600;

#[derive(Debug, Deserialize, Default)]
struct PtyQuery {
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
    /// Run this instead of a login shell (still on a terminal).
    #[serde(default)]
    command: Option<String>,
}

/// The remote shell line that sizes the terminal and starts the shell or command. Pure.
fn pty_script(cols: u16, rows: u16, cwd: Option<&str>, command: Option<&str>) -> String {
    let cd = cwd
        .map(|c| format!("cd {} 2>/dev/null; ", shell_quote(c)))
        .unwrap_or_default();
    let run = match command {
        Some(c) => format!("exec sh -c {}", shell_quote(c)),
        None => "exec \"${SHELL:-/bin/sh}\" -l".to_string(),
    };
    format!("stty cols {cols} rows {rows} 2>/dev/null; export TERM=xterm-256color; {cd}{run}")
}

async fn pty(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PtyQuery>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse as _;
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if s.policy.process.is_some() {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "this session has process rules, and a terminal would bypass them".into(),
        });
    }
    let (cols, rows) = (q.cols.unwrap_or(80), q.rows.unwrap_or(24));
    if !(1..=500).contains(&cols) || !(1..=300).contains(&rows) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "cols must be 1-500 and rows 1-300".into(),
        });
    }
    if OPEN_PTYS.load(std::sync::atomic::Ordering::SeqCst) >= MAX_OPEN_PTYS {
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: format!("at most {MAX_OPEN_PTYS} terminals at once on this host"),
        });
    }
    let cwd = s.workspace.as_ref().map(|w| w.path.clone());
    let script = pty_script(cols, rows, cwd.as_deref(), q.command.as_deref());
    // Spawn before upgrading so a missing ssh or an unreachable guest is an HTTP error.
    let vm = m.ensure_running_for_request(s.sandbox_id).await?;
    let child = m.pty_spawn(&vm, &script).map_err(|e| ApiError {
        status: StatusCode::CONFLICT,
        message: format!("{e:#}"),
    })?;
    record(
        &m,
        id,
        "pty.open",
        json!({"cols": cols, "rows": rows, "command": q.command}),
    );
    Ok(ws
        .on_upgrade(move |socket| relay_pty(m, id, socket, child))
        .into_response())
}

async fn relay_pty(
    m: Arc<VmManager>,
    id: Uuid,
    socket: axum::extract::ws::WebSocket,
    mut child: tokio::process::Child,
) {
    use axum::extract::ws::{CloseFrame, Message};
    use futures::{SinkExt, StreamExt};
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    OPEN_PTYS.fetch_add(1, Ordering::SeqCst);
    let started = std::time::Instant::now();
    let (mut tx, mut rx) = socket.split();
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        OPEN_PTYS.fetch_sub(1, Ordering::SeqCst);
        return;
    };
    let bytes_in = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bytes_out = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (bi, bo) = (bytes_in.clone(), bytes_out.clone());

    let up = async move {
        while let Some(Ok(msg)) = rx.next().await {
            let data: Vec<u8> = match msg {
                Message::Binary(b) => b.to_vec(),
                Message::Text(t) => t.as_bytes().to_vec(),
                Message::Close(_) => break,
                _ => continue,
            };
            bi.fetch_add(data.len() as u64, Ordering::Relaxed);
            if stdin.write_all(&data).await.is_err() {
                break;
            }
        }
    };
    let down = async {
        let mut buf = vec![0u8; 8192];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    bo.fetch_add(n as u64, Ordering::Relaxed);
                    if tx
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                }
            }
        }
        true
    };
    // Whichever side ends first ends the session; the hour cap bounds a forgotten terminal.
    let client_alive = {
        use futures::future::{Either, select};
        let (up, down) = (Box::pin(up), Box::pin(down));
        match tokio::time::timeout(
            std::time::Duration::from_secs(PTY_MAX_SECONDS),
            select(up, down),
        )
        .await
        {
            Ok(Either::Left(_)) => false,
            Ok(Either::Right((alive, _))) => alive,
            Err(_) => true,
        }
    };
    // The shell ended on its own: its status is ssh's exit code, so let ssh finish rather than kill
    // it (a killed ssh has no code). A client that left takes the shell down with it.
    if !client_alive {
        let _ = child.start_kill();
    }
    let code = match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
        Ok(st) => st.ok().and_then(|s| s.code()),
        Err(_) => {
            let _ = child.start_kill();
            None
        }
    };
    if client_alive {
        let _ = tx
            .send(Message::Close(Some(CloseFrame {
                code: 1000,
                reason: format!("exit={}", code.unwrap_or(-1)).into(),
            })))
            .await;
    }
    OPEN_PTYS.fetch_sub(1, Ordering::SeqCst);
    record(
        &m,
        id,
        "pty.close",
        json!({
            "duration_ms": started.elapsed().as_millis() as u64,
            "bytes_in": bytes_in.load(Ordering::Relaxed),
            "bytes_out": bytes_out.load(Ordering::Relaxed),
            "exit_code": code,
        }),
    );
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
    if let Err(e) = enforce_rules(&s, &body.command) {
        record(
            &m,
            id,
            "job.refused",
            json!({"command": body.command, "reason": e.message}),
        );
        return Err(e);
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

// ---------------------------------------------------------------------------------------
// Files: list, read, write and remove paths in a session's guest. Read and write go through the
// sandbox file API the daemon already has; list and remove run a short command in the guest.

fn valid_file_path(p: &str) -> bool {
    p.starts_with('/')
        && p.len() <= 4096
        && !p.contains(['\0', '\n'])
        && !p.split('/').any(|c| c == "..")
}

#[derive(Debug, Deserialize)]
struct PathQuery {
    path: String,
    #[serde(default)]
    recursive: bool,
}

fn path_arg(p: &str) -> ApiResult<()> {
    if valid_file_path(p) {
        return Ok(());
    }
    Err(ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "path must be absolute, at most 4096 characters, with no '..', newline or NUL"
            .into(),
    })
}

/// One directory entry as `find -printf '%y\t%s\t%T@\t%m\t%f\0'` prints it.
fn parse_listing(bytes: &[u8]) -> (String, Vec<Value>) {
    let mut kind = String::from("unknown");
    let mut entries = Vec::new();
    for (i, rec) in bytes
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .enumerate()
    {
        let text = String::from_utf8_lossy(rec);
        if i == 0 {
            kind = match text.as_ref() {
                "D" => "dir".into(),
                "F" => "file".into(),
                _ => "unknown".into(),
            };
            continue;
        }
        let mut f = text.splitn(5, '\t');
        let (Some(ty), Some(size), Some(mtime), Some(mode), Some(name)) =
            (f.next(), f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        entries.push(json!({
            "name": name,
            "type": match ty { "f" => "file", "d" => "dir", "l" => "symlink", _ => "other" },
            "size": size.parse::<u64>().unwrap_or(0),
            "mtime": mtime.split('.').next().and_then(|t| t.parse::<i64>().ok()),
            "mode": u32::from_str_radix(mode, 8).ok(),
        }));
    }
    (kind, entries)
}

fn list_script(path: &str) -> String {
    let p = shell_quote(path);
    let fmt = "'%y\\t%s\\t%T@\\t%m\\t%f\\0'";
    format!(
        "[ -e {p} ] || [ -L {p} ] || exit 44; {{ if [ -d {p} ]; then printf 'D\\0'; \
         find {p} -mindepth 1 -maxdepth 1 -printf {fmt}; \
         else printf 'F\\0'; find {p} -maxdepth 0 -printf {fmt}; fi; }} | base64 | tr -d '\\n'"
    )
}

async fn guest_exec(m: &Arc<VmManager>, s: &Session, cmd: String) -> ApiResult<Value> {
    m.ensure_running_for_request(s.sandbox_id).await?;
    let res = m
        .exec_with_policy(s.sandbox_id, cmd, Some(30), None)
        .await?;
    Ok(json!(res))
}

async fn files_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    path_arg(&q.path)?;
    let out = guest_exec(&m, &s, list_script(&q.path)).await?;
    match out["exit_code"].as_i64() {
        Some(0) => {}
        Some(44) => {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: format!("{} does not exist", q.path),
            });
        }
        other => {
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!(
                    "listing failed (exit {other:?}): {}",
                    out["stderr"].as_str().unwrap_or("")
                ),
            });
        }
    }
    let (kind, mut entries) =
        parse_listing(&base64_decode(out["stdout"].as_str().unwrap_or("").trim()));
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(
        json!({"path": q.path, "kind": kind, "entries": entries}),
    ))
}

async fn files_read(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    path_arg(&q.path)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let res = m
        .get_file(s.sandbox_id, q.path.clone())
        .await
        .map_err(|e| ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("{}: {e:#}", q.path),
        })?;
    Ok(Json(json!(res)))
}

#[derive(Debug, Deserialize)]
struct WriteBody {
    path: String,
    content_base64: String,
    #[serde(default)]
    mode: Option<u32>,
    /// Create missing parent directories first (default true).
    #[serde(default = "yes")]
    parents: bool,
}

fn yes() -> bool {
    true
}

async fn files_write(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<WriteBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    path_arg(&body.path)?;
    if body.path.ends_with('/') {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "path must name a file".into(),
        });
    }
    if body.parents
        && let Some(dir) = std::path::Path::new(&body.path)
            .parent()
            .and_then(|d| d.to_str())
        && dir != "/"
    {
        let out = guest_exec(
            &m,
            &s,
            // The guest user is not root on stock images: fall back to sudo, then hand the topmost
            // directory that had to be created to that user, so this and later writes succeed.
            format!(
                "t={0}; u=; while [ ! -e \"$t\" ]; do u=\"$t\"; t=$(dirname \"$t\"); done; \
                 mkdir -p {0} 2>/dev/null || (sudo -n mkdir -p {0} && sudo -n chown -R \"$(id -u):$(id -g)\" \"$u\")",
                shell_quote(dir)
            ),
        )
        .await?;
        if out["exit_code"].as_i64() != Some(0) {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!("could not create {dir}"),
            });
        }
    }
    let bytes = base64_decode(&body.content_base64).len();
    m.put_file(
        s.sandbox_id,
        body.path.clone(),
        body.content_base64,
        body.mode,
    )
    .await?;
    record(
        &m,
        id,
        "file.write",
        json!({"path": body.path, "bytes": bytes}),
    );
    Ok(Json(json!({"path": body.path, "bytes": bytes})))
}

async fn files_delete(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    path_arg(&q.path)?;
    // `/` and one-component paths (`/etc`, `/usr`) are never a sensible thing to remove.
    if q.path.trim_end_matches('/').matches('/').count() < 2 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "refusing to remove a top-level path".into(),
        });
    }
    let flag = if q.recursive { "-rf" } else { "-f" };
    let out = guest_exec(&m, &s, format!("rm {flag} -- {}", shell_quote(&q.path))).await?;
    if out["exit_code"].as_i64() != Some(0) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: format!(
                "remove failed: {} (a directory needs recursive=true)",
                out["stderr"].as_str().unwrap_or("")
            ),
        });
    }
    record(
        &m,
        id,
        "file.delete",
        json!({"path": q.path, "recursive": q.recursive}),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------
// Git: status, diff, log, commit and branch for the repository in a session's workspace. Each
// call runs `git` in the guest through the ordinary exec path, so the guest image must have git
// (the stock debian-13 does not; a session with network access can `sudo apt-get install -y git`).

const GIT_OUTPUT_MAX: usize = 262_144;

#[derive(Debug, Deserialize, Default)]
struct GitQuery {
    /// Repository directory; defaults to the session's workspace path.
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

fn repo_dir(s: &Session, repo: &Option<String>) -> ApiResult<String> {
    let dir = repo
        .clone()
        .or_else(|| s.workspace.as_ref().map(|w| w.path.clone()))
        .unwrap_or_else(|| "/workspace".to_string());
    path_arg(&dir)?;
    Ok(dir)
}

/// Branch names git itself would accept, minus anything that could be read as an option.
fn valid_branch(b: &str) -> bool {
    !b.is_empty()
        && b.len() <= 200
        && !b.starts_with(['-', '/', '.'])
        && !b.ends_with(['/', '.'])
        && !b.contains("..")
        && !b.contains("//")
        && !b.contains("@{")
        && !b.ends_with(".lock")
        && b.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
}

/// Runs `git <args>` in `repo`. `Ok` carries the exit code and both streams; a guest without
/// git is a 409 that says how to fix it.
async fn run_git(
    m: &Arc<VmManager>,
    s: &Session,
    repo: &str,
    args: &str,
) -> ApiResult<(i64, String, String)> {
    let cmd = format!(
        "command -v git >/dev/null 2>&1 || exit 127; cd {} 2>/dev/null || exit 126; \
         git -c core.quotepath=false -c safe.directory='*' {args}",
        shell_quote(repo)
    );
    let out = guest_exec(m, s, cmd).await?;
    let code = out["exit_code"].as_i64().unwrap_or(-1);
    match code {
        127 => Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "git is not installed in the guest. On a session with network access: \
                      sudo apt-get install -y git (the stock debian-13 image has none)"
                .into(),
        }),
        126 => Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("{repo} does not exist in the guest"),
        }),
        _ => Ok((
            code,
            out["stdout"].as_str().unwrap_or("").to_string(),
            out["stderr"].as_str().unwrap_or("").to_string(),
        )),
    }
}

fn git_failed(what: &str, code: i64, stderr: &str) -> ApiError {
    let not_repo = stderr.contains("not a git repository");
    ApiError {
        status: if not_repo {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        },
        message: format!("git {what} failed (exit {code}): {}", stderr.trim()),
    }
}

/// `git status --porcelain=v1 -b -z` -> (branch line, entries). Renames and copies carry the old
/// path in the following NUL-separated record.
fn parse_status(out: &str) -> (String, Vec<Value>) {
    let mut recs = out.split('\0').filter(|r| !r.is_empty());
    let branch = recs
        .next()
        .and_then(|r| r.strip_prefix("## "))
        .unwrap_or("")
        .to_string();
    let mut entries = Vec::new();
    while let Some(r) = recs.next() {
        if r.len() < 4 {
            continue;
        }
        let (xy, path) = r.split_at(2);
        let path = path.trim_start();
        let mut e = json!({"index": &xy[..1], "worktree": &xy[1..2], "path": path});
        if matches!(&xy[..1], "R" | "C")
            && let Some(old) = recs.next()
        {
            e["from"] = json!(old);
        }
        entries.push(e);
    }
    (branch, entries)
}

fn truncate_output(s: String) -> (String, bool) {
    if s.len() <= GIT_OUTPUT_MAX {
        return (s, false);
    }
    let mut end = GIT_OUTPUT_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

async fn git_status(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<GitQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let repo = repo_dir(&s, &q.repo)?;
    let (code, out, err) = run_git(&m, &s, &repo, "status --porcelain=v1 -b -z").await?;
    if code != 0 {
        return Err(git_failed("status", code, &err));
    }
    let (branch, entries) = parse_status(&out);
    Ok(Json(
        json!({"repo": repo, "branch": branch, "clean": entries.is_empty(), "entries": entries}),
    ))
}

async fn git_diff(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<GitQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let repo = repo_dir(&s, &q.repo)?;
    let mut args = String::from("diff --no-color --no-ext-diff");
    if q.staged {
        args.push_str(" --staged");
    }
    if let Some(p) = &q.path {
        // Relative to the repo; never an option and never outside it.
        if p.is_empty()
            || p.starts_with(['-', '/'])
            || p.split('/').any(|c| c == "..")
            || p.contains(['\0', '\n'])
        {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: "path must be relative to the repository, without '..'".into(),
            });
        }
        args.push_str(&format!(" -- {}", shell_quote(p)));
    }
    let (code, out, err) = run_git(&m, &s, &repo, &args).await?;
    if code != 0 {
        return Err(git_failed("diff", code, &err));
    }
    let (diff, truncated) = truncate_output(out);
    Ok(Json(
        json!({"repo": repo, "staged": q.staged, "diff": diff, "truncated": truncated}),
    ))
}

async fn git_log(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<GitQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let repo = repo_dir(&s, &q.repo)?;
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let args = format!("log -n {limit} --format=%H%x1f%an%x1f%aI%x1f%s%x1e");
    let (code, out, err) = run_git(&m, &s, &repo, &args).await?;
    if code != 0 {
        // A fresh repository with no commit yet is an empty log, not an error.
        if err.contains("does not have any commits") {
            return Ok(Json(json!({"repo": repo, "commits": []})));
        }
        return Err(git_failed("log", code, &err));
    }
    let commits: Vec<Value> = out
        .split('\x1e')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let mut f = r.splitn(4, '\x1f');
            Some(json!({
                "commit": f.next()?, "author": f.next()?, "date": f.next()?, "subject": f.next()?,
            }))
        })
        .collect();
    Ok(Json(json!({"repo": repo, "commits": commits})))
}

#[derive(Debug, Deserialize)]
struct CommitBody {
    message: String,
    #[serde(default)]
    repo: Option<String>,
    /// Stage everything first (`git add -A`); default true. False commits what is staged.
    #[serde(default = "yes")]
    all: bool,
}

async fn git_commit(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<CommitBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let repo = repo_dir(&s, &body.repo)?;
    let message = body.message.trim();
    if message.is_empty() || message.len() > 4000 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "message is required (at most 4000 characters)".into(),
        });
    }
    if body.all {
        let (code, _, err) = run_git(&m, &s, &repo, "add -A").await?;
        if code != 0 {
            return Err(git_failed("add", code, &err));
        }
    }
    // A committer identity is required; use a clearly synthetic one unless the repo has its own.
    let args = format!(
        "-c user.name=\"$(git config user.name || echo 'FluxVM Agent')\" \
         -c user.email=\"$(git config user.email || echo 'agent@fluxvm.local')\" \
         commit -m {} --no-gpg-sign",
        shell_quote(message)
    );
    let (code, out, err) = run_git(&m, &s, &repo, &args).await?;
    if code != 0 {
        let text = format!("{out}{err}");
        if text.contains("nothing to commit") || text.contains("no changes added") {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: "nothing to commit".into(),
            });
        }
        return Err(git_failed("commit", code, &text));
    }
    let (_, head, _) = run_git(&m, &s, &repo, "rev-parse HEAD").await?;
    let commit = head.trim().to_string();
    record(
        &m,
        id,
        "git.commit",
        json!({"repo": repo, "commit": commit, "message": message}),
    );
    Ok(Json(json!({"repo": repo, "commit": commit})))
}

#[derive(Debug, Deserialize)]
struct BranchBody {
    name: String,
    #[serde(default)]
    repo: Option<String>,
}

async fn git_branch(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<BranchBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if !valid_branch(&body.name) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "invalid branch name".into(),
        });
    }
    let repo = repo_dir(&s, &body.repo)?;
    let (code, _, err) = run_git(
        &m,
        &s,
        &repo,
        &format!("switch -c {}", shell_quote(&body.name)),
    )
    .await?;
    if code != 0 {
        let status = if err.contains("already exists") {
            StatusCode::CONFLICT
        } else {
            StatusCode::BAD_REQUEST
        };
        return Err(ApiError {
            status,
            message: format!("git switch failed (exit {code}): {}", err.trim()),
        });
    }
    record(
        &m,
        id,
        "git.branch",
        json!({"repo": repo, "branch": body.name}),
    );
    Ok(Json(json!({"repo": repo, "branch": body.name})))
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
        assert_eq!(r.unenforced, vec!["filesystem"]);
        let rules = r.process.unwrap();
        assert_eq!(rules.allow, vec!["git"]);
        assert!(compile_policy(Some(pol(json!({"process": {"ask": ["x"]}})))).is_err());
        assert!(compile_policy(Some(pol(json!({"process": {"nope": []}})))).is_err());
        assert!(compile_policy(Some(pol(json!({"process": {"deny": "rm"}})))).is_err());
    }

    #[test]
    fn mount_paths_stay_under_data_roots() {
        assert!(valid_mount_path("/mnt/data"));
        assert!(valid_mount_path("/data"));
        assert!(valid_mount_path("/workspace/cache"));
        for bad in [
            "/",
            "/etc",
            "/etc/x",
            "/mnt/../etc",
            "mnt/x",
            "/mnt/a b",
            "/mnt/$x",
            "/mnt/a;b",
            "/mnt/'x",
            "/usr/lib",
        ] {
            assert!(!valid_mount_path(bad), "{bad}");
        }
    }

    #[test]
    fn mount_script_formats_only_when_blkid_says_empty() {
        let s = mount_script("fluxvol0", "/mnt/data");
        assert!(s.contains("virtio-fluxvol0"));
        assert!(s.contains("[ $rc -eq 2 ]"), "{s}");
        assert!(s.contains("exit 4"));
        assert!(s.contains("nofail"));
    }

    #[test]
    fn command_rules_check_every_piece_of_a_compound_command() {
        let r = CommandRules {
            allow: vec![],
            deny: vec!["rm *".into(), "sudo*".into(), "curl *|*".into()],
        };
        assert!(r.check("ls -la && cargo test").is_ok());
        assert!(r.check("echo hi; rm -rf /workspace").is_err());
        assert!(r.check("true || sudo reboot").is_err());
        assert!(r.check("cat x\nrm y").is_err());
        let a = CommandRules {
            allow: vec!["git *".into(), "ls".into(), "cargo *".into()],
            deny: vec!["git push*".into()],
        };
        assert!(a.check("git status && ls").is_ok());
        assert!(a.check("git push origin main").is_err());
        assert!(a.check("ls | wc -l").is_err(), "wc is not allowed");
        assert!(a.check("python3 x.py").is_err());
        assert!(r.check("'rm' '-rf' 'x'").is_err(), "CLI-quoted argv");
        assert!(r.check("r\"m\" x").is_err());
        assert!(glob_match("a*c", "abbbc"));
        assert!(!glob_match("a*c", "abbbd"));
        assert!(glob_match("*", ""));
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
        assert!(nat.snapshot && !nat.fork && nat.pty);
        assert!(capabilities_for("vz", true).pty);
        assert!(!capabilities_for("flux-vm", false).pty && !capabilities_for("qemu", false).pty);
        assert!(capabilities_for("vz", true).fork);
        assert!(!capabilities_for("cloud-hypervisor", true).snapshot);
        assert!(!capabilities_for("qemu", true).fork);
    }

    #[test]
    fn the_terminal_script_sizes_the_tty_and_quotes_the_command() {
        let s = pty_script(120, 40, Some("/workspace"), None);
        assert!(s.starts_with("stty cols 120 rows 40"));
        assert!(s.contains("cd '/workspace'"));
        assert!(s.contains("exec \"${SHELL:-/bin/sh}\" -l"));
        let c = pty_script(80, 24, None, Some("echo 'a b'; ls"));
        assert!(c.contains("exec sh -c 'echo '\\''a b'\\''; ls'"), "{c}");
        assert!(!c.contains("cd "));
    }

    #[test]
    fn trajectory_steps_are_ordered_and_numbered() {
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let out = number_steps(vec![
            (t("2026-01-01T00:00:02Z"), json!({"kind": "c"})),
            (t("2026-01-01T00:00:01Z"), json!({"kind": "a"})),
            (t("2026-01-01T00:00:01Z"), json!({"kind": "b"})),
        ]);
        let kinds: Vec<_> = out.iter().map(|v| v["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["a", "b", "c"]);
        assert_eq!(out[2]["seq"], 2);
        assert_eq!(out[0]["ts"], "2026-01-01T00:00:01Z");
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

    #[test]
    fn a_listing_parses_names_with_spaces_and_distinguishes_dir_from_file() {
        let raw = b"D\0f\t12\t1760000000.5\t644\ta file.txt\0d\t4096\t1760000001.0\t755\tsub\0";
        let (kind, e) = parse_listing(raw);
        assert_eq!(kind, "dir");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0]["name"], "a file.txt");
        assert_eq!(e[0]["type"], "file");
        assert_eq!(e[0]["size"], 12);
        assert_eq!(e[0]["mode"], 0o644);
        assert_eq!(e[1]["type"], "dir");
        assert_eq!(parse_listing(b"F\0f\t1\t1.0\t600\tx\0").0, "file");
    }

    #[test]
    fn file_paths_must_be_absolute_and_free_of_traversal() {
        assert!(valid_file_path("/workspace/a b/c.txt"));
        for bad in [
            "rel/x",
            "/a/../b",
            "/a\nb",
            "/a\0b",
            &format!("/{}", "x".repeat(5000)),
        ] {
            assert!(!valid_file_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn status_output_parses_branch_entries_and_renames() {
        let out =
            "## feature...origin/feature [ahead 1]\0 M src/a.rs\0?? new file.txt\0R  b.rs\0a.rs\0";
        let (branch, e) = parse_status(out);
        assert_eq!(branch, "feature...origin/feature [ahead 1]");
        assert_eq!(e.len(), 3);
        assert_eq!(e[0]["worktree"], "M");
        assert_eq!(e[0]["path"], "src/a.rs");
        assert_eq!(e[1]["index"], "?");
        assert_eq!(e[1]["path"], "new file.txt");
        assert_eq!(e[2]["path"], "b.rs");
        assert_eq!(e[2]["from"], "a.rs");
        let (b, e) = parse_status("## main\0");
        assert_eq!(b, "main");
        assert!(e.is_empty());
    }

    #[test]
    fn branch_names_cannot_be_read_as_options_or_escape() {
        for ok in ["main", "feature/x-1", "agent_2.0"] {
            assert!(valid_branch(ok), "{ok}");
        }
        for bad in [
            "", "-D", "--detach", "a..b", "a b", "a;b", "/x", "x/", ".x", "x.lock", "a@{1}", "a//b",
        ] {
            assert!(!valid_branch(bad), "{bad}");
        }
    }
}
