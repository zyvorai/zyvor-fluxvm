// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac ps --fleet [INVENTORY]` and `fluxctl mac images --fleet [INVENTORY]`: the same listings, from every Mac of
//! the inventory at once over ssh, one table (or one JSON document with `--json`) with the Mac named in each row. A Mac
//! that cannot be reached is a row in `hosts` with its error, never a silent gap. Read-only.
//!
//! `--format` and `-q` are not available with `--fleet` (they are per-Mac templates); `--filter` and `--all` are passed
//! to each Mac.

use crate::mac_fleet::{Conn, Which, par_map, render_table, resolve, transport};
use crate::mac_fleet_remote::{PRELUDE, remote, sh_quote};
use crate::mac_fleet_ssh::{Target, Transport, explain_ssh_failure};
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct HostStatus {
    pub host: String,
    pub name: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// `mac <verb> --json [-a] [-f X]...` as one remote script.
pub fn list_script(verb: &str, all: bool, filters: &[String]) -> Result<String> {
    if !matches!(verb, "ps" | "images") {
        bail!("not a listing: {verb}");
    }
    let mut s = format!("{PRELUDE}[ -n \"$FLUXCTL\" ] || {{ echo 'fluxctl is not installed on this Mac' >&2; exit 127; }}\n");
    s.push_str(&format!("exec \"$FLUXCTL\" mac {verb} --json"));
    if all {
        s.push_str(" -a");
    }
    for f in filters {
        s.push_str(&format!(" -f {}", sh_quote(f)));
    }
    s.push('\n');
    Ok(s)
}

/// Ask every Mac. Each result is the parsed JSON array or the reason it is missing.
pub fn collect(
    tr: &dyn Transport,
    targets: &[Target],
    jobs: usize,
    script: &str,
    timeout: Duration,
) -> Vec<(HostStatus, Vec<Value>)> {
    par_map(targets, jobs, |t| {
        let mut hs = HostStatus { host: t.label(), name: t.name.clone(), ok: false, error: None };
        match tr.run_timed(t, &remote(script), None, Some(timeout)) {
            Err(e) => hs.error = Some(format!("{e:#}")),
            Ok(o) if o.ssh_failed() => hs.error = Some(explain_ssh_failure(t, &o.stderr)),
            Ok(o) if !o.ok() => {
                hs.error = Some(format!("exit status {}: {}", o.code, o.stderr.trim().lines().last().unwrap_or("")))
            }
            Ok(o) => match serde_json::from_str::<Value>(o.stdout.trim()) {
                Ok(Value::Array(rows)) => {
                    hs.ok = true;
                    return (hs, rows);
                }
                _ => hs.error = Some("the Mac did not answer with a JSON list".into()),
            },
        }
        (hs, vec![])
    })
}

/// A row with the Mac added and the raw `vm` record dropped (it is not part of the contract).
fn tag(host: &HostStatus, mut row: Value) -> Value {
    if let Some(o) = row.as_object_mut() {
        o.remove("vm");
        o.insert("host".into(), json!(host.name));
        o.insert("host_label".into(), json!(host.host));
    }
    row
}

pub fn ps_document(results: &[(HostStatus, Vec<Value>)]) -> Value {
    json!({
        "schema_version": 1,
        "hosts": results.iter().map(|(h, _)| h).collect::<Vec<_>>(),
        "instances": results.iter().flat_map(|(h, rows)| rows.iter().map(|r| tag(h, r.clone()))).collect::<Vec<_>>(),
    })
}

pub fn images_document(results: &[(HostStatus, Vec<Value>)]) -> Value {
    json!({
        "schema_version": 1,
        "hosts": results.iter().map(|(h, _)| h).collect::<Vec<_>>(),
        "images": results.iter().flat_map(|(h, rows)| rows.iter().map(|r| tag(h, r.clone()))).collect::<Vec<_>>(),
    })
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("-").to_owned()
}

fn errors(results: &[(HostStatus, Vec<Value>)]) -> String {
    results
        .iter()
        .filter_map(|(h, _)| h.error.as_ref().map(|e| format!("\n{}: {e}", h.host)))
        .collect()
}

pub fn render_ps(results: &[(HostStatus, Vec<Value>)]) -> String {
    let rows: Vec<Vec<String>> = results
        .iter()
        .flat_map(|(h, rs)| {
            rs.iter().map(|r| {
                vec![h.name.clone(), s(&r["name"]), s(&r["image"]), s(&r["status"]), s(&r["ip"]), s(&r["created"])]
            })
        })
        .collect();
    let mut out = render_table(&["HOST", "NAME", "IMAGE", "STATUS", "IP", "CREATED"], &rows);
    out.push_str(&errors(results));
    out
}

fn size(b: Option<u64>) -> String {
    match b {
        None => "-".into(),
        Some(b) if b >= 1 << 30 => format!("{:.1}G", b as f64 / (1u64 << 30) as f64),
        Some(b) => format!("{}M", b >> 20),
    }
}

pub fn render_images(results: &[(HostStatus, Vec<Value>)]) -> String {
    let rows: Vec<Vec<String>> = results
        .iter()
        .flat_map(|(h, rs)| {
            rs.iter().map(|r| {
                let d = r["digest"].as_str().unwrap_or("");
                vec![
                    h.name.clone(),
                    s(&r["reference"]),
                    if d.is_empty() { "local".into() } else { d.chars().take(19).collect() },
                    size(r["size_bytes"].as_u64()),
                ]
            })
        })
        .collect();
    let mut out = render_table(&["HOST", "IMAGE", "DIGEST", "SIZE"], &rows);
    out.push_str(&errors(results));
    out
}

fn conn() -> Conn {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct W {
        #[command(flatten)]
        c: Conn,
    }
    W::parse_from(["fluxctl"]).c
}

fn run(verb: &str, inventory: &PathBuf, json_out: bool, all: bool, filters: &[String]) -> Result<()> {
    let c = conn();
    let which = Which { hosts: vec![], inventory: Some(inventory.clone()), all: true };
    let (targets, inv) = resolve(&which, &c)?;
    let tr = transport(&c, inv.as_ref());
    let script = list_script(verb, all, filters)?;
    let res = collect(tr.as_ref(), &targets, c.jobs, &script, Duration::from_secs(60));
    let doc = if verb == "ps" { ps_document(&res) } else { images_document(&res) };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else if verb == "ps" {
        println!("{}", render_ps(&res));
    } else {
        println!("{}", render_images(&res));
    }
    let bad = res.iter().filter(|(h, _)| !h.ok).count();
    if bad > 0 {
        bail!("{bad} of {} Macs could not be listed", res.len());
    }
    Ok(())
}

pub fn ps_fleet(inventory: &PathBuf, json_out: bool, o: &crate::mac_more::PsOpts) -> Result<()> {
    if o.format.is_some() || o.quiet {
        bail!("--format and -q do not combine with --fleet: use --json");
    }
    run("ps", inventory, json_out, o.all, &o.filter)
}

pub fn images_fleet(inventory: &PathBuf, json_out: bool, e: &crate::mac_plus::ImagesExtra) -> Result<()> {
    run("images", inventory, json_out, e.all, &e.filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_ssh::fake::{FakeTransport, failed, ok};

    fn targets() -> Vec<Target> {
        vec![Target::parse("admin@a.local").unwrap(), Target::parse("admin@b.local").unwrap(), Target::parse("admin@c.local").unwrap()]
    }

    #[test]
    fn the_script_passes_filters_quoted_and_only_for_known_verbs() {
        let s = list_script("ps", true, &["label=a b".into(), "name=$(x)".into()]).unwrap();
        assert!(s.contains("mac ps --json -a -f 'label=a b' -f 'name=$(x)'"), "{s}");
        assert!(list_script("rm", false, &[]).is_err());
    }

    #[test]
    fn ps_from_three_macs_one_unreachable() {
        let tr = FakeTransport::new(|t, c| {
            assert!(c.contains("mac ps --json"));
            match t.host.as_str() {
                "a.local" => ok(r#"[{"name":"fleet-1","image":"reg/x:1","status":"running","ip":"192.168.64.2","created":"2026-11-21","vm":{"id":"raw"}}]"#),
                "b.local" => ok("[]"),
                _ => failed(255, "Connection refused"),
            }
        });
        let res = collect(&tr, &targets(), 3, &list_script("ps", false, &[]).unwrap(), Duration::from_secs(5));
        let doc = ps_document(&res);
        assert_eq!(doc["instances"].as_array().unwrap().len(), 1);
        assert_eq!(doc["instances"][0]["host"], "a");
        assert!(doc["instances"][0].get("vm").is_none(), "the raw record is not passed through");
        let hosts = doc["hosts"].as_array().unwrap();
        assert_eq!(hosts.iter().filter(|h| h["ok"] == false).count(), 1);
        assert!(hosts[2]["error"].as_str().unwrap().contains("Remote Login"));
        let t = render_ps(&res);
        assert!(t.contains("a     fleet-1") || t.contains("fleet-1"), "{t}");
        assert!(t.contains("c.local") && t.contains("Remote Login"), "{t}");
    }

    #[test]
    fn images_are_aggregated_with_digests_and_sizes() {
        let tr = FakeTransport::new(|_, _| ok(r#"[{"reference":"reg/x:1","digest":"sha256:0123456789abcdef0123","size_bytes":3221225472},{"reference":"local","digest":""}]"#));
        let res = collect(&tr, &targets()[..1], 1, &list_script("images", false, &[]).unwrap(), Duration::from_secs(5));
        let t = render_images(&res);
        assert!(t.contains("3.0G") && t.contains("local") && t.contains("sha256:0123456789ab"), "{t}");
        assert_eq!(images_document(&res)["images"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_reply_that_is_not_a_list_is_an_error_row() {
        let tr = FakeTransport::new(|_, _| ok("hello"));
        let res = collect(&tr, &targets()[..1], 1, &list_script("ps", false, &[]).unwrap(), Duration::from_secs(5));
        assert!(!res[0].0.ok && res[0].0.error.as_deref().unwrap().contains("JSON"));
    }
}
