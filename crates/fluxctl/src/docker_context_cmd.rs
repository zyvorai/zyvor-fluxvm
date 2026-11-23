// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl docker context install|uninstall|status`: the Docker CLI context named `fluxvm` that points `docker` at the engine
//! `fluxctl docker up` runs. The logic and file formats live in `fluxvm_api::docker_context`; this adds the terminal output.
//! `docker up` calls [`auto_install`] when the engine is ready, so `docker --context fluxvm ps` works with no extra step.

use crate::docker_host;
use anyhow::{Result, bail};
use fluxvm_api::docker_context::{self as ctx, NAME};
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug, Clone, Default)]
pub struct ContextArgs {
    /// The engine socket the context points at (default: the one `fluxctl docker up` publishes).
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Point at the daemon's built-in Docker socket instead (`docker build` there is forwarded to the Mac builder).
    #[arg(long, conflicts_with = "socket")]
    pub builtin: bool,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum ContextCommand {
    /// Create (or refresh) the `fluxvm` docker context so `docker --context fluxvm ps` reaches FluxVM's engine. It does not
    /// change your current context unless you add `--use`. Nothing else in ~/.docker is touched.
    Install {
        #[command(flatten)]
        args: ContextArgs,
        /// Also make `fluxvm` the context `docker` uses by default (`docker context use fluxvm`).
        #[arg(long = "use")]
        use_current: bool,
    },
    /// Remove the `fluxvm` context. If it was current, `docker` goes back to its default context.
    Uninstall,
    /// Fix a `fluxvm` context whose socket is dead: point it at the FluxVM socket that answers (the engine from `fluxctl docker
    /// up`, or the daemon's built-in one). Does nothing when the context already works.
    Repair,
    /// Whether the context exists, is current, and the engine socket answers; says what to run next.
    Status {
        #[command(flatten)]
        args: ContextArgs,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

fn socket_of(a: &ContextArgs) -> PathBuf {
    if a.builtin {
        return docker_host::builtin_socket();
    }
    a.socket.clone().unwrap_or_else(docker_host::default_socket)
}

/// What `repair` did, in one sentence (and an error when nothing can be fixed).
fn repair(dir: &Path) -> Result<String> {
    let meta = ctx::meta_path(dir);
    let host = std::fs::read_to_string(&meta)
        .ok()
        .and_then(|t| ctx::host_in_meta(&t));
    let first_builtin = host
        .as_deref()
        .and_then(ctx::socket_of_host)
        .is_some_and(|p| p.file_name().is_some_and(|n| n == "docker.sock"));
    let mut cands = vec![docker_host::default_socket(), docker_host::builtin_socket()];
    if first_builtin {
        cands.reverse();
    }
    Ok(
        match ctx::plan_repair(host.as_deref(), &|s| docker_host::ping(s), &cands) {
            ctx::Repair::NoContext => format!(
                "there is no docker context `{NAME}` to repair. Create it with `fluxctl docker context install`"
            ),
            ctx::Repair::Foreign(h) => format!(
                "docker context `{NAME}` points at {h}, not at a FluxVM socket, so it was left alone. Reset it with `fluxctl docker context install`"
            ),
            ctx::Repair::Healthy(s) => format!(
                "docker context `{NAME}` already works ({}); nothing to repair",
                s.display()
            ),
            ctx::Repair::Repoint { from, to } => {
                let i = ctx::install(dir, &to, false)?;
                format!(
                    "docker context `{NAME}` pointed at {}, which does not answer. It now points at {} ({}). Try: docker --context {NAME} ps",
                    from.display(),
                    to.display(),
                    i.host
                )
            }
            ctx::Repair::NothingAnswers { from } => bail!(
                "docker context `{NAME}` points at {}, which does not answer, and no other FluxVM Docker socket answers either. \
             Start one: `fluxctl docker up` for the full engine, or start the daemon (`fluxvmd`) for the built-in socket, then run `fluxctl docker context repair` again",
                from.display()
            ),
        },
    )
}

fn cli_present() -> bool {
    ctx::docker_cli_in(std::env::var_os("PATH").as_deref()).is_some()
}

fn no_config_dir() -> anyhow::Error {
    anyhow::anyhow!(
        "cannot tell where the Docker CLI keeps its settings: neither DOCKER_CONFIG nor HOME is set. Set DOCKER_CONFIG to a folder \
         (for example `export DOCKER_CONFIG=$HOME/.docker`) and run `fluxctl docker context install` again"
    )
}

/// The one-line result of an install, for the terminal.
pub fn install_line(i: &ctx::Installed) -> String {
    let what = if i.changed {
        "created"
    } else {
        "already set up"
    };
    let cur = if i.made_current {
        "; `docker` now uses it by default"
    } else {
        ""
    };
    format!(
        "docker context `{NAME}` {what} ({}){cur}. Try: docker --context {NAME} ps",
        i.host
    )
}

/// The line for a machine with no Docker CLI.
pub fn no_cli_line() -> String {
    format!(
        "the docker command is not installed here, so no docker context was made; {}. Then run `fluxctl docker context install`",
        ctx::install_hint()
    )
}

fn install(dir: &Path, socket: &Path, use_current: bool) -> Result<String> {
    Ok(install_line(&ctx::install(dir, socket, use_current)?))
}

/// What `docker up` does at its end: make the context (never switching the current one), print one line, and never fail the `up`.
pub fn auto_install(socket: &Path) {
    if !cli_present() {
        eprintln!("note: {}", no_cli_line());
        return;
    }
    let Some(dir) = ctx::config_dir_from_env() else {
        eprintln!("note: {}", no_config_dir());
        return;
    };
    match install(&dir, socket, false) {
        Ok(line) => eprintln!("{line}"),
        Err(e) => eprintln!("note: the docker context was not made: {e}"),
    }
}

fn status_text(v: &serde_json::Value) -> String {
    let mut out = format!(
        "{}\nstate: {}\nsocket: {} ({})\n",
        v["summary"].as_str().unwrap_or(""),
        v["state"].as_str().unwrap_or(""),
        v["socket"].as_str().unwrap_or(""),
        if v["socket_answers"] == true {
            "answers"
        } else {
            "does not answer"
        }
    );
    out.push_str(&format!(
        "context: {}, {}\n",
        if v["exists"] == true {
            "exists"
        } else {
            "not created"
        },
        if v["current"] == true {
            "current"
        } else {
            "not current"
        }
    ));
    if let Some(n) = v["next"].as_str() {
        out.push_str(&format!("next: {n}\n"));
    }
    if matches!(v["state"].as_str(), Some("stale" | "socket-down")) {
        out.push_str("or, if another FluxVM socket answers: fluxctl docker context repair\n");
    }
    out
}

pub fn run(command: ContextCommand) -> Result<()> {
    match command {
        ContextCommand::Install { args, use_current } => {
            if !cli_present() {
                println!("{}", no_cli_line());
                return Ok(());
            }
            let Some(dir) = ctx::config_dir_from_env() else {
                bail!("{}", no_config_dir());
            };
            println!("{}", install(&dir, &socket_of(&args), use_current)?);
            if args.builtin {
                println!(
                    "note: the docker command builds with BuildKit by default, which this socket does not serve. Use `DOCKER_BUILDKIT=0 docker build -t NAME:TAG .` \
                     (or `export DOCKER_BUILDKIT=0`), or `fluxctl mac build -t NAME:TAG .`"
                );
            }
        }
        ContextCommand::Repair => {
            let Some(dir) = ctx::config_dir_from_env() else {
                bail!("{}", no_config_dir());
            };
            println!("{}", repair(&dir)?);
        }
        ContextCommand::Uninstall => {
            let Some(dir) = ctx::config_dir_from_env() else {
                bail!("{}", no_config_dir());
            };
            let u = ctx::uninstall(&dir)?;
            println!(
                "{}",
                match (u.removed, u.reset_current) {
                    (false, _) => format!("there was no docker context `{NAME}`; nothing to do"),
                    (true, true) => format!(
                        "docker context `{NAME}` removed; `docker` is back on its default context"
                    ),
                    (true, false) => format!("docker context `{NAME}` removed"),
                }
            );
        }
        ContextCommand::Status { args, json } => {
            let socket = socket_of(&args);
            let dir = ctx::config_dir_from_env();
            let (ec, eh) = (
                std::env::var("DOCKER_CONTEXT").ok(),
                std::env::var("DOCKER_HOST").ok(),
            );
            let v = ctx::status(&ctx::StatusInput {
                dir: dir.as_deref(),
                socket: &socket,
                socket_answers: docker_host::ping(&socket),
                docker_cli: cli_present(),
                env_context: ec.as_deref(),
                env_host: eh.as_deref(),
            });
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", status_text(&v));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_line_says_what_happened_and_how_to_try_it() {
        let mut i = ctx::Installed {
            host: "unix:///s".into(),
            meta: PathBuf::from("/m"),
            changed: true,
            made_current: false,
        };
        let l = install_line(&i);
        assert!(l.contains("created") && l.contains("docker --context fluxvm ps"));
        assert!(!l.contains("by default"));
        i.changed = false;
        i.made_current = true;
        let l = install_line(&i);
        assert!(l.contains("already set up") && l.contains("by default"));
    }

    #[test]
    fn no_cli_line_has_the_install_hint() {
        assert!(no_cli_line().contains("not installed") && no_cli_line().contains("install"));
    }

    #[test]
    fn status_text_names_the_next_command() {
        let v = serde_json::json!({"summary":"s","state":"socket-down","socket":"/x","socket_answers":false,"exists":true,"current":false,"next":"fluxctl docker up"});
        let t = status_text(&v);
        assert!(t.contains("does not answer") && t.contains("next: fluxctl docker up"));
        assert!(t.contains("exists, not current"));
        assert!(t.contains("fluxctl docker context repair"));
    }
}

#[cfg(test)]
mod repair_tests {
    use super::*;

    #[test]
    fn repair_with_no_context_says_how_to_make_one() {
        let d = tempfile::tempdir().unwrap();
        let t = repair(d.path()).unwrap();
        assert!(t.contains("no docker context") && t.contains("fluxctl docker context install"));
    }

    #[test]
    fn repair_of_a_dead_socket_explains_what_to_start() {
        let d = tempfile::tempdir().unwrap();
        ctx::install(d.path(), Path::new("/nonexistent-dir/dead.sock"), false).unwrap();
        let e = repair(d.path()).unwrap_err().to_string();
        assert!(
            e.contains("does not answer") && e.contains("fluxctl docker up"),
            "{e}"
        );
    }

    #[test]
    fn builtin_socket_sits_next_to_the_engine_socket() {
        assert_eq!(
            docker_host::builtin_socket().parent(),
            docker_host::default_socket().parent()
        );
        assert!(docker_host::builtin_socket().ends_with("docker.sock"));
    }
}
