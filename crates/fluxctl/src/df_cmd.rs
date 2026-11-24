// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl df [-v] [--json]`: where FluxVM's disk space goes and how much `fluxctl prune` could give back, in the shape
//! of `docker system df` (TYPE, TOTAL, ACTIVE, SIZE, RECLAIMABLE). It reads the daemon's `GET /v1/host/storage` (blocks
//! really allocated, so a sparse disk counts what it occupies) and `GET /v1/vms` (status and the protection label).
//! RECLAIMABLE counts only VMs that are stopped or failed and not protected: exactly what `fluxctl prune --older-than 0s`
//! would remove. Copy-on-write clones share blocks, so the rows can add up to more than the volume holds; the free-space
//! line is the truth.

use crate::cli_errors::coded;
use crate::remote::Remote;
use anyhow::Result;
use fluxvm_core::protect::is_protected;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

#[derive(clap::Args, Debug, Clone)]
pub struct DfArgs {
    /// Also list every VM and branch with its size.
    #[arg(short, long)]
    pub verbose: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub allocated_bytes: u64,
    #[serde(default)]
    pub logical_bytes: u64,
    #[serde(default)]
    pub files: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Volume {
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VmStorage {
    pub vm_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub disk: Usage,
    #[serde(default)]
    pub other: Usage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BranchStorage {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StorageReport {
    #[serde(default)]
    pub volume: Volume,
    #[serde(default)]
    pub vms: Vec<VmStorage>,
    #[serde(default)]
    pub branches: Vec<BranchStorage>,
    #[serde(default)]
    pub images: Usage,
}

/// One line of the summary table.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Row {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub total: u64,
    pub active: u64,
    pub size_bytes: u64,
    /// `None` for kinds `fluxctl` cannot remove in one step (images are shared; branches have `branch prune`).
    pub reclaimable_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Df {
    pub rows: Vec<Row>,
    pub volume_total_bytes: Option<u64>,
    pub volume_free_bytes: Option<u64>,
    /// Stopped or failed, unprotected VMs: what `fluxctl prune --older-than 0s` would delete.
    pub reclaimable_vms: Vec<String>,
    pub protected_vms: Vec<String>,
}

fn vm_total(v: &VmStorage) -> u64 {
    v.disk
        .allocated_bytes
        .saturating_add(v.other.allocated_bytes)
}

/// The summary. `protected` holds the ids of VMs with the protection label. Pure.
pub fn summarize(s: &StorageReport, protected: &HashSet<String>) -> Df {
    let active = |v: &VmStorage| matches!(v.status.as_str(), "running" | "paused" | "creating");
    let reclaimable = |v: &VmStorage| !active(v) && !protected.contains(&v.vm_id);
    let vm_rows: Vec<&VmStorage> = s.vms.iter().collect();
    let rows = vec![
        Row {
            kind: "VMs",
            total: vm_rows.len() as u64,
            active: vm_rows.iter().filter(|v| active(v)).count() as u64,
            size_bytes: vm_rows.iter().map(|v| vm_total(v)).sum(),
            reclaimable_bytes: Some(
                vm_rows
                    .iter()
                    .filter(|v| reclaimable(v))
                    .map(|v| vm_total(v))
                    .sum(),
            ),
        },
        Row {
            kind: "Branches",
            total: s.branches.len() as u64,
            active: 0,
            size_bytes: s.branches.iter().map(|b| b.usage.allocated_bytes).sum(),
            reclaimable_bytes: None,
        },
        Row {
            kind: "Images",
            total: 0,
            active: 0,
            size_bytes: s.images.allocated_bytes,
            reclaimable_bytes: None,
        },
    ];
    Df {
        rows,
        volume_total_bytes: s.volume.total_bytes,
        volume_free_bytes: s.volume.free_bytes,
        reclaimable_vms: vm_rows
            .iter()
            .filter(|v| reclaimable(v))
            .map(|v| v.name.clone())
            .collect(),
        protected_vms: vm_rows
            .iter()
            .filter(|v| protected.contains(&v.vm_id))
            .map(|v| v.name.clone())
            .collect(),
    }
}

pub fn bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// Docker-style table. Pure.
pub fn render(d: &Df, s: &StorageReport, verbose: bool) -> String {
    let mut out = format!(
        "{:<10} {:>6} {:>7} {:>12} {:>14}\n",
        "TYPE", "TOTAL", "ACTIVE", "SIZE", "RECLAIMABLE"
    );
    for r in &d.rows {
        let (total, active) = if r.kind == "Images" {
            ("-".to_string(), "-".to_string())
        } else if r.kind == "Branches" {
            (r.total.to_string(), "-".to_string())
        } else {
            (r.total.to_string(), r.active.to_string())
        };
        let rec = match r.reclaimable_bytes {
            Some(b) if r.size_bytes > 0 => format!("{} ({}%)", bytes(b), b * 100 / r.size_bytes),
            Some(b) => bytes(b),
            None => "-".into(),
        };
        out.push_str(&format!(
            "{:<10} {:>6} {:>7} {:>12} {:>14}\n",
            r.kind,
            total,
            active,
            bytes(r.size_bytes),
            rec
        ));
    }
    match (d.volume_free_bytes, d.volume_total_bytes) {
        (Some(f), Some(t)) => {
            out.push_str(&format!("\nVolume: {} free of {}\n", bytes(f), bytes(t)))
        }
        (Some(f), None) => out.push_str(&format!("\nVolume: {} free\n", bytes(f))),
        _ => {}
    }
    if !d.protected_vms.is_empty() {
        out.push_str(&format!(
            "Protected (never reclaimable): {}\n",
            d.protected_vms.join(", ")
        ));
    }
    if d.reclaimable_vms.is_empty() {
        out.push_str("Nothing to reclaim: no stopped, unprotected VM.\n");
    } else {
        out.push_str(
            "Reclaim by deleting stopped VMs: `fluxctl prune --older-than 0s` (preview) then `--yes`.\n",
        );
    }
    if verbose {
        out.push_str(&format!(
            "\n{:<24} {:<9} {:>12} {:>12}\n",
            "VM", "STATUS", "DISK", "OTHER"
        ));
        for v in &s.vms {
            out.push_str(&format!(
                "{:<24} {:<9} {:>12} {:>12}\n",
                v.name.chars().take(24).collect::<String>(),
                v.status,
                bytes(v.disk.allocated_bytes),
                bytes(v.other.allocated_bytes)
            ));
        }
        if !s.branches.is_empty() {
            out.push_str(&format!("\n{:<24} {:>12}\n", "BRANCH", "SIZE"));
            for b in &s.branches {
                out.push_str(&format!(
                    "{:<24} {:>12}\n",
                    b.name.chars().take(24).collect::<String>(),
                    bytes(b.usage.allocated_bytes)
                ));
            }
        }
    }
    out
}

/// Reads the storage report and the set of protected VM ids from the daemon.
pub async fn fetch(r: &Remote) -> Result<(StorageReport, HashSet<String>)> {
    let down = |e: anyhow::Error| {
        coded(
            format!("could not read disk usage from the daemon: {e:#}"),
            "DAEMON_REQUIRED",
        )
    };
    let v = r
        .call(Method::GET, "/v1/host/storage", None)
        .await
        .map_err(down)?;
    let report: StorageReport = serde_json::from_value(v).map_err(|e| {
        coded(
            format!("the daemon's storage answer was not understood: {e}"),
            "DAEMON_REQUIRED",
        )
    })?;
    let vms = r.call(Method::GET, "/v1/vms", None).await.map_err(down)?;
    let protected = vms["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| {
            let l: BTreeMap<String, String> =
                serde_json::from_value(v["labels"].clone()).unwrap_or_default();
            is_protected(&l)
        })
        .filter_map(|v| v["id"].as_str().map(str::to_string))
        .collect();
    Ok((report, protected))
}

pub async fn run(r: &Remote, a: &DfArgs) -> Result<()> {
    let (report, protected) = fetch(r).await?;
    let d = summarize(&report, &protected);
    if a.json {
        let mut v = serde_json::to_value(&d)?;
        if a.verbose {
            v["vms"] = Value::Array(
                report
                    .vms
                    .iter()
                    .map(|x| serde_json::json!({"id": x.vm_id, "name": x.name, "status": x.status, "disk_bytes": x.disk.allocated_bytes, "other_bytes": x.other.allocated_bytes}))
                    .collect(),
            );
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        print!("{}", render(&d, &report, a.verbose));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fake, spawn, vm};
    use serde_json::json;
    use std::sync::Arc;

    fn storage_json() -> Value {
        let row = |id: u128, name: &str, status: &str, disk: u64, other: u64| {
            json!({"vm_id": uuid::Uuid::from_u128(id), "name": name, "status": status,
                "disk": {"allocated_bytes": disk, "logical_bytes": disk * 2, "files": 1},
                "other": {"allocated_bytes": other, "logical_bytes": other, "files": 2}})
        };
        json!({
            "volume": {"path": "/s", "total_bytes": 100u64 << 30, "free_bytes": 40u64 << 30},
            "vms": [
                row(1, "web", "running", 4 << 30, 1 << 30),
                row(2, "db", "stopped", 3 << 30, 0),
                row(3, "scratch", "stopped", 1 << 30, 1 << 20),
            ],
            "branches": [{"id": uuid::Uuid::from_u128(9), "name": "b1", "usage": {"allocated_bytes": 1u64 << 30, "logical_bytes": 1u64 << 30, "files": 1}}],
            "images": {"allocated_bytes": 2u64 << 30, "logical_bytes": 2u64 << 30, "files": 3},
            "state_dir": {"allocated_bytes": 0, "logical_bytes": 0, "files": 0},
            "note": "x"
        })
    }

    #[tokio::test]
    async fn reclaimable_is_stopped_and_unprotected_only() {
        let f = Arc::new(Fake::default());
        *f.vms.lock().unwrap() = vec![
            vm(1, "web", "running", 1, &[]),
            vm(2, "db", "stopped", 1, &[("fluxvm.io/protected", "true")]),
            vm(3, "scratch", "stopped", 1, &[]),
        ];
        *f.storage.lock().unwrap() = storage_json();
        let r = spawn(f).await;
        let (rep, prot) = fetch(&r).await.unwrap();
        let d = summarize(&rep, &prot);
        let vms = &d.rows[0];
        assert_eq!((vms.total, vms.active), (3, 1));
        assert_eq!(
            vms.size_bytes,
            (4 << 30) + (1 << 30) + (3 << 30) + (1 << 30) + (1 << 20)
        );
        // Only `scratch` can go: `db` is protected and `web` runs.
        assert_eq!(vms.reclaimable_bytes, Some((1 << 30) + (1 << 20)));
        assert_eq!(d.reclaimable_vms, ["scratch"]);
        assert_eq!(d.protected_vms, ["db"]);
        let text = render(&d, &rep, true);
        assert!(text.starts_with("TYPE"), "{text}");
        assert!(text.contains("Protected (never reclaimable): db"));
        assert!(text.contains("Volume: 40.0 GiB free of 100.0 GiB"));
        assert!(text.contains("fluxctl prune"));
        assert!(text.contains("scratch") && text.contains("BRANCH"));
    }

    #[tokio::test]
    async fn the_run_prints_json_and_a_down_daemon_is_explained() {
        let f = Arc::new(Fake::default());
        *f.storage.lock().unwrap() = storage_json();
        let r = spawn(f).await;
        run(
            &r,
            &DfArgs {
                verbose: true,
                json: true,
            },
        )
        .await
        .unwrap();
        let dead = Remote::new("http://127.0.0.1:1", None);
        let e = run(
            &dead,
            &DfArgs {
                verbose: false,
                json: false,
            },
        )
        .await
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("fluxctl explain DAEMON_REQUIRED"),
            "{e:#}"
        );
    }

    #[test]
    fn an_empty_host_says_there_is_nothing_to_reclaim() {
        let d = summarize(&StorageReport::default(), &HashSet::new());
        let t = render(&d, &StorageReport::default(), false);
        assert!(t.contains("Nothing to reclaim"), "{t}");
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1 << 30), "1.0 GiB");
    }
}
