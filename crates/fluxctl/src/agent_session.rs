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
    /// Fork into COUNT child sessions (in-tree KVM engine only; refused elsewhere with the reason).
    /// REST: `POST /v1/agents/sessions/{id}/fork`.
    Fork {
        id: Uuid,
        #[arg(long, default_value_t = 1)]
        count: u32,
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
    /// Delete the session and its sandbox. REST: `DELETE /v1/agents/sessions/{id}`.
    Rm { id: Uuid },
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
            let mut body = json!({ "command": command.join(" ") });
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
        AgentCommand::Fork { id, count } => show(
            &r.call(
                Method::POST,
                &format!("/v1/agents/sessions/{id}/fork"),
                Some(json!({ "count": count })),
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
        AgentCommand::Artifacts { id } => show(
            &r.call(
                Method::GET,
                &format!("/v1/agents/sessions/{id}/artifacts"),
                None,
            )
            .await
            .context("artifacts")?,
        ),
        AgentCommand::Rm { id } => {
            r.call(Method::DELETE, &format!("/v1/agents/sessions/{id}"), None)
                .await?;
            println!("{}", json!({"ok": true, "deleted": id}));
            Ok(())
        }
    }
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
