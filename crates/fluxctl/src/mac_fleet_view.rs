// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet logs` and `fluxctl mac-fleet events`: read-only views across all the Macs, one ssh call per Mac.
//!
//! - `mac-fleet logs --name NAME`: a guest console log from whichever Mac has it.
//! - `mac-fleet events`: the recorded `mac.*` / `kube.apple.*` events of every Mac, merged by time.
//!
//! (`status --detail` is in mac_fleet_status; `mac ps --fleet` and `mac images --fleet` are in mac_fleet_agg.) Every call is
//! the Mac's own `fluxctl` over ssh, so a Mac whose fluxctl is older than the command is reported as such rather than failing
//! the whole view. Nothing here changes a Mac.

use crate::mac_fleet::{Conn, Which, jobs, par_map, resolve, transport};
use crate::mac_fleet_remote::{PRELUDE, remote, sh_quote};
use crate::mac_fleet_ssh::{ExecOut, Target, Transport, explain_ssh_failure};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

const LOOK: Duration = Duration::from_secs(60);

/// Run `fluxctl ARGS` on a Mac. `args` are fixed words chosen here or quoted values; nothing from a Mac is ever put back in.
fn fluxctl_on(tr: &dyn Transport, t: &Target, args: &str) -> Result<ExecOut, String> {
    let script = format!(
        "{PRELUDE}[ -n \"$FLUXCTL\" ] || {{ echo 'fluxctl is not installed' >&2; exit 127; }}\n\"$FLUXCTL\" {args}\n"
    );
    match tr.run_timed(t, &remote(&script), None, Some(LOOK)) {
        Err(e) => Err(format!("{e:#}")),
        Ok(o) if o.ssh_failed() => Err(explain_ssh_failure(t, &o.stderr)),
        Ok(o) => Ok(o),
    }
}
fn first_error(o: &ExecOut) -> String {
    let e = crate::mac_fleet::last_lines(&o.stderr, 1);
    if e.is_empty() {
        format!("exit status {}", o.code)
    } else {
        e
    }
}
// ---- logs --------------------------------------------------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct LogsArgs {
    #[command(flatten)]
    pub which: Which,
    /// The instance (a job guest is `fleet-<id>`): looked for on every selected Mac.
    #[arg(long, value_name = "NAME")]
    pub name: String,
    /// Lines from the end of the console log.
    #[arg(long, default_value_t = 100)]
    pub tail: usize,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogResult {
    pub host: String,
    pub found: bool,
    pub error: Option<String>,
    pub lines: Vec<String>,
}

pub fn valid_instance_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 120
        && !n.starts_with('-')
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

pub fn logs_host(tr: &dyn Transport, t: &Target, name: &str, tail: usize) -> LogResult {
    let mut r = LogResult {
        host: t.label(),
        found: false,
        error: None,
        lines: vec![],
    };
    match fluxctl_on(
        tr,
        t,
        &format!("mac logs --tail {tail} {} 2>&1", sh_quote(name)),
    ) {
        Err(e) => r.error = Some(e),
        Ok(o) if o.ok() => {
            r.found = true;
            r.lines = o.stdout.lines().map(str::to_owned).collect();
        }
        Ok(o) => {
            // "no such instance" is the normal answer from the Macs that do not have it; anything else is reported.
            let msg = crate::mac_fleet::last_lines(&format!("{}{}", o.stdout, o.stderr), 1);
            let lower = msg.to_ascii_lowercase();
            if !(lower.contains("no such")
                || lower.contains("not found")
                || lower.contains("no instance"))
            {
                r.error = Some(msg);
            }
        }
    }
    r
}

pub fn render_logs(rs: &[LogResult]) -> String {
    let mut out = String::new();
    for r in rs.iter().filter(|r| r.found) {
        for l in &r.lines {
            out.push_str(&format!("[{}] {l}\n", r.host));
        }
    }
    for r in rs {
        if let Some(e) = &r.error {
            out.push_str(&format!("[{}] could not read: {e}\n", r.host));
        }
    }
    out
}

pub fn logs_cmd(a: LogsArgs) -> Result<()> {
    if !valid_instance_name(&a.name) {
        anyhow::bail!("{:?} is not an instance name", a.name);
    }
    let (targets, inv) = resolve(&a.which, &a.conn)?;
    let tr = transport(&a.conn, inv.as_ref());
    let rs = par_map(&targets, jobs(&a.conn), |t| {
        logs_host(tr.as_ref(), t, &a.name, a.tail)
    });
    if a.conn.dry_run {
        if let Some(d) = tr.as_dry() {
            for l in d.lines.lock().unwrap().iter() {
                println!("{l}");
            }
        }
        return Ok(());
    }
    if a.conn.json {
        println!("{}", serde_json::to_string_pretty(&rs)?);
    } else {
        print!("{}", render_logs(&rs));
    }
    if !rs.iter().any(|r| r.found) {
        anyhow::bail!(
            "no selected Mac has an instance named {:?}{}",
            a.name,
            if rs.iter().any(|r| r.error.is_some()) {
                " (some could not be read)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

// ---- events ------------------------------------------------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct EventsArgs {
    #[command(flatten)]
    pub which: Which,
    /// Newest N events across the fleet.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
    /// Only events whose name starts with this (`mac.`, `kube.apple.`, `mac.slots`).
    #[arg(long, value_name = "PREFIX")]
    pub event: Option<String>,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(Debug, Clone, Serialize)]
pub struct FleetEvent {
    pub ts: String,
    pub host: String,
    pub event: String,
    pub detail: Value,
}

/// `mac events --json` prints one JSON object per line (`{ts, event, vm_id?, fields}`); lines that are not that are ignored.
pub fn parse_events(host: &str, text: &str) -> Vec<FleetEvent> {
    text.lines()
        .filter_map(|l| {
            let v: Value = serde_json::from_str(l.trim()).ok()?;
            Some(FleetEvent {
                ts: v["ts"].as_str()?.to_owned(),
                host: host.to_owned(),
                event: v["event"].as_str()?.to_owned(),
                detail: json!({"vm_id": v["vm_id"], "fields": v["fields"]}),
            })
        })
        .collect()
}

fn ts_key(ts: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

/// Oldest first, newest `limit` kept, optionally filtered by event-name prefix. Stable for equal times (host order).
pub fn merge_events(
    mut all: Vec<FleetEvent>,
    limit: usize,
    prefix: Option<&str>,
) -> Vec<FleetEvent> {
    if let Some(p) = prefix {
        all.retain(|e| e.event.starts_with(p));
    }
    all.sort_by_key(|e| ts_key(&e.ts));
    let skip = all.len().saturating_sub(limit);
    all.split_off(skip)
}

pub fn render_events(es: &[FleetEvent]) -> String {
    es.iter()
        .map(|e| {
            let f = &e.detail["fields"];
            let fields = f
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| {
                            format!(
                                "{k}={}",
                                v.as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| v.to_string())
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            format!("{} {} {} {}\n", e.ts, e.host, e.event, fields)
        })
        .collect()
}

pub fn events_cmd(a: EventsArgs) -> Result<()> {
    let (targets, inv) = resolve(&a.which, &a.conn)?;
    let tr = transport(&a.conn, inv.as_ref());
    let per_host = par_map(&targets, jobs(&a.conn), |t| {
        let r = fluxctl_on(
            tr.as_ref(),
            t,
            &format!("mac events --json --limit {}", a.limit),
        );
        (t.label(), r)
    });
    if a.conn.dry_run {
        if let Some(d) = tr.as_dry() {
            for l in d.lines.lock().unwrap().iter() {
                println!("{l}");
            }
        }
        return Ok(());
    }
    let mut all = vec![];
    let mut failed = vec![];
    for (host, r) in per_host {
        match r {
            Ok(o) if o.ok() => all.extend(parse_events(&host, &o.stdout)),
            Ok(o) => failed.push(format!("{host}: {}", first_error(&o))),
            Err(e) => failed.push(format!("{host}: {e}")),
        }
    }
    let merged = merge_events(all, a.limit, a.event.as_deref());
    if a.conn.json {
        for e in &merged {
            println!("{}", serde_json::to_string(e)?);
        }
    } else {
        print!("{}", render_events(&merged));
    }
    for f in &failed {
        eprintln!("could not read events: {f}");
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "events could not be read from {} of {} Macs",
            failed.len(),
            targets.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_ssh::fake::{FakeTransport, failed, ok};

    fn mini(n: u8) -> Target {
        Target::parse(&format!("admin@mini-{n}")).unwrap()
    }

    #[test]
    fn logs_come_from_the_mac_that_has_the_guest() {
        let tr = FakeTransport::new(|t, _| {
            if t.host == "mini-1" {
                ok("boot\nready\n")
            } else {
                failed(1, "Error: no such instance fleet-ab")
            }
        });
        let rs = vec![
            logs_host(&tr, &mini(1), "fleet-ab", 50),
            logs_host(&tr, &mini(2), "fleet-ab", 50),
        ];
        assert!(
            rs[0].found && !rs[1].found && rs[1].error.is_none(),
            "absent is not an error"
        );
        assert_eq!(
            render_logs(&rs),
            "[admin@mini-1] boot\n[admin@mini-1] ready\n"
        );
        assert!(tr.calls.lock().unwrap()[0].command.contains("--tail 50"));
        assert!(
            valid_instance_name("fleet-ab1")
                && !valid_instance_name("-rf")
                && !valid_instance_name("a b;")
        );
        let broken = FakeTransport::new(|_, _| failed(2, "boom"));
        assert_eq!(
            logs_host(&broken, &mini(1), "x", 5).error.as_deref(),
            Some("boom")
        );
    }

    #[test]
    fn events_from_all_macs_merge_by_time_newest_kept_and_filtered() {
        let a = parse_events(
            "m1",
            "{\"ts\":\"2026-11-21T10:00:02Z\",\"event\":\"mac.started\",\"fields\":{\"name\":\"a\"}}\nnoise\n{\"ts\":\"2026-11-21T10:00:00Z\",\"event\":\"mac.created\",\"fields\":{}}\n",
        );
        let b = parse_events(
            "m2",
            "{\"ts\":\"2026-11-21T10:00:01+00:00\",\"event\":\"kube.apple.node.ready\",\"fields\":{}}\n{\"ts\":\"2026-11-21T10:00:03Z\",\"event\":\"mac.stopped\",\"fields\":{\"name\":\"a\"}}\n",
        );
        let mut all = a;
        all.extend(b);
        let m = merge_events(all.clone(), 10, None);
        assert_eq!(
            m.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            [
                "mac.created",
                "kube.apple.node.ready",
                "mac.started",
                "mac.stopped"
            ]
        );
        let last2 = merge_events(all.clone(), 2, None);
        assert_eq!(last2[0].event, "mac.started");
        let only = merge_events(all, 10, Some("mac."));
        assert_eq!(only.len(), 3);
        assert!(render_events(&m).contains("m1 mac.started name=a"));
    }
}
