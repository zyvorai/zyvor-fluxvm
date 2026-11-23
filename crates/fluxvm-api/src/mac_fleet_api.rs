// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `GET /v1/mac-fleet` (admin): the Mac fleet as `fluxctl mac-fleet top` last saw it, for the Studio Fleet page.
//!
//! The daemon does not ssh into Macs. `fluxctl mac-fleet top` (and every refresh of it) writes `mac-fleet-top.json` next to
//! the fleet state file (`$FLUXVM_MAC_FLEET_TOP`, else `mac-fleet-top.json` beside `$FLUXVM_MAC_FLEET_STATE`, else
//! `~/.config/fluxvm/mac-fleet-top.json`); this route reads that snapshot and the live cordon list (`mac-fleet-state.json`), so a
//! `cordon` from a terminal shows at once. Each row carries the exact `fluxctl` commands for the actions (Studio copies them,
//! it never runs them), and the answer says how old the snapshot is and which command refreshes it. Neither file holds a secret.

use super::{ApiResult, require_admin};
use axum::{Extension, Json};
use fluxvm_core::config::Role;
use serde_json::{Value, json};
use std::path::PathBuf;

/// A snapshot older than this many seconds is called stale.
const STALE_SECS: u64 = 120;
const REFRESH: &str = "fluxctl mac-fleet top --once -f hosts.yaml";

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/fluxvm"))
}

fn state_path() -> Option<PathBuf> {
    std::env::var_os("FLUXVM_MAC_FLEET_STATE")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| config_dir().map(|d| d.join("mac-fleet-state.json")))
}

fn snapshot_path() -> Option<PathBuf> {
    std::env::var_os("FLUXVM_MAC_FLEET_TOP")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("FLUXVM_MAC_FLEET_STATE")
                .filter(|p| !p.is_empty())
                .map(|p| PathBuf::from(p).with_file_name("mac-fleet-top.json"))
        })
        .or_else(|| config_dir().map(|d| d.join("mac-fleet-top.json")))
}

fn read_json(p: Option<PathBuf>) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(p?).ok()?).ok()
}

fn commands(name: &str) -> Value {
    json!({
        "cordon": format!("fluxctl mac-fleet cordon {name}"),
        "uncordon": format!("fluxctl mac-fleet uncordon {name}"),
        "drain": format!("fluxctl mac-fleet drain {name} --wait"),
        "doctor": format!("fluxctl mac-fleet doctor {name}"),
    })
}

/// The page's data from the two files and the clock. Pure, so the tests need no files.
pub(crate) fn build(snapshot: Option<&Value>, state: Option<&Value>, now: u64) -> Value {
    let cordoned = |n: &str| state.is_some_and(|s| s["drained"].get(n).is_some());
    let mut hosts: Vec<Value> = vec![];
    let (mut taken, mut status) = (None, "no_snapshot");
    if let Some(snap) = snapshot
        && let Some(rows) = snap["hosts"].as_array()
    {
        taken = snap["taken_unix"].as_u64();
        status = "live";
        for r in rows {
            let name = r["name"].as_str().unwrap_or("");
            let guests = r["macos_guests"].as_u64().unwrap_or(0);
            let c = cordoned(name);
            let mut row = r.clone();
            let shown = match (r["state"].as_str().unwrap_or(""), c) {
                ("unreachable", _) => "unreachable",
                (_, true) if guests > 0 => "draining",
                (_, true) => "cordoned",
                ("cordoned" | "draining", false) if guests >= 2 => "full",
                ("cordoned" | "draining", false) => "ready",
                (s, false) => match s {
                    "full" => "full",
                    _ => "ready",
                },
            };
            row["state"] = json!(shown);
            row["cordoned"] = json!(c);
            row["commands"] = commands(name);
            hosts.push(row);
        }
    } else if let Some(applied) = state.and_then(|s| s["applied"].as_object()) {
        // Never probed: list the Macs the last `apply` configured, so the page is not empty.
        for list in applied.values().filter_map(Value::as_array) {
            for h in list {
                let name = h["name"].as_str().unwrap_or("");
                if hosts.iter().any(|x| x["name"] == name) {
                    continue;
                }
                hosts.push(json!({
                    "name": name, "host": h["host"], "state": "unknown", "cordoned": cordoned(name),
                    "macos_guests": 0, "macos_limit": 2, "thermal": "unknown", "thermal_limit_pct": null,
                    "disk_free_gib": null, "disk_low": false, "on_ac": null, "error": null,
                    "next": REFRESH, "commands": commands(name),
                }));
            }
        }
    }
    let age = taken.map(|t| now.saturating_sub(t));
    if status == "live" && age.is_some_and(|a| a > STALE_SECS) {
        status = "stale";
    }
    if hosts.is_empty() {
        status = "empty";
    }
    let count = |f: &dyn Fn(&Value) -> bool| hosts.iter().filter(|h| f(h)).count();
    let up = |h: &Value| !matches!(h["state"].as_str(), Some("unreachable" | "unknown"));
    let slots_used: u64 = hosts
        .iter()
        .filter(|h| up(h))
        .map(|h| h["macos_guests"].as_u64().unwrap_or(0))
        .sum();
    let summary = json!({
        "macs": hosts.len(),
        "ready": count(&|h| matches!(h["state"].as_str(), Some("ready" | "full"))),
        "cordoned": count(&|h| matches!(h["state"].as_str(), Some("cordoned" | "draining"))),
        "unreachable": count(&|h| h["state"] == "unreachable"),
        "slots_used": slots_used,
        "slots_total": count(&|h| up(h)) * 2,
    });
    let message = match status {
        "empty" => "No Macs yet. Add one: fluxctl mac-fleet add user@host".to_string(),
        "no_snapshot" => format!("The fleet has not been checked yet. Run: {REFRESH}"),
        "stale" => format!(
            "Last checked {} ago. Refresh: {REFRESH}",
            age_text(age.unwrap_or(0))
        ),
        _ => format!(
            "{} Mac(s), {} ready, {} cordoned, {} unreachable; {}/{} macOS slots busy",
            summary["macs"],
            summary["ready"],
            summary["cordoned"],
            summary["unreachable"],
            summary["slots_used"],
            summary["slots_total"]
        ),
    };
    json!({
        "status": status,
        "message": message,
        "taken_unix": taken,
        "age_secs": age,
        "hosts": hosts,
        "summary": summary,
        "refresh_command": REFRESH,
        "add_command": "fluxctl mac-fleet add user@host",
    })
}

fn age_text(s: u64) -> String {
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    }
}

/// `GET /v1/mac-fleet`
pub(crate) async fn get(Extension(role): Extension<Role>) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    Ok(Json(build(
        read_json(snapshot_path()).as_ref(),
        read_json(state_path()).as_ref(),
        now,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Value {
        json!({"taken_unix": 1000, "hosts": [
            {"name": "mini-1", "host": "a@mini-1", "state": "ready", "macos_guests": 1},
            {"name": "mini-2", "host": "a@mini-2", "state": "full", "macos_guests": 2},
            {"name": "mini-3", "host": "a@mini-3", "state": "unreachable", "macos_guests": 0}]})
    }

    #[test]
    fn a_fresh_snapshot_is_live_with_commands_per_mac() {
        let v = build(Some(&snap()), None, 1030);
        assert_eq!(v["status"], "live");
        assert_eq!(v["summary"]["macs"], 3);
        assert_eq!(v["summary"]["slots_used"], 3);
        assert_eq!(v["summary"]["slots_total"], 4);
        assert_eq!(
            v["hosts"][0]["commands"]["cordon"],
            "fluxctl mac-fleet cordon mini-1"
        );
        assert_eq!(v["hosts"][2]["state"], "unreachable");
    }

    #[test]
    fn the_live_cordon_list_wins_over_the_snapshot() {
        let st = json!({"drained": {"mini-1": {"since_unix": 5}}});
        let v = build(Some(&snap()), Some(&st), 1030);
        assert_eq!(v["hosts"][0]["state"], "draining");
        assert_eq!(v["hosts"][0]["cordoned"], true);
        assert_eq!(v["summary"]["cordoned"], 1);
    }

    #[test]
    fn old_snapshots_are_stale_and_missing_ones_say_what_to_run() {
        let v = build(Some(&snap()), None, 1000 + 600);
        assert_eq!(v["status"], "stale");
        assert!(v["message"].as_str().unwrap().contains("mac-fleet top"));
        let none = build(None, None, 5);
        assert_eq!(none["status"], "empty");
        assert!(none["message"].as_str().unwrap().contains("mac-fleet add"));
    }

    #[test]
    fn macs_from_the_last_apply_are_listed_before_the_first_check() {
        let st = json!({"applied": {"/x/inv.yaml": [{"name": "mini-9", "host": "mini-9.local"}]}});
        let v = build(None, Some(&st), 5);
        assert_eq!(v["status"], "no_snapshot");
        assert_eq!(v["hosts"][0]["name"], "mini-9");
        assert_eq!(v["hosts"][0]["state"], "unknown");
    }
}
