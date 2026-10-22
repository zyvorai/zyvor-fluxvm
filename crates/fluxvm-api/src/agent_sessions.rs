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
//! * `GET    /v1/agents/sessions/{id}/screenshot`    the guest display as a PNG (`vz`; scope `screen`)
//! * `POST   /v1/agents/sessions/{id}/input`         keyboard and mouse input to the display (`vz`; scope `screen`)
//! * `POST   /v1/agents/sessions/{id}/screen/wait`   block until the display changes (`vz`; scope `screen`)
//! * `GET    /v1/agents/sessions/{id}/probe`         which tools the guest image has (git, sha256sum, node, browsers ...)
//! * `POST   /v1/agents/sessions/{id}/grants`        give the session a brokered credential for named hosts (also GET, DELETE)
//! * `POST   /v1/agents/sessions/{id}/tokens`        mint an expiring token scoped to this session (also GET, DELETE .../{token})
//! * `POST   /v1/agents/sessions/{id}/checkpoint`    snapshot under a tag
//! * `POST   /v1/agents/sessions/{id}/restore`       restore a checkpoint
//! * `POST   /v1/agents/sessions/{id}/fork`          fork into child sessions (in-tree KVM only)
//! * `POST   /v1/agents/sessions/{id}/promote`       apply a fork child's workspace changes to its parent (dry run, review, apply)
//! * `GET    /v1/agents/sessions/{id}/events`        ledger plus the sandbox's VM events
//! * `GET    /v1/agents/sessions/{id}/artifacts`     workspace changes since the baseline
//! * `POST   /v1/agents/sessions/{id}/pty-ticket`    a 30 s single-use ticket for a browser terminal
//! * `GET    /console/pty/{ticket}`                  the terminal WebSocket for that ticket (no bearer: the ticket is it)
//! * `GET    /v1/agents/sessions/{id}/ssh`           how to reach the guest with plain ssh (same host as the daemon)
//! * `GET    /v1/agents/sessions/{id}/pty`           WebSocket: an interactive shell on a pseudo-terminal
//! * `GET    /v1/agents/sessions/{id}/trajectory`    ledger and VM events merged into one ordered list
//! * `POST   /v1/agents/sessions/{id}/preview`       mint an expiring preview URL for a guest port
//! * `GET    /v1/agents/sessions/{id}/previews`      list live preview URLs
//! * `DELETE /v1/agents/sessions/{id}/preview/{token}` revoke one
//! * `ANY    /preview/{token}/{*path}`               the preview itself (the token is the credential)
//! * `POST   /v1/agents/sessions/{id}/jobs`          start a detached command
//! * `GET    /v1/agents/sessions/{id}/jobs/{job}`    its state and output from `?offset=`
//! * `DELETE /v1/agents/sessions/{id}/jobs/{job}`    stop it
//! * `GET    /v1/agents/sessions/{id}/jobs`          list jobs (command, state, exit code)
//! * `GET    /v1/agents/sessions/{id}/events/stream` Server-Sent Events: ledger and VM events as they happen, resumable with `Last-Event-ID`
//! * `GET    /v1/agents/sessions/{id}/jobs/{job}/stream` Server-Sent Events: output from `?offset=` / `Last-Event-ID`, then `exit`
//! * `POST   /v1/agents/sessions/{id}/jobs/{job}/stdin` write to a job started with `stdin: true`
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

use super::agent_session_approvals as approvals;
use super::agent_session_auth::{self, Op, SessionScope, authorize};
use super::agent_session_transfer::{self as transfer, Chunk, Upload, UploadState};
use super::{ApiError, ApiResult, AuditActor, TokenTenant, change_error, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{any, get, post},
};
use chrono::{DateTime, Utc};
use fluxvm_core::config::Role;
use fluxvm_guest_protocol::{
    AgentRequest, AgentResponse, ExecPolicy, ExecTcpRule, JobPhase, JobStatus, JobStream,
};
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
        .route("/v1/agents/sessions/{id}/probe", get(probe))
        .route("/v1/agents/sessions/{id}/screenshot", get(screenshot))
        .route("/v1/agents/sessions/{id}/input", post(screen_input))
        .route("/v1/agents/sessions/{id}/screen/wait", post(screen_wait))
        .route(
            "/v1/agents/sessions/{id}/grants",
            post(grant_create).get(grant_list).delete(grant_revoke_all),
        )
        .route(
            "/v1/agents/sessions/{id}/grants/{grant}",
            axum::routing::delete(grant_revoke),
        )
        .route(
            "/v1/agents/sessions/{id}/tokens",
            post(mint_token).get(list_tokens),
        )
        .route(
            "/v1/agents/sessions/{id}/tokens/{token}",
            axum::routing::delete(revoke_token),
        )
        .route("/v1/agents/sessions/{id}/checkpoint", post(checkpoint))
        .route("/v1/agents/sessions/{id}/restore", post(restore))
        .route("/v1/agents/sessions/{id}/rewind", get(rewind_list))
        .route(
            "/v1/agents/sessions/{id}/rewind/config",
            post(rewind_config),
        )
        .route("/v1/agents/sessions/{id}/rewind/undo", post(rewind_undo))
        .route(
            "/v1/agents/sessions/{id}/rewind/branch",
            post(rewind_branch),
        )
        .route(
            "/v1/agents/sessions/{id}/rewind/{tag}/screenshot",
            get(rewind_screenshot),
        )
        .route(
            "/v1/agents/sessions/{id}/rewind/pin",
            post(rewind_pin_point),
        )
        .route(
            "/v1/agents/sessions/{id}/rewind/bisect",
            post(rewind_bisect),
        )
        .route("/v1/agents/sessions/{id}/fork", post(fork))
        .route("/v1/agents/sessions/{id}/promote", post(promote))
        .route("/v1/agents/sessions/{id}/events", get(events))
        .route("/v1/agents/sessions/{id}/artifacts", get(artifacts))
        .route("/v1/agents/sessions/{id}/trajectory", get(trajectory))
        .route("/v1/agents/sessions/{id}/pty", get(pty))
        .route("/v1/agents/sessions/{id}/pty-ticket", post(pty_ticket))
        .route("/console/pty/{ticket}", get(pty_by_ticket))
        .route("/v1/agents/sessions/{id}/ssh", get(ssh_target))
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
        .route("/v1/agents/sessions/{id}/files/range", get(files_range))
        .route(
            "/v1/agents/sessions/{id}/uploads",
            post(upload_create).get(upload_list),
        )
        .route(
            "/v1/agents/sessions/{id}/uploads/{upload}",
            get(upload_get)
                .delete(upload_abort)
                .put(upload_chunk)
                .layer(axum::extract::DefaultBodyLimit::max(
                    transfer::MAX_CHUNK_BYTES + 4096,
                )),
        )
        .route(
            "/v1/agents/sessions/{id}/uploads/{upload}/complete",
            post(upload_complete),
        )
        .route("/v1/agents/sessions/{id}/git/status", get(git_status))
        .route("/v1/agents/sessions/{id}/git/diff", get(git_diff))
        .route("/v1/agents/sessions/{id}/git/log", get(git_log))
        .route("/v1/agents/sessions/{id}/git/commit", post(git_commit))
        .route("/v1/agents/sessions/{id}/git/branch", post(git_branch))
        .route("/v1/agents/sessions/{id}/approvals", get(approval_list))
        .route(
            "/v1/agents/sessions/{id}/approvals/{approval}/approve",
            post(approval_approve),
        )
        .route(
            "/v1/agents/sessions/{id}/approvals/{approval}/deny",
            post(approval_deny),
        )
        .route(
            "/v1/agents/sessions/{id}/jobs",
            post(job_start).get(job_list),
        )
        .route("/v1/agents/sessions/{id}/events/stream", get(events_stream))
        .route(
            "/v1/agents/sessions/{id}/jobs/{job}/stream",
            get(job_stream),
        )
        .route("/v1/agents/sessions/{id}/jobs/{job}/stdin", post(job_stdin))
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
    /// The guest enforced the session's filesystem confinement when probed at create. False
    /// for sessions without a `filesystem` policy.
    #[serde(default)]
    confinement: bool,
    /// The guest enforced the session's executable allow-list (`policy.process.enforce = "exec"`)
    /// when probed at create: only the listed programs can start, whatever shell builds the command.
    #[serde(default)]
    exec_allowlist: bool,
    /// The egress proxy enforces the session's `network.allow` method/path rules for this
    /// sandbox. False for sessions without such rules.
    #[serde(default)]
    http_policy: bool,
    /// How exec reaches the guest: `agent` (the guest agent over vsock: hardened exec, job
    /// supervisor, confinement) or `ssh` (vz guests without the agent: exec works, background
    /// children can hold a command open until its timeout and jobs run as shell scripts). Empty
    /// where it was not probed.
    #[serde(default)]
    engine: String,
    /// The guest agent has the job supervisor (`jobs-v2`).
    #[serde(default)]
    supervisor: bool,
    /// A memory checkpoint (`memory: true`) can be taken: the VM's device set can be saved. Probed at create on vz;
    /// when false, `memory_checkpoint_reason` says why and a checkpoint needs `memory: false`.
    #[serde(default)]
    memory_checkpoint: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_checkpoint_reason: Option<String>,
    /// `macos` for a macOS guest (no guest agent: SSH engine only, no fork, no confinement, open network); empty for Linux.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    guest_os: String,
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
    /// `process.enforce = "exec"`: the allow and ask programs are also compiled into the guest's
    /// executable allow-list (Landlock), which holds even where the command string does not show
    /// what runs.
    #[serde(default)]
    enforce_exec: bool,
    /// Parts of the policy that are accepted but not enforced.
    unenforced: Vec<String>,
    /// The guest confinement (Landlock + seccomp) applied to exec, jobs, files and git. Present
    /// only when the policy has a `filesystem` section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exec: Option<ExecPolicy>,
    /// `strict` (default): create fails if the guest cannot enforce the filesystem rules.
    /// `best-effort`: the session is created and `capabilities.confinement` says what happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enforcement: Option<String>,
    /// Method/host/path rules compiled from `network.allow` entries that name methods or paths
    /// (`docs/http-acl.md` grammar), enforced by the host's egress proxy next to the host-wide
    /// rules: a request must pass both.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    http_rules: Vec<String>,
    /// `strict` (default): create fails if the sandbox cannot be filtered. `best-effort`: the
    /// session is created and `capabilities.http_policy` says `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    network_enforcement: Option<String>,
    /// The rules could not be re-bound to the sandbox's address after a restore or fork, so its
    /// traffic is no longer attributed to them. Exec and jobs are refused until it is recreated.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    http_unbound: bool,
}

/// Paths every confined command may read: enough for a shell, a toolchain and `/proc`.
const CONFINED_READ: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/proc", "/sys", "/dev",
    "/run",
];
/// Paths every confined command may write: scratch space (job logs live under `/tmp`) and the
/// harmless device files shells open.
const CONFINED_WRITE: &[&str] = &["/tmp", "/var/tmp", "/dev/null", "/dev/tty", "/dev/shm"];

/// Directories a bare program name in `process.allow` is looked up in. The guest skips the
/// candidates that do not exist (and reports them), so listing them all costs nothing.
const EXEC_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
    "/usr/local/sbin",
    "/usr/sbin",
    "/sbin",
];
/// Programs every enforced session needs whatever its rules say: the shell commands run through,
/// `true` (the create-time probe) and the dynamic loader the kernel executes for dynamic programs.
const EXEC_BASE: &[&str] = &[
    "/bin/sh",
    "/usr/bin/sh",
    "/bin/true",
    "/usr/bin/true",
    "/lib64/ld-linux-x86-64.so.2",
    "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
    "/lib/ld-linux-aarch64.so.1",
    "/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
    "/lib/ld-musl-x86_64.so.1",
    "/lib/ld-musl-aarch64.so.1",
];

/// The program a `process` pattern names: its first word. With `enforce = "exec"` it has to be
/// literal, because the kernel enforces paths and cannot match a glob.
fn pattern_program(pat: &str) -> Result<String, String> {
    let word = pat
        .split_whitespace()
        .next()
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"'))
        .unwrap_or_default();
    if word.is_empty() || word.contains(['*', '?', '[', ']', '{', '}', '$', '`']) {
        return Err(format!(
            "process pattern `{pat}`: enforce = \"exec\" needs a literal program name or absolute path \
             as its first word"
        ));
    }
    if word.contains('/') {
        return confine_path(word)
            .filter(|p| !word.ends_with('/') && p == word)
            .ok_or_else(|| {
                format!(
                    "process pattern `{pat}`: the program path must be absolute and without `..`"
                )
            });
    }
    Ok(word.to_string())
}

/// Compile the programs the `allow` and `ask` patterns name into guest executable paths.
fn exec_allow_paths(rules: &CommandRules) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = EXEC_BASE.iter().map(|s| s.to_string()).collect();
    for pat in rules.allow.iter().chain(&rules.ask) {
        let prog = pattern_program(pat)?;
        if prog.starts_with('/') {
            out.push(prog);
        } else {
            out.extend(EXEC_DIRS.iter().map(|d| format!("{d}/{prog}")));
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// One `filesystem.read` / `filesystem.write` entry: an absolute path with no `..`. A trailing
/// `/**` or `/*` is accepted and means the directory (Landlock rules are subtree rules).
fn confine_path(raw: &str) -> Option<String> {
    let t = raw.trim();
    let t = t
        .strip_suffix("/**")
        .or_else(|| t.strip_suffix("/*"))
        .unwrap_or(t);
    let t = if t.len() > 1 {
        t.trim_end_matches('/')
    } else {
        t
    };
    let ok = t.starts_with('/')
        && t.len() <= 512
        && !t.split('/').any(|c| c == "..")
        && !t.chars().any(|c| c.is_control() || "*?[]{}".contains(c));
    ok.then(|| t.to_string())
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
    /// Commands that run only after an operator approves them (see `agent_session_approvals`).
    /// A piece matching an `ask` pattern also satisfies a non-empty `allow` list.
    #[serde(default)]
    ask: Vec<String>,
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
            if !self.allow.is_empty()
                && !self.allow.iter().any(|a| glob_match(a, &piece))
                && !self.ask.iter().any(|a| glob_match(a, &piece))
            {
                return Err(format!("`{piece}` matches no allow rule"));
            }
        }
        Ok(())
    }

    /// The `ask` pattern a command matches, if any: running it needs an approval.
    fn ask_match(&self, cmd: &str) -> Option<String> {
        command_pieces(cmd)
            .iter()
            .find_map(|piece| self.ask.iter().find(|a| glob_match(a, piece)).cloned())
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
    /// Limits set at create; `None` means unlimited. Usage lives in its own file (see `Usage`),
    /// so counting an exec never changes `revision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    budget: Option<Budget>,
    /// Bumped by every save. `If-Match: <revision>` on checkpoint, restore, fork and delete
    /// makes the call fail with 412 when the session changed since the caller read it.
    #[serde(default)]
    revision: u64,
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

async fn save(m: &VmManager, s: &mut Session) -> ApiResult<()> {
    s.revision += 1;
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

// ---------------------------------------------------------------------------------------
// Budgets: limits a session was created with, and the usage counted against them.

/// Limits on what a session may consume. Every field is optional; a limit that is absent does
/// not apply. A fork child gets the parent's *remaining* budget, so forking cannot multiply it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct Budget {
    /// Commands run through `exec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_execs: Option<u64>,
    /// Wall-clock seconds spent in `exec`, summed. Checked when a command starts (a command is
    /// also given at most the seconds left as its timeout), so parallel commands can overshoot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_exec_seconds: Option<u64>,
    /// Background jobs started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_jobs: Option<u64>,
    /// Background jobs running at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_concurrent_jobs: Option<u64>,
    /// Fork children created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_children: Option<u64>,
    /// Wall-clock seconds of `exec` plus background jobs (a running job counts up to now).
    /// Checked when a command or job starts; a command is also given at most what is left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_compute_seconds: Option<u64>,
    /// Bytes of output one `exec` returns per stream (the rest is dropped, `output_truncated`
    /// says so) and bytes a job retains per stream. The command is not slowed or killed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_output_bytes: Option<u64>,
    /// Size of the session's workspace directory, measured with `du` in the guest when a command
    /// or job starts. Growth during a run is not stopped; the next start is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_workspace_bytes: Option<u64>,
}

/// What a session has used. Counted when a command, job or fork is accepted.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct Usage {
    execs: u64,
    exec_ms: u64,
    jobs: u64,
    children: u64,
    /// The workspace size at the last check, when a workspace budget is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_bytes: Option<u64>,
}

fn validate_budget(b: Option<Budget>) -> Result<Option<Budget>, ApiError> {
    let Some(b) = b else { return Ok(None) };
    for (name, v) in [
        ("max_execs", b.max_execs),
        ("max_exec_seconds", b.max_exec_seconds),
        ("max_jobs", b.max_jobs),
        ("max_concurrent_jobs", b.max_concurrent_jobs),
        ("max_children", b.max_children),
        ("max_compute_seconds", b.max_compute_seconds),
    ] {
        if v == Some(0) || v.is_some_and(|n| n > 10_000_000) {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!("budget.{name} must be 1-10000000"),
            });
        }
    }
    for (name, v, max) in [
        ("max_output_bytes", b.max_output_bytes, JOB_MAX_OUTPUT),
        ("max_workspace_bytes", b.max_workspace_bytes, 1 << 40),
    ] {
        if v == Some(0) || v.is_some_and(|n| n > max) {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!("budget.{name} must be 1-{max}"),
            });
        }
    }
    Ok((b != Budget::default()).then_some(b))
}

fn budget_exhausted(what: &str, used: u64, limit: u64) -> ApiError {
    ApiError {
        status: StatusCode::FORBIDDEN,
        message: format!("this session's {what} budget is used up ({used} of {limit})"),
    }
}

impl Budget {
    /// Whether one more `exec` may start under `u`, and the seconds it may run (`None`: no time
    /// limit). Counts nothing.
    fn check_exec(&self, u: &Usage, job_ms: u64) -> Result<Option<u64>, ApiError> {
        if let Some(max) = self.max_execs
            && u.execs >= max
        {
            return Err(budget_exhausted("exec", u.execs, max));
        }
        let mut left = None;
        if let Some(max) = self.max_exec_seconds {
            let used = u.exec_ms.div_ceil(1000);
            if used >= max {
                return Err(budget_exhausted("exec time (seconds)", used, max));
            }
            left = Some(max - used);
        }
        if let Some(max) = self.max_compute_seconds {
            let used = (u.exec_ms + job_ms).div_ceil(1000);
            if used >= max {
                return Err(budget_exhausted("compute time (seconds)", used, max));
            }
            left = Some(left.map_or(max - used, |l| l.min(max - used)));
        }
        Ok(left)
    }

    fn check_job(&self, u: &Usage, running: u64, job_ms: u64) -> Result<(), ApiError> {
        if let Some(max) = self.max_compute_seconds {
            let used = (u.exec_ms + job_ms).div_ceil(1000);
            if used >= max {
                return Err(budget_exhausted("compute time (seconds)", used, max));
            }
        }
        if let Some(max) = self.max_concurrent_jobs
            && running >= max
        {
            return Err(budget_exhausted("concurrent job", running, max));
        }
        if let Some(max) = self.max_jobs
            && u.jobs >= max
        {
            return Err(budget_exhausted("job", u.jobs, max));
        }
        Ok(())
    }

    fn check_fork(&self, u: &Usage, count: u64) -> Result<(), ApiError> {
        if let Some(max) = self.max_children
            && u.children + count > max
        {
            return Err(ApiError {
                status: StatusCode::FORBIDDEN,
                message: format!(
                    "this session's fork budget is {max} children and {} are already used",
                    u.children
                ),
            });
        }
        Ok(())
    }

    /// What is left after `u`: the budget a fork child starts with.
    fn remaining(&self, u: &Usage) -> Budget {
        let left = |max: Option<u64>, used: u64| max.map(|m| m.saturating_sub(used).max(1));
        Budget {
            max_execs: left(self.max_execs, u.execs),
            max_exec_seconds: left(self.max_exec_seconds, u.exec_ms.div_ceil(1000)),
            max_jobs: left(self.max_jobs, u.jobs),
            max_concurrent_jobs: self.max_concurrent_jobs,
            max_children: left(self.max_children, u.children),
            // The child's own jobs start at zero, so it gets the parent's compute that is left.
            max_compute_seconds: left(self.max_compute_seconds, u.exec_ms.div_ceil(1000)),
            max_output_bytes: self.max_output_bytes,
            max_workspace_bytes: self.max_workspace_bytes,
        }
    }
}

static USAGE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn usage_path(m: &VmManager, id: Uuid) -> PathBuf {
    m.cfg
        .state_dir
        .join("agent-session-usage")
        .join(format!("{id}.json"))
}

async fn usage_read(m: &VmManager, id: Uuid) -> Usage {
    match tokio::fs::read(usage_path(m, id)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Usage::default(),
    }
}

/// Applies `f` to the usage under a lock; if it returns `Err` nothing is written. A write
/// failure is an error too: a limit that cannot be counted must not be silently skipped.
async fn usage_update<T>(
    m: &VmManager,
    id: Uuid,
    f: impl FnOnce(&mut Usage) -> Result<T, ApiError>,
) -> ApiResult<T> {
    let _g = USAGE_LOCK.lock().await;
    let mut u = usage_read(m, id).await;
    let out = f(&mut u)?;
    let path = usage_path(m, id);
    if let Some(d) = path.parent() {
        tokio::fs::create_dir_all(d)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec(&u).map_err(anyhow::Error::from)?)
        .await
        .map_err(anyhow::Error::from)?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(out)
}

/// Accepts one `exec` against the budget and counts it. Returns the most seconds the command may
/// run (the seconds left), or `None` when the budget has no time limit.
async fn budget_start_exec(m: &VmManager, s: &Session) -> ApiResult<Option<u64>> {
    let job_ms = match &s.budget {
        Some(b) if b.max_compute_seconds.is_some() => job_compute_ms(m, s.id).await,
        _ => 0,
    };
    usage_update(m, s.id, |u| {
        // No limits: still counted, so usage is reported for every session.
        let left = match &s.budget {
            Some(b) => b.check_exec(u, job_ms)?,
            None => None,
        };
        u.execs += 1;
        Ok(left)
    })
    .await
}

/// Wall-clock milliseconds the session's background jobs have run: finished ones from their
/// metadata, running ones up to now.
async fn job_compute_ms(m: &VmManager, id: Uuid) -> u64 {
    let now = Utc::now();
    job_meta_read(m, id)
        .await
        .iter()
        .map(|j| {
            let end = j.finished_at.unwrap_or(now);
            (end - j.started_at).num_milliseconds().max(0) as u64
        })
        .sum()
}

/// Cuts `stdout` and `stderr` of an exec answer to `cap` bytes each (on a character boundary) and
/// sets `output_truncated`. Returns whether anything was cut.
fn cap_output(out: &mut Value, cap: usize) -> bool {
    let mut cut = false;
    for key in ["stdout", "stderr"] {
        let Some(text) = out[key].as_str() else {
            continue;
        };
        if text.len() > cap {
            let mut end = cap;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            let dropped = text.len() - end;
            let kept = text[..end].to_string();
            out[key] = json!(kept);
            out[format!("{key}_dropped_bytes")] = json!(dropped);
            cut = true;
        }
    }
    if cut {
        out["output_truncated"] = json!(true);
    }
    cut
}

/// Refuses a command or job start when the workspace is over `max_workspace_bytes`. The size
/// comes from `du` in the guest, so the sandbox must be running.
async fn budget_check_workspace(m: &VmManager, s: &Session) -> ApiResult<()> {
    let (Some(max), Some(ws)) = (
        s.budget.as_ref().and_then(|b| b.max_workspace_bytes),
        s.workspace.as_ref(),
    ) else {
        return Ok(());
    };
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            format!("du -sk {} 2>/dev/null | cut -f1", shell_quote(&ws.path)),
            Some(30),
            s.policy.exec.clone(),
        )
        .await?;
    let kib = json!(res)["stdout"]
        .as_str()
        .and_then(|o| o.trim().parse::<u64>().ok())
        .ok_or_else(|| ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: "could not measure the workspace size (needs `du` in the guest)".into(),
        })?;
    let used = kib.saturating_mul(1024);
    let _ = usage_update(m, s.id, |u| {
        u.workspace_bytes = Some(used);
        Ok(())
    })
    .await;
    if used >= max {
        return Err(budget_exhausted("workspace size (bytes)", used, max));
    }
    Ok(())
}

async fn budget_finish_exec(m: &VmManager, id: Uuid, elapsed_ms: u64) {
    let _ = usage_update(m, id, |u| {
        u.exec_ms += elapsed_ms;
        Ok(())
    })
    .await;
}

// ---------------------------------------------------------------------------------------
// Lifecycle gate: conflicting operations on one session are refused, not interleaved.

#[derive(Default)]
struct Gate {
    /// The lifecycle operation in flight (checkpoint, restore, fork or delete).
    lifecycle: Option<&'static str>,
    /// Commands, jobs and mutating file/git calls currently running.
    work: u32,
}

static GATES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<Uuid, Gate>>> =
    std::sync::OnceLock::new();

fn with_gate<R>(id: Uuid, f: impl FnOnce(&mut Gate) -> R) -> R {
    let mut map = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let g = map.entry(id).or_default();
    let out = f(g);
    if g.lifecycle.is_none() && g.work == 0 {
        map.remove(&id);
    }
    out
}

fn gate_conflict(message: String) -> ApiError {
    ApiError {
        status: StatusCode::CONFLICT,
        message,
    }
}

/// Held while a checkpoint, restore, fork or delete runs.
struct LifecycleGuard(Uuid, Option<PathBuf>);

impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        with_gate(self.0, |g| g.lifecycle = None);
        // The operation ended, whether it succeeded or failed: it was not interrupted.
        if let Some(p) = &self.1 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn op_marker_path(m: &VmManager, id: Uuid) -> PathBuf {
    m.cfg
        .state_dir
        .join("agent-session-ops")
        .join(format!("{id}.json"))
}

impl LifecycleGuard {
    /// Writes a marker that exists only while the operation runs. A daemon that dies mid-way
    /// leaves it behind, and the session record then reports the interrupted operation, so a
    /// half-done restore or fork is visible rather than silent.
    fn mark(mut self, m: &VmManager, kind: &str) -> Self {
        let path = op_marker_path(m, self.0);
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let body = json!({"op": kind, "started_at": Utc::now(), "reported": false});
        if std::fs::write(&path, body.to_string()).is_ok() {
            self.1 = Some(path);
        }
        self
    }
}

/// The operation a previous daemon was running on this session when it stopped, if any. Reports it
/// to the ledger once.
fn interrupted_op(m: &VmManager, id: Uuid) -> Option<Value> {
    // Something in this process holds the slot: the operation is live, not interrupted.
    if with_gate(id, |g| g.lifecycle.is_some()) {
        return None;
    }
    let path = op_marker_path(m, id);
    let mut v: Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    if v["reported"] != json!(true) {
        record(
            m,
            id,
            "lifecycle.interrupted",
            json!({"op": v["op"], "started_at": v["started_at"]}),
        );
        v["reported"] = json!(true);
        let _ = std::fs::write(&path, v.to_string());
    }
    Some(json!({"op": v["op"], "started_at": v["started_at"]}))
}

/// Takes the session's lifecycle slot, or `409` naming what holds it. Restore and fork also
/// refuse while commands run (they would replace the machine under them); a checkpoint does
/// not, and delete does not either (it is how a stuck session is ended).
fn begin_lifecycle(id: Uuid, kind: &'static str) -> ApiResult<LifecycleGuard> {
    with_gate(id, |g| {
        if let Some(k) = g.lifecycle {
            return Err(gate_conflict(format!(
                "a {k} is already in progress on this session; retry when it finishes"
            )));
        }
        if matches!(kind, "restore" | "fork" | "promote") && g.work > 0 {
            return Err(gate_conflict(format!(
                "{} command(s) are still running on this session; a {kind} would replace the machine under them",
                g.work
            )));
        }
        g.lifecycle = Some(kind);
        Ok(LifecycleGuard(id, None))
    })
}

/// Held while a command, job start or mutating file/git call runs.
struct WorkGuard(Uuid);

impl Drop for WorkGuard {
    fn drop(&mut self) {
        with_gate(self.0, |g| g.work = g.work.saturating_sub(1));
    }
}

fn begin_work(id: Uuid) -> ApiResult<WorkGuard> {
    with_gate(id, |g| {
        if let Some(k) = g.lifecycle {
            return Err(gate_conflict(format!(
                "a {k} is in progress on this session; retry shortly"
            )));
        }
        g.work += 1;
        Ok(WorkGuard(id))
    })
}

/// `If-Match: <revision>` (quoted or not, or `*`): `412` when the session moved on.
fn check_if_match(h: &axum::http::HeaderMap, s: &Session) -> ApiResult<()> {
    let Some(v) = h.get(axum::http::header::IF_MATCH) else {
        return Ok(());
    };
    let raw = v.to_str().unwrap_or("").trim().trim_matches('"');
    if raw == "*" {
        return Ok(());
    }
    let want: u64 = raw.parse().map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "If-Match must be the session's revision number".into(),
    })?;
    if want != s.revision {
        return Err(ApiError {
            status: StatusCode::PRECONDITION_FAILED,
            message: format!(
                "the session changed: it is at revision {}, the request expected {want}",
                s.revision
            ),
        });
    }
    Ok(())
}

/// Brings a stored session in line with what the daemon knows. A session past its expiry is
/// persisted as `expired`; every route sees that. With `probe`, a sandbox the manager no longer
/// has is also reported as `gone`, in the response only: that lookup can fail for reasons that
/// are not the sandbox being deleted, so it is never written back and never changes what the
/// other routes do. Only the session record and the listing ask for it.
async fn reconcile(m: &VmManager, s: &mut Session, probe: bool) {
    if s.state != "running" {
        return;
    }
    if s.expires_at.is_some_and(|e| e <= Utc::now()) {
        s.state = "expired".into();
        if save(m, s).await.is_ok() {
            record(
                m,
                s.id,
                "session.expire",
                json!({"expired_at": s.expires_at}),
            );
        }
    } else if probe && m.get(s.sandbox_id).await.is_err() {
        s.state = "gone".into();
    }
}

async fn load(m: &VmManager, id: Uuid, tenant: &Option<String>) -> ApiResult<Session> {
    let bytes = tokio::fs::read(session_path(m, id))
        .await
        .map_err(|_| not_found(id))?;
    let mut s: Session = serde_json::from_slice(&bytes).map_err(anyhow::Error::from)?;
    // A tenant-scoped token only ever sees its own sessions (404, not 403, like sandboxes).
    if &s.tenant != tenant {
        return Err(not_found(id));
    }
    reconcile(m, &mut s, false).await;
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
    /// Host names (`example.com`, `*.example.com`), or `{host, methods?, paths?}` objects that
    /// limit what may be sent to a host. Method/path rules need the host's egress proxy.
    #[serde(default)]
    allow: Vec<Value>,
    /// `strict` (default) or `best-effort`; only matters when an entry names methods or paths.
    #[serde(default)]
    enforcement: Option<String>,
}

/// The proxy rules for a policy whose `network.allow` limits some hosts to methods or paths.
/// Once any allow rule exists the proxy denies everything that matches none, so every allowed
/// host also gets an unrestricted rule (the host and, as the host allow-list does, its
/// subdomains); a host with limits gets only the limited rules.
fn http_rules_for(
    allow_hosts: &[String],
    limited: &[(String, Vec<String>, Vec<String>)],
) -> Result<Vec<String>, String> {
    let mut rules = Vec::new();
    let is_limited = |h: &str| limited.iter().any(|(lh, _, _)| lh == h);
    // A host that appears both bare and limited stays limited: the narrower entry wins.
    let mut hosts: Vec<&String> = allow_hosts.iter().filter(|h| !is_limited(h)).collect();
    hosts.sort();
    hosts.dedup();
    for h in hosts {
        let bare = h.trim_start_matches("*.");
        rules.push(format!("allow * {bare}"));
        rules.push(format!("allow * *.{bare}"));
    }
    for (host, methods, paths) in limited {
        let bare = host.trim_start_matches("*.");
        let methods: Vec<String> = if methods.is_empty() {
            vec!["*".into()]
        } else {
            methods
                .iter()
                .map(|m| m.trim().to_ascii_uppercase())
                .collect()
        };
        for m in &methods {
            if m != "*"
                && !(!m.is_empty() && m.len() <= 16 && m.bytes().all(|b| b.is_ascii_uppercase()))
            {
                return Err(format!(
                    "network.allow methods: {m:?} is not an HTTP method"
                ));
            }
        }
        let paths: Vec<String> = if paths.is_empty() {
            vec![String::new()]
        } else {
            paths.iter().map(|p| p.trim().to_string()).collect()
        };
        for p in &paths {
            if !p.is_empty() && !p.starts_with('/') {
                return Err(format!("network.allow paths: {p:?} must start with /"));
            }
        }
        for m in &methods {
            for p in &paths {
                rules.push(format!("allow {m} {bare}{p}"));
                rules.push(format!("allow {m} *.{bare}{p}"));
            }
        }
    }
    rules.sort();
    rules.dedup();
    // The same parser the proxy uses, so a rule the proxy would refuse is refused here.
    fluxvm_network::http_acl::HttpAcl::parse(&rules)?;
    Ok(rules)
}

/// The guest policy of a session that confines the filesystem: the common read and write paths,
/// network reach left to the sandbox.
fn confined_default() -> ExecPolicy {
    ExecPolicy {
        read: CONFINED_READ.iter().map(|s| s.to_string()).collect(),
        write: CONFINED_WRITE.iter().map(|s| s.to_string()).collect(),
        // Network reach is the sandbox's job (no NIC, or the host egress proxy); Landlock
        // port rules would only break the proxy path, so the guest policy leaves it open.
        tcp_connect: ExecTcpRule::Any,
        tcp_bind: ExecTcpRule::Any,
        allow_unix: true,
        allow_udp: true,
        // Missing optional paths (/lib32, /dev/shm) are skipped and reported, not fatal.
        best_effort: true,
        ..Default::default()
    }
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
        match n.enforcement.as_deref() {
            None => {}
            Some(m @ ("strict" | "best-effort")) => r.network_enforcement = Some(m.into()),
            Some(_) => {
                return Err(bad(
                    "network.enforcement must be strict or best-effort".into()
                ));
            }
        }
        // Entries that limit methods or paths, as (host, methods, paths).
        let mut limited: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
        for item in n.allow {
            match item {
                Value::String(h) => r.allow_hosts.push(h),
                Value::Object(o) => {
                    if let Some(k) = o
                        .keys()
                        .find(|k| !matches!(k.as_str(), "host" | "methods" | "paths"))
                    {
                        return Err(bad(format!(
                            "unknown network.allow key `{k}` (use host, methods, paths)"
                        )));
                    }
                    let Some(host) = o.get("host").and_then(Value::as_str) else {
                        return Err(bad("allow entry needs a host".into()));
                    };
                    let strs = |key: &str| -> Result<Vec<String>, ApiError> {
                        match o.get(key) {
                            None => Ok(Vec::new()),
                            Some(Value::Array(a)) => a
                                .iter()
                                .map(|v| {
                                    v.as_str().map(str::to_string).ok_or_else(|| {
                                        bad(format!("network.allow {key} entries are strings"))
                                    })
                                })
                                .collect(),
                            Some(_) => Err(bad(format!("network.allow {key} must be a list"))),
                        }
                    };
                    let (methods, paths) = (strs("methods")?, strs("paths")?);
                    r.allow_hosts.push(host.to_string());
                    if !methods.is_empty() || !paths.is_empty() {
                        limited.push((host.to_string(), methods, paths));
                    }
                }
                _ => return Err(bad("allow entries are host names or objects".into())),
            }
        }
        if !limited.is_empty() {
            r.http_rules = http_rules_for(&r.allow_hosts, &limited).map_err(bad)?;
        }
        if default == "deny" && r.allow_hosts.is_empty() && p.preset.as_deref() != Some("open") {
            r.offline = true;
        }
    }
    if let Some(v) = p.filesystem {
        let obj = v
            .as_object()
            .ok_or_else(|| bad("filesystem must be an object with read and/or write".into()))?;
        let mut exec = confined_default();
        for (k, val) in obj {
            let list = match k.as_str() {
                "read" => &mut exec.read,
                "write" => &mut exec.write,
                "enforcement" => {
                    match val.as_str() {
                        Some(m @ ("strict" | "best-effort")) => r.enforcement = Some(m.into()),
                        _ => {
                            return Err(bad(
                                "filesystem.enforcement must be strict or best-effort".into(),
                            ));
                        }
                    }
                    continue;
                }
                other => return Err(bad(format!("unknown filesystem rule `{other}`"))),
            };
            for e in val
                .as_array()
                .ok_or_else(|| bad(format!("filesystem.{k} must be a list of paths")))?
            {
                let path = e.as_str().and_then(confine_path).ok_or_else(|| {
                    bad(format!(
                        "filesystem.{k} entries are absolute paths without `..` or wildcards \
                         (a trailing /** means the directory)"
                    ))
                })?;
                list.push(path);
            }
        }
        exec.read.sort();
        exec.read.dedup();
        exec.write.sort();
        exec.write.dedup();
        r.exec = Some(exec);
    }
    if let Some(v) = p.process {
        let obj = v
            .as_object()
            .ok_or_else(|| bad("process must be an object with allow and/or deny".into()))?;
        let mut rules = CommandRules::default();
        let mut enforce_exec = false;
        for (k, val) in obj {
            let list = match k.as_str() {
                "allow" => &mut rules.allow,
                "deny" => &mut rules.deny,
                "ask" => &mut rules.ask,
                "enforce" => {
                    match val.as_str() {
                        Some("exec") => enforce_exec = true,
                        Some("none") => {}
                        _ => {
                            return Err(bad("process.enforce must be \"exec\" or \"none\"".into()));
                        }
                    }
                    continue;
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
        if enforce_exec {
            if rules.allow.is_empty() && rules.ask.is_empty() {
                return Err(bad(
                    "process.enforce = \"exec\" needs a non-empty allow (or ask) list: with none, \
                     nothing would be permitted to run"
                        .into(),
                ));
            }
            let paths = exec_allow_paths(&rules).map_err(bad)?;
            // Confinement is needed for the execute right to apply, so a policy without a
            // `filesystem` section gets the default one.
            let exec = r.exec.get_or_insert_with(confined_default);
            exec.exec = Some(paths);
            r.enforce_exec = true;
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
    #[serde(default)]
    budget: Option<Budget>,
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
    /// Fail the create (and remove the sandbox) unless the workspace directory exists and, when
    /// `git` is given, the clone succeeded. Default: the session is made and `workspace.note`
    /// says what went wrong.
    #[serde(default)]
    require_ready: bool,
}

#[derive(Debug, Deserialize)]
struct RuntimeBody {
    /// Named host volumes (`POST /v1/volumes`) attached as disks. vz only; see `docs/agent-sessions.md`.
    #[serde(default)]
    volumes: Vec<VolumeBody>,
    #[serde(default)]
    image: Option<String>,
    /// A named template under `sandbox.templates_dir` (its `spec.json`). A macOS guest comes from one: build it with
    /// `scripts/macos-session-template.sh`. Not combinable with `image`.
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    cpus: Option<u8>,
    #[serde(default)]
    memory_mb: Option<u64>,
    /// `"agent"` refuses a session whose guest would run on the SSH engine (no supervisor, no confine, no HTTP rules)
    /// instead of silently creating one. vz only; other backends have no SSH engine.
    #[serde(default)]
    engine: Option<String>,
    /// vz only: wire the host Metal/MLX (or llama.cpp) OpenAI endpoint into the session over vsock.
    #[serde(default)]
    host_gpu: bool,
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
        confinement: false,
        exec_allowlist: false,
        http_policy: false,
        engine: String::new(),
        supervisor: false,
        memory_checkpoint: matches!(backend, "flux-vm" | "qemu" | "firecracker"),
        memory_checkpoint_reason: None,
        guest_os: String::new(),
    }
}

/// A macOS guest has no guest agent and no way to restore a saved state onto another machine identity, so a session
/// on one is SSH-engine only, cannot fork, and runs on the Mac's NAT (an offline guest would lose SSH too).
fn macos_capabilities() -> Capabilities {
    Capabilities {
        snapshot: true,
        fork: false,
        pty: true,
        engine: "ssh".into(),
        guest_os: "macos".into(),
        ..capabilities_for("vz", false)
    }
}

/// Apple lets one host run two macOS VMs at once (a kernel limit, not FluxVM's).
const MAX_MACOS_GUESTS: usize = 2;

/// Why a macOS guest cannot honor a session policy, or `None`.
fn macos_policy_refusal(policy: &PolicyReport, has_volumes: bool) -> Option<String> {
    let what = if policy.offline || !policy.allow_hosts.is_empty() {
        "network.allow / offline (a macOS guest has no in-guest egress forwarder, and with no network card it has no SSH)"
    } else if !policy.http_rules.is_empty() {
        "HTTP method/path rules (they need the runner's relay, which a macOS guest does not use)"
    } else if has_volumes {
        "runtime.volumes (the mount script is Linux-only)"
    } else {
        return None;
    };
    Some(format!(
        "a macOS guest session cannot enforce {what}: it runs on the Mac's NAT with the SSH engine and no guest agent. \
         Remove it, or use a Linux guest"
    ))
}

/// Where a macOS session's workspace goes when none is named: the system volume is read-only, so under the user's home.
fn macos_default_workspace(user: Option<&str>) -> String {
    match user {
        Some(u) if valid_guest_user(u) => format!("/Users/{u}/workspace"),
        _ => "/Users/Shared/workspace".into(),
    }
}

fn valid_guest_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 32
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// The image an agent session gets when the caller named none: `agent-micro` when this host's
/// catalog has it (the guest agent is baked in), otherwise the sandbox default.
fn default_session_image(m: &VmManager) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let name = fluxvm_scheduler::vz_guest::AGENT_MICRO_IMAGE;
    fluxvm_image::catalog::list_with_verification(&m.cfg)
        .ok()?
        .iter()
        .any(|e| e.entry.name == name)
        .then(|| name.to_string())
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
    if s.capabilities.guest_os == "macos" {
        return Some(
            "a macOS guest cannot be forked: its saved state is bound to one machine identifier and auxiliary \
             storage. Take a checkpoint and restore it on this session, or create a second session from the template"
                .to_string(),
        );
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
    let ws_explicit = ws.and_then(|w| w.path.clone());
    let mut ws_path = ws_explicit.clone().unwrap_or_else(|| "/workspace".into());
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
    let budget = validate_budget(body.budget)?;
    if budget
        .as_ref()
        .is_some_and(|b| b.max_workspace_bytes.is_some())
        && body.workspace.is_none()
    {
        return Err(bad(
            "budget.max_workspace_bytes needs a workspace to measure",
        ));
    }

    // A macOS guest comes from a template; it has no guest agent, so what the session may ask for is narrower.
    let template = body.runtime.as_ref().and_then(|r| r.template.clone());
    let mut macos_user: Option<Option<String>> = None;
    if let Some(t) = &template {
        if body.runtime.as_ref().is_some_and(|r| r.image.is_some()) {
            return Err(bad(
                "runtime.template and runtime.image cannot both be set: the template names the image",
            ));
        }
        if t.is_empty() || t.len() > 64 || t.starts_with('.') || !valid_guest_user(t) {
            return Err(bad(
                "runtime.template must be 1-64 characters of [A-Za-z0-9._-]",
            ));
        }
        let spec = m
            .load_template_spec(t)
            .await
            .map_err(|e| bad(&format!("template {t:?}: {e:#}")))?;
        if spec
            .apple
            .as_ref()
            .is_some_and(|a| a.guest_os == fluxvm_core::model::AppleGuest::Macos)
        {
            // The SSH user: `cloud_init.user` (what `vz_guest` reaches first), else the provisioned account.
            macos_user = Some(
                spec.cloud_init
                    .as_ref()
                    .and_then(|c| c.user.clone())
                    .or_else(|| {
                        spec.apple
                            .as_ref()
                            .and_then(|a| a.provision_username.clone())
                    }),
            );
        }
    }
    if let Some(user) = &macos_user {
        if !cfg!(target_os = "macos") {
            return Err(bad("macOS guests run only on a Mac host"));
        }
        if let Some(why) = macos_policy_refusal(
            &policy,
            body.runtime.as_ref().is_some_and(|r| !r.volumes.is_empty()),
        ) {
            return Err(bad(&why));
        }
        if ws_explicit.is_none() {
            ws_path = macos_default_workspace(user.as_deref());
        }
        let running = m
            .list()
            .await
            .iter()
            .filter(|v| {
                v.status == fluxvm_core::model::VmStatus::Running
                    && v.request
                        .apple
                        .as_ref()
                        .is_some_and(|a| a.guest_os == fluxvm_core::model::AppleGuest::Macos)
            })
            .count();
        if running >= MAX_MACOS_GUESTS {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!(
                    "{running} macOS guests are already running; Apple allows {MAX_MACOS_GUESTS} at once on one host. \
                     Stop one, or delete a session"
                ),
            });
        }
    }

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
    let require_agent = match body.runtime.as_ref().and_then(|r| r.engine.as_deref()) {
        None => false,
        Some("agent") => true,
        Some(other) => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!("runtime.engine must be \"agent\" (got {other:?})"),
            });
        }
    };
    let actor_name = actor.as_ref().map(|a| a.0.0.as_str());
    let id = Uuid::new_v4();
    let req = fluxvm_scheduler::SandboxCreateRequest {
        name: Some(format!("agent-{}", &id.to_string()[..8])),
        image: if template.is_some() {
            None
        } else {
            body.runtime
                .as_ref()
                .and_then(|r| r.image.clone())
                .or_else(|| default_session_image(&m))
        },
        template: template.clone(),
        ttl_seconds: body.ttl_seconds,
        vcpus: body.runtime.as_ref().and_then(|r| r.cpus),
        memory_mib: body.runtime.as_ref().and_then(|r| r.memory_mb),
        offline: policy.offline,
        allow_hosts: policy.allow_hosts.clone(),
        host_gpu: body.runtime.as_ref().is_some_and(|r| r.host_gpu),
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
        capabilities: if macos_user.is_some() {
            macos_capabilities()
        } else {
            capabilities_for(&backend, policy.offline || !policy.allow_hosts.is_empty())
        },
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
        budget: budget.clone(),
        revision: 0,
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
            let mk = if macos_user.is_some() {
                // The system volume is read-only and the guest user owns its home.
                format!("mkdir -p {p}")
            } else {
                format!(
                    "mkdir -p {p} 2>/dev/null || (sudo -n mkdir -p {p} && sudo -n chown \"$(id -u):$(id -g)\" {p})"
                )
            };
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
        if w.require_ready && (state.note.is_some() || (w.git.is_some() && !state.cloned)) {
            let why = state
                .note
                .clone()
                .unwrap_or_else(|| "the workspace was not initialized".into());
            // Nothing was promised yet: remove the sandbox and the record.
            let _ = m.delete(rec.id).await;
            let _ = std::fs::remove_file(session_path(&m, id));
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!("the workspace is not ready: {why}"),
            });
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
    if session.policy.exec.is_some() {
        // The workspace and volumes are the agent's to write; they are known only now. The setup
        // above ran unconfined: it executed fixed scripts before the policy applied.
        if let Some(e) = session.policy.exec.as_mut() {
            if let Some(w) = &session.workspace {
                e.write.push(w.path.clone());
            }
            e.write
                .extend(session.volumes.iter().map(|v| v.path.clone()));
            e.write.sort();
            e.write.dedup();
        }
        let probe = match m.ensure_running_for_request(rec.id).await {
            Ok(_) => m
                .exec_with_policy(rec.id, "true".into(), Some(30), session.policy.exec.clone())
                .await
                .ok(),
            Err(_) => None,
        };
        let enforcement = probe
            .and_then(|r| serde_json::to_value(r).ok())
            .map(|v| v["enforcement"].clone());
        let enforced = enforcement
            .as_ref()
            .and_then(|e| e["filesystem"].as_bool())
            .unwrap_or(false);
        let exec_enforced = enforcement
            .as_ref()
            .and_then(|e| e["exec_restricted"].as_bool())
            .unwrap_or(false);
        record(&m, id, "policy.probe", json!({"enforcement": enforcement}));
        if session.policy.enforce_exec && !exec_enforced {
            // No best-effort for this one: process.enforce = "exec" exists to be a boundary, and a
            // session that only looks restricted is worse than a refused create.
            let _ = m.delete(rec.id).await;
            let _ = std::fs::remove_file(session_path(&m, id));
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "the guest cannot enforce the executable allow-list on backend {} (needs a Linux \
                     guest with Landlock and an up-to-date fluxvm guest agent); \
                     remove process.enforce to use the command-string rules only",
                    session.backend
                ),
            });
        }
        session.capabilities.exec_allowlist = exec_enforced;
        if !enforced && session.policy.enforcement.as_deref() != Some("best-effort") {
            let _ = m.delete(rec.id).await;
            let _ = std::fs::remove_file(session_path(&m, id));
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!(
                    "the guest cannot enforce the filesystem policy on backend {} (needs a Linux guest \
                     with Landlock and the fluxvm guest agent: macOS guests and procbox sandboxes refuse). \
                     Set filesystem.enforcement to best-effort to create the session without it",
                    session.backend
                ),
            });
        }
        session.capabilities.confinement = enforced;
    }
    if !session.policy.http_rules.is_empty() {
        // The sandbox must be up for its guest address to be known.
        let started = m.ensure_running_for_request(rec.id).await;
        let applied = match started {
            Ok(_) => m
                .set_http_policy(rec.id, &session.policy.http_rules)
                .await
                .map_err(|e| format!("{e:#}")),
            Err(e) => Err(format!("the sandbox did not start: {e:#}")),
        };
        match applied {
            Ok(()) => session.capabilities.http_policy = true,
            Err(why) => {
                record(&m, id, "policy.http_unenforced", json!({"reason": why}));
                if session.policy.network_enforcement.as_deref() != Some("best-effort") {
                    let _ = m.delete(rec.id).await;
                    let _ = std::fs::remove_file(session_path(&m, id));
                    m.clear_http_policy(rec.id);
                    return Err(ApiError {
                        status: StatusCode::BAD_REQUEST,
                        message: format!(
                            "{why}. Set network.enforcement to best-effort to create the session \
                             with the host allow-list only"
                        ),
                    });
                }
            }
        }
    }
    if session.backend == "vz" {
        // Say which engine the session will actually get, so nobody finds out from a hung command.
        if m.ensure_running_for_request(rec.id).await.is_ok() {
            let supervisor = m.guest_jobs_supported(rec.id).await;
            session.capabilities.supervisor = supervisor;
            session.capabilities.engine = if supervisor { "agent" } else { "ssh" }.into();
            record(
                &m,
                id,
                "session.engine",
                json!({"engine": session.capabilities.engine, "supervisor": supervisor}),
            );
            if let Ok(v) = m.vz_save_support(rec.id).await {
                session.capabilities.memory_checkpoint = v["supported"].as_bool().unwrap_or(false);
                session.capabilities.memory_checkpoint_reason =
                    v["reason"].as_str().map(str::to_string);
                record(&m, id, "session.save_support", v);
            }
        }
    }
    if require_agent && session.backend == "vz" && session.capabilities.engine != "agent" {
        let _ = m.delete(rec.id).await;
        let _ = std::fs::remove_file(session_path(&m, id));
        m.clear_http_policy(rec.id);
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: "runtime.engine is \"agent\" but this guest would run on the SSH engine: the image has no \
                      guest agent (use agent-micro, or an image built with it)"
                .into(),
        });
    }
    save(&m, &mut session).await?;
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
                && let Ok(mut s) = serde_json::from_slice::<Session>(&b)
                && s.tenant == tenant
            {
                reconcile(&m, &mut s, true).await;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    reconcile(&m, &mut s, true).await;
    let mut out = json!(s);
    out["usage"] = json!(usage_read(&m, id).await);
    if let Some(op) = interrupted_op(&m, id) {
        out["interrupted_op"] = op;
    }
    Ok(Json(out))
}

async fn delete_session(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    let _gate = begin_lifecycle(id, "delete")?.mark(&m, "delete");
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    check_if_match(&headers, &s)?;
    if s.state != "deleted" {
        // The sandbox may already be gone (TTL); the session is still closed.
        if let Err(e) = m.delete(s.sandbox_id).await {
            tracing::warn!(session = %id, error = %format!("{e:#}"), "agent session: sandbox delete");
        }
        s.state = "deleted".into();
        save(&m, &mut s).await?;
        record(&m, id, "session.delete", json!({}));
    }
    session_cleanup(&m, &s).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes everything a finished session leaves besides its record and ledger: tokens, grants,
/// HTTP rules, previews, approvals, job metadata and usage. Safe to run again.
async fn session_cleanup(m: &VmManager, s: &Session) {
    agent_session_auth::purge(&m.cfg.state_dir, s.id).await;
    let _ = m.revoke_grant(s.sandbox_id, None).await;
    // The in-memory broker store, by sandbox id: this also covers a sandbox the manager no longer
    // knows (`revoke_grant` looks it up first).
    fluxvm_core::grants::global().remove_sandbox(&s.sandbox_id.to_string());
    let _ = tokio::fs::remove_file(job_meta_path(m, s.id)).await;
    let _ = tokio::fs::remove_file(usage_path(m, s.id)).await;
    let _ = tokio::fs::remove_file(rewind_path(m, s.id)).await;
    let _ = tokio::fs::remove_dir_all(rewind_path(m, s.id).with_extension("shots")).await;
    approvals::remove(&m.cfg.state_dir, s.id).await;
    let _ = tokio::fs::remove_file(uploads_path(m, s.id)).await;
    m.clear_http_policy(s.sandbox_id);
    if let Ok(mut rd) = tokio::fs::read_dir(previews_dir(m)).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let Ok(b) = tokio::fs::read(e.path()).await else {
                continue;
            };
            if serde_json::from_slice::<Preview>(&b).is_ok_and(|p| p.session == s.id) {
                let _ = tokio::fs::remove_file(e.path()).await;
            }
        }
    }
}

/// How often the sweeper looks, and how long after start it first does.
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(60);
const SWEEP_FIRST: std::time::Duration = std::time::Duration::from_secs(30);

/// One pass: an expired session is marked `expired` (it is otherwise only noticed when someone
/// reads it) and every ended session's side files are removed. Returns how many sessions it
/// cleaned.
pub(crate) async fn sweep_sessions(m: &VmManager) -> usize {
    let mut cleaned = 0;
    let Ok(mut rd) = tokio::fs::read_dir(dir(m)).await else {
        return 0;
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        let Ok(mut s) = serde_json::from_slice::<Session>(&bytes) else {
            continue;
        };
        reconcile(m, &mut s, false).await;
        if !matches!(s.state.as_str(), "expired" | "deleted") {
            continue;
        }
        // Skip the work when nothing is left to remove.
        let leftovers = job_meta_path(m, s.id).exists()
            || usage_path(m, s.id).exists()
            || m.cfg
                .state_dir
                .join("agent-session-tokens")
                .join(format!("{}.json", s.id))
                .exists()
            || m.cfg
                .state_dir
                .join("agent-session-approvals")
                .join(format!("{}.json", s.id))
                .exists()
            || m.http_policy_status(s.sandbox_id).0
            || !fluxvm_core::grants::global()
                .list(&s.sandbox_id.to_string(), Utc::now())
                .is_empty();
        if leftovers {
            session_cleanup(m, &s).await;
            record(m, s.id, "session.cleanup", json!({"state": s.state}));
            cleaned += 1;
        }
    }
    cleaned
}

/// Starts the sweeper once per process. Like the history sampler it does nothing without a Tokio
/// runtime and holds only a weak reference to the manager.
pub(crate) fn start_sweeper(manager: &Arc<VmManager>) {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if STARTED.set(()).is_err() {
        return;
    }
    let weak = Arc::downgrade(manager);
    handle.spawn(async move {
        let mut tick =
            tokio::time::interval_at(tokio::time::Instant::now() + SWEEP_FIRST, SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(m) = weak.upgrade() else { return };
            let n = sweep_sessions(&m).await;
            if n > 0 {
                tracing::info!(sessions = n, "agent sessions: swept ended sessions");
            }
        }
    });
}

// ---------------------------------------------------------------------------------------
// Credential grants: the sandbox credential broker, through the session. The agent never holds
// the secret: the egress proxy injects it for the named hosts, from this sandbox's address only.
// A grant ends with the session and never outlives its expiry; a fork child starts with none.

/// `POST .../grants`: same body as `POST /v1/sandboxes/{id}/grants`. Admin only (a scoped
/// session token cannot grant itself credentials).
async fn grant_create(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(mut req): Json<fluxvm_core::grants::GrantRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    // A grant never outlives the session.
    if let Some(end) = s.expires_at {
        let now = Utc::now();
        let wanted = match (req.expires_at, req.ttl_seconds) {
            (Some(at), _) => at,
            (None, Some(ttl)) if ttl > 0 => now + chrono::Duration::seconds(ttl),
            _ => end,
        };
        let at = wanted.min(end);
        if at <= now {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!("agent session {id} has expired"),
            });
        }
        req.expires_at = Some(at);
        req.ttl_seconds = None;
    }
    m.ensure_running_for_request(s.sandbox_id).await?;
    let info = m.add_grant(s.sandbox_id, req).await?;
    // Names and hosts, never the value.
    record(
        &m,
        id,
        "grant.create",
        json!({"grant": info.id, "secret_ref": info.secret_ref, "hosts": info.hosts,
               "expires_at": info.expires_at, "bound": info.bound}),
    );
    Ok((StatusCode::CREATED, Json(json!(info))))
}

async fn grant_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    Ok(Json(json!({"grants": m.list_grants(s.sandbox_id).await?})))
}

async fn grant_revoke_all(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    let revoked = m.revoke_grant(s.sandbox_id, None).await?;
    record(&m, id, "grant.revoke", json!({"grant": "all"}));
    Ok(Json(json!({"revoked": revoked})))
}

async fn grant_revoke(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, grant)): Path<(Uuid, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    if !m.revoke_grant(s.sandbox_id, Some(&grant)).await? {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "grant not found".into(),
        });
    }
    record(&m, id, "grant.revoke", json!({"grant": grant}));
    Ok(Json(json!({"revoked": true})))
}

#[derive(Debug, Deserialize)]
struct MintTokenBody {
    /// Operations to grant: read, exec, jobs, files.read, files.write, git, checkpoint, preview, screen.
    ops: Vec<String>,
    #[serde(default)]
    ttl_seconds: Option<u64>,
    #[serde(default)]
    label: Option<String>,
}

/// `POST .../tokens`: mint an expiring credential that works on this session only. Admin only,
/// and a scoped token can never mint another (the role it carries is read-only).
async fn mint_token(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<MintTokenBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let bad = |message: String| ApiError {
        status: StatusCode::BAD_REQUEST,
        message,
    };
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if body.ops.is_empty() {
        return Err(bad("ops must name at least one operation".into()));
    }
    let mut ops = Vec::new();
    for o in &body.ops {
        let op = Op::parse(o).ok_or_else(|| {
            bad(format!(
                "unknown operation {o:?}; grantable: {}",
                Op::ALL.map(Op::as_str).join(", ")
            ))
        })?;
        if !ops.contains(&op) {
            ops.push(op);
        }
    }
    let ttl = body
        .ttl_seconds
        .unwrap_or(agent_session_auth::DEFAULT_TTL_SECS);
    if ttl == 0 || ttl > agent_session_auth::MAX_TTL_SECS {
        return Err(bad(format!(
            "ttl_seconds must be 1-{}",
            agent_session_auth::MAX_TTL_SECS
        )));
    }
    let label = match body.label {
        Some(l) if l.trim().is_empty() || l.len() > 100 || l.chars().any(char::is_control) => {
            return Err(bad("label is 1-100 printable characters".into()));
        }
        l => l,
    };
    // A token never outlives its session's sandbox.
    let mut expires_at = Utc::now() + chrono::Duration::seconds(ttl as i64);
    if let Some(e) = s.expires_at {
        expires_at = expires_at.min(e);
    }
    if expires_at <= Utc::now() {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!("agent session {id} has expired"),
        });
    }
    let minted = agent_session_auth::mint(
        &m.cfg.state_dir,
        id,
        s.tenant.clone(),
        ops,
        label,
        expires_at,
    )
    .await?;
    // The ledger gets what was granted and until when, never the credential.
    record(
        &m,
        id,
        "token.mint",
        json!({"token": minted.record.id, "ops": minted.record.ops, "expires_at": minted.record.expires_at}),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": minted.record.id,
            "token": minted.secret_token,
            "session": id,
            "ops": minted.record.ops,
            "label": minted.record.label,
            "expires_at": minted.record.expires_at,
            "note": "shown once; use as `Authorization: Bearer <token>` on /v1/agents/sessions/{id}/...",
        })),
    ))
}

async fn list_tokens(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    Ok(Json(json!({
        "items": agent_session_auth::list(&m.cfg.state_dir, id).await
    })))
}

async fn revoke_token(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, token)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    if !agent_session_auth::revoke(&m.cfg.state_dir, id, token).await? {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: "token not found".into(),
        });
    }
    record(&m, id, "token.revoke", json!({"token": token}));
    Ok(StatusCode::NO_CONTENT)
}

/// Refuses work in a session whose HTTP rules are not applied to its sandbox's traffic.
fn egress_bound(s: &Session) -> ApiResult<()> {
    if s.policy.http_unbound {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!(
                "session {}'s HTTP policy is not bound to its sandbox's address (the address is \
                 in use by another sandbox's rules); delete the session and create it again",
                s.id
            ),
        });
    }
    Ok(())
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

enum AskGate {
    /// The command matches no `ask` rule.
    Clear,
    /// An approval was consumed for this request.
    Approved,
    /// Nobody has approved it yet.
    Required(Box<approvals::Approval>),
}

/// The `428` answer for a request that needs an approval. It is not a 2xx, so the idempotency
/// layer never stores it and the same `Idempotency-Key` can be retried after the decision.
fn approval_required(a: &approvals::Approval) -> (StatusCode, Json<Value>) {
    (
        StatusCode::PRECONDITION_REQUIRED,
        Json(json!({
            "approval_required": true,
            "approval_id": a.id,
            "state": "pending",
            "kind": a.kind,
            "summary": a.summary,
            "matched": a.matched,
            "params": a.params,
            "expires_at": a.expires_at,
            // `error` is the key the CLI and SDKs print for a non-2xx answer.
            "error": format!(
                "this command needs an operator's approval (approval_id {}): approve it with POST .../approvals/{}/approve, then repeat the request with approval_id",
                a.id, a.id
            ),
        })),
    )
}

/// Applies the session's `process.ask` rules to one request. `work` is released while waiting
/// for a decision, so a held request never blocks a checkpoint or restore.
#[allow(clippy::too_many_arguments)]
async fn approval_gate(
    m: &Arc<VmManager>,
    s: &Session,
    kind: &str,
    command: &str,
    params: Value,
    presented: Option<Uuid>,
    wait_secs: Option<u64>,
    work: &mut Option<WorkGuard>,
) -> ApiResult<AskGate> {
    let Some(matched) = s.policy.process.as_ref().and_then(|r| r.ask_match(command)) else {
        return Ok(AskGate::Clear);
    };
    let policy_hash = approvals::hash_str(&serde_json::to_string(&s.policy).unwrap_or_default());
    let subject = approvals::Subject {
        session: s.id,
        kind,
        command,
        params,
        policy_hash: &policy_hash,
        revision: s.revision,
    };
    let state_dir = m.cfg.state_dir.clone();
    let now = Utc::now();
    let mut decision = approvals::mutate(&state_dir, s.id, |all| {
        approvals::sweep(all, now);
        approvals::decide_request(all, &subject, &matched, presented, now)
    })
    .await?;
    if let approvals::Decision::Pending(a) = &decision
        && a.requested_at == now
    {
        record(
            m,
            s.id,
            "approval.requested",
            json!({"approval": a.id, "kind": kind, "command": a.summary, "matched": matched}),
        );
    }
    let wait = wait_secs.unwrap_or(0).min(approvals::MAX_WAIT_SECS);
    if wait > 0
        && let approvals::Decision::Pending(a) = &decision
    {
        let id = a.id;
        // A held request must not pin the session against checkpoint and restore.
        *work = None;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(wait);
        let mut outcome: Option<approvals::State> = None;
        while std::time::Instant::now() < until {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let all = approvals::list(&state_dir, s.id).await;
            match all.iter().find(|a| a.id == id).map(|a| a.state) {
                Some(approvals::State::Pending) => continue,
                other => {
                    outcome = other;
                    break;
                }
            }
        }
        *work = Some(begin_work(s.id)?);
        // The session may have been restored or its policy changed while waiting.
        let cur = load(m, s.id, &s.tenant).await?;
        running(&cur)?;
        let now = Utc::now();
        if outcome.is_some() {
            decision = approvals::mutate(&state_dir, s.id, |all| {
                approvals::decide_request(all, &subject, &matched, Some(id), now)
            })
            .await?;
        }
        if cur.revision != s.revision {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: "the session changed while waiting for the approval; repeat the request"
                    .into(),
            });
        }
    }
    match decision {
        approvals::Decision::Run(id) => {
            record(
                m,
                s.id,
                "approval.consumed",
                json!({"approval": id, "kind": kind, "command": clip_for_ledger(command)}),
            );
            let _ = id;
            Ok(AskGate::Approved)
        }
        approvals::Decision::Pending(a) => Ok(AskGate::Required(a)),
        approvals::Decision::Refuse(status, message) => {
            record(
                m,
                s.id,
                "approval.refused",
                json!({"kind": kind, "command": clip_for_ledger(command), "reason": message}),
            );
            Err(ApiError { status, message })
        }
    }
}

fn clip_for_ledger(s: &str) -> String {
    s.chars().take(512).collect()
}

/// `GET .../approvals`: the session's approval requests, newest first.
async fn approval_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    let now = Utc::now();
    let mut all = approvals::list(&m.cfg.state_dir, id).await;
    all.sort_by_key(|a| std::cmp::Reverse(a.requested_at));
    let items: Vec<Value> = all
        .iter()
        .map(|a| {
            let mut v = json!(a);
            v["state"] = json!(a.effective_state(now));
            v
        })
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Debug, Deserialize, Default)]
struct DecideBody {
    #[serde(default)]
    note: Option<String>,
}

// One argument per piece of request context the handler is given; a struct would only rename them.
#[allow(clippy::too_many_arguments)]
async fn decide_approval(
    m: Arc<VmManager>,
    role: Role,
    actor: Option<Extension<AuditActor>>,
    token_tenant: Option<Extension<TokenTenant>>,
    id: Uuid,
    approval: Uuid,
    approve: bool,
    note: Option<String>,
) -> ApiResult<Json<Value>> {
    // Admin only, and never through a session token: those carry the read-only role. An agent
    // cannot approve its own request.
    require_admin(role)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    let by = actor
        .map(|Extension(a)| a.0)
        .unwrap_or_else(|| "admin".into());
    let now = Utc::now();
    let by2 = by.clone();
    let res = approvals::mutate(&m.cfg.state_dir, id, |all| {
        approvals::decide_record(all, approval, approve, &by2, note, now)
    })
    .await?;
    let a = res.map_err(|(status, message)| ApiError { status, message })?;
    record(
        &m,
        id,
        if approve {
            "approval.approved"
        } else {
            "approval.denied"
        },
        json!({"approval": a.id, "by": by, "command": a.summary, "kind": a.kind}),
    );
    let mut v = json!(a);
    v["state"] = json!(a.effective_state(now));
    Ok(Json(v))
}

async fn approval_approve(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    actor: Option<Extension<AuditActor>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, approval)): Path<(Uuid, Uuid)>,
    body: Option<Json<DecideBody>>,
) -> ApiResult<Json<Value>> {
    let note = body.and_then(|Json(b)| b.note);
    decide_approval(m, role, actor, token_tenant, id, approval, true, note).await
}

async fn approval_deny(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    actor: Option<Extension<AuditActor>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, approval)): Path<(Uuid, Uuid)>,
    body: Option<Json<DecideBody>>,
) -> ApiResult<Json<Value>> {
    let note = body.and_then(|Json(b)| b.note);
    decide_approval(m, role, actor, token_tenant, id, approval, false, note).await
}

#[derive(Debug, Deserialize)]
struct ExecBody {
    command: String,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// A command matching a `process.ask` rule needs an approval: repeat the request with the id
    /// from the `428` answer once an operator approved it.
    #[serde(default)]
    approval_id: Option<Uuid>,
    /// Hold the request open this long (at most 120) for the decision instead of answering `428`.
    #[serde(default)]
    approval_wait_seconds: Option<u64>,
}

async fn exec_session(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<ExecBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(role, &scope, id, Op::Exec)?;
    let mut work = Some(begin_work(id)?);
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    egress_bound(&s)?;
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
    match approval_gate(
        &m,
        &s,
        "exec",
        &body.command,
        json!({"timeout_seconds": body.timeout_seconds}),
        body.approval_id,
        body.approval_wait_seconds,
        &mut work,
    )
    .await?
    {
        AskGate::Clear | AskGate::Approved => {}
        AskGate::Required(a) => return Ok(approval_required(&a)),
    }
    m.ensure_running_for_request(s.sandbox_id).await?;
    budget_check_workspace(&m, &s).await?;
    let left = budget_start_exec(&m, &s).await?;
    rewind_capture(&m, &s, "exec", &body.command).await;
    // A command never gets more time than the budget has left.
    let timeout = match (body.timeout_seconds, left) {
        (Some(t), Some(l)) => Some(t.min(l)),
        (None, Some(l)) => Some(l.min(fluxvm_guest_protocol::DEFAULT_EXEC_TIMEOUT_SECS)),
        (t, None) => t,
    };
    let started = std::time::Instant::now();
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            body.command.clone(),
            timeout,
            s.policy.exec.clone(),
        )
        .await;
    budget_finish_exec(&m, id, started.elapsed().as_millis() as u64).await;
    let res = res?;
    let mut out = json!(res);
    let truncated = match s.budget.as_ref().and_then(|b| b.max_output_bytes) {
        Some(cap) => cap_output(&mut out, cap as usize),
        None => false,
    };
    record(
        &m,
        id,
        "exec",
        json!({
            "command": body.command,
            "exit_code": out["exit_code"],
            "duration_ms": started.elapsed().as_millis() as u64,
            "output_truncated": truncated,
        }),
    );
    Ok((StatusCode::OK, Json(out)))
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    body: Option<Json<CheckpointBody>>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Checkpoint)?;
    let _gate = begin_lifecycle(id, "checkpoint")?.mark(&m, "checkpoint");
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    check_if_match(&headers, &s)?;
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
    if memory
        && !s.capabilities.memory_checkpoint
        && let Some(why) = &s.capabilities.memory_checkpoint_reason
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!(
                "this session's devices cannot be saved with memory: {why}. Take a disk-only checkpoint with \
                 {{\"memory\": false}}"
            ),
        });
    }
    m.create_vm_snapshot_with(s.sandbox_id, &tag, memory)
        .await?;
    let cp = Checkpoint {
        tag: tag.clone(),
        memory,
        created_at: Utc::now(),
    };
    s.checkpoints.retain(|c| c.tag != tag);
    s.checkpoints.push(cp.clone());
    save(&m, &mut s).await?;
    record(&m, id, "checkpoint", json!({"tag": tag, "memory": memory}));
    Ok(Json(json!(cp)))
}

// ---------------------------------------------------------------------------------------
// Rewind: automatic checkpoints taken before each exec, so the last action can be undone.
//
// The points live in their own file (like `Usage`), not in `Session::checkpoints`: taking one on
// every exec must not bump the session `revision` and 412 a caller that holds an `If-Match`.

const REWIND_DEFAULT_POINTS: u32 = 10;
const REWIND_MAX_POINTS: u32 = 50;
/// Timeline screenshots are thumbnails: enough to recognise the screen, cheap to keep.
const REWIND_SHOT_WIDTH: u32 = 480;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RewindPoint {
    tag: String,
    memory: bool,
    created_at: DateTime<Utc>,
    /// What caused it (`exec`).
    trigger: String,
    /// The command about to run when the point was taken: undoing to this point reverses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// A pinned point is never evicted from the ring and survives an undo.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pinned: bool,
    /// The guest display was captured with the point: `GET .../rewind/{tag}/screenshot`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    screenshot: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RewindState {
    enabled: bool,
    max_points: u32,
    /// Ask for memory in each point (needs `capabilities.memory_checkpoint`; else disk-only).
    memory: bool,
    /// What takes a point: any of `exec`, `job`, `file`, `git`. Only `exec` by default.
    #[serde(default = "rewind_default_triggers")]
    triggers: Vec<String>,
    /// Oldest first.
    points: Vec<RewindPoint>,
}

const REWIND_TRIGGERS: [&str; 4] = ["exec", "job", "file", "git"];

fn rewind_default_triggers() -> Vec<String> {
    vec!["exec".into()]
}

/// The triggers in canonical order without repeats, or why the list is not acceptable.
fn rewind_triggers(raw: &[String]) -> Result<Vec<String>, String> {
    if let Some(bad) = raw.iter().find(|t| !REWIND_TRIGGERS.contains(&t.as_str())) {
        return Err(format!(
            "unknown trigger {bad:?}; use any of {}",
            REWIND_TRIGGERS.join(", ")
        ));
    }
    let out: Vec<String> = REWIND_TRIGGERS
        .iter()
        .filter(|t| raw.iter().any(|r| r == **t))
        .map(|t| t.to_string())
        .collect();
    if out.is_empty() {
        return Err("triggers must name at least one of exec, job, file, git".into());
    }
    Ok(out)
}

impl Default for RewindState {
    fn default() -> Self {
        RewindState {
            enabled: false,
            max_points: REWIND_DEFAULT_POINTS,
            memory: true,
            triggers: rewind_default_triggers(),
            points: Vec::new(),
        }
    }
}

/// Drops the oldest unpinned points until the ring fits `max_points`, and returns their tags.
/// Pinned points stay (a ring made only of pins may exceed the limit; `rewind_pin` prevents that).
fn rewind_evict(st: &mut RewindState) -> Vec<String> {
    let keep = st.max_points.max(1) as usize;
    let mut out = Vec::new();
    while st.points.len() > keep {
        match st.points.iter().position(|p| !p.pinned) {
            Some(i) => out.push(st.points.remove(i).tag),
            None => break,
        }
    }
    out
}

/// Adds a point and returns the tags that fell off the ring.
fn rewind_push(st: &mut RewindState, p: RewindPoint) -> Vec<String> {
    st.points.push(p);
    rewind_evict(st)
}

/// Pins or unpins `tag`. A pin that would leave no room for new points is refused.
fn rewind_pin(st: &mut RewindState, tag: &str, pinned: bool) -> Result<(), String> {
    let pins = st
        .points
        .iter()
        .filter(|p| p.pinned && p.tag != tag)
        .count();
    if pinned && pins + 1 >= st.max_points as usize {
        return Err(format!(
            "{} point(s) are already pinned; the ring holds {} and needs room for new ones",
            pins, st.max_points
        ));
    }
    match st.points.iter_mut().find(|p| p.tag == tag) {
        Some(p) => {
            p.pinned = pinned;
            Ok(())
        }
        None => Err(format!("no rewind point {tag:?}")),
    }
}

/// Index of the point `steps` actions back (`1` is the point taken before the last exec).
fn rewind_target(st: &RewindState, steps: u32) -> Result<usize, String> {
    if steps == 0 {
        return Err("steps must be at least 1".into());
    }
    let n = st.points.len();
    if n == 0 {
        return Err("no rewind points: enable rewind, then run a command".into());
    }
    if steps as usize > n {
        return Err(format!(
            "only {n} rewind point(s) are kept; asked to go back {steps}"
        ));
    }
    Ok(n - steps as usize)
}

/// After restoring to `idx`, drops that point and every newer one (they describe a timeline that
/// no longer exists) and returns their tags. Pinned points are kept: the user asked to keep them.
fn rewind_truncate(st: &mut RewindState, idx: usize) -> Vec<String> {
    let mut dropped = Vec::new();
    let mut i = idx;
    while i < st.points.len() {
        if st.points[i].pinned {
            i += 1;
        } else {
            dropped.push(st.points.remove(i).tag);
        }
    }
    dropped
}

fn rewind_path(m: &VmManager, id: Uuid) -> PathBuf {
    m.cfg
        .state_dir
        .join("agent-session-rewind")
        .join(format!("{id}.json"))
}

fn rewind_shot_path(m: &VmManager, id: Uuid, tag: &str) -> PathBuf {
    m.cfg
        .state_dir
        .join("agent-session-rewind")
        .join(format!("{id}.shots"))
        .join(format!("{tag}.png"))
}

/// Deletes the snapshots and screenshots of points that left the ring.
async fn rewind_forget(m: &VmManager, s: &Session, tags: &[String]) {
    for t in tags {
        let _ = m.delete_vm_snapshot(s.sandbox_id, t).await;
        let _ = tokio::fs::remove_file(rewind_shot_path(m, s.id, t)).await;
    }
}

static REWIND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn rewind_read(m: &VmManager, id: Uuid) -> RewindState {
    match tokio::fs::read(rewind_path(m, id)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => RewindState::default(),
    }
}

async fn rewind_write(m: &VmManager, id: Uuid, st: &RewindState) -> ApiResult<()> {
    let path = rewind_path(m, id);
    if let Some(d) = path.parent() {
        tokio::fs::create_dir_all(d)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(
        &tmp,
        serde_json::to_vec_pretty(st).map_err(anyhow::Error::from)?,
    )
    .await
    .map_err(anyhow::Error::from)?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(())
}

/// Saves a small screenshot for point `tag`; `true` when there is one.
async fn rewind_take_shot(m: &VmManager, s: &Session, tag: &str) -> bool {
    let shot = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        m.vm_screenshot(s.sandbox_id, Some(REWIND_SHOT_WIDTH)),
    )
    .await
    {
        Ok(Ok(shot)) if !shot.blank => shot,
        _ => return false,
    };
    let path = rewind_shot_path(m, s.id, tag);
    if let Some(d) = path.parent()
        && tokio::fs::create_dir_all(d).await.is_err()
    {
        return false;
    }
    tokio::fs::write(&path, &shot.png).await.is_ok()
}

/// Takes a point before `command` runs, when rewind is on. Best effort: a point that cannot be
/// taken (another lifecycle operation holds the slot, the snapshot fails) is written to the
/// ledger and the command still runs, so rewind never makes exec less available.
async fn rewind_capture(m: &Arc<VmManager>, s: &Session, kind: &str, what: &str) {
    let st = rewind_read(m, s.id).await;
    if !st.enabled || !s.capabilities.snapshot || !st.triggers.iter().any(|t| t == kind) {
        return;
    }
    let guard = match begin_lifecycle(s.id, "checkpoint") {
        Ok(g) => g.mark(m, "checkpoint"),
        Err(e) => {
            record(m, s.id, "rewind.skipped", json!({"reason": e.message}));
            return;
        }
    };
    let memory = st.memory && s.capabilities.memory_checkpoint;
    let tag = format!("rw-{}", Utc::now().timestamp_millis());
    let started = std::time::Instant::now();
    if let Err(e) = m.create_vm_snapshot_with(s.sandbox_id, &tag, memory).await {
        record(
            m,
            s.id,
            "rewind.capture_failed",
            json!({"reason": format!("{e:#}")}),
        );
        drop(guard);
        return;
    }
    // What the display showed at this point, for the timeline. Best effort and short: a guest with
    // no display, or a slow one, just has no picture.
    let screenshot = rewind_take_shot(m, s, &tag).await;
    drop(guard);
    let point = RewindPoint {
        tag: tag.clone(),
        memory,
        created_at: Utc::now(),
        trigger: kind.into(),
        command: Some(what.chars().take(512).collect()),
        pinned: false,
        screenshot,
    };
    let evicted = {
        let _g = REWIND_LOCK.lock().await;
        let mut cur = rewind_read(m, s.id).await;
        let evicted = rewind_push(&mut cur, point);
        if let Err(e) = rewind_write(m, s.id, &cur).await {
            record(
                m,
                s.id,
                "rewind.capture_failed",
                json!({"reason": e.message, "tag": tag}),
            );
            rewind_forget(m, s, std::slice::from_ref(&tag)).await;
            return;
        }
        evicted
    };
    rewind_forget(m, s, &evicted).await;
    record(
        m,
        s.id,
        "rewind.point",
        json!({"tag": tag, "memory": memory, "ms": started.elapsed().as_millis() as u64,
               "evicted": evicted.len()}),
    );
}

async fn rewind_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    let mut v = json!(rewind_read(&m, id).await);
    v["memory_available"] = json!(s.capabilities.memory_checkpoint);
    Ok(Json(v))
}

/// `GET /v1/agents/sessions/{id}/rewind/{tag}/screenshot` — the display as it was at that point.
async fn rewind_screenshot(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, tag)): Path<(Uuid, String)>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    authorize(role, &scope, id, Op::Screen)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    // The tag is only ever used as a file name once it is known to be one of the ring's points.
    let st = rewind_read(&m, id).await;
    if !st.points.iter().any(|p| p.tag == tag && p.screenshot) {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("session {id} has no screenshot for rewind point {tag:?}"),
        });
    }
    let png = tokio::fs::read(rewind_shot_path(&m, id, &tag))
        .await
        .map_err(|_| ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("the screenshot of rewind point {tag:?} is gone"),
        })?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "image/png".to_string()),
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
        ],
        png,
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct RewindConfigBody {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    max_points: Option<u32>,
    #[serde(default)]
    memory: Option<bool>,
    /// Any of `exec`, `job`, `file`, `git`.
    #[serde(default)]
    triggers: Option<Vec<String>>,
}

async fn rewind_config(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<RewindConfigBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if let Some(n) = body.max_points
        && !(1..=REWIND_MAX_POINTS).contains(&n)
    {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: format!("max_points must be 1-{REWIND_MAX_POINTS}"),
        });
    }
    let triggers = match &body.triggers {
        Some(t) => Some(rewind_triggers(t).map_err(|message| ApiError {
            status: StatusCode::BAD_REQUEST,
            message,
        })?),
        None => None,
    };
    if body.enabled == Some(true) && !s.capabilities.snapshot {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!("backend {} cannot snapshot, so it cannot rewind", s.backend),
        });
    }
    let (st, evicted) = {
        let _g = REWIND_LOCK.lock().await;
        let mut st = rewind_read(&m, id).await;
        if let Some(e) = body.enabled {
            st.enabled = e;
        }
        if let Some(n) = body.max_points {
            st.max_points = n;
        }
        if let Some(mem) = body.memory {
            st.memory = mem;
        }
        if let Some(t) = triggers {
            st.triggers = t;
        }
        // A smaller ring drops the oldest unpinned points now.
        let evicted = rewind_evict(&mut st);
        rewind_write(&m, id, &st).await?;
        (st, evicted)
    };
    rewind_forget(&m, &s, &evicted).await;
    record(
        &m,
        id,
        "rewind.config",
        json!({"enabled": st.enabled, "max_points": st.max_points, "memory": st.memory,
               "triggers": st.triggers}),
    );
    Ok(Json(json!(st)))
}

/// The next state to test while narrowing `(lo, hi)`, where `lo` passed and `hi` failed; `None`
/// when they are adjacent, so `hi` is the first bad state.
fn bisect_next(lo: usize, hi: usize) -> Option<usize> {
    (hi > lo + 1).then(|| lo + (hi - lo) / 2)
}

const BISECT_TEST_SECS: u64 = 60;

#[derive(Debug, Deserialize)]
struct BisectBody {
    /// Shell command run in the guest after each restore; exit 0 means the state is good.
    test: String,
    /// A point known to pass (default: the oldest point).
    #[serde(default)]
    good: Option<String>,
    /// A point known to fail (default: the machine as it is now).
    #[serde(default)]
    bad: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

/// Finds the first command whose effect made `test` fail. States are the rewind points (the
/// machine just before each exec) plus, last, the machine as it is now; the command that ran
/// between two adjacent states is the first point's `command`. Leaves the machine as it was.
async fn rewind_bisect(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<BisectBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let _gate = begin_lifecycle(id, "restore")?.mark(&m, "restore");
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    check_if_match(&headers, &s)?;
    running(&s)?;
    egress_bound(&s)?;
    if body.test.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "test is required".into(),
        });
    }
    enforce_rules(&s, &body.test)?;
    let st = rewind_read(&m, id).await;
    let n = st.points.len();
    if n == 0 {
        return Err(gate_conflict(
            "no rewind points: enable rewind, then run commands".into(),
        ));
    }
    // State `n` is the machine as it is now.
    let find = |tag: &Option<String>, default: usize| -> ApiResult<usize> {
        match tag {
            None => Ok(default),
            Some(t) => st
                .points
                .iter()
                .position(|p| &p.tag == t)
                .ok_or_else(|| ApiError {
                    status: StatusCode::NOT_FOUND,
                    message: format!("session {id} has no rewind point {t:?}"),
                }),
        }
    };
    let mut lo = find(&body.good, 0)?;
    let mut hi = find(&body.bad, n)?;
    if lo >= hi {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "good must be an older state than bad".into(),
        });
    }
    // The machine now, so it can be put back whatever happens below.
    let head_tag = format!("rw-bisect-{}", Utc::now().timestamp_millis());
    let memory = st.memory && s.capabilities.memory_checkpoint;
    m.create_vm_snapshot_with(s.sandbox_id, &head_tag, memory)
        .await?;
    let timeout = body
        .timeout_seconds
        .unwrap_or(BISECT_TEST_SECS)
        .clamp(1, 600);
    let mut steps: Vec<Value> = Vec::new();
    let started = std::time::Instant::now();

    let outcome: ApiResult<()> = async {
        // Restores state `i` and returns (passed, exit code, tag).
        let test_state = |i: usize| {
            let m = m.clone();
            let s = s.clone();
            let tag = if i == n {
                head_tag.clone()
            } else {
                st.points[i].tag.clone()
            };
            let cmd = body.test.clone();
            async move {
                restore_session_to(&m, &s, &tag).await?;
                let res = m
                    .exec_with_policy(s.sandbox_id, cmd, Some(timeout), s.policy.exec.clone())
                    .await?;
                let code = json!(res)["exit_code"].as_i64().unwrap_or(-1);
                Ok::<(bool, i64, String), ApiError>((code == 0, code, tag))
            }
        };
        // The two ends must disagree, or there is nothing to find.
        let (lo_ok, lo_code, lo_tag) = test_state(lo).await?;
        steps.push(json!({"state": lo, "tag": lo_tag, "passed": lo_ok, "exit_code": lo_code}));
        if !lo_ok {
            return Err(gate_conflict(format!(
                "the test already fails on the good point (exit {lo_code}); pick an older `good`"
            )));
        }
        let (hi_ok, hi_code, hi_tag) = test_state(hi).await?;
        steps.push(json!({"state": hi, "tag": hi_tag, "passed": hi_ok, "exit_code": hi_code}));
        if hi_ok {
            return Err(gate_conflict(
                "the test passes on the bad state too; nothing to bisect".into(),
            ));
        }
        while let Some(mid) = bisect_next(lo, hi) {
            let (ok, code, tag) = test_state(mid).await?;
            steps.push(json!({"state": mid, "tag": tag, "passed": ok, "exit_code": code}));
            if ok {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(())
    }
    .await;

    // Put the machine back, success or not, and drop the temporary snapshot.
    let restored = restore_session_to(&m, &s, &head_tag).await;
    let _ = m.delete_vm_snapshot(s.sandbox_id, &head_tag).await;
    outcome?;
    restored?;

    let culprit = &st.points[lo];
    record(
        &m,
        id,
        "rewind.bisect",
        json!({"command": culprit.command, "last_good": culprit.tag, "steps": steps.len(),
               "ms": started.elapsed().as_millis() as u64}),
    );
    Ok(Json(json!({
        "culprit": {"command": culprit.command, "ran_after": culprit.tag, "at": culprit.created_at},
        "first_bad_state": hi,
        "tested": steps,
        "ms": started.elapsed().as_millis() as u64,
        "machine_restored": true,
    })))
}

#[derive(Debug, Deserialize)]
struct RewindBranchBody {
    /// The point to branch from, as `GET .../rewind` lists them.
    tag: String,
    /// Child sessions to start from it (1-32, default 1).
    #[serde(default)]
    count: Option<u32>,
    /// `best-effort` (default), `unique` or `preserve`, as for fork.
    #[serde(default)]
    identity: Option<String>,
}

/// `POST /v1/agents/sessions/{id}/rewind/branch` — starts child sessions from a *past* point, leaving the session itself
/// exactly as it is. Where `undo` goes back, `branch` explores: "what if I had not run that command?" runs in a child
/// while the parent carries on. Same forkability rules (and children counted against the budget) as `fork`.
async fn rewind_branch(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    actor: Option<Extension<AuditActor>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RewindBranchBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let _gate = begin_lifecycle(id, "fork")?.mark(&m, "fork");
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    check_if_match(&headers, &s)?;
    running(&s)?;
    if let Some(why) = fork_refusal(&s) {
        return Err(gate_conflict(why));
    }
    let count = body.count.unwrap_or(1);
    if count == 0 || count > 32 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "count must be 1-32".into(),
        });
    }
    let identity = body.identity.unwrap_or_else(|| "best-effort".into());
    if !["best-effort", "unique", "preserve"].contains(&identity.as_str()) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "identity must be best-effort, unique or preserve".into(),
        });
    }
    let st = rewind_read(&m, id).await;
    let point = st
        .points
        .iter()
        .find(|p| p.tag == body.tag)
        .cloned()
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("session {id} has no rewind point {:?}", body.tag),
        })?;
    let spec: fluxvm_scheduler::fork::ForkSpec = serde_json::from_value(json!({
        "count": count,
        "name_prefix": format!("agent-{}-branch", &id.to_string()[..8]),
        "identity": identity,
    }))
    .map_err(anyhow::Error::from)?;
    let actor_name = actor.as_ref().map(|a| a.0.0.as_str());
    let started = std::time::Instant::now();
    let usage_now = usage_read(&m, id).await;
    let child_budget = s.budget.as_ref().map(|b| b.remaining(&usage_now));
    usage_update(&m, id, |u| {
        if let Some(b) = &s.budget {
            b.check_fork(u, count as u64)?;
        }
        u.children += count as u64;
        Ok(())
    })
    .await?;
    let refund = |m: Arc<VmManager>| async move {
        let _ = usage_update(&m, id, |u| {
            u.children = u.children.saturating_sub(count as u64);
            Ok(())
        })
        .await;
    };
    // The point's snapshot becomes a short-lived branch, which the children are started from; the
    // session's own machine is not touched at any point.
    let branch_name = format!("rw-{}", &Uuid::new_v4().simple().to_string()[..12]);
    let branch = match m
        .create_branch_from_snapshot(
            s.sandbox_id,
            &point.tag,
            branch_name,
            Default::default(),
            actor_name,
        )
        .await
    {
        Ok(b) => b,
        Err(e) => {
            refund(m.clone()).await;
            return Err(e.into());
        }
    };
    let started_children = m.start_branch(branch.id, spec, actor_name).await;
    // Started VMs are independent clones: the branch has done its job either way.
    if let Err(e) = m.delete_branch(branch.id, actor_name).await {
        record(
            &m,
            id,
            "rewind.branch_cleanup_failed",
            json!({"branch": branch.id, "reason": format!("{e:#}")}),
        );
    }
    let report = match started_children {
        Ok(r) => r,
        Err(e) => {
            refund(m.clone()).await;
            return Err(e.into());
        }
    };

    let mut kids = Vec::new();
    for child in &report.children {
        let cid = Uuid::new_v4();
        let mut cs = Session {
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
            // The parent's files *now* are not this child's starting point, so there is no baseline
            // to diff or promote against.
            workspace: s.workspace.clone().map(|mut w| {
                w.baseline = false;
                w
            }),
            volumes: Vec::new(),
            checkpoints: Vec::new(),
            budget: child_budget.clone(),
            revision: 0,
        };
        if !cs.policy.http_rules.is_empty()
            && let Err(e) = m.set_http_policy(child.id, &cs.policy.http_rules).await
        {
            record(
                &m,
                cid,
                "http_policy.unbound",
                json!({"reason": format!("{e:#}")}),
            );
            cs.policy.http_unbound = true;
        }
        save(&m, &mut cs).await?;
        record(
            &m,
            cid,
            "session.branch_child",
            json!({"parent": id, "from": point.tag, "undone": point.command}),
        );
        s.children.push(cid);
        kids.push(cs);
    }
    save(&m, &mut s).await?;
    record(
        &m,
        id,
        "rewind.branch",
        json!({"tag": point.tag, "count": count, "children": kids.iter().map(|k| k.id).collect::<Vec<_>>(),
               "ms": started.elapsed().as_millis() as u64}),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "items": kids,
            "from": point,
            "identity": identity,
            "total_ms": started.elapsed().as_millis() as u64,
        })),
    ))
}

#[derive(Debug, Deserialize)]
struct RewindPinBody {
    tag: String,
    /// `false` unpins.
    #[serde(default = "pin_default")]
    pinned: bool,
}

fn pin_default() -> bool {
    true
}

async fn rewind_pin_point(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<RewindPinBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    let (st, evicted) = {
        let _g = REWIND_LOCK.lock().await;
        let mut st = rewind_read(&m, id).await;
        rewind_pin(&mut st, &body.tag, body.pinned).map_err(|message| ApiError {
            status: StatusCode::CONFLICT,
            message,
        })?;
        // Unpinning can let an over-full ring shrink back.
        let evicted = rewind_evict(&mut st);
        rewind_write(&m, id, &st).await?;
        (st, evicted)
    };
    if !evicted.is_empty() {
        let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
        rewind_forget(&m, &s, &evicted).await;
    }
    record(
        &m,
        id,
        "rewind.pin",
        json!({"tag": body.tag, "pinned": body.pinned}),
    );
    Ok(Json(json!(st)))
}

#[derive(Debug, Deserialize, Default)]
struct RewindUndoBody {
    /// How many actions to go back; `1` (default) reverses the last exec.
    #[serde(default)]
    steps: Option<u32>,
    /// Go back to this point instead (by tag, as `GET .../rewind` lists them).
    #[serde(default)]
    to: Option<String>,
}

async fn rewind_undo(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    body: Option<Json<RewindUndoBody>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let _gate = begin_lifecycle(id, "restore")?.mark(&m, "restore");
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    check_if_match(&headers, &s)?;
    running(&s)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let st = rewind_read(&m, id).await;
    let (idx, steps) = match &body.to {
        Some(tag) => {
            let i = st
                .points
                .iter()
                .position(|p| &p.tag == tag)
                .ok_or_else(|| ApiError {
                    status: StatusCode::NOT_FOUND,
                    message: format!("session {id} has no rewind point {tag:?}"),
                })?;
            (i, (st.points.len() - i) as u32)
        }
        None => {
            let steps = body.steps.unwrap_or(1);
            let idx = rewind_target(&st, steps).map_err(|message| ApiError {
                status: StatusCode::CONFLICT,
                message,
            })?;
            (idx, steps)
        }
    };
    let point = st.points[idx].clone();
    let mut out = restore_session_to(&m, &s, &point.tag).await?;
    let dropped = {
        let _g = REWIND_LOCK.lock().await;
        let mut cur = rewind_read(&m, id).await;
        // Points are only ever added under the gate we hold, so the index still names `point`.
        let dropped = match cur.points.iter().position(|p| p.tag == point.tag) {
            Some(i) => rewind_truncate(&mut cur, i),
            None => Vec::new(),
        };
        rewind_write(&m, id, &cur).await?;
        dropped
    };
    rewind_forget(&m, &s, &dropped).await;
    record(
        &m,
        id,
        "rewind.undo",
        json!({"tag": point.tag, "steps": steps, "undid": point.command, "dropped": dropped.len()}),
    );
    out["rewound_to"] = json!(point);
    out["steps"] = json!(steps);
    Ok(Json(out))
}

/// Puts the session's machine back to the snapshot `tag` and re-binds what hangs off its address
/// (credential grants, HTTP rules). The caller holds the lifecycle slot and has checked that the
/// session is running.
async fn restore_session_to(m: &Arc<VmManager>, s: &Session, tag: &str) -> ApiResult<Value> {
    let id = s.id;
    let vm = m
        .restore_vm_snapshot(s.sandbox_id, tag)
        .await
        .map_err(change_error)?;
    record(m, id, "restore", json!({"tag": tag}));
    // The restored machine may have another guest address: re-bind its credential grants to it
    // (or drop them if that address is taken), so a grant never follows someone else's traffic.
    let grants_rebound = match m.rebind_grants(s.sandbox_id).await {
        Ok(has) => has,
        Err(e) => {
            record(m, id, "grant.revoked", json!({"reason": format!("{e:#}")}));
            false
        }
    };
    // The same for the session's HTTP rules: if the new address belongs to another sandbox's
    // rules, this sandbox's traffic is no longer attributed to them, so the session stops
    // taking exec and jobs rather than running with a policy that is not applied.
    let mut http_bound = None;
    if !s.policy.http_rules.is_empty() {
        let unbound = match m.rebind_http_policy(s.sandbox_id).await {
            Ok(_) => false,
            Err(e) => {
                record(
                    m,
                    id,
                    "http_policy.unbound",
                    json!({"reason": format!("{e:#}")}),
                );
                true
            }
        };
        if unbound != s.policy.http_unbound {
            let mut cur = s.clone();
            cur.policy.http_unbound = unbound;
            save(m, &mut cur).await?;
        }
        http_bound = Some(!unbound);
    }
    Ok(json!({
        "restored": tag, "vm": vm, "grants_rebound": grants_rebound,
        "http_policy_bound": http_bound,
    }))
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
    headers: axum::http::HeaderMap,
    Json(body): Json<RestoreBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let _gate = begin_lifecycle(id, "restore")?.mark(&m, "restore");
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    check_if_match(&headers, &s)?;
    running(&s)?;
    if !s.checkpoints.iter().any(|c| c.tag == body.tag) {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("session {id} has no checkpoint {:?}", body.tag),
        });
    }
    restore_session_to(&m, &s, &body.tag).await.map(Json)
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
    headers: axum::http::HeaderMap,
    body: Option<Json<ForkBody>>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(role)?;
    let _gate = begin_lifecycle(id, "fork")?.mark(&m, "fork");
    let clones_before = fluxvm_scheduler::clone_counters();
    let mut s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    check_if_match(&headers, &s)?;
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
    // The parent's files at the fork instant are every child's starting point, so they become
    // each child's baseline: a child's `artifacts` and a later `promote` then see exactly what it
    // did since the fork. The lifecycle gate means nothing is running on the parent meanwhile.
    // Children are counted against the parent's budget and start with what it has left.
    let usage_now = usage_read(&m, id).await;
    let child_budget = s.budget.as_ref().map(|b| b.remaining(&usage_now));
    usage_update(&m, id, |u| {
        if let Some(b) = &s.budget {
            b.check_fork(u, count as u64)?;
        }
        u.children += count as u64;
        Ok(())
    })
    .await?;
    let seed = match &s.workspace {
        Some(w) if w.baseline => m
            .sandbox_manifest(s.sandbox_id, vec![w.path.clone()])
            .await
            .ok()
            .map(|mf| (w.path.clone(), mf)),
        _ => None,
    };
    let report = match m.fork_vm_spec(s.sandbox_id, spec, actor_name).await {
        Ok(r) => r,
        Err(e) => {
            // Nothing was created, so nothing is spent.
            let _ = usage_update(&m, id, |u| {
                u.children = u.children.saturating_sub(count as u64);
                Ok(())
            })
            .await;
            return Err(e.into());
        }
    };

    let mut kids = Vec::new();
    for child in &report.children {
        let cid = Uuid::new_v4();
        let mut cs = Session {
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
            budget: child_budget.clone(),
            revision: 0,
        };
        if !cs.policy.http_rules.is_empty() {
            // A child is a new sandbox with its own address; the rules are installed for it, or
            // it is marked unbound and refuses exec and jobs.
            if let Err(e) = m.set_http_policy(child.id, &cs.policy.http_rules).await {
                record(
                    &m,
                    cid,
                    "http_policy.unbound",
                    json!({"reason": format!("{e:#}")}),
                );
                cs.policy.http_unbound = true;
            }
        }
        let seeded = match &seed {
            Some((path, mf)) => m
                .sandbox_baseline_seed(child.id, vec![path.clone()], mf.clone())
                .await
                .is_ok(),
            None => false,
        };
        if let Some(w) = cs.workspace.as_mut() {
            w.baseline = seeded;
        }
        save(&m, &mut cs).await?;
        record(&m, cid, "session.fork_child", json!({"parent": id}));
        s.children.push(cid);
        kids.push(cs);
    }
    save(&m, &mut s).await?;
    let clones_after = fluxvm_scheduler::clone_counters();
    let files_cloned = clones_after.0.saturating_sub(clones_before.0);
    let files_copied = clones_after.1.saturating_sub(clones_before.1);
    record(
        &m,
        id,
        "fork",
        json!({"count": count, "children": s.children, "total_ms": report.total_ms,
               "files_cloned": files_cloned, "files_copied": files_copied}),
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "items": kids,
            "identity": identity,
            // Copied files were not copy-on-write: the fork was slower and used real disk for them.
            "files_cloned": files_cloned,
            "files_copied": files_copied,
            "total_ms": report.total_ms,
            "parent_snapshot_ms": report.parent_snapshot_ms,
        })),
    ))
}

// ---------------------------------------------------------------------------------------
// Promote: apply what a fork child did to its workspace onto its parent.

const MAX_PROMOTE_CHANGES: usize = 2000;

#[derive(Debug, Deserialize, Default)]
struct PromoteBody {
    /// Compute and return the plan; change nothing.
    #[serde(default)]
    dry_run: bool,
    /// The `plan_id` of a plan you reviewed: the apply is refused if the plan is different now.
    #[serde(default)]
    plan_id: Option<String>,
    /// Apply the paths that merge cleanly and leave conflicting ones alone (default: any
    /// conflict refuses the whole promotion).
    #[serde(default)]
    skip_conflicts: bool,
    /// Only these files or directories (absolute, inside the workspace).
    #[serde(default)]
    paths: Option<Vec<String>>,
}

/// A digest of a plan, so an apply can say "the plan I reviewed".
fn plan_digest(plan: &fluxvm_scheduler::changes::PromotionPlan) -> String {
    use sha2::{Digest, Sha256};
    let mut text = String::new();
    for c in &plan.apply {
        text.push_str(&format!("apply\t{}\t{}\n", c.kind, c.path));
    }
    for p in &plan.already_applied {
        text.push_str(&format!("already\t{p}\n"));
    }
    for c in &plan.conflicts {
        text.push_str(&format!("conflict\t{}\t{}\n", c.reason, c.path));
    }
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `POST .../{child}/promote`: three-way merge of the child's workspace changes onto its parent.
/// Admin only. The parent gets a disk checkpoint first; a path the parent also changed is a
/// conflict, never overwritten.
async fn promote(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    body: Option<Json<PromoteBody>>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let conflict = |message: String| ApiError {
        status: StatusCode::CONFLICT,
        message,
    };
    let tenant = caller_tenant(&token_tenant);
    let _child_gate = begin_lifecycle(id, "promote")?;
    let child = load(&m, id, &tenant).await?;
    running(&child)?;
    let parent_id = child.parent.ok_or_else(|| {
        conflict("this session is not a fork child: there is no parent to promote into".into())
    })?;
    let _parent_gate = begin_lifecycle(parent_id, "promote")?;
    let mut parent = load(&m, parent_id, &tenant).await?;
    running(&parent)?;
    let ws = match (&child.workspace, &parent.workspace) {
        (Some(c), Some(p)) if c.baseline && c.path == p.path => c.path.clone(),
        _ => {
            return Err(conflict(
                "promotion needs a workspace with a baseline on both sides; the child's baseline is \
                 recorded at fork time, so a child forked before this was available cannot be promoted"
                    .into(),
            ));
        }
    };
    let filter: Option<Vec<String>> = match body.paths {
        Some(ps) => {
            let mut out = Vec::new();
            for p in ps {
                let t = p.trim_end_matches('/').to_string();
                if !valid_file_path(&t) || !(t == ws || t.starts_with(&format!("{ws}/"))) {
                    return Err(ApiError {
                        status: StatusCode::BAD_REQUEST,
                        message: format!("paths entries must be absolute and inside {ws}"),
                    });
                }
                out.push(t);
            }
            Some(out)
        }
        None => None,
    };
    m.ensure_running_for_request(child.sandbox_id).await?;
    m.ensure_running_for_request(parent.sandbox_id).await?;
    let mut plan = m
        .sandbox_promotion_plan(child.sandbox_id, parent.sandbox_id, vec![ws.clone()])
        .await
        .map_err(change_error)?;
    // Paths come from the guest's own file listing: only ones inside the workspace and plainly
    // nameable are ever transferred. The rest are reported, not moved.
    let safe = |p: &str| valid_file_path(p) && p.starts_with(&format!("{ws}/"));
    let (ok, unsafe_): (Vec<_>, Vec<_>) = plan.apply.drain(..).partition(|c| safe(&c.path));
    plan.apply = ok;
    for c in unsafe_ {
        plan.conflicts
            .push(fluxvm_scheduler::changes::PromotionConflict {
                path: c.path,
                reason: "path is not safe to transfer",
            });
    }
    if let Some(f) = &filter {
        let inside = |p: &str| f.iter().any(|r| p == r || p.starts_with(&format!("{r}/")));
        plan.apply.retain(|c| inside(&c.path));
        plan.conflicts.retain(|c| inside(&c.path));
        plan.already_applied.retain(|p| inside(p));
    }
    if plan.apply.len() > MAX_PROMOTE_CHANGES {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: format!(
                "{} changes to apply; narrow it with `paths` (at most {MAX_PROMOTE_CHANGES} at once)",
                plan.apply.len()
            ),
        });
    }
    let plan_id = plan_digest(&plan);
    if body.dry_run {
        return Ok(Json(json!({
            "session": id, "into": parent_id, "dry_run": true, "applied": false,
            "plan_id": plan_id, "plan": plan,
        })));
    }
    if let Some(want) = &body.plan_id
        && want != &plan_id
    {
        return Err(conflict(
            "the plan is different from the one you reviewed (a side changed); dry-run again"
                .into(),
        ));
    }
    if !plan.conflicts.is_empty() && !body.skip_conflicts {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!(
                "{} conflicting path(s), nothing was changed: {}; resolve them or pass skip_conflicts",
                plan.conflicts.len(),
                plan.conflicts
                    .iter()
                    .take(5)
                    .map(|c| format!("{} ({})", c.path, c.reason))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    if plan.apply.is_empty() {
        return Ok(Json(json!({
            "session": id, "into": parent_id, "applied": false, "plan_id": plan_id,
            "plan": plan, "note": "nothing to apply",
        })));
    }
    if !parent.capabilities.snapshot {
        return Err(conflict(format!(
            "backend {} cannot checkpoint the parent, and promotion never writes without one",
            parent.backend
        )));
    }
    // The way back: a disk checkpoint of the parent immediately before the first write.
    let tag = format!(
        "pre-promote-{}-{}",
        &id.to_string()[..8],
        Utc::now().format("%H%M%S")
    );
    m.create_vm_snapshot_with(parent.sandbox_id, &tag, false)
        .await?;
    parent.checkpoints.push(Checkpoint {
        tag: tag.clone(),
        memory: false,
        created_at: Utc::now(),
    });
    save(&m, &mut parent).await?;

    let mut applied: Vec<Value> = Vec::new();
    let fail = |applied: &[Value], path: &str, why: String| ApiError {
        status: StatusCode::BAD_GATEWAY,
        message: format!(
            "promotion stopped at {path}: {why}. {} path(s) were already applied; restore the parent's \
             checkpoint {tag:?} to undo them",
            applied.len()
        ),
    };
    for c in &plan.apply {
        let step: Result<(), String> = async {
            if c.kind == "deleted" {
                let out = m
                    .exec_with_policy(
                        parent.sandbox_id,
                        format!("rm -f -- {}", shell_quote(&c.path)),
                        Some(30),
                        None,
                    )
                    .await
                    .map_err(|e| format!("{e:#}"))?;
                return match serde_json::to_value(&out)
                    .ok()
                    .and_then(|v| v["exit_code"].as_i64())
                {
                    Some(0) => Ok(()),
                    code => Err(format!("rm exited {code:?}")),
                };
            }
            let (content, mode) = match m
                .get_file(child.sandbox_id, c.path.clone())
                .await
                .map_err(|e| format!("reading from the child: {e:#}"))?
            {
                fluxvm_guest_protocol::AgentResponse::FileContent {
                    content_base64,
                    mode,
                } => (content_base64, mode),
                other => return Err(format!("unexpected guest response: {other:?}")),
            };
            if let Some(dir) = std::path::Path::new(&c.path)
                .parent()
                .and_then(|d| d.to_str())
            {
                m.exec_with_policy(
                    parent.sandbox_id,
                    format!("mkdir -p -- {}", shell_quote(dir)),
                    Some(30),
                    None,
                )
                .await
                .map_err(|e| format!("creating {dir}: {e:#}"))?;
            }
            m.put_file(parent.sandbox_id, c.path.clone(), content, Some(mode))
                .await
                .map_err(|e| format!("writing to the parent: {e:#}"))?;
            Ok(())
        }
        .await;
        match step {
            Ok(()) => applied.push(json!({"path": c.path, "kind": c.kind})),
            Err(why) => {
                record(
                    &m,
                    parent_id,
                    "promote.failed",
                    json!({"child": id, "path": c.path, "reason": why, "applied": applied.len(), "checkpoint": tag}),
                );
                return Err(fail(&applied, &c.path, why));
            }
        }
    }
    record(
        &m,
        parent_id,
        "promote",
        json!({"child": id, "applied": applied.len(), "already_applied": plan.already_applied.len(),
               "skipped_conflicts": plan.conflicts.len(), "checkpoint": tag, "plan_id": plan_id}),
    );
    record(
        &m,
        id,
        "promote.out",
        json!({"into": parent_id, "applied": applied.len(), "plan_id": plan_id}),
    );
    Ok(Json(json!({
        "session": id, "into": parent_id, "applied": true, "plan_id": plan_id,
        "checkpoint": tag, "changes": applied,
        "already_applied": plan.already_applied, "skipped_conflicts": plan.conflicts,
    })))
}

#[derive(Debug, Deserialize, Default)]
struct EventsQuery {
    #[serde(default)]
    since: Option<DateTime<Utc>>,
}

async fn events(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<PreviewBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(role, &scope, id, Op::Preview)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, token)): Path<(Uuid, String)>,
) -> ApiResult<StatusCode> {
    authorize(role, &scope, id, Op::Preview)?;
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

/// Chrome's DevTools discovery endpoints (`/json`, `/json/list`, `/json/version`, `/json/new`).
fn is_cdp_discovery_path(path: &str) -> bool {
    let p = path.trim_matches('/');
    p == "json" || p.starts_with("json/") && !p.starts_with("json/protocol")
}

/// Rewrites `webSocketDebuggerUrl` / `devtoolsFrontendUrl` in a DevTools discovery document
/// (an object, or an array of them) so they point at `host` under `/preview/<token>`.
fn rewrite_cdp_urls(v: &mut Value, host: &str, token: &str, secure: bool) {
    match v {
        Value::Array(a) => a
            .iter_mut()
            .for_each(|x| rewrite_cdp_urls(x, host, token, secure)),
        Value::Object(o) => {
            let prefix = format!("/preview/{token}");
            if let Some(Value::String(s)) = o.get_mut("webSocketDebuggerUrl") {
                // ws://10.0.0.5:9223/devtools/page/ID -> ws(s)://<host>/preview/<token>/devtools/page/ID
                if let Some(path) = s
                    .strip_prefix("ws://")
                    .or_else(|| s.strip_prefix("wss://"))
                    .and_then(|r| r.find('/').map(|i| r[i..].to_string()))
                {
                    let scheme = if secure { "wss" } else { "ws" };
                    *s = format!("{scheme}://{host}{prefix}{path}");
                }
            }
            if let Some(Value::String(s)) = o.get_mut("devtoolsFrontendUrl") {
                // /devtools/inspector.html?ws=10.0.0.5:9223/devtools/page/ID
                //   -> /preview/<token>/devtools/inspector.html?ws=<host>/preview/<token>/devtools/page/ID
                let (path, query) = match s.split_once("?ws=") {
                    Some((p, q)) => (p.to_string(), Some(q.to_string())),
                    None => (s.clone(), None),
                };
                if path.starts_with('/') {
                    let mut out = format!("{prefix}{path}");
                    if let Some(q) = query {
                        let tail = q.find('/').map(|i| q[i..].to_string()).unwrap_or_default();
                        out.push_str(&format!("?ws={host}{prefix}{tail}"));
                    }
                    *s = out;
                }
            }
            // Nested documents (none today) are walked too.
            o.values_mut()
                .filter(|x| x.is_array() || x.is_object())
                .for_each(|x| rewrite_cdp_urls(x, host, token, secure));
        }
        _ => {}
    }
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
    // How the client reached this daemon, for rewriting DevTools URLs back to the preview.
    let public_host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let secure = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"));
    let cdp_discovery = is_cdp_discovery_path(&path);
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
    // Chrome DevTools discovery documents name the guest's own address and no preview prefix, so
    // a client that follows `webSocketDebuggerUrl` (Playwright `connectOverCDP`) would go
    // straight past the preview. Point those URLs back at it.
    if cdp_discovery
        && !is_ws
        && resp.status() == StatusCode::OK
        && let Some(host) = public_host
        && resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("json"))
    {
        let (mut parts, body) = resp.into_parts();
        match axum::body::to_bytes(body, 8 * 1024 * 1024).await {
            Ok(bytes) => {
                let out = serde_json::from_slice::<Value>(&bytes)
                    .ok()
                    .map(|mut v| {
                        rewrite_cdp_urls(&mut v, &host, &token, secure);
                        serde_json::to_vec(&v).unwrap_or_else(|_| bytes.to_vec())
                    })
                    .unwrap_or_else(|| bytes.to_vec());
                parts.headers.remove(axum::http::header::CONTENT_LENGTH);
                resp = axum::response::Response::from_parts(parts, axum::body::Body::from(out));
            }
            Err(_) => {
                return (StatusCode::BAD_GATEWAY, "DevTools response too large").into_response();
            }
        }
    }
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
// Probe: what the guest image can do, so a caller finds out before it builds on it.

const PROBE_TOOLS: &[&str] = &[
    "git",
    "sha256sum",
    "python3",
    "pip3",
    "node",
    "npm",
    "npx",
    "cargo",
    "go",
    "gcc",
    "cc",
    "make",
    "curl",
    "chromium",
    "chromium-browser",
    "google-chrome",
    "playwright",
    "socat",
];

fn probe_script() -> String {
    let names = PROBE_TOOLS.join(" ");
    format!(
        "for t in {names}; do p=$(command -v \"$t\" 2>/dev/null) && echo \"$t=$p\"; done; \
         d=\"${{PLAYWRIGHT_BROWSERS_PATH:-$HOME/.cache/ms-playwright}}\"; \
         [ -d \"$d\" ] && echo \"playwright-browsers=$d\"; \
         echo \"arch=$(uname -m)\"; echo \"kernel=$(uname -r)\"; true"
    )
}

/// Turns the probe's `name=value` lines into the report. Pure.
fn probe_report(stdout: &str) -> Value {
    let mut found = serde_json::Map::new();
    let mut info = serde_json::Map::new();
    for line in stdout.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k == "arch" || k == "kernel" {
            info.insert(k.into(), json!(v));
        } else if PROBE_TOOLS.contains(&k) || k == "playwright-browsers" {
            found.insert(k.into(), json!(v));
        }
    }
    let has = |t: &str| found.contains_key(t);
    let browser = has("chromium") || has("chromium-browser") || has("google-chrome");
    json!({
        "tools": found,
        "system": info,
        // What the session features need from the image.
        "ready_for": {
            "git": has("git"),
            "promote": has("sha256sum"),
            "build_node": has("node") && has("npm"),
            "build_python": has("python3"),
            "build_rust": has("cargo"),
            "build_c": has("make") && (has("gcc") || has("cc")),
            "browser_cdp": browser,
            "browser_cdp_through_preview": browser && has("socat"),
            "playwright": has("playwright") || has("playwright-browsers"),
        },
    })
}

/// `GET .../probe`: one short command in the guest (under the session's confinement, if any).
async fn probe(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Read)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            probe_script(),
            Some(30),
            s.policy.exec.clone(),
        )
        .await?;
    let out = json!(res);
    let mut report = probe_report(out["stdout"].as_str().unwrap_or(""));
    report["session"] = json!(id);
    Ok(Json(report))
}

#[derive(Debug, Deserialize, Default)]
struct ScreenshotQuery {
    #[serde(default)]
    max_width: Option<u32>,
}

/// `GET /v1/agents/sessions/{id}/screenshot[?max_width=N]` — the guest display as a PNG (vz sessions). The screen can
/// show anything the guest does, so it is its own scope (`screen`), not part of `read`. Ledgered without the pixels.
async fn screenshot(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<ScreenshotQuery>,
) -> ApiResult<axum::response::Response> {
    use axum::response::IntoResponse;
    authorize(role, &scope, id, Op::Screen)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let shot = m.vm_screenshot(s.sandbox_id, q.max_width).await?;
    record(
        &m,
        id,
        "screen.shot",
        json!({"width": shot.width, "height": shot.height, "blank": shot.blank, "bytes": shot.png.len()}),
    );
    let h = |k: &'static str, v: String| (axum::http::HeaderName::from_static(k), v);
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "image/png".to_string()),
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
            h("x-fluxvm-screen-width", shot.width.to_string()),
            h("x-fluxvm-screen-height", shot.height.to_string()),
            h("x-fluxvm-screen-blank", shot.blank.to_string()),
        ],
        shot.png,
    )
        .into_response())
}

#[derive(Debug, Deserialize, Default)]
struct ScreenWaitBody {
    /// Give up after this long (default 10, at most 60).
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// How often to look (default 500 ms, 100-5000).
    #[serde(default)]
    interval_ms: Option<u64>,
    /// Compare at this width (default 320): small enough to ignore cursor-blink noise at the pixel level, no more.
    #[serde(default)]
    max_width: Option<u32>,
}

/// `POST /v1/agents/sessions/{id}/screen/wait` — returns when the display differs from how it looked at the call, or
/// at the timeout: `{"changed", "waited_ms", "width", "height"}`. Take a screenshot afterwards to see it. Needs the
/// `screen` scope; ledgered as `screen.wait`.
async fn screen_wait(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    body: Option<Json<ScreenWaitBody>>,
) -> ApiResult<Json<Value>> {
    use sha2::{Digest, Sha256};
    authorize(role, &scope, id, Op::Screen)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let timeout = std::time::Duration::from_secs(b.timeout_seconds.unwrap_or(10).clamp(1, 60));
    let interval = std::time::Duration::from_millis(b.interval_ms.unwrap_or(500).clamp(100, 5000));
    let width = b.max_width.or(Some(320));
    let started = std::time::Instant::now();
    let first = m.vm_screenshot(s.sandbox_id, width).await?;
    let base = Sha256::digest(&first.png);
    let (mut changed, mut dims) = (false, (first.width, first.height));
    while started.elapsed() < timeout {
        tokio::time::sleep(interval).await;
        let now = m.vm_screenshot(s.sandbox_id, width).await?;
        dims = (now.width, now.height);
        if Sha256::digest(&now.png) != base {
            changed = true;
            break;
        }
    }
    let waited_ms = started.elapsed().as_millis() as u64;
    record(
        &m,
        id,
        "screen.wait",
        json!({"changed": changed, "waited_ms": waited_ms}),
    );
    Ok(Json(
        json!({"changed": changed, "waited_ms": waited_ms, "width": dims.0, "height": dims.1}),
    ))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScreenInput {
    Batch {
        actions: Vec<fluxvm_scheduler::vz_screen::InputAction>,
    },
    One(fluxvm_scheduler::vz_screen::InputAction),
}

/// `POST /v1/agents/sessions/{id}/input` — one action or `{"actions": [...]}` to the guest display, in order.
/// Ledgered as `screen.input` with the action count and the action kinds (typed text is not copied into the ledger).
async fn screen_input(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(req): Json<ScreenInput>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Screen)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let actions = match req {
        ScreenInput::Batch { actions } => actions,
        ScreenInput::One(a) => vec![a],
    };
    if actions.is_empty() || actions.len() > 256 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "send 1-256 actions".into(),
        });
    }
    let kinds: Vec<String> = actions
        .iter()
        .map(|a| {
            serde_json::to_value(a)
                .ok()
                .and_then(|v| v.get("action").and_then(|k| k.as_str().map(str::to_string)))
                .unwrap_or_else(|| "?".into())
        })
        .collect();
    let n = m.vm_input(s.sandbox_id, &actions).await?;
    record(
        &m,
        id,
        "screen.input",
        json!({"actions": n, "kinds": kinds}),
    );
    Ok(Json(json!({"ok": true, "actions": n})))
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
    /// The client's own `TERM` (kept only when it is a plain terminfo name the guest knows).
    #[serde(default)]
    term: Option<String>,
    /// `truecolor` or `24bit` when the client's terminal does 24-bit colour.
    #[serde(default)]
    colorterm: Option<String>,
    /// Do not add the colour aliases and prompt-friendly defaults to the login shell.
    #[serde(default)]
    plain: bool,
    /// The client terminal's `TERM_PROGRAM` / `TERM_PROGRAM_VERSION` (WezTerm, iTerm.app,
    /// Apple_Terminal, ghostty...), so guest tools can tell which graphics and link protocols it has.
    #[serde(default)]
    term_program: Option<String>,
    #[serde(default)]
    term_program_version: Option<String>,
}

/// How the terminal should look: what the client's terminal can do, and whether to dress the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PtyLook {
    term: String,
    truecolor: bool,
    plain: bool,
    program: Option<(String, Option<String>)>,
}

impl PtyLook {
    fn from_query(q: &PtyQuery) -> Self {
        let ok = |t: &str| {
            !t.is_empty()
                && t.len() <= 40
                && t.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        };
        // program names and versions: the same characters (e.g. `iTerm.app`, `20240203-110809-5046fc22`)
        let word = ok;
        Self {
            term: q
                .term
                .as_deref()
                .filter(|t| ok(t))
                .unwrap_or("xterm-256color")
                .to_string(),
            truecolor: matches!(q.colorterm.as_deref(), Some("truecolor" | "24bit")),
            plain: q.plain,
            program: q.term_program.as_deref().filter(|p| word(p)).map(|p| {
                (
                    p.to_string(),
                    q.term_program_version
                        .as_deref()
                        .filter(|v| word(v))
                        .map(str::to_string),
                )
            }),
        }
    }
}

/// An rc file for the login shell: the usual profile, then colour for the tools that have it
/// off by default on Debian, and coloured man pages. Written to the guest by the start script.
const PTY_RC: &str = r#"[ -r /etc/profile ] && . /etc/profile
for f in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do [ -r "$f" ] && { . "$f"; break; }; done
alias grep='grep --color=auto'; alias egrep='egrep --color=auto'; alias fgrep='fgrep --color=auto'
alias diff='diff --color=auto'; alias ip='ip -color=auto'; alias ll='ls -lAh'; alias la='ls -A'
export LESS="-R $LESS"
export LESS_TERMCAP_md=$'\e[1;36m' LESS_TERMCAP_us=$'\e[4;32m' LESS_TERMCAP_so=$'\e[1;30;43m'
export LESS_TERMCAP_me=$'\e[0m' LESS_TERMCAP_ue=$'\e[0m' LESS_TERMCAP_se=$'\e[0m'
export GCC_COLORS='error=01;31:warning=01;35:note=01;36:caret=01;32:locus=01:quote=01'
if [ "$TERM_PROGRAM" = FluxVM ] && [ -n "$BASH_VERSION" ]; then
  # OSC 133 prompt marks for the web terminal: A = prompt start, C = command start, D;<exit> = finished.
  __fx_precmd() { printf '\033]133;D;%s\007' "$?"; }
  PROMPT_COMMAND="__fx_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
  PS1='\[\033]133;A\007\]'"$PS1"
  PS0='\033]133;C\007'"$PS0"
fi
"#;

/// Connection details for plain `ssh` (WezTerm's ssh domains, scp, rsync, port forwards). The key is
/// the host's shared sandbox key, so this is for a client on the daemon's own machine and is refused
/// to tenant tokens, which must not receive a key that opens every sandbox.
async fn ssh_target(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    if caller_tenant(&token_tenant).is_some() {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message:
                "the ssh key is shared by every sandbox on this host; a tenant token cannot have it"
                    .into(),
        });
    }
    let s = load(&m, id, &None).await?;
    running(&s)?;
    check_pty_allowed(&s)?;
    let vm = m.ensure_running_for_request(s.sandbox_id).await?;
    let g = m.ssh_target(&vm).map_err(|e| ApiError {
        status: StatusCode::CONFLICT,
        message: format!("{e:#}"),
    })?;
    Ok(Json(json!({
        "user": g.user,
        "host": if g.vsock.is_some() { Value::Null } else { json!(g.ip) },
        "identity_file": g.key,
        "vsock_socket": g.vsock,
        "note": "valid on the machine running the daemon; the vsock proxy is `fluxctl vsock-proxy <socket> 22`",
    })))
}

/// The remote shell line that sizes the terminal and starts the shell or command. Pure.
fn pty_script(
    cols: u16,
    rows: u16,
    cwd: Option<&str>,
    command: Option<&str>,
    tty_file: &str,
    rc_file: &str,
    look: &PtyLook,
) -> String {
    let cd = cwd
        .map(|c| format!("cd {} 2>/dev/null; ", shell_quote(c)))
        .unwrap_or_default();
    // The client's TERM only if the guest has its terminfo; otherwise the widely known one.
    let term = format!(
        "export TERM={t}; infocmp \"$TERM\" >/dev/null 2>&1 || export TERM=xterm-256color;",
        t = shell_quote(&look.term)
    );
    let program = look
        .program
        .as_ref()
        .map(|(p, v)| {
            let mut out = format!("export TERM_PROGRAM={};", shell_quote(p));
            if let Some(v) = v {
                out.push_str(&format!(" export TERM_PROGRAM_VERSION={};", shell_quote(v)));
            }
            // iTerm2's own marker, which image tools look for first
            if p == "iTerm.app" {
                out.push_str(" export LC_TERMINAL=iTerm2;");
            }
            out
        })
        .unwrap_or_default();
    let color = format!(
        "{ct} export CLICOLOR=1; {program}",
        ct = if look.truecolor {
            "export COLORTERM=truecolor;"
        } else {
            ""
        }
    );
    let run = match command {
        Some(c) => format!("exec sh -c {}", shell_quote(c)),
        // bash gets the colour rc file; any other shell is started as a plain login shell.
        None if !look.plain => format!(
            "case \"${{SHELL##*/}}\" in bash) printf %s {rc} > {f}; exec bash --rcfile {f} -i;; esac; exec \"${{SHELL:-/bin/sh}}\" -l",
            rc = shell_quote(PTY_RC),
            f = shell_quote(rc_file)
        ),
        None => "exec \"${SHELL:-/bin/sh}\" -l".to_string(),
    };
    // The tty's path is kept in a file so a later resize can address it from another connection.
    format!(
        "stty cols {cols} rows {rows} 2>/dev/null; tty > {tf} 2>/dev/null; {term} {color} {cd}{run}",
        tf = shell_quote(tty_file)
    )
}

/// The guest-side colour rc file of a terminal.
fn pty_rc_file(token: Uuid) -> String {
    format!("/tmp/.fluxvm-rc-{token}")
}

/// The guest-side file holding a terminal's tty path. `token` is a UUID we generated.
fn pty_tty_file(token: Uuid) -> String {
    format!("/tmp/.fluxvm-pty-{token}")
}

/// Resize a running terminal by setting the window size on its tty; the kernel then signals the
/// foreground process group (SIGWINCH). Pure so it is unit-tested.
fn pty_resize_script(tty_file: &str, cols: u16, rows: u16) -> String {
    let f = shell_quote(tty_file);
    format!("t=$(cat {f}) && stty -F \"$t\" cols {cols} rows {rows}")
}

/// `{"resize": {"cols": N, "rows": N}}` as a text frame, within the limits `open` enforces.
fn parse_resize(text: &str) -> Option<(u16, u16)> {
    let v: Value = serde_json::from_str(text).ok()?;
    let r = v.get("resize")?;
    let cols = u16::try_from(r.get("cols")?.as_u64()?).ok()?;
    let rows = u16::try_from(r.get("rows")?.as_u64()?).ok()?;
    ((1..=500).contains(&cols) && (1..=300).contains(&rows)).then_some((cols, rows))
}

/// Single-use terminal tickets for browsers, which cannot put `Authorization` on a WebSocket. A
/// ticket is 256 random bits, lives 30 seconds, is consumed by the first use, and exists only in
/// memory: it is never written to disk or to the ledger.
const PTY_TICKET_SECS: u64 = 30;
const MAX_PTY_TICKETS: usize = 256;

fn pty_tickets()
-> &'static std::sync::Mutex<std::collections::HashMap<String, (Uuid, std::time::Instant)>> {
    static T: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, (Uuid, std::time::Instant)>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(Default::default)
}

/// Mints a ticket for `session`; `None` when too many are outstanding (a flood, not a person).
fn mint_pty_ticket(session: Uuid) -> Option<String> {
    let now = std::time::Instant::now();
    let mut t = pty_tickets().lock().unwrap_or_else(|e| e.into_inner());
    t.retain(|_, (_, exp)| *exp > now);
    if t.len() >= MAX_PTY_TICKETS {
        return None;
    }
    let ticket = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    t.insert(
        ticket.clone(),
        (
            session,
            now + std::time::Duration::from_secs(PTY_TICKET_SECS),
        ),
    );
    Some(ticket)
}

/// Consumes a ticket: the session it was minted for, once, if it has not expired.
fn take_pty_ticket(ticket: &str) -> Option<Uuid> {
    let mut t = pty_tickets().lock().unwrap_or_else(|e| e.into_inner());
    let (session, exp) = t.remove(ticket)?;
    (exp > std::time::Instant::now()).then_some(session)
}

async fn pty_ticket(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    egress_bound(&s)?;
    check_pty_allowed(&s)?;
    let ticket = mint_pty_ticket(id).ok_or(ApiError {
        status: StatusCode::TOO_MANY_REQUESTS,
        message: "too many terminal tickets outstanding".into(),
    })?;
    Ok(Json(
        json!({"ticket": ticket, "expires_in": PTY_TICKET_SECS}),
    ))
}

/// A terminal would walk around command rules and the guest confinement (it is an SSH shell,
/// not an exec), so a session that has either refuses one.
fn check_pty_allowed(s: &Session) -> Result<(), ApiError> {
    if s.policy.process.is_some() {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "this session has process rules, and a terminal would bypass them".into(),
        });
    }
    if s.policy.exec.is_some() {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "this session has a filesystem policy, and a terminal would bypass it".into(),
        });
    }
    Ok(())
}

async fn pty(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PtyQuery>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<axum::response::Response, ApiError> {
    require_admin(role)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    open_pty(m, s, q, ws).await
}

/// `GET /console/pty/{ticket}`: the browser's terminal. The ticket stands in for the bearer token
/// and was minted by an authenticated, tenant-checked request; the session is loaded again here
/// so a session deleted or changed since is refused.
async fn pty_by_ticket(
    State(m): State<Arc<VmManager>>,
    Path(ticket): Path<String>,
    Query(q): Query<PtyQuery>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<axum::response::Response, ApiError> {
    let id = take_pty_ticket(&ticket).ok_or(ApiError {
        status: StatusCode::NOT_FOUND,
        message: "terminal ticket not found".into(),
    })?;
    let s = load(&m, id, &None).await?;
    open_pty(m, s, q, ws).await
}

async fn open_pty(
    m: Arc<VmManager>,
    s: Session,
    q: PtyQuery,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<axum::response::Response, ApiError> {
    use axum::response::IntoResponse as _;
    let id = s.id;
    running(&s)?;
    check_pty_allowed(&s)?;
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
    let token = Uuid::new_v4();
    let tty_file = pty_tty_file(token);
    let look = PtyLook::from_query(&q);
    let rc_file = pty_rc_file(token);
    let script = pty_script(
        cols,
        rows,
        cwd.as_deref(),
        q.command.as_deref(),
        &tty_file,
        &rc_file,
        &look,
    );
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
        .on_upgrade(move |socket| relay_pty(m, id, s.sandbox_id, tty_file, rc_file, socket, child))
        .into_response())
}

async fn relay_pty(
    m: Arc<VmManager>,
    id: Uuid,
    sandbox: Uuid,
    tty_file: String,
    rc_file: String,
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

    let (resize_m, resize_file) = (m.clone(), tty_file.clone());
    let up = async move {
        while let Some(Ok(msg)) = rx.next().await {
            let data: Vec<u8> = match msg {
                Message::Binary(b) => b.to_vec(),
                // A text frame is a control message when it parses as one; anything else is input.
                Message::Text(t) => {
                    if let Some((cols, rows)) = parse_resize(t.as_str()) {
                        let (m, f) = (resize_m.clone(), resize_file.clone());
                        // Off the relay: it is an exec, and keystrokes must not wait for it.
                        tokio::spawn(async move {
                            let _ = m
                                .exec_with_policy(
                                    sandbox,
                                    pty_resize_script(&f, cols, rows),
                                    Some(10),
                                    None,
                                )
                                .await;
                        });
                        continue;
                    }
                    t.as_bytes().to_vec()
                }
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
    // Best effort: a file left behind in /tmp is harmless and is gone on reboot.
    let rm = format!("rm -f {} {}", shell_quote(&tty_file), shell_quote(&rc_file));
    let _ = m.exec_with_policy(sandbox, rm, Some(10), None).await;
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
// Jobs: a command that outlives the request. It runs detached in the guest (`setsid` where it exists; macOS has none), its
// output goes to a file (bounded: the log stops at `max_output_bytes` and the job gets SIGPIPE),
// and the caller reads that file from a byte offset, either by polling `GET .../jobs/{job}` or
// as a Server-Sent Events stream (`.../stream`, `Last-Event-ID` is the byte offset, so a dropped
// connection resumes). With `stdin: true` the job reads from a FIFO that `POST .../stdin`
// appends to. Everything is built on the ordinary exec path, so each read costs one guest exec;
// there is no TTY and no end-of-file on stdin. Metadata (command, start, exit) is kept on the
// host so a listing survives a daemon restart.

const JOBS_DIR: &str = "/tmp/fluxvm-jobs";
const JOB_READ_MAX: usize = 65_536;
const JOB_DEFAULT_MAX_OUTPUT: u64 = 16 * 1024 * 1024;
const JOB_MAX_OUTPUT: u64 = 256 * 1024 * 1024;
/// One stdin write: it travels as a shell argument, so it is kept small.
const JOB_STDIN_MAX: usize = 48 * 1024;
const JOB_STREAM_POLL: std::time::Duration = std::time::Duration::from_millis(400);
const JOB_STREAM_MAX_SECS: u64 = 3600;
const MAX_JOB_STREAMS: usize = 16;

#[derive(Debug, Deserialize)]
struct JobBody {
    command: String,
    /// Give the job a stdin that `POST .../stdin` writes to (default: /dev/null).
    #[serde(default)]
    stdin: bool,
    /// Stop recording output after this many bytes (default 16 MiB, at most 256 MiB).
    #[serde(default)]
    max_output_bytes: Option<u64>,
    /// Kill the job's process tree after this many seconds. Needs the guest job supervisor.
    #[serde(default)]
    timeout_seconds: Option<u64>,
    /// See `ExecBody::approval_id`.
    #[serde(default)]
    approval_id: Option<Uuid>,
    #[serde(default)]
    approval_wait_seconds: Option<u64>,
}

/// `JobMeta.engine` of a job run by the guest agent's supervisor (shell-script jobs leave it empty).
const ENGINE_SUPERVISOR: &str = "supervisor";
/// How long one supervisor call may take beyond its own long poll.
const SUP_CALL_SECS: u64 = 30;

fn phase_name(p: JobPhase) -> &'static str {
    match p {
        JobPhase::Running => "running",
        JobPhase::Exited => "exited",
        JobPhase::Signaled => "signaled",
        JobPhase::TimedOut => "timed_out",
        JobPhase::Canceled => "killed",
    }
}

/// A supervisor call that turns the agent's `Error` answers into API errors.
async fn sup_call(
    m: &VmManager,
    sandbox: Uuid,
    req: AgentRequest,
    wait_secs: u64,
) -> ApiResult<AgentResponse> {
    let res = m
        .guest_agent_call(
            sandbox,
            req,
            std::time::Duration::from_secs(SUP_CALL_SECS + wait_secs),
        )
        .await?;
    match res {
        AgentResponse::Error { message } => Err(ApiError {
            status: if message.starts_with("no such job") {
                StatusCode::NOT_FOUND
            } else if message.contains("stdin is closed") || message.contains("already ended") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_GATEWAY
            },
            message,
        }),
        other => Ok(other),
    }
}

/// Offsets from a `Last-Event-ID`: `N` (stdout) or `N:M` (stdout, stderr).
fn parse_event_offsets(v: &str) -> (u64, u64) {
    let v = v.trim();
    match v.split_once(':') {
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
        None => (v.parse().unwrap_or(0), 0),
    }
}

/// A signal given by name (`TERM`, `SIGKILL`) or number.
fn signal_number(s: &str) -> Option<i32> {
    let t = s.trim().to_ascii_uppercase();
    let t = t.strip_prefix("SIG").unwrap_or(&t);
    match t {
        "HUP" => Some(1),
        "INT" => Some(2),
        "QUIT" => Some(3),
        "KILL" => Some(9),
        "USR1" => Some(10),
        "USR2" => Some(12),
        "TERM" => Some(15),
        n => n.parse::<i32>().ok().filter(|n| (1..=64).contains(n)),
    }
}

/// Records how a supervised job ended (first observation wins).
async fn job_meta_status(m: &VmManager, id: Uuid, st: &JobStatus) {
    if !st.phase.is_final() {
        return;
    }
    let job = st.job_id.parse::<Uuid>().ok();
    job_meta_update(m, id, |all| {
        if let Some(j) = all.iter_mut().find(|j| Some(j.job) == job)
            && j.exit_code.is_none()
        {
            j.exit_code = Some(
                st.exit_code
                    .map(i64::from)
                    .or(st.signal.map(|s| 128 + i64::from(s)))
                    .unwrap_or(-1),
            );
            j.phase = Some(phase_name(st.phase).to_string());
            j.signal = st.signal;
            j.dropped_bytes = st.stdout_dropped + st.stderr_dropped;
            j.finished_at = Some(Utc::now());
        }
    })
    .await;
}

fn job_stdin_path_in(dir: &str, job: Uuid) -> String {
    format!("{dir}/{job}.in")
}

/// The guest-side shell line that starts a job. Pure, so it is unit-tested.
fn job_start_script(job: Uuid, command: &str, stdin: bool, max_output: u64) -> String {
    job_start_script_in(JOBS_DIR, job, command, stdin, max_output)
}

/// [`job_start_script`] with the jobs directory chosen by the caller (tests use a private one).
fn job_start_script_in(
    dir: &str,
    job: Uuid,
    command: &str,
    stdin: bool,
    max_output: u64,
) -> String {
    let f = format!("{dir}/{job}");
    let out = shell_quote(&format!("{f}.out"));
    let exit = shell_quote(&format!("{f}.exit"));
    let pid = shell_quote(&format!("{f}.pid"));
    let fifo = job_stdin_path_in(dir, job);
    let mkfifo = if stdin {
        format!("mkfifo {} && ", shell_quote(&fifo))
    } else {
        String::new()
    };
    let input = if stdin {
        shell_quote(&fifo)
    } else {
        "/dev/null".to_string()
    };
    // The inner shell records its pid (== process group, because of setsid), holds the stdin
    // FIFO open read-write so writers never block and the job never sees EOF, pipes the command's
    // output through `head -c` (the bound), and records the exit code last, so "exit file exists"
    // means the output file is complete.
    format!(
        "mkdir -p {dir} && {mkfifo}{{ S=; command -v setsid >/dev/null 2>&1 && S=setsid; (exec $S sh -c 'echo $$ > \"$1\"; exec 3<>\"$6\"; {{ sh -c \"$2\" 2>&1 <&3; echo $? > \"$4.rc\"; }} | head -c \"$5\" > \"$3\"; mv \"$4.rc\" \"$4\"' _ {pid} {cmd} {out} {exit} {max_output} {input} > /dev/null 2>&1 &); }} ; echo started",
        dir = shell_quote(dir),
        cmd = shell_quote(command),
    )
}

/// The guest-side line that reads a job: exit code first, then output from `offset`, then the
/// log's size, so an exit seen here means the output read after it is complete.
fn job_read_script(job: Uuid, offset: u64) -> String {
    job_read_script_in(JOBS_DIR, job, offset)
}

fn job_read_script_in(dir: &str, job: Uuid, offset: u64) -> String {
    let f = format!("{dir}/{job}");
    format!(
        "e=$(cat {exit} 2>/dev/null); if [ ! -e {out} ]; then echo MISSING; exit 0; fi; \
         echo \"EXIT:$e\"; tail -c +{from} {out} | head -c {max} | base64 | tr -d '\\n'; echo; \
         echo \"SIZE:$(wc -c < {out})\"",
        exit = shell_quote(&format!("{f}.exit")),
        out = shell_quote(&format!("{f}.out")),
        from = offset + 1,
        max = JOB_READ_MAX,
    )
}

/// The guest-side line that appends to a job's stdin. Refuses (exit 3) when the job has no stdin
/// FIFO or has already exited, because opening a FIFO nobody reads would block.
fn job_stdin_script(job: Uuid, data: &[u8]) -> String {
    job_stdin_script_in(JOBS_DIR, job, data)
}

fn job_stdin_script_in(dir: &str, job: Uuid, data: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        "f={fifo}; [ -p \"$f\" ] && [ ! -e {exit} ] || exit 3; printf %s {b64} | base64 -d > \"$f\"",
        fifo = shell_quote(&job_stdin_path_in(dir, job)),
        exit = shell_quote(&format!("{dir}/{job}.exit")),
        b64 = base64::engine::general_purpose::STANDARD.encode(data),
    )
}

/// One parsed answer of [`job_read_script`].
enum JobRead {
    Missing,
    Chunk {
        exit: Option<i64>,
        bytes: Vec<u8>,
        size: u64,
    },
}

fn parse_job_read(stdout: &str) -> JobRead {
    let mut lines = stdout.lines();
    let head = lines.next().unwrap_or("");
    if head == "MISSING" {
        return JobRead::Missing;
    }
    let exit = head
        .strip_prefix("EXIT:")
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .and_then(|e| e.parse::<i64>().ok());
    let bytes = base64_decode(lines.next().unwrap_or(""));
    let size = lines
        .next()
        .and_then(|l| l.strip_prefix("SIZE:"))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or(0);
    JobRead::Chunk { exit, bytes, size }
}

// Host-side job metadata: `<state_dir>/agent-session-jobs/<session>.json`.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobMeta {
    job: Uuid,
    command: String,
    stdin: bool,
    max_output_bytes: u64,
    started_at: DateTime<Utc>,
    #[serde(default)]
    killed: bool,
    /// `supervisor` for jobs run by the guest agent; empty for shell-script jobs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    engine: String,
    /// How a supervised job ended: `exited`, `signaled`, `timed_out` or `killed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    #[serde(default)]
    dropped_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished_at: Option<DateTime<Utc>>,
}

impl JobMeta {
    fn state(&self) -> &'static str {
        match self.phase.as_deref() {
            Some("signaled") => return "signaled",
            Some("timed_out") => return "timed_out",
            Some("killed") => return "killed",
            _ => {}
        }
        match (self.exit_code, self.killed) {
            (None, _) => "running",
            (Some(_), true) => "killed",
            (Some(_), false) => "exited",
        }
    }
}

static JOB_META_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn job_meta_path(m: &VmManager, id: Uuid) -> PathBuf {
    m.cfg
        .state_dir
        .join("agent-session-jobs")
        .join(format!("{id}.json"))
}

async fn job_meta_read(m: &VmManager, id: Uuid) -> Vec<JobMeta> {
    match tokio::fs::read(job_meta_path(m, id)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Read-modify-write of one session's job metadata. Failures are logged, never fatal.
async fn job_meta_update(m: &VmManager, id: Uuid, f: impl FnOnce(&mut Vec<JobMeta>)) {
    let _g = JOB_META_LOCK.lock().await;
    let mut all = job_meta_read(m, id).await;
    f(&mut all);
    let path = job_meta_path(m, id);
    let res = async {
        if let Some(d) = path.parent() {
            tokio::fs::create_dir_all(d).await?;
        }
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, serde_json::to_vec_pretty(&all)?).await?;
        tokio::fs::rename(&tmp, &path).await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(session = %id, error = %e, "agent session: job metadata not saved");
    }
}

/// Notes an observed exit in the metadata (first observation wins).
async fn job_meta_exited(m: &VmManager, id: Uuid, job: Uuid, code: i64) {
    job_meta_update(m, id, |all| {
        if let Some(j) = all.iter_mut().find(|j| j.job == job)
            && j.exit_code.is_none()
        {
            j.exit_code = Some(code);
            j.finished_at = Some(Utc::now());
        }
    })
    .await;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<JobBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(role, &scope, id, Op::Jobs)?;
    let mut work = Some(begin_work(id)?);
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    egress_bound(&s)?;
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
    match approval_gate(
        &m,
        &s,
        "job",
        &body.command,
        json!({
            "stdin": body.stdin,
            "max_output_bytes": body.max_output_bytes,
            "timeout_seconds": body.timeout_seconds,
        }),
        body.approval_id,
        body.approval_wait_seconds,
        &mut work,
    )
    .await?
    {
        AskGate::Clear | AskGate::Approved => {}
        AskGate::Required(a) => return Ok(approval_required(&a)),
    }
    let running_jobs = match s.budget.as_ref().and_then(|b| b.max_concurrent_jobs) {
        Some(_) if m.ensure_running_for_request(s.sandbox_id).await.is_ok() => {
            refresh_job_states(&m, &s).await;
            job_meta_read(&m, id)
                .await
                .iter()
                .filter(|j| j.exit_code.is_none())
                .count() as u64
        }
        _ => 0,
    };
    let job_ms = match &s.budget {
        Some(b) if b.max_compute_seconds.is_some() => job_compute_ms(&m, id).await,
        _ => 0,
    };
    usage_update(&m, id, |u| {
        if let Some(b) = &s.budget {
            b.check_job(u, running_jobs, job_ms)?;
        }
        u.jobs += 1;
        Ok(())
    })
    .await?;
    let max_output = body.max_output_bytes.unwrap_or(JOB_DEFAULT_MAX_OUTPUT);
    if max_output == 0 || max_output > JOB_MAX_OUTPUT {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: format!("max_output_bytes must be 1-{JOB_MAX_OUTPUT}"),
        });
    }
    // The session's output budget caps what a job retains.
    let max_output = match s.budget.as_ref().and_then(|b| b.max_output_bytes) {
        Some(cap) => max_output.min(cap),
        None => max_output,
    };
    m.ensure_running_for_request(s.sandbox_id).await?;
    budget_check_workspace(&m, &s).await?;
    rewind_capture(&m, &s, "job", &body.command).await;
    let job = Uuid::new_v4();
    if m.guest_jobs_supported(s.sandbox_id).await {
        let started = sup_call(
            &m,
            s.sandbox_id,
            AgentRequest::JobStart {
                job_id: job.to_string(),
                command: body.command.clone(),
                process: None,
                policy: s.policy.exec.clone(),
                timeout_seconds: body.timeout_seconds,
                stdin: body.stdin,
                max_output_bytes: Some(max_output),
            },
            0,
        )
        .await?;
        record(
            &m,
            id,
            "job.start",
            json!({"job": job, "command": body.command, "stdin": body.stdin, "engine": ENGINE_SUPERVISOR}),
        );
        let meta = JobMeta {
            job,
            command: body.command.clone(),
            stdin: body.stdin,
            max_output_bytes: max_output,
            started_at: Utc::now(),
            killed: false,
            engine: ENGINE_SUPERVISOR.into(),
            phase: None,
            signal: None,
            dropped_bytes: 0,
            exit_code: None,
            finished_at: None,
        };
        job_meta_update(&m, id, |all| all.push(meta)).await;
        let enforcement = match started {
            AgentResponse::JobStarted { status } => status.enforcement,
            _ => None,
        };
        return Ok((
            StatusCode::CREATED,
            Json(json!({
                "job": job, "state": "running", "engine": ENGINE_SUPERVISOR, "stdin": body.stdin,
                "max_output_bytes": max_output, "timeout_seconds": body.timeout_seconds,
                "enforcement": enforcement,
            })),
        ));
    }
    if body.timeout_seconds.is_some() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "timeout_seconds needs the guest job supervisor (jobs-v2), which this guest agent does not have; wrap the command in `timeout` instead".into(),
        });
    }
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            job_start_script(job, &body.command, body.stdin, max_output),
            Some(30),
            s.policy.exec.clone(),
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
        json!({"job": job, "command": body.command, "stdin": body.stdin}),
    );
    let meta = JobMeta {
        job,
        command: body.command.clone(),
        stdin: body.stdin,
        max_output_bytes: max_output,
        started_at: Utc::now(),
        killed: false,
        engine: String::new(),
        phase: None,
        signal: None,
        dropped_bytes: 0,
        exit_code: None,
        finished_at: None,
    };
    job_meta_update(&m, id, |all| all.push(meta)).await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "job": job, "state": "running", "stdin": body.stdin, "max_output_bytes": max_output,
        })),
    ))
}

#[derive(Debug, Deserialize)]
struct JobQuery {
    #[serde(default)]
    offset: u64,
    /// Supervised jobs only: where to continue the stderr stream.
    #[serde(default)]
    stderr_offset: u64,
    /// Supervised jobs only: wait up to this long (at most 25 s) for new output.
    #[serde(default)]
    wait_ms: Option<u64>,
}

async fn job_read(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, job)): Path<(Uuid, Uuid)>,
    Query(q): Query<JobQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Jobs)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let supervised = job_meta_read(&m, id)
        .await
        .iter()
        .any(|j| j.job == job && j.engine == ENGINE_SUPERVISOR);
    if supervised {
        return sup_job_read(&m, &s, job, &q).await.map(Json);
    }
    let res = m
        .exec_with_policy(s.sandbox_id, job_read_script(job, q.offset), Some(30), None)
        .await?;
    let out = json!(res);
    let (exit, bytes, size) = match parse_job_read(out["stdout"].as_str().unwrap_or("")) {
        JobRead::Missing => {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: format!("job {job} not found in session {id}"),
            });
        }
        JobRead::Chunk { exit, bytes, size } => (exit, bytes, size),
    };
    if let Some(code) = exit {
        job_meta_exited(&m, id, job, code).await;
    }
    let max = job_meta_read(&m, id)
        .await
        .iter()
        .find(|j| j.job == job)
        .map_or(JOB_DEFAULT_MAX_OUTPUT, |j| j.max_output_bytes);
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
        // the log reached max_output_bytes: later output was not recorded
        "truncated": size >= max,
    })))
}

/// One supervised read: stdout from `offset` (long poll), stderr from `stderr_offset` (no wait).
async fn sup_job_read(m: &VmManager, s: &Session, job: Uuid, q: &JobQuery) -> ApiResult<Value> {
    let wait = q
        .wait_ms
        .unwrap_or(0)
        .min(fluxvm_guest_protocol::MAX_JOB_WAIT_MS);
    let read = |stream, offset, wait_ms| AgentRequest::JobRead {
        job_id: job.to_string(),
        stream,
        offset,
        max_bytes: Some(JOB_READ_MAX as u32),
        wait_ms: Some(wait_ms),
    };
    let AgentResponse::JobOutput {
        data_base64: out64,
        next_offset,
        eof: out_eof,
        status,
    } = sup_call(
        m,
        s.sandbox_id,
        read(JobStream::Stdout, q.offset, wait),
        wait / 1000 + 1,
    )
    .await?
    else {
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: "unexpected answer from the guest agent".into(),
        });
    };
    let AgentResponse::JobOutput {
        data_base64: err64,
        next_offset: err_next,
        eof: err_eof,
        ..
    } = sup_call(
        m,
        s.sandbox_id,
        read(JobStream::Stderr, q.stderr_offset, 0),
        0,
    )
    .await?
    else {
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: "unexpected answer from the guest agent".into(),
        });
    };
    job_meta_status(m, s.id, &status).await;
    let (out, err) = (base64_decode(&out64), base64_decode(&err64));
    Ok(json!({
        "job": job,
        "engine": ENGINE_SUPERVISOR,
        "state": phase_name(status.phase),
        "exit_code": status.exit_code,
        "signal": status.signal,
        "output": String::from_utf8_lossy(&out),
        "output_base64": out64,
        "offset": q.offset,
        "next_offset": next_offset,
        "stderr": String::from_utf8_lossy(&err),
        "stderr_base64": err64,
        "stderr_offset": q.stderr_offset,
        "stderr_next_offset": err_next,
        "more": out.len() >= JOB_READ_MAX || err.len() >= JOB_READ_MAX,
        // data past the retention cap was read and discarded; the job was not slowed or killed
        "truncated": status.stdout_dropped > 0 || status.stderr_dropped > 0,
        "dropped_bytes": {"stdout": status.stdout_dropped, "stderr": status.stderr_dropped},
        "eof": status.phase.is_final() && out_eof && err_eof,
    }))
}

/// Reads the guest's job exit files in one exec and records each observed exit.
async fn refresh_job_states(m: &VmManager, s: &Session) {
    // Supervised jobs: one JobList answers for all of them.
    if job_meta_read(m, s.id)
        .await
        .iter()
        .any(|j| j.engine == ENGINE_SUPERVISOR && j.exit_code.is_none())
        && let Ok(AgentResponse::Jobs { jobs }) =
            sup_call(m, s.sandbox_id, AgentRequest::JobList, 0).await
    {
        for st in &jobs {
            job_meta_status(m, s.id, st).await;
        }
    }
    let Ok(res) = m
        .exec_with_policy(
            s.sandbox_id,
            format!(
                "cd {} 2>/dev/null && for f in *.exit; do [ -e \"$f\" ] && echo \"$f $(cat \"$f\")\"; done; true",
                shell_quote(JOBS_DIR)
            ),
            Some(30),
            None,
        )
        .await
    else {
        return;
    };
    let out = json!(res);
    let exits: Vec<(Uuid, i64)> = out["stdout"]
        .as_str()
        .unwrap_or("")
        .lines()
        .filter_map(|l| {
            let (f, code) = l.split_once(' ')?;
            Some((
                Uuid::parse_str(f.strip_suffix(".exit")?).ok()?,
                code.trim().parse().ok()?,
            ))
        })
        .collect();
    for (job, code) in exits {
        job_meta_exited(m, s.id, job, code).await;
    }
}

/// `GET .../jobs`: every job the session started, with its state refreshed from the guest in one
/// exec (the exit files), so it is right after a daemon restart too.
async fn job_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Jobs)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    if s.state == "running" && m.ensure_running_for_request(s.sandbox_id).await.is_ok() {
        refresh_job_states(&m, &s).await;
    }
    let items: Vec<Value> = job_meta_read(&m, id)
        .await
        .into_iter()
        .map(|j| {
            json!({
                "job": j.job, "command": j.command, "state": j.state(), "exit_code": j.exit_code,
                "signal": j.signal, "engine": j.engine, "dropped_bytes": j.dropped_bytes,
                "stdin": j.stdin, "max_output_bytes": j.max_output_bytes,
                "started_at": j.started_at, "finished_at": j.finished_at,
            })
        })
        .collect();
    Ok(Json(json!({"items": items})))
}

#[derive(Debug, Deserialize)]
struct StdinBody {
    /// UTF-8 text to write.
    #[serde(default)]
    text: Option<String>,
    /// Or raw bytes, base64.
    #[serde(default)]
    data_base64: Option<String>,
    /// Close the job's stdin after this write (EOF). Needs the guest job supervisor.
    #[serde(default)]
    close: bool,
}

/// `POST .../jobs/{job}/stdin`: append to a job started with `stdin: true`. There is no
/// end-of-file for shell-script jobs: a program that waits for EOF should read from a file you put
/// in the guest. Supervised jobs take `close: true`.
async fn job_stdin(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, job)): Path<(Uuid, Uuid)>,
    Json(body): Json<StdinBody>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Jobs)?;
    let bad = |message: &str| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
    };
    let data = match (body.text, body.data_base64) {
        (Some(t), None) => t.into_bytes(),
        (None, Some(b)) => base64_decode(&b),
        (None, None) if body.close => Vec::new(),
        _ => return Err(bad("give exactly one of text or data_base64")),
    };
    let supervised_max = fluxvm_guest_protocol::MAX_JOB_READ_BYTES as usize;
    if (data.is_empty() && !body.close) || data.len() > supervised_max.max(JOB_STDIN_MAX) {
        return Err(bad(&format!("stdin writes are 1-{JOB_STDIN_MAX} bytes")));
    }
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let supervised = job_meta_read(&m, id)
        .await
        .iter()
        .any(|j| j.job == job && j.engine == ENGINE_SUPERVISOR);
    if supervised {
        use base64::Engine as _;
        m.ensure_running_for_request(s.sandbox_id).await?;
        sup_call(
            &m,
            s.sandbox_id,
            AgentRequest::JobWrite {
                job_id: job.to_string(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                close: body.close,
            },
            0,
        )
        .await?;
        record(
            &m,
            id,
            "job.stdin",
            json!({"job": job, "bytes": data.len(), "close": body.close}),
        );
        return Ok(Json(
            json!({"job": job, "bytes": data.len(), "closed": body.close}),
        ));
    }
    if body.close {
        return Err(bad(
            "closing stdin needs the guest job supervisor (jobs-v2); this job was started without it",
        ));
    }
    if data.len() > JOB_STDIN_MAX {
        return Err(bad(&format!("stdin writes are 1-{JOB_STDIN_MAX} bytes")));
    }
    if !job_meta_read(&m, id)
        .await
        .iter()
        .any(|j| j.job == job && j.stdin)
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!(
                "job {job} was not started with stdin: true (or is not in this session)"
            ),
        });
    }
    // Same confinement as the job itself: it is a write into the guest on the session's behalf.
    m.ensure_running_for_request(s.sandbox_id).await?;
    let res = m
        .exec_with_policy(s.sandbox_id, job_stdin_script(job, &data), Some(10), None)
        .await?;
    match json!(res)["exit_code"].as_i64() {
        Some(0) => {}
        Some(3) => {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!("job {job} has already exited"),
            });
        }
        code => {
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!("could not write to the job's stdin (exit {code:?})"),
            });
        }
    }
    // Sizes only: what a program is fed may be a secret.
    record(
        &m,
        id,
        "job.stdin",
        json!({"job": job, "bytes": data.len()}),
    );
    Ok(Json(json!({"job": job, "bytes": data.len()})))
}

static JOB_STREAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct StreamSlot;

impl Drop for StreamSlot {
    fn drop(&mut self) {
        JOB_STREAMS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

const EVENT_STREAM_POLL: std::time::Duration = std::time::Duration::from_millis(500);
const EVENT_STREAM_MAX_SECS: u64 = 3600;
const MAX_EVENT_STREAMS: usize = 64;
static EVENT_STREAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct EventSlot;

impl Drop for EventSlot {
    fn drop(&mut self) {
        EVENT_STREAMS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Debug, Deserialize, Default)]
struct EventsStreamQuery {
    /// Replay only what happened after this time, then follow. Default: replay everything.
    #[serde(default)]
    since: Option<DateTime<Utc>>,
}

/// Event id of the stream: `<ledger entries delivered>|<RFC 3339 time of the last VM event>`.
fn event_cursor(ledger_seen: usize, vm_ts: Option<DateTime<Utc>>) -> String {
    let ts = vm_ts
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_default();
    format!("{ledger_seen}|{ts}")
}

fn parse_event_cursor(v: &str) -> Option<(usize, Option<DateTime<Utc>>)> {
    let (n, ts) = v.trim().split_once('|')?;
    let n = n.parse().ok()?;
    if ts.is_empty() {
        return Some((n, None));
    }
    Some((n, Some(ts.parse().ok()?)))
}

/// `GET .../events/stream`: Server-Sent Events for the session's own record. `ledger` events carry
/// one ledger entry (what the API did), `vm` events one hypervisor event (what the VMM saw). The
/// existing history is replayed first (or only what is newer than `?since=`), then new entries
/// follow about every 500 ms. Each event's id is a cursor: reconnecting with `Last-Event-ID`
/// continues with no ledger entry repeated or missed; a VM event that shares its exact timestamp
/// with the last one seen may repeat (at-least-once). An `error` event ends the stream when the
/// session is deleted, expired or gone (after its last entries) or the hour is up. At most 64 streams per host. Needs `read`, like `events`.
async fn events_stream(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Query(q): Query<EventsStreamQuery>,
) -> Result<
    axum::response::sse::Sse<
        impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    ApiError,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    authorize(role, &scope, id, Op::Read)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    let resume = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_event_cursor);
    let (mut seen, mut vm_ts) = match (resume, q.since) {
        (Some(c), _) => c,
        (None, Some(t)) => {
            let older = read_ledger(&m, id, None)
                .iter()
                .filter(|v| {
                    v["ts"]
                        .as_str()
                        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
                        .is_some_and(|ts| ts <= t)
                })
                .count();
            (older, Some(t))
        }
        (None, None) => (0, None),
    };
    if EVENT_STREAMS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_EVENT_STREAMS {
        EVENT_STREAMS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: format!("at most {MAX_EVENT_STREAMS} event streams at once on this host"),
        });
    }
    let slot = EventSlot;
    let sandbox = s.sandbox_id;
    let tenant = s.tenant.clone();
    let stream = async_stream::stream! {
        let _slot = slot;
        let began = std::time::Instant::now();
        let mut vm_seen: Vec<String> = Vec::new();
        let err = |msg: String| Event::default().event("error").data(json!({"error": msg}).to_string());
        loop {
            if began.elapsed().as_secs() > EVENT_STREAM_MAX_SECS {
                yield Ok(err("stream time limit reached; reconnect with Last-Event-ID".into()));
                break;
            }
            let state = match load(&m, id, &tenant).await {
                Ok(cur) => cur.state,
                Err(e) => {
                    yield Ok(err(e.message));
                    break;
                }
            };
            let fresh: Vec<Value> = read_ledger(&m, id, None).into_iter().skip(seen).collect();
            for entry in fresh {
                seen += 1;
                yield Ok(Event::default().event("ledger").id(event_cursor(seen, vm_ts)).data(entry.to_string()));
            }
            if let Some(log) = fluxvm_scheduler::events::global() {
                let filter = fluxvm_scheduler::EventFilter {
                    vm: Some(sandbox),
                    // The filter is strictly-after; step back so events sharing the last timestamp
                    // are seen again and dropped by `vm_seen` instead of being missed.
                    since: vm_ts.map(|t| t - chrono::Duration::nanoseconds(1)),
                    event: None,
                    limit: Some(500),
                };
                let mut events = log.list(&filter).unwrap_or_default();
                events.sort_by_key(|e| e.ts);
                for e in events {
                    let body = json!(e).to_string();
                    if vm_ts == Some(e.ts) {
                        if vm_seen.contains(&body) {
                            continue;
                        }
                    } else {
                        vm_ts = Some(e.ts);
                        vm_seen.clear();
                    }
                    vm_seen.push(body.clone());
                    yield Ok(Event::default().event("vm").id(event_cursor(seen, vm_ts)).data(body));
                }
            }
            // A deleted, expired or gone session keeps its record but will not change: say so
            // after its last entries (the delete itself) went out, instead of idling to the limit.
            if state != "running" {
                yield Ok(err(format!("session is {state}")));
                break;
            }
            tokio::time::sleep(EVENT_STREAM_POLL).await;
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Debug, Deserialize, Default)]
struct StreamQuery {
    #[serde(default)]
    offset: Option<u64>,
    /// Supervised jobs: where to continue stderr.
    #[serde(default)]
    stderr_offset: Option<u64>,
}

/// `GET .../jobs/{job}/stream`: Server-Sent Events. `output` events carry `{offset, next_offset,
/// output, output_base64}` and use `next_offset` as the event id, so a client that reconnects
/// with `Last-Event-ID` (or `?offset=`) continues without gaps or repeats. The stream ends with an
/// `exit` event, or an `error` event if the job or session went away. At most 16 streams per
/// host, one hour each.
async fn job_stream(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, job)): Path<(Uuid, Uuid)>,
    headers: axum::http::HeaderMap,
    Query(q): Query<StreamQuery>,
) -> Result<
    axum::response::sse::Sse<
        impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    ApiError,
> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    authorize(role, &scope, id, Op::Jobs)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(parse_event_offsets);
    let start = q.offset.or(last.map(|l| l.0)).unwrap_or(0);
    let err_start = q.stderr_offset.or(last.map(|l| l.1)).unwrap_or(0);
    let supervised = job_meta_read(&m, id)
        .await
        .iter()
        .any(|j| j.job == job && j.engine == ENGINE_SUPERVISOR);
    if JOB_STREAMS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_JOB_STREAMS {
        JOB_STREAMS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: format!("at most {MAX_JOB_STREAMS} job streams at once on this host"),
        });
    }
    let slot = StreamSlot;
    let sandbox = s.sandbox_id;
    let stream = async_stream::stream! {
        let _slot = slot;
        let began = std::time::Instant::now();
        let mut offset = start;
        let mut err_offset = err_start;
        let err = |msg: String| Event::default().event("error").data(json!({"error": msg}).to_string());
        loop {
            if began.elapsed().as_secs() > JOB_STREAM_MAX_SECS {
                yield Ok(err(format!("stream time limit reached; reconnect with Last-Event-ID {offset}")));
                break;
            }
            // The session may have been restored, expired or deleted since the last poll.
            match load(&m, id, &s.tenant).await {
                Ok(cur) if cur.state == "running" => {}
                Ok(cur) => {
                    yield Ok(err(format!("session is {}", cur.state)));
                    break;
                }
                Err(e) => {
                    yield Ok(err(e.message));
                    break;
                }
            }
            if supervised {
                use base64::Engine as _;
                if let Err(e) = m.ensure_running_for_request(sandbox).await {
                    yield Ok(err(format!("{e:#}")));
                    break;
                }
                let mk = |stream, off, wait_ms| AgentRequest::JobRead {
                    job_id: job.to_string(),
                    stream,
                    offset: off,
                    max_bytes: Some(JOB_READ_MAX as u32),
                    wait_ms: Some(wait_ms),
                };
                // stdout long-polls, so the loop is paced by the guest, not by a timer.
                let out = sup_call(&m, sandbox, mk(JobStream::Stdout, offset, 2000), 2).await;
                let (out_bytes, out_eof, status) = match out {
                    Ok(AgentResponse::JobOutput { data_base64, eof, status, .. }) => {
                        (base64_decode(&data_base64), eof, status)
                    }
                    Ok(_) => {
                        yield Ok(err("unexpected answer from the guest agent".into()));
                        break;
                    }
                    Err(e) => {
                        yield Ok(err(e.message));
                        break;
                    }
                };
                let (err_bytes, err_eof) = match sup_call(&m, sandbox, mk(JobStream::Stderr, err_offset, 0), 0).await {
                    Ok(AgentResponse::JobOutput { data_base64, eof, .. }) => (base64_decode(&data_base64), eof),
                    Ok(_) => (Vec::new(), false),
                    Err(e) => {
                        yield Ok(err(e.message));
                        break;
                    }
                };
                let b64 = base64::engine::general_purpose::STANDARD;
                if !out_bytes.is_empty() {
                    let from = offset;
                    offset += out_bytes.len() as u64;
                    // The id carries both offsets so a reconnect resumes both streams.
                    yield Ok(Event::default()
                        .event("output")
                        .id(format!("{offset}:{err_offset}"))
                        .data(json!({
                            "offset": from,
                            "next_offset": offset,
                            "output": String::from_utf8_lossy(&out_bytes),
                            "output_base64": b64.encode(&out_bytes),
                        }).to_string()));
                }
                if !err_bytes.is_empty() {
                    let from = err_offset;
                    err_offset += err_bytes.len() as u64;
                    yield Ok(Event::default()
                        .event("stderr")
                        .id(format!("{offset}:{err_offset}"))
                        .data(json!({
                            "offset": from,
                            "next_offset": err_offset,
                            "output": String::from_utf8_lossy(&err_bytes),
                            "output_base64": b64.encode(&err_bytes),
                        }).to_string()));
                }
                let more = out_bytes.len() >= JOB_READ_MAX || err_bytes.len() >= JOB_READ_MAX;
                if status.phase.is_final() && out_eof && err_eof && !more {
                    job_meta_status(&m, id, &status).await;
                    yield Ok(Event::default()
                        .event("exit")
                        .id(format!("{offset}:{err_offset}"))
                        .data(json!({
                            "exit_code": status.exit_code,
                            "state": phase_name(status.phase),
                            "signal": status.signal,
                            "next_offset": offset,
                            "stderr_next_offset": err_offset,
                            "dropped_bytes": {"stdout": status.stdout_dropped, "stderr": status.stderr_dropped},
                        }).to_string()));
                    break;
                }
                continue;
            }
            let res = match m.ensure_running_for_request(sandbox).await {
                Ok(_) => m
                    .exec_with_policy(sandbox, job_read_script(job, offset), Some(30), None)
                    .await,
                Err(e) => Err(e),
            };
            let res = match res {
                Ok(r) => json!(r),
                Err(e) => {
                    yield Ok(err(format!("{e:#}")));
                    break;
                }
            };
            match parse_job_read(res["stdout"].as_str().unwrap_or("")) {
                JobRead::Missing => {
                    yield Ok(err(format!("job {job} not found in session {id}")));
                    break;
                }
                JobRead::Chunk { exit, bytes, .. } => {
                    let more = bytes.len() >= JOB_READ_MAX;
                    if !bytes.is_empty() {
                        let next = offset + bytes.len() as u64;
                        use base64::Engine as _;
                        yield Ok(Event::default()
                            .event("output")
                            .id(next.to_string())
                            .data(json!({
                                "offset": offset,
                                "next_offset": next,
                                "output": String::from_utf8_lossy(&bytes),
                                "output_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                            }).to_string()));
                        offset = next;
                    }
                    if more {
                        continue;
                    }
                    if let Some(code) = exit {
                        job_meta_exited(&m, id, job, code).await;
                        yield Ok(Event::default()
                            .event("exit")
                            .id(offset.to_string())
                            .data(json!({"exit_code": code, "next_offset": offset}).to_string()));
                        break;
                    }
                }
            }
            tokio::time::sleep(JOB_STREAM_POLL).await;
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Debug, Deserialize, Default)]
struct KillQuery {
    /// Supervised jobs: a signal name or number (default TERM). The whole process tree gets it.
    #[serde(default)]
    signal: Option<String>,
}

async fn job_kill(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, job)): Path<(Uuid, Uuid)>,
    Query(q): Query<KillQuery>,
) -> ApiResult<StatusCode> {
    authorize(role, &scope, id, Op::Jobs)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    let supervised = job_meta_read(&m, id)
        .await
        .iter()
        .any(|j| j.job == job && j.engine == ENGINE_SUPERVISOR);
    if supervised {
        let sig = match q.signal.as_deref() {
            None => 15,
            Some(name) => signal_number(name).ok_or_else(|| ApiError {
                status: StatusCode::BAD_REQUEST,
                message: format!("unknown signal {name:?}; use a name like TERM or KILL, or 1-64"),
            })?,
        };
        m.ensure_running_for_request(s.sandbox_id).await?;
        sup_call(
            &m,
            s.sandbox_id,
            AgentRequest::JobSignal {
                job_id: job.to_string(),
                signal: sig,
                tree: true,
            },
            0,
        )
        .await?;
        job_meta_update(&m, id, |all| {
            if let Some(j) = all.iter_mut().find(|j| j.job == job) {
                j.killed = true;
            }
        })
        .await;
        record(&m, id, "job.kill", json!({"job": job, "signal": sig}));
        return Ok(StatusCode::NO_CONTENT);
    }
    let pid = shell_quote(&format!("{JOBS_DIR}/{job}.pid"));
    let exit = shell_quote(&format!("{JOBS_DIR}/{job}.exit"));
    // Negative pid = the whole process group the job started in. That group includes the wrapper
    // that would have written the exit file, so a killed job gets the conventional 143 here.
    let cmd = format!(
        "p=$(cat {pid} 2>/dev/null) && [ -n \"$p\" ] && {{ kill -TERM -- \"-$p\" 2>/dev/null \
         || {{ pkill -TERM -P \"$p\" 2>/dev/null; kill -TERM \"$p\" 2>/dev/null; }}; }}; \
         sleep 0.3; [ -e {exit} ] || echo 143 > {exit}; true"
    );
    m.exec_with_policy(s.sandbox_id, cmd, Some(30), None)
        .await?;
    job_meta_update(&m, id, |all| {
        if let Some(j) = all.iter_mut().find(|j| j.job == job) {
            j.killed = true;
            if j.exit_code.is_none() {
                j.exit_code = Some(143);
                j.finished_at = Some(Utc::now());
            }
        }
    })
    .await;
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
        .exec_with_policy(s.sandbox_id, cmd, Some(30), s.policy.exec.clone())
        .await?;
    Ok(json!(res))
}

async fn files_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::FilesRead)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::FilesRead)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<WriteBody>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::FilesWrite)?;
    let _work = begin_work(id)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    path_arg(&body.path)?;
    if body.path.ends_with('/') {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "path must name a file".into(),
        });
    }
    rewind_capture(&m, &s, "file", &format!("write {}", body.path)).await;
    if body.parents {
        ensure_parent_dir(&m, &s, &body.path).await?;
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

/// Creates the directory a file is going into. The guest user is not root on stock images: fall
/// back to sudo, then hand the topmost directory that had to be created to that user, so this and
/// later writes succeed.
async fn ensure_parent_dir(m: &Arc<VmManager>, s: &Session, path: &str) -> ApiResult<()> {
    let Some(dir) = std::path::Path::new(path)
        .parent()
        .and_then(|d| d.to_str())
        .filter(|d| *d != "/")
    else {
        return Ok(());
    };
    let out = guest_exec(
        m,
        s,
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
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Resumable transfer: ranged reads and chunked uploads (see `agent_session_transfer`).

#[derive(Debug, Deserialize)]
struct RangeQuery {
    path: String,
    #[serde(default)]
    offset: u64,
    /// Bytes to return (default and most: 4 MiB).
    #[serde(default)]
    length: Option<u64>,
    /// Also return the file's SHA-256 (the guest hashes the whole file).
    #[serde(default)]
    checksum: bool,
}

/// `GET .../files/range`: a slice of a file with its size, and its SHA-256 on request.
async fn files_range(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<RangeQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::FilesRead)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    path_arg(&q.path)?;
    let len = q
        .length
        .unwrap_or(transfer::MAX_RANGE_BYTES)
        .min(transfer::MAX_RANGE_BYTES);
    if len == 0 {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "length must be at least 1".into(),
        });
    }
    m.ensure_running_for_request(s.sandbox_id).await?;
    // A checksum reads the whole file, so allow it more time than a slice.
    let timeout = if q.checksum { 600 } else { 60 };
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            transfer::range_script(&q.path, q.offset, len, q.checksum),
            Some(timeout),
            s.policy.exec.clone(),
        )
        .await?;
    let out = json!(res);
    match out["exit_code"].as_i64() {
        Some(0) => {}
        Some(2) => {
            return Err(ApiError {
                status: StatusCode::NOT_FOUND,
                message: format!("{}: no such file", q.path),
            });
        }
        code => {
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!(
                    "could not read {} (exit {code:?}): {}",
                    q.path,
                    out["stderr"].as_str().unwrap_or("")
                ),
            });
        }
    }
    let r =
        transfer::parse_range(out["stdout"].as_str().unwrap_or("")).ok_or_else(|| ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: "the guest's answer was not understood (needs wc, tail, head and base64)"
                .into(),
        })?;
    let returned = base64_decode(&r.data_base64).len() as u64;
    Ok(Json(json!({
        "path": q.path,
        "size": r.size,
        "offset": q.offset,
        "length": returned,
        "eof": q.offset + returned >= r.size,
        "sha256": r.sha256,
        "content_base64": r.data_base64,
    })))
}

fn uploads_path(m: &VmManager, id: Uuid) -> PathBuf {
    m.cfg
        .state_dir
        .join("agent-session-uploads")
        .join(format!("{id}.json"))
}

static UPLOAD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn uploads_read(m: &VmManager, id: Uuid) -> Vec<Upload> {
    match tokio::fs::read(uploads_path(m, id)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Read-modify-write of the session's uploads under one lock, so two chunks cannot both be
/// accepted at the same offset.
async fn uploads_update<R>(
    m: &VmManager,
    id: Uuid,
    f: impl FnOnce(&mut Vec<Upload>) -> ApiResult<R>,
) -> ApiResult<R> {
    let _g = UPLOAD_LOCK.lock().await;
    let mut all = uploads_read(m, id).await;
    let r = f(&mut all)?;
    let path = uploads_path(m, id);
    if let Some(d) = path.parent() {
        tokio::fs::create_dir_all(d)
            .await
            .map_err(anyhow::Error::from)?;
    }
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec(&all).map_err(anyhow::Error::from)?)
        .await
        .map_err(anyhow::Error::from)?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(r)
}

fn upload_not_found(id: Uuid, upload: Uuid) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        message: format!("upload {upload} not found in session {id}"),
    }
}

#[derive(Debug, Deserialize)]
struct UploadBody {
    path: String,
    /// The file's size in bytes, at most 2 GiB. `complete` refuses a different total.
    size: u64,
    /// Lowercase hex SHA-256; checked in the guest before the file appears.
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    mode: Option<u32>,
    /// Create missing parent directories first (default true).
    #[serde(default = "yes")]
    parents: bool,
}

/// `POST .../uploads`: opens an upload. Send the chunks with `PUT .../uploads/{id}?offset=N`.
async fn upload_create(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<UploadBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    authorize(role, &scope, id, Op::FilesWrite)?;
    let _work = begin_work(id)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    egress_bound(&s)?;
    let bad = |message: &str| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: message.into(),
    };
    path_arg(&body.path)?;
    if body.path.ends_with('/') {
        return Err(bad("path must name a file"));
    }
    if body.size == 0 || body.size > transfer::MAX_UPLOAD_BYTES {
        return Err(bad(&format!(
            "size must be 1-{} bytes (use files/content for an empty file)",
            transfer::MAX_UPLOAD_BYTES
        )));
    }
    let sha256 = match body.sha256.as_deref() {
        None => None,
        Some(h) => {
            Some(transfer::normalize_sha256(h).ok_or_else(|| bad("sha256 must be 64 hex digits"))?)
        }
    };
    if body.mode.is_some_and(|m| m > 0o7777) {
        return Err(bad("mode must be a permission mask up to 0o7777"));
    }
    if body.parents {
        ensure_parent_dir(&m, &s, &body.path).await?;
    }
    let u = Upload {
        id: Uuid::new_v4(),
        path: body.path,
        size: body.size,
        sha256,
        mode: body.mode,
        received: 0,
        parts: 0,
        state: UploadState::Open,
        created_at: Utc::now(),
        completed_at: None,
    };
    let created = u.clone();
    uploads_update(&m, id, move |all| {
        let open = all.iter().filter(|x| x.state == UploadState::Open).count();
        if open >= transfer::MAX_OPEN_UPLOADS {
            return Err(ApiError {
                status: StatusCode::TOO_MANY_REQUESTS,
                message: format!(
                    "at most {} uploads may be open at once; complete or abort one",
                    transfer::MAX_OPEN_UPLOADS
                ),
            });
        }
        all.push(created);
        Ok(())
    })
    .await?;
    let opened = guest_exec(&m, &s, transfer::open_script(&u)).await;
    if !matches!(&opened, Ok(o) if o["exit_code"].as_i64() == Some(0)) {
        let _ = uploads_update(&m, id, |all| {
            all.retain(|x| x.id != u.id);
            Ok(())
        })
        .await;
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: "could not prepare the upload directory in the guest".into(),
        });
    }
    record(
        &m,
        id,
        "upload.open",
        json!({"upload": u.id, "path": u.path, "size": u.size}),
    );
    Ok((StatusCode::CREATED, Json(json!(u))))
}

async fn upload_list(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::FilesRead)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    Ok(Json(json!({"items": uploads_read(&m, id).await})))
}

/// `GET .../uploads/{id}`: where the upload stands; `received` is where to continue.
async fn upload_get(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, upload)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::FilesRead)?;
    load(&m, id, &caller_tenant(&token_tenant)).await?;
    uploads_read(&m, id)
        .await
        .into_iter()
        .find(|u| u.id == upload)
        .map(|u| Json(json!(u)))
        .ok_or_else(|| upload_not_found(id, upload))
}

#[derive(Debug, Deserialize)]
struct ChunkQuery {
    offset: u64,
}

/// `PUT .../uploads/{id}?offset=N`: the next chunk, raw bytes (at most 4 MiB). `N` must be the
/// upload's `received`; otherwise `409` says where to continue.
async fn upload_chunk(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, upload)): Path<(Uuid, Uuid)>,
    Query(q): Query<ChunkQuery>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    use base64::Engine as _;
    authorize(role, &scope, id, Op::FilesWrite)?;
    let _work = begin_work(id)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    egress_bound(&s)?;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let current = uploads_read(&m, id)
        .await
        .into_iter()
        .find(|u| u.id == upload)
        .ok_or_else(|| upload_not_found(id, upload))?;
    if current.state != UploadState::Open {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!("upload {upload} is already complete"),
        });
    }
    let index = match current.check_chunk(q.offset, body.len()) {
        Chunk::Accept { index } => index,
        Chunk::Resume { received } => {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!(
                    "upload {upload} has {received} bytes: send the next chunk with offset={received}"
                ),
            });
        }
        Chunk::TooBig(why) => {
            return Err(ApiError {
                status: StatusCode::BAD_REQUEST,
                message: why,
            });
        }
    };
    // The part goes to the guest first. If the host record then fails to update, the chunk is
    // simply sent again: the same part path is overwritten.
    m.put_file(
        s.sandbox_id,
        current.part_path(index),
        base64::engine::general_purpose::STANDARD.encode(&body),
        Some(0o600),
    )
    .await?;
    let len = body.len() as u64;
    let after = uploads_update(&m, id, move |all| {
        let u = all
            .iter_mut()
            .find(|u| u.id == upload)
            .ok_or_else(|| upload_not_found(id, upload))?;
        if u.received != q.offset {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!(
                    "upload {upload} moved to {} bytes; resume there",
                    u.received
                ),
            });
        }
        u.received += len;
        u.parts += 1;
        Ok(u.clone())
    })
    .await?;
    Ok(Json(json!({
        "upload": upload, "received": after.received, "size": after.size,
        "parts": after.parts, "remaining": after.size - after.received,
    })))
}

/// `POST .../uploads/{id}/complete`: assembles the parts, verifies size and checksum in the
/// guest, and renames the file into place.
async fn upload_complete(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, upload)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::FilesWrite)?;
    let _work = begin_work(id)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    egress_bound(&s)?;
    let u = uploads_read(&m, id)
        .await
        .into_iter()
        .find(|u| u.id == upload)
        .ok_or_else(|| upload_not_found(id, upload))?;
    if u.state == UploadState::Complete {
        // Completing twice is a no-op: a retry after a lost answer succeeds.
        return Ok(Json(json!(u)));
    }
    if u.received != u.size {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!(
                "upload {upload} has {} of {} bytes: continue with offset={}",
                u.received, u.size, u.received
            ),
        });
    }
    rewind_capture(&m, &s, "file", &format!("upload {}", u.path)).await;
    m.ensure_running_for_request(s.sandbox_id).await?;
    let res = m
        .exec_with_policy(
            s.sandbox_id,
            transfer::complete_script(&u),
            Some(900),
            s.policy.exec.clone(),
        )
        .await?;
    let out = json!(res);
    match out["exit_code"].as_i64() {
        Some(0) => {}
        Some(code @ (3..=6)) => {
            let why = match code {
                3 => "the assembled size does not match the declared size",
                4 => "the checksum does not match",
                5 => "the guest has no sha256sum or shasum to verify the checksum",
                _ => "a part is missing in the guest",
            };
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!(
                    "upload {upload} was not completed: {why}; nothing was written to {}",
                    u.path
                ),
            });
        }
        code => {
            return Err(ApiError {
                status: StatusCode::BAD_GATEWAY,
                message: format!(
                    "could not assemble the upload (exit {code:?}): {}",
                    out["stderr"].as_str().unwrap_or("")
                ),
            });
        }
    }
    let done = uploads_update(&m, id, move |all| {
        let u = all
            .iter_mut()
            .find(|u| u.id == upload)
            .ok_or_else(|| upload_not_found(id, upload))?;
        u.state = UploadState::Complete;
        u.completed_at = Some(Utc::now());
        Ok(u.clone())
    })
    .await?;
    record(
        &m,
        id,
        "file.upload",
        json!({"path": done.path, "bytes": done.size, "sha256_verified": done.sha256.is_some()}),
    );
    Ok(Json(json!(done)))
}

/// `DELETE .../uploads/{id}`: drops an upload and its parts in the guest.
async fn upload_abort(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((id, upload)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    authorize(role, &scope, id, Op::FilesWrite)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    let u = uploads_read(&m, id)
        .await
        .into_iter()
        .find(|u| u.id == upload)
        .ok_or_else(|| upload_not_found(id, upload))?;
    if s.state == "running" && m.ensure_running_for_request(s.sandbox_id).await.is_ok() {
        let _ = guest_exec(&m, &s, transfer::abort_script(&u)).await;
    }
    uploads_update(&m, id, move |all| {
        all.retain(|x| x.id != upload);
        Ok(())
    })
    .await?;
    record(
        &m,
        id,
        "upload.abort",
        json!({"upload": upload, "path": u.path}),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn files_delete(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<PathQuery>,
) -> ApiResult<StatusCode> {
    authorize(role, &scope, id, Op::FilesWrite)?;
    let _work = begin_work(id)?;
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
    rewind_capture(&m, &s, "file", &format!("delete {}", q.path)).await;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<GitQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Git)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<GitQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Git)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Query(q): Query<GitQuery>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Git)?;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<CommitBody>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Git)?;
    let _work = begin_work(id)?;
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
    rewind_capture(&m, &s, "git", &format!("commit {}", message)).await;
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
    scope: Option<Extension<SessionScope>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(id): Path<Uuid>,
    Json(body): Json<BranchBody>,
) -> ApiResult<Json<Value>> {
    authorize(role, &scope, id, Op::Git)?;
    let _work = begin_work(id)?;
    let s = load(&m, id, &caller_tenant(&token_tenant)).await?;
    running(&s)?;
    if !valid_branch(&body.name) {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "invalid branch name".into(),
        });
    }
    let repo = repo_dir(&s, &body.repo)?;
    rewind_capture(&m, &s, "git", &format!("branch {}", body.name)).await;
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
    #[test]
    fn event_cursor_round_trips() {
        let ts: DateTime<Utc> = "2026-10-17T01:02:03.123456789Z".parse().unwrap();
        assert_eq!(
            parse_event_cursor(&event_cursor(7, Some(ts))),
            Some((7, Some(ts)))
        );
        assert_eq!(parse_event_cursor(&event_cursor(0, None)), Some((0, None)));
        assert_eq!(parse_event_cursor("garbage"), None);
        assert_eq!(parse_event_cursor("x|"), None);
        assert_eq!(parse_event_cursor("3|not-a-time"), None);
    }

    use super::*;

    fn pol(v: Value) -> PolicyBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn process_enforce_exec_compiles_programs_to_guest_paths() {
        let r = compile_policy(Some(pol(json!({
            "process": {"allow": ["git *", "/opt/tool/run --x"], "ask": ["deploy *"], "enforce": "exec"}
        }))))
        .unwrap();
        assert!(r.enforce_exec);
        let e = r.exec.expect("enforcement implies a guest policy");
        let x = e.exec.expect("exec allow-list");
        assert!(x.contains(&"/usr/bin/git".to_string()));
        assert!(x.contains(&"/usr/local/bin/deploy".to_string()));
        assert!(x.contains(&"/opt/tool/run".to_string()));
        // Always present: the shell, the probe command and the dynamic loader.
        assert!(x.contains(&"/bin/sh".to_string()));
        assert!(x.contains(&"/usr/bin/true".to_string()));
        assert!(x.contains(&"/lib64/ld-linux-x86-64.so.2".to_string()));
        // Programs the policy did not name are not there.
        assert!(
            !x.iter()
                .any(|p| p.ends_with("/curl") || p.ends_with("/python3"))
        );
        // No filesystem section: the default confinement still applies.
        assert!(!e.read.is_empty());
    }

    #[test]
    fn process_enforce_exec_keeps_a_filesystem_section() {
        let r = compile_policy(Some(pol(json!({
            "filesystem": {"read": ["/srv/data"]},
            "process": {"allow": ["ls"], "enforce": "exec"}
        }))))
        .unwrap();
        let e = r.exec.unwrap();
        assert!(e.read.contains(&"/srv/data".to_string()));
        assert!(e.exec.is_some());
    }

    #[test]
    fn process_enforce_exec_refuses_what_it_cannot_enforce() {
        let bad = |v: Value| compile_policy(Some(pol(v))).unwrap_err().message;
        // A glob in the program position cannot be a path.
        assert!(
            bad(json!({"process": {"allow": ["g*t status"], "enforce": "exec"}}))
                .contains("literal")
        );
        // Nothing allowed would run nothing at all.
        assert!(
            bad(json!({"process": {"deny": ["rm *"], "enforce": "exec"}})).contains("non-empty")
        );
        // A relative or traversing path.
        assert!(
            bad(json!({"process": {"allow": ["./x"], "enforce": "exec"}})).contains("absolute")
        );
        assert!(
            bad(json!({"process": {"allow": ["/a/../b"], "enforce": "exec"}})).contains("absolute")
        );
        // `none` is the same as leaving it out.
        let r = compile_policy(Some(pol(
            json!({"process": {"allow": ["ls"], "enforce": "none"}}),
        )))
        .unwrap();
        assert!(!r.enforce_exec && r.exec.is_none());
    }

    fn rw_point(tag: &str) -> RewindPoint {
        RewindPoint {
            tag: tag.into(),
            memory: false,
            created_at: Utc::now(),
            trigger: "exec".into(),
            command: None,
            pinned: false,
            screenshot: false,
        }
    }

    #[test]
    fn rewind_pins_survive_eviction_and_undo() {
        let mut st = RewindState {
            max_points: 3,
            ..Default::default()
        };
        for t in ["a", "b", "c"] {
            rewind_push(&mut st, rw_point(t));
        }
        rewind_pin(&mut st, "a", true).unwrap();
        // "a" is oldest but pinned: "b" goes instead.
        assert_eq!(rewind_push(&mut st, rw_point("d")), vec!["b".to_string()]);
        let tags: Vec<_> = st.points.iter().map(|p| p.tag.as_str()).collect();
        assert_eq!(tags, ["a", "c", "d"]);
        // Undo to "c" drops "c" and "d" but keeps the pinned "a" that sits before it.
        rewind_pin(&mut st, "d", true).unwrap();
        let idx = st.points.iter().position(|p| p.tag == "c").unwrap();
        assert_eq!(rewind_truncate(&mut st, idx), vec!["c".to_string()]);
        let tags: Vec<_> = st.points.iter().map(|p| p.tag.as_str()).collect();
        assert_eq!(tags, ["a", "d"]);
    }

    #[test]
    fn rewind_refuses_a_pin_that_fills_the_ring() {
        let mut st = RewindState {
            max_points: 2,
            ..Default::default()
        };
        rewind_push(&mut st, rw_point("a"));
        rewind_push(&mut st, rw_point("b"));
        rewind_pin(&mut st, "a", true).unwrap();
        // One pin leaves one slot; a second would leave none for new points.
        rewind_pin(&mut st, "b", true).unwrap_err();
        assert!(rewind_pin(&mut st, "zzz", true).is_err());
        st.max_points = 3;
        rewind_pin(&mut st, "b", true).unwrap();
        rewind_pin(&mut st, "a", false).unwrap();
    }

    #[test]
    fn rewind_ring_evicts_the_oldest_points() {
        let mut st = RewindState {
            max_points: 3,
            ..Default::default()
        };
        for t in ["a", "b", "c"] {
            assert!(rewind_push(&mut st, rw_point(t)).is_empty());
        }
        assert_eq!(rewind_push(&mut st, rw_point("d")), vec!["a".to_string()]);
        let tags: Vec<_> = st.points.iter().map(|p| p.tag.as_str()).collect();
        assert_eq!(tags, ["b", "c", "d"]);
    }

    #[test]
    fn rewind_undo_counts_back_from_the_newest_point() {
        let mut st = RewindState::default();
        assert!(rewind_target(&st, 1).is_err(), "nothing to undo yet");
        for t in ["a", "b", "c"] {
            rewind_push(&mut st, rw_point(t));
        }
        assert_eq!(rewind_target(&st, 1), Ok(2));
        assert_eq!(rewind_target(&st, 3), Ok(0));
        assert!(rewind_target(&st, 4).is_err());
        assert!(rewind_target(&st, 0).is_err());
    }

    #[test]
    fn rewind_undo_drops_the_target_and_everything_newer() {
        let mut st = RewindState::default();
        for t in ["a", "b", "c", "d"] {
            rewind_push(&mut st, rw_point(t));
        }
        let idx = rewind_target(&st, 2).unwrap();
        assert_eq!(
            rewind_truncate(&mut st, idx),
            vec!["c".to_string(), "d".to_string()]
        );
        let tags: Vec<_> = st.points.iter().map(|p| p.tag.as_str()).collect();
        assert_eq!(tags, ["a", "b"]);
        // The next undo goes one action further back from the restored state.
        assert_eq!(rewind_target(&st, 1), Ok(1));
    }

    #[test]
    fn bisect_narrows_to_the_first_bad_state() {
        // States 0..=9, the first bad one is `first_bad`; the ends are never tested again.
        for first_bad in 1..=9usize {
            let (mut lo, mut hi) = (0usize, 9usize);
            let mut tested = 0;
            while let Some(mid) = bisect_next(lo, hi) {
                tested += 1;
                if mid < first_bad {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            assert_eq!(hi, first_bad, "first_bad={first_bad}");
            assert_eq!(lo + 1, hi);
            assert!(tested <= 4, "9 states need at most ceil(log2(9)) tests");
        }
        assert_eq!(bisect_next(3, 4), None);
        assert_eq!(bisect_next(0, 2), Some(1));
    }

    #[test]
    fn rewind_triggers_are_validated_and_ordered() {
        let v = |a: &[&str]| rewind_triggers(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(v(&["git", "exec", "git"]).unwrap(), ["exec", "git"]);
        assert_eq!(
            v(&["file", "job", "git", "exec"]).unwrap(),
            ["exec", "job", "file", "git"]
        );
        assert!(v(&[]).is_err());
        assert!(v(&["exec", "mail"]).unwrap_err().contains("mail"));
        assert_eq!(RewindState::default().triggers, ["exec"]);
        // A state file written before triggers existed reads back as exec-only.
        let old: RewindState = serde_json::from_value(
            json!({"enabled": true, "max_points": 5, "memory": true, "points": []}),
        )
        .unwrap();
        assert_eq!(old.triggers, ["exec"]);
    }

    #[test]
    fn rewind_state_defaults_are_off_and_survive_a_round_trip() {
        let st = RewindState::default();
        assert!(!st.enabled && st.memory && st.max_points == REWIND_DEFAULT_POINTS);
        let back: RewindState = serde_json::from_value(json!(st)).unwrap();
        assert_eq!(back, st);
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
                json!({"network": {"allow": [{"host": "a.b", "verbs": ["GET"]}]}})
            )))
            .is_err(),
            "an unknown allow key must be refused, not ignored"
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
        assert!(r.unenforced.is_empty());
        let e = r.exec.expect("filesystem compiles to a guest policy");
        assert!(e.write.contains(&"/tmp".to_string()));
        assert!(e.read.contains(&"/usr".to_string()));
        assert!(e.write.contains(&"/workspace".to_string()));
        assert!(e.best_effort && e.allow_udp);
        let rules = r.process.unwrap();
        assert_eq!(rules.allow, vec!["git"]);
        let r = compile_policy(Some(pol(json!({"process": {"ask": ["deploy *"]}})))).unwrap();
        assert_eq!(r.process.unwrap().ask, vec!["deploy *".to_string()]);
        assert!(compile_policy(Some(pol(json!({"process": {"nope": []}})))).is_err());
        assert!(compile_policy(Some(pol(json!({"process": {"enforce": "yes"}})))).is_err());
        assert!(compile_policy(Some(pol(json!({"process": {"deny": "rm"}})))).is_err());
    }

    #[test]
    fn filesystem_rules_are_validated_paths() {
        let fs = |v: Value| compile_policy(Some(pol(json!({"filesystem": v}))));
        let r = fs(
            json!({"read": ["/home/u/src/"], "write": ["/work/**"], "enforcement": "best-effort"}),
        )
        .unwrap();
        let e = r.exec.unwrap();
        assert!(e.read.contains(&"/home/u/src".to_string()));
        assert!(e.write.contains(&"/work".to_string()));
        assert_eq!(r.enforcement.as_deref(), Some("best-effort"));
        for bad in [
            json!(["rel/path"]),
            json!(["/a/../b"]),
            json!(["/a/*/b"]),
            json!([1]),
        ] {
            assert!(fs(json!({"write": bad})).is_err());
        }
        assert!(fs(json!({"nope": []})).is_err());
        assert!(fs(json!({"enforcement": "maybe"})).is_err());
        assert!(fs(json!("x")).is_err());
        assert!(compile_policy(None).unwrap().exec.is_none());
    }

    fn fixture_session() -> Session {
        Session {
            id: Uuid::new_v4(),
            agent: "t".into(),
            sandbox_id: Uuid::new_v4(),
            backend: "vz".into(),
            parent: None,
            children: Vec::new(),
            tenant: None,
            state: "running".into(),
            created_at: Utc::now(),
            expires_at: None,
            capabilities: capabilities_for("vz", false),
            policy: PolicyReport::default(),
            workspace: None,
            checkpoints: Vec::new(),
            volumes: Vec::new(),
            budget: None,
            revision: 0,
        }
    }

    #[test]
    fn a_budget_is_checked_before_it_is_spent_and_a_child_gets_what_is_left() {
        let b = Budget {
            max_execs: Some(3),
            max_exec_seconds: Some(10),
            max_jobs: Some(2),
            max_concurrent_jobs: Some(1),
            max_children: Some(4),
            ..Default::default()
        };
        let mut u = Usage::default();
        // Fresh: 10 s left for the first command.
        assert_eq!(b.check_exec(&u, 0).unwrap(), Some(10));
        u.execs = 2;
        u.exec_ms = 7_500;
        assert_eq!(
            b.check_exec(&u, 0).unwrap(),
            Some(2),
            "partial seconds round up as used"
        );
        u.execs = 3;
        assert_eq!(
            b.check_exec(&u, 0).unwrap_err().status,
            StatusCode::FORBIDDEN
        );
        u.execs = 1;
        u.exec_ms = 10_000;
        assert!(
            b.check_exec(&u, 0)
                .unwrap_err()
                .message
                .contains("exec time")
        );
        // Jobs: concurrency and total are separate limits.
        let mut u = Usage::default();
        assert!(b.check_job(&u, 0, 0).is_ok());
        assert!(
            b.check_job(&u, 1, 0)
                .unwrap_err()
                .message
                .contains("concurrent")
        );
        u.jobs = 2;
        assert!(b.check_job(&u, 0, 0).is_err());
        // Forks: the whole request must fit.
        let u = Usage {
            children: 3,
            ..Default::default()
        };
        assert!(b.check_fork(&u, 1).is_ok());
        assert!(b.check_fork(&u, 2).is_err());
        // No limits means no refusals.
        assert_eq!(Budget::default().check_exec(&u, 0).unwrap(), None);
        // A child starts with the remainder, never with zero (zero would read as "refused" and
        // zero is not a valid limit), and the concurrency cap is inherited as is.
        let used = Usage {
            execs: 3,
            exec_ms: 9_001,
            jobs: 1,
            children: 2,
            workspace_bytes: None,
        };
        let r = b.remaining(&used);
        assert_eq!(r.max_execs, Some(1));
        assert_eq!(r.max_exec_seconds, Some(1));
        assert_eq!(r.max_jobs, Some(1));
        assert_eq!(r.max_concurrent_jobs, Some(1));
        assert_eq!(r.max_children, Some(2));
    }

    #[test]
    fn budget_inputs_are_validated() {
        assert!(validate_budget(None).unwrap().is_none());
        assert!(validate_budget(Some(Budget::default())).unwrap().is_none());
        for bad in [
            Budget {
                max_execs: Some(0),
                ..Default::default()
            },
            Budget {
                max_jobs: Some(20_000_000),
                ..Default::default()
            },
            Budget {
                max_output_bytes: Some(0),
                ..Default::default()
            },
            Budget {
                max_workspace_bytes: Some(u64::MAX),
                ..Default::default()
            },
            Budget {
                max_compute_seconds: Some(0),
                ..Default::default()
            },
        ] {
            assert!(validate_budget(Some(bad)).is_err());
        }
        let ok = validate_budget(Some(Budget {
            max_children: Some(8),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(ok.unwrap().max_children, Some(8));
    }

    #[test]
    fn devtools_discovery_urls_point_back_at_the_preview() {
        assert!(is_cdp_discovery_path("json"));
        assert!(is_cdp_discovery_path("json/"));
        assert!(is_cdp_discovery_path("/json/list"));
        assert!(is_cdp_discovery_path("json/version"));
        assert!(!is_cdp_discovery_path("json/protocol"));
        assert!(!is_cdp_discovery_path("jsonx"));
        assert!(!is_cdp_discovery_path("devtools/page/1"));
        // What Chrome returns when reached through the sandbox proxy.
        let mut doc = json!([{
            "id": "ABC", "title": "t", "type": "page",
            "webSocketDebuggerUrl": "ws://192.168.64.5:9223/devtools/page/ABC",
            "devtoolsFrontendUrl": "/devtools/inspector.html?ws=192.168.64.5:9223/devtools/page/ABC",
        }]);
        rewrite_cdp_urls(&mut doc, "daemon.example:8080", "tok123", false);
        assert_eq!(
            doc[0]["webSocketDebuggerUrl"],
            "ws://daemon.example:8080/preview/tok123/devtools/page/ABC"
        );
        assert_eq!(
            doc[0]["devtoolsFrontendUrl"],
            "/preview/tok123/devtools/inspector.html?ws=daemon.example:8080/preview/tok123/devtools/page/ABC"
        );
        assert_eq!(doc[0]["title"], "t");
        // /json/version is an object, and behind TLS the scheme is wss.
        let mut v = json!({
            "Browser": "Chrome/155",
            "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/browser/XYZ",
        });
        rewrite_cdp_urls(&mut v, "h", "t", true);
        assert_eq!(
            v["webSocketDebuggerUrl"],
            "wss://h/preview/t/devtools/browser/XYZ"
        );
        assert_eq!(v["Browser"], "Chrome/155");
        // Anything else is left alone.
        let mut other = json!({"webSocketDebuggerUrl": 5, "x": ["ws://a/b"]});
        rewrite_cdp_urls(&mut other, "h", "t", false);
        assert_eq!(other, json!({"webSocketDebuggerUrl": 5, "x": ["ws://a/b"]}));
    }

    #[test]
    fn a_probe_report_says_what_each_feature_needs() {
        let r = probe_report(
            "git=/usr/bin/git\nsha256sum=/usr/bin/sha256sum\nchromium=/usr/bin/chromium\n\
             socat=/usr/bin/socat\nnode=/usr/bin/node\narch=aarch64\nkernel=6.1\njunk=1\n",
        );
        assert_eq!(r["tools"]["git"], "/usr/bin/git");
        assert!(r["tools"].get("junk").is_none());
        assert_eq!(r["system"]["arch"], "aarch64");
        let ready = &r["ready_for"];
        assert_eq!(ready["git"], true);
        assert_eq!(ready["promote"], true);
        assert_eq!(ready["browser_cdp"], true);
        assert_eq!(ready["browser_cdp_through_preview"], true);
        // node without npm is not a Node toolchain; no compiler means no C builds.
        assert_eq!(ready["build_node"], false);
        assert_eq!(ready["build_c"], false);
        assert_eq!(ready["playwright"], false);
        // The script asks for every tool the report knows and nothing else.
        let script = probe_script();
        for t in PROBE_TOOLS {
            assert!(script.contains(t), "{t}");
        }
        assert_eq!(probe_report("")["ready_for"]["promote"], false);
    }

    #[test]
    fn conflicting_lifecycle_operations_are_refused_not_interleaved() {
        let id = Uuid::new_v4();
        let ck = begin_lifecycle(id, "checkpoint").unwrap();
        // A second lifecycle op and any new work are refused while one runs.
        assert!(begin_lifecycle(id, "restore").is_err());
        assert!(begin_lifecycle(id, "delete").is_err());
        assert!(begin_work(id).is_err());
        drop(ck);
        // Work in flight blocks restore and fork, but not a checkpoint or a delete.
        let w = begin_work(id).unwrap();
        assert!(begin_lifecycle(id, "restore").is_err());
        assert!(begin_lifecycle(id, "fork").is_err());
        drop(begin_lifecycle(id, "checkpoint").unwrap());
        drop(begin_lifecycle(id, "delete").unwrap());
        drop(w);
        drop(begin_lifecycle(id, "restore").unwrap());
        // Another session is unaffected, and an idle gate leaves no entry behind.
        let other = Uuid::new_v4();
        let _held = begin_lifecycle(id, "fork").unwrap();
        assert!(begin_work(other).is_ok());
        drop(_held);
        assert!(!GATES.get().unwrap().lock().unwrap().contains_key(&id));
    }

    #[test]
    fn if_match_compares_the_revision() {
        use axum::http::{HeaderMap, HeaderValue, header::IF_MATCH};
        let mut s = fixture_session();
        s.revision = 4;
        let h = |v: &str| {
            let mut m = HeaderMap::new();
            m.insert(IF_MATCH, HeaderValue::from_str(v).unwrap());
            m
        };
        assert!(check_if_match(&HeaderMap::new(), &s).is_ok());
        assert!(check_if_match(&h("4"), &s).is_ok());
        assert!(check_if_match(&h("\"4\""), &s).is_ok());
        assert!(check_if_match(&h("*"), &s).is_ok());
        assert_eq!(
            check_if_match(&h("3"), &s).unwrap_err().status,
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(
            check_if_match(&h("x"), &s).unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn network_allow_entries_can_limit_methods_and_paths() {
        let r = compile_policy(Some(pol(json!({
            "network": {"allow": [
                "pypi.org",
                {"host": "github.com", "methods": ["get", "HEAD"], "paths": ["/org/*"]},
                {"host": "api.example.com"},
            ]}
        }))))
        .unwrap();
        assert_eq!(
            r.allow_hosts,
            vec!["api.example.com", "github.com", "pypi.org"]
        );
        // Unlimited hosts get a blanket rule (the proxy denies what no allow rule matches), the
        // limited one only its own.
        for want in [
            "allow * pypi.org",
            "allow * *.pypi.org",
            "allow * api.example.com",
            "allow GET github.com/org/*",
            "allow HEAD *.github.com/org/*",
        ] {
            assert!(
                r.http_rules.contains(&want.to_string()),
                "{want}: {:?}",
                r.http_rules
            );
        }
        assert!(!r.http_rules.iter().any(|x| x == "allow * github.com"));
        // Plain host lists compile no HTTP rules at all.
        let plain =
            compile_policy(Some(pol(json!({"network": {"allow": ["a.example"]}})))).unwrap();
        assert!(plain.http_rules.is_empty());
        // Bad entries are refused with a reason.
        for bad in [
            json!({"network": {"allow": [{"host": "x.io", "methods": ["G ET"]}]}}),
            json!({"network": {"allow": [{"host": "x.io", "paths": ["no-slash"]}]}}),
            json!({"network": {"allow": [{"host": "x.io", "paths": ["/a/../b"]}]}}),
            json!({"network": {"allow": [{"host": "x.io", "verbs": ["GET"]}]}}),
            json!({"network": {"allow": [{"host": "x.io", "methods": "GET"}]}}),
            json!({"network": {"allow": [{"methods": ["GET"]}]}}),
            json!({"network": {"enforcement": "maybe"}}),
        ] {
            assert!(compile_policy(Some(pol(bad.clone()))).is_err(), "{bad}");
        }
        let be = compile_policy(Some(pol(json!({"network": {
            "allow": [{"host": "x.io", "methods": ["GET"]}], "enforcement": "best-effort"}}))))
        .unwrap();
        assert_eq!(be.network_enforcement.as_deref(), Some("best-effort"));
    }

    #[test]
    fn output_is_cut_per_stream_on_a_character_boundary() {
        let mut out = json!({"exit_code": 0, "stdout": "héllo wörld", "stderr": "ok"});
        // 2 bytes land inside "é": the cut backs up to the boundary before it.
        assert!(cap_output(&mut out, 2));
        assert_eq!(out["stdout"], "h");
        assert_eq!(out["stdout_dropped_bytes"], 12);
        assert_eq!(out["stderr"], "ok");
        assert_eq!(out["output_truncated"], true);
        let mut small = json!({"stdout": "ab", "stderr": ""});
        assert!(!cap_output(&mut small, 2));
        assert!(small.get("output_truncated").is_none());
    }

    #[test]
    fn compute_budget_counts_exec_and_job_time() {
        let b = Budget {
            max_compute_seconds: Some(100),
            max_exec_seconds: Some(500),
            ..Default::default()
        };
        let u = Usage {
            exec_ms: 30_000,
            ..Default::default()
        };
        // 30 s of exec + 50 s of jobs: 20 s left, and that is the most one command may run.
        assert_eq!(b.check_exec(&u, 50_000).unwrap(), Some(20));
        assert!(b.check_exec(&u, 70_000).is_err());
        assert!(b.check_job(&u, 0, 70_000).is_err());
        assert!(b.check_job(&u, 0, 10_000).is_ok());
        // A child starts with what is left of the parent's compute.
        assert_eq!(b.remaining(&u).max_compute_seconds, Some(70));
        assert_eq!(Budget::default().check_exec(&u, 1).unwrap(), None);
    }

    #[test]
    fn event_ids_carry_both_stream_offsets() {
        assert_eq!(parse_event_offsets("120"), (120, 0));
        assert_eq!(parse_event_offsets(" 120:45 "), (120, 45));
        assert_eq!(parse_event_offsets("junk"), (0, 0));
    }

    #[test]
    fn signals_are_named_or_numbered() {
        assert_eq!(signal_number("TERM"), Some(15));
        assert_eq!(signal_number("sigkill"), Some(9));
        assert_eq!(signal_number("2"), Some(2));
        assert_eq!(signal_number("0"), None);
        assert_eq!(signal_number("65"), None);
        assert_eq!(signal_number("BOGUS"), None);
    }

    #[test]
    fn a_supervised_job_reports_how_it_ended() {
        let mut j = JobMeta {
            job: Uuid::nil(),
            command: "x".into(),
            stdin: false,
            max_output_bytes: 1,
            started_at: Utc::now(),
            killed: false,
            engine: ENGINE_SUPERVISOR.into(),
            phase: None,
            signal: None,
            dropped_bytes: 0,
            exit_code: None,
            finished_at: None,
        };
        assert_eq!(j.state(), "running");
        j.exit_code = Some(0);
        j.phase = Some("exited".into());
        assert_eq!(j.state(), "exited");
        j.phase = Some("timed_out".into());
        assert_eq!(j.state(), "timed_out");
        j.phase = Some("signaled".into());
        assert_eq!(j.state(), "signaled");
        // Metadata written before the supervisor existed has no engine or phase.
        let old: JobMeta = serde_json::from_value(json!({
            "job": Uuid::nil(), "command": "x", "stdin": false, "max_output_bytes": 1,
            "started_at": Utc::now(), "exit_code": 3
        }))
        .unwrap();
        assert_eq!(old.state(), "exited");
        assert!(old.engine.is_empty());
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
    fn ask_rules_match_any_piece_and_count_as_allowed() {
        let r = CommandRules {
            allow: vec!["ls".into()],
            deny: vec!["rm -rf /*".into()],
            ask: vec!["deploy *".into(), "git push*".into()],
        };
        assert_eq!(r.ask_match("ls"), None);
        assert_eq!(
            r.ask_match("ls && deploy prod").as_deref(),
            Some("deploy *")
        );
        assert_eq!(
            r.ask_match("'git' 'push' origin").as_deref(),
            Some("git push*")
        );
        // An ask pattern satisfies a non-empty allow list; deny still wins.
        assert!(r.check("deploy prod").is_ok());
        assert!(r.check("rm -rf /x").is_err());
        assert!(r.check("python3 x.py").is_err());
        assert_eq!(CommandRules::default().ask_match("deploy prod"), None);
    }

    #[test]
    fn command_rules_check_every_piece_of_a_compound_command() {
        let r = CommandRules {
            allow: vec![],
            deny: vec!["rm *".into(), "sudo*".into(), "curl *|*".into()],
            ask: vec![],
        };
        assert!(r.check("ls -la && cargo test").is_ok());
        assert!(r.check("echo hi; rm -rf /workspace").is_err());
        assert!(r.check("true || sudo reboot").is_err());
        assert!(r.check("cat x\nrm y").is_err());
        let a = CommandRules {
            allow: vec!["git *".into(), "ls".into(), "cargo *".into()],
            deny: vec!["git push*".into()],
            ask: vec![],
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
    fn a_macos_session_is_ssh_only_cannot_fork_and_refuses_what_it_cannot_enforce() {
        let c = macos_capabilities();
        assert!(
            c.snapshot && c.pty && !c.fork && !c.confinement && !c.http_policy && !c.supervisor
        );
        assert_eq!((c.engine.as_str(), c.guest_os.as_str()), ("ssh", "macos"));
        assert!(capabilities_for("vz", false).guest_os.is_empty());

        let mut s = fixture_session();
        s.capabilities = c;
        let why = fork_refusal(&s).expect("a macOS session cannot fork");
        assert!(
            why.contains("macOS") && why.contains("machine identifier"),
            "{why}"
        );

        let open = PolicyReport::default();
        assert!(macos_policy_refusal(&open, false).is_none());
        let offline = PolicyReport {
            offline: true,
            ..Default::default()
        };
        assert!(
            macos_policy_refusal(&offline, false)
                .unwrap()
                .contains("network.allow")
        );
        let allow = PolicyReport {
            allow_hosts: vec!["example.com".into()],
            ..Default::default()
        };
        assert!(macos_policy_refusal(&allow, false).is_some());
        let rules = PolicyReport {
            http_rules: vec!["allow GET example.com/*".into()],
            ..Default::default()
        };
        assert!(
            macos_policy_refusal(&rules, false)
                .unwrap()
                .contains("HTTP")
        );
        assert!(
            macos_policy_refusal(&open, true)
                .unwrap()
                .contains("volumes")
        );
    }

    #[test]
    fn a_macos_workspace_lives_under_the_guest_users_home() {
        assert_eq!(
            macos_default_workspace(Some("flux")),
            "/Users/flux/workspace"
        );
        assert_eq!(macos_default_workspace(None), "/Users/Shared/workspace");
        assert_eq!(
            macos_default_workspace(Some("../etc")),
            "/Users/Shared/workspace"
        );
        assert!(valid_guest_user("mac-template.1") && !valid_guest_user("a/b"));
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
        let look = PtyLook::from_query(&PtyQuery::default());
        let s = pty_script(
            120,
            40,
            Some("/workspace"),
            None,
            "/tmp/.fluxvm-pty-x",
            "/tmp/.fluxvm-rc-x",
            &look,
        );
        assert!(s.starts_with("stty cols 120 rows 40"));
        assert!(s.contains("cd '/workspace'"));
        assert!(s.contains("exec \"${SHELL:-/bin/sh}\" -l"));
        let c = pty_script(
            80,
            24,
            None,
            Some("echo 'a b'; ls"),
            "/tmp/.fluxvm-pty-x",
            "/tmp/.fluxvm-rc-x",
            &look,
        );
        assert!(c.contains("exec sh -c 'echo '\\''a b'\\''; ls'"), "{c}");
        assert!(!c.contains("cd "));
        assert!(s.contains("tty > '/tmp/.fluxvm-pty-x'"));
    }

    #[test]
    fn terminal_tickets_are_single_use_and_only_for_their_session() {
        let a = Uuid::new_v4();
        let t = mint_pty_ticket(a).unwrap();
        assert_eq!(t.len(), 64, "256 bits as hex");
        assert_eq!(take_pty_ticket("not-a-ticket"), None);
        assert_eq!(take_pty_ticket(&t), Some(a));
        assert_eq!(take_pty_ticket(&t), None, "a ticket works once");
        let u = mint_pty_ticket(a).unwrap();
        assert_ne!(t, u);
        // an expired ticket is gone: age it by editing its deadline
        {
            let mut m = pty_tickets().lock().unwrap();
            m.get_mut(&u).unwrap().1 =
                std::time::Instant::now() - std::time::Duration::from_secs(1);
        }
        assert_eq!(take_pty_ticket(&u), None);
    }

    #[test]
    fn the_terminal_look_follows_the_client_and_stays_safe() {
        let q = |term: Option<&str>, ct: Option<&str>, plain: bool| PtyQuery {
            term: term.map(String::from),
            colorterm: ct.map(String::from),
            plain,
            ..Default::default()
        };
        let d = PtyLook::from_query(&q(None, None, false));
        assert_eq!(d.term, "xterm-256color");
        assert!(!d.truecolor && !d.plain);
        let l = PtyLook::from_query(&q(Some("xterm-ghostty"), Some("truecolor"), false));
        assert_eq!(l.term, "xterm-ghostty");
        assert!(l.truecolor);
        assert!(PtyLook::from_query(&q(None, Some("24bit"), false)).truecolor);
        // anything that is not a plain terminfo name falls back; nothing reaches the shell
        for bad in ["a b", "x;rm -rf /", "$(id)", "", &"t".repeat(41)] {
            assert_eq!(
                PtyLook::from_query(&q(Some(bad), None, false)).term,
                "xterm-256color",
                "{bad}"
            );
        }
        assert!(!PtyLook::from_query(&q(None, Some("yes"), false)).truecolor);
        // the script: truecolor only when asked, rc file only for a shell and not when plain
        let on = pty_script(80, 24, None, None, "/t", "/rc", &l);
        assert!(on.contains("export COLORTERM=truecolor;"));
        assert!(on.contains("bash --rcfile '/rc' -i"));
        assert!(
            on.contains("infocmp \"$TERM\""),
            "falls back when the guest lacks the terminfo"
        );
        let plain = pty_script(
            80,
            24,
            None,
            None,
            "/t",
            "/rc",
            &PtyLook::from_query(&q(None, None, true)),
        );
        assert!(!plain.contains("--rcfile") && !plain.contains("COLORTERM"));
        let cmd = pty_script(80, 24, None, Some("ls"), "/t", "/rc", &l);
        assert!(
            !cmd.contains("--rcfile"),
            "a one-off command does not get the rc file"
        );
        assert!(PTY_RC.contains("--color=auto") && PTY_RC.contains("LESS_TERMCAP_md"));
        // prompt marks are only emitted for the FluxVM web terminal, never for another emulator
        assert!(PTY_RC.contains(r#""$TERM_PROGRAM" = FluxVM"#) && PTY_RC.contains("133;A"));
        // the client's terminal program is passed on only as a plain word
        let w = PtyLook::from_query(&PtyQuery {
            term_program: Some("WezTerm".into()),
            term_program_version: Some("20240203-110809-5046fc22".into()),
            ..Default::default()
        });
        let ws = pty_script(80, 24, None, None, "/t", "/rc", &w);
        assert!(ws.contains("export TERM_PROGRAM='WezTerm';"));
        assert!(ws.contains("export TERM_PROGRAM_VERSION='20240203-110809-5046fc22';"));
        let it = PtyLook::from_query(&PtyQuery {
            term_program: Some("iTerm.app".into()),
            ..Default::default()
        });
        assert!(pty_script(80, 24, None, None, "/t", "/rc", &it).contains("LC_TERMINAL=iTerm2"));
        let evil = PtyLook::from_query(&PtyQuery {
            term_program: Some("x; reboot".into()),
            ..Default::default()
        });
        assert_eq!(evil.program, None);
    }

    #[test]
    fn resize_messages_are_parsed_strictly() {
        assert_eq!(
            parse_resize(r#"{"resize":{"cols":132,"rows":43}}"#),
            Some((132, 43))
        );
        for bad in [
            "ls -l",
            r#"{"resize":{"cols":0,"rows":10}}"#,
            r#"{"resize":{"cols":501,"rows":10}}"#,
            r#"{"resize":{"cols":80}}"#,
            r#"{"resize":{"cols":-1,"rows":5}}"#,
            r#"{"other":1}"#,
        ] {
            assert_eq!(parse_resize(bad), None, "{bad}");
        }
        let cmd = pty_resize_script("/tmp/.fluxvm-pty-x", 100, 30);
        assert!(cmd.contains("cat '/tmp/.fluxvm-pty-x'") && cmd.contains("cols 100 rows 30"));
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
        let start = job_start_script(job, "echo 'hi there'; sleep 1", false, 1024);
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

    /// Runs the real guest scripts with a local `sh`. `setsid` is absent on macOS, so a shim that
    /// just execs its arguments stands in for it; the scripts are otherwise unchanged.
    #[cfg(unix)]
    mod live_scripts {
        use super::super::*;
        use std::process::Command;
        use std::time::{Duration, Instant};

        struct Sh {
            path: std::ffi::OsString,
            /// Private jobs directory: the production path (`/tmp/fluxvm-jobs`) is shared and
            /// fixed, so another user's leftover there made these tests fail.
            jobs: String,
            _dir: tempfile::TempDir,
        }

        fn sh() -> Sh {
            let dir = tempfile::tempdir().unwrap();
            let shim = dir.path().join("setsid");
            std::fs::write(&shim, "#!/bin/sh\nexec \"$@\"\n").unwrap();
            std::os::unix::fs::PermissionsExt::set_mode(
                &mut std::fs::metadata(&shim).unwrap().permissions(),
                0o755,
            );
            let mut p = std::fs::metadata(&shim).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o755);
            std::fs::set_permissions(&shim, p).unwrap();
            let mut path = std::ffi::OsString::from(dir.path());
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            let jobs = dir.path().join("jobs").to_string_lossy().into_owned();
            Sh {
                path,
                jobs,
                _dir: dir,
            }
        }

        fn run(sh: &Sh, script: &str) -> (i32, String) {
            let o = Command::new("sh")
                .arg("-c")
                .arg(script)
                .env("PATH", &sh.path)
                .output()
                .unwrap();
            (
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stdout).to_string(),
            )
        }

        /// Polls `job_read_script` until the job has exited (or the deadline).
        fn wait_exit(sh: &Sh, job: Uuid) -> (Option<i64>, Vec<u8>, u64) {
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                let (_, out) = run(sh, &job_read_script_in(&sh.jobs, job, 0));
                if let JobRead::Chunk { exit, bytes, size } = parse_job_read(&out)
                    && (exit.is_some() || Instant::now() > end)
                {
                    return (exit, bytes, size);
                }
                assert!(
                    Instant::now() < end + Duration::from_secs(1),
                    "job never appeared"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }

        #[test]
        fn a_job_records_output_and_its_exit_code() {
            let sh = sh();
            let job = Uuid::new_v4();
            let (code, out) = run(
                &sh,
                &job_start_script_in(
                    &sh.jobs,
                    job,
                    "echo out; echo err >&2; exit 7",
                    false,
                    1 << 20,
                ),
            );
            assert_eq!((code, out.trim()), (0, "started"));
            let (exit, bytes, _) = wait_exit(&sh, job);
            assert_eq!(exit, Some(7));
            assert_eq!(String::from_utf8_lossy(&bytes), "out\nerr\n");
        }

        #[test]
        fn stdin_reaches_the_job_and_a_dead_job_refuses_it() {
            let sh = sh();
            let job = Uuid::new_v4();
            // `head -n 2` reads two lines from stdin, so the job lives until both arrive.
            run(
                &sh,
                &job_start_script_in(&sh.jobs, job, "head -n 2", true, 1 << 20),
            );
            let end = Instant::now() + Duration::from_secs(5);
            // The FIFO is created before the job is backgrounded, so the first write works.
            let (c1, _) = run(&sh, &job_stdin_script_in(&sh.jobs, job, b"one\n"));
            assert_eq!(c1, 0);
            let (c2, _) = run(&sh, &job_stdin_script_in(&sh.jobs, job, b"two\n"));
            assert_eq!(c2, 0);
            let (exit, bytes, _) = wait_exit(&sh, job);
            assert!(Instant::now() < end + Duration::from_secs(10));
            assert_eq!(exit, Some(0));
            assert_eq!(String::from_utf8_lossy(&bytes), "one\ntwo\n");
            // Exited: a further write is refused, not blocked forever.
            let (c3, _) = run(&sh, &job_stdin_script_in(&sh.jobs, job, b"three\n"));
            assert_eq!(c3, 3);
            // A job without stdin has no FIFO at all.
            let plain = Uuid::new_v4();
            run(
                &sh,
                &job_start_script_in(&sh.jobs, plain, "true", false, 1024),
            );
            assert_eq!(run(&sh, &job_stdin_script_in(&sh.jobs, plain, b"x")).0, 3);
        }

        #[test]
        fn the_output_log_is_bounded_and_the_job_still_reports_an_exit() {
            let sh = sh();
            let job = Uuid::new_v4();
            run(
                &sh,
                &job_start_script_in(
                    &sh.jobs,
                    job,
                    "yes aaaaaaaaaaaaaaaaaaaa | head -c 200000",
                    false,
                    1000,
                ),
            );
            let (exit, bytes, size) = wait_exit(&sh, job);
            assert!(
                exit.is_some(),
                "an exit is recorded even when output was cut"
            );
            assert_eq!(size, 1000);
            assert_eq!(bytes.len(), 1000);
        }
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
