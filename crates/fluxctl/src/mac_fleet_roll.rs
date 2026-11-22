// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet upgrade --rolling`: one Mac at a time. For each: drain it (no new job is placed, running jobs
//! finish), wait until it has no VM, upgrade it, wait until its daemon answers at the new version (the health gate), put it
//! back. The first Mac that fails any step stops the roll: it stays drained so no job lands on a half-upgraded Mac, and the
//! ones not yet touched are left alone. A Mac that was already drained before the roll is not resumed afterwards.
//!
//! Not run against real Macs: see docs/mac-fleet-scheduler.md.

use crate::mac_fleet::{HostResult, StatusRow, status_host, upgrade_host};
use crate::mac_fleet_remote::InstallSpec;
use crate::mac_fleet_run::{Clock, set_drain};
use crate::mac_fleet_sched::FleetState;
use crate::mac_fleet_ssh::{Target, Transport};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct RollOpts {
    /// Seconds to wait for a drained Mac's VMs to end.
    pub drain_timeout: u64,
    /// Seconds to wait for the daemon to answer at the new version after the upgrade.
    pub health_timeout: u64,
    pub poll: u64,
}

impl Default for RollOpts {
    fn default() -> Self {
        Self { drain_timeout: 3600, health_timeout: 300, poll: 10 }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RollStep {
    pub host: String,
    /// `upgraded`, `failed` or `skipped`.
    pub outcome: String,
    pub detail: String,
    /// Still drained when the roll ended.
    pub drained: bool,
}

fn same_version(a: &str, b: &str) -> bool {
    a.trim_start_matches('v') == b.trim_start_matches('v')
}

/// VM count as `status` reports it (`-` when unknown).
fn vm_count(r: &StatusRow) -> Option<u32> {
    r.vms.parse().ok()
}

/// Why this status row is not yet a healthy Mac at `want`, or `None` when it is.
pub fn health_problem(r: &StatusRow, want: &str) -> Option<String> {
    if r.state != "ok" {
        return Some(r.error.clone().unwrap_or_else(|| "unreachable".into()));
    }
    if r.daemon != "up" {
        return Some(format!("the daemon is {}", r.daemon));
    }
    if !same_version(&r.version, want) {
        return Some(format!("it runs {} (wanted {want})", r.version));
    }
    None
}

fn wait_until(clock: &dyn Clock, timeout: u64, poll: u64, mut ready: impl FnMut() -> Option<String>) -> Result<(), String> {
    let start = clock.now();
    loop {
        let why = ready();
        let Some(why) = why else { return Ok(()) };
        let waited = clock.now().saturating_sub(start);
        if waited >= timeout {
            return Err(format!("{why} (after {waited}s)"));
        }
        clock.sleep(poll.min(timeout - waited).max(1));
    }
}

pub fn roll(
    tr: &dyn Transport,
    targets: &[Target],
    install: &InstallSpec,
    state_path: &Path,
    opts: &RollOpts,
    clock: &dyn Clock,
    log: &dyn Fn(&str),
) -> anyhow::Result<Vec<RollStep>> {
    let mut steps: Vec<RollStep> = vec![];
    let mut stopped = false;
    for t in targets {
        let host = t.label();
        if stopped {
            steps.push(RollStep { host, outcome: "skipped".into(), detail: "an earlier Mac failed".into(), drained: false });
            continue;
        }
        let was_drained = FleetState::load(state_path)?.is_drained(&t.name);
        log(&format!("{host}: draining"));
        set_drain(state_path, std::slice::from_ref(&t.name), true, clock.now())?;
        let fail = |why: String, steps: &mut Vec<RollStep>| {
            log(&format!("{host}: {why}; the roll stops here and this Mac stays drained"));
            steps.push(RollStep { host: host.clone(), outcome: "failed".into(), detail: why, drained: true });
        };
        // 1. its VMs end
        let drained = wait_until(clock, opts.drain_timeout, opts.poll, || {
            let r = status_host(tr, t);
            match (r.state.as_str(), vm_count(&r)) {
                ("ok", Some(0)) => None,
                ("ok", Some(n)) => Some(format!("{n} VM(s) still running")),
                _ => r.error.or(Some("cannot tell how many VMs run".into())),
            }
        });
        if let Err(e) = drained {
            fail(format!("did not drain: {e}"), &mut steps);
            stopped = true;
            continue;
        }
        // 2. upgrade (never forced: a VM that appeared in the meantime refuses it)
        log(&format!("{host}: upgrading to {}", install.version));
        let r: HostResult = upgrade_host(tr, t, install, false);
        if !r.ok {
            fail(format!("upgrade failed: {}", r.error.unwrap_or_else(|| "see steps".into())), &mut steps);
            stopped = true;
            continue;
        }
        // 3. health gate
        let healthy = wait_until(clock, opts.health_timeout, opts.poll, || health_problem(&status_host(tr, t), &install.version));
        if let Err(e) = healthy {
            fail(format!("failed the health check: {e}"), &mut steps);
            stopped = true;
            continue;
        }
        // 4. back into rotation
        if !was_drained {
            set_drain(state_path, std::slice::from_ref(&t.name), false, clock.now())?;
        }
        log(&format!("{host}: upgraded and healthy{}", if was_drained { " (left drained, as it was before)" } else { ", resumed" }));
        steps.push(RollStep { host, outcome: "upgraded".into(), detail: format!("now {}", install.version), drained: was_drained });
    }
    Ok(steps)
}

pub fn render(steps: &[RollStep]) -> String {
    crate::mac_fleet::render_table(
        &["HOST", "RESULT", "DRAINED", "DETAIL"],
        &steps
            .iter()
            .map(|s| vec![s.host.clone(), s.outcome.clone(), if s.drained { "yes" } else { "no" }.into(), s.detail.clone()])
            .collect::<Vec<_>>(),
    )
}

/// The command: roll through `targets`, print the table (or JSON), fail if any Mac failed.
pub fn roll_cmd(
    conn: &crate::mac_fleet::Conn,
    tr: &dyn Transport,
    targets: &[Target],
    install: &InstallSpec,
    opts: RollOpts,
) -> anyhow::Result<()> {
    if conn.dry_run {
        println!(
            "dry run: each of {} Mac(s), one at a time: drain, wait for its VMs to end, upgrade to {}, wait for the daemon, resume",
            targets.len(),
            install.version
        );
        return Ok(());
    }
    let state = crate::mac_fleet_sched::state_path()?;
    let steps = roll(tr, targets, install, &state, &opts, &crate::mac_fleet_run::SystemClock, &|m| eprintln!("{m}"))?;
    if conn.json {
        println!("{}", serde_json::to_string_pretty(&steps)?);
    } else {
        print!("{}", render(&steps));
    }
    if let Some(f) = steps.iter().find(|s| s.outcome == "failed") {
        anyhow::bail!(
            "the rolling upgrade stopped at {}: {}. That Mac stays drained (`fluxctl mac-fleet resume`); {} Mac(s) were not touched",
            f.host,
            f.detail,
            steps.iter().filter(|s| s.outcome == "skipped").count()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_remote::Via;
    use crate::mac_fleet_ssh::fake::{FakeTransport, ok};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
        fn sleep(&self, s: u64) {
            self.0.fetch_add(s, Ordering::SeqCst);
        }
    }

    fn install() -> InstallSpec {
        InstallSpec { version: "0.5.0".into(), sha256: None, release_base: "https://example.invalid".into(), via: Via::Release }
    }

    fn row(vms: &str, daemon: &str, version: &str) -> StatusRow {
        StatusRow {
            host: "h".into(),
            state: "ok".into(),
            version: version.into(),
            daemon: daemon.into(),
            vms: vms.into(),
            macos_slots: "0/2".into(),
            k3s: "-".into(),
            node: "-".into(),
            session: "-".into(),
            error: None,
        }
    }

    #[test]
    fn the_health_gate_names_what_is_wrong() {
        assert_eq!(health_problem(&row("0", "up", "0.5.0"), "v0.5.0"), None);
        assert!(health_problem(&row("0", "down", "0.5.0"), "0.5.0").unwrap().contains("daemon is down"));
        assert!(health_problem(&row("0", "up", "0.4.9"), "0.5.0").unwrap().contains("runs 0.4.9"));
        let mut r = row("-", "-", "-");
        r.state = "unreachable".into();
        assert!(health_problem(&r, "0.5.0").is_some());
    }

    /// A Mac that reports `version` for status probes once it has been "upgraded" (a command mentioning `install`).
    fn mac(version_after: &'static str, vms_before: u32) -> impl Fn(&Target, &str) -> crate::mac_fleet_ssh::ExecOut + Send + Sync {
        let upgraded = Mutex::new(false);
        let polls = Mutex::new(0u32);
        move |_, c| {
            if c.contains("fluxctl_version=") {
                let up = *upgraded.lock().unwrap();
                let mut p = polls.lock().unwrap();
                *p += 1;
                let vms = if !up && *p <= vms_before { 1 } else { 0 };
                let v = if up { version_after } else { "0.4.9" };
                let items: Vec<String> = (0..vms).map(|_| r#"{"status":"running","request":{}}"#.to_owned()).collect();
                ok(&format!(
                    "os=Darwin\narch=arm64\nmacos=15.1\nuser=admin\nconsole_user=admin\ndisk_free_gib=300\nfilevault=FileVault is Off.\nautologin=admin\nbrew=yes\nfluxctl_version={v}\nagent_binary=yes\ndaemon_loaded=yes\nagent_loaded=no\nkube_loaded=no\ndaemon_healthy=yes\nenrolled=no\nvms_json={{\"items\":[{}]}}\n",
                    items.join(",")
                ))
            } else {
                // Any step that is not a probe is the install or the restart.
                *upgraded.lock().unwrap() = true;
                ok("")
            }
        }
    }

    #[test]
    fn macs_are_rolled_one_at_a_time_drained_then_resumed() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("s.json");
        let tr = FakeTransport::new(mac("0.5.0", 2));
        let targets = vec![Target::parse("admin@a.local").unwrap(), Target::parse("admin@b.local").unwrap()];
        let clock = FakeClock(AtomicU64::new(0));
        let logs = Mutex::new(vec![]);
        let log = |m: &str| logs.lock().unwrap().push(m.to_owned());
        let steps = roll(&tr, &targets, &install(), &state, &RollOpts { drain_timeout: 100, health_timeout: 100, poll: 5 }, &clock, &log).unwrap();
        assert_eq!(steps.iter().map(|s| s.outcome.as_str()).collect::<Vec<_>>(), ["upgraded", "upgraded"]);
        assert!(clock.now() > 0, "it waited for the VMs to end");
        // nothing is left drained
        assert!(FleetState::load(&state).unwrap().drained.is_empty());
        // a was fully done before b was touched
        let calls = tr.calls.lock().unwrap();
        let first_b = calls.iter().position(|c| c.host == "b.local").unwrap();
        let last_a = calls.iter().rposition(|c| c.host == "a.local").unwrap();
        assert!(last_a < first_b, "one Mac at a time");
    }

    #[test]
    fn a_failed_health_gate_stops_the_roll_and_leaves_the_mac_drained() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("s.json");
        // After the upgrade the daemon still reports the old version.
        let tr = FakeTransport::new(mac("0.4.9", 0));
        let targets = vec![Target::parse("admin@a.local").unwrap(), Target::parse("admin@b.local").unwrap()];
        let clock = FakeClock(AtomicU64::new(0));
        let steps = roll(&tr, &targets, &install(), &state, &RollOpts { drain_timeout: 20, health_timeout: 20, poll: 5 }, &clock, &|_| {}).unwrap();
        assert_eq!(steps[0].outcome, "failed");
        assert!(steps[0].detail.contains("health check"), "{}", steps[0].detail);
        assert_eq!(steps[1].outcome, "skipped");
        assert!(tr.calls_for("b.local").is_empty(), "the second Mac is never touched");
        let st = FleetState::load(&state).unwrap();
        assert!(st.is_drained("a") && !st.is_drained("b"));
    }

    #[test]
    fn a_mac_that_never_drains_is_not_upgraded() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("s.json");
        let tr = FakeTransport::new(mac("0.5.0", 1000));
        let targets = vec![Target::parse("admin@a.local").unwrap()];
        let clock = FakeClock(AtomicU64::new(0));
        let steps = roll(&tr, &targets, &install(), &state, &RollOpts { drain_timeout: 30, health_timeout: 30, poll: 10 }, &clock, &|_| {}).unwrap();
        assert!(steps[0].detail.contains("did not drain"), "{}", steps[0].detail);
        assert!(!tr.calls_for("a.local").iter().any(|c| c.command.contains("install")), "no install ran");
    }

    #[test]
    fn a_mac_drained_before_the_roll_stays_drained() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("s.json");
        set_drain(&state, &["a".to_owned()], true, 1).unwrap();
        let tr = FakeTransport::new(mac("0.5.0", 0));
        let targets = vec![Target::parse("admin@a.local").unwrap()];
        let clock = FakeClock(AtomicU64::new(0));
        let steps = roll(&tr, &targets, &install(), &state, &RollOpts::default(), &clock, &|_| {}).unwrap();
        assert_eq!(steps[0].outcome, "upgraded");
        assert!(FleetState::load(&state).unwrap().is_drained("a"));
    }
}
