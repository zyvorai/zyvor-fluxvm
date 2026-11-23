// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The "check my whole setup" report behind `fluxctl doctor` and `GET /v1/doctor`.
//!
//! It adds no checks of its own to the health model in [`crate::health`]. It takes the [`ClusterHealth`] that
//! `fluxctl cluster status` already builds, sorts its items into a short list of areas a person thinks in
//! (virtualization, disk, heat and the login session, the daemon, Kubernetes, the Mac fleet, the gateway add-on), and
//! writes one green, yellow or red line for each, with the exact next command. Things the health model cannot see
//! (the Docker context, registry logins, thermal limit) come in as ready-made lines or as [`HostFacts`]. Pure: the CLI
//! and the daemon each gather the facts their own way and call [`build`].

use crate::health::{ClusterHealth, Fix, Item, Severity};
use serde::{Deserialize, Serialize};

pub const SCHEMA: u32 = 1;

/// A CPU speed limit below this percent is worth a yellow line (the same bar the Mac scheduler uses to stop placing VMs).
pub const THERMAL_WARN_PCT: u32 = 80;

/// The command that writes a support bundle.
pub const BUNDLE_COMMAND: &str = "fluxctl support-bundle";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupCheck {
    /// Stable name of the area: `virtualization`, `disk`, `session`, `host`, `daemon`, `docker`, `registry`,
    /// `cluster`, `fleet`, `gateway`, `bundle`.
    pub area: String,
    pub title: String,
    pub light: Severity,
    /// One plain-English line.
    pub message: String,
    /// What to do next, in words, with the exact command in backticks. Present on every yellow and red line.
    pub next: Option<String>,
    /// The command alone, for a copy button.
    pub command: Option<String>,
    /// `fluxctl doctor --fix` can do the next step by itself.
    pub safe_fix: bool,
    /// False when the check could not run here (no fleet inventory, a daemon that cannot see the user's home).
    pub checked: bool,
}

impl SetupCheck {
    pub fn ok(area: &str, title: &str, message: impl Into<String>) -> Self {
        Self {
            area: area.into(),
            title: title.into(),
            light: Severity::Ok,
            message: message.into(),
            next: None,
            command: None,
            safe_fix: false,
            checked: true,
        }
    }

    pub fn problem(
        area: &str,
        title: &str,
        light: Severity,
        message: impl Into<String>,
        command: Option<&str>,
        advice: impl Into<String>,
    ) -> Self {
        let advice = advice.into();
        Self {
            area: area.into(),
            title: title.into(),
            light,
            message: message.into(),
            next: Some(match command {
                Some(c) if advice.is_empty() => format!("Run `{c}`."),
                Some(c) => format!("{advice} Run `{c}`."),
                None => advice,
            }),
            command: command.map(str::to_owned),
            safe_fix: false,
            checked: true,
        }
    }

    pub fn unchecked(area: &str, title: &str, why: impl Into<String>, command: &str) -> Self {
        Self {
            area: area.into(),
            title: title.into(),
            light: Severity::Ok,
            message: why.into(),
            next: Some(format!("Run `{command}` to check it.")),
            command: Some(command.to_owned()),
            safe_fix: false,
            checked: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupReport {
    pub schema: u32,
    pub generated_at: u64,
    /// The worst light of the lines that were checked.
    pub light: Severity,
    /// "Everything checked is fine" / "2 things need attention".
    pub headline: String,
    pub checks: Vec<SetupCheck>,
    /// The single most useful next step, or `None` when everything is green.
    pub next: Option<String>,
    /// Something is red: a support bundle is worth making now.
    pub offer_support_bundle: bool,
    pub support_bundle_command: String,
}

/// Facts the health model does not carry. Every field is optional: unknown never makes a line yellow or red.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    /// `pmset -g therm` CPU speed limit when below 100.
    pub thermal_limit: Option<u32>,
    pub disk_free_gib: Option<u64>,
}

const AREAS: &[(&str, &str)] = &[
    ("virtualization", "Virtual machines"),
    ("disk", "Disk space"),
    ("session", "Login session and heat"),
    ("host", "Other host setup"),
    ("daemon", "FluxVM service"),
    ("cluster", "Kubernetes"),
    ("fleet", "Mac fleet"),
    ("gateway", "Gateway add-on"),
];

fn area_of(i: &Item, local: bool) -> &'static str {
    if !local {
        return "fleet";
    }
    if i.source == "gateway" {
        return "gateway";
    }
    match i.explain.as_deref().unwrap_or("") {
        "NO_KVM" | "RUNNER_PROBLEM" | "UNSUPPORTED_HOST" => "virtualization",
        "LOW_DISK" => "disk",
        "NO_LOGIN_SESSION" | "SECURITY_SETTING" | "POWER_SETTINGS" => "session",
        "DAEMON_DOWN" | "NOT_INSTALLED" | "AGENT_NOT_LOADED" | "STALE_PATHS" => "daemon",
        "K3S_NOT_READY" => "cluster",
        c if c.contains("GATEWAY") || c.starts_with("ROUTE_") => "gateway",
        _ => "host",
    }
}

fn next_of(f: Option<&Fix>, code: Option<&str>) -> (Option<String>, String) {
    match f {
        Some(f) if f.command.is_some() => (f.command.clone(), String::new()),
        Some(f) if f.steps.is_some() => (None, f.steps.clone().unwrap_or_default()),
        _ => (
            None,
            format!(
                "Run `fluxctl explain {}` for the steps.",
                code.unwrap_or("CHECK_FAILED")
            ),
        ),
    }
}

fn green_text(area: &str, local_count: usize) -> String {
    match area {
        "virtualization" => "This machine can start virtual machines.".into(),
        "disk" => "There is enough free disk space.".into(),
        "session" => "The login session is usable and the machine is not throttled.".into(),
        "host" => "The rest of the host setup is fine.".into(),
        "daemon" => "FluxVM is installed and running.".into(),
        "cluster" => "No Kubernetes problems found.".into(),
        "fleet" => format!("All {local_count} other machines answered and are fine."),
        _ => "No problems found.".into(),
    }
}

/// One line for one area: the worst item decides the light, the message and the next step.
fn line(
    area: &str,
    title: &str,
    items: &[&Item],
    extra_ok: Option<String>,
    fleet: usize,
) -> SetupCheck {
    let Some(worst) = items.iter().max_by_key(|i| i.severity) else {
        return SetupCheck::ok(
            area,
            title,
            extra_ok.unwrap_or_else(|| green_text(area, fleet)),
        );
    };
    if worst.severity == Severity::Ok {
        return SetupCheck::ok(area, title, worst.message.clone());
    }
    let (command, advice) = next_of(worst.fix.as_ref(), worst.explain.as_deref());
    let more = items.iter().filter(|i| i.severity != Severity::Ok).count() - 1;
    let message = if more > 0 {
        format!("{} (and {more} more in this area)", worst.message)
    } else {
        worst.message.clone()
    };
    let mut c = SetupCheck::problem(
        area,
        title,
        worst.severity,
        message,
        command.as_deref(),
        advice,
    );
    c.safe_fix = worst.fix.as_ref().is_some_and(|f| f.safe);
    if c.safe_fix {
        c.next = Some("Run `fluxctl doctor --fix`; it is safe and does this for you.".into());
        c.command = Some("fluxctl doctor --fix".into());
    }
    c
}

/// Build the report. `extras` are finished lines from checks the health model does not have (Docker context, registry
/// logins); `fleet_machines` is how many other machines were looked at (0 when none was asked for).
pub fn build(
    h: &ClusterHealth,
    host: &HostFacts,
    extras: Vec<SetupCheck>,
    fleet_machines: usize,
    now: u64,
) -> SetupReport {
    let mut buckets: Vec<(&str, Vec<&Item>)> = AREAS.iter().map(|(a, _)| (*a, vec![])).collect();
    let mut thermal: Vec<Item> = vec![];
    if let Some(n) = host.thermal_limit.filter(|n| *n < THERMAL_WARN_PCT) {
        thermal.push(Item {
            id: "this-machine/thermal".into(),
            machine: "this machine".into(),
            source: "host".into(),
            severity: Severity::Warn,
            message: format!("This machine is running hot: the CPU is limited to {n}% speed, so VMs run slowly and new ones are not placed here."),
            cause: None,
            fix: Some(Fix {
                steps: Some("Let it cool (move it out of the sun, clear the vents, stop heavy jobs) and check again.".into()),
                ..Default::default()
            }),
            explain: Some("POWER_SETTINGS".into()),
        });
    }
    for m in &h.machines {
        for i in &m.items {
            let a = area_of(i, m.local);
            if let Some(b) = buckets.iter_mut().find(|(n, _)| *n == a) {
                b.1.push(i);
            }
        }
    }
    for i in h.items.iter().filter(|i| i.source == "gateway") {
        if let Some(b) = buckets.iter_mut().find(|(n, _)| *n == "gateway") {
            b.1.push(i);
        }
    }
    for i in &thermal {
        if let Some(b) = buckets.iter_mut().find(|(n, _)| *n == "session") {
            b.1.push(i);
        }
    }

    let mut checks: Vec<SetupCheck> = vec![];
    for (area, title) in AREAS {
        let items = &buckets.iter().find(|(n, _)| n == area).unwrap().1;
        match *area {
            // The "other host setup" line only appears when it has something to say.
            "host" if items.is_empty() => continue,
            "fleet" if items.is_empty() && fleet_machines == 0 => {
                checks.push(SetupCheck::unchecked(
                    "fleet",
                    title,
                    "No Mac fleet was given, so none was checked.",
                    "fluxctl doctor -f inventory.yaml",
                ));
                continue;
            }
            "gateway" if items.is_empty() => {
                // Only claim "fine" when a cluster was actually looked at; the caller says so with an extra line.
                if extras.iter().any(|e| e.area == "gateway") {
                    continue;
                }
                checks.push(SetupCheck::ok(
                    "gateway",
                    title,
                    "No Gateway or route problems found (or no cluster uses the gateway add-on).",
                ));
                continue;
            }
            _ => {}
        }
        let ok_text = match *area {
            "disk" => host
                .disk_free_gib
                .map(|g| format!("{g} GiB free for FluxVM.")),
            _ => None,
        };
        checks.push(line(area, title, items, ok_text, fleet_machines));
    }
    checks.extend(extras);

    let light = checks
        .iter()
        .filter(|c| c.checked)
        .map(|c| c.light)
        .max()
        .unwrap_or(Severity::Ok);
    let red = light == Severity::Fail;
    // The support bundle line: always available, and pushed forward when something is red.
    let mut bundle = SetupCheck::ok(
        "bundle",
        "Support bundle",
        "Ready: one file with versions, health and logs, secrets removed.",
    );
    bundle.command = Some(BUNDLE_COMMAND.into());
    if red {
        bundle.message = "Something is red. A support bundle packs the logs and this health report for whoever helps you, with secrets removed.".into();
        bundle.next = Some(format!(
            "Run `{BUNDLE_COMMAND}` (or `fluxctl doctor --bundle`) and send the file."
        ));
    }
    checks.push(bundle);

    let attention = checks
        .iter()
        .filter(|c| c.checked && c.light != Severity::Ok)
        .count();
    let headline = match (attention, red) {
        (0, _) => "Your setup looks good.".to_owned(),
        (1, true) => "1 thing is broken.".to_owned(),
        (n, true) => format!("{n} things need attention, at least one is broken."),
        (1, false) => "1 thing could be better.".to_owned(),
        (n, false) => format!("{n} things could be better."),
    };
    let first = checks
        .iter()
        .filter(|c| c.checked && c.light != Severity::Ok)
        .max_by_key(|c| (c.light, c.safe_fix));
    SetupReport {
        schema: SCHEMA,
        generated_at: now,
        light,
        headline,
        next: first.and_then(|c| c.next.clone()),
        offer_support_bundle: red,
        support_bundle_command: BUNDLE_COMMAND.into(),
        checks,
    }
}

/// Plain text: one line per check, then the next command under each one that is not green.
pub fn render(r: &SetupReport, color: bool) -> String {
    let mut o = format!("{}\n\n", r.headline);
    for c in &r.checks {
        let (word, code) = if !c.checked {
            ("skipped", "2")
        } else {
            match c.light {
                Severity::Ok => ("green ", "32"),
                Severity::Warn => ("yellow", "33"),
                Severity::Fail => ("red   ", "31"),
            }
        };
        let tag = if color {
            format!("\x1b[{code}m{word}\x1b[0m")
        } else {
            word.to_owned()
        };
        o.push_str(&format!("{tag}  {}: {}\n", c.title, c.message));
        if let Some(n) = &c.next
            && (c.light != Severity::Ok || !c.checked || c.area == "bundle")
        {
            o.push_str(&format!("        -> {n}\n"));
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{self, MachineHealth, NodeFacts, Platform, RawCheck};

    fn node(checks: Vec<RawCheck>) -> NodeFacts {
        NodeFacts {
            name: "mini".into(),
            platform: Platform::Linux,
            local: true,
            reachable: true,
            installed: Some(true),
            daemon_up: Some(true),
            disk_free_gib: Some(100),
            checks,
            ..Default::default()
        }
    }

    fn raw(name: &str, sev: Severity, detail: &str, fix: Option<&str>) -> RawCheck {
        RawCheck {
            source: "doctor".into(),
            name: name.into(),
            severity: sev,
            detail: detail.into(),
            fix: fix.map(str::to_owned),
            path: None,
        }
    }

    fn report(n: NodeFacts, host: HostFacts) -> SetupReport {
        let h = health::assess(&[n], 1_700_000_000);
        build(&h, &host, vec![], 0, 1)
    }

    #[test]
    fn a_healthy_machine_is_green_with_no_next_step() {
        let r = report(node(vec![]), HostFacts::default());
        assert_eq!(r.light, Severity::Ok, "{:#?}", r.checks);
        assert!(r.next.is_none() && !r.offer_support_bundle);
        assert!(r.headline.contains("looks good"));
        for a in [
            "virtualization",
            "disk",
            "session",
            "daemon",
            "cluster",
            "fleet",
            "gateway",
            "bundle",
        ] {
            assert!(r.checks.iter().any(|c| c.area == a), "missing {a}");
        }
    }

    #[test]
    fn a_missing_kvm_is_red_in_the_virtualization_line_with_the_command() {
        let r = report(
            node(vec![raw(
                "kvm",
                Severity::Fail,
                "/dev/kvm is missing",
                Some("run `sudo modprobe kvm`"),
            )]),
            HostFacts::default(),
        );
        let c = r
            .checks
            .iter()
            .find(|c| c.area == "virtualization")
            .unwrap();
        assert_eq!(c.light, Severity::Fail);
        assert_eq!(c.command.as_deref(), Some("sudo usermod -aG kvm $USER"));
        assert!(
            c.next
                .as_deref()
                .unwrap()
                .contains("`sudo usermod -aG kvm $USER`")
        );
        assert_eq!(r.light, Severity::Fail);
        assert!(r.offer_support_bundle);
        let b = r.checks.iter().find(|c| c.area == "bundle").unwrap();
        assert!(
            b.next
                .as_deref()
                .unwrap()
                .contains("fluxctl support-bundle")
        );
    }

    #[test]
    fn a_hot_machine_is_yellow_and_a_cool_one_is_not() {
        let r = report(
            node(vec![]),
            HostFacts {
                thermal_limit: Some(60),
                ..Default::default()
            },
        );
        let c = r.checks.iter().find(|c| c.area == "session").unwrap();
        assert_eq!(c.light, Severity::Warn);
        assert!(c.message.contains("60%") && c.next.is_some());
        let r = report(
            node(vec![]),
            HostFacts {
                thermal_limit: Some(90),
                ..Default::default()
            },
        );
        assert_eq!(r.light, Severity::Ok);
    }

    #[test]
    fn every_yellow_and_red_line_says_what_to_do() {
        let r = report(
            node(vec![
                raw("ip forwarding", Severity::Warn, "off", None),
                raw("disk", Severity::Fail, "1 GiB free", Some("free space")),
                raw("kvm", Severity::Fail, "missing", None),
            ]),
            HostFacts::default(),
        );
        for c in r
            .checks
            .iter()
            .filter(|c| c.checked && c.light != Severity::Ok)
        {
            assert!(c.next.as_deref().is_some_and(|n| !n.is_empty()), "{c:?}");
        }
    }

    #[test]
    fn unchecked_areas_do_not_colour_the_report_and_say_how_to_check() {
        let r = report(node(vec![]), HostFacts::default());
        let f = r.checks.iter().find(|c| c.area == "fleet").unwrap();
        assert!(!f.checked && f.next.as_deref().unwrap().contains("fluxctl doctor -f"));
        assert_eq!(r.light, Severity::Ok);
    }

    #[test]
    fn items_on_other_machines_land_in_the_fleet_line() {
        let mut other = node(vec![raw("daemon", Severity::Fail, "down", None)]);
        other.name = "mac2".into();
        other.local = false;
        let h = health::assess(&[node(vec![]), other], 1);
        let r = build(&h, &HostFacts::default(), vec![], 1, 1);
        let f = r.checks.iter().find(|c| c.area == "fleet").unwrap();
        assert_eq!(f.light, Severity::Fail);
        assert_eq!(
            r.checks.iter().find(|c| c.area == "daemon").unwrap().light,
            Severity::Ok
        );
    }

    #[test]
    fn a_safe_fix_points_at_doctor_fix() {
        let mut m = MachineHealth {
            name: "m".into(),
            platform: Platform::Mac,
            local: true,
            light: Severity::Warn,
            headline: String::new(),
            version: None,
            update: None,
            capacity: Default::default(),
            last_backup_age_secs: None,
            last_backup: String::new(),
            checks_passed: 0,
            items: vec![Item {
                id: "m/x".into(),
                machine: "m".into(),
                source: "doctor".into(),
                severity: Severity::Warn,
                message: "FluxVM is stopped".into(),
                cause: None,
                fix: Some(Fix {
                    safe: true,
                    action: Some(health::FixAction::StartService),
                    ..Default::default()
                }),
                explain: Some("AGENT_NOT_LOADED".into()),
            }],
        };
        let h0 = health::assess(&[node(vec![])], 1);
        let mut h = h0.clone();
        m.name = h.machines[0].name.clone();
        h.machines[0] = m;
        let r = build(&h, &HostFacts::default(), vec![], 0, 1);
        let c = r.checks.iter().find(|c| c.area == "daemon").unwrap();
        assert!(c.safe_fix && c.command.as_deref() == Some("fluxctl doctor --fix"));
    }

    #[test]
    fn render_lists_lights_and_next_commands() {
        let r = report(
            node(vec![raw(
                "kvm",
                Severity::Fail,
                "missing",
                Some("run `sudo modprobe kvm`"),
            )]),
            HostFacts::default(),
        );
        let t = render(&r, false);
        assert!(
            t.contains("red") && t.contains("-> ") && t.contains("sudo usermod"),
            "{t}"
        );
        assert!(t.contains("green"));
    }

    #[test]
    fn json_round_trips() {
        let r = report(node(vec![]), HostFacts::default());
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["light"], "ok");
        let back: SetupReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
}
