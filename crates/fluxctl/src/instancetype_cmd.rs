// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl instancetype ls|get|create|rm` and `fluxctl preference ls|get|create|rm`.
//!
//! Against a daemon (`--server` or a context) these call `/v1/instancetypes` and `/v1/preferences`;
//! locally they read and write the same files under `state_dir` the daemon does. See
//! `fluxvm_core::instancetype` and docs/instancetypes.md.

use crate::output::{self, Column, OutputFormat, col, wide};
use crate::remote::Remote;
use anyhow::{Context, Result};
use clap::Subcommand;
use fluxvm_core::instancetype::{
    Catalog, ClassRef, InstancetypeLimits, InstancetypeSpec, PreferenceSpec,
};
use fluxvm_core::model::DiskBus;
use reqwest::Method;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const TYPE_COLUMNS: &[Column] = &[
    col("NAME", "/name"),
    col("VERSION", "/version"),
    col("VCPUS", "/spec/vcpus"),
    col("MEMORY_MIB", "/spec/memory_mib"),
    col("BUILTIN", "/builtin"),
    col("DESCRIPTION", "/description"),
    wide("HUGEPAGES", "/spec/hugepages"),
    wide("CPU_MODEL", "/spec/cpu_model"),
    wide("CREATED", "/created_at"),
];

const PREF_COLUMNS: &[Column] = &[
    col("NAME", "/name"),
    col("DISK_BUS", "/spec/disk_bus"),
    col("NIC_MODEL", "/spec/nic_model"),
    col("SECURE_BOOT", "/spec/secure_boot"),
    col("TPM", "/spec/tpm"),
    col("BUILTIN", "/builtin"),
    wide("DESCRIPTION", "/description"),
    wide("CREATED", "/created_at"),
];

#[derive(Subcommand)]
pub enum InstancetypeCommand {
    /// List built-in and stored instancetypes, every version.
    #[command(visible_alias = "list")]
    Ls,
    /// Show one instancetype: `NAME` (latest version) or `NAME:VERSION`.
    Get { name: String },
    /// Store an instancetype. Without `--version` it becomes the next version of NAME; a version that
    /// exists (stored or built in) needs `--replace`.
    Create {
        name: String,
        #[arg(long)]
        vcpus: u8,
        #[arg(long)]
        memory_mib: u64,
        #[arg(long)]
        version: Option<u32>,
        /// CPU hotplug ceiling.
        #[arg(long)]
        max_vcpus: Option<u8>,
        /// Memory hotplug ceiling in MiB.
        #[arg(long)]
        max_memory_mib: Option<u64>,
        #[arg(long)]
        hugepages: Option<bool>,
        /// CPU model; applied to `cpu_template` on Firecracker only.
        #[arg(long)]
        cpu_model: Option<String>,
        /// Network bandwidth cap, Mbit/s.
        #[arg(long)]
        net_mbit_limit: Option<u32>,
        #[arg(long)]
        net_pps_limit: Option<u64>,
        /// Block bandwidth cap, Mbit/s.
        #[arg(long)]
        blk_mbit_limit: Option<u32>,
        #[arg(long)]
        blk_ops_limit: Option<u64>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, default_value_t = false)]
        replace: bool,
    },
    /// Delete every stored version of NAME, or one with `NAME:VERSION`. Built-ins cannot be deleted.
    #[command(visible_aliases = ["delete", "remove"])]
    Rm { name: String },
}

#[derive(Subcommand)]
pub enum PreferenceCommand {
    /// List built-in and stored preferences.
    #[command(visible_alias = "list")]
    Ls,
    Get {
        name: String,
    },
    /// Store a preference (device and firmware defaults; the spec's own settings win).
    Create {
        name: String,
        /// `virtio-blk` (applied to the root disk), `scsi` or `vhost-user-blk` (data disks only).
        #[arg(long, value_parser = parse_bus)]
        disk_bus: Option<DiskBus>,
        #[arg(long)]
        nic_model: Option<String>,
        #[arg(long)]
        secure_boot: Option<bool>,
        #[arg(long)]
        tpm: Option<bool>,
        #[arg(long)]
        firmware: Option<PathBuf>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, default_value_t = false)]
        replace: bool,
    },
    #[command(visible_aliases = ["delete", "remove"])]
    Rm {
        name: String,
    },
}

fn parse_bus(s: &str) -> Result<DiskBus, String> {
    serde_json::from_value(json!(s))
        .map_err(|_| format!("{s:?} is not a disk bus: use scsi, virtio-blk or vhost-user-blk"))
}

fn pretty(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

pub async fn run_instancetype(
    command: InstancetypeCommand,
    remote: Option<Remote>,
    state_dir: &Path,
    format: OutputFormat,
) -> Result<()> {
    match command {
        InstancetypeCommand::Ls => {
            let items = match &remote {
                Some(r) => r.call(Method::GET, "/v1/instancetypes", None).await?["items"].clone(),
                None => {
                    let c = Catalog::load(state_dir)?;
                    json!(
                        c.instancetypes
                            .values()
                            .flat_map(|v| v.values())
                            .collect::<Vec<_>>()
                    )
                }
            };
            output::print_list(format, &items, TYPE_COLUMNS)
        }
        InstancetypeCommand::Get { name } => match &remote {
            Some(r) => pretty(
                &r.call(Method::GET, &format!("/v1/instancetypes/{name}"), None)
                    .await?,
            ),
            None => {
                let c = Catalog::load(state_dir)?;
                pretty(&json!(c.find_instancetype(&ClassRef::parse(&name)?)?))
            }
        },
        InstancetypeCommand::Create {
            name,
            vcpus,
            memory_mib,
            version,
            max_vcpus,
            max_memory_mib,
            hugepages,
            cpu_model,
            net_mbit_limit,
            net_pps_limit,
            blk_mbit_limit,
            blk_ops_limit,
            description,
            replace,
        } => {
            let limits = InstancetypeLimits {
                net_mbit_limit,
                net_pps_limit,
                blk_mbit_limit,
                blk_ops_limit,
            };
            let spec = InstancetypeSpec {
                vcpus,
                memory_mib,
                max_vcpus,
                max_memory_mib,
                hugepages,
                cpu_model,
                limits: (limits != InstancetypeLimits::default()).then_some(limits),
            };
            match &remote {
                Some(r) => pretty(
                    &r.call(
                        Method::POST,
                        "/v1/instancetypes",
                        Some(json!({
                            "name": name, "version": version, "description": description,
                            "spec": spec, "replace": replace,
                        })),
                    )
                    .await?,
                ),
                None => pretty(&json!(fluxvm_core::instancetype::save_instancetype(
                    state_dir,
                    &name,
                    version,
                    description,
                    spec,
                    replace
                )?)),
            }
        }
        InstancetypeCommand::Rm { name } => {
            match &remote {
                Some(r) => {
                    r.call(Method::DELETE, &format!("/v1/instancetypes/{name}"), None)
                        .await?;
                }
                None => {
                    let r = ClassRef::parse(&name)?;
                    fluxvm_core::instancetype::delete_instancetype(state_dir, &r.name, r.version)?
                }
            }
            println!("{}", json!({"ok": true, "deleted": name}));
            Ok(())
        }
    }
}

pub async fn run_preference(
    command: PreferenceCommand,
    remote: Option<Remote>,
    state_dir: &Path,
    format: OutputFormat,
) -> Result<()> {
    match command {
        PreferenceCommand::Ls => {
            let items = match &remote {
                Some(r) => r.call(Method::GET, "/v1/preferences", None).await?["items"].clone(),
                None => json!(
                    Catalog::load(state_dir)?
                        .preferences
                        .values()
                        .collect::<Vec<_>>()
                ),
            };
            output::print_list(format, &items, PREF_COLUMNS)
        }
        PreferenceCommand::Get { name } => match &remote {
            Some(r) => pretty(
                &r.call(Method::GET, &format!("/v1/preferences/{name}"), None)
                    .await?,
            ),
            None => pretty(&json!(
                Catalog::load(state_dir)?
                    .find_preference(&name)
                    .context("preference lookup")?
            )),
        },
        PreferenceCommand::Create {
            name,
            disk_bus,
            nic_model,
            secure_boot,
            tpm,
            firmware,
            description,
            replace,
        } => {
            let spec = PreferenceSpec {
                disk_bus,
                nic_model,
                secure_boot,
                tpm,
                firmware,
            };
            match &remote {
                Some(r) => pretty(
                    &r.call(
                        Method::POST,
                        "/v1/preferences",
                        Some(json!({
                            "name": name, "description": description, "spec": spec, "replace": replace,
                        })),
                    )
                    .await?,
                ),
                None => pretty(&json!(fluxvm_core::instancetype::save_preference(
                    state_dir,
                    &name,
                    description,
                    spec,
                    replace
                )?)),
            }
        }
        PreferenceCommand::Rm { name } => {
            match &remote {
                Some(r) => {
                    r.call(Method::DELETE, &format!("/v1/preferences/{name}"), None)
                        .await?;
                }
                None => fluxvm_core::instancetype::delete_preference(state_dir, &name)?,
            }
            println!("{}", json!({"ok": true, "deleted": name}));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_create_get_rm_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        run_instancetype(
            InstancetypeCommand::Create {
                name: "web".into(),
                vcpus: 2,
                memory_mib: 4096,
                version: None,
                max_vcpus: None,
                max_memory_mib: None,
                hugepages: None,
                cpu_model: None,
                net_mbit_limit: Some(100),
                net_pps_limit: None,
                blk_mbit_limit: None,
                blk_ops_limit: None,
                description: None,
                replace: false,
            },
            None,
            d,
            OutputFormat::Json,
        )
        .await
        .unwrap();
        let c = Catalog::load(d).unwrap();
        let t = c
            .find_instancetype(&ClassRef::parse("web:1").unwrap())
            .unwrap();
        assert_eq!(t.spec.limits.as_ref().unwrap().net_mbit_limit, Some(100));
        run_instancetype(InstancetypeCommand::Ls, None, d, OutputFormat::Table)
            .await
            .unwrap();
        run_instancetype(
            InstancetypeCommand::Rm { name: "web".into() },
            None,
            d,
            OutputFormat::Json,
        )
        .await
        .unwrap();
        assert!(
            run_instancetype(
                InstancetypeCommand::Rm { name: "web".into() },
                None,
                d,
                OutputFormat::Json
            )
            .await
            .is_err()
        );
        assert!(
            run_instancetype(
                InstancetypeCommand::Rm {
                    name: "small".into()
                },
                None,
                d,
                OutputFormat::Json
            )
            .await
            .is_err()
        );
        run_preference(
            PreferenceCommand::Create {
                name: "p".into(),
                disk_bus: Some(DiskBus::VirtioBlk),
                nic_model: None,
                secure_boot: None,
                tpm: None,
                firmware: None,
                description: None,
                replace: false,
            },
            None,
            d,
            OutputFormat::Json,
        )
        .await
        .unwrap();
        run_preference(PreferenceCommand::Ls, None, d, OutputFormat::Wide)
            .await
            .unwrap();
        assert_eq!(parse_bus("virtio-blk"), Ok(DiskBus::VirtioBlk));
        assert!(parse_bus("ide").is_err());
    }
}
