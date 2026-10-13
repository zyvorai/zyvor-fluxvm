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
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
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
    /// `agent logs`. No stdin, no TTY. REST: `POST .../jobs`.
    Bg {
        id: Uuid,
        /// The command: one shell line, or an argv after `--`.
        #[arg(trailing_var_arg = true, required = true, num_args = 1..)]
        command: Vec<String>,
    },
    /// Output of a background job. With `-f` keep reading until it exits and exit with its code.
    /// REST: `GET .../jobs/{job}?offset=N`.
    Logs {
        id: Uuid,
        job: Uuid,
        /// Follow until the job exits (polls about twice a second).
        #[arg(short = 'f', long)]
        follow: bool,
    },
    /// Stop a background job (SIGTERM to its process group). REST: `DELETE .../jobs/{job}`.
    Kill { id: Uuid, job: Uuid },
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
    /// Delete the session and its sandbox. REST: `DELETE /v1/agents/sessions/{id}`.
    Rm { id: Uuid },
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

pub async fn run(r: &Remote, cmd: AgentCommand) -> Result<()> {
    match cmd {
        AgentCommand::Run {
            agent,
            repo,
            workspace_path,
            image,
            cpus,
            memory_mb,
            policy,
            allow,
            volume,
            cmd_allow,
            cmd_deny,
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
        AgentCommand::Exec {
            id,
            timeout_seconds,
            command,
        } => {
            let mut body = json!({ "command": guest_command(&command) });
            if let Some(t) = timeout_seconds {
                body["timeout_seconds"] = json!(t);
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
        AgentCommand::Bg { id, command } => {
            let v = r
                .call(
                    Method::POST,
                    &format!("/v1/agents/sessions/{id}/jobs"),
                    Some(json!({"command": guest_command(&command)})),
                )
                .await
                .context("start job")?;
            show(&v)
        }
        AgentCommand::Logs { id, job, follow } => {
            use std::io::Write as _;
            let mut offset = 0u64;
            loop {
                let v = r
                    .call(
                        Method::GET,
                        &format!("/v1/agents/sessions/{id}/jobs/{job}?offset={offset}"),
                        None,
                    )
                    .await
                    .context("read job")?;
                let out = v["output"].as_str().unwrap_or("");
                print!("{out}");
                let _ = std::io::stdout().flush();
                offset = v["next_offset"].as_u64().unwrap_or(offset);
                let more = v["more"].as_bool() == Some(true);
                if let Some(code) = v["exit_code"].as_i64()
                    && !more
                {
                    eprintln!("[job exited {code}]");
                    std::process::exit(if follow { code.clamp(0, 255) as i32 } else { 0 });
                }
                if !follow && !more {
                    return Ok(());
                }
                if !more {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        AgentCommand::Kill { id, job } => {
            r.call(
                Method::DELETE,
                &format!("/v1/agents/sessions/{id}/jobs/{job}"),
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
