// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl prune [--older-than 7d] [-l SELECTOR] [--failed] [--yes] [--json]`: remove stopped VMs in one step, like
//! `docker container prune` and `vagrant global-status --prune`, but a preview first: without `--yes` it only prints what
//! it would delete and how much disk that frees. It never touches a running or paused VM, never a VM with the
//! `fluxvm.io/protected=true` label (`fluxctl protect`), and "older" means created longer ago than `--older-than`.
//! Failed VMs are included only with `--failed`.

use crate::cli_errors::coded;
use crate::remote::{Remote, encode_query};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use fluxvm_core::protect::is_protected;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(clap::Args, Debug, Clone)]
pub struct PruneArgs {
    /// Only VMs created more than this long ago: `30m`, `12h`, `7d`, `2w`, or `0s` for all.
    #[arg(long, default_value = "7d")]
    pub older_than: String,
    /// Only VMs whose labels match, for example `env=dev,team`.
    #[arg(short = 'l', long)]
    pub label: Option<String>,
    /// Also remove VMs in the failed state.
    #[arg(long)]
    pub failed: bool,
    /// Really delete. Without it the command only shows the plan.
    #[arg(long)]
    pub yes: bool,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// `30s`, `15m`, `12h`, `7d` or `2w`.
pub fn parse_age(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    let bad = || {
        format!(
            "cannot read --older-than {t:?}: use a length like `30m`, `12h`, `7d`, `2w` (or `0s`)"
        )
    };
    if t.len() < 2 || !t.is_char_boundary(t.len() - 1) {
        return Err(bad());
    }
    let (n, unit) = t.split_at(t.len() - 1);
    let n: i64 = n.parse().map_err(|_| bad())?;
    if !(0..=36_500).contains(&n) {
        return Err(bad());
    }
    Ok(match unit {
        "s" => Duration::seconds(n),
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        _ => return Err(bad()),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct VmLite {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub status: String,
    pub age_days: i64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct Plan {
    pub delete: Vec<Candidate>,
    /// Matched everything except that the VM is protected.
    pub skipped_protected: Vec<String>,
    /// Stopped but newer than `--older-than`.
    pub skipped_young: Vec<String>,
}

/// Decides what to delete. `sizes` maps VM id to allocated bytes (best effort). Pure.
pub fn plan(
    items: &[Value],
    now: DateTime<Utc>,
    min_age: Duration,
    with_failed: bool,
    sizes: &BTreeMap<String, u64>,
) -> Plan {
    let mut p = Plan::default();
    for v in items {
        let Ok(vm) = serde_json::from_value::<VmLite>(v.clone()) else {
            continue;
        };
        let eligible = vm.status == "stopped" || (with_failed && vm.status == "failed");
        if !eligible {
            continue;
        }
        if is_protected(&vm.labels) {
            p.skipped_protected.push(vm.name);
            continue;
        }
        let age = now - vm.created_at;
        if age < min_age {
            p.skipped_young.push(vm.name);
            continue;
        }
        p.delete.push(Candidate {
            bytes: sizes.get(&vm.id).copied().unwrap_or(0),
            age_days: age.num_days(),
            id: vm.id,
            name: vm.name,
            status: vm.status,
        });
    }
    p.delete
        .sort_by(|a, b| b.age_days.cmp(&a.age_days).then(a.name.cmp(&b.name)));
    p
}

pub fn render(p: &Plan, executed: bool, results: &[Value]) -> String {
    let mut out = String::new();
    if p.delete.is_empty() {
        out.push_str("Nothing to prune.\n");
    } else {
        out.push_str(&format!(
            "{:<24} {:<8} {:>8} {:>12}\n",
            "VM", "STATUS", "AGE", "SIZE"
        ));
        for c in &p.delete {
            out.push_str(&format!(
                "{:<24} {:<8} {:>7}d {:>12}\n",
                c.name.chars().take(24).collect::<String>(),
                c.status,
                c.age_days,
                crate::df_cmd::bytes(c.bytes)
            ));
        }
        let total: u64 = p.delete.iter().map(|c| c.bytes).sum();
        if executed {
            let ok = results.iter().filter(|r| r["ok"] == true).count();
            out.push_str(&format!(
                "\nDeleted {ok} of {} VM(s), about {} freed.\n",
                p.delete.len(),
                crate::df_cmd::bytes(total)
            ));
            for r in results.iter().filter(|r| r["ok"] != true) {
                out.push_str(&format!(
                    "failed {}: {}\n",
                    r["name"].as_str().unwrap_or("?"),
                    r["error"].as_str().unwrap_or("")
                ));
            }
        } else {
            out.push_str(&format!(
                "\nWould delete {} VM(s), about {} freed. Nothing was deleted; add --yes to do it.\n",
                p.delete.len(),
                crate::df_cmd::bytes(total)
            ));
        }
    }
    if !p.skipped_protected.is_empty() {
        out.push_str(&format!(
            "Kept (protected): {}\n",
            p.skipped_protected.join(", ")
        ));
    }
    if !p.skipped_young.is_empty() {
        out.push_str(&format!(
            "Kept (newer than --older-than): {}\n",
            p.skipped_young.join(", ")
        ));
    }
    out
}

pub async fn run(r: &Remote, a: &PruneArgs) -> Result<()> {
    let age = parse_age(&a.older_than).map_err(|m| coded(m, "PRUNE_BAD_AGE"))?;
    let path = match &a.label {
        Some(l) => format!("/v1/vms?label={}", encode_query(l)),
        None => "/v1/vms".to_string(),
    };
    let v = r.call(Method::GET, &path, None).await.map_err(|e| {
        coded(
            format!("could not list VMs from the daemon: {e:#}"),
            "DAEMON_REQUIRED",
        )
    })?;
    let items = v["items"].as_array().cloned().unwrap_or_default();
    // Sizes are a nicety: an older daemon without the storage report still prunes.
    let sizes: BTreeMap<String, u64> = match crate::df_cmd::fetch(r).await {
        Ok((rep, _)) => rep
            .vms
            .iter()
            .map(|v| {
                (
                    v.vm_id.clone(),
                    v.disk
                        .allocated_bytes
                        .saturating_add(v.other.allocated_bytes),
                )
            })
            .collect(),
        Err(_) => BTreeMap::new(),
    };
    let p = plan(&items, Utc::now(), age, a.failed, &sizes);
    let mut results = Vec::new();
    if a.yes {
        for c in &p.delete {
            match r
                .call(Method::DELETE, &format!("/v1/vms/{}", c.id), None)
                .await
            {
                Ok(_) => results.push(json!({"id": c.id, "name": c.name, "ok": true})),
                Err(e) => results.push(
                    json!({"id": c.id, "name": c.name, "ok": false, "error": format!("{e:#}")}),
                ),
            }
        }
    }
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"executed": a.yes, "plan": p, "results": results})
            )?
        );
    } else {
        print!("{}", render(&p, a.yes, &results));
    }
    let failed = results.iter().filter(|r| r["ok"] != true).count();
    if failed > 0 {
        anyhow::bail!("{failed} VM(s) could not be deleted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Fake, spawn, vm};
    use std::sync::Arc;

    fn fake() -> Arc<Fake> {
        let f = Arc::new(Fake::default());
        *f.vms.lock().unwrap() = vec![
            vm(1, "running-old", "running", 30, &[]),
            vm(2, "stopped-old", "stopped", 30, &[]),
            vm(3, "stopped-young", "stopped", 1, &[]),
            vm(
                4,
                "stopped-protected",
                "stopped",
                30,
                &[("fluxvm.io/protected", "true")],
            ),
            vm(5, "failed-old", "failed", 30, &[]),
            vm(6, "dev-old", "stopped", 40, &[("env", "dev")]),
        ];
        *f.storage.lock().unwrap() = json!({"vms": [
            {"vm_id": uuid::Uuid::from_u128(2), "name": "stopped-old", "status": "stopped",
             "disk": {"allocated_bytes": 2u64 << 30}, "other": {"allocated_bytes": 0}}]});
        f
    }

    fn args(older: &str, label: Option<&str>, failed: bool, yes: bool) -> PruneArgs {
        PruneArgs {
            older_than: older.into(),
            label: label.map(Into::into),
            failed,
            yes,
            json: true,
        }
    }

    fn deletes(f: &Fake) -> Vec<String> {
        f.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.starts_with("DELETE"))
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn without_yes_nothing_is_deleted() {
        let f = fake();
        let r = spawn(f.clone()).await;
        run(&r, &args("7d", None, false, false)).await.unwrap();
        assert!(deletes(&f).is_empty());
        assert_eq!(f.vms.lock().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn yes_deletes_only_old_stopped_unprotected_vms() {
        let f = fake();
        let r = spawn(f.clone()).await;
        run(&r, &args("7d", None, false, true)).await.unwrap();
        let mut left: Vec<String> = f
            .vms
            .lock()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "failed-old",
                "running-old",
                "stopped-protected",
                "stopped-young"
            ]
        );
        assert_eq!(deletes(&f).len(), 2);
    }

    #[tokio::test]
    async fn failed_flag_selector_and_zero_age() {
        let f = fake();
        let r = spawn(f.clone()).await;
        run(&r, &args("0s", Some("env=dev"), false, true))
            .await
            .unwrap();
        assert_eq!(deletes(&f).len(), 1, "{:?}", f.calls.lock().unwrap());
        let f2 = fake();
        let r3 = spawn(f2.clone()).await;
        run(&r3, &args("7d", None, true, true)).await.unwrap();
        assert!(
            f2.vms
                .lock()
                .unwrap()
                .iter()
                .all(|v| v["name"] != "failed-old")
        );
    }

    #[tokio::test]
    async fn a_refusal_from_the_daemon_is_reported_and_fails_the_command() {
        // The VM becomes protected between the listing and the delete: the daemon's 409 must surface.
        let f = fake();
        let r = spawn(f.clone()).await;
        let items = f.vms.lock().unwrap().clone();
        let p = plan(
            &items,
            Utc::now(),
            Duration::days(7),
            false,
            &BTreeMap::new(),
        );
        f.vms.lock().unwrap().iter_mut().for_each(|v| {
            if v["name"] == "stopped-old" {
                v["labels"] = json!({"fluxvm.io/protected": "true"});
            }
        });
        let c = p.delete.iter().find(|c| c.name == "stopped-old").unwrap();
        let e = r
            .call(Method::DELETE, &format!("/v1/vms/{}", c.id), None)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("protected"), "{e:#}");
    }

    #[test]
    fn the_plan_explains_what_it_kept_and_sums_sizes() {
        let items = vec![
            vm(2, "a", "stopped", 10, &[]),
            vm(3, "b", "stopped", 1, &[]),
            vm(4, "c", "stopped", 10, &[("fluxvm.io/protected", "true")]),
        ];
        let sizes = BTreeMap::from([(uuid::Uuid::from_u128(2).to_string(), 3u64 << 30)]);
        let p = plan(&items, Utc::now(), Duration::days(7), false, &sizes);
        assert_eq!(p.delete.len(), 1);
        assert_eq!(p.skipped_young, ["b"]);
        assert_eq!(p.skipped_protected, ["c"]);
        let t = render(&p, false, &[]);
        assert!(
            t.contains("Would delete 1 VM(s), about 3.0 GiB") && t.contains("add --yes"),
            "{t}"
        );
        assert!(
            t.contains("Kept (protected): c") && t.contains("Kept (newer than --older-than): b")
        );
        assert_eq!(render(&Plan::default(), false, &[]), "Nothing to prune.\n");
    }

    #[test]
    fn ages_parse_and_bad_ones_name_the_fix() {
        assert_eq!(parse_age("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_age("0s").unwrap(), Duration::zero());
        assert_eq!(parse_age("2w").unwrap(), Duration::weeks(2));
        for bad in ["", "d", "7", "7x", "-1d", "1.5d", "7 d"] {
            assert!(
                parse_age(bad).unwrap_err().contains("--older-than"),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn a_bad_age_is_a_coded_error_before_any_call() {
        let f = fake();
        let r = spawn(f.clone()).await;
        let e = run(&r, &args("soon", None, false, true)).await.unwrap_err();
        assert!(format!("{e:#}").contains("fluxctl explain PRUNE_BAD_AGE"));
        assert!(f.calls.lock().unwrap().is_empty());
    }
}
