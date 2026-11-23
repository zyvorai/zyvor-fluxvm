// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet add HOST`: bring a Mac into the fleet and prove it can take jobs, in one command.
//!
//! In order, stopping at the first step that cannot go on and saying exactly what to fix:
//!
//! 1. **ssh**: the Mac answers, the key is accepted and the host key is trusted (the ssh failure is explained with the
//!    command that fixes it);
//! 2. **preflight**: Apple silicon, macOS, disk and session, checked before anything is installed;
//! 3. **install**: the same idempotent steps as `up` (pinned, checksum-verified FluxVM, daemon, optional enroll and k3s);
//! 4. **runner**: the Mac can do what `mac-fleet run` asks of it: the daemon lists its VMs, `fluxctl mac images` answers
//!    (templates can be distributed to it), and its login session can restore saved state.
//!
//! A Mac that passes all four is `READY` and `fluxctl mac-fleet top` shows it. Safe to repeat.

use crate::mac_errors::coded;
use crate::mac_fleet::{
    Conn, Settings, UpArgs, build_settings, jobs, last_lines, par_map, resolve, transport, up_host,
};
use crate::mac_fleet_remote::{Level, Want, check, parse_facts, probe_script, remote};
use crate::mac_fleet_sched::FleetState;
use crate::mac_fleet_ssh::{Target, Transport, explain_ssh_failure};
use crate::mac_fleet_status::detail_host;
use anyhow::Result;
use serde::Serialize;
use std::time::Duration;

#[derive(clap::Args)]
pub struct AddArgs {
    #[command(flatten)]
    pub up: UpArgs,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Line {
    /// `ok`, `fix` (blocks), `warn`.
    pub status: String,
    pub step: String,
    pub text: String,
    /// The exact command or action when `status` is not `ok`.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AddReport {
    pub host: String,
    pub name: String,
    pub ready: bool,
    pub lines: Vec<Line>,
}

impl AddReport {
    fn push(&mut self, status: &str, step: &str, text: impl Into<String>, fix: Option<String>) {
        self.lines.push(Line {
            status: status.into(),
            step: step.into(),
            text: text.into(),
            fix,
        });
    }
}

pub fn add_host(tr: &dyn Transport, target: &Target, s: &Settings) -> AddReport {
    let mut rep = AddReport {
        host: target.label(),
        name: target.name.clone(),
        ..Default::default()
    };
    // 1. ssh
    let facts = match tr.run(target, &remote(&probe_script()), None) {
        Err(e) => {
            rep.push("fix", "ssh", format!("{e:#}"), None);
            return rep;
        }
        Ok(o) if o.ssh_failed() => {
            let why = explain_ssh_failure(target, &o.stderr);
            rep.push("fix", "ssh", "cannot connect", Some(why));
            return rep;
        }
        Ok(o) if !o.ok() => {
            rep.push(
                "fix",
                "ssh",
                "connected, but the check script failed",
                Some(last_lines(&o.stderr, 3)),
            );
            return rep;
        }
        Ok(o) => parse_facts(&o.stdout),
    };
    rep.push("ok", "ssh", format!("{} answers", target.label()), None);
    // 2. preflight
    let findings = check(
        &facts,
        Want {
            enroll: s.central.is_some(),
            k3s: target.k3s,
        },
    );
    let errors: Vec<_> = findings
        .iter()
        .filter(|f| f.level == Level::Error)
        .collect();
    for f in findings.iter().filter(|f| f.level == Level::Warn) {
        rep.push("warn", "preflight", f.msg.clone(), Some(f.fix.clone()));
    }
    if !errors.is_empty() {
        for f in errors {
            rep.push("fix", "preflight", f.msg.clone(), Some(f.fix.clone()));
        }
        return rep;
    }
    rep.push("ok", "preflight", "this Mac can run FluxVM", None);
    // 3. install
    let r = up_host(tr, target, s);
    if !r.ok {
        let e = r.error.unwrap_or_else(|| "the install failed".into());
        rep.push(
            "fix",
            "install",
            e,
            Some(format!(
                "fix that, then run `fluxctl mac-fleet add {}` again (it picks up where it stopped)",
                target.label()
            )),
        );
        return rep;
    }
    let done = r.steps.iter().filter(|x| x.ok).count();
    rep.push(
        "ok",
        "install",
        format!("{done} step(s) done, FluxVM {}", s.install.version),
        None,
    );
    // 4. runner
    let d = detail_host(tr, target, &FleetState::default(), Duration::from_secs(60));
    if let Some(e) = &d.error {
        rep.push(
            "fix",
            "runner",
            format!("installed, but the Mac cannot take jobs yet: {e}"),
            Some(format!("fluxctl mac-fleet doctor {}", target.label())),
        );
        return rep;
    }
    if !d.templates_known {
        rep.push(
            "fix",
            "runner",
            "`fluxctl mac images` did not answer on the Mac, so templates cannot be distributed to it",
            Some(format!(
                "fluxctl mac-fleet exec {} -- fluxctl mac images",
                target.label()
            )),
        );
        return rep;
    }
    for c in d.headless.iter().flatten().filter(|c| c.status != "ok") {
        rep.push(
            "warn",
            "runner",
            format!("{}: {}", c.name, c.detail),
            c.fix.clone(),
        );
    }
    rep.push(
        "ok",
        "runner",
        "the daemon answers and templates can be distributed",
        None,
    );
    rep.ready = true;
    rep
}

pub fn render_add(reps: &[AddReport]) -> String {
    let mut out = String::new();
    for r in reps {
        out.push_str(&format!(
            "{} ({}): {}\n",
            r.name,
            r.host,
            if r.ready { "READY" } else { "NOT READY" }
        ));
        for l in &r.lines {
            let tag = match l.status.as_str() {
                "ok" => "ok  ",
                "warn" => "warn",
                _ => "FIX ",
            };
            out.push_str(&format!("  [{tag}] {}: {}\n", l.step, l.text));
            if let Some(f) = &l.fix {
                out.push_str(&format!("         fix: {f}\n"));
            }
        }
        if r.ready {
            out.push_str("  next: fluxctl mac-fleet top\n");
        }
    }
    out
}

pub fn add_cmd(a: AddArgs) -> Result<()> {
    let a = a.up;
    if a.conn.dry_run {
        return crate::mac_fleet::run(crate::mac_fleet::MacFleetCommand::Up(a));
    }
    let (mut targets, inv) = resolve(&a.which, &a.conn)?;
    let s = build_settings(&a.flags, inv.as_ref(), &mut targets, a.name.as_deref())?;
    let tr = transport(&a.conn, inv.as_ref());
    let reps = par_map(&targets, jobs(&a.conn), |t| add_host(tr.as_ref(), t, &s));
    print_reports(&a.conn, &reps)?;
    let bad = reps.iter().filter(|r| !r.ready).count();
    if bad > 0 {
        return Err(coded(
            format!(
                "{bad} of {} Mac(s) are not ready; the fixes are above",
                reps.len()
            ),
            "FLEET_ADD_BLOCKED",
        ));
    }
    Ok(())
}

fn print_reports(conn: &Conn, reps: &[AddReport]) -> Result<()> {
    if conn.json {
        println!("{}", serde_json::to_string_pretty(reps)?);
    } else {
        print!("{}", render_add(reps));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_remote::{DEFAULT_RELEASE_BASE, InstallSpec, K3sSpec, Via};
    use crate::mac_fleet_ssh::fake::*;

    fn settings() -> Settings {
        Settings {
            install: InstallSpec {
                version: "0.4.21".into(),
                sha256: None,
                release_base: DEFAULT_RELEASE_BASE.into(),
                via: Via::Release,
            },
            central: None,
            advertise_url: None,
            token: None,
            k3s: K3sSpec::default(),
            labels: vec![],
            all_k3s: false,
        }
    }

    const HEALTHY: &str = "os=Darwin\narch=arm64\nmacos=15.1\nuser=admin\nconsole_user=admin\ndisk_free_gib=200\nfilevault=FileVault is Off.\nautologin=admin\nbrew=yes\nfluxctl_version=0.4.21\nagent_binary=no\ndaemon_loaded=yes\nagent_loaded=no\nkube_loaded=no\ndaemon_healthy=yes\nenrolled=no\nvms_json={\"items\":[]}\nkube_json=\nncpu=8\nmemsize=34359738368\npower=ac\nimages_json=[]\n";

    #[test]
    fn an_unreachable_mac_gets_the_ssh_fix_and_nothing_is_installed() {
        let tr = FakeTransport::new(|_, _| failed(255, "Permission denied (publickey)."));
        let r = add_host(&tr, &Target::parse("admin@m1").unwrap(), &settings());
        assert!(!r.ready);
        assert!(r.lines[0].fix.as_deref().unwrap().contains("ssh-copy-id"));
        assert_eq!(tr.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_preflight_stops_before_the_install() {
        let tr = FakeTransport::new(|_, _| ok(&HEALTHY.replace("arch=arm64", "arch=x86_64")));
        let r = add_host(&tr, &Target::parse("admin@m1").unwrap(), &settings());
        assert!(!r.ready);
        assert!(
            r.lines
                .iter()
                .any(|l| l.status == "fix" && l.step == "preflight")
        );
        assert_eq!(tr.calls.lock().unwrap().len(), 1, "nothing was installed");
    }

    #[test]
    fn a_healthy_mac_is_ready_and_the_report_says_what_is_next() {
        let tr = FakeTransport::new(|_, _| ok(HEALTHY));
        let r = add_host(&tr, &Target::parse("admin@m1").unwrap(), &settings());
        assert!(r.ready, "{r:?}");
        let text = render_add(&[r]);
        assert!(text.contains("READY") && text.contains("mac-fleet top"));
    }
}
