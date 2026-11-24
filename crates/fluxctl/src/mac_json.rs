// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The stable `--json` documents of `fluxctl mac run`, `ps` and `inspect` (schema version 1), documented in
//! docs/macos-containers.md ("JSON output").
//!
//! A scheduler or CI script reads these, so the rule is: **keys are only added, never renamed, removed or retyped within a
//! schema version**. Every object carries `schema_version`. The `vm` key (the daemon's raw VM record) is a passthrough and is
//! NOT part of the contract. Golden files in `src/mac_json_golden/` pin the shape; a change shows up as a test failure and
//! needs `UPDATE_GOLDEN=1` plus a CHANGELOG line.

use crate::mac_inspect::{
    self as view, IMAGE_LABEL, PROJECT_LABEL, RESTART_LABEL, SERVICE_LABEL, is_internal_label,
};
use serde_json::{Value, json};

pub const SCHEMA_VERSION: u32 = 1;

/// The host port that forwards to the guest's SSH port (22), when `-p` published one.
fn ssh_port(vm: &Value) -> Value {
    view::ports(vm)
        .into_iter()
        .find(|(_, g)| *g == 22)
        .map_or(Value::Null, |(h, _)| json!(h))
}

fn user_labels(vm: &Value) -> Value {
    let m: serde_json::Map<String, Value> = vm["labels"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !is_internal_label(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    Value::Object(m)
}

/// One element of `mac ps --json`.
pub fn ps_row(vm: &Value) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "id": vm["id"],
        "short_id": view::short_id(vm),
        "name": vm["name"],
        "image": vm["labels"][IMAGE_LABEL],
        "status": view::status(vm),
        "ip": vm["guest_ip"],
        "ports": view::ports(vm).iter().map(|(h, g)| json!({"host": h, "guest": g})).collect::<Vec<_>>(),
        "ssh_port": ssh_port(vm),
        "created": vm["created_at"],
        "restart_policy": vm["labels"][RESTART_LABEL].as_str().unwrap_or("no"),
        "project": vm["labels"][PROJECT_LABEL],
        "service": vm["labels"][SERVICE_LABEL],
        "labels": user_labels(vm),
        "vm": vm,
    })
}

/// `mac ps --json`: an array, one element per listed instance.
pub fn ps_document(rows: &[&Value]) -> Value {
    Value::Array(rows.iter().map(|v| ps_row(v)).collect())
}

/// One element of `mac inspect`: the existing inspect document plus `schema_version`.
pub fn inspect_v1(mut doc: Value) -> Value {
    if let Some(o) = doc.as_object_mut() {
        o.insert("schema_version".into(), json!(SCHEMA_VERSION));
    }
    doc
}

/// The line `mac run --json` prints last on stdout. After `--rm` the instance is gone, so this is the only record.
pub fn run_document(o: &crate::mac_instance::RunOutcome) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "name": o.name,
        "id": o.id,
        "image": o.image,
        "status": if o.removed { "removed" } else { "running" },
        "ip": o.ip,
        "removed": o.removed,
        "ran_command": o.ran_command,
        "detached": o.detached,
    })
}

/// The line `mac create --json` prints: the instance exists and has not been started.
pub fn create_document(name: &str, id: &str, image: &str) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "name": name,
        "id": id,
        "image": image,
        "status": "created",
        "started": false,
    })
}

/// The line `mac update --json` prints: what was applied. `labels` holds the user labels set; `restart_policy` is the new policy
/// (`no` when it was cleared) or null when `--restart` was not given.
pub fn update_document(name: &str, id: &str, patch: &Value) -> Value {
    let l = &patch["labels"];
    let user: serde_json::Map<String, Value> = l
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| !is_internal_label(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let restart = match l.get(RESTART_LABEL) {
        None => Value::Null,
        Some(Value::Null) => json!("no"),
        Some(v) => v.clone(),
    };
    json!({
        "schema_version": SCHEMA_VERSION,
        "name": name,
        "id": id,
        "restart_policy": restart,
        "labels": user,
    })
}

/// `mac update --json`: [`update_document`] plus what the daemon did with a size change. Keys are only added: `cpus` and
/// `memory_mib` are the sizes asked for (null when not), `applied` and `pending_restart` say what took effect and what
/// still needs a restart, `restarted` and `checkpoint` (the `auto-before-update-<time>` tag, or null) describe `--restart-now`.
pub fn update_document_ext(
    name: &str,
    id: &str,
    patch: &Value,
    answer: &Value,
    cpus: Value,
    memory_mib: Value,
) -> Value {
    let mut d = update_document(name, id, patch);
    d["cpus"] = cpus;
    d["memory_mib"] = memory_mib;
    d["applied"] = answer["applied"].clone();
    d["pending_restart"] = answer["pending_restart"].clone();
    d["restarted"] = json!(answer["restarted"].as_bool().unwrap_or(false));
    d["checkpoint"] = answer["checkpoint"].clone();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_instance::RunOutcome;

    fn vm() -> Value {
        json!({
            "id": "0b1c2d3e-0000-4000-8000-000000000001",
            "name": "ci-1",
            "status": "running",
            "guest_ip": "192.168.64.7",
            "created_at": "2026-11-21T10:00:00Z",
            "labels": {
                "fluxvm.mac": "instance",
                "fluxvm.mac.image": "macos-xcode:15",
                "fluxvm.mac.restart": "always",
                "team": "ios"
            },
            "request": {
                "vcpus": 4,
                "memory_mib": 8192,
                "network": {"forwards": [{"host_port": 2222, "guest_port": 22}, {"host_port": 8080, "guest_port": 80}]}
            },
            "disk": "/state/ci-1/disk.raw",
            "workspace": "/state/ci-1"
        })
    }

    fn golden(name: &str, actual: &Value) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/mac_json_golden")
            .join(name);
        let text = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
        }
        let want = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing golden {name}; run with UPDATE_GOLDEN=1"));
        assert_eq!(
            text, want,
            "golden {name} differs: the --json schema changed (UPDATE_GOLDEN=1 to accept, and say so in CHANGELOG.md)"
        );
    }

    /// `vm` is the raw passthrough, so the goldens pin only the contract keys.
    fn without_vm(mut v: Value) -> Value {
        v.as_object_mut().unwrap().remove("vm");
        v
    }

    #[test]
    fn ps_json_matches_golden() {
        let v = vm();
        let doc = ps_document(&[&v]);
        assert_eq!(doc[0]["vm"], v, "the raw record is passed through");
        golden(
            "ps.json",
            &Value::Array(
                doc.as_array()
                    .unwrap()
                    .iter()
                    .cloned()
                    .map(without_vm)
                    .collect(),
            ),
        );
    }

    #[test]
    fn ps_row_has_the_documented_keys_and_types() {
        let row = ps_row(&vm());
        let keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for k in [
            "schema_version",
            "id",
            "short_id",
            "name",
            "image",
            "status",
            "ip",
            "ports",
            "ssh_port",
            "created",
            "restart_policy",
            "project",
            "service",
            "labels",
            "vm",
        ] {
            assert!(keys.contains(&k), "missing {k}");
        }
        assert_eq!(keys.len(), 15, "a new key must be documented: {keys:?}");
        assert_eq!(row["schema_version"], 1);
        assert_eq!(row["ssh_port"], 2222);
        assert_eq!(row["labels"], json!({"team": "ios"}));
        assert_eq!(row["restart_policy"], "always");
    }

    #[test]
    fn ps_row_degrades_to_nulls_not_missing_keys() {
        let row = ps_row(&json!({"name": "x"}));
        assert_eq!(row["status"], "unknown");
        assert_eq!(row["ip"], Value::Null);
        assert_eq!(row["ports"], json!([]));
        assert_eq!(row["ssh_port"], Value::Null);
        assert_eq!(row["restart_policy"], "no");
        assert_eq!(row["labels"], json!({}));
        assert_eq!(row.as_object().unwrap().len(), 15);
    }

    #[test]
    fn ps_document_of_nothing_is_an_empty_array() {
        assert_eq!(ps_document(&[]), json!([]));
    }

    #[test]
    fn inspect_json_matches_golden() {
        let doc = inspect_v1(view::inspect(
            &vm(),
            Some(json!({"user": "admin", "workdir": "/src"})),
        ));
        assert_eq!(doc["schema_version"], 1);
        golden("inspect.json", &without_vm(doc));
    }

    #[test]
    fn create_and_update_json_match_golden() {
        golden(
            "create.json",
            &create_document(
                "ci-1",
                "0b1c2d3e-0000-4000-8000-000000000001",
                "macos-xcode:15",
            ),
        );
        let patch =
            crate::mac_lifecycle::update_patch(Some("always"), &["team=ios".into()], &[]).unwrap();
        golden(
            "update.json",
            &update_document("ci-1", "0b1c2d3e-0000-4000-8000-000000000001", &patch),
        );
        let cleared = crate::mac_lifecycle::update_patch(Some("no"), &[], &[]).unwrap();
        let d = update_document("ci-1", "x", &cleared);
        assert_eq!(
            (&d["restart_policy"], &d["labels"]),
            (&json!("no"), &json!({}))
        );
        let only_labels = crate::mac_lifecycle::update_patch(None, &["a=b".into()], &[]).unwrap();
        assert_eq!(
            update_document("n", "i", &only_labels)["restart_policy"],
            Value::Null
        );
    }

    #[test]
    fn run_json_matches_golden() {
        let kept = RunOutcome {
            name: "ci-1".into(),
            id: "0b1c2d3e-0000-4000-8000-000000000001".into(),
            image: "macos-xcode:15".into(),
            ip: Some("192.168.64.7".into()),
            removed: false,
            ran_command: true,
            detached: true,
            print_name: true,
        };
        golden("run-detached.json", &run_document(&kept));
        let gone = RunOutcome {
            removed: true,
            detached: false,
            print_name: false,
            ..kept
        };
        let d = run_document(&gone);
        assert_eq!(d["status"], "removed");
        golden("run-rm.json", &d);
    }
}
