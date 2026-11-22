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
    a.socket.clone().unwrap_or_else(docker_host::default_socket)
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
    }
}
