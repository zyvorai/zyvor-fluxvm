// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl agents [list] | restart NAME | repair [--dry-run]`: every FluxVM LaunchAgent on this Mac in one place, with a
//! plain-English health and what to do. Studio: Home, Background services. The logic is `fluxvm_core::launch_agents`.

use anyhow::{Result, bail};
use fluxvm_core::launch_agents::{self as la, Health};

#[derive(clap::Subcommand)]
pub enum AgentsCommand {
    /// List every FluxVM LaunchAgent: what it is, loaded or not, pid, last exit and what to do. Exit 1 when any needs attention.
    List {
        /// The raw document instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Restart one: its label or a short name (`daemon`, `kube`, `docker`, `node-agent`, `vmnet`, `usb`). An agent that is
    /// installed but not loaded is loaded instead.
    Restart { name: String },
    /// Print the end of one agent's log (the file its plist writes to): a label or short name, `-n` lines (default 50).
    Logs {
        name: String,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
    },
    /// Load every agent that is installed but not loaded. Never edits or deletes a plist, never uses sudo.
    Repair {
        /// Only say which would be loaded.
        #[arg(long)]
        dry_run: bool,
    },
}

fn home() -> Result<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(Into::into)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

fn word(h: Health) -> &'static str {
    match h {
        Health::Ok => "OK",
        Health::NotLoaded => "NOT LOADED",
        Health::Crashing => "FAILING",
        Health::StaleProgram => "STALE",
    }
}

/// Exit code: 0 healthy, 1 something needs attention.
pub fn run(cmd: Option<AgentsCommand>) -> Result<i32> {
    if !cfg!(target_os = "macos") {
        bail!("LaunchAgents exist only on macOS; on Linux use `systemctl --user status fluxvm`");
    }
    let home = home()?;
    match cmd.unwrap_or(AgentsCommand::List { json: false }) {
        AgentsCommand::List { json } => {
            let r = la::list(&home, &la::System);
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!("{}", r.headline);
                for a in &r.agents {
                    let run = a
                        .pid
                        .map(|p| format!("pid {p}"))
                        .unwrap_or_else(|| "not running".into());
                    println!(
                        "  {:<11} {:<38} {}  ({})",
                        word(a.health),
                        a.name,
                        run,
                        a.label
                    );
                    if let Some(w) = &a.what_to_do {
                        println!("              {w}");
                    }
                }
            }
            Ok(i32::from(r.agents.iter().any(|a| a.health != Health::Ok)))
        }
        AgentsCommand::Restart { name } => match la::restart(&home, &la::System, &name) {
            Ok(label) => {
                println!("restarted {label}");
                Ok(0)
            }
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        },
        AgentsCommand::Logs { name, lines } => match la::log_tail(&home, &la::System, &name, lines)
        {
            Ok(t) => {
                println!("{t}");
                Ok(0)
            }
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        },
        AgentsCommand::Repair { dry_run } => {
            let r = la::repair(&home, &la::System, dry_run);
            if r.is_empty() {
                println!("nothing to repair: every installed agent is loaded");
            }
            for x in &r {
                println!(
                    "{} {}: {}",
                    if x.ok { "ok  " } else { "FAIL" },
                    x.label,
                    x.detail
                );
            }
            Ok(i32::from(r.iter().any(|x| !x.ok)))
        }
    }
}
