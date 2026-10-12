// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl fleet …`: every verb, with output for people and for scripts.
//!
//! On a terminal the output is tables, short confirmations and a suggested next command. Piped, or with
//! `-o json`, it is the API's JSON exactly as before, so scripts keep working. `-o table` / `-o wide` force the
//! human form.

use crate::fleet_client::{self, HttpError, call, query};
use crate::output::OutputFormat;
use crate::{
    FleetCommand, FleetImageCommand, FleetLeaseCommand, FleetNetworkCommand, FleetTenantCommand,
};
use anstyle::{AnsiColor, Style};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Display;
use std::io::{IsTerminal, Write};

// ---- output -------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Out {
    pub human: bool,
    pub wide: bool,
    pub color: bool,
}

impl Out {
    /// `explicit` is whether `-o`/`FLUXCTL_OUTPUT` was given; without it a terminal gets the human form.
    pub fn new(format: OutputFormat, explicit: bool) -> Self {
        let tty = std::io::stdout().is_terminal();
        let human = match format {
            OutputFormat::Table | OutputFormat::Wide => true,
            OutputFormat::Json => !explicit && tty,
        };
        let forced = std::env::var_os("FORCE_COLOR").is_some()
            || std::env::var_os("CLICOLOR_FORCE").is_some();
        let color = human && std::env::var_os("NO_COLOR").is_none() && (tty || forced);
        Out {
            human,
            wide: format == OutputFormat::Wide,
            color,
        }
    }

    fn paint(&self, style: Style, text: impl Display) -> String {
        if self.color {
            format!("{}{}{}", style.render(), text, style.render_reset())
        } else {
            text.to_string()
        }
    }
    fn bold(&self, t: impl Display) -> String {
        self.paint(Style::new().bold(), t)
    }
    fn dim(&self, t: impl Display) -> String {
        self.paint(Style::new().dimmed(), t)
    }
    fn green(&self, t: impl Display) -> String {
        self.paint(AnsiColor::Green.on_default(), t)
    }
    fn yellow(&self, t: impl Display) -> String {
        self.paint(AnsiColor::Yellow.on_default(), t)
    }
    fn red(&self, t: impl Display) -> String {
        self.paint(AnsiColor::Red.on_default(), t)
    }
    fn cyan(&self, t: impl Display) -> String {
        self.paint(AnsiColor::Cyan.on_default(), t)
    }

    fn json(&self, v: &Value) -> Result<()> {
        println!("{}", serde_json::to_string_pretty(v)?);
        Ok(())
    }
    fn done(&self, msg: impl Display) {
        println!("{} {msg}", self.green("✓"));
    }
    fn next(&self, cmd: impl Display) {
        println!("{} {}", self.dim("Next:"), self.cyan(cmd));
    }
    fn warn(&self, msg: impl Display) {
        eprintln!("{} {msg}", self.yellow("!"));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Ok,
    Warn,
    Bad,
    Dim,
}

struct Cell {
    text: String,
    tone: Tone,
}

fn c(text: impl Display) -> Cell {
    Cell {
        text: text.to_string(),
        tone: Tone::Plain,
    }
}

fn ct(text: impl Display, tone: Tone) -> Cell {
    Cell {
        text: text.to_string(),
        tone,
    }
}

struct Table {
    headers: Vec<&'static str>,
    rows: Vec<Vec<Cell>>,
}

impl Table {
    fn new(headers: Vec<&'static str>) -> Self {
        Table {
            headers,
            rows: Vec::new(),
        }
    }

    fn row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }

    fn render(&self, out: &Out) -> String {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (w, cell) in widths.iter_mut().zip(r) {
                *w = (*w).max(cell.text.chars().count());
            }
        }
        let last = self.headers.len().saturating_sub(1);
        let mut s = String::new();
        let pad = |text: &str, i: usize| {
            if i == last {
                text.to_owned()
            } else {
                let n = widths[i].saturating_sub(text.chars().count());
                format!("{text}{}", " ".repeat(n))
            }
        };
        let head: Vec<String> = self
            .headers
            .iter()
            .enumerate()
            .map(|(i, h)| out.dim(pad(*h, i)))
            .collect();
        s.push_str(head.join("  ").trim_end());
        s.push('\n');
        for r in &self.rows {
            let cells: Vec<String> = r
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    let t = pad(cell.text.as_str(), i);
                    match cell.tone {
                        Tone::Plain => t,
                        Tone::Ok => out.green(t),
                        Tone::Warn => out.yellow(t),
                        Tone::Bad => out.red(t),
                        Tone::Dim => out.dim(t),
                    }
                })
                .collect();
            s.push_str(cells.join("  ").trim_end());
            s.push('\n');
        }
        s
    }

    fn print(&self, out: &Out) {
        print!("{}", self.render(out));
    }
}

/// Aligned `label  value` lines.
fn kv(out: &Out, pairs: &[(&str, String)]) {
    let w = pairs
        .iter()
        .map(|(k, _)| k.chars().count())
        .max()
        .unwrap_or(0);
    for (k, v) in pairs {
        if v.is_empty() {
            continue;
        }
        let n = w.saturating_sub(k.chars().count());
        println!("{}{}  {v}", out.dim(k), " ".repeat(n));
    }
}

// ---- value helpers ------------------------------------------------------------------------------------------------

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn items(v: &Value) -> Vec<Value> {
    v.get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn num(x: u64) -> String {
    x.to_string()
}

fn gib(mib: u64) -> String {
    if mib % 1024 == 0 {
        format!("{} GiB", mib / 1024)
    } else {
        format!("{:.1} GiB", mib as f64 / 1024.0)
    }
}

fn time(v: &Value, k: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s(v, k))
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn span(secs: i64) -> String {
    match secs {
        a if a < 60 => format!("{a}s"),
        a if a < 3600 => format!("{}m", a / 60),
        a if a < 48 * 3600 => format!("{}h", a / 3600),
        a => format!("{}d", a / 86400),
    }
}

fn rel(t: DateTime<Utc>) -> String {
    let secs = (t - Utc::now()).num_seconds();
    if secs.abs() < 30 {
        "now".into()
    } else if secs > 0 {
        format!("in {}", span(secs))
    } else {
        format!("{} ago", span(-secs))
    }
}

fn local(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%b %-d %H:%M")
        .to_string()
}

fn guest(vm: &Value) -> &'static str {
    let g = vm
        .pointer("/request/apple/guest_os")
        .and_then(Value::as_str)
        .unwrap_or("");
    if g == "macos" { "macOS" } else { "Linux" }
}

fn vm_status(vm: &Value) -> (String, Tone) {
    let st = vm
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| vm.get("state").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_owned();
    let tone = match st.as_str() {
        "running" => Tone::Ok,
        "creating" | "starting" | "paused" => Tone::Warn,
        "failed" => Tone::Bad,
        _ => Tone::Dim,
    };
    (st, tone)
}

/// `●●`, `●○`, `○○`: a Mac's two macOS slots (Apple's licence allows two macOS VMs per Mac).
fn slots(used: u64) -> String {
    (0..2).map(|i| if i < used { '●' } else { '○' }).collect()
}

fn bar(out: &Out, used: u64, total: u64) -> String {
    const W: u64 = 16;
    if total == 0 {
        return String::new();
    }
    let filled = (used.min(total) * W).div_ceil(total).min(W);
    let b = format!(
        "{}{}",
        "█".repeat(filled as usize),
        "░".repeat((W - filled) as usize)
    );
    let ratio = used as f64 / total as f64;
    if ratio >= 1.0 {
        out.red(b)
    } else if ratio >= 0.8 {
        out.yellow(b)
    } else {
        out.green(b)
    }
}

fn lease_line(l: &Value) -> (String, Tone) {
    match time(l, "min_end") {
        Some(t) if t > Utc::now() => (format!("releasable {}", rel(t)), Tone::Plain),
        Some(_) => ("releasable now".into(), Tone::Ok),
        None => (String::new(), Tone::Plain),
    }
}

/// Parses `90s`, `30m`, `8h`, `7d` (or plain seconds).
pub fn parse_duration(v: &str) -> Result<u64, String> {
    let v = v.trim();
    let (num, mult) = match v.chars().last() {
        Some('s') => (&v[..v.len() - 1], 1),
        Some('m') => (&v[..v.len() - 1], 60),
        Some('h') => (&v[..v.len() - 1], 3600),
        Some('d') => (&v[..v.len() - 1], 86400),
        _ => (v, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("'{v}' is not a duration like 30m, 8h or 7d"))
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.chain()
        .find_map(|c| c.downcast_ref::<HttpError>())
        .map(|h| h.status.as_u16())
}

// ---- entry point --------------------------------------------------------------------------------------------------

pub async fn run(
    central: &str,
    token: Option<&str>,
    command: FleetCommand,
    out: Out,
) -> Result<()> {
    let central = central.trim_end_matches('/');
    let result = dispatch(central, token, command, out).await;
    result.map_err(|e| {
        let unreachable = e.chain().any(|c| {
            c.downcast_ref::<reqwest::Error>()
                .is_some_and(|r| r.is_connect() || r.is_timeout())
        });
        if unreachable {
            e.context(format!(
                "could not reach central at {central}\nhint: is `fluxvm-agent central` running there? Set --central or CENTRAL_URL"
            ))
        } else {
            e
        }
    })
}

async fn get(central: &str, token: Option<&str>, path: &str) -> Result<Value> {
    call(central, token, Method::GET, path, None).await
}

async fn dispatch(
    central: &str,
    token: Option<&str>,
    command: FleetCommand,
    out: Out,
) -> Result<()> {
    match command {
        FleetCommand::Status => status(central, token, &out).await,
        FleetCommand::Nodes => {
            let v = fleet_client::list_nodes(central, token).await?;
            if !out.human {
                return out.json(&v);
            }
            let nodes = items(&v);
            if nodes.is_empty() {
                println!("No Macs have registered yet.");
                out.next("run `fluxvm-agent node --central …` on each Mac, or scripts/fleet-sim.sh to try it");
                return Ok(());
            }
            let leases = items(
                &get(central, token, "/fleet/leases?state=active")
                    .await
                    .unwrap_or_default(),
            );
            mac_table(&out, &nodes, &leases).print(&out);
            Ok(())
        }
        FleetCommand::Node { name } => {
            let n = fleet_client::get_node(central, token, &name).await?;
            if !out.human {
                return out.json(&n);
            }
            node_detail(&out, &n);
            Ok(())
        }
        FleetCommand::Cordon { name } => {
            let v = fleet_client::cordon(central, token, &name).await?;
            if !out.human {
                return out.json(&v);
            }
            out.done(format!(
                "{name} cordoned: no new VMs are placed on it. Running VMs are untouched."
            ));
            out.next(format!(
                "fluxctl fleet drain {name}   (or `fluxctl fleet uncordon {name}`)"
            ));
            Ok(())
        }
        FleetCommand::Uncordon { name } => {
            let v = fleet_client::uncordon(central, token, &name).await?;
            if !out.human {
                return out.json(&v);
            }
            out.done(format!("{name} uncordoned: it takes new VMs again."));
            Ok(())
        }
        FleetCommand::Deregister { name } => {
            fleet_client::deregister(central, token, &name).await?;
            if !out.human {
                println!("{{\"ok\":true}}");
                return Ok(());
            }
            out.done(format!("Forgot {name}."));
            Ok(())
        }
        FleetCommand::Capacity => {
            let v = fleet_client::capacity(central, token).await?;
            if !out.human {
                return out.json(&v);
            }
            capacity_block(&out, &v);
            Ok(())
        }
        FleetCommand::Create {
            spec,
            node,
            node_selector,
        } => {
            let body: Value = serde_json::from_slice(&std::fs::read(&spec)?)
                .with_context(|| format!("parsing {}", spec.display()))?;
            let v = fleet_client::create_vm(
                central,
                token,
                body,
                node,
                node_selector.into_iter().collect(),
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            launched(&out, &v, None);
            Ok(())
        }
        FleetCommand::Launch {
            name,
            image,
            disk,
            guest,
            vcpus,
            memory_gib,
            mac,
            network,
            ttl,
        } => {
            launch(
                central,
                token,
                &out,
                LaunchArgs {
                    name,
                    image,
                    disk,
                    guest,
                    vcpus,
                    memory_gib,
                    mac,
                    network,
                    ttl,
                },
            )
            .await
        }
        FleetCommand::Vms => {
            let v = fleet_client::list_vms(central, token).await?;
            if !out.human {
                return out.json(&v);
            }
            let vms = items(&v);
            if vms.is_empty() {
                println!("No VMs yet.");
                out.next("fluxctl fleet launch --image <name>   (see `fluxctl fleet images`)");
            } else {
                vm_table(&out, &vms, true).print(&out);
            }
            for n in v
                .get("unreachable_nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.warn(format!(
                    "{} did not answer, so its VMs are not listed",
                    s(n, "node")
                ));
            }
            Ok(())
        }
        FleetCommand::NodeVms { name } => {
            let v = fleet_client::node_vms(central, token, &name).await?;
            if !out.human {
                return out.json(&v);
            }
            let vms = items(&v);
            if vms.is_empty() {
                println!("No VMs on {name}.");
            } else {
                vm_table(&out, &vms, false).print(&out);
            }
            Ok(())
        }
        FleetCommand::Delete { node, id } => {
            fleet_client::delete_vm(central, token, &node, id).await?;
            if !out.human {
                println!("{{\"ok\":true}}");
                return Ok(());
            }
            out.done(format!("Deleted VM {id} on {node}."));
            Ok(())
        }
        FleetCommand::Rm { vm, yes } => {
            let (node, id, name) = resolve_vm(central, token, &vm).await?;
            if out.human
                && !yes
                && !confirm(&format!(
                    "Delete {name} on {node}? Its disk is deleted too."
                ))?
            {
                println!("Kept {name}.");
                return Ok(());
            }
            call(
                central,
                token,
                Method::DELETE,
                &format!("/fleet/vms/{}/{}", esc(&node), esc(&id)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&json!({"ok": true, "node": node, "id": id}));
            }
            out.done(format!("Deleted {name} on {node}."));
            Ok(())
        }
        FleetCommand::Exec {
            node,
            id,
            timeout,
            command,
        } => exec(central, token, &out, &node, &id, timeout, command.join(" ")).await,
        FleetCommand::Run {
            vm,
            timeout,
            command,
        } => {
            let (node, id, _) = resolve_vm(central, token, &vm).await?;
            exec(central, token, &out, &node, &id, timeout, command.join(" ")).await
        }
        FleetCommand::Screen {
            vm,
            file,
            max_width,
        } => {
            let (node, id, name) = resolve_vm(central, token, &vm).await?;
            let (ctype, bytes) = fleet_client::get_bytes(
                central,
                token,
                &format!(
                    "/fleet/vms/{}/{}/screen?max_width={max_width}",
                    esc(&node),
                    esc(&id)
                ),
            )
            .await
            .map_err(|e| {
                if status_of(&e) == Some(409) {
                    e.context(format!("{name} has no screen to show (it needs to be a running vz VM with a display)"))
                } else {
                    e
                }
            })?;
            let ext = if ctype.contains("svg") { "svg" } else { "png" };
            let path = file.unwrap_or_else(|| std::path::PathBuf::from(format!("{name}.{ext}")));
            std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            if !out.human {
                return out
                    .json(&json!({"file": path, "bytes": bytes.len(), "content_type": ctype}));
            }
            out.done(format!("Saved the screen of {name} to {}.", path.display()));
            Ok(())
        }
        FleetCommand::Events { limit, tenant } => {
            let q = query(&[("limit", Some(limit.to_string())), ("tenant", tenant)]);
            let v = get(central, token, &format!("/fleet/events{q}")).await?;
            if !out.human {
                return out.json(&v);
            }
            let list = items(&v);
            if list.is_empty() {
                println!("Nothing has happened yet.");
                return Ok(());
            }
            let mut t = Table::new(vec!["WHEN", "WHO", "WHAT", "RESULT"]);
            for e in &list {
                let code = u(e, "status");
                let target = match s(e, "target") {
                    "" => String::new(),
                    x => format!(" {x}"),
                };
                t.row(vec![
                    ct(time(e, "time").map(rel).unwrap_or_default(), Tone::Dim),
                    c(s(e, "actor")),
                    c(format!("{} {}{target}", s(e, "method"), s(e, "path"))),
                    if code < 400 {
                        ct("done", Tone::Ok)
                    } else {
                        ct(format!("refused ({code})"), Tone::Bad)
                    },
                ]);
            }
            t.print(&out);
            Ok(())
        }
        FleetCommand::Images { command } => images(central, token, &out, command).await,
        FleetCommand::Networks { command } => networks(central, token, &out, command).await,
        FleetCommand::Tenant { command } => tenant(central, token, &out, command).await,
        FleetCommand::Lease { command } => lease(central, token, &out, command).await,
        FleetCommand::Drain { name, mode, force } => {
            let q = query(&[
                ("mode", Some(mode)),
                ("force", force.then(|| "true".to_string())),
            ]);
            let v = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/nodes/{}/drain{q}", esc(&name)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            let actions = v
                .get("actions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let count = |a: &str| actions.iter().filter(|x| s(x, "action") == a).count();
            out.done(format!(
                "{name} is draining ({}): {} deleted, {} moved, {} still running.",
                s(&v, "mode"),
                count("evicted"),
                count("rescheduled"),
                u(&v, "remaining")
            ));
            for f in actions.iter().filter(|x| s(x, "action") == "failed") {
                out.warn(format!(
                    "VM {} could not be handled: {}",
                    s(f, "vm"),
                    s(f, "error")
                ));
            }
            if v.get("done").and_then(Value::as_bool) == Some(true) {
                out.next(format!(
                    "{name} is empty. Uncordon it after maintenance: fluxctl fleet uncordon {name}"
                ));
            } else {
                out.next(format!("fluxctl fleet drain-status {name}"));
            }
            Ok(())
        }
        FleetCommand::DrainStatus { name } => {
            let v = get(
                central,
                token,
                &format!("/fleet/nodes/{}/drain", esc(&name)),
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            let yn = |k: &str| {
                if v.get(k).and_then(Value::as_bool) == Some(true) {
                    "yes".to_string()
                } else {
                    "no".to_string()
                }
            };
            kv(
                &out,
                &[
                    ("Mac", name.clone()),
                    ("Cordoned", yn("cordoned")),
                    ("Draining", yn("draining")),
                    ("VMs left", u(&v, "vm_count").to_string()),
                    ("Needs scrub", yn("scrub_pending")),
                    (
                        "Leased to",
                        v.get("lease")
                            .map(|l| s(l, "tenant").to_owned())
                            .unwrap_or_default(),
                    ),
                ],
            );
            Ok(())
        }
        FleetCommand::Scrub { name, force } => {
            let q = query(&[("force", force.then(|| "true".to_string()))]);
            let v = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/nodes/{}/scrub{q}", esc(&name)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            let deleted = v
                .get("deleted")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            out.done(format!(
                "{name} scrubbed{}: it can be leased again.",
                if deleted > 0 {
                    format!(" ({deleted} VMs deleted)")
                } else {
                    String::new()
                }
            ));
            Ok(())
        }
        FleetCommand::Usage {
            from,
            to,
            tenant,
            csv,
        } => {
            let q = query(&[
                ("from", from),
                ("to", to),
                ("tenant", tenant),
                ("format", csv.then(|| "csv".to_string())),
            ]);
            let v = get(central, token, &format!("/fleet/usage{q}")).await?;
            match v {
                Value::String(text) => {
                    print!("{text}");
                    Ok(())
                }
                v if !out.human => out.json(&v),
                v => {
                    usage_table(&out, &v).print(&out);
                    println!(
                        "{}",
                        out.dim("Hours. Leases are billed for at least 24 hours each. `--csv` for a spreadsheet.")
                    );
                    Ok(())
                }
            }
        }
        FleetCommand::Reconcile => {
            let v = call(central, token, Method::POST, "/fleet/reconcile", None).await?;
            if !out.human {
                return out.json(&v);
            }
            let n = |k: &str| {
                v.get(k)
                    .map(|x| {
                        x.as_array()
                            .map_or_else(|| x.as_u64().unwrap_or(0), |a| a.len() as u64)
                    })
                    .unwrap_or(0)
            };
            out.done(format!(
                "Reconciled: {} leases ended, {} VM records closed, {} network keys rotated.",
                n("expired_leases"),
                n("closed_ledger_entries"),
                n("rotated_overlay_keys")
            ));
            for e in v
                .get("errors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.warn(format!("{}: {}", s(e, "node"), s(e, "error")));
            }
            Ok(())
        }
    }
}

fn esc(s: &str) -> String {
    fleet_client::query_escape(s)
}

// ---- status -------------------------------------------------------------------------------------------------------

async fn status(central: &str, token: Option<&str>, out: &Out) -> Result<()> {
    let me = match get(central, token, "/fleet/tenants/me").await {
        Ok(me) => Some(me),
        Err(e) if status_of(&e) == Some(400) => None,
        Err(e) => return Err(e),
    };
    let leases = items(&get(central, token, "/fleet/leases?state=active").await?);
    let vms_v = fleet_client::list_vms(central, token).await?;
    let vms = items(&vms_v);
    match me {
        Some(me) => {
            let mut nodes = Vec::new();
            for l in &leases {
                if let Ok(n) = get(
                    central,
                    token,
                    &format!("/fleet/nodes/{}", esc(s(l, "node"))),
                )
                .await
                {
                    nodes.push(n);
                }
            }
            if !out.human {
                return out
                    .json(&json!({"tenant": me, "leases": leases, "macs": nodes, "vms": vms}));
            }
            tenant_status(out, central, &me, &leases, &nodes, &vms, token).await
        }
        None => {
            let nodes = items(&fleet_client::list_nodes(central, token).await?);
            let cap = fleet_client::capacity(central, token).await?;
            let tenants = items(
                &get(central, token, "/fleet/tenants")
                    .await
                    .unwrap_or_default(),
            );
            let mut drains = HashMap::new();
            for n in &nodes {
                if let Ok(d) = get(
                    central,
                    token,
                    &format!("/fleet/nodes/{}/drain", esc(s(n, "name"))),
                )
                .await
                {
                    drains.insert(s(n, "name").to_owned(), d);
                }
            }
            if !out.human {
                return out.json(&json!({"capacity": cap, "nodes": nodes, "leases": leases, "tenants": tenants, "vms": vms}));
            }
            println!(
                "{}  {}",
                out.bold("Fleet"),
                out.dim(format!("{central}, operator"))
            );
            println!();
            let healthy = nodes
                .iter()
                .filter(|n| n.get("healthy").and_then(Value::as_bool) == Some(true))
                .count();
            let cordoned = nodes
                .iter()
                .filter(|n| n.get("cordoned").and_then(Value::as_bool) == Some(true))
                .count();
            kv(
                out,
                &[
                    (
                        "Macs",
                        format!(
                            "{healthy} of {} healthy, {} leased, {cordoned} cordoned",
                            nodes.len(),
                            leases.len()
                        ),
                    ),
                    (
                        "vCPUs",
                        meter(
                            out,
                            u(&cap, "vcpus_total").saturating_sub(u(&cap, "vcpus_free")),
                            u(&cap, "vcpus_total"),
                            |x| x.to_string(),
                        ),
                    ),
                    (
                        "Memory",
                        meter(
                            out,
                            u(&cap, "memory_mib_total").saturating_sub(u(&cap, "memory_mib_free")),
                            u(&cap, "memory_mib_total"),
                            gib,
                        ),
                    ),
                    ("VMs", vms.len().to_string()),
                    ("Tenants", tenants.len().to_string()),
                ],
            );
            let mut attention: Vec<String> = Vec::new();
            for n in &nodes {
                let name = s(n, "name");
                if n.get("healthy").and_then(Value::as_bool) == Some(false) {
                    let seen = time(n, "last_seen").map(rel).unwrap_or_default();
                    attention.push(out.red(format!(
                        "✗ {name} has not reported since {seen}. Check the Mac and its node agent."
                    )));
                }
                let d = drains.get(name);
                if d.and_then(|d| d.get("scrub_pending"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    attention.push(out.yellow(format!("! {name} needs a scrub before it can be leased again: fluxctl fleet scrub {name}")));
                }
                if d.and_then(|d| d.get("draining")).and_then(Value::as_bool) == Some(true)
                    && u(n, "vm_count") == 0
                {
                    attention.push(out.yellow(format!(
                        "! {name} has finished draining: fluxctl fleet uncordon {name}"
                    )));
                }
            }
            for un in vms_v
                .get("unreachable_nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                attention.push(out.red(format!("✗ {} did not answer the VM list.", s(un, "node"))));
            }
            println!();
            if attention.is_empty() {
                println!(
                    "{}",
                    out.green("Every Mac is reporting. Nothing needs you.")
                );
            } else {
                println!("{}", out.bold("Needs attention"));
                for a in attention {
                    println!("  {a}");
                }
            }
            if !nodes.is_empty() {
                println!();
                mac_table(out, &nodes, &leases).print(out);
            }
            Ok(())
        }
    }
}

fn meter(out: &Out, used: u64, total: u64, fmt: fn(u64) -> String) -> String {
    if total == 0 {
        return fmt(used);
    }
    format!("{}  {} of {}", bar(out, used, total), fmt(used), fmt(total))
}

async fn tenant_status(
    out: &Out,
    central: &str,
    me: &Value,
    leases: &[Value],
    nodes: &[Value],
    vms: &[Value],
    token: Option<&str>,
) -> Result<()> {
    let id = s(me, "id");
    let display = s(me, "display_name");
    println!(
        "{}  {}",
        out.bold(if display.is_empty() { id } else { display }),
        out.dim(format!("tenant {id}, {central}"))
    );
    println!();
    let q = me.get("quota").cloned().unwrap_or_default();
    let used = me.get("in_use").cloned().unwrap_or_default();
    let line = |used: u64, max: Option<u64>, fmt: fn(u64) -> String| match max {
        Some(m) => meter(out, used, m, fmt),
        None => format!("{}  {}", fmt(used), out.dim("(no limit)")),
    };
    kv(
        out,
        &[
            (
                "Macs",
                line(
                    u(&used, "leased_hosts"),
                    q.get("max_leased_hosts").and_then(Value::as_u64),
                    num,
                ),
            ),
            (
                "VMs",
                line(
                    u(&used, "vms"),
                    q.get("max_vms").and_then(Value::as_u64),
                    num,
                ),
            ),
            (
                "vCPUs",
                line(
                    u(&used, "vcpus"),
                    q.get("max_vcpus").and_then(Value::as_u64),
                    num,
                ),
            ),
            (
                "Memory",
                line(
                    u(&used, "memory_mib"),
                    q.get("max_memory_mib").and_then(Value::as_u64),
                    gib,
                ),
            ),
        ],
    );
    println!();
    if leases.is_empty() {
        println!("You have no Macs. A leased Mac is yours alone, for at least 24 hours.");
        out.next("fluxctl fleet lease create");
        return Ok(());
    }
    let mut t = Table::new(vec!["MAC", "LEASE", "MACOS", "VMS", "FREE"]);
    for l in leases {
        let name = s(l, "node");
        let n = nodes.iter().find(|n| s(n, "name") == name);
        let on: Vec<&Value> = vms.iter().filter(|v| s(v, "node") == name).collect();
        let macos = n
            .and_then(|n| n.pointer("/apple/macos_guests"))
            .and_then(Value::as_u64)
            .unwrap_or_else(|| on.iter().filter(|v| guest(v) == "macOS").count() as u64);
        let (ll, tone) = lease_line(l);
        t.row(vec![
            c(name),
            ct(ll, tone),
            c(slots(macos)),
            c(on.len()),
            ct(
                n.map(|n| {
                    format!(
                        "{} vCPUs, {}",
                        u(n, "free_vcpus"),
                        gib(u(n, "free_memory_mib"))
                    )
                })
                .unwrap_or_default(),
                Tone::Dim,
            ),
        ]);
    }
    t.print(out);
    println!();
    if vms.is_empty() {
        let images = items(
            &get(central, token, "/fleet/images")
                .await
                .unwrap_or_default(),
        );
        let first = images
            .iter()
            .find(|i| s(i, "guest_os") == "macos")
            .or(images.first())
            .map(|i| s(i, "name").to_owned());
        match first {
            Some(img) => out.next(format!("fluxctl fleet launch --image {img}")),
            None => out.next("fluxctl fleet launch --disk <path on the Mac> --guest macos"),
        }
    } else {
        out.next("fluxctl fleet vms");
    }
    Ok(())
}

fn capacity_block(out: &Out, v: &Value) {
    kv(
        out,
        &[
            (
                "Macs",
                format!(
                    "{} of {} healthy, {} take new VMs",
                    u(v, "nodes_healthy"),
                    u(v, "nodes_total"),
                    u(v, "nodes_schedulable")
                ),
            ),
            (
                "vCPUs",
                meter(
                    out,
                    u(v, "vcpus_total").saturating_sub(u(v, "vcpus_free")),
                    u(v, "vcpus_total"),
                    |x| x.to_string(),
                ),
            ),
            (
                "Memory",
                meter(
                    out,
                    u(v, "memory_mib_total").saturating_sub(u(v, "memory_mib_free")),
                    u(v, "memory_mib_total"),
                    gib,
                ),
            ),
            ("VMs", u(v, "vm_count").to_string()),
        ],
    );
}

// ---- tables -------------------------------------------------------------------------------------------------------

fn mac_table(out: &Out, nodes: &[Value], leases: &[Value]) -> Table {
    let mut headers = vec!["MAC", "STATE", "LEASE", "MACOS", "VMS", "FREE", "SEEN"];
    if out.wide {
        headers.extend(["LABELS", "URL"]);
    }
    let mut t = Table::new(headers);
    let mut sorted: Vec<&Value> = nodes.iter().collect();
    sorted.sort_by(|a, b| s(a, "name").cmp(s(b, "name")));
    for n in sorted {
        let name = s(n, "name");
        let lease = leases.iter().find(|l| s(l, "node") == name);
        let (state, tone) = if n.get("healthy").and_then(Value::as_bool) == Some(false) {
            ("not responding".to_owned(), Tone::Bad)
        } else if n.get("cordoned").and_then(Value::as_bool) == Some(true) {
            ("cordoned".to_owned(), Tone::Warn)
        } else if let Some(l) = lease {
            (format!("leased to {}", s(l, "tenant")), Tone::Ok)
        } else {
            ("available".to_owned(), Tone::Plain)
        };
        let macos = n.pointer("/apple/macos_guests").and_then(Value::as_u64);
        let mut row = vec![
            c(name),
            ct(state, tone),
            lease.map_or_else(
                || ct("-", Tone::Dim),
                |l| {
                    let (t, tone) = lease_line(l);
                    ct(t, tone)
                },
            ),
            if n.get("apple").is_some_and(|a| !a.is_null()) {
                c(slots(macos.unwrap_or(0)))
            } else {
                ct("Linux", Tone::Dim)
            },
            c(u(n, "vm_count")),
            c(format!(
                "{} vCPUs, {}",
                u(n, "free_vcpus"),
                gib(u(n, "free_memory_mib"))
            )),
            ct(time(n, "last_seen").map(rel).unwrap_or_default(), Tone::Dim),
        ];
        if out.wide {
            let labels = n
                .get("labels")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("")))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            row.push(c(if labels.is_empty() {
                "-".into()
            } else {
                labels
            }));
            row.push(ct(s(n, "fluxvm_url"), Tone::Dim));
        }
        t.row(row);
    }
    t
}

fn node_detail(out: &Out, n: &Value) {
    let a = n.get("apple").cloned().unwrap_or(Value::Null);
    let caps: Vec<&str> = [
        ("nested_virtualization", "nested virtualization"),
        ("vmnet", "vmnet networks"),
        ("secure_boot", "Secure Boot"),
        ("rosetta", "Rosetta"),
    ]
    .iter()
    .filter(|(k, _)| a.get(*k).and_then(Value::as_bool) == Some(true))
    .map(|(_, label)| *label)
    .collect();
    let healthy = n.get("healthy").and_then(Value::as_bool);
    kv(
        out,
        &[
            ("Mac", out.bold(s(n, "name"))),
            (
                "State",
                match (healthy, n.get("cordoned").and_then(Value::as_bool)) {
                    (Some(false), _) => out.red("not responding"),
                    (_, Some(true)) => out.yellow("cordoned"),
                    _ => out.green("healthy"),
                },
            ),
            (
                "Size",
                format!(
                    "{} vCPUs, {}",
                    u(n, "vcpus_total"),
                    gib(u(n, "memory_mib_total"))
                ),
            ),
            (
                "Free",
                format!(
                    "{} vCPUs, {}",
                    u(n, "free_vcpus"),
                    gib(u(n, "free_memory_mib"))
                ),
            ),
            ("VMs", u(n, "vm_count").to_string()),
            (
                "macOS slots",
                if a.is_null() {
                    String::new()
                } else {
                    format!(
                        "{}  {} of 2 in use",
                        slots(u(&a, "macos_guests")),
                        u(&a, "macos_guests")
                    )
                },
            ),
            ("Capabilities", caps.join(", ")),
            (
                "Last report",
                time(n, "last_seen").map(rel).unwrap_or_default(),
            ),
            ("Address", s(n, "fluxvm_url").to_owned()),
        ],
    );
}

fn vm_table(out: &Out, vms: &[Value], with_mac: bool) -> Table {
    let mut headers = vec!["NAME", "STATUS"];
    if with_mac {
        headers.push("MAC");
    }
    headers.extend(["GUEST", "SIZE", "AGE"]);
    if out.wide {
        headers.extend(["TENANT", "EXPIRES", "ID"]);
    } else {
        headers.push("ID");
    }
    let mut t = Table::new(headers);
    let mut sorted: Vec<&Value> = vms.iter().collect();
    sorted.sort_by(|a, b| s(a, "name").cmp(s(b, "name")));
    for v in sorted {
        let (st, tone) = vm_status(v);
        let req = v.get("request").cloned().unwrap_or_default();
        let id = s(v, "id");
        let mut row = vec![c(s(v, "name")), ct(st, tone)];
        if with_mac {
            row.push(c(s(v, "node")));
        }
        row.push(c(guest(v)));
        row.push(c(format!(
            "{} vCPUs, {}",
            u(&req, "vcpus"),
            gib(u(&req, "memory_mib"))
        )));
        row.push(ct(
            time(v, "created_at")
                .map(|t| span((Utc::now() - t).num_seconds().max(0)))
                .unwrap_or_default(),
            Tone::Dim,
        ));
        if out.wide {
            row.push(c(match s(&req, "tenant") {
                "" => "-",
                t => t,
            }));
            row.push(ct(
                time(v, "expires_at").map(rel).unwrap_or_else(|| "-".into()),
                Tone::Dim,
            ));
            row.push(ct(id, Tone::Dim));
        } else {
            row.push(ct(id.chars().take(8).collect::<String>(), Tone::Dim));
        }
        t.row(row);
    }
    t
}

fn usage_table(out: &Out, v: &Value) -> Table {
    let mut t = Table::new(vec![
        "TENANT",
        "VM",
        "VCPU",
        "MEMORY GIB",
        "MACOS VM",
        "LEASED",
        "BILLABLE",
    ]);
    let h = |x: u64| format!("{:.1}", x as f64 / 3600.0);
    for r in v
        .get("tenants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        t.row(vec![
            c(match s(r, "tenant") {
                "" => "(none)",
                x => x,
            }),
            c(h(u(r, "vm_seconds"))),
            c(h(u(r, "vcpu_seconds"))),
            c(format!(
                "{:.1}",
                u(r, "memory_mib_seconds") as f64 / 1024.0 / 3600.0
            )),
            c(h(u(r, "macos_vm_seconds"))),
            c(h(u(r, "lease_seconds"))),
            ct(h(u(r, "billable_lease_seconds")), Tone::Ok),
        ]);
    }
    let _ = out;
    t
}

// ---- VMs ----------------------------------------------------------------------------------------------------------

/// Finds a VM by name, full id or id prefix (6+ characters). Returns (node, id, name).
async fn resolve_vm(
    central: &str,
    token: Option<&str>,
    key: &str,
) -> Result<(String, String, String)> {
    let vms = items(&fleet_client::list_vms(central, token).await?);
    let exact: Vec<&Value> = vms
        .iter()
        .filter(|v| s(v, "name") == key || s(v, "id") == key)
        .collect();
    let found: Vec<&Value> = if exact.is_empty() && key.len() >= 6 {
        vms.iter().filter(|v| s(v, "id").starts_with(key)).collect()
    } else {
        exact
    };
    match found.as_slice() {
        [v] => Ok((
            s(v, "node").to_owned(),
            s(v, "id").to_owned(),
            s(v, "name").to_owned(),
        )),
        [] => bail!("no VM named '{key}'\nhint: list your VMs with `fluxctl fleet vms`"),
        many => {
            let list: Vec<String> = many
                .iter()
                .map(|v| format!("  {} on {} ({})", s(v, "name"), s(v, "node"), s(v, "id")))
                .collect();
            bail!(
                "'{key}' matches {} VMs; use the ID:\n{}",
                many.len(),
                list.join("\n")
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn exec(
    central: &str,
    token: Option<&str>,
    out: &Out,
    node: &str,
    id: &str,
    timeout: Option<u64>,
    command: String,
) -> Result<()> {
    let v = call(
        central,
        token,
        Method::POST,
        &format!("/fleet/vms/{}/{}/exec", esc(node), esc(id)),
        Some(json!({"command": command, "timeout_seconds": timeout})),
    )
    .await?;
    if !out.human {
        return out.json(&v);
    }
    print!("{}", s(&v, "stdout"));
    eprint!("{}", s(&v, "stderr"));
    std::io::stdout().flush()?;
    let code = v
        .get("exit_code")
        .or_else(|| v.get("exit_status"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if code != 0 {
        std::process::exit(i32::try_from(code).unwrap_or(1));
    }
    Ok(())
}

struct LaunchArgs {
    name: Option<String>,
    image: Option<String>,
    disk: Option<String>,
    guest: Option<String>,
    vcpus: Option<u8>,
    memory_gib: Option<u64>,
    mac: Option<String>,
    network: Vec<String>,
    ttl: Option<u64>,
}

async fn launch(central: &str, token: Option<&str>, out: &Out, a: LaunchArgs) -> Result<()> {
    let entry = match (&a.image, &a.disk) {
        (Some(name), _) => {
            let catalog = items(&get(central, token, "/fleet/images").await?);
            match catalog.iter().find(|i| s(i, "name") == name) {
                Some(e) => Some(e.clone()),
                None => {
                    let names: Vec<&str> = catalog.iter().map(|i| s(i, "name")).collect();
                    if names.is_empty() {
                        bail!(
                            "no image named '{name}': the catalog is empty\nhint: the operator adds images with `fluxctl fleet images add`, or launch from a disk with --disk PATH --guest macos"
                        );
                    }
                    bail!(
                        "no image named '{name}'\nhint: available: {}",
                        names.join(", ")
                    );
                }
            }
        }
        (None, Some(_)) => None,
        (None, None) => bail!(
            "say what to run: --image NAME (see `fluxctl fleet images`) or --disk PATH --guest macos|linux"
        ),
    };
    let guest_os = match (&a.guest, &entry) {
        (Some(g), _) => g.clone(),
        (None, Some(e)) => s(e, "guest_os").to_owned(),
        (None, None) => bail!("--disk needs --guest macos or --guest linux"),
    };
    let path = match (&a.disk, &entry) {
        (Some(d), _) => d.clone(),
        (None, Some(e)) => s(e, "image").to_owned(),
        (None, None) => unreachable!("checked above"),
    };
    let vcpus = a
        .vcpus
        .map(u64::from)
        .or_else(|| {
            entry
                .as_ref()
                .and_then(|e| e.get("vcpus"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(4);
    let memory_mib = a
        .memory_gib
        .map(|g| g.saturating_mul(1024))
        .or_else(|| {
            entry
                .as_ref()
                .and_then(|e| e.get("memory_mib"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(8192);
    let name = match a.name {
        Some(n) => n,
        None => {
            let taken: Vec<String> = items(&fleet_client::list_vms(central, token).await?)
                .iter()
                .map(|v| s(v, "name").to_owned())
                .collect();
            let base: String = entry
                .as_ref()
                .map(|e| s(e, "name"))
                .unwrap_or("vm")
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '-' {
                        ch
                    } else {
                        '-'
                    }
                })
                .collect();
            (1..)
                .map(|i| format!("{base}-{i}"))
                .find(|n| !taken.contains(n))
                .unwrap_or_else(|| base.clone())
        }
    };
    let mut body = json!({
        "name": name,
        "backend": "vz",
        "image": path,
        "vcpus": vcpus,
        "memory_mib": memory_mib,
        "apple": {"guest_os": guest_os},
    });
    if let Some(d) = entry
        .as_ref()
        .and_then(|e| e.get("disk_size_gib"))
        .filter(|d| !d.is_null())
    {
        body["disk_size_gib"] = d.clone();
    }
    if !a.network.is_empty() {
        body["apple"]["networks"] = json!(
            a.network
                .iter()
                .map(|n| json!({"name": n}))
                .collect::<Vec<_>>()
        );
    }
    if let Some(t) = a.ttl {
        body["ttl_seconds"] = json!(t);
    }
    let r = fleet_client::create_vm(central, token, body, a.mac, HashMap::new()).await;
    let v = match r {
        Ok(v) => v,
        Err(e)
            if guest_os == "macos"
                && matches!(status_of(&e), Some(503) | Some(409) | Some(502)) =>
        {
            return Err(e.context(
                "no Mac could take it\nhint: macOS VMs run on Macs you lease, two per Mac. See `fluxctl fleet status`, or lease another Mac: fluxctl fleet lease create",
            ));
        }
        Err(e) => return Err(e),
    };
    if !out.human {
        return out.json(&v);
    }
    let label = entry
        .as_ref()
        .map(|e| match s(e, "display_name") {
            "" => s(e, "name").to_owned(),
            d => d.to_owned(),
        })
        .unwrap_or_else(|| path.clone());
    launched(
        out,
        &v,
        Some(format!("{label}, {vcpus} vCPUs, {}", gib(memory_mib))),
    );
    Ok(())
}

fn launched(out: &Out, v: &Value, detail: Option<String>) {
    let vm = v.get("vm").cloned().unwrap_or_default();
    let name = s(&vm, "name");
    let node = s(v, "node");
    out.done(format!(
        "Launched {} on {node}{}",
        out.bold(name),
        detail.map(|d| format!(" ({d})")).unwrap_or_default()
    ));
    kv(out, &[("  ID", s(&vm, "id").to_owned())]);
    let probe = if guest(&vm) == "macOS" {
        "sw_vers"
    } else {
        "uname -a"
    };
    out.next(format!("fluxctl fleet run {name} -- {probe}"));
}

// ---- images -------------------------------------------------------------------------------------------------------

async fn images(
    central: &str,
    token: Option<&str>,
    out: &Out,
    command: Option<FleetImageCommand>,
) -> Result<()> {
    match command.unwrap_or(FleetImageCommand::List) {
        FleetImageCommand::List => {
            let v = get(central, token, "/fleet/images").await?;
            if !out.human {
                return out.json(&v);
            }
            let list = items(&v);
            if list.is_empty() {
                println!("The image catalog is empty.");
                out.next("fluxctl fleet images add <name> --path <disk on the Macs> --guest macos   (operator)");
                return Ok(());
            }
            let mut headers = vec!["NAME", "GUEST", "DEFAULT SIZE", "DESCRIPTION"];
            if out.wide {
                headers.push("PATH");
            }
            let mut t = Table::new(headers);
            for i in &list {
                let mut row = vec![
                    c(s(i, "name")),
                    c(if s(i, "guest_os") == "macos" {
                        "macOS"
                    } else {
                        "Linux"
                    }),
                    c(format!(
                        "{} vCPUs, {}",
                        u(i, "vcpus"),
                        gib(u(i, "memory_mib"))
                    )),
                    ct(
                        match (s(i, "display_name"), s(i, "description")) {
                            ("", d) => d.to_owned(),
                            (n, "") => n.to_owned(),
                            (n, d) => format!("{n}: {d}"),
                        },
                        Tone::Dim,
                    ),
                ];
                if out.wide {
                    row.push(ct(s(i, "image"), Tone::Dim));
                }
                t.row(row);
            }
            t.print(out);
            out.next(format!(
                "fluxctl fleet launch --image {}",
                s(&list[0], "name")
            ));
            Ok(())
        }
        FleetImageCommand::Add {
            name,
            path,
            guest,
            vcpus,
            memory_gib,
            display_name,
            description,
            source,
        } => {
            let v = call(
                central,
                token,
                Method::PUT,
                &format!("/fleet/images/{}", esc(&name)),
                Some(json!({
                    "guest_os": guest,
                    "image": path,
                    "vcpus": vcpus,
                    "memory_mib": memory_gib.saturating_mul(1024),
                    "display_name": display_name,
                    "description": description,
                    "source": source,
                })),
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            out.done(format!(
                "{name} is in the catalog. Tenants can launch it by name."
            ));
            println!(
                "{}",
                out.dim(format!(
                    "  The disk must be at {} on every Mac that should run it.",
                    s(&v, "image")
                ))
            );
            Ok(())
        }
        FleetImageCommand::Remove { name } => {
            call(
                central,
                token,
                Method::DELETE,
                &format!("/fleet/images/{}", esc(&name)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&json!({"ok": true}));
            }
            out.done(format!(
                "{name} removed from the catalog. VMs made from it keep running."
            ));
            Ok(())
        }
    }
}

// ---- networks -----------------------------------------------------------------------------------------------------

async fn networks(
    central: &str,
    token: Option<&str>,
    out: &Out,
    command: Option<FleetNetworkCommand>,
) -> Result<()> {
    match command.unwrap_or(FleetNetworkCommand::List) {
        FleetNetworkCommand::List => {
            let v = get(central, token, "/fleet/overlays").await?;
            if !out.human {
                return out.json(&v);
            }
            let list = items(&v);
            if list.is_empty() {
                println!("No private networks.");
                out.next("fluxctl fleet networks create <name>");
                return Ok(());
            }
            let mut t = Table::new(vec!["NETWORK", "SUBNET", "MACS", "TENANT", "KEY ROTATED"]);
            for n in &list {
                let macs: Vec<&String> = n
                    .get("members")
                    .and_then(Value::as_object)
                    .map(|m| m.keys().collect())
                    .unwrap_or_default();
                t.row(vec![
                    c(s(n, "name")),
                    c(s(n, "subnet")),
                    c(if macs.is_empty() {
                        "-".to_owned()
                    } else {
                        macs.iter()
                            .map(|m| m.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }),
                    ct(
                        match s(n, "tenant") {
                            "" => "operator",
                            t => t,
                        },
                        Tone::Dim,
                    ),
                    ct(
                        time(n, "key_rotated_at").map(rel).unwrap_or_default(),
                        Tone::Dim,
                    ),
                ]);
            }
            t.print(out);
            Ok(())
        }
        FleetNetworkCommand::Show { name } => {
            let n = get(central, token, &format!("/fleet/overlays/{}", esc(&name))).await?;
            if !out.human {
                return out.json(&n);
            }
            let prefix = s(&n, "subnet").trim_end_matches("0/24").to_owned();
            kv(
                out,
                &[
                    ("Network", out.bold(s(&n, "name"))),
                    ("Subnet", s(&n, "subnet").to_owned()),
                    ("MTU", u(&n, "mtu").to_string()),
                    ("Port", u(&n, "port").to_string()),
                    ("Tenant", s(&n, "tenant").to_owned()),
                    (
                        "Key",
                        format!(
                            "epoch {}, set {}",
                            u(&n, "key_epoch"),
                            time(&n, "key_rotated_at").map(rel).unwrap_or_default()
                        ),
                    ),
                ],
            );
            println!();
            let mut t = Table::new(vec!["MAC", "GUEST ADDRESSES"]);
            for (mac, range) in n
                .get("members")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                let lo = range.get(0).and_then(Value::as_u64).unwrap_or(0);
                let hi = range.get(1).and_then(Value::as_u64).unwrap_or(0);
                t.row(vec![c(mac), c(format!("{prefix}{lo} to {prefix}{hi}"))]);
            }
            t.print(out);
            println!();
            println!(
                "{} {}",
                out.dim("Join a VM:"),
                out.cyan(format!(
                    "fluxctl fleet launch … --network {}",
                    s(&n, "name")
                ))
            );
            Ok(())
        }
        FleetNetworkCommand::Create {
            name,
            mac,
            tenant,
            vms_per_mac,
            mtu,
        } => {
            let mut body = json!({"name": name, "hosts_per_node": vms_per_mac, "mtu": mtu});
            if !mac.is_empty() {
                body["nodes"] = json!(mac);
            }
            if let Some(t) = tenant {
                body["tenant"] = json!(t);
            }
            let n = call(central, token, Method::POST, "/fleet/overlays", Some(body)).await?;
            if !out.human {
                return out.json(&n);
            }
            let macs: Vec<String> = n
                .get("members")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            out.done(format!(
                "Created {} ({}) on {}.",
                out.bold(s(&n, "name")),
                s(&n, "subnet"),
                if macs.is_empty() {
                    "no Macs yet".to_owned()
                } else {
                    macs.join(", ")
                }
            ));
            out.next(format!(
                "fluxctl fleet launch --image <name> --network {}",
                s(&n, "name")
            ));
            Ok(())
        }
        FleetNetworkCommand::Delete { name, yes } => {
            if out.human
                && !yes
                && !confirm(&format!(
                    "Delete {name}? VMs on it lose their links to VMs on other Macs."
                ))?
            {
                println!("Kept {name}.");
                return Ok(());
            }
            call(
                central,
                token,
                Method::DELETE,
                &format!("/fleet/overlays/{}", esc(&name)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&json!({"ok": true}));
            }
            out.done(format!("Deleted {name}."));
            Ok(())
        }
        FleetNetworkCommand::Rotate { name } => {
            let n = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/overlays/{}/rotate", esc(&name)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&n);
            }
            out.done(format!(
                "New key (epoch {}) for {name}. Every Mac switches within a minute; traffic keeps flowing.",
                u(&n, "key_epoch")
            ));
            Ok(())
        }
        FleetNetworkCommand::AddMac { name, mac } => {
            let n = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/overlays/{}/members", esc(&name)),
                Some(json!({"node": mac})),
            )
            .await?;
            if !out.human {
                return out.json(&n);
            }
            out.done(format!("{mac} joined {name}."));
            Ok(())
        }
        FleetNetworkCommand::RemoveMac { name, mac } => {
            let n = call(
                central,
                token,
                Method::DELETE,
                &format!("/fleet/overlays/{}/members/{}", esc(&name), esc(&mac)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&n);
            }
            out.done(format!("{mac} left {name}."));
            Ok(())
        }
    }
}

// ---- tenants ------------------------------------------------------------------------------------------------------

async fn tenant(
    central: &str,
    token: Option<&str>,
    out: &Out,
    command: FleetTenantCommand,
) -> Result<()> {
    match command {
        FleetTenantCommand::Create {
            id,
            display_name,
            quota,
        } => {
            let v = call(
                central,
                token,
                Method::POST,
                "/fleet/tenants",
                Some(json!({"id": id, "display_name": display_name, "quota": quota.to_json()})),
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            out.done(format!("Created tenant {}.", out.bold(&id)));
            println!();
            println!("  {}  {}", out.dim("Token"), out.bold(s(&v, "token")));
            println!();
            println!("  Store it now: it is shown only this once (central keeps only its hash).");
            println!(
                "  The tenant signs in at {central}/console, or sets FLUXVM_AGENT_TOKEN for fluxctl."
            );
            Ok(())
        }
        FleetTenantCommand::List => {
            let v = get(central, token, "/fleet/tenants").await?;
            if !out.human {
                return out.json(&v);
            }
            let list = items(&v);
            if list.is_empty() {
                println!("No tenants yet: whoever holds the fleet token is the only user.");
                out.next("fluxctl fleet tenant create <id> --max-leased-hosts 2");
                return Ok(());
            }
            let mut t = Table::new(vec![
                "TENANT", "NAME", "MACS", "VMS", "VCPUS", "MEMORY", "STATE",
            ]);
            for x in &list {
                let q = x.get("quota").cloned().unwrap_or_default();
                let used = x.get("in_use").cloned().unwrap_or_default();
                let of = |k: &str, qk: &str| match q.get(qk).and_then(Value::as_u64) {
                    Some(m) => format!("{}/{m}", u(&used, k)),
                    None => u(&used, k).to_string(),
                };
                let disabled = x.get("disabled").and_then(Value::as_bool) == Some(true);
                t.row(vec![
                    c(s(x, "id")),
                    ct(s(x, "display_name"), Tone::Dim),
                    c(of("leased_hosts", "max_leased_hosts")),
                    c(of("vms", "max_vms")),
                    c(of("vcpus", "max_vcpus")),
                    c(match q.get("max_memory_mib").and_then(Value::as_u64) {
                        Some(m) => format!("{}/{}", gib(u(&used, "memory_mib")), gib(m)),
                        None => gib(u(&used, "memory_mib")),
                    }),
                    if disabled {
                        ct("disabled", Tone::Bad)
                    } else {
                        ct("active", Tone::Ok)
                    },
                ]);
            }
            t.print(out);
            Ok(())
        }
        FleetTenantCommand::Get { id } => {
            let v = get(central, token, &format!("/fleet/tenants/{}", esc(&id))).await?;
            if !out.human {
                return out.json(&v);
            }
            tenant_detail(out, &v);
            Ok(())
        }
        FleetTenantCommand::Me => {
            let v = get(central, token, "/fleet/tenants/me").await?;
            if !out.human {
                return out.json(&v);
            }
            tenant_detail(out, &v);
            Ok(())
        }
        FleetTenantCommand::Update {
            id,
            quota,
            disable,
            enable,
        } => {
            let mut body = json!({});
            if !quota.is_empty() {
                let current = get(central, token, &format!("/fleet/tenants/{}", esc(&id))).await?;
                let mut q = current.get("quota").cloned().unwrap_or_else(|| json!({}));
                if let (Some(qo), Some(new)) = (q.as_object_mut(), quota.to_json().as_object()) {
                    for (k, v) in new {
                        if !v.is_null() {
                            qo.insert(k.clone(), v.clone());
                        }
                    }
                }
                body["quota"] = q;
            }
            if disable || enable {
                body["disabled"] = json!(disable);
            }
            let v = call(
                central,
                token,
                Method::PATCH,
                &format!("/fleet/tenants/{}", esc(&id)),
                Some(body),
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            out.done(format!(
                "Updated {id}{}.",
                if disable {
                    ": disabled, its token is refused"
                } else if enable {
                    ": enabled"
                } else {
                    ""
                }
            ));
            Ok(())
        }
        FleetTenantCommand::Delete { id } => {
            call(
                central,
                token,
                Method::DELETE,
                &format!("/fleet/tenants/{}", esc(&id)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&json!({"ok": true}));
            }
            out.done(format!("Deleted tenant {id}. Its usage records are kept."));
            Ok(())
        }
        FleetTenantCommand::RotateToken { id } => {
            let v = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/tenants/{}/rotate-token", esc(&id)),
                None,
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            out.done(format!("New token for {id}. The old one stopped working."));
            println!();
            println!("  {}  {}", out.dim("Token"), out.bold(s(&v, "token")));
            println!();
            println!("  Store it now: it is shown only this once.");
            Ok(())
        }
    }
}

fn tenant_detail(out: &Out, v: &Value) {
    let q = v.get("quota").cloned().unwrap_or_default();
    let used = v.get("in_use").cloned().unwrap_or_default();
    let of = |k: &str, qk: &str, f: fn(u64) -> String| match q.get(qk).and_then(Value::as_u64) {
        Some(m) => meter(out, u(&used, k), m, f),
        None => format!("{}  {}", f(u(&used, k)), out.dim("(no limit)")),
    };
    kv(
        out,
        &[
            ("Tenant", out.bold(s(v, "id"))),
            ("Name", s(v, "display_name").to_owned()),
            (
                "State",
                if v.get("disabled").and_then(Value::as_bool) == Some(true) {
                    out.red("disabled")
                } else {
                    out.green("active")
                },
            ),
            ("Macs", of("leased_hosts", "max_leased_hosts", num)),
            ("VMs", of("vms", "max_vms", num)),
            ("vCPUs", of("vcpus", "max_vcpus", num)),
            ("Memory", of("memory_mib", "max_memory_mib", gib)),
        ],
    );
}

// ---- leases -------------------------------------------------------------------------------------------------------

async fn lease(
    central: &str,
    token: Option<&str>,
    out: &Out,
    command: FleetLeaseCommand,
) -> Result<()> {
    match command {
        FleetLeaseCommand::Create {
            tenant,
            node,
            hours,
            min_vcpus,
            min_memory_mib,
            nested,
            node_selector,
        } => {
            let selector: HashMap<String, String> = node_selector.into_iter().collect();
            let v = call(
                central,
                token,
                Method::POST,
                "/fleet/leases",
                Some(json!({
                    "tenant": tenant,
                    "node": node,
                    "hours": hours,
                    "min_vcpus": min_vcpus.unwrap_or(0),
                    "min_memory_mib": min_memory_mib.unwrap_or(0),
                    "nested_virtualization": nested,
                    "nodeSelector": selector,
                })),
            )
            .await?;
            if !out.human {
                return out.json(&v);
            }
            let l = v.get("lease").cloned().unwrap_or_default();
            let n = v.get("node").cloned().unwrap_or_default();
            out.done(format!(
                "Leased {} to {}{}.",
                out.bold(s(&l, "node")),
                s(&l, "tenant"),
                match time(&l, "ends_at") {
                    Some(t) => format!(" until {}", local(t)),
                    None => ", until released".to_owned(),
                }
            ));
            kv(
                out,
                &[
                    (
                        "  Size",
                        if n.is_null() {
                            String::new()
                        } else {
                            format!(
                                "{} vCPUs, {}",
                                u(&n, "vcpus_total"),
                                gib(u(&n, "memory_mib_total"))
                            )
                        },
                    ),
                    (
                        "  Releasable",
                        time(&l, "min_end")
                            .map(|t| format!("{} ({})", local(t), rel(t)))
                            .unwrap_or_default(),
                    ),
                    ("  Lease ID", s(&l, "id").to_owned()),
                ],
            );
            println!(
                "{}",
                out.dim("  Billed for at least 24 hours, as Apple's macOS licence requires.")
            );
            out.next("fluxctl fleet launch --image <name>   (see `fluxctl fleet images`)");
            Ok(())
        }
        FleetLeaseCommand::List { tenant, state } => {
            let q = query(&[("tenant", tenant), ("state", state)]);
            let v = get(central, token, &format!("/fleet/leases{q}")).await?;
            if !out.human {
                return out.json(&v);
            }
            let list = items(&v);
            if list.is_empty() {
                println!("No leases.");
                out.next("fluxctl fleet lease create");
                return Ok(());
            }
            let mut t = Table::new(vec![
                "MAC",
                "TENANT",
                "STATE",
                "STARTED",
                "RELEASABLE",
                "ENDS",
                "ID",
            ]);
            for l in &list {
                let active = s(l, "state") == "active";
                let (rl, tone) = if active {
                    lease_line(l)
                } else {
                    ("-".into(), Tone::Dim)
                };
                t.row(vec![
                    c(s(l, "node")),
                    c(s(l, "tenant")),
                    ct(s(l, "state"), if active { Tone::Ok } else { Tone::Dim }),
                    ct(
                        time(l, "started_at").map(local).unwrap_or_default(),
                        Tone::Dim,
                    ),
                    ct(rl, tone),
                    ct(
                        time(l, "ends_at").map(local).unwrap_or_else(|| {
                            if active {
                                "when released".into()
                            } else {
                                "-".into()
                            }
                        }),
                        Tone::Dim,
                    ),
                    ct(s(l, "id"), Tone::Dim),
                ]);
            }
            t.print(out);
            Ok(())
        }
        FleetLeaseCommand::Get { id } => {
            let l = get(central, token, &format!("/fleet/leases/{}", esc(&id))).await?;
            if !out.human {
                return out.json(&l);
            }
            let (rl, _) = lease_line(&l);
            kv(
                out,
                &[
                    ("Lease", s(&l, "id").to_owned()),
                    ("Mac", out.bold(s(&l, "node"))),
                    ("Tenant", s(&l, "tenant").to_owned()),
                    ("State", s(&l, "state").to_owned()),
                    (
                        "Started",
                        time(&l, "started_at").map(local).unwrap_or_default(),
                    ),
                    (
                        "Minimum until",
                        time(&l, "min_end").map(local).unwrap_or_default(),
                    ),
                    (
                        "Ends",
                        time(&l, "ends_at")
                            .map(local)
                            .unwrap_or_else(|| "when released".into()),
                    ),
                    (
                        "",
                        if s(&l, "state") == "active" {
                            rl
                        } else {
                            String::new()
                        },
                    ),
                ],
            );
            Ok(())
        }
        FleetLeaseCommand::Release { id, force } => {
            let q = query(&[("force", force.then(|| "true".to_string()))]);
            let v = call(
                central,
                token,
                Method::POST,
                &format!("/fleet/leases/{}/release{q}", esc(&id)),
                None,
            )
            .await
            .map_err(|e| {
                // 409 is also "not active"; only the 24-hour minimum gets this hint.
                let early = e
                    .chain()
                    .find_map(|c| c.downcast_ref::<HttpError>())
                    .is_some_and(|h| h.status.as_u16() == 409 && h.message.contains("24-hour minimum"));
                if early {
                    e.context("the 24-hour minimum is not over yet\nhint: see when with `fluxctl fleet lease get <id>`; an operator can pass --force (still billed to 24 hours)")
                } else {
                    e
                }
            })?;
            if !out.human {
                return out.json(&v);
            }
            let l = v.get("lease").cloned().unwrap_or_default();
            let evicted = v
                .get("evicted")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            out.done(format!(
                "Released {}{}. It is scrubbed before anyone else can lease it.",
                s(&l, "node"),
                if evicted > 0 {
                    format!(" ({evicted} VMs deleted)")
                } else {
                    String::new()
                }
            ));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Out {
        Out {
            human: true,
            wide: false,
            color: false,
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("30m"), Ok(1800));
        assert_eq!(parse_duration("8h"), Ok(28800));
        assert_eq!(parse_duration("7d"), Ok(604800));
        assert_eq!(parse_duration("600"), Ok(600));
        assert!(parse_duration("0h").is_err());
        assert!(parse_duration("soon").is_err());
    }

    #[test]
    fn table_aligns_and_trims() {
        let mut t = Table::new(vec!["NAME", "STATUS"]);
        t.row(vec![c("ios-build-1"), ct("running", Tone::Ok)]);
        t.row(vec![c("db"), ct("stopped", Tone::Dim)]);
        let r = t.render(&plain());
        assert_eq!(
            r,
            "NAME         STATUS\nios-build-1  running\ndb           stopped\n"
        );
    }

    #[test]
    fn macs_show_slots_and_leases() {
        let now = Utc::now();
        let nodes = vec![
            json!({"name": "studio-1", "healthy": true, "cordoned": false, "vm_count": 2, "free_vcpus": 12,
                   "free_memory_mib": 65536, "last_seen": now.to_rfc3339(), "apple": {"macos_guests": 2}}),
            json!({"name": "linux-1", "healthy": false, "cordoned": false, "vm_count": 0, "free_vcpus": 8,
                   "free_memory_mib": 16384, "last_seen": now.to_rfc3339(), "apple": null}),
        ];
        let leases = vec![json!({"node": "studio-1", "tenant": "acme",
            "min_end": (now + chrono::Duration::hours(5)).to_rfc3339()})];
        let r = mac_table(&plain(), &nodes, &leases).render(&plain());
        assert!(r.contains("leased to acme"), "{r}");
        assert!(r.contains("●●"), "{r}");
        assert!(
            r.contains("releasable in 4h") || r.contains("releasable in 5h"),
            "{r}"
        );
        assert!(r.contains("not responding"), "{r}");
        assert!(r.lines().next().unwrap().starts_with("MAC"), "{r}");
        // Linux hosts sort first, and have no macOS slots.
        assert!(r.lines().nth(1).unwrap().contains("Linux"), "{r}");
    }

    #[test]
    fn colour_is_off_when_asked() {
        let out = plain();
        assert_eq!(out.green("x"), "x");
        let on = Out {
            color: true,
            ..plain()
        };
        assert!(on.green("x").contains('\u{1b}'));
    }

    #[test]
    fn json_unless_a_terminal_or_asked() {
        // Explicit -o json is always JSON; -o table is always human.
        assert!(!Out::new(OutputFormat::Json, true).human);
        assert!(Out::new(OutputFormat::Table, true).human);
        assert!(Out::new(OutputFormat::Wide, false).wide);
    }

    /// The friendlier verbs against a real central and a simulated Mac.
    #[tokio::test]
    async fn launch_run_and_rm_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let app = fluxvm_agent::central::router(fluxvm_agent::central::CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some("op".into()),
            reconcile_interval: None,
            ha: None,
        });
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let central = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let sim = fluxvm_agent::sim::SimNode::default().router();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let node_url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, sim).await.unwrap() });
        let http = reqwest::Client::new();
        http.post(format!("{central}/fleet/register"))
            .bearer_auth("op")
            .json(
                &json!({"name": "mac-1", "fluxvm_url": node_url, "vcpus_total": 12,
                "memory_mib_total": 32768, "vm_count": 0, "labels": {},
                "apple": {"free_cpu": 12, "free_memory_mib": 32768, "nested_virtualization": true,
                          "vmnet": true, "custom_virtio": true, "bridged_interfaces": []}}),
            )
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let tok = Some("op");
        let quiet = Out {
            human: false,
            wide: false,
            color: false,
        };
        call(
            &central,
            tok,
            Method::PUT,
            "/fleet/images/debian-13",
            Some(json!({"guest_os": "linux",
            "image": "/vms/debian.raw", "vcpus": 2, "memory_mib": 2048})),
        )
        .await
        .unwrap();

        // An unknown image names the ones that exist.
        let err = launch(
            &central,
            tok,
            &quiet,
            LaunchArgs {
                name: None,
                image: Some("nope".into()),
                disk: None,
                guest: None,
                vcpus: None,
                memory_gib: None,
                mac: None,
                network: vec![],
                ttl: None,
            },
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("debian-13"), "{err:#}");

        // Launch by image: name, size and path come from the catalog.
        launch(
            &central,
            tok,
            &quiet,
            LaunchArgs {
                name: None,
                image: Some("debian-13".into()),
                disk: None,
                guest: None,
                vcpus: None,
                memory_gib: None,
                mac: None,
                network: vec![],
                ttl: Some(3600),
            },
        )
        .await
        .unwrap();
        let vms = items(&fleet_client::list_vms(&central, tok).await.unwrap());
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0]["name"], "debian-13-1");
        assert_eq!(vms[0]["request"]["image"], "/vms/debian.raw");
        assert_eq!(vms[0]["request"]["ttl_seconds"], 3600);

        // Resolve by name and by id prefix; a missing name says how to list.
        let (node, id, _) = resolve_vm(&central, tok, "debian-13-1").await.unwrap();
        assert_eq!(node, "mac-1");
        assert_eq!(resolve_vm(&central, tok, &id[..8]).await.unwrap().1, id);
        let err = resolve_vm(&central, tok, "ghost").await.unwrap_err();
        assert!(err.to_string().contains("fluxctl fleet vms"), "{err}");

        // A refused call carries central's message and a hint, not raw JSON.
        let err = get(&central, Some("wrong"), "/fleet/vms")
            .await
            .unwrap_err();
        assert_eq!(status_of(&err), Some(401));
        assert!(err.to_string().contains("hint:"), "{err}");
    }
}
