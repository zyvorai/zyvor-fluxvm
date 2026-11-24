// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl self-test`: a quick end-to-end smoke check of this install (probes live in `fluxvm_core::self_test`, shared
//! with `POST /v1/self-test` and the Studio Home card). `--fix` runs the same safe repairs as `fluxctl fix` (never
//! sudo, never a security setting) and then checks again.

use crate::cluster_cmd::{self, Env};
use anyhow::Result;
use fluxvm_core::health;
use fluxvm_core::self_test::{self, Check, DaemonProbe, Inputs, Report, Status};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

#[derive(clap::Args, Clone)]
pub struct SelfTestArgs {
    /// Machine-readable output (the same document as `GET /v1/self-test`).
    #[arg(long)]
    pub json: bool,
    /// Apply the safe fixes `fluxctl fix` knows (start the service, create a missing folder), then check again. Never
    /// runs sudo and never changes a security setting.
    #[arg(long)]
    pub fix: bool,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn base(listen: &str) -> String {
    if listen.starts_with("http://") || listen.starts_with("https://") {
        listen.trim_end_matches('/').to_owned()
    } else {
        format!("http://{}", listen.trim_end_matches('/'))
    }
}

async fn probe_daemon(env: &Env, token: Option<&str>) -> DaemonProbe {
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(h) => h,
        Err(e) => {
            return DaemonProbe {
                reachable: Err(e.to_string()),
                scheduler: None,
            };
        }
    };
    let url = format!("{}/healthz", base(&env.listen));
    match http.get(&url).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            return DaemonProbe {
                reachable: Err(format!("{url} answered {}", r.status())),
                scheduler: None,
            };
        }
        Err(_) => {
            return DaemonProbe {
                reachable: Err(format!("nothing is listening on {}", env.listen)),
                scheduler: None,
            };
        }
    }
    let mut req = http.get(format!("{}/v1/vms", base(&env.listen)));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let scheduler = match req.send().await {
        Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
            Ok(v) => Ok(v["items"].as_array().map_or(0, Vec::len)),
            Err(e) => Err(format!("unreadable answer: {e}")),
        },
        Ok(r) => Err(format!("answered {}", r.status())),
        Err(e) => Err(e.to_string()),
    };
    DaemonProbe {
        reachable: Ok(()),
        scheduler: Some(scheduler),
    }
}

async fn probe(env: &Env, docker_sock: &Option<PathBuf>, token: Option<&str>) -> Report {
    let daemon = probe_daemon(env, token).await;
    let inp = Inputs {
        state_dir: env.state_dir.clone(),
        docker_sock: docker_sock.clone(),
        agent_label: crate::launch_agent::LABEL.to_owned(),
        daemon,
    };
    tokio::task::spawn_blocking(move || self_test::run(&inp, now()))
        .await
        .unwrap_or_else(|e| {
            self_test::summarize(
                vec![Check {
                    id: "self_test".into(),
                    name: "Self-test".into(),
                    status: Status::Fail,
                    detail: format!("the probes crashed: {e}"),
                    what_to_do: Some("Report this with `fluxctl support-bundle`.".into()),
                    explain: None,
                }],
                now(),
            )
        })
}

/// The safe fixes of `fluxctl fix`, nothing else. Returns what was done, one line each.
async fn apply_safe_fixes(env: &Env) -> Vec<String> {
    let node = cluster_cmd::gather_local(env).await;
    let h = health::assess(&[node], now());
    let plan = cluster_cmd::plan(&h);
    let applied = cluster_cmd::apply(&plan, false, &cluster_cmd::RealApplier);
    if !applied.is_empty() {
        let _ = cluster_cmd::record(&cluster_cmd::journal_path(&env.state_dir), &applied);
    }
    applied
        .iter()
        .map(|a| {
            format!(
                "{} {}{}",
                if a.ok { "done:" } else { "FAILED:" },
                a.what,
                if a.ok {
                    String::new()
                } else {
                    format!(" ({})", a.output)
                }
            )
        })
        .collect()
}

pub async fn run(
    a: &SelfTestArgs,
    env: &Env,
    docker_sock: Option<PathBuf>,
    token: Option<&str>,
) -> Result<i32> {
    let mut report = probe(env, &docker_sock, token).await;
    let mut fixed = vec![];
    if a.fix && report.status != Status::Pass {
        fixed = apply_safe_fixes(env).await;
        if !fixed.is_empty() {
            // A started service needs a moment before it answers.
            tokio::time::sleep(Duration::from_secs(3)).await;
            report = probe(env, &docker_sock, token).await;
        }
    }
    if a.json {
        let mut v = serde_json::to_value(&report)?;
        if a.fix {
            v["fixed"] = serde_json::json!(fixed);
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        for l in &fixed {
            println!("{l}");
        }
        print!("{}", render(&report, color_on()));
    }
    Ok(report.exit_code())
}

fn color_on() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn word(s: Status, color: bool) -> String {
    let code = match s {
        Status::Pass => "32",
        Status::Warn => "33",
        Status::Fail => "31",
        Status::Skip => "2",
    };
    if color {
        format!("\x1b[{code}m{}\x1b[0m", s.word())
    } else {
        s.word().to_owned()
    }
}

pub fn render(r: &Report, color: bool) -> String {
    let w = r
        .checks
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for c in &r.checks {
        out += &format!("{}  {:<w$}  {}\n", word(c.status, color), c.name, c.detail);
    }
    let todo: Vec<&Check> = r
        .checks
        .iter()
        .filter(|c| c.what_to_do.is_some() && matches!(c.status, Status::Warn | Status::Fail))
        .collect();
    if !todo.is_empty() {
        out += "\nWhat to do:\n";
        for c in todo {
            out += &format!("  {}: {}", c.name, c.what_to_do.as_deref().unwrap_or(""));
            if let Some(e) = &c.explain {
                out += &format!(" (more: `fluxctl explain {e}`)");
            }
            out.push('\n');
        }
    }
    out += &format!("\n{}: {}\n", word(r.status, color), r.headline);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        self_test::summarize(
            vec![
                Check {
                    id: "a".into(),
                    name: "Disk space".into(),
                    status: Status::Pass,
                    detail: "50 GiB free".into(),
                    what_to_do: None,
                    explain: None,
                },
                Check {
                    id: "b".into(),
                    name: "Docker socket".into(),
                    status: Status::Fail,
                    detail: "no answer".into(),
                    what_to_do: Some("Restart the daemon.".into()),
                    explain: Some("DOCKER_SOCKET_DOWN".into()),
                },
            ],
            1,
        )
    }

    #[test]
    fn table_has_a_word_per_check_and_a_plain_what_to_do() {
        let t = render(&report(), false);
        assert!(t.contains("PASS  Disk space"), "{t}");
        assert!(t.contains("FAIL  Docker socket"), "{t}");
        assert!(t.contains("What to do:"));
        assert!(t.contains("Restart the daemon. (more: `fluxctl explain DOCKER_SOCKET_DOWN`)"));
        assert!(!t.contains('\x1b'));
    }

    #[test]
    fn a_clean_report_has_no_what_to_do_section() {
        let r = self_test::summarize(vec![report().checks[0].clone()], 1);
        let t = render(&r, false);
        assert!(!t.contains("What to do"));
        assert!(t.contains("All 1 checks passed."));
    }

    #[test]
    fn json_and_exit_code() {
        let r = report();
        assert_eq!(r.exit_code(), 1);
        assert_eq!(serde_json::to_value(&r).unwrap()["failed"], 1);
        assert_eq!(base("127.0.0.1:7878"), "http://127.0.0.1:7878");
        assert_eq!(base("https://h:1/"), "https://h:1");
    }
}
