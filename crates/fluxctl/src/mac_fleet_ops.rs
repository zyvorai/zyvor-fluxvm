// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet cordon | uncordon | drain | top`: keep new jobs off a Mac, wait for it to empty, and watch the
//! fleet's capacity live.
//!
//! * `cordon` and `uncordon` flip the same drain list the scheduler reads before every placement (what `drain` and
//!   `resume` always did); running jobs are not touched.
//! * `drain HOST` cordons, then with `--wait` stays until the Mac has no macOS guest left (or `--wait-timeout`), so it is
//!   safe to upgrade or reboot afterwards.
//! * `top` is one table: macOS guests against the 2-per-Mac limit, thermal state, free disk and power, per Mac. It
//!   refreshes in a terminal, prints once into a pipe or with `--json`, and writes `mac-fleet-top.json` next to the state
//!   file, which `GET /v1/mac-fleet` (the Studio Fleet page) reads. The snapshot holds no secret.

use crate::mac_errors::coded;
use crate::mac_fleet::{Conn, Which, jobs, par_map, render_table, resolve, transport};
use crate::mac_fleet_remote::{MACOS_SLOTS, PRELUDE, remote};
use crate::mac_fleet_run::{Clock, DrainArgs, SystemClock, set_drain, view_host};
use crate::mac_fleet_sched::{FleetState, HostView, state_path};
use crate::mac_fleet_ssh::{Target, Transport};
use crate::mac_host::{Thermal, parse_pmset_therm};
use anyhow::Result;
use serde::Serialize;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

/// Free disk under which a Mac is flagged: a macOS template is tens of GB and each guest clones one.
pub const LOW_DISK_GIB: u64 = 50;

// ---- cordon, uncordon, drain ----------------------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct DrainOpts {
    #[command(flatten)]
    pub base: DrainArgs,
    /// After cordoning, wait until the Mac has no macOS guest left, so it is safe to upgrade or reboot.
    #[arg(long)]
    pub wait: bool,
    /// Seconds `--wait` may take before giving up (the Mac stays cordoned).
    #[arg(long, value_name = "SECS", default_value_t = 1800)]
    pub wait_timeout: u64,
}

/// `cordon` (`on` true) and `uncordon`: the drain flag only, never waits.
pub fn cordon_cmd(a: DrainArgs, on: bool) -> Result<()> {
    crate::mac_fleet_run::drain_cmd(a, on)
}

/// What one poll of a draining Mac says.
#[derive(Debug, PartialEq, Eq)]
pub enum Drained {
    Empty,
    Busy(u32),
    Unreachable(String),
}

pub fn drain_state(v: &HostView) -> Drained {
    match &v.unavailable {
        Some(e) => Drained::Unreachable(e.clone()),
        None if v.caps.macos_guests == 0 => Drained::Empty,
        None => Drained::Busy(v.caps.macos_guests),
    }
}

/// Poll until `target` has no macOS guest. Returns the lines to print; `Err` when it never emptied.
pub fn wait_empty(
    tr: &dyn Transport,
    target: &Target,
    clock: &dyn Clock,
    timeout: u64,
    poll: u64,
    say: &mut dyn FnMut(&str),
) -> Result<()> {
    let start = clock.now();
    let mut last = None;
    loop {
        let st = drain_state(&view_host(tr, target, Duration::from_secs(60)));
        match &st {
            Drained::Empty => {
                say(&format!(
                    "{}: empty, safe to upgrade or reboot",
                    target.name
                ));
                return Ok(());
            }
            Drained::Unreachable(e) => {
                return Err(coded(
                    format!(
                        "{}: cannot tell whether it is empty: {e}. It stays cordoned; fix that and run the drain again",
                        target.name
                    ),
                    "FLEET_DRAIN_TIMEOUT",
                ));
            }
            Drained::Busy(n) => {
                if last != Some(*n) {
                    say(&format!(
                        "{}: {n} macOS guest(s) still running; waiting",
                        target.name
                    ));
                    last = Some(*n);
                }
            }
        }
        if clock.now().saturating_sub(start) >= timeout {
            return Err(coded(
                format!(
                    "{}: still has running guests after {timeout}s. It stays cordoned, so nothing new lands on it; run `fluxctl mac-fleet drain {} --wait` again, or `fluxctl mac-fleet exec {} -- fluxctl mac ps` to see what is running",
                    target.name, target.name, target.name
                ),
                "FLEET_DRAIN_TIMEOUT",
            ));
        }
        clock.sleep(poll);
    }
}

pub fn drain_cmd(a: DrainOpts) -> Result<()> {
    if a.base.list || !a.wait || a.base.conn.dry_run {
        return crate::mac_fleet_run::drain_cmd(a.base, true);
    }
    let (targets, inv) = resolve(&a.base.which, &a.base.conn)?;
    let names: Vec<String> = targets.iter().map(|t| t.name.clone()).collect();
    for l in set_drain(&state_path()?, &names, true, SystemClock.now())? {
        println!("{l}");
    }
    let tr = transport(&a.base.conn, inv.as_ref());
    let rs = par_map(&targets, jobs(&a.base.conn), |t| {
        wait_empty(tr.as_ref(), t, &SystemClock, a.wait_timeout, 10, &mut |l| {
            println!("{l}")
        })
    });
    let bad: Vec<String> = rs
        .into_iter()
        .filter_map(|r| r.err().map(|e| format!("{e:#}")))
        .collect();
    if !bad.is_empty() {
        anyhow::bail!("{}", bad.join("\n"));
    }
    Ok(())
}

// ---- top -------------------------------------------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct TopArgs {
    #[command(flatten)]
    pub which: Which,
    /// Print once and exit (the default when output is not a terminal, and with --json).
    #[arg(long)]
    pub once: bool,
    /// Seconds between refreshes in a terminal.
    #[arg(long, value_name = "SECS", default_value_t = 5)]
    pub interval: u64,
    /// Only the Macs that match: `label=KEY`, `label=KEY=VALUE`, `name=PART`, `host=PART`. Repeatable. A filtered view is not
    /// saved for Studio, which keeps showing the whole fleet.
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filter: Vec<String>,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TopRow {
    pub name: String,
    pub host: String,
    /// `ready`, `full`, `cordoned`, `draining` (cordoned with guests still running) or `unreachable`.
    pub state: String,
    pub macos_guests: u32,
    pub macos_limit: usize,
    /// `nominal`, `throttled` or `unknown`.
    pub thermal: String,
    /// CPU speed limit in percent while throttled.
    pub thermal_limit_pct: Option<u32>,
    pub disk_free_gib: Option<u64>,
    pub disk_low: bool,
    pub on_ac: Option<bool>,
    /// The Mac's labels from the inventory (`fluxctl mac-fleet label`), `{"zone": "lab"}`.
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    pub error: Option<String>,
    /// What to do about the row, when something is wrong; a command or a sentence.
    pub next: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct TopSummary {
    pub macs: usize,
    pub ready: usize,
    pub cordoned: usize,
    pub unreachable: usize,
    pub slots_used: u32,
    pub slots_total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopSnapshot {
    pub taken_unix: u64,
    pub hosts: Vec<TopRow>,
    pub summary: TopSummary,
}

pub fn thermal_script() -> String {
    format!("{PRELUDE}pmset -g therm 2>/dev/null; true\n")
}

pub fn top_row(v: &HostView, cordoned: bool, thermal: Option<Thermal>) -> TopRow {
    let guests = v.caps.macos_guests;
    let (t_name, t_pct) = match thermal {
        Some(Thermal::Nominal) => ("nominal", None),
        Some(Thermal::Throttled(n)) => ("throttled", Some(n)),
        _ => ("unknown", None),
    };
    let disk_low = v.disk_free_gib.is_some_and(|g| g < LOW_DISK_GIB);
    let (state, next) = if let Some(e) = &v.unavailable {
        ("unreachable", Some(e.clone()))
    } else if cordoned && guests > 0 {
        (
            "draining",
            Some(format!(
                "wait for it to empty: fluxctl mac-fleet drain {} --wait",
                v.name
            )),
        )
    } else if cordoned {
        (
            "cordoned",
            Some(format!(
                "let it take jobs again: fluxctl mac-fleet uncordon {}",
                v.name
            )),
        )
    } else if guests as usize >= MACOS_SLOTS {
        ("full", None)
    } else {
        ("ready", None)
    };
    let next = next.or_else(|| {
        if t_name == "throttled" {
            Some("the Mac is running slower to stay cool: check airflow and `fluxctl mac host status` (FLEET_HOST_THROTTLED)".into())
        } else if disk_low {
            Some(format!("under {LOW_DISK_GIB} GiB free: `fluxctl mac-fleet exec {} -- fluxctl mac system prune` (FLEET_DISK_LOW)", v.name))
        } else {
            None
        }
    });
    TopRow {
        name: v.name.clone(),
        host: v.label.clone(),
        state: state.into(),
        macos_guests: guests,
        macos_limit: MACOS_SLOTS,
        thermal: t_name.into(),
        thermal_limit_pct: t_pct,
        disk_free_gib: v.disk_free_gib,
        disk_low,
        on_ac: v.on_ac,
        labels: crate::mac_fleet_label::labels_map(&v.labels),
        error: v.unavailable.clone(),
        next,
    }
}

pub fn top_host(
    tr: &dyn Transport,
    target: &Target,
    state: &FleetState,
    timeout: Duration,
) -> TopRow {
    let v = view_host(tr, target, timeout);
    let thermal = if v.unavailable.is_none() {
        tr.run_timed(target, &remote(&thermal_script()), None, Some(timeout))
            .ok()
            .filter(|o| !o.ssh_failed())
            .map(|o| parse_pmset_therm(&o.stdout))
    } else {
        None
    };
    top_row(&v, state.is_drained(&target.name), thermal)
}

pub fn summarize(rows: &[TopRow]) -> TopSummary {
    TopSummary {
        macs: rows.len(),
        ready: rows
            .iter()
            .filter(|r| r.state == "ready" || r.state == "full")
            .count(),
        cordoned: rows
            .iter()
            .filter(|r| r.state == "cordoned" || r.state == "draining")
            .count(),
        unreachable: rows.iter().filter(|r| r.state == "unreachable").count(),
        slots_used: rows
            .iter()
            .filter(|r| r.state != "unreachable")
            .map(|r| r.macos_guests)
            .sum(),
        slots_total: rows.iter().filter(|r| r.state != "unreachable").count() * MACOS_SLOTS,
    }
}

pub fn render_top(rows: &[TopRow]) -> String {
    let mut out = render_table(
        &["HOST", "STATE", "VMS", "THERMAL", "DISK", "POWER"],
        &rows
            .iter()
            .map(|r| {
                let up = r.state != "unreachable";
                vec![
                    r.name.clone(),
                    r.state.clone(),
                    if up {
                        format!("{}/{}", r.macos_guests, r.macos_limit)
                    } else {
                        "-".into()
                    },
                    match (up, r.thermal_limit_pct) {
                        (false, _) => "-".into(),
                        (_, Some(p)) => format!("throttled {p}%"),
                        _ => r.thermal.clone(),
                    },
                    match (up, r.disk_free_gib) {
                        (true, Some(g)) => {
                            format!("{g}G{}", if r.disk_low { " (low)" } else { "" })
                        }
                        _ => "-".into(),
                    },
                    r.on_ac
                        .map_or("-", |a| if a { "ac" } else { "battery" })
                        .into(),
                ]
            })
            .collect::<Vec<_>>(),
    );
    let s = summarize(rows);
    out.push_str(&format!(
        "\n{} Mac(s): {} ready, {} cordoned, {} unreachable; {}/{} macOS slots busy\n",
        s.macs, s.ready, s.cordoned, s.unreachable, s.slots_used, s.slots_total
    ));
    for r in rows {
        if let Some(n) = &r.next {
            out.push_str(&format!("{}: {n}\n", r.name));
        }
    }
    out
}

/// Where the Studio page reads the last `top` from: next to the fleet state file, or `$FLUXVM_MAC_FLEET_TOP`.
pub fn snapshot_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_FLEET_TOP").filter(|p| !p.is_empty()) {
        return Ok(p.into());
    }
    Ok(state_path()?.with_file_name("mac-fleet-top.json"))
}

fn save_snapshot(snap: &TopSnapshot) {
    let write = || -> Result<()> {
        let p = snapshot_path()?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(snap)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    };
    if let Err(e) = write() {
        eprintln!("note: could not save the fleet snapshot for Studio: {e:#}");
    }
}

pub fn take(
    tr: &dyn Transport,
    targets: &[Target],
    state: &FleetState,
    jobs: usize,
    now: u64,
) -> TopSnapshot {
    let hosts = par_map(targets, jobs, |t| {
        top_host(tr, t, state, Duration::from_secs(30))
    });
    TopSnapshot {
        taken_unix: now,
        summary: summarize(&hosts),
        hosts,
    }
}

pub fn top_cmd(a: TopArgs) -> Result<()> {
    let (targets, inv) = resolve(&a.which, &a.conn)?;
    let filtered = !a.filter.is_empty();
    let targets = crate::mac_fleet_label::filter_targets(targets, &a.filter)?;
    let tr = transport(&a.conn, inv.as_ref());
    if a.conn.dry_run {
        let _ = take(tr.as_ref(), &targets, &FleetState::default(), 1, 0);
        if let Some(d) = tr.as_dry() {
            for l in d.lines.lock().unwrap().iter() {
                println!("{l}");
            }
        }
        return Ok(());
    }
    let live = !a.once && !a.conn.json && std::io::stdout().is_terminal();
    loop {
        let state = FleetState::load(&state_path()?)?;
        let snap = take(
            tr.as_ref(),
            &targets,
            &state,
            jobs(&a.conn),
            SystemClock.now(),
        );
        if !filtered {
            save_snapshot(&snap);
        }
        if a.conn.json {
            println!("{}", serde_json::to_string_pretty(&snap)?);
        } else if live {
            print!("\x1b[H\x1b[2J");
            print!("{}", render_top(&snap.hosts));
            println!("\nrefreshing every {}s; Ctrl-C to stop", a.interval.max(1));
        } else {
            print!("{}", render_top(&snap.hosts));
        }
        if !live {
            let bad = snap.summary.unreachable;
            if bad > 0 {
                anyhow::bail!("{bad} of {} Macs could not be reached", snap.summary.macs);
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(a.interval.max(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_ssh::fake::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const FRESH: &str = "os=Darwin\narch=arm64\nmacos=15.1\nuser=admin\nconsole_user=admin\ndisk_free_gib=200\nfilevault=FileVault is Off.\nautologin=admin\nbrew=yes\nfluxctl_version=0.4.21\nagent_binary=no\ndaemon_loaded=yes\nagent_loaded=no\nkube_loaded=no\ndaemon_healthy=yes\nenrolled=no\nkube_json=\nncpu=8\nmemsize=34359738368\npower=ac\n";

    fn probe_out(guests: usize, disk: u64) -> String {
        let items: Vec<String> = (0..guests)
            .map(|_| "{\"status\":\"running\",\"request\":{\"vcpus\":4,\"memory_mib\":8192,\"apple\":{\"guest_os\":\"macos\"}}}".into())
            .collect();
        FRESH.replace("disk_free_gib=200", &format!("disk_free_gib={disk}"))
            + &format!("vms_json={{\"items\":[{}]}}\n", items.join(","))
    }

    struct Clk(AtomicU64);
    impl Clock for Clk {
        fn now(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        fn sleep(&self, s: u64) {
            self.0.fetch_add(s, Ordering::SeqCst);
        }
    }

    fn fake(guests: usize, disk: u64, therm: &'static str) -> FakeTransport {
        FakeTransport::new(move |t, c| {
            if t.host == "down" {
                failed(255, "Permission denied (publickey).")
            } else if c.contains("pmset -g therm") {
                ok(therm)
            } else if c.contains("echo \"os=$(uname -s)\"") {
                ok(&probe_out(guests, disk))
            } else {
                ok("[]")
            }
        })
    }

    #[test]
    fn top_shows_guests_against_the_limit_thermal_and_disk() {
        let tr = fake(
            2,
            30,
            "Note: No thermal warning level has been recorded\nNote: No performance warning level has been recorded\nCPU_Speed_Limit = 70\n",
        );
        let state = FleetState::default();
        let t = Target::parse("a.local").unwrap();
        let r = top_host(&tr, &t, &state, Duration::from_secs(5));
        assert_eq!((r.state.as_str(), r.macos_guests), ("full", 2));
        assert_eq!(r.thermal, "throttled");
        assert!(r.disk_low && r.next.as_deref().unwrap().contains("FLEET_HOST_THROTTLED"));
        let table = render_top(&[r]);
        assert!(
            table.contains("2/2") && table.contains("throttled 70%") && table.contains("30G (low)")
        );
        assert!(table.contains("2/2 macOS slots busy"), "{table}");
    }

    #[test]
    fn cordoned_and_unreachable_macs_say_what_to_do() {
        let tr = fake(1, 200, "CPU_Speed_Limit = 100\n");
        let mut state = FleetState::default();
        state.drain("a", 1);
        let a = top_host(
            &tr,
            &Target::parse("a").unwrap(),
            &state,
            Duration::from_secs(5),
        );
        assert_eq!(a.state, "draining");
        assert!(a.next.unwrap().contains("drain a --wait"));
        let d = top_host(
            &tr,
            &Target::parse("down").unwrap(),
            &state,
            Duration::from_secs(5),
        );
        assert_eq!(d.state, "unreachable");
        let s = summarize(&[
            top_host(
                &tr,
                &Target::parse("b").unwrap(),
                &state,
                Duration::from_secs(5),
            ),
            d,
        ]);
        assert_eq!(
            (s.macs, s.ready, s.unreachable, s.slots_total),
            (2, 1, 1, 2)
        );
    }

    #[test]
    fn drain_wait_returns_when_the_mac_empties_and_times_out_when_it_does_not() {
        let polls = AtomicU64::new(0);
        let tr = FakeTransport::new(move |_, c| {
            if c.contains("echo \"os=$(uname -s)\"") {
                let n = polls.fetch_add(1, Ordering::SeqCst);
                ok(&probe_out(if n < 2 { 1 } else { 0 }, 200))
            } else {
                ok("[]")
            }
        });
        let t = Target::parse("a").unwrap();
        let clk = Clk(AtomicU64::new(0));
        let mut said = vec![];
        wait_empty(&tr, &t, &clk, 600, 10, &mut |l| said.push(l.to_owned())).unwrap();
        assert!(said.iter().any(|l| l.contains("1 macOS guest")));
        assert!(said.last().unwrap().contains("safe to upgrade"));

        let busy = fake(1, 200, "");
        let e = wait_empty(&busy, &t, &Clk(AtomicU64::new(0)), 30, 10, &mut |_| {}).unwrap_err();
        assert!(
            format!("{e}").contains("FLEET_DRAIN_TIMEOUT") && format!("{e}").contains("cordoned")
        );
    }

    #[test]
    fn the_snapshot_is_json_without_secrets_for_studio() {
        let tr = fake(0, 200, "");
        let snap = take(
            &tr,
            &[Target::parse("a").unwrap()],
            &FleetState::default(),
            2,
            42,
        );
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["taken_unix"], 42);
        assert_eq!(v["hosts"][0]["state"], "ready");
        assert_eq!(v["summary"]["slots_total"], 2);
    }

    #[test]
    fn the_snapshot_carries_each_macs_labels_for_studio() {
        let tr = fake(0, 200, "");
        let mut t = Target::parse("a").unwrap();
        t.labels = vec!["zone=lab".into(), "team=ios".into()];
        let snap = take(&tr, &[t], &FleetState::default(), 2, 1);
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(
            v["hosts"][0]["labels"],
            serde_json::json!({"team": "ios", "zone": "lab"})
        );
    }
}
