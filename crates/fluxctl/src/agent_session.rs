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
            ttl_seconds,
        } => show(
            &r.call(
                Method::POST,
                "/v1/agents/sessions",
                Some(create_body(
                    agent,
                    repo,
                    workspace_path,
                    image,
                    cpus,
                    memory_mb,
                    policy,
                    allow,
                    ttl_seconds,
                )),
            )
            .await?,
        ),
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
