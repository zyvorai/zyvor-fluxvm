// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac network ls|inspect` and the refusals for `create|rm|connect|disconnect|prune`. A macOS guest here has one network
//! card on Virtualization.framework's NAT (docs/macos-architecture.md, section 6). There is exactly one network, it cannot be created
//! or removed, and what it does not do is printed by `inspect`, not hidden. Nothing here talks to the Mac's network stack: it reads
//! the daemon's instance records.

use crate::mac_errors::coded;
use crate::mac_inspect::{align_grid, ports_text, status};
use anyhow::Result;
use serde_json::{Value, json};

pub const NAME: &str = "nat";

/// What the NAT does not do, as `inspect` prints it. The first line is verified on hardware (docs/macos-architecture.md); no
/// part of this list was re-run on a Mac for this command.
pub const LIMITS: &[&str] = &[
    "guests on this NAT cannot reach each other (only the Mac can reach a guest); to let them talk, publish a port with `-p`",
    "published ports (-p) are TCP only and listen on 127.0.0.1 of this Mac unless you give an address",
    "no DNS names for instances: use the address from `fluxctl mac inspect NAME`",
    "no user-defined networks, no aliases, no `--network` choice",
];

pub fn list(instances: &[&Value], json_out: bool) -> String {
    let n = instances
        .iter()
        .filter(|v| matches!(status(v), "running" | "paused" | "creating"))
        .count();
    if json_out {
        return serde_json::to_string_pretty(&json!([{
            "name": NAME, "driver": "vz-nat", "scope": "local", "instances": n
        }]))
        .unwrap_or_default();
    }
    align_grid(&[
        vec![
            "NAME".into(),
            "DRIVER".into(),
            "SCOPE".into(),
            "INSTANCES".into(),
        ],
        vec![NAME.into(), "vz-nat".into(), "local".into(), n.to_string()],
    ])
}

pub fn inspect(name: &str, instances: &[&Value]) -> Result<Value> {
    if name != NAME {
        return Err(coded(
            format!(
                "no network named {name:?}. A macOS guest has one network, `{NAME}`: use `fluxctl mac network inspect {NAME}`"
            ),
            "MAC_NETWORK_UNSUPPORTED",
        ));
    }
    let mut members = serde_json::Map::new();
    for v in instances
        .iter()
        .filter(|v| matches!(status(v), "running" | "paused" | "creating"))
    {
        members.insert(
            v["name"].as_str().unwrap_or("?").to_string(),
            json!({"ip": v["guest_ip"], "published_ports": ports_text(v)}),
        );
    }
    Ok(json!({
        "name": NAME,
        "driver": "vz-nat",
        "scope": "local",
        "subnet": "assigned by macOS to each guest; see `ip` below",
        "attachable": false,
        "instances": members,
        "limits": LIMITS,
    }))
}

pub fn render_inspect(v: &Value) -> String {
    let mut out = format!(
        "Network {} (driver {}, scope {})\n",
        v["name"].as_str().unwrap_or(NAME),
        v["driver"].as_str().unwrap_or("-"),
        v["scope"].as_str().unwrap_or("-")
    );
    match v["instances"].as_object().filter(|m| !m.is_empty()) {
        None => out.push_str("No instance is attached right now.\n"),
        Some(m) => {
            out.push_str("Attached instances:\n");
            for (n, i) in m {
                out.push_str(&format!(
                    "  {n}  ip {}  ports {}\n",
                    i["ip"].as_str().unwrap_or("-"),
                    i["published_ports"].as_str().unwrap_or("-")
                ));
            }
        }
    }
    out.push_str("What this network does not do:\n");
    for l in LIMITS {
        out.push_str(&format!("  - {l}\n"));
    }
    out
}

/// `network create|rm|connect|disconnect|prune`: always a plain refusal that says what to do instead.
pub fn refuse(verb: &str) -> Result<()> {
    let hint = match verb {
        "create" => {
            "A macOS guest has one NAT network and cannot join others. To let two instances talk, publish a port on one (`fluxctl mac run -p 9000:9000 ...`) and have the other call the Mac's address on that port"
        }
        "rm" | "prune" => "There is only the built-in `nat` network, and nothing to remove",
        _ => {
            "Every instance is already on the one `nat` network; there is nothing to connect or disconnect"
        }
    };
    Err(coded(
        format!("`mac network {verb}` is not supported on macOS guests. {hint}"),
        "MAC_NETWORK_UNSUPPORTED",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str, st: &str) -> Value {
        json!({"name": name, "status": st, "guest_ip": "192.168.64.3",
          "request": {"network": {"forwards": [{"host_port": 8080, "guest_port": 80}]}}})
    }

    #[test]
    fn ls_counts_only_attached_instances() {
        let a = vm("a", "running");
        let b = vm("b", "stopped");
        let t = list(&[&a, &b], false);
        assert!(
            t.contains("nat")
                && t.contains("vz-nat")
                && t.lines().nth(1).unwrap().trim_end().ends_with('1'),
            "{t}"
        );
        let j: Value = serde_json::from_str(&list(&[&a, &b], true)).unwrap();
        assert_eq!(j[0]["instances"], 1);
    }

    #[test]
    fn inspect_states_the_limits_and_lists_members() {
        let a = vm("a", "running");
        let v = inspect("nat", &[&a]).unwrap();
        assert_eq!(v["instances"]["a"]["ip"], "192.168.64.3");
        assert_eq!(v["instances"]["a"]["published_ports"], "8080->80");
        let t = render_inspect(&v);
        assert!(
            t.contains("cannot reach each other") && t.contains("no DNS names"),
            "{t}"
        );
        let e = inspect("backend", &[]).unwrap_err().to_string();
        assert!(
            e.contains("one network") && e.contains("MAC_NETWORK_UNSUPPORTED"),
            "{e}"
        );
    }

    #[test]
    fn create_and_friends_refuse_with_what_to_do() {
        let e = refuse("create").unwrap_err().to_string();
        assert!(
            e.contains("publish a port") && e.contains("MAC_NETWORK_UNSUPPORTED"),
            "{e}"
        );
        assert!(
            refuse("connect")
                .unwrap_err()
                .to_string()
                .contains("nothing to connect")
        );
        assert!(
            refuse("rm")
                .unwrap_err()
                .to_string()
                .contains("nothing to remove")
        );
    }
}
