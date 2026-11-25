// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl agents`, `GET /v1/agents` and the Studio Home "Background services" card: every FluxVM LaunchAgent on this
//! Mac (the daemon, the Kubernetes and Docker agents, per-worker keepers, the fleet node agent, the vmnet and USB helpers)
//! in one list, with a plain-English health and what to do.
//!
//! Read-only except [`repair`] (loads an installed agent that launchd does not have loaded) and [`restart`]
//! (`launchctl kickstart -k`). It never edits or deletes a plist and never uses sudo. On other systems the list is empty.
//! The decisions ([`classify`], [`plan_repairs`], [`resolve`], [`parse_plist`]) are pure and tested; the launchctl calls
//! go through [`Launchctl`], which tests fake.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Version of the JSON document. Adding fields does not bump it.
pub const SCHEMA: u32 = 1;
const PREFIX: &str = "dev.zyvor.fluxvm";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Loaded, and either running or idle between runs with a clean last exit.
    Ok,
    /// Installed but launchd does not have it loaded, so it will never start.
    NotLoaded,
    /// Loaded, not running, and its last exit was an error: it is failing to start or dying.
    Crashing,
    /// The program the plist runs no longer exists (a moved or removed install).
    StaleProgram,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentInfo {
    pub label: String,
    /// What it is, in words.
    pub name: String,
    pub loaded: bool,
    pub state: Option<String>,
    pub pid: Option<u32>,
    pub last_exit_code: Option<i32>,
    pub program: Option<String>,
    pub program_exists: bool,
    pub plist: PathBuf,
    pub log: Option<PathBuf>,
    pub health: Health,
    /// What to do, when the health is not `ok`.
    pub what_to_do: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
    pub schema: u32,
    pub agents: Vec<AgentInfo>,
    pub headline: String,
}

/// The two launchctl calls this module makes; tests fake it.
pub trait Launchctl {
    /// `launchctl print gui/<uid>/<label>`: the text when loaded, `None` when it is not.
    fn print(&self, label: &str) -> Option<String>;
    /// `launchctl bootstrap gui/<uid> <plist>`.
    fn bootstrap(&self, plist: &Path) -> Result<(), String>;
    /// `launchctl kickstart -k gui/<uid>/<label>`.
    fn kickstart(&self, label: &str) -> Result<(), String>;
}

/// The real launchctl of the logged-in user.
pub struct System;

impl System {
    fn domain() -> String {
        // SAFETY: getuid has no preconditions.
        format!("gui/{}", unsafe { libc::getuid() })
    }
    fn run(args: &[&str]) -> Result<std::process::Output, String> {
        std::process::Command::new("launchctl")
            .args(args)
            .output()
            .map_err(|e| format!("running launchctl: {e}"))
    }
}

impl Launchctl for System {
    fn print(&self, label: &str) -> Option<String> {
        let o = Self::run(&["print", &format!("{}/{label}", Self::domain())]).ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }
    fn bootstrap(&self, plist: &Path) -> Result<(), String> {
        let o = Self::run(&["bootstrap", &Self::domain(), &plist.display().to_string()])?;
        if o.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&o.stderr).trim().to_owned())
        }
    }
    fn kickstart(&self, label: &str) -> Result<(), String> {
        let o = Self::run(&["kickstart", "-k", &format!("{}/{label}", Self::domain())])?;
        if o.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&o.stderr).trim().to_owned())
        }
    }
}

/// What the agent is, from its label.
pub fn describe(label: &str) -> String {
    match label {
        "dev.zyvor.fluxvm" => "FluxVM daemon".into(),
        "dev.zyvor.fluxvm.kube" => "Kubernetes cluster supervisor".into(),
        "dev.zyvor.fluxvm.docker-engine" => "Docker engine".into(),
        "dev.zyvor.fluxvm-agent" => "Fleet node agent".into(),
        "dev.zyvor.fluxvm.vmnetd" => "VM networking helper (vmnet)".into(),
        "dev.zyvor.fluxvm.usbd" => "USB access helper".into(),
        l => match l.strip_prefix("dev.zyvor.fluxvm.kube.") {
            Some(node) => format!("Keeps Kubernetes worker {node} running"),
            None => match l.strip_prefix("dev.zyvor.fluxvm-") {
                Some(role) => format!("FluxVM {role} service"),
                None => l.to_owned(),
            },
        },
    }
}

/// The command that installs it again, for a stale program path.
pub fn install_command(label: &str) -> String {
    match label {
        "dev.zyvor.fluxvm" => "fluxctl service install".into(),
        "dev.zyvor.fluxvm.kube" => "fluxctl kube agent install".into(),
        "dev.zyvor.fluxvm.docker-engine" => "fluxctl docker agent install".into(),
        "dev.zyvor.fluxvm-agent" => "fluxctl node-agent install".into(),
        "dev.zyvor.fluxvm.vmnetd" => "fluxctl helper-agent vmnet install".into(),
        "dev.zyvor.fluxvm.usbd" => "fluxctl helper-agent usb install".into(),
        l => match l
            .strip_prefix("dev.zyvor.fluxvm.kube.")
            .and_then(|n| n.rsplit_once("-w"))
        {
            Some((name, w)) => {
                format!("fluxctl kube agent node install --name {name} --worker {w}")
            }
            None => "its `install` command".into(),
        },
    }
}

fn first_string_after(text: &str, key: &str) -> Option<String> {
    let rest = &text[text.find(&format!("<key>{key}</key>"))? + key.len() + 11..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("<array>").unwrap_or(rest).trim_start();
    let rest = rest.strip_prefix("<string>")?;
    Some(unescape(&rest[..rest.find("</string>")?]))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `(program, log)` from a plist's text: the first `ProgramArguments` entry and `StandardErrorPath` (else `StandardOutPath`).
pub fn parse_plist(text: &str) -> (Option<String>, Option<String>) {
    (
        first_string_after(text, "ProgramArguments"),
        first_string_after(text, "StandardErrorPath")
            .or_else(|| first_string_after(text, "StandardOutPath")),
    )
}

/// `state`, `pid` and `last exit code` from `launchctl print`.
pub fn parse_print(text: &str) -> (Option<String>, Option<u32>, Option<i32>) {
    let field = |key: &str| {
        text.lines().find_map(|l| {
            l.trim()
                .strip_prefix(key)?
                .trim()
                .strip_prefix('=')
                .map(|v| v.trim().to_owned())
        })
    };
    // `0`, `78: EX_CONFIG` or `(never exited)`: the number, when there is one.
    let code = field("last exit code").and_then(|v| {
        let digits: String = v
            .chars()
            .enumerate()
            .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
            .map(|(_, c)| c)
            .collect();
        digits.parse().ok()
    });
    (
        field("state"),
        field("pid").and_then(|p| p.parse().ok()),
        code,
    )
}

/// Health and what to do, from what launchd and the file system say.
pub fn classify(
    label: &str,
    loaded: bool,
    pid: Option<u32>,
    last_exit: Option<i32>,
    program_exists: bool,
    log: Option<&Path>,
) -> (Health, Option<String>) {
    if !program_exists {
        return (
            Health::StaleProgram,
            Some(format!(
                "The program it runs is gone (the install moved or was removed). Run `{}` to point it at the current one.",
                install_command(label)
            )),
        );
    }
    if !loaded {
        return (
            Health::NotLoaded,
            Some(
                "It is installed but not loaded, so it never starts. Run `fluxctl agents repair`."
                    .into(),
            ),
        );
    }
    if pid.is_none() && last_exit.is_some_and(|c| c != 0) {
        let log = log
            .map(|l| format!(" Its log is {}.", l.display()))
            .unwrap_or_default();
        return (
            Health::Crashing,
            Some(format!(
                "It is loaded but not running, and its last run failed (exit {}).{log} `fluxctl agents restart` tries it again; if it fails again, read the log.",
                last_exit.unwrap_or(0)
            )),
        );
    }
    (Health::Ok, None)
}

/// Every installed FluxVM LaunchAgent under `home`, worst first. Empty when there is none (or off macOS).
pub fn list(home: &Path, lc: &dyn Launchctl) -> Report {
    let dir = home.join("Library/LaunchAgents");
    let mut agents: Vec<AgentInfo> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let label = name.strip_suffix(".plist")?.to_owned();
            label
                .starts_with(PREFIX)
                .then(|| inspect(&label, &e.path(), lc))
        })
        .collect();
    agents.sort_by(|a, b| {
        (a.health == Health::Ok, &a.label).cmp(&(b.health == Health::Ok, &b.label))
    });
    let bad = agents.iter().filter(|a| a.health != Health::Ok).count();
    let headline = match (agents.len(), bad) {
        (0, _) => "No FluxVM background services are installed on this Mac.".to_owned(),
        (n, 0) => format!("All {n} background services are loaded and healthy."),
        (n, b) => format!("{b} of {n} background services need attention."),
    };
    Report {
        schema: SCHEMA,
        agents,
        headline,
    }
}

fn inspect(label: &str, plist: &Path, lc: &dyn Launchctl) -> AgentInfo {
    let text = std::fs::read_to_string(plist).unwrap_or_default();
    let (program, log) = parse_plist(&text);
    let printed = lc.print(label);
    let (state, pid, last_exit_code) = printed
        .as_deref()
        .map(parse_print)
        .unwrap_or((None, None, None));
    let program_exists = program.as_deref().is_none_or(|p| Path::new(p).exists());
    let log = log.map(PathBuf::from);
    let (health, what_to_do) = classify(
        label,
        printed.is_some(),
        pid,
        last_exit_code,
        program_exists,
        log.as_deref(),
    );
    AgentInfo {
        label: label.to_owned(),
        name: describe(label),
        loaded: printed.is_some(),
        state,
        pid,
        last_exit_code,
        program,
        program_exists,
        plist: plist.to_owned(),
        log,
        health,
        what_to_do,
    }
}

/// The agents `repair` would load: installed, program present, not loaded. Nothing else is touched.
pub fn plan_repairs(agents: &[AgentInfo]) -> Vec<&AgentInfo> {
    agents
        .iter()
        .filter(|a| a.health == Health::NotLoaded)
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepairResult {
    pub label: String,
    pub ok: bool,
    /// Why it failed, or `"would load"` for a dry run.
    pub detail: String,
}

/// Loads every installed-but-not-loaded agent (`dry_run` only says which).
pub fn repair(home: &Path, lc: &dyn Launchctl, dry_run: bool) -> Vec<RepairResult> {
    let report = list(home, lc);
    plan_repairs(&report.agents)
        .into_iter()
        .map(|a| {
            if dry_run {
                return RepairResult {
                    label: a.label.clone(),
                    ok: true,
                    detail: "would load".into(),
                };
            }
            match lc.bootstrap(&a.plist) {
                Ok(()) => RepairResult {
                    label: a.label.clone(),
                    ok: true,
                    detail: "loaded".into(),
                },
                Err(e) => RepairResult {
                    label: a.label.clone(),
                    ok: false,
                    detail: e,
                },
            }
        })
        .collect()
}

/// Which installed agent `query` means: the full label, or what follows `dev.zyvor.fluxvm.` (`kube`, `docker-engine`,
/// `vmnetd`), or `daemon`, `docker`, `node-agent`, `vmnet`, `usb`. Errors list the candidates.
pub fn resolve(query: &str, labels: &[String]) -> Result<String, String> {
    let alias = match query {
        "daemon" => "dev.zyvor.fluxvm",
        "docker" => "dev.zyvor.fluxvm.docker-engine",
        "node-agent" => "dev.zyvor.fluxvm-agent",
        "vmnet" => "dev.zyvor.fluxvm.vmnetd",
        "usb" => "dev.zyvor.fluxvm.usbd",
        q => q,
    };
    let dotted = format!("{PREFIX}.{alias}");
    let dashed = format!("{PREFIX}-{alias}");
    let hits: Vec<&String> = labels
        .iter()
        .filter(|l| l.as_str() == alias || **l == dotted || **l == dashed)
        .collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => Err(format!(
            "no installed FluxVM service matches {query:?}; installed: {}",
            if labels.is_empty() {
                "none".into()
            } else {
                labels.join(", ")
            }
        )),
        _ => Err(format!("{query:?} matches more than one service")),
    }
}

/// Restarts one agent by label or short name.
pub fn restart(home: &Path, lc: &dyn Launchctl, query: &str) -> Result<String, String> {
    let report = list(home, lc);
    let labels: Vec<String> = report.agents.iter().map(|a| a.label.clone()).collect();
    let label = resolve(query, &labels)?;
    let a = report
        .agents
        .iter()
        .find(|a| a.label == label)
        .ok_or("not installed")?;
    if !a.loaded {
        lc.bootstrap(&a.plist)?;
    } else {
        lc.kickstart(&label)?;
    }
    Ok(label)
}

/// The last `lines` lines of a text file, reading at most the last 256 KiB so a huge log costs the same as a small one.
pub fn tail(path: &Path, lines: usize) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 256 * 1024;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(WINDOW);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(WINDOW).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    // A window that starts mid-file starts mid-line: drop that partial first line.
    let text = if start > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let all: Vec<&str> = text.lines().collect();
    Ok(all[all.len().saturating_sub(lines)..].join("\n"))
}

/// The tail of one agent's log, by label or short name.
pub fn log_tail(
    home: &Path,
    lc: &dyn Launchctl,
    query: &str,
    lines: usize,
) -> Result<String, String> {
    let report = list(home, lc);
    let labels: Vec<String> = report.agents.iter().map(|a| a.label.clone()).collect();
    let label = resolve(query, &labels)?;
    let a = report
        .agents
        .iter()
        .find(|a| a.label == label)
        .ok_or("not installed")?;
    let path = a
        .log
        .as_ref()
        .ok_or_else(|| format!("{label} has no log file in its plist"))?;
    match tail(path, lines.clamp(1, 1000)) {
        Ok(t) if t.is_empty() => Ok(format!("(the log {} is empty)", path.display())),
        Ok(t) => Ok(t),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        printed: HashMap<String, String>,
        calls: RefCell<Vec<String>>,
    }
    impl Launchctl for Fake {
        fn print(&self, label: &str) -> Option<String> {
            self.printed.get(label).cloned()
        }
        fn bootstrap(&self, plist: &Path) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(format!("bootstrap {}", plist.display()));
            Ok(())
        }
        fn kickstart(&self, label: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("kickstart {label}"));
            Ok(())
        }
    }

    fn home_with(plists: &[(&str, &str)]) -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let la = d.path().join("Library/LaunchAgents");
        std::fs::create_dir_all(&la).unwrap();
        for (label, prog) in plists {
            std::fs::write(
                la.join(format!("{label}.plist")),
                format!(
                    "<dict><key>Label</key><string>{label}</string><key>ProgramArguments</key>\n  <array>\n    <string>{prog}</string>\n  </array><key>StandardErrorPath</key><string>/tmp/{label}.log</string></dict>"
                ),
            )
            .unwrap();
        }
        d
    }

    const RUNNING: &str = "x = {\n\tstate = running\n\tpid = 42\n\tlast exit code = 0\n}";
    const DEAD: &str = "x = {\n\tstate = not running\n\tlast exit code = 78: EX_CONFIG\n}";

    #[test]
    fn plist_program_and_log_are_read_across_whitespace_and_escapes() {
        let t = "<key>ProgramArguments</key>\n <array>\n  <string>/a &amp; b/fluxctl</string>\n</array><key>StandardOutPath</key><string>/l.log</string>";
        assert_eq!(
            parse_plist(t),
            (Some("/a & b/fluxctl".into()), Some("/l.log".into()))
        );
        assert_eq!(parse_plist("<dict/>"), (None, None));
    }

    #[test]
    fn launchctl_print_is_parsed_including_a_named_exit_code() {
        assert_eq!(
            parse_print(RUNNING),
            (Some("running".into()), Some(42), Some(0))
        );
        assert_eq!(
            parse_print(DEAD),
            (Some("not running".into()), None, Some(78))
        );
    }

    #[test]
    fn health_is_decided_in_priority_order() {
        let l = Some(Path::new("/l.log"));
        assert_eq!(classify("a", true, Some(1), Some(0), true, l).0, Health::Ok);
        assert_eq!(
            classify("a", false, None, None, true, l).0,
            Health::NotLoaded
        );
        assert_eq!(
            classify("a", true, None, Some(78), true, l).0,
            Health::Crashing
        );
        // Idle between runs with a clean exit is fine; a nonzero exit while running is the next restart's business.
        assert_eq!(classify("a", true, None, Some(0), true, l).0, Health::Ok);
        assert_eq!(classify("a", true, Some(9), Some(1), true, l).0, Health::Ok);
        // A missing program outranks everything, and the hint names the install command.
        let (h, w) = classify("dev.zyvor.fluxvm.kube", false, None, None, false, l);
        assert_eq!(h, Health::StaleProgram);
        assert!(w.unwrap().contains("fluxctl kube agent install"));
    }

    #[test]
    fn a_list_names_each_agent_and_puts_the_unhealthy_first() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let h = home_with(&[
            ("dev.zyvor.fluxvm", &exe),
            ("dev.zyvor.fluxvm.kube", &exe),
            ("dev.zyvor.fluxvm.vmnetd", "/no/such/program"),
            ("com.other.thing", &exe),
        ]);
        let mut fake = Fake::default();
        fake.printed
            .insert("dev.zyvor.fluxvm".into(), RUNNING.into());
        let r = list(h.path(), &fake);
        assert_eq!(r.agents.len(), 3, "only dev.zyvor.fluxvm* is listed");
        assert_ne!(
            r.agents[0].health,
            Health::Ok,
            "unhealthy agents come first"
        );
        assert_eq!(r.headline, "2 of 3 background services need attention.");
        let daemon = r
            .agents
            .iter()
            .find(|a| a.label == "dev.zyvor.fluxvm")
            .unwrap();
        assert_eq!(
            (daemon.health, daemon.pid, daemon.name.as_str()),
            (Health::Ok, Some(42), "FluxVM daemon")
        );
    }

    #[test]
    fn repair_loads_only_installed_but_not_loaded_agents_and_dry_run_loads_nothing() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let h = home_with(&[
            ("dev.zyvor.fluxvm", &exe),
            ("dev.zyvor.fluxvm.kube", &exe),
            ("dev.zyvor.fluxvm.usbd", "/gone"),
        ]);
        let mut fake = Fake::default();
        fake.printed
            .insert("dev.zyvor.fluxvm".into(), RUNNING.into());
        let dry = repair(h.path(), &fake, true);
        assert_eq!(dry.len(), 1);
        assert_eq!(
            (dry[0].label.as_str(), dry[0].detail.as_str()),
            ("dev.zyvor.fluxvm.kube", "would load")
        );
        assert!(fake.calls.borrow().is_empty());
        let done = repair(h.path(), &fake, false);
        assert!(done[0].ok);
        assert_eq!(fake.calls.borrow().len(), 1);
        assert!(
            fake.calls.borrow()[0].starts_with("bootstrap ")
                && fake.calls.borrow()[0].ends_with("dev.zyvor.fluxvm.kube.plist")
        );
    }

    #[test]
    fn short_names_resolve_and_unknown_ones_list_what_is_installed() {
        let labels = vec![
            "dev.zyvor.fluxvm".to_owned(),
            "dev.zyvor.fluxvm.kube".to_owned(),
            "dev.zyvor.fluxvm.docker-engine".to_owned(),
        ];
        assert_eq!(resolve("daemon", &labels).unwrap(), "dev.zyvor.fluxvm");
        assert_eq!(resolve("kube", &labels).unwrap(), "dev.zyvor.fluxvm.kube");
        assert_eq!(
            resolve("docker", &labels).unwrap(),
            "dev.zyvor.fluxvm.docker-engine"
        );
        assert_eq!(
            resolve("dev.zyvor.fluxvm.kube", &labels).unwrap(),
            "dev.zyvor.fluxvm.kube"
        );
        let more = vec!["dev.zyvor.fluxvm-central".to_owned()];
        assert_eq!(
            resolve("central", &more).unwrap(),
            "dev.zyvor.fluxvm-central"
        );
        assert_eq!(
            describe("dev.zyvor.fluxvm-central"),
            "FluxVM central service"
        );
        let e = resolve("usb", &labels).unwrap_err();
        assert!(e.contains("installed: dev.zyvor.fluxvm,"), "{e}");
    }

    #[test]
    fn restart_kickstarts_a_loaded_agent_and_bootstraps_an_unloaded_one() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let h = home_with(&[("dev.zyvor.fluxvm", &exe), ("dev.zyvor.fluxvm.kube", &exe)]);
        let mut fake = Fake::default();
        fake.printed
            .insert("dev.zyvor.fluxvm".into(), RUNNING.into());
        assert_eq!(
            restart(h.path(), &fake, "daemon").unwrap(),
            "dev.zyvor.fluxvm"
        );
        assert_eq!(fake.calls.borrow()[0], "kickstart dev.zyvor.fluxvm");
        restart(h.path(), &fake, "kube").unwrap();
        assert!(fake.calls.borrow()[1].starts_with("bootstrap "));
        assert!(restart(h.path(), &fake, "nope").is_err());
    }

    #[test]
    fn worker_keepers_are_described_and_have_an_install_command() {
        assert_eq!(
            describe("dev.zyvor.fluxvm.kube.dev-w2"),
            "Keeps Kubernetes worker dev-w2 running"
        );
        assert_eq!(
            install_command("dev.zyvor.fluxvm.kube.dev-w2"),
            "fluxctl kube agent node install --name dev --worker 2"
        );
    }

    #[test]
    fn a_log_tail_returns_the_last_lines_and_never_reads_the_whole_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.log");
        std::fs::write(&f, "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(tail(&f, 2).unwrap(), "three\nfour");
        assert_eq!(tail(&f, 99).unwrap(), "one\ntwo\nthree\nfour");
        // Bigger than the window: only the end is read, and the cut first line is dropped.
        let big: String = (0..100_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, &big).unwrap();
        let t = tail(&f, 3).unwrap();
        assert_eq!(t, "line 99997\nline 99998\nline 99999");
        assert!(tail(&d.path().join("missing"), 1).is_err());
    }

    #[test]
    fn an_agents_log_is_found_through_its_plist() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let h = home_with(&[("dev.zyvor.fluxvm.kube", &exe)]);
        let fake = Fake::default();
        // home_with points StandardErrorPath at /tmp/<label>.log, which does not exist here.
        let e = log_tail(h.path(), &fake, "kube", 5).unwrap_err();
        assert!(
            e.contains("cannot read /tmp/dev.zyvor.fluxvm.kube.log"),
            "{e}"
        );
        assert!(log_tail(h.path(), &fake, "nope", 5).is_err());
    }
}
