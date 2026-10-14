// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl agent`: AgentSession from the command line. REST: `/v1/agents/sessions/*`.
//! Always talks to the daemon (`--server`, the current context, or the local `listen`).

use crate::remote::Remote;
use anyhow::{Context, Result};
use clap::Subcommand;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Start a session: a sandbox with a workspace and a network policy. Prints the session record.
    /// REST: `POST /v1/agents/sessions`.
    Run {
        /// Which agent this is for, for the record (`claude-code`, `codex`, ...).
        #[arg(default_value = "custom")]
        agent: String,
        /// https git URL cloned into the workspace (the policy must allow its host).
        #[arg(long)]
        repo: Option<String>,
        /// Guest path of the workspace.
        #[arg(long)]
        workspace_path: Option<String>,
        /// Fail the create (and remove the sandbox) unless the workspace directory exists and the
        /// `--repo` clone succeeded; by default the session is made and `workspace.note` says why not.
        #[arg(long)]
        require_ready: bool,
        /// Guest image, for example `agent-micro`.
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        cpus: Option<u8>,
        #[arg(long)]
        memory_mb: Option<u64>,
        /// `offline`, `developer-safe` or `open`.
        #[arg(long)]
        policy: Option<String>,
        /// Host the session may reach (repeatable); everything else is refused.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Attach a named volume (`fluxctl`/REST `POST /v1/volumes`) as `NAME:/guest/path`
        /// (repeatable, vz only). Formatted ext4 on first use. Such a session cannot fork.
        #[arg(long = "volume")]
        volume: Vec<String>,
        /// Command pattern the session may run (repeatable, `*` wildcard); with any given, other
        /// commands are refused. A guardrail on the command string, not a security boundary.
        #[arg(long = "cmd-allow")]
        cmd_allow: Vec<String>,
        /// Command pattern the session may not run (repeatable, `*` wildcard).
        #[arg(long = "cmd-deny")]
        cmd_deny: Vec<String>,
        /// Confine every command, job, file and git call in the guest (Landlock + seccomp):
        /// system directories read-only, `/tmp` and the workspace writable. Create fails when the
        /// guest cannot enforce it, unless `--confine-best-effort`. Linux guests only.
        #[arg(long)]
        confine: bool,
        /// Extra path the session may read when confined (repeatable, absolute; implies `--confine`).
        #[arg(long = "fs-read")]
        fs_read: Vec<String>,
        /// Extra path the session may write when confined (repeatable, absolute; implies `--confine`).
        #[arg(long = "fs-write")]
        fs_write: Vec<String>,
        /// Create the session even if the guest cannot enforce the confinement (implies `--confine`;
        /// `capabilities.confinement` then says whether it did).
        #[arg(long)]
        confine_best_effort: bool,
        /// Budget: most commands (`exec`) the session may run.
        #[arg(long)]
        max_execs: Option<u64>,
        /// Budget: most wall-clock seconds spent in commands, summed.
        #[arg(long)]
        max_exec_seconds: Option<u64>,
        /// Budget: most background jobs started.
        #[arg(long)]
        max_jobs: Option<u64>,
        /// Budget: most background jobs running at once.
        #[arg(long)]
        max_concurrent_jobs: Option<u64>,
        /// Budget: most fork children created. Children start with what is left of the others.
        #[arg(long)]
        max_children: Option<u64>,
        /// Budget: seconds of command plus background-job wall time.
        #[arg(long)]
        max_compute_seconds: Option<u64>,
        /// Budget: bytes of output kept per stream (commands return at most this; jobs retain it).
        #[arg(long)]
        max_output_bytes: Option<u64>,
        /// Budget: workspace size in bytes, measured when a command or job starts.
        #[arg(long)]
        max_workspace_bytes: Option<u64>,
        #[arg(long)]
        ttl_seconds: Option<u64>,
    },
    /// List sessions. REST: `GET /v1/agents/sessions`.
    Ls,
    /// One session with its capabilities, policy and checkpoints. REST: `GET /v1/agents/sessions/{id}`.
    Get { id: Uuid },
    /// Run a command in the session. REST: `POST /v1/agents/sessions/{id}/exec`.
    Exec {
        id: Uuid,
        #[arg(long)]
        timeout_seconds: Option<u64>,
        /// An approval an operator granted for exactly this command (see `agent approvals`).
        #[arg(long)]
        approval_id: Option<Uuid>,
        /// If the command needs approval, wait up to this many seconds (at most 120) for the decision.
        #[arg(long)]
        approval_wait: Option<u64>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
    /// The session's approval requests (commands that matched a `process.ask` rule) and their state.
    /// REST: `GET .../approvals`.
    Approvals { id: Uuid },
    /// Approve a pending request: the agent may then run exactly that command once. Needs an admin
    /// credential; a session token cannot. REST: `POST .../approvals/{approval}/approve`.
    Approve {
        id: Uuid,
        approval: Uuid,
        #[arg(long)]
        note: Option<String>,
    },
    /// Deny a pending request. REST: `POST .../approvals/{approval}/deny`.
    Deny {
        id: Uuid,
        approval: Uuid,
        #[arg(long)]
        note: Option<String>,
    },
    /// Snapshot the session under a tag. REST: `POST /v1/agents/sessions/{id}/checkpoint`.
    Checkpoint {
        id: Uuid,
        #[arg(long)]
        tag: Option<String>,
        /// Disk-only snapshot: far smaller, no memory state.
        #[arg(long)]
        disk_only: bool,
    },
    /// Restore a checkpoint. REST: `POST /v1/agents/sessions/{id}/restore`.
    Restore { id: Uuid, tag: String },
    /// Fork into COUNT child sessions (in-tree KVM, or a vz session with no NIC: policy offline or an
    /// allow-list; refused otherwise with the reason).
    /// REST: `POST /v1/agents/sessions/{id}/fork`.
    Fork {
        id: Uuid,
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// How hard to make each child a different machine: best-effort (default), unique, preserve.
        #[arg(long)]
        identity: Option<String>,
    },
    /// What the session did: the ledger plus the sandbox's VM events. REST: `GET .../events`.
    Timeline {
        id: Uuid,
        /// RFC 3339 time; only newer entries.
        #[arg(long)]
        since: Option<String>,
    },
    /// Write the session's trajectory (ledger + VM events, ordered) as JSON Lines: one header
    /// line, then one line per step. REST: `GET .../trajectory`.
    Export {
        id: Uuid,
        /// File to write; stdout when omitted (and no `--otlp`).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Also send the trajectory as OpenTelemetry traces (OTLP/HTTP JSON) to this collector,
        /// e.g. `http://localhost:4318`; `/v1/traces` is appended unless already there. With
        /// `--otlp` and no `--out`, nothing is printed.
        #[arg(long)]
        otlp: Option<String>,
        /// Header sent to the collector, `KEY=VALUE` (repeatable), e.g. an auth token.
        #[arg(long = "otlp-header")]
        otlp_header: Vec<String>,
    },
    /// Workspace changes since the baseline, checkpoints and children. REST: `GET .../artifacts`.
    Artifacts { id: Uuid },
    /// Git in the session's workspace repo (needs git in the guest). REST: `.../git/*`.
    Git {
        id: Uuid,
        #[command(subcommand)]
        op: GitOp,
    },
    /// List a directory in the guest (or stat a file). REST: `GET .../files?path=`.
    Files { id: Uuid, path: String },
    /// Print a guest file to stdout. REST: `GET .../files/content?path=`.
    Cat { id: Uuid, path: String },
    /// Copy a local file into the guest (parents are created). REST: `PUT .../files/content`.
    Put {
        id: Uuid,
        local: std::path::PathBuf,
        remote: String,
        /// File mode in octal, e.g. 755.
        #[arg(long)]
        mode: Option<String>,
    },
    /// Copy a guest file to a local path (stdout when omitted).
    Pull {
        id: Uuid,
        remote: String,
        local: Option<std::path::PathBuf>,
    },
    /// Remove a guest file; `-r` for a directory. REST: `DELETE .../files?path=`.
    RmFile {
        id: Uuid,
        path: String,
        #[arg(short = 'r', long)]
        recursive: bool,
    },
    /// Start a command that keeps running after this call returns; prints the job id. Read it with
    /// `agent logs`. No TTY; stdin only with `--stdin`. REST: `POST .../jobs`.
    Bg {
        id: Uuid,
        /// Give the job a stdin that `agent stdin` writes to (`agent stdin --close` sends EOF).
        #[arg(long)]
        stdin: bool,
        /// Stop recording output after this many bytes (default 16 MiB, at most 256 MiB).
        #[arg(long)]
        max_output_bytes: Option<u64>,
        /// Kill the job's whole process tree after this many seconds (needs the guest job supervisor).
        #[arg(long)]
        timeout: Option<u64>,
        /// An approval an operator granted for exactly this command (see `agent approvals`).
        #[arg(long)]
        approval_id: Option<Uuid>,
        /// If the command needs approval, wait up to this many seconds (at most 120) for the decision.
        #[arg(long)]
        approval_wait: Option<u64>,
        /// The command: one shell line, or an argv after `--`.
        #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// The session's background jobs: command, state and exit code. REST: `GET .../jobs`.
    Jobs { id: Uuid },
    /// Write to the stdin of a job started with `bg --stdin`. REST: `POST .../jobs/{job}/stdin`.
    Stdin {
        id: Uuid,
        job: Uuid,
        /// Text to write (a newline is not added). May be omitted with `--close`.
        text: Option<String>,
        /// Close the job's stdin after writing (EOF). Needs the guest job supervisor.
        #[arg(long)]
        close: bool,
    },
    /// Output of a background job. With `-f` keep reading until it exits and exit with its code.
    /// REST: `GET .../jobs/{job}?offset=N`.
    Logs {
        id: Uuid,
        job: Uuid,
        /// Follow until the job exits. Supervised jobs long-poll; shell-script jobs poll twice a second.
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Stop a background job (SIGTERM to its whole process tree by default). REST: `DELETE .../jobs/{job}`.
    Kill {
        id: Uuid,
        job: Uuid,
        /// Signal name or number (supervised jobs only), e.g. KILL.
        #[arg(long)]
        signal: Option<String>,
    },
    /// Mint an expiring preview URL for a guest port: whoever holds the URL reaches that port and
    /// nothing else. REST: `POST .../preview`.
    Expose {
        id: Uuid,
        /// The guest TCP port a server listens on.
        port: u16,
        /// Seconds until the URL stops working (1-86400).
        #[arg(long, default_value_t = 3600)]
        ttl: u64,
    },
    /// Live preview URLs of a session. REST: `GET .../previews`.
    Previews { id: Uuid },
    /// Revoke a preview URL by its token. REST: `DELETE .../preview/{token}`.
    Unexpose { id: Uuid, token: String },
    /// Open an interactive shell in the session on a real terminal (WebSocket, `ssh -tt` in the
    /// daemon). Starts at this terminal's size; a later resize is not forwarded. Refused for a
    /// session with `--cmd-allow/--cmd-deny` rules. REST: `GET .../pty` (WebSocket).
    Shell {
        id: Uuid,
        /// Open the browser terminal for this session (tabs, themes, images) instead of using
        /// this terminal. Prints the URL and opens it on macOS; sign in with your token there.
        #[arg(long)]
        web: bool,
        /// Start the login shell as it is, without the colour aliases and coloured man pages.
        #[arg(long)]
        plain: bool,
        /// Run this on the terminal instead of a login shell.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Print an OpenSSH config block for the session's guest, so any ssh client works with it:
    /// `ssh`, `scp`, `rsync`, port forwards, and WezTerm's panes and tabs (`wezterm ssh HOST`).
    /// Only valid on the machine running the daemon (the key is that host's). REST: `GET .../ssh`.
    SshConfig {
        id: Uuid,
        /// Host alias; `fluxvm-<first 8 of the session id>` when left out.
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete the session and its sandbox. REST: `DELETE /v1/agents/sessions/{id}`.
    Rm { id: Uuid },
    /// Try several candidate fixes in isolated forks, test each, rank them and keep the winner:
    /// forks one child per `--candidate`, runs the candidate and then `--test` in it, scores every
    /// result, deletes the losers and prints the winner. Review and apply it with
    /// `agent promote <winner> --dry-run`. Same forkability rules as `fork`.
    Evaluate {
        id: Uuid,
        /// A candidate command (repeatable, 1-32): what to try in one child.
        #[arg(long = "candidate", required = true)]
        candidates: Vec<String>,
        /// Command that decides whether a candidate passes (exit 0). Without it a candidate passes
        /// when it exits 0.
        #[arg(long)]
        test: Option<String>,
        /// Per-command timeout in seconds.
        #[arg(long)]
        timeout_seconds: Option<u64>,
        /// Keep the losing children too (their ids are in the output).
        #[arg(long)]
        keep_all: bool,
        #[arg(long)]
        identity: Option<String>,
    },
    /// Which tools the guest image has (git, sha256sum for promote, node, a browser, playwright ...)
    /// and which session features that makes possible. REST: `GET /v1/agents/sessions/{id}/probe`.
    Probe { id: Uuid },
    /// Copy a local file into the guest in verified, resumable chunks: parts are written in the guest,
    /// then assembled, size- and SHA-256-checked, and renamed into place. Rerun with `--resume <upload>`
    /// after an interruption. REST: `POST .../uploads`, `PUT .../uploads/{upload}`, `POST .../complete`.
    Upload {
        id: Uuid,
        /// Local file.
        local: std::path::PathBuf,
        /// Absolute path in the guest.
        remote: String,
        /// Permission bits, octal (for example 755).
        #[arg(long, value_parser = parse_octal)]
        mode: Option<u32>,
        /// Continue an upload that was interrupted (its id was printed when it opened).
        #[arg(long)]
        resume: Option<Uuid>,
        /// Chunk size in MiB (1-4).
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u64).range(1..=4))]
        chunk_mib: u64,
    },
    /// Copy a guest file to a local path in ranged reads, verified against the guest's SHA-256. Writes
    /// `<local>.part` and renames it when complete; `--resume` continues a `.part`. REST:
    /// `GET .../files/range`.
    Download {
        id: Uuid,
        /// Absolute path in the guest.
        remote: String,
        /// Local destination.
        local: std::path::PathBuf,
        /// Continue from the existing `<local>.part`.
        #[arg(long)]
        resume: bool,
    },
    /// Mint an expiring token that works on this one session and only for the listed operations
    /// (read, exec, jobs, files.read, files.write, git, checkpoint, preview, screen). Prints it once; use it as
    /// the bearer token for `fluxctl --token` or REST. REST: `POST /v1/agents/sessions/{id}/tokens`.
    Token {
        id: Uuid,
        /// Operations to grant (comma separated or repeated).
        #[arg(long, value_delimiter = ',', required = true)]
        ops: Vec<String>,
        /// Lifetime in seconds (default 3600, at most 86400; never past the session's own expiry).
        #[arg(long)]
        ttl_seconds: Option<u64>,
        #[arg(long)]
        label: Option<String>,
    },
    /// Give the session a brokered credential: the egress proxy injects it as an `Authorization`
    /// header for the named hosts, so the agent never holds the secret. It ends with the session
    /// and is not inherited by fork children. REST: `POST /v1/agents/sessions/{id}/grants`.
    Grant {
        id: Uuid,
        /// Name of the secret; without `--value` it is read from the daemon's `FLUXVM_SECRET_<REF>`.
        #[arg(long)]
        secret_ref: String,
        /// Destination host (repeatable; exact or domain suffix) the secret may be sent to.
        #[arg(long = "host", required = true)]
        hosts: Vec<String>,
        /// The full header value, for example `Bearer abc`. Prefer a daemon-side secret_ref.
        #[arg(long, env = "FLUXVM_GRANT_VALUE", hide_env_values = true)]
        value: Option<String>,
        #[arg(long)]
        ttl_seconds: Option<i64>,
        /// Send the secret only with this HTTP method (repeatable), for example `--method GET`.
        #[arg(long = "method")]
        methods: Vec<String>,
        /// Send the secret only to this path (repeatable; `*` matches any run), for example
        /// `--path '/org/repo.git/*'`.
        #[arg(long = "path")]
        paths: Vec<String>,
    },
    /// List the session's live grants (never the secrets). REST: `GET .../grants`.
    Grants { id: Uuid },
    /// Revoke one grant, or every grant of the session with no id. REST: `DELETE .../grants[/{grant}]`.
    Ungrant { id: Uuid, grant: Option<String> },
    /// List a session's live tokens (never the secrets). REST: `GET .../tokens`.
    Tokens { id: Uuid },
    /// Revoke one session token by its id. REST: `DELETE .../tokens/{token}`.
    Untoken { id: Uuid, token: Uuid },
    /// Apply a fork child's workspace changes to its parent as a three-way merge. `--dry-run`
    /// prints the plan and its `plan_id`; apply with `--plan-id` to pin what you reviewed. The
    /// parent is checkpointed first and conflicting paths are never overwritten.
    /// REST: `POST /v1/agents/sessions/{id}/promote`.
    Promote {
        /// The child session.
        id: Uuid,
        #[arg(long)]
        dry_run: bool,
        /// The `plan_id` from a dry run; the apply is refused if the plan differs now.
        #[arg(long)]
        plan_id: Option<String>,
        /// Apply the paths that merge cleanly and skip conflicting ones.
        #[arg(long)]
        skip_conflicts: bool,
        /// Only this file or directory inside the workspace (repeatable).
        #[arg(long = "path")]
        paths: Vec<String>,
    },
    /// Fork COUNT children, run the same command in each in parallel, print every result, then
    /// delete the children (unless `--keep`). Composed from fork, exec and delete; each child is
    /// a full sandbox, so COUNT is bounded by the host. Same forkability rules as `fork`.
    Rollout {
        id: Uuid,
        #[arg(long, default_value_t = 2)]
        count: u32,
        /// Per-command timeout in seconds.
        #[arg(long)]
        timeout_seconds: Option<u64>,
        /// Keep the children (their ids are in the output) instead of deleting them.
        #[arg(long)]
        keep: bool,
        #[arg(long)]
        identity: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum GitOp {
    /// Branch and changed paths.
    Status {
        /// Repository directory in the guest (default: the session workspace).
        #[arg(long)]
        repo: Option<String>,
    },
    /// The working-tree diff (or `--staged`).
    Diff {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        staged: bool,
        /// Limit to one path, relative to the repository.
        path: Option<String>,
    },
    /// Recent commits.
    Log {
        #[arg(long)]
        repo: Option<String>,
        #[arg(short = 'n', long)]
        limit: Option<u32>,
    },
    /// Stage everything (unless `--staged-only`) and commit.
    Commit {
        message: String,
        #[arg(long)]
        repo: Option<String>,
        /// Commit only what is already staged.
        #[arg(long)]
        staged_only: bool,
    },
    /// Create a branch and switch to it.
    Branch {
        name: String,
        #[arg(long)]
        repo: Option<String>,
    },
}

fn git_query(repo: &Option<String>, extra: &[(&str, String)]) -> String {
    let mut parts = Vec::new();
    if let Some(r) = repo {
        parts.push(format!("repo={}", crate::remote::encode_query(r)));
    }
    for (k, v) in extra {
        parts.push(format!("{k}={}", crate::remote::encode_query(v)));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

fn show(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Fork, run `command` in every child at once, collect the results, delete the children unless
/// `keep`. A child that fails to exec is reported in its own entry; it never aborts the others.
pub(crate) async fn rollout(
    r: &Remote,
    id: Uuid,
    count: u32,
    command: &str,
    timeout_seconds: Option<u64>,
    keep: bool,
    identity: Option<&str>,
) -> Result<Value> {
    let mut fork_body = json!({"count": count});
    if let Some(i) = identity {
        fork_body["identity"] = json!(i);
    }
    let forked = r
        .call(
            Method::POST,
            &format!("/v1/agents/sessions/{id}/fork"),
            Some(fork_body),
        )
        .await
        .context("fork")?;
    let kids: Vec<String> = forked["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect();
    let started = std::time::Instant::now();
    let runs = kids.iter().map(|cid| async move {
        let mut body = json!({"command": command});
        if let Some(t) = timeout_seconds {
            body["timeout_seconds"] = json!(t);
        }
        let t0 = std::time::Instant::now();
        match r
            .call(
                Method::POST,
                &format!("/v1/agents/sessions/{cid}/exec"),
                Some(body),
            )
            .await
        {
            Ok(v) => json!({
                "session": cid,
                "exit_code": v["exit_code"],
                "stdout": v["stdout"],
                "stderr": v["stderr"],
                "duration_ms": t0.elapsed().as_millis() as u64,
            }),
            Err(e) => json!({"session": cid, "error": format!("{e:#}")}),
        }
    });
    let results = futures_util::future::join_all(runs).await;
    let exec_ms = started.elapsed().as_millis() as u64;
    let mut deleted = 0;
    if !keep {
        let dels = kids.iter().map(|cid| async move {
            r.call(Method::DELETE, &format!("/v1/agents/sessions/{cid}"), None)
                .await
                .is_ok()
        });
        deleted = futures_util::future::join_all(dels)
            .await
            .into_iter()
            .filter(|ok| *ok)
            .count();
    }
    Ok(json!({
        "parent": id,
        "fork_ms": forked["total_ms"],
        "exec_wall_ms": exec_ms,
        "results": results,
        "children_kept": keep,
        "children_deleted": deleted,
    }))
}

/// One candidate's outcome, as far as ranking is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub candidate_exit: Option<i64>,
    pub test_exit: Option<i64>,
    pub had_test: bool,
    pub duration_ms: u64,
}

impl Outcome {
    /// Passed: the test exited 0 (or, with no test, the candidate did).
    pub(crate) fn passed(&self) -> bool {
        if self.had_test {
            self.candidate_exit == Some(0) && self.test_exit == Some(0)
        } else {
            self.candidate_exit == Some(0)
        }
    }

    /// 100 passed; 10 ran cleanly but failed its test; 0 failed or never ran.
    pub(crate) fn score(&self) -> u32 {
        if self.passed() {
            100
        } else if self.candidate_exit == Some(0) {
            10
        } else {
            0
        }
    }
}

/// Index of the best outcome: highest score, then shortest, then lowest index. `None` when
/// nothing passed, because a winner that fails its test must not be offered for promotion.
pub(crate) fn pick_winner(outcomes: &[Outcome]) -> Option<usize> {
    outcomes
        .iter()
        .enumerate()
        .filter(|(_, o)| o.passed())
        .min_by_key(|(i, o)| (std::cmp::Reverse(o.score()), o.duration_ms, *i))
        .map(|(i, _)| i)
}

fn tail(v: &Value, n: usize) -> String {
    let s = v.as_str().unwrap_or("");
    let skip = s.chars().count().saturating_sub(n);
    s.chars().skip(skip).collect()
}

/// Fork one child per candidate, run candidate then test in each, rank, keep the winner.
/// A child that errors is a result with `error`, never an abort of the others.
pub(crate) async fn evaluate(
    r: &Remote,
    id: Uuid,
    candidates: &[String],
    test: Option<&str>,
    timeout_seconds: Option<u64>,
    keep_all: bool,
    identity: Option<&str>,
) -> Result<Value> {
    if candidates.is_empty() || candidates.len() > 32 {
        anyhow::bail!("give 1-32 candidates");
    }
    if candidates.iter().any(|c| c.trim().is_empty()) {
        anyhow::bail!("a candidate command is empty");
    }
    let mut fork_body = json!({"count": candidates.len()});
    if let Some(i) = identity {
        fork_body["identity"] = json!(i);
    }
    let forked = r
        .call(
            Method::POST,
            &format!("/v1/agents/sessions/{id}/fork"),
            Some(fork_body),
        )
        .await
        .context("fork")?;
    let kids: Vec<String> = forked["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect();
    if kids.len() != candidates.len() {
        // Do not leave a half-made fork behind.
        for cid in &kids {
            let _ = r
                .call(Method::DELETE, &format!("/v1/agents/sessions/{cid}"), None)
                .await;
        }
        anyhow::bail!(
            "asked for {} children, got {}; they were deleted",
            candidates.len(),
            kids.len()
        );
    }
    let exec = |cid: &str, cmd: &str| {
        let mut body = json!({"command": cmd});
        if let Some(t) = timeout_seconds {
            body["timeout_seconds"] = json!(t);
        }
        let path = format!("/v1/agents/sessions/{cid}/exec");
        async move { r.call(Method::POST, &path, Some(body)).await }
    };
    let runs = kids.iter().zip(candidates).map(|(cid, cmd)| {
        let exec = &exec;
        async move {
            let t0 = std::time::Instant::now();
            let cand = match exec(cid, cmd).await {
                Ok(v) => v,
                Err(e) => {
                    return (
                        Outcome {
                            candidate_exit: None,
                            test_exit: None,
                            had_test: test.is_some(),
                            duration_ms: t0.elapsed().as_millis() as u64,
                        },
                        json!({"session": cid, "command": cmd, "error": format!("{e:#}")}),
                    );
                }
            };
            let cand_exit = cand["exit_code"].as_i64();
            let mut test_exit = None;
            let mut test_v = Value::Null;
            // A candidate that did not exit cleanly is not worth testing.
            if let (Some(t), Some(0)) = (test, cand_exit) {
                match exec(cid, t).await {
                    Ok(v) => {
                        test_exit = v["exit_code"].as_i64();
                        test_v = v;
                    }
                    Err(e) => test_v = json!({"error": format!("{e:#}")}),
                }
            }
            let o = Outcome {
                candidate_exit: cand_exit,
                test_exit,
                had_test: test.is_some(),
                duration_ms: t0.elapsed().as_millis() as u64,
            };
            let detail = json!({
                "session": cid,
                "command": cmd,
                "candidate_exit": cand_exit,
                "test_exit": test_exit,
                "passed": o.passed(),
                "score": o.score(),
                "duration_ms": o.duration_ms,
                "stdout_tail": tail(&cand["stdout"], 2000),
                "stderr_tail": tail(&cand["stderr"], 2000),
                "test_stdout_tail": tail(&test_v["stdout"], 2000),
                "test_stderr_tail": tail(&test_v["stderr"], 2000),
            });
            (o, detail)
        }
    });
    let done = futures_util::future::join_all(runs).await;
    let outcomes: Vec<Outcome> = done.iter().map(|(o, _)| o.clone()).collect();
    let winner = pick_winner(&outcomes);
    let mut results: Vec<Value> = Vec::new();
    for (i, (_, mut d)) in done.into_iter().enumerate() {
        d["index"] = json!(i);
        results.push(d);
    }
    let mut deleted = 0;
    for (i, cid) in kids.iter().enumerate() {
        if keep_all || Some(i) == winner {
            continue;
        }
        if r.call(Method::DELETE, &format!("/v1/agents/sessions/{cid}"), None)
            .await
            .is_ok()
        {
            deleted += 1;
        }
    }
    let winner_session = winner.map(|i| kids[i].clone());
    Ok(json!({
        "parent": id,
        "fork_ms": forked["total_ms"],
        "results": results,
        "winner": winner_session,
        "winner_index": winner,
        "children_deleted": deleted,
        "next": winner_session.as_ref().map(|w| format!("fluxctl agent promote {w} --dry-run")),
    }))
}

async fn decide_approval(
    r: &Remote,
    id: Uuid,
    approval: Uuid,
    verb: &str,
    note: Option<String>,
) -> Result<()> {
    let body = note.map(|n| json!({"note": n}));
    show(
        &r.call(
            Method::POST,
            &format!("/v1/agents/sessions/{id}/approvals/{approval}/{verb}"),
            body,
        )
        .await?,
    )
}

fn parse_octal(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0o"), 8)
        .ok()
        .filter(|m| *m <= 0o7777)
        .ok_or_else(|| format!("{s:?} is not an octal permission mask"))
}

/// The offset a `409` from a chunk upload says to continue at.
fn resume_offset(message: &str) -> Option<u64> {
    let rest = message.split("offset=").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn file_sha256(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

async fn upload_file(
    r: &Remote,
    id: Uuid,
    local: &std::path::Path,
    remote: &str,
    mode: Option<u32>,
    resume: Option<Uuid>,
    chunk: usize,
) -> Result<Value> {
    use std::io::{Read, Seek, SeekFrom};
    let size = std::fs::metadata(local)
        .with_context(|| format!("reading {}", local.display()))?
        .len();
    eprintln!("hashing {} ({size} bytes)", local.display());
    let sha = file_sha256(local)?;
    let base = format!("/v1/agents/sessions/{id}/uploads");
    let (upload, mut offset) = match resume {
        Some(u) => {
            let v = r.call(Method::GET, &format!("{base}/{u}"), None).await?;
            anyhow::ensure!(
                v["size"].as_u64() == Some(size) && v["sha256"].as_str() == Some(&sha),
                "upload {u} was opened for a different file (size or checksum differ)"
            );
            (u, v["received"].as_u64().unwrap_or(0))
        }
        None => {
            let mut body = json!({"path": remote, "size": size, "sha256": sha});
            if let Some(m) = mode {
                body["mode"] = json!(m);
            }
            let v = r.call(Method::POST, &base, Some(body)).await?;
            let u: Uuid = v["id"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .context("the daemon did not return an upload id")?;
            eprintln!("upload {u} opened; resume with --resume {u}");
            (u, 0)
        }
    };
    let mut f = std::fs::File::open(local)?;
    let mut buf = vec![0u8; chunk];
    let mut failures = 0u32;
    while offset < size {
        f.seek(SeekFrom::Start(offset))?;
        let want = chunk.min((size - offset) as usize);
        f.read_exact(&mut buf[..want])?;
        match r
            .put_bytes(
                &format!("{base}/{upload}?offset={offset}"),
                buf[..want].to_vec(),
            )
            .await
        {
            Ok(v) => {
                failures = 0;
                offset = v["received"].as_u64().unwrap_or(offset + want as u64);
                eprintln!("{offset}/{size} bytes ({}%)", offset * 100 / size);
            }
            Err(e) => {
                // Another writer, or a chunk that landed before its answer was lost: continue
                // where the daemon says it is.
                if let Some(at) = resume_offset(&format!("{e:#}")) {
                    offset = at;
                    continue;
                }
                failures += 1;
                if failures > 3 {
                    return Err(e.context(format!(
                        "upload {upload} stopped at {offset} bytes; rerun with --resume {upload}"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_secs(u64::from(failures))).await;
                // Ask where it is; the chunk may have landed.
                if let Ok(v) = r.call(Method::GET, &format!("{base}/{upload}"), None).await {
                    offset = v["received"].as_u64().unwrap_or(offset);
                }
            }
        }
    }
    r.call(Method::POST, &format!("{base}/{upload}/complete"), None)
        .await
}

async fn download_file(
    r: &Remote,
    id: Uuid,
    remote: &str,
    local: &std::path::Path,
    resume: bool,
) -> Result<Value> {
    use base64::Engine as _;
    use std::io::Write;
    let part = {
        let mut n = local.as_os_str().to_owned();
        n.push(".part");
        std::path::PathBuf::from(n)
    };
    let mut offset = if resume {
        std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resume)
        .truncate(!resume)
        .open(&part)
        .with_context(|| format!("opening {}", part.display()))?;
    let mut expect_sha: Option<String> = None;
    let mut size;
    loop {
        // The checksum is asked for once, on the first slice; the guest hashes the whole file.
        let want_sum = expect_sha.is_none();
        let q = format!(
            "/v1/agents/sessions/{id}/files/range?path={}&offset={offset}&checksum={want_sum}",
            urlencoding_path(remote)
        );
        let v = r.call(Method::GET, &q, None).await?;
        size = v["size"].as_u64().unwrap_or(0);
        if want_sum {
            expect_sha = v["sha256"].as_str().map(str::to_string);
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(v["content_base64"].as_str().unwrap_or(""))
            .context("decoding a chunk")?;
        out.write_all(&data)?;
        offset += data.len() as u64;
        eprintln!("{offset}/{size} bytes");
        if v["eof"].as_bool() == Some(true) || data.is_empty() || offset >= size {
            break;
        }
    }
    out.flush()?;
    drop(out);
    anyhow::ensure!(
        offset == size,
        "the file changed while it was being read ({offset} of {size} bytes); rerun"
    );
    let got = file_sha256(&part)?;
    if let Some(want) = &expect_sha {
        anyhow::ensure!(
            &got == want,
            "checksum mismatch (guest {want}, local {got}); {} was kept, rerun without --resume",
            part.display()
        );
    }
    std::fs::rename(&part, local)?;
    Ok(json!({
        "path": remote, "local": local, "bytes": size, "sha256": got,
        "verified_against_guest": expect_sha.is_some(),
    }))
}

/// Percent-encodes a path for a query value.
fn urlencoding_path(p: &str) -> String {
    let mut out = String::new();
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub async fn run(r: &Remote, cmd: AgentCommand) -> Result<()> {
    match cmd {
        AgentCommand::Upload {
            id,
            local,
            remote,
            mode,
            resume,
            chunk_mib,
        } => show(
            &upload_file(
                r,
                id,
                &local,
                &remote,
                mode,
                resume,
                (chunk_mib as usize) * 1024 * 1024,
            )
            .await?,
        ),
        AgentCommand::Download {
            id,
            remote,
            local,
            resume,
        } => show(&download_file(r, id, &remote, &local, resume).await?),
        AgentCommand::Run {
            agent,
            repo,
            workspace_path,
            require_ready,
            image,
            cpus,
            memory_mb,
            policy,
            allow,
            volume,
            cmd_allow,
            cmd_deny,
            confine,
            fs_read,
            fs_write,
            confine_best_effort,
            max_execs,
            max_exec_seconds,
            max_jobs,
            max_concurrent_jobs,
            max_children,
            max_compute_seconds,
            max_output_bytes,
            max_workspace_bytes,
            ttl_seconds,
        } => {
            let mut body = create_body(
                agent,
                repo,
                workspace_path,
                image,
                cpus,
                memory_mb,
                policy,
                allow,
                ttl_seconds,
            );
            add_process_rules(&mut body, &cmd_allow, &cmd_deny);
            add_filesystem(
                &mut body,
                confine || confine_best_effort || !fs_read.is_empty() || !fs_write.is_empty(),
                &fs_read,
                &fs_write,
                confine_best_effort,
            );
            add_budget(
                &mut body,
                [
                    ("max_execs", max_execs),
                    ("max_exec_seconds", max_exec_seconds),
                    ("max_jobs", max_jobs),
                    ("max_concurrent_jobs", max_concurrent_jobs),
                    ("max_children", max_children),
                    ("max_compute_seconds", max_compute_seconds),
                    ("max_output_bytes", max_output_bytes),
                    ("max_workspace_bytes", max_workspace_bytes),
                ],
            );
            if require_ready && body.get("workspace").is_some() {
                body["workspace"]["require_ready"] = json!(true);
            }
            add_volumes(&mut body, &volume)?;
            show(
                &r.call(Method::POST, "/v1/agents/sessions", Some(body))
                    .await?,
            )
        }
        AgentCommand::Ls => show(&r.call(Method::GET, "/v1/agents/sessions", None).await?),
        AgentCommand::Get { id } => show(
            &r.call(Method::GET, &format!("/v1/agents/sessions/{id}"), None)
                .await?,
        ),
        AgentCommand::Approvals { id } => show(
            &r.call(
                Method::GET,
                &format!("/v1/agents/sessions/{id}/approvals"),
                None,
            )
            .await?,
        ),
        AgentCommand::Approve { id, approval, note } => {
            decide_approval(r, id, approval, "approve", note).await
        }
        AgentCommand::Deny { id, approval, note } => {
            decide_approval(r, id, approval, "deny", note).await
        }
        AgentCommand::Exec {
            id,
            timeout_seconds,
            approval_id,
            approval_wait,
            command,
        } => {
            let mut body = json!({ "command": guest_command(&command) });
            if let Some(t) = timeout_seconds {
                body["timeout_seconds"] = json!(t);
            }
            if let Some(a) = approval_id {
                body["approval_id"] = json!(a);
            }
            if let Some(w) = approval_wait {
                body["approval_wait_seconds"] = json!(w);
            }
            let v = r
                .call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/exec"),
                    Some(body),
                )
                .await?;
            // Mirror the guest command: relay its output and exit with its code.
            if let Some(s) = v["stdout"].as_str() {
                print!("{s}");
            }
            if let Some(s) = v["stderr"].as_str() {
                eprint!("{s}");
            }
            match v["exit_code"].as_i64() {
                Some(0) | None => Ok(()),
                Some(code) => std::process::exit(code.clamp(1, 255) as i32),
            }
        }
        AgentCommand::Checkpoint { id, tag, disk_only } => {
            let mut body = json!({});
            if let Some(t) = tag {
                body["tag"] = json!(t);
            }
            if disk_only {
                body["memory"] = json!(false);
            }
            show(
                &r.call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/checkpoint"),
                    Some(body),
                )
                .await?,
            )
        }
        AgentCommand::Restore { id, tag } => show(
            &r.call(
                Method::POST,
                &format!("/v1/agents/sessions/{id}/restore"),
                Some(json!({ "tag": tag })),
            )
            .await?,
        ),
        AgentCommand::Fork {
            id,
            count,
            identity,
        } => {
            let mut body = json!({ "count": count });
            if let Some(i) = identity {
                body["identity"] = json!(i);
            }
            show(
                &r.call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/fork"),
                    Some(body),
                )
                .await?,
            )
        }
        AgentCommand::Token {
            id,
            ops,
            ttl_seconds,
            label,
        } => {
            let mut body = json!({ "ops": ops });
            if let Some(t) = ttl_seconds {
                body["ttl_seconds"] = json!(t);
            }
            if let Some(l) = label {
                body["label"] = json!(l);
            }
            show(
                &r.call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/tokens"),
                    Some(body),
                )
                .await?,
            )
        }
        AgentCommand::Grant {
            id,
            secret_ref,
            hosts,
            value,
            ttl_seconds,
            methods,
            paths,
        } => {
            let mut body = json!({"secret_ref": secret_ref, "hosts": hosts});
            if !methods.is_empty() {
                body["methods"] = json!(methods);
            }
            if !paths.is_empty() {
                body["paths"] = json!(paths);
            }
            if let Some(v) = value {
                body["value"] = json!(v);
            }
            if let Some(t) = ttl_seconds {
                body["ttl_seconds"] = json!(t);
            }
            show(
                &r.call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/grants"),
                    Some(body),
                )
                .await?,
            )
        }
        AgentCommand::Probe { id } => show(
            &r.call(
                Method::GET,
                &format!("/v1/agents/sessions/{id}/probe"),
                None,
            )
            .await?,
        ),
        AgentCommand::Grants { id } => show(
            &r.call(
                Method::GET,
                &format!("/v1/agents/sessions/{id}/grants"),
                None,
            )
            .await?,
        ),
        AgentCommand::Ungrant { id, grant } => {
            let path = match grant {
                Some(g) => format!("/v1/agents/sessions/{id}/grants/{g}"),
                None => format!("/v1/agents/sessions/{id}/grants"),
            };
            show(&r.call(Method::DELETE, &path, None).await?)
        }
        AgentCommand::Tokens { id } => show(
            &r.call(
                Method::GET,
                &format!("/v1/agents/sessions/{id}/tokens"),
                None,
            )
            .await?,
        ),
        AgentCommand::Untoken { id, token } => show(
            &r.call(
                Method::DELETE,
                &format!("/v1/agents/sessions/{id}/tokens/{token}"),
                None,
            )
            .await?,
        ),
        AgentCommand::Promote {
            id,
            dry_run,
            plan_id,
            skip_conflicts,
            paths,
        } => show(
            &r.call(
                Method::POST,
                &format!("/v1/agents/sessions/{id}/promote"),
                Some(promote_body(dry_run, plan_id, skip_conflicts, &paths)),
            )
            .await?,
        ),
        AgentCommand::Timeline { id, since } => {
            let mut path = format!("/v1/agents/sessions/{id}/events");
            if let Some(s) = since {
                path.push_str("?since=");
                path.push_str(&crate::remote::encode_query(&s));
            }
            let v = r.call(Method::GET, &path, None).await?;
            // One line per ledger entry, newest last; the raw JSON is `-o json` territory.
            for e in v["ledger"].as_array().into_iter().flatten() {
                println!(
                    "{}  {:<16} {}",
                    e["ts"].as_str().unwrap_or("-"),
                    e["kind"].as_str().unwrap_or("-"),
                    e["detail"]
                );
            }
            let vm = v["vm_events"].as_array().map_or(0, Vec::len);
            if vm > 0 {
                println!("({vm} VM events: fluxctl events --vm <sandbox id>)");
            }
            Ok(())
        }
        AgentCommand::Rollout {
            id,
            count,
            timeout_seconds,
            keep,
            identity,
            command,
        } => show(
            &rollout(
                r,
                id,
                count,
                &guest_command(&command),
                timeout_seconds,
                keep,
                identity.as_deref(),
            )
            .await?,
        ),
        AgentCommand::Evaluate {
            id,
            candidates,
            test,
            timeout_seconds,
            keep_all,
            identity,
        } => show(
            &evaluate(
                r,
                id,
                &candidates,
                test.as_deref(),
                timeout_seconds,
                keep_all,
                identity.as_deref(),
            )
            .await?,
        ),
        AgentCommand::SshConfig { id, name } => {
            let v = r
                .call(Method::GET, &format!("/v1/agents/sessions/{id}/ssh"), None)
                .await
                .context("ssh details")?;
            let alias = name.unwrap_or_else(|| format!("fluxvm-{}", &id.to_string()[..8]));
            print!("{}", ssh_config_block(&alias, &v)?);
            Ok(())
        }
        AgentCommand::Shell { id, web: true, .. } => {
            let url = web_terminal_url(r.base(), id);
            println!("{url}");
            if cfg!(target_os = "macos") {
                let _ = std::process::Command::new("open").arg(&url).status();
            }
            Ok(())
        }
        AgentCommand::Shell {
            id, plain, command, ..
        } => shell(r, id, &command, plain).await,
        AgentCommand::Export {
            id,
            out,
            otlp,
            otlp_header,
        } => {
            use std::io::Write as _;
            let v = r
                .call(
                    Method::GET,
                    &format!("/v1/agents/sessions/{id}/trajectory"),
                    None,
                )
                .await
                .context("trajectory")?;
            if let Some(endpoint) = &otlp {
                let doc = otlp_traces(&v)?;
                send_otlp(endpoint, &otlp_header, &doc).await?;
                eprintln!(
                    "sent {} spans to {endpoint}",
                    doc["resourceSpans"][0]["scopeSpans"][0]["spans"]
                        .as_array()
                        .map_or(0, Vec::len)
                );
                if out.is_none() {
                    return Ok(());
                }
            }
            let mut buf = Vec::new();
            writeln!(
                buf,
                "{}",
                json!({"type": "session", "session": v["session"]})
            )?;
            for step in v["steps"].as_array().into_iter().flatten() {
                writeln!(buf, "{step}")?;
            }
            match out {
                Some(p) => {
                    std::fs::write(&p, &buf).with_context(|| format!("write {}", p.display()))?
                }
                None => std::io::stdout().write_all(&buf)?,
            }
            Ok(())
        }
        AgentCommand::Artifacts { id } => show(
            &r.call(
                Method::GET,
                &format!("/v1/agents/sessions/{id}/artifacts"),
                None,
            )
            .await
            .context("artifacts")?,
        ),
        AgentCommand::Git { id, op } => {
            let base = format!("/v1/agents/sessions/{id}/git");
            match op {
                GitOp::Status { repo } => show(
                    &r.call(
                        Method::GET,
                        &format!("{base}/status{}", git_query(&repo, &[])),
                        None,
                    )
                    .await
                    .context("git status")?,
                ),
                GitOp::Diff { repo, staged, path } => {
                    let mut extra = Vec::new();
                    if staged {
                        extra.push(("staged", "true".to_string()));
                    }
                    if let Some(p) = path {
                        extra.push(("path", p));
                    }
                    let v = r
                        .call(
                            Method::GET,
                            &format!("{base}/diff{}", git_query(&repo, &extra)),
                            None,
                        )
                        .await
                        .context("git diff")?;
                    // The diff itself is what you want on stdout, ready for `| less` or `| git apply`.
                    print!("{}", v["diff"].as_str().unwrap_or(""));
                    if v["truncated"].as_bool() == Some(true) {
                        eprintln!("[diff truncated at 256 KiB]");
                    }
                    Ok(())
                }
                GitOp::Log { repo, limit } => {
                    let extra: Vec<(&str, String)> = limit
                        .map(|n| ("limit", n.to_string()))
                        .into_iter()
                        .collect();
                    let v = r
                        .call(
                            Method::GET,
                            &format!("{base}/log{}", git_query(&repo, &extra)),
                            None,
                        )
                        .await
                        .context("git log")?;
                    for c in v["commits"].as_array().into_iter().flatten() {
                        println!(
                            "{} {} {} {}",
                            c["commit"].as_str().unwrap_or("").get(..8).unwrap_or(""),
                            c["date"].as_str().unwrap_or(""),
                            c["author"].as_str().unwrap_or(""),
                            c["subject"].as_str().unwrap_or("")
                        );
                    }
                    Ok(())
                }
                GitOp::Commit {
                    message,
                    repo,
                    staged_only,
                } => show(
                    &r.call(
                        Method::POST,
                        &format!("{base}/commit"),
                        Some(json!({"message": message, "repo": repo, "all": !staged_only})),
                    )
                    .await
                    .context("git commit")?,
                ),
                GitOp::Branch { name, repo } => show(
                    &r.call(
                        Method::POST,
                        &format!("{base}/branch"),
                        Some(json!({"name": name, "repo": repo})),
                    )
                    .await
                    .context("git branch")?,
                ),
            }
        }
        AgentCommand::Files { id, path } => show(
            &r.call(
                Method::GET,
                &format!(
                    "/v1/agents/sessions/{id}/files?path={}",
                    crate::remote::encode_query(&path)
                ),
                None,
            )
            .await
            .context("list files")?,
        ),
        AgentCommand::Cat { id, path }
        | AgentCommand::Pull {
            id,
            remote: path,
            local: None,
        } => {
            use base64::Engine as _;
            use std::io::Write as _;
            let v = read_guest_file(r, id, &path).await?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(v["content_base64"].as_str().unwrap_or(""))
                .context("decode content")?;
            std::io::stdout().write_all(&bytes)?;
            Ok(())
        }
        AgentCommand::Pull {
            id,
            remote,
            local: Some(local),
        } => {
            use base64::Engine as _;
            let v = read_guest_file(r, id, &remote).await?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(v["content_base64"].as_str().unwrap_or(""))
                .context("decode content")?;
            std::fs::write(&local, &bytes).with_context(|| format!("write {}", local.display()))?;
            println!(
                "{}",
                json!({"path": remote, "bytes": bytes.len(), "to": local})
            );
            Ok(())
        }
        AgentCommand::Put {
            id,
            local,
            remote,
            mode,
        } => {
            use base64::Engine as _;
            let bytes =
                std::fs::read(&local).with_context(|| format!("read {}", local.display()))?;
            check_file_write_size(bytes.len(), &local.display().to_string())?;
            let mut body = json!({
                "path": remote,
                "content_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
            });
            if let Some(m) = mode {
                body["mode"] = json!(
                    u32::from_str_radix(m.trim_start_matches("0o"), 8)
                        .map_err(|_| anyhow::anyhow!("--mode must be octal, e.g. 755"))?
                );
            }
            show(
                &r.call(
                    Method::PUT,
                    &format!("/v1/agents/sessions/{id}/files/content"),
                    Some(body),
                )
                .await
                .context("write file")?,
            )
        }
        AgentCommand::RmFile {
            id,
            path,
            recursive,
        } => {
            r.call(
                Method::DELETE,
                &format!(
                    "/v1/agents/sessions/{id}/files?path={}&recursive={recursive}",
                    crate::remote::encode_query(&path)
                ),
                None,
            )
            .await
            .context("remove file")?;
            println!("{}", json!({"ok": true, "removed": path}));
            Ok(())
        }
        AgentCommand::Bg {
            id,
            stdin,
            max_output_bytes,
            timeout,
            approval_id,
            approval_wait,
            command,
        } => {
            let mut body = json!({"command": guest_command(&command)});
            if let Some(a) = approval_id {
                body["approval_id"] = json!(a);
            }
            if let Some(w) = approval_wait {
                body["approval_wait_seconds"] = json!(w);
            }
            if stdin {
                body["stdin"] = json!(true);
            }
            if let Some(t) = timeout {
                body["timeout_seconds"] = json!(t);
            }
            if let Some(n) = max_output_bytes {
                body["max_output_bytes"] = json!(n);
            }
            let v = r
                .call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/jobs"),
                    Some(body),
                )
                .await
                .context("start job")?;
            show(&v)
        }
        AgentCommand::Jobs { id } => show(
            &r.call(Method::GET, &format!("/v1/agents/sessions/{id}/jobs"), None)
                .await?,
        ),
        AgentCommand::Stdin {
            id,
            job,
            text,
            close,
        } => {
            if text.is_none() && !close {
                anyhow::bail!("give text to write, or --close");
            }
            let mut body = json!({});
            if let Some(t) = text {
                body["text"] = json!(t);
            }
            if close {
                body["close"] = json!(true);
            }
            show(
                &r.call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/jobs/{job}/stdin"),
                    Some(body),
                )
                .await
                .context("write stdin")?,
            )
        }
        AgentCommand::Logs { id, job, follow } => {
            use std::io::Write as _;
            let (mut offset, mut err_offset) = (0u64, 0u64);
            // Supervised jobs hold the request open while there is nothing to read.
            let wait = if follow { 15_000 } else { 0 };
            loop {
                let v = r
                    .call(
                        Method::GET,
                        &format!(
                            "/v1/agents/sessions/{id}/jobs/{job}?offset={offset}&stderr_offset={err_offset}&wait_ms={wait}"
                        ),
                        None,
                    )
                    .await
                    .context("read job")?;
                print!("{}", v["output"].as_str().unwrap_or(""));
                let _ = std::io::stdout().flush();
                if let Some(e) = v["stderr"].as_str().filter(|e| !e.is_empty()) {
                    eprint!("{e}");
                }
                offset = v["next_offset"].as_u64().unwrap_or(offset);
                err_offset = v["stderr_next_offset"].as_u64().unwrap_or(err_offset);
                let more = v["more"].as_bool() == Some(true);
                // Shell-script jobs have no `eof`; they are done once an exit code shows.
                let drained = v["eof"].as_bool().unwrap_or(true);
                let code = v["exit_code"]
                    .as_i64()
                    .or(v["signal"].as_i64().map(|s| 128 + s));
                let finished = v["state"].as_str().is_some_and(|s| s != "running");
                if finished && drained && !more {
                    let code = code.unwrap_or(-1);
                    eprintln!("[job {} {code}]", v["state"].as_str().unwrap_or("exited"));
                    std::process::exit(if follow { code.clamp(0, 255) as i32 } else { 0 });
                }
                if !follow && !more {
                    return Ok(());
                }
                if !more && v["engine"].as_str() != Some("supervisor") {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        AgentCommand::Kill { id, job, signal } => {
            let q = signal.map(|s| format!("?signal={s}")).unwrap_or_default();
            r.call(
                Method::DELETE,
                &format!("/v1/agents/sessions/{id}/jobs/{job}{q}"),
                None,
            )
            .await?;
            println!("{}", json!({"ok": true, "killed": job}));
            Ok(())
        }
        AgentCommand::Expose { id, port, ttl } => {
            let mut v = r
                .call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/preview"),
                    Some(json!({"port": port, "ttl_seconds": ttl})),
                )
                .await
                .context("preview")?;
            // The daemon answers with a path; the full URL is the daemon's address plus that path.
            if let Some(path) = v["url"].as_str() {
                v["url"] = json!(format!("{}{}", r.base().trim_end_matches('/'), path));
            }
            show(&v)
        }
        AgentCommand::Previews { id } => show(
            &r.call(
                Method::GET,
                &format!("/v1/agents/sessions/{id}/previews"),
                None,
            )
            .await?,
        ),
        AgentCommand::Unexpose { id, token } => {
            r.call(
                Method::DELETE,
                &format!("/v1/agents/sessions/{id}/preview/{token}"),
                None,
            )
            .await?;
            println!("{}", json!({"ok": true, "revoked": token}));
            Ok(())
        }
        AgentCommand::Rm { id } => {
            r.call(Method::DELETE, &format!("/v1/agents/sessions/{id}"), None)
                .await?;
            println!("{}", json!({"ok": true, "deleted": id}));
            Ok(())
        }
    }
}

async fn read_guest_file(r: &Remote, id: Uuid, path: &str) -> Result<Value> {
    r.call(
        Method::GET,
        &format!(
            "/v1/agents/sessions/{id}/files/content?path={}",
            crate::remote::encode_query(path)
        ),
        None,
    )
    .await
    .context("read file")
}

/// One argument is a shell line, as typed (`agent exec ID -- 'cd /w && make'`); several are an
/// argv, so each word is single-quoted and `-- sh -c 'a; b'` keeps its script in one piece.
fn guest_command(args: &[String]) -> String {
    if let [one] = args {
        return one.clone();
    }
    args.iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `POST /v1/agents/sessions` body for `agent run` flags.
/// Puts the local terminal in raw mode and restores it on drop. `None` when stdin is not a tty.
struct RawMode {
    orig: libc::termios,
}

impl RawMode {
    fn enter() -> Option<Self> {
        // SAFETY: plain termios calls on fd 0 with a zeroed struct that tcgetattr fills.
        unsafe {
            if libc::isatty(0) == 0 {
                return None;
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return None;
            }
            let orig = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(0, libc::TCSAFLUSH, &t) != 0 {
                return None;
            }
            Some(Self { orig })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores the attributes read in `enter`.
        unsafe {
            libc::tcsetattr(0, libc::TCSAFLUSH, &self.orig);
        }
    }
}

fn terminal_size() -> (u16, u16) {
    // SAFETY: TIOCGWINSZ fills a winsize; a failure leaves the zeroed value, handled below.
    unsafe {
        let mut w: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut w) == 0 && w.ws_col > 0 && w.ws_row > 0 {
            return (w.ws_col, w.ws_row);
        }
    }
    (80, 24)
}

/// The `ws://` URL of a session's terminal. Pure, so it is unit-tested.
pub(crate) fn pty_url(base: &str, id: Uuid, cols: u16, rows: u16, command: Option<&str>) -> String {
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        format!("ws://{base}")
    };
    let mut url = format!("{ws}/v1/agents/sessions/{id}/pty?cols={cols}&rows={rows}");
    if let Some(c) = command {
        url.push_str("&command=");
        url.push_str(&crate::remote::encode_query(c));
    }
    url
}

/// An OpenSSH `Host` block from `GET .../ssh`. The guest is new every session, so host keys are
/// not recorded; a vsock guest goes through this binary's `vsock-proxy`. Pure, so it is unit-tested.
pub(crate) fn ssh_config_block(alias: &str, v: &Value) -> Result<String> {
    if alias.is_empty()
        || !alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        anyhow::bail!("host alias {alias:?} may use letters, digits, '.', '_' and '-'");
    }
    let user = v["user"].as_str().context("no user in the answer")?;
    let key = v["identity_file"]
        .as_str()
        .context("no identity file in the answer")?;
    let mut out = format!("Host {alias}\n");
    match (v["host"].as_str(), v["vsock_socket"].as_str()) {
        (Some(host), _) => out.push_str(&format!("  HostName {host}\n")),
        (None, Some(sock)) => {
            let exe = std::env::current_exe().unwrap_or_else(|_| "fluxctl".into());
            out.push_str("  HostName vsock\n");
            out.push_str(&format!(
                "  ProxyCommand {}\n",
                fluxvm_apple::ssh::proxy_command(&exe, std::path::Path::new(sock))
            ));
        }
        (None, None) => anyhow::bail!("the daemon returned neither an address nor a vsock socket"),
    }
    out.push_str(&format!(
        "  User {user}\n  IdentityFile {key}\n  IdentitiesOnly yes\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  LogLevel ERROR\n"
    ));
    Ok(out)
}

/// The browser terminal for a session. The token is never part of it: the page asks for one.
pub(crate) fn web_terminal_url(base: &str, id: Uuid) -> String {
    format!(
        "{}/console/terminal?session={id}",
        base.trim_end_matches('/')
    )
}

/// The most one file write may carry: the daemon's request body limit is 2 MiB and the file travels
/// base64 inside JSON. Past it the server drops the connection (a reset, not a clean 413), so the
/// client refuses first with a message that says what to do.
pub(crate) const MAX_FILE_WRITE_BYTES: usize = 1_500_000;

pub(crate) fn check_file_write_size(len: usize, what: &str) -> Result<()> {
    if len > MAX_FILE_WRITE_BYTES {
        anyhow::bail!(
            "{what} is {len} bytes; one write is limited to {MAX_FILE_WRITE_BYTES} bytes. Split it \
             (`split -b 1000000`) and write the pieces, or put it somewhere the guest can fetch it from"
        );
    }
    Ok(())
}

/// The `&term=&colorterm=&plain=` part of the terminal URL for this client. Pure, so it is
/// unit-tested; the daemon validates `term` again.
pub(crate) fn pty_look_query(term: Option<&str>, colorterm: Option<&str>, plain: bool) -> String {
    let mut q = String::new();
    if let Some(t) = term.filter(|t| !t.is_empty() && *t != "dumb") {
        q.push_str("&term=");
        q.push_str(&crate::remote::encode_query(t));
    }
    if let Some(c) = colorterm.filter(|c| matches!(*c, "truecolor" | "24bit")) {
        q.push_str("&colorterm=");
        q.push_str(c);
    }
    if plain {
        q.push_str("&plain=true");
    }
    q
}

/// `&term_program=&term_program_version=` from this terminal (WezTerm, iTerm.app, ...).
pub(crate) fn pty_program_query(program: Option<&str>, version: Option<&str>) -> String {
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 40
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    };
    let Some(p) = program.filter(|p| ok(p)) else {
        return String::new();
    };
    let mut q = format!("&term_program={}", crate::remote::encode_query(p));
    if let Some(v) = version.filter(|v| ok(v)) {
        q.push_str("&term_program_version=");
        q.push_str(&crate::remote::encode_query(v));
    }
    q
}

async fn shell(r: &Remote, id: Uuid, command: &[String], plain: bool) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use std::io::Write as _;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    let (cols, rows) = terminal_size();
    let cmd = (!command.is_empty()).then(|| guest_command(command));
    let mut url = pty_url(r.base(), id, cols, rows, cmd.as_deref());
    // Tell the guest what this terminal is, so colours and keys match what the user is looking at.
    url.push_str(&pty_look_query(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
        plain,
    ));
    url.push_str(&pty_program_query(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM_PROGRAM_VERSION").ok().as_deref(),
    ));
    let mut req = url.as_str().into_client_request().context("terminal url")?;
    if let Some(t) = r.bearer() {
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {t}").parse().context("bearer token")?,
        );
    }
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.context(
        "opening the terminal (is the session running, and does it have no command rules?)",
    )?;

    // Keys are read on a thread: a blocking read on stdin must not stall the runtime.
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let mut buf = [0u8; 1024];
        let mut stdin = std::io::stdin().lock();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || key_tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let raw = RawMode::enter();
    let mut stdout = std::io::stdout();
    let mut exit = None;
    let mut stdin_open = true;
    // A resize of the local terminal is told to the daemon, which sets the guest tty's size.
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .context("watching for terminal resizes")?;
    let mut last_size = (cols, rows);
    loop {
        tokio::select! {
            _ = winch.recv() => {
                let now = terminal_size();
                if now != last_size {
                    last_size = now;
                    let msg = json!({"resize": {"cols": now.0, "rows": now.1}}).to_string();
                    if ws.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
            },
            key = key_rx.recv(), if stdin_open => match key {
                Some(bytes) => {
                    if ws.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                // Piped input ended: send the terminal's end-of-file (^D) so the shell finishes what
                // it was given and exits, and keep reading its output until it closes.
                None => {
                    stdin_open = false;
                    if ws.send(Message::Binary(vec![0x04].into())).await.is_err() {
                        break;
                    }
                }
            },
            msg = ws.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    stdout.write_all(&b)?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Text(t))) => {
                    stdout.write_all(t.as_bytes())?;
                    stdout.flush()?;
                }
                Some(Ok(Message::Close(frame))) => {
                    exit = frame
                        .and_then(|f| f.reason.strip_prefix("exit=").and_then(|c| c.parse::<i32>().ok()));
                    break;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
        }
    }
    drop(raw);
    match exit {
        Some(0) | None => Ok(()),
        Some(code) => std::process::exit(code.clamp(1, 255)),
    }
}

fn span_id(session: &str, tag: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(format!("{session}:{tag}"));
    let mut id = u64::from_be_bytes(h[..8].try_into().expect("8 bytes"));
    if id == 0 {
        id = 1; // an all-zero span id is invalid in OTLP
    }
    format!("{id:016x}")
}

fn otlp_attr(key: &str, v: &Value) -> Option<Value> {
    let value = match v {
        Value::String(s) => json!({"stringValue": s}),
        Value::Bool(b) => json!({"boolValue": b}),
        Value::Number(n) if n.is_i64() => json!({"intValue": n.to_string()}),
        Value::Number(n) => json!({"doubleValue": n}),
        Value::Null => return None,
        other => json!({"stringValue": other.to_string()}),
    };
    Some(json!({"key": key, "value": value}))
}

/// Turn a session trajectory (`GET .../trajectory`) into an OTLP/JSON trace: one root span for
/// the session and one child per step. The trace id is the session's UUID. The ledger stamps an
/// `exec` when it finishes, so an exec's span ends at its stamp and starts `duration_ms` earlier;
/// every other step is an instant (start == end). Pure, so it is unit-tested.
pub(crate) fn otlp_traces(traj: &Value) -> Result<Value> {
    let session = traj["session"]["id"]
        .as_str()
        .context("trajectory has no session id")?;
    let trace_id = session.replace('-', "");
    if trace_id.len() != 32 {
        anyhow::bail!("session id {session:?} is not a UUID");
    }
    let nanos = |ts: &str| -> Result<i128> {
        let t = chrono::DateTime::parse_from_rfc3339(ts)
            .with_context(|| format!("bad timestamp {ts:?}"))?;
        Ok(t.timestamp_nanos_opt().map(i128::from).unwrap_or(0))
    };
    let root_id = span_id(session, "root");
    let steps = traj["steps"].as_array().cloned().unwrap_or_default();
    let mut spans = Vec::new();
    let (mut first, mut last) = (i128::MAX, 0i128);
    for st in &steps {
        let end = nanos(st["ts"].as_str().unwrap_or(""))?;
        let dur_ms = st["detail"]["duration_ms"].as_u64().unwrap_or(0) as i128;
        let start = end - dur_ms * 1_000_000;
        first = first.min(start);
        last = last.max(end);
        let source = st["source"].as_str().unwrap_or("-");
        let kind = st["kind"].as_str().unwrap_or("-");
        let mut attrs = vec![
            json!({"key": "fluxvm.source", "value": {"stringValue": source}}),
            json!({"key": "fluxvm.step", "value": {"intValue": st["seq"].as_u64().unwrap_or(0).to_string()}}),
        ];
        if let Some(d) = st["detail"].as_object() {
            for (k, v) in d {
                if let Some(a) = otlp_attr(&format!("fluxvm.{k}"), v) {
                    attrs.push(a);
                }
            }
        }
        let failed = st["detail"]["exit_code"].as_i64().is_some_and(|c| c != 0)
            || kind.ends_with(".refused");
        spans.push(json!({
            "traceId": trace_id,
            "spanId": span_id(session, &st["seq"].to_string()),
            "parentSpanId": root_id,
            "name": kind,
            "kind": 1,
            "startTimeUnixNano": start.to_string(),
            "endTimeUnixNano": end.to_string(),
            "attributes": attrs,
            "status": if failed { json!({"code": 2}) } else { json!({"code": 0}) },
        }));
    }
    if first == i128::MAX {
        first = nanos(traj["session"]["created_at"].as_str().unwrap_or(""))?;
        last = first;
    }
    let mut root_attrs = Vec::new();
    for k in ["agent", "backend", "state", "sandbox_id"] {
        if let Some(a) = otlp_attr(&format!("fluxvm.session.{k}"), &traj["session"][k]) {
            root_attrs.push(a);
        }
    }
    spans.insert(
        0,
        json!({
            "traceId": trace_id,
            "spanId": root_id,
            "name": "agent.session",
            "kind": 1,
            "startTimeUnixNano": first.to_string(),
            "endTimeUnixNano": last.to_string(),
            "attributes": root_attrs,
            "status": {"code": 0},
        }),
    );
    Ok(json!({"resourceSpans": [{
        "resource": {"attributes": [
            {"key": "service.name", "value": {"stringValue": "fluxvm"}},
        ]},
        "scopeSpans": [{"scope": {"name": "fluxctl agent export"}, "spans": spans}],
    }]}))
}

async fn send_otlp(endpoint: &str, headers: &[String], doc: &Value) -> Result<()> {
    let url = if endpoint.trim_end_matches('/').ends_with("/v1/traces") {
        endpoint.to_string()
    } else {
        format!("{}/v1/traces", endpoint.trim_end_matches('/'))
    };
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(15))
        .json(doc);
    for h in headers {
        let (k, v) = h
            .split_once('=')
            .filter(|(k, _)| !k.is_empty())
            .with_context(|| format!("--otlp-header {h:?} must be KEY=VALUE"))?;
        req = req.header(k, v);
    }
    let resp = req.send().await.with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("collector answered {} for {url}", resp.status());
    }
    Ok(())
}

/// Add `runtime.volumes` from `NAME:/guest/path` specs.
pub(crate) fn add_volumes(body: &mut Value, specs: &[String]) -> Result<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let mut vols = Vec::new();
    for spec in specs {
        let (name, path) = spec
            .split_once(':')
            .filter(|(n, p)| !n.is_empty() && p.starts_with('/'))
            .with_context(|| format!("volume {spec:?} must be NAME:/absolute/guest/path"))?;
        vols.push(json!({"name": name, "path": path}));
    }
    body["runtime"]["volumes"] = json!(vols);
    Ok(())
}

/// The `promote` request body; only what was asked for, so the daemon's defaults apply otherwise.
pub(crate) fn promote_body(
    dry_run: bool,
    plan_id: Option<String>,
    skip_conflicts: bool,
    paths: &[String],
) -> Value {
    let mut b = json!({});
    if dry_run {
        b["dry_run"] = json!(true);
    }
    if let Some(p) = plan_id {
        b["plan_id"] = json!(p);
    }
    if skip_conflicts {
        b["skip_conflicts"] = json!(true);
    }
    if !paths.is_empty() {
        b["paths"] = json!(paths);
    }
    b
}

/// Attach `budget` to a create body when any limit was given.
pub(crate) fn add_budget<const N: usize>(body: &mut Value, limits: [(&str, Option<u64>); N]) {
    for (name, v) in limits {
        if let Some(n) = v {
            body["budget"][name] = json!(n);
        }
    }
}

/// Attach `policy.filesystem` (guest confinement) to a create body when `confine` is set.
pub(crate) fn add_filesystem(
    body: &mut Value,
    confine: bool,
    read: &[String],
    write: &[String],
    best_effort: bool,
) {
    if !confine {
        return;
    }
    let mut fs = json!({});
    if !read.is_empty() {
        fs["read"] = json!(read);
    }
    if !write.is_empty() {
        fs["write"] = json!(write);
    }
    if best_effort {
        fs["enforcement"] = json!("best-effort");
    }
    body["policy"]["filesystem"] = fs;
}

/// Attach `policy.process` command rules to a create body when any were given.
pub(crate) fn add_process_rules(body: &mut Value, allow: &[String], deny: &[String]) {
    if allow.is_empty() && deny.is_empty() {
        return;
    }
    let mut rules = json!({});
    if !allow.is_empty() {
        rules["allow"] = json!(allow);
    }
    if !deny.is_empty() {
        rules["deny"] = json!(deny);
    }
    body["policy"]["process"] = rules;
}

#[allow(clippy::too_many_arguments)]
fn create_body(
    agent: String,
    repo: Option<String>,
    workspace_path: Option<String>,
    image: Option<String>,
    cpus: Option<u8>,
    memory_mb: Option<u64>,
    policy: Option<String>,
    allow: Vec<String>,
    ttl_seconds: Option<u64>,
) -> Value {
    let mut body = json!({ "agent": agent });
    if repo.is_some() || workspace_path.is_some() {
        let mut ws = json!({});
        if let Some(g) = repo {
            ws["git"] = json!(g);
        }
        if let Some(p) = workspace_path {
            ws["path"] = json!(p);
        }
        body["workspace"] = ws;
    }
    if image.is_some() || cpus.is_some() || memory_mb.is_some() {
        let mut rt = json!({});
        if let Some(i) = image {
            rt["image"] = json!(i);
        }
        if let Some(c) = cpus {
            rt["cpus"] = json!(c);
        }
        if let Some(m) = memory_mb {
            rt["memory_mb"] = json!(m);
        }
        body["runtime"] = rt;
    }
    if policy.is_some() || !allow.is_empty() {
        let mut p = json!({});
        if let Some(name) = policy {
            p["preset"] = json!(name);
        }
        if !allow.is_empty() {
            p["network"] = json!({ "default": "deny", "allow": allow });
        }
        body["policy"] = p;
    }
    if let Some(t) = ttl_seconds {
        body["ttl_seconds"] = json!(t);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_helpers() {
        assert_eq!(
            resume_offset(
                "PUT /x: 409 Conflict: upload u has 8388608 bytes: send the next chunk with offset=8388608"
            ),
            Some(8388608)
        );
        assert_eq!(resume_offset("no offset here"), None);
        assert_eq!(parse_octal("755"), Ok(0o755));
        assert_eq!(parse_octal("0o640"), Ok(0o640));
        assert!(parse_octal("999").is_err());
        assert!(parse_octal("77777").is_err());
        assert_eq!(urlencoding_path("/a b/c&d.txt"), "/a%20b/c%26d.txt");
        let dir = std::env::temp_dir().join(format!("fluxctl-sha-{}", std::process::id()));
        std::fs::write(&dir, b"abc").unwrap();
        assert_eq!(
            file_sha256(&dir).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn exec_keeps_a_script_in_one_piece() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(guest_command(&v(&["cd /w && make"])), "cd /w && make");
        assert_eq!(
            guest_command(&v(&["sh", "-c", "uname -a; exit 3"])),
            "'sh' '-c' 'uname -a; exit 3'"
        );
        assert_eq!(guest_command(&v(&["echo", "it's"])), "'echo' 'it'\\''s'");
    }

    #[test]
    fn the_web_terminal_url_has_no_token() {
        let id = Uuid::nil();
        assert_eq!(
            web_terminal_url("http://127.0.0.1:7788/", id),
            format!("http://127.0.0.1:7788/console/terminal?session={id}")
        );
    }

    #[test]
    fn the_terminal_program_is_forwarded_as_plain_words() {
        assert_eq!(pty_program_query(None, None), "");
        assert_eq!(
            pty_program_query(Some("WezTerm"), Some("20240203-110809-5046fc22")),
            "&term_program=WezTerm&term_program_version=20240203-110809-5046fc22"
        );
        assert_eq!(
            pty_program_query(Some("iTerm.app"), None),
            "&term_program=iTerm.app"
        );
        assert_eq!(pty_program_query(Some("a b"), Some("1")), "");
        assert_eq!(pty_program_query(Some("x"), Some("a;b")), "&term_program=x");
    }

    #[test]
    fn an_ssh_config_block_covers_an_address_and_a_vsock_guest() {
        let nat = ssh_config_block(
            "fluxvm-ab12cd34",
            &json!({"user": "sandbox", "host": "192.168.64.6", "identity_file": "/k/id", "vsock_socket": null}),
        )
        .unwrap();
        assert!(nat.starts_with("Host fluxvm-ab12cd34\n"));
        assert!(nat.contains("HostName 192.168.64.6") && nat.contains("User sandbox"));
        assert!(nat.contains("IdentityFile /k/id") && nat.contains("StrictHostKeyChecking no"));
        assert!(!nat.contains("ProxyCommand"));
        let off = ssh_config_block(
            "w",
            &json!({"user": "sandbox", "host": null, "identity_file": "/k/id", "vsock_socket": "/s/vsock.sock"}),
        )
        .unwrap();
        assert!(
            off.contains("ProxyCommand")
                && off.contains("vsock-proxy")
                && off.contains("'/s/vsock.sock' 22")
        );
        assert!(ssh_config_block("bad name", &json!({})).is_err());
        assert!(ssh_config_block("ok", &json!({"user": "u", "identity_file": "k"})).is_err());
    }

    #[test]
    fn an_oversize_write_is_refused_before_it_is_sent() {
        assert!(check_file_write_size(MAX_FILE_WRITE_BYTES, "f").is_ok());
        let e = check_file_write_size(MAX_FILE_WRITE_BYTES + 1, "big.bin").unwrap_err();
        let m = e.to_string();
        assert!(
            m.contains("big.bin") && m.contains("limited to") && m.contains("split"),
            "{m}"
        );
        // the limit leaves room for base64 (x4/3) and the JSON around it inside 2 MiB
        assert!(MAX_FILE_WRITE_BYTES * 4 / 3 + 1024 < 2 * 1024 * 1024);
    }

    #[test]
    fn the_terminal_look_is_taken_from_this_terminal() {
        assert_eq!(pty_look_query(None, None, false), "");
        assert_eq!(pty_look_query(Some("dumb"), None, false), "");
        assert_eq!(
            pty_look_query(Some("xterm-256color"), Some("truecolor"), false),
            "&term=xterm-256color&colorterm=truecolor"
        );
        // an unknown COLORTERM value is not forwarded; the shell never sees raw client text
        assert_eq!(
            pty_look_query(Some("a b"), Some("rm"), true),
            "&term=a%20b&plain=true"
        );
    }

    #[test]
    fn the_terminal_url_follows_the_server_scheme() {
        let id = Uuid::nil();
        assert_eq!(
            pty_url("http://127.0.0.1:7788", id, 100, 30, None),
            format!("ws://127.0.0.1:7788/v1/agents/sessions/{id}/pty?cols=100&rows=30")
        );
        assert!(pty_url("https://h", id, 80, 24, Some("ls -l")).starts_with("wss://h/"));
        assert!(pty_url("https://h", id, 80, 24, Some("ls -l")).ends_with("&command=ls%20-l"));
    }

    #[test]
    fn a_trajectory_becomes_one_trace() {
        let traj = json!({
            "session": {"id": "4de0d044-b7a3-46e3-b2c7-f6279110cabd", "agent": "custom",
                        "backend": "vz", "state": "running", "sandbox_id": "x",
                        "created_at": "2026-10-13T09:30:00Z"},
            "steps": [
                {"seq": 0, "ts": "2026-10-13T09:30:10Z", "source": "vm", "kind": "vm.create", "detail": {"name": "a"}},
                {"seq": 1, "ts": "2026-10-13T09:30:12.500Z", "source": "ledger", "kind": "exec",
                 "detail": {"command": "ls", "exit_code": 2, "duration_ms": 500}},
                {"seq": 2, "ts": "2026-10-13T09:30:13Z", "source": "ledger", "kind": "exec.refused", "detail": {"command": "rm"}},
            ],
        });
        let doc = otlp_traces(&traj).unwrap();
        let spans = doc["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap();
        assert_eq!(spans.len(), 4, "root + 3 steps");
        assert_eq!(spans[0]["name"], "agent.session");
        assert_eq!(spans[0]["traceId"], "4de0d044b7a346e3b2c7f6279110cabd");
        // Every child hangs off the root and shares the trace.
        for s in &spans[1..] {
            assert_eq!(s["parentSpanId"], spans[0]["spanId"]);
            assert_eq!(s["traceId"], spans[0]["traceId"]);
            assert_eq!(s["spanId"].as_str().unwrap().len(), 16);
        }
        // The exec ended at its stamp and started duration_ms earlier; non-zero exit is an error.
        let exec = &spans[2];
        let end: i128 = exec["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
        let start: i128 = exec["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
        assert_eq!(end - start, 500_000_000);
        assert_eq!(exec["status"]["code"], 2);
        assert_eq!(
            spans[3]["status"]["code"], 2,
            "a refused command is an error"
        );
        assert_eq!(spans[1]["status"]["code"], 0);
        // The root covers every step.
        let root_start: i128 = spans[0]["startTimeUnixNano"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(root_start <= start);
        let ids: std::collections::HashSet<_> = spans
            .iter()
            .map(|s| s["spanId"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 4, "span ids are unique");
        assert!(otlp_traces(&json!({"session": {"id": "nope"}, "steps": []})).is_err());
    }

    #[test]
    fn volume_specs_become_runtime_volumes() {
        let mut b = json!({"agent": "custom", "runtime": {"image": "x"}});
        add_volumes(&mut b, &[]).unwrap();
        assert!(b["runtime"].get("volumes").is_none());
        add_volumes(&mut b, &["data:/mnt/data".into()]).unwrap();
        assert_eq!(b["runtime"]["volumes"][0]["name"], "data");
        assert_eq!(b["runtime"]["volumes"][0]["path"], "/mnt/data");
        assert_eq!(b["runtime"]["image"], "x");
        assert!(add_volumes(&mut b, &["nopath".into()]).is_err());
        assert!(add_volumes(&mut b, &["a:relative".into()]).is_err());
        let mut c = json!({"agent": "custom"});
        add_volumes(&mut c, &["v:/data".into()]).unwrap();
        assert_eq!(c["runtime"]["volumes"][0]["path"], "/data");
    }

    #[test]
    fn command_rules_land_under_policy_process() {
        let mut b = json!({"agent": "custom"});
        add_process_rules(&mut b, &[], &[]);
        assert!(b.get("policy").is_none());
        add_process_rules(&mut b, &["git *".into()], &["git push*".into()]);
        assert_eq!(b["policy"]["process"]["allow"][0], "git *");
        assert_eq!(b["policy"]["process"]["deny"][0], "git push*");
    }

    #[test]
    fn confinement_flags_land_under_policy_filesystem_and_only_when_asked() {
        let mut b = json!({"agent": "custom"});
        add_filesystem(&mut b, false, &[], &[], false);
        assert!(b.get("policy").is_none());
        add_filesystem(&mut b, true, &[], &[], false);
        assert_eq!(b["policy"]["filesystem"], json!({}));
        let mut c = json!({"agent": "custom", "policy": {"preset": "offline"}});
        add_filesystem(&mut c, true, &["/data".into()], &["/out".into()], true);
        assert_eq!(c["policy"]["preset"], "offline");
        assert_eq!(c["policy"]["filesystem"]["read"][0], "/data");
        assert_eq!(c["policy"]["filesystem"]["write"][0], "/out");
        assert_eq!(c["policy"]["filesystem"]["enforcement"], "best-effort");
    }

    #[test]
    fn the_winner_passes_first_then_is_fastest_and_nothing_failing_is_offered() {
        let o = |c: Option<i64>, t: Option<i64>, ms: u64| Outcome {
            candidate_exit: c,
            test_exit: t,
            had_test: true,
            duration_ms: ms,
        };
        // 1 and 3 pass; 3 is faster. 0 failed its test, 2 failed to run cleanly.
        let all = [
            o(Some(0), Some(1), 10),
            o(Some(0), Some(0), 900),
            o(Some(2), None, 5),
            o(Some(0), Some(0), 400),
        ];
        assert_eq!(pick_winner(&all), Some(3));
        assert_eq!(all[0].score(), 10);
        assert_eq!(all[2].score(), 0);
        assert_eq!(all[3].score(), 100);
        // Ties on score and time go to the lowest index.
        let tie = [o(Some(0), Some(0), 7), o(Some(0), Some(0), 7)];
        assert_eq!(pick_winner(&tie), Some(0));
        // Nothing passed: no winner, so nothing is offered for promotion.
        assert_eq!(
            pick_winner(&[o(Some(0), Some(1), 1), o(None, None, 1)]),
            None
        );
        // No test command: exiting 0 is passing.
        let no_test = Outcome {
            candidate_exit: Some(0),
            test_exit: None,
            had_test: false,
            duration_ms: 3,
        };
        assert!(no_test.passed());
        assert_eq!(pick_winner(&[no_test]), Some(0));
    }

    #[test]
    fn budget_flags_land_under_budget_and_only_when_given() {
        let mut b = json!({"agent": "custom"});
        add_budget(&mut b, [("max_execs", None), ("max_jobs", None)]);
        assert!(b.get("budget").is_none());
        add_budget(&mut b, [("max_execs", Some(50)), ("max_children", Some(4))]);
        assert_eq!(b["budget"]["max_execs"], 50);
        assert_eq!(b["budget"]["max_children"], 4);
        assert!(b["budget"].get("max_jobs").is_none());
    }

    #[test]
    fn promote_body_carries_only_what_was_asked_for() {
        assert_eq!(promote_body(false, None, false, &[]), json!({}));
        let b = promote_body(true, Some("abc".into()), true, &["/workspace/src".into()]);
        assert_eq!(b["dry_run"], true);
        assert_eq!(b["plan_id"], "abc");
        assert_eq!(b["skip_conflicts"], true);
        assert_eq!(b["paths"][0], "/workspace/src");
    }

    #[test]
    fn run_flags_become_the_create_body() {
        let b = create_body(
            "claude-code".into(),
            Some("https://github.com/zyvorai/zyvor-fluxvm".into()),
            None,
            Some("agent-micro".into()),
            Some(2),
            None,
            Some("developer-safe".into()),
            vec!["example.com".into()],
            Some(600),
        );
        assert_eq!(b["agent"], "claude-code");
        assert_eq!(
            b["workspace"]["git"],
            "https://github.com/zyvorai/zyvor-fluxvm"
        );
        assert_eq!(b["runtime"]["image"], "agent-micro");
        assert_eq!(b["runtime"]["cpus"], 2);
        assert!(b["runtime"].get("memory_mb").is_none());
        assert_eq!(b["policy"]["preset"], "developer-safe");
        assert_eq!(b["policy"]["network"]["default"], "deny");
        assert_eq!(b["ttl_seconds"], 600);
        // Nothing asked for, nothing sent.
        let min = create_body(
            "custom".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            vec![],
            None,
        );
        assert_eq!(min, json!({"agent": "custom"}));
    }
}
