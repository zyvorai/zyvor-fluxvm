// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac-fleet status --detail`: the table of `status` plus what the scheduler and an operator of a headless
//! Mac need: whether the Mac is drained (and since when), which templates it holds, and the headless checks of
//! `fluxctl mac host headless-check` (an unlocked session, sleep, auto-login), read over ssh. Read-only.

use crate::mac_fleet::{par_map, render_table};
use crate::mac_fleet_remote::{PRELUDE, remote};
use crate::mac_fleet_run::view_host;
use crate::mac_fleet_sched::{FleetState, HeldTemplate, HostView};
use crate::mac_fleet_ssh::{Target, Transport};
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HeadlessCheck {
    pub name: String,
    /// `ok`, `warn` or `fail`.
    pub status: String,
    pub detail: String,
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetailRow {
    pub host: String,
    pub name: String,
    pub reachable: bool,
    pub error: Option<String>,
    pub drained: bool,
    pub drained_since_unix: Option<u64>,
    pub macos_guests: u32,
    pub templates: Vec<HeldTemplate>,
    pub templates_known: bool,
    pub on_ac: Option<bool>,
    pub disk_free_gib: Option<u64>,
    /// `None` when the Mac could not be asked (old FluxVM without `mac host`, or not macOS).
    pub headless: Option<Vec<HeadlessCheck>>,
}

pub fn headless_script() -> String {
    // The command exits 1 on a failed check but still prints the document; keep it.
    format!("{PRELUDE}[ -n \"$FLUXCTL\" ] && \"$FLUXCTL\" mac host headless-check --json 2>/dev/null; true\n")
}

pub fn parse_headless(out: &str) -> Option<Vec<HeadlessCheck>> {
    let v: serde_json::Value = serde_json::from_str(out.trim()).ok()?;
    let checks = v["checks"].as_array()?;
    Some(
        checks
            .iter()
            .map(|c| HeadlessCheck {
                name: c["name"].as_str().unwrap_or("").into(),
                status: c["status"].as_str().unwrap_or("").into(),
                detail: c["detail"].as_str().unwrap_or("").into(),
                fix: c["fix"].as_str().map(str::to_owned),
            })
            .collect(),
    )
}

pub fn detail_row(target: &Target, v: &HostView, state: &FleetState, headless: Option<Vec<HeadlessCheck>>) -> DetailRow {
    DetailRow {
        host: target.label(),
        name: target.name.clone(),
        reachable: v.unavailable.is_none(),
        error: v.unavailable.clone(),
        drained: state.is_drained(&target.name),
        drained_since_unix: state.drained.get(&target.name).map(|d| d.since_unix),
        macos_guests: v.caps.macos_guests,
        templates: v.templates.clone(),
        templates_known: v.templates_known,
        on_ac: v.on_ac,
        disk_free_gib: v.disk_free_gib,
        headless,
    }
}

pub fn detail_host(tr: &dyn Transport, target: &Target, state: &FleetState, timeout: Duration) -> DetailRow {
    let v = view_host(tr, target, timeout);
    let headless = if v.unavailable.is_none() {
        tr.run_timed(target, &remote(&headless_script()), None, Some(timeout))
            .ok()
            .filter(|o| !o.ssh_failed())
            .and_then(|o| parse_headless(&o.stdout))
    } else {
        None
    };
    detail_row(target, &v, state, headless)
}

pub fn detail(tr: &dyn Transport, targets: &[Target], state: &FleetState, jobs: usize, timeout: Duration) -> Vec<DetailRow> {
    par_map(targets, jobs, |t| detail_host(tr, t, state, timeout))
}

fn since(now: u64, then: u64) -> String {
    let s = now.saturating_sub(then);
    match s {
        0..=89 => format!("{s}s"),
        90..=5399 => format!("{}m", s / 60),
        5400..=129_599 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

pub fn render_detail(rows: &[DetailRow], now: u64) -> String {
    let mut out = render_table(
        &["HOST", "DRAIN", "GUESTS", "AC", "DISK", "TEMPLATES", "HEADLESS"],
        &rows
            .iter()
            .map(|r| {
                let headless = match &r.headless {
                    None => "?".to_owned(),
                    Some(c) => {
                        let f = c.iter().filter(|c| c.status == "fail").count();
                        let w = c.iter().filter(|c| c.status == "warn").count();
                        if f + w == 0 { "ok".into() } else { format!("{f} fail, {w} warn") }
                    }
                };
                vec![
                    r.host.clone(),
                    match r.drained_since_unix {
                        Some(t) if r.drained => format!("drained {}", since(now, t)),
                        _ if r.drained => "drained".into(),
                        _ => "-".into(),
                    },
                    if r.reachable { format!("{}/2", r.macos_guests) } else { "-".into() },
                    r.on_ac.map_or("?", |a| if a { "ac" } else { "battery" }).into(),
                    r.disk_free_gib.map_or("-".into(), |g| format!("{g}G")),
                    if !r.templates_known { "?".into() } else { r.templates.len().to_string() },
                    headless,
                ]
            })
            .collect::<Vec<_>>(),
    );
    for r in rows {
        if let Some(e) = &r.error {
            out.push_str(&format!("\n{}: {e}", r.host));
        }
        for t in &r.templates {
            out.push_str(&format!(
                "\n{}: template {}{}",
                r.host,
                t.reference,
                if t.digest.is_empty() { String::new() } else { format!(" ({})", t.digest.chars().take(19).collect::<String>()) }
            ));
        }
        for c in r.headless.iter().flatten().filter(|c| c.status != "ok") {
            out.push_str(&format!("\n{}: headless {} {}: {}", r.host, c.status, c.name, c.detail));
            if let Some(f) = &c.fix {
                out.push_str(&format!(" (fix: {f})"));
            }
        }
    }
    out.push('\n');
    out
}

/// The command: probe every Mac, print the table (or JSON).
pub fn status_detail_cmd(
    conn: &crate::mac_fleet::Conn,
    inv: Option<&crate::mac_fleet_inv::Inventory>,
    targets: &[Target],
) -> anyhow::Result<()> {
    let tr = crate::mac_fleet::transport(conn, inv);
    let state_path = crate::mac_fleet_sched::state_path()?;
    let state = FleetState::load(&state_path)?;
    if conn.dry_run {
        for t in targets {
            let _ = tr.run(t, &remote(&headless_script()), None);
        }
        println!("dry run: the probe and the headless check would be sent to {} Mac(s)", targets.len());
        return Ok(());
    }
    let rows = detail(tr.as_ref(), targets, &state, crate::mac_fleet::jobs(conn), Duration::from_secs(30));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let leases = crate::mac_fleet_lease::live(&state_path, now);
    if conn.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"hosts": rows, "leases": leases.len()}))?
        );
    } else {
        print!("{}", render_detail(&rows, now));
        println!("shared leases held by running placements on this machine: {}", leases.len());
    }
    let bad = rows.iter().filter(|r| !r.reachable).count();
    if bad > 0 {
        anyhow::bail!("{bad} of {} Macs could not be reached", rows.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_ssh::fake::{FakeTransport, ok};

    const HEADLESS: &str = r#"{"ok":false,"checks":[
        {"name":"console session","status":"fail","detail":"login window is showing","fix":"turn on automatic login"},
        {"name":"sleep","status":"ok","detail":"never","fix":null}]}"#;

    fn reply() -> String {
        "os=Darwin\narch=arm64\nmacos=15.1\nfluxctl_version=0.4.21\ndaemon_healthy=yes\ndaemon_loaded=yes\ndisk_free_gib=300\n\
         vms_json={\"items\":[{\"status\":\"running\",\"request\":{\"vcpus\":4,\"memory_mib\":8192,\"apple\":{\"guest_os\":\"macos\"}}}]}\n\
         ncpu=10\nmemsize=34359738368\npower=ac\nimages_json=[{\"reference\":\"reg/x:1\",\"digest\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}]\n"
            .to_owned()
    }

    #[test]
    fn detail_shows_drain_templates_and_headless_facts() {
        // The shared probe script now also asks the Mac for its headless-check, so tell the two commands apart by the probe marker.
        let tr = FakeTransport::new(|_, c| if c.contains("daemon_healthy") { ok(&reply()) } else if c.contains("headless-check") { ok(HEADLESS) } else { ok(&reply()) });
        let t = Target::parse("admin@mini-1.local").unwrap();
        let mut st = FleetState::default();
        st.drain("mini-1", 1000);
        let rows = detail(&tr, std::slice::from_ref(&t), &st, 1, Duration::from_secs(5));
        let r = &rows[0];
        assert!(r.reachable && r.drained && r.macos_guests == 1);
        assert_eq!(r.templates.len(), 1);
        assert_eq!(r.headless.as_ref().unwrap()[0].status, "fail");
        let text = render_detail(&rows, 1000 + 7200);
        assert!(text.contains("drained 2h"), "{text}");
        assert!(text.contains("1 fail, 0 warn"), "{text}");
        assert!(text.contains("template reg/x:1"), "{text}");
        assert!(text.contains("fix: turn on automatic login"), "{text}");
        let j = serde_json::to_value(&rows).unwrap();
        assert_eq!(j[0]["headless"][0]["name"], "console session");
        assert_eq!(j[0]["drained"], true);
    }

    #[test]
    fn an_unreachable_mac_is_reported_not_probed_for_headless() {
        let tr = FakeTransport::new(|_, _| crate::mac_fleet_ssh::ExecOut { code: 255, stdout: String::new(), stderr: "Connection refused".into() });
        let t = Target::parse("admin@down.local").unwrap();
        let rows = detail(&tr, std::slice::from_ref(&t), &FleetState::default(), 1, Duration::from_secs(5));
        assert!(!rows[0].reachable && rows[0].headless.is_none());
        assert_eq!(tr.calls_for("down.local").len(), 1, "one probe, no headless call");
        assert!(render_detail(&rows, 0).contains("Remote Login"));
    }

    #[test]
    fn headless_output_that_is_not_json_means_unknown() {
        assert!(parse_headless("").is_none());
        assert!(parse_headless("error: no such command").is_none());
        assert_eq!(parse_headless(HEADLESS).unwrap().len(), 2);
    }
}
