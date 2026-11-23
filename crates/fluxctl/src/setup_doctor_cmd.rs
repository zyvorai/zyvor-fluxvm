// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl doctor`: one command that checks the whole setup and fixes what it safely can.
//!
//! It adds no checks of its own to the machine. It runs what `fluxctl cluster status` already runs (the host doctors, the
//! daemon, Kubernetes, the Mac fleet, the gateway add-on), sorts the answer into a short green/yellow/red list
//! (`fluxvm_core::setup_doctor`, which `GET /v1/doctor` also uses), and adds the two things that model does not look at:
//! the Docker context and registry logins. `--fix` applies the same few safe fixes as `fluxctl fix` (never sudo, never a
//! security setting), the stale MCP paths the classic doctor repairs, and the missing Docker context. The per-check
//! report this command used to print is still there as `--classic` (and `--host-only`).

use crate::cluster_cmd::{self, Env};
use crate::mac_fleet::Conn;
use crate::support_cmd::{self, SupportBundleArgs};
use anyhow::Result;
use fluxvm_api::docker_context as ctx;
use fluxvm_core::config::Config;
use fluxvm_core::health::{self, Severity};
use fluxvm_core::setup_doctor::{self, HostFacts, SetupCheck, SetupReport};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(clap::Args, Clone, Default)]
pub struct More {
    /// Print the older per-check report of this host (daemon, binaries, KVM, disk) instead of the whole-setup list.
    #[arg(long)]
    pub classic: bool,
    /// Machine-readable output (what Studio reads from `GET /v1/doctor`).
    #[arg(long)]
    pub json: bool,
    /// Also check the Macs of this fleet inventory (YAML or JSON) over ssh, read-only.
    #[arg(long, short = 'f', value_name = "FILE", env = "FLUXVM_FLEET_INVENTORY")]
    pub inventory: Option<PathBuf>,
    /// If something is red, write a support bundle without asking.
    #[arg(long)]
    pub bundle: bool,
    /// ssh private key for Macs that do not set their own.
    #[arg(long, short = 'i', value_name = "FILE")]
    pub identity: Option<PathBuf>,
    /// Trust a fleet host key the first time it is seen.
    #[arg(long)]
    pub accept_new_hostkey: bool,
}

// ---- the lines the health model does not have ---------------------------------------------------------------------------

/// The Docker context line, from `fluxvm_api::docker_context::status`.
pub fn docker_line(v: &Value) -> (SetupCheck, bool) {
    let title = "Docker context";
    let state = v["state"].as_str().unwrap_or("");
    let summary = v["summary"].as_str().unwrap_or("").to_owned();
    match state {
        "current" | "ready" => (
            SetupCheck::ok(
                "docker",
                title,
                "`docker --context fluxvm ps` reaches the FluxVM engine.",
            ),
            false,
        ),
        // Not everyone uses the docker command: nothing to fix until they install it.
        "no-docker-cli" => (
            SetupCheck::ok(
                "docker",
                title,
                "The docker command is not installed here (optional); the context is made when you install it.",
            ),
            false,
        ),
        "socket-down" => (
            SetupCheck::problem(
                "docker",
                title,
                Severity::Warn,
                "The `fluxvm` docker context exists but the engine is not running.",
                Some("fluxctl docker up"),
                "Start the engine.",
            ),
            false,
        ),
        _ => {
            let mut c = SetupCheck::problem(
                "docker",
                title,
                Severity::Warn,
                summary,
                Some("fluxctl doctor --fix"),
                "This is safe to do for you.",
            );
            c.safe_fix = true;
            (c, true)
        }
    }
}

/// The registry logins line: a login file other users can read, or one that is damaged, is worth a yellow.
pub fn registry_line(logins: Result<Vec<(String, String)>, String>) -> SetupCheck {
    let title = "Registry logins";
    match logins {
        Ok(l) if l.is_empty() => SetupCheck::ok(
            "registry",
            title,
            "No registry logins saved. You only need one for private images (`fluxctl mac login REGISTRY`).",
        ),
        Ok(l) => SetupCheck::ok(
            "registry",
            title,
            format!(
                "Logged in to {}.",
                l.iter()
                    .map(|(r, u)| format!("{r} as {u}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Err(e) => SetupCheck::problem(
            "registry",
            title,
            Severity::Warn,
            format!("The saved registry logins cannot be used ({e})."),
            Some("fluxctl mac login REGISTRY"),
            "Log in again to replace the damaged login file.",
        ),
    }
}

fn docker_status() -> Value {
    let socket = crate::docker_host::default_socket();
    let dir = ctx::config_dir_from_env();
    let (ec, eh) = (
        std::env::var("DOCKER_CONTEXT").ok(),
        std::env::var("DOCKER_HOST").ok(),
    );
    ctx::status(&ctx::StatusInput {
        dir: dir.as_deref(),
        socket: &socket,
        socket_answers: crate::docker_host::ping(&socket),
        docker_cli: ctx::docker_cli_in(std::env::var_os("PATH").as_deref()).is_some(),
        env_context: ec.as_deref(),
        env_host: eh.as_deref(),
    })
}

fn registry_logins() -> Result<Vec<(String, String)>, String> {
    let path = crate::mac_login::default_path().map_err(|e| format!("{e:#}"))?;
    crate::mac_login::logins(&path).map_err(|e| format!("{e:#}"))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn conn(a: &More) -> Conn {
    Conn {
        identity: a.identity.clone(),
        known_hosts: None,
        accept_new_hostkey: a.accept_new_hostkey,
        jobs: 8,
        connect_timeout: 10,
        dry_run: false,
        json: false,
        step_timeout: 60,
    }
}

struct Snapshot {
    health: health::ClusterHealth,
    report: SetupReport,
    docker_fixable: bool,
}

async fn snapshot(a: &More, env: &Env) -> Result<Snapshot> {
    let nodes = cluster_cmd::gather(env, a.inventory.as_deref(), false, &conn(a)).await?;
    let h = health::assess_with(&nodes, cluster_cmd::gateway_items_now().await, now());
    let (docker, docker_fixable) = docker_line(&docker_status());
    let extras = vec![docker, registry_line(registry_logins())];
    let host = HostFacts {
        thermal_limit: tokio::task::spawn_blocking(fluxvm_apple::host_health::probe_thermal_limit)
            .await
            .unwrap_or(None),
        disk_free_gib: nodes.first().and_then(|n| n.disk_free_gib),
    };
    let report = setup_doctor::build(&h, &host, extras, nodes.len().saturating_sub(1), now());
    Ok(Snapshot {
        health: h,
        report,
        docker_fixable,
    })
}

// ---- fixing -----------------------------------------------------------------------------------------------------------

struct Done {
    what: String,
    ok: bool,
    detail: String,
}

fn fix_docker_context() -> Result<String> {
    let dir = ctx::config_dir_from_env()
        .ok_or_else(|| anyhow::anyhow!("cannot tell where the Docker CLI keeps its settings"))?;
    let i = ctx::install(&dir, &crate::docker_host::default_socket(), false)?;
    Ok(crate::docker_context_cmd::install_line(&i))
}

fn apply_safe(s: &Snapshot, state_dir: &std::path::Path) -> Vec<Done> {
    let mut out = vec![];
    // The same safe fixes as `fluxctl fix` (no sudo, no security settings), recorded in the same change log.
    let plan = cluster_cmd::plan(&s.health);
    let applied = cluster_cmd::apply(&plan, false, &cluster_cmd::RealApplier);
    let log = cluster_cmd::journal_path(state_dir);
    let _ = cluster_cmd::record(&log, &applied);
    out.extend(applied.into_iter().map(|a| Done {
        what: a.what,
        ok: a.ok,
        detail: a.output,
    }));
    if s.docker_fixable {
        let r = fix_docker_context();
        out.push(Done {
            what: "create the `fluxvm` docker context".into(),
            ok: r.is_ok(),
            detail: r.unwrap_or_else(|e| format!("{e:#}")),
        });
    }
    if let Ok(exe) = crate::stable_exe::stable_exe()
        && let Ok(fixed) = crate::mcp_install::repair_stale(false, "fluxvm", &exe)
    {
        for f in fixed {
            out.push(Done {
                what: format!(
                    "point the {} MCP entry at the current fluxctl",
                    f["client"].as_str().unwrap_or("client")
                ),
                ok: true,
                detail: format!(
                    "{} -> {}",
                    f["was"].as_str().unwrap_or("?"),
                    f["now"].as_str().unwrap_or("?")
                ),
            });
        }
    }
    out
}

// ---- the command ------------------------------------------------------------------------------------------------------------

fn color_on() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

pub async fn run(a: &More, fix: bool, env: &Env, cfg: &Config) -> Result<i32> {
    let mut snap = snapshot(a, env).await?;
    let mut done: Vec<Done> = vec![];
    if fix {
        done = apply_safe(&snap, &cfg.state_dir);
        // Look again so the report is the state the user is left with.
        if !done.is_empty() {
            snap = snapshot(a, env).await?;
        }
    }
    let r = &snap.report;
    let red = r.light == Severity::Fail;
    if a.json {
        let mut v = serde_json::to_value(r)?;
        v["ok"] = json!(!red);
        v["fixed"] = json!(
            done.iter()
                .map(|d| json!({"what": d.what, "ok": d.ok, "detail": d.detail}))
                .collect::<Vec<_>>()
        );
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        for d in &done {
            println!(
                "{} {}{}",
                if d.ok { "fixed:" } else { "could not:" },
                d.what,
                if d.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", d.detail)
                }
            );
        }
        if !done.is_empty() {
            println!();
        }
        print!("{}", setup_doctor::render(r, color_on()));
        if r.checks.iter().any(|c| c.safe_fix) && !fix {
            println!(
                "\n`fluxctl doctor --fix` applies the safe fixes above; nothing else is changed."
            );
        }
    }
    if red && r.offer_support_bundle && offer_bundle(a, env, cfg).await? && !a.json {
        println!();
    }
    Ok(i32::from(red))
}

/// Something is red: write the bundle now with `--bundle`, or ask when a person is at the terminal.
async fn offer_bundle(a: &More, env: &Env, cfg: &Config) -> Result<bool> {
    let go = if a.bundle {
        true
    } else if a.json || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        false
    } else {
        eprint!("\nSomething is red. Create a support bundle now? [y/N] ");
        std::io::stderr().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    if !go {
        return Ok(false);
    }
    support_cmd::run(
        &SupportBundleArgs {
            out: None,
            log_lines: None,
            json: false,
        },
        env,
        cfg,
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(state: &str) -> Value {
        json!({"state": state, "summary": "the summary"})
    }

    #[test]
    fn docker_states_map_to_lights() {
        for ok in ["current", "ready", "no-docker-cli"] {
            let (c, fixable) = docker_line(&st(ok));
            assert_eq!(c.light, Severity::Ok, "{ok}");
            assert!(!fixable);
        }
        for fixable_state in ["not-installed", "stale"] {
            let (c, fixable) = docker_line(&st(fixable_state));
            assert_eq!(c.light, Severity::Warn);
            assert!(fixable && c.safe_fix && c.command.as_deref() == Some("fluxctl doctor --fix"));
        }
        let (c, fixable) = docker_line(&st("socket-down"));
        assert!(!fixable && c.command.as_deref() == Some("fluxctl docker up"));
    }

    #[test]
    fn registry_lines() {
        assert_eq!(registry_line(Ok(vec![])).light, Severity::Ok);
        let c = registry_line(Ok(vec![("ghcr.io".into(), "alice".into())]));
        assert!(c.message.contains("ghcr.io as alice"));
        let bad = registry_line(Err("chmod 600 /x".into()));
        assert_eq!(bad.light, Severity::Warn);
        assert!(bad.message.contains("chmod 600"));
        assert!(bad.next.unwrap().contains("fluxctl mac login"));
    }

    #[test]
    fn registry_lines_never_print_a_secret() {
        // `logins` returns only registry and user; the line is built from those two.
        let c = registry_line(Ok(vec![("r.io".into(), "u".into())]));
        assert!(!c.message.contains("pass") && !c.message.contains("token"));
    }
}
