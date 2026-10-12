// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet pools | pool-create | pool-set | pool-rm`: warm pools (`/fleet/pools`), VMs central keeps booted so a
//! launch of their image starts in seconds.

use super::{Out, Table, Tone, c, confirm, ct, esc, gib, items, rel, s, time, u};
use crate::fleet_client::call;
use anyhow::{Result, bail};
use reqwest::Method;
use serde_json::{Value, json};

/// Parses `--memory`: `16`, `16G`, `16GiB` (GiB) or `4096M`, `4096MiB` (MiB). Returns MiB.
pub fn parse_memory(v: &str) -> Result<u64, String> {
    let t = v.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = t.strip_suffix("gib").or_else(|| t.strip_suffix('g')) {
        (n, 1024)
    } else if let Some(n) = t.strip_suffix("mib").or_else(|| t.strip_suffix('m')) {
        (n, 1)
    } else {
        (t.as_str(), 1024)
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|n| *n >= 512)
        .ok_or_else(|| format!("'{v}' is not a memory size like 16G or 4096M (at least 512M)"))
}

/// `●●○`: ready VMs against `min_ready` (more dots than ten are summarised).
fn dots(ready: u64, want: u64) -> String {
    if want > 10 || ready > 10 {
        return String::new();
    }
    let full = ready.min(want.max(ready));
    let mut d: String = (0..full).map(|_| '●').collect();
    d.extend((full..want).map(|_| '○'));
    d
}

fn state(p: &Value) -> (String, Tone) {
    match s(&p["status"], "state") {
        "ready" => ("Ready".into(), Tone::Ok),
        "filling" => ("Filling".into(), Tone::Warn),
        "backing_off" => ("Waiting to retry".into(), Tone::Bad),
        "paused" => ("Paused (min 0)".into(), Tone::Dim),
        other => (other.to_owned(), Tone::Plain),
    }
}

/// `fluxctl fleet pools`.
pub(super) async fn list(central: &str, token: Option<&str>, out: &Out) -> Result<()> {
    let v = call(central, token, Method::GET, "/fleet/pools", None).await?;
    if !out.human {
        return out.json(&v);
    }
    let list = items(&v);
    if list.is_empty() {
        println!(
            "No warm pools yet. A pool keeps VMs of an image booted, so launching one takes seconds."
        );
        out.next("fluxctl fleet pool-create <name> --image <catalog image> --min-ready 1");
        return Ok(());
    }
    let mut headers = vec![
        "POOL",
        "IMAGE",
        "SIZE",
        "READY",
        "BOOTING",
        "CLAIMS 24H",
        "AVG CLAIM",
        "STATE",
    ];
    if out.wide {
        headers.extend(["OWNER", "MAX", "LAST CLAIM"]);
    }
    let mut t = Table::new(headers);
    for p in &list {
        let st = &p["status"];
        let (ready, min) = (u(st, "ready"), u(p, "min_ready"));
        let (text, tone) = state(p);
        let mut row = vec![
            c(s(p, "name")),
            c(s(p, "image")),
            c(format!(
                "{} vCPUs, {}",
                u(&p["size"], "vcpus"),
                gib(u(&p["size"], "memory_mib"))
            )),
            ct(
                format!("{ready}/{min} {}", dots(ready, min)).trim_end(),
                if ready >= min { Tone::Ok } else { Tone::Warn },
            ),
            c(u(st, "booting")),
            c(u(st, "claimed_24h")),
            ct(
                st.get("avg_claim_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                    .unwrap_or_else(|| "-".into()),
                Tone::Dim,
            ),
            ct(text, tone),
        ];
        if out.wide {
            row.push(c(s(p, "owner")));
            row.push(c(u(p, "max_total")));
            row.push(ct(
                time(st, "last_claim_at")
                    .map(rel)
                    .unwrap_or_else(|| "-".into()),
                Tone::Dim,
            ));
        }
        t.row(row);
    }
    t.print(out);
    for p in &list {
        let err = s(&p["status"], "last_error");
        if !err.is_empty() {
            out.warn(format!("{}: {err}", s(p, "name")));
        }
    }
    Ok(())
}

/// What `pool-create` asks for.
pub(super) struct NewPool {
    pub name: String,
    pub image: String,
    pub min_ready: u32,
    pub max: Option<u32>,
    pub vcpus: Option<u32>,
    pub memory_mib: Option<u64>,
    pub tenant: Option<String>,
    pub node_selector: Vec<(String, String)>,
    pub nodes: Vec<String>,
}

/// The `POST /fleet/pools` body.
fn create_body(a: &NewPool) -> Value {
    let mut b = json!({"name": a.name, "image": a.image, "min_ready": a.min_ready});
    if let Some(m) = a.max {
        b["max_total"] = json!(m);
    }
    if let Some(v) = a.vcpus {
        b["vcpus"] = json!(v);
    }
    if let Some(m) = a.memory_mib {
        b["memory_mib"] = json!(m);
    }
    if let Some(t) = &a.tenant {
        b["tenant"] = json!(t);
    }
    if !a.node_selector.is_empty() {
        b["node_selector"] = json!(
            a.node_selector
                .iter()
                .cloned()
                .collect::<std::collections::BTreeMap<_, _>>()
        );
    }
    if !a.nodes.is_empty() {
        b["nodes"] = json!(a.nodes);
    }
    b
}

/// `fluxctl fleet pool-create`.
pub(super) async fn create(
    central: &str,
    token: Option<&str>,
    out: &Out,
    a: NewPool,
) -> Result<()> {
    let v = call(
        central,
        token,
        Method::POST,
        "/fleet/pools",
        Some(create_body(&a)),
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    out.done(format!(
        "Created warm pool {}: keeps {} of {} booted ({} vCPUs, {} each).",
        out.bold(s(&v, "name")),
        a.min_ready,
        s(&v, "image"),
        u(&v["size"], "vcpus"),
        gib(u(&v["size"], "memory_mib"))
    ));
    if let Some(note) = v.get("note").and_then(Value::as_str) {
        out.warn(note);
    }
    out.next(format!(
        "fluxctl fleet launch --image {}   (starts from a warm VM once one is ready: fluxctl fleet pools)",
        s(&v, "image")
    ));
    Ok(())
}

/// `fluxctl fleet pool-set`.
pub(super) async fn set(
    central: &str,
    token: Option<&str>,
    out: &Out,
    name: &str,
    min_ready: Option<u32>,
    max: Option<u32>,
) -> Result<()> {
    if min_ready.is_none() && max.is_none() {
        bail!("say what to change: --min-ready N (0 pauses the pool) or --max N");
    }
    let mut body = json!({});
    if let Some(m) = min_ready {
        body["min_ready"] = json!(m);
    }
    if let Some(m) = max {
        body["max_total"] = json!(m);
    }
    let v = call(
        central,
        token,
        Method::PATCH,
        &format!("/fleet/pools/{}", esc(name)),
        Some(body),
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    let min = u(&v, "min_ready");
    if min == 0 {
        out.done(format!(
            "{name} paused: it boots nothing new. Its warm VMs are deleted on the next pass."
        ));
    } else {
        out.done(format!(
            "{name} keeps {min} ready (at most {} alive at once).",
            u(&v, "max_total")
        ));
    }
    Ok(())
}

/// `fluxctl fleet pool-rm`.
pub(super) async fn remove(
    central: &str,
    token: Option<&str>,
    out: &Out,
    name: &str,
    yes: bool,
) -> Result<()> {
    if out.human && !yes {
        let p = call(
            central,
            token,
            Method::GET,
            &format!("/fleet/pools/{}", esc(name)),
            None,
        )
        .await?;
        let warm = u(&p["status"], "ready") + u(&p["status"], "booting");
        let q = format!(
            "Delete warm pool {name}? Its {warm} warm VM{} deleted; VMs already launched from it keep running.",
            if warm == 1 { " is" } else { "s are" }
        );
        if !confirm(&q)? {
            println!("Kept {name}.");
            return Ok(());
        }
    }
    let v = call(
        central,
        token,
        Method::DELETE,
        &format!("/fleet/pools/{}", esc(name)),
        None,
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    let drained = v
        .get("drained")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let failed: Vec<&Value> = drained.iter().filter(|d| d["deleted"] != true).collect();
    out.done(format!(
        "Deleted warm pool {name} and {} warm VM{}.",
        drained.len() - failed.len(),
        if drained.len() - failed.len() == 1 {
            ""
        } else {
            "s"
        }
    ));
    for f in failed {
        out.warn(format!(
            "could not delete {} on {}: {}",
            s(f, "name"),
            s(f, "node"),
            s(f, "error")
        ));
    }
    Ok(())
}

/// After a launch: says so when it was answered from a warm pool.
pub(super) fn note_warm(out: &Out, v: &Value) {
    if v.get("warm").and_then(Value::as_bool) == Some(true) {
        let ms = v.get("claim_ms").and_then(Value::as_u64).unwrap_or(0);
        println!(
            "  {}",
            out.dim(format!(
                "Started instantly from warm pool {} ({:.1}s)",
                s(v, "pool"),
                ms as f64 / 1000.0
            ))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sizes() {
        assert_eq!(parse_memory("16"), Ok(16384));
        assert_eq!(parse_memory("16G"), Ok(16384));
        assert_eq!(parse_memory("8gib"), Ok(8192));
        assert_eq!(parse_memory("4096M"), Ok(4096));
        assert_eq!(parse_memory("2048MiB"), Ok(2048));
        assert!(parse_memory("100M").is_err());
        assert!(parse_memory("lots").is_err());
    }

    #[test]
    fn create_bodies_carry_only_what_was_given() {
        let b = create_body(&NewPool {
            name: "ios-ci".into(),
            image: "macos-27-xcode".into(),
            min_ready: 2,
            max: None,
            vcpus: None,
            memory_mib: Some(16384),
            tenant: None,
            node_selector: vec![("zone".into(), "lab".into())],
            nodes: vec![],
        });
        assert_eq!(
            b,
            json!({"name": "ios-ci", "image": "macos-27-xcode", "min_ready": 2, "memory_mib": 16384,
                   "node_selector": {"zone": "lab"}})
        );
        assert_eq!(dots(1, 3), "●○○");
        assert_eq!(dots(3, 2), "●●●");
        assert_eq!(dots(0, 20), "");
    }
}
