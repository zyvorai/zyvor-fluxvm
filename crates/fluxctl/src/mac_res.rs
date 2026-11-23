// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Clap surface and dispatch for `fluxctl mac volume ...` and `fluxctl mac network ...` (logic in `mac_volume` and `mac_net`).

use crate::mac_volume::{self as vol, VolumeStore};
use anyhow::{Result, ensure};
use clap::Subcommand;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum MacRes {
    /// Named volumes: a folder on this Mac kept after the instance is gone and shared into guests with `-v NAME:/path`. See docs/macos-containers.md.
    Volume {
        #[command(subcommand)]
        command: VolumeCommand,
    },
    /// The network instances are on. A macOS guest has one NAT network; `inspect` says what it does and does not do.
    Network {
        #[command(subcommand)]
        command: NetworkCommand,
    },
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum VolumeCommand {
    /// Make a volume (nothing happens if it exists). `mac run -v NAME:/path` also makes it on first use. Does not need the daemon.
    Create {
        /// Letters, digits, `_`, `.`, `-`. Default: a random name.
        name: Option<String>,
        #[arg(long = "label", value_name = "KEY=VALUE")]
        label: Vec<String>,
    },
    /// List volumes with their size and the instances that hold them.
    Ls {
        #[arg(long)]
        json: bool,
        /// Only the names.
        #[arg(short = 'q', long)]
        quiet: bool,
    },
    /// Details of volumes as JSON: where the folder is, size, labels, instances using it.
    Inspect {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Delete volumes and their data. Refused while any instance, running or stopped, uses one.
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
        /// No question; a volume that does not exist is not an error.
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Delete every volume no instance uses. `--dry-run` shows which and how much room it frees.
    Prune {
        #[arg(long)]
        dry_run: bool,
        #[arg(short = 'f', long)]
        force: bool,
    },
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum NetworkCommand {
    /// The networks (there is one: `nat`) and how many instances are on it.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// What is on the network and what it does not support. Defaults to `nat`.
    Inspect {
        #[arg(default_value = "nat")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Not supported on macOS guests; says why and what to do instead.
    #[command(hide = true)]
    Create {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Not supported on macOS guests.
    #[command(hide = true)]
    Rm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Not supported on macOS guests.
    #[command(hide = true)]
    Connect {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Not supported on macOS guests.
    #[command(hide = true)]
    Disconnect {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Not supported on macOS guests.
    #[command(hide = true)]
    Prune {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

impl MacRes {
    /// Only `volume create` and the refusals of `network` work without the daemon.
    pub fn is_client_side(&self) -> bool {
        match self {
            MacRes::Volume { command } => matches!(command, VolumeCommand::Create { .. }),
            MacRes::Network { command } => !matches!(
                command,
                NetworkCommand::Ls { .. } | NetworkCommand::Inspect { .. }
            ),
        }
    }
}

pub fn run_client(cmd: MacRes) -> Result<()> {
    match cmd {
        MacRes::Volume {
            command: VolumeCommand::Create { name, label },
        } => {
            let name =
                name.unwrap_or_else(|| format!("vol-{}", &crate::mac_cmd::random_name()[4..]));
            let mut labels = BTreeMap::new();
            for l in &label {
                let (k, v) = crate::mac_instance::parse_label(l)?;
                labels.insert(k, v);
            }
            let (v, _) = VolumeStore::open_default()?.create(&name, labels)?;
            println!("{}", v.meta.name);
            Ok(())
        }
        MacRes::Network { command } => {
            let verb = match command {
                NetworkCommand::Create { .. } => "create",
                NetworkCommand::Rm { .. } => "rm",
                NetworkCommand::Connect { .. } => "connect",
                NetworkCommand::Disconnect { .. } => "disconnect",
                NetworkCommand::Prune { .. } => "prune",
                _ => anyhow::bail!("not a client-side command"),
            };
            crate::mac_net::refuse(verb)
        }
        _ => anyhow::bail!("not a client-side command"),
    }
}

pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacRes) -> Result<()> {
    match cmd {
        MacRes::Network { command } => {
            let items = crate::mac_more::instances(r).await?;
            let refs: Vec<&Value> = items.iter().collect();
            match command {
                NetworkCommand::Ls { json } => print!("{}", crate::mac_net::list(&refs, json)),
                NetworkCommand::Inspect { name, json } => {
                    let v = crate::mac_net::inspect(&name, &refs)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&v)?);
                    } else {
                        print!("{}", crate::mac_net::render_inspect(&v));
                    }
                }
                _ => anyhow::bail!("internal: handled client side"),
            }
            Ok(())
        }
        MacRes::Volume { command } => volume(r, command).await,
    }
}

async fn volume(r: &crate::remote::Remote, cmd: VolumeCommand) -> Result<()> {
    let store = VolumeStore::open_default()?;
    let items = crate::mac_more::instances(r).await?;
    let refs: Vec<&Value> = items.iter().collect();
    let vols = store.list()?;
    let used = vol::users(&refs, &vols);
    match cmd {
        VolumeCommand::Ls { json, quiet } => {
            if json {
                let rows: Vec<Value> = vols
                    .iter()
                    .map(|v| vol::inspect_json(v, &used[&v.meta.name]))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if quiet {
                for v in &vols {
                    println!("{}", v.meta.name);
                }
            } else {
                print!("{}", vol::render_ls(&vols, &used));
            }
        }
        VolumeCommand::Inspect { names } => {
            let mut out = Vec::new();
            for n in &names {
                let v = store.require(n)?;
                out.push(vol::inspect_json(&v, &used[&v.meta.name]));
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        VolumeCommand::Rm { names, force } => {
            // Check every name first so a refusal leaves everything in place.
            for n in &names {
                match store.get(n)? {
                    None if force => {}
                    None => {
                        store.require(n)?;
                    }
                    Some(v) => vol::check_unused(n, &used[&v.meta.name])?,
                }
            }
            for n in &names {
                if store.get(n)?.is_some() {
                    let bytes = store.remove(n)?;
                    println!(
                        "removed {n} (freed about {})",
                        crate::mac_inspect::human_bytes(bytes)
                    );
                }
            }
        }
        VolumeCommand::Prune { dry_run, force } => {
            let unused: Vec<&vol::Volume> = vols
                .iter()
                .filter(|v| used[&v.meta.name].is_empty())
                .collect();
            if unused.is_empty() {
                println!("no unused volumes");
                return Ok(());
            }
            let total: u64 = unused.iter().map(|v| vol::tree_alloc(&v.data)).sum();
            for v in &unused {
                println!(
                    "volume {} ({})",
                    v.meta.name,
                    crate::mac_inspect::human_bytes(vol::tree_alloc(&v.data))
                );
            }
            println!("Would free {}.", crate::mac_inspect::human_bytes(total));
            if dry_run {
                return Ok(());
            }
            ensure!(
                crate::mac_more::confirm(
                    &format!("This deletes {} volume(s) and their data", unused.len()),
                    force
                )?,
                "cancelled"
            );
            for v in unused {
                store.remove(&v.meta.name)?;
            }
            println!("removed");
        }
        VolumeCommand::Create { .. } => anyhow::bail!("internal: handled client side"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_cmd::MacCommand as C;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        c: C,
    }

    fn parse(a: &[&str]) -> C {
        let mut v = vec!["t"];
        v.extend(a);
        T::try_parse_from(v).unwrap().c
    }

    #[test]
    fn the_commands_parse_and_only_create_and_refusals_skip_the_daemon() {
        let C::Res(c) = parse(&["volume", "create", "db", "--label", "a=b"]) else {
            panic!()
        };
        assert!(c.is_client_side());
        let C::Res(c) = parse(&["volume", "ls", "-q"]) else {
            panic!()
        };
        assert!(!c.is_client_side());
        let C::Res(c) = parse(&["volume", "rm", "-f", "a", "b"]) else {
            panic!()
        };
        assert!(
            matches!(c, MacRes::Volume { command: VolumeCommand::Rm { force: true, ref names } } if names.len() == 2)
        );
        let C::Res(c) = parse(&["network", "create", "--driver", "bridge", "x"]) else {
            panic!()
        };
        assert!(c.is_client_side());
        let C::Res(c) = parse(&["network", "inspect"]) else {
            panic!()
        };
        assert!(!c.is_client_side());
        assert!(T::try_parse_from(["t", "volume", "rm"]).is_err());
    }

    #[test]
    fn network_create_is_refused_in_plain_english() {
        let C::Res(c) = parse(&["network", "create", "x"]) else {
            panic!()
        };
        let e = run_client(c).unwrap_err().to_string();
        assert!(
            e.contains("not supported on macOS guests") && e.contains("MAC_NETWORK_UNSUPPORTED"),
            "{e}"
        );
    }
}
