// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl cluster status` and `fluxctl fix`: the plain-English view of the cluster and the one-step repair.
//!
//! The judging is `fluxvm_core::health` (pure, shared with the daemon's `GET /v1/cluster/health` and the Studio Home
//! page). This module gathers facts (this machine; and the Macs of a fleet inventory over ssh, read-only), prints the
//! result, and applies the few fixes that are safe: starting a stopped service, creating a missing directory, pulling a
//! missing image. Anything else is printed as the exact command to run. Nothing here runs sudo unless `--sudo` is given,
//! and nothing changes a security setting, ever.

use crate::mac_fleet::{Conn, par_map};
use crate::mac_fleet_inv::Inventory;
use crate::mac_fleet_remote::{self as rem, Facts, Want};
use crate::mac_fleet_sched::{HostView, SchedFacts, parse_sched_facts, sched_probe_script};
use crate::mac_fleet_ssh::{Target, Transport, explain_ssh_failure};
use anyhow::{Context, Result, bail};
use fluxvm_core::health::{
    self, ClusterHealth, Fix, FixAction, Item, NodeFacts, Platform, RawCheck, Severity,
};
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

#[derive(clap::Args, Clone)]
pub struct StatusArgs {
    /// Also check the Macs of this fleet inventory (YAML or JSON) over ssh, read-only.
    #[arg(long, short = 'f', value_name = "FILE")]
    pub inventory: Option<PathBuf>,
    /// Do not check this machine (only the inventory).
    #[arg(long)]
    pub no_local: bool,
    /// Print every item with its cause and fix, not just the headline of each machine.
    #[arg(long, short = 'v')]
    pub verbose: bool,
    #[command(flatten)]
    pub conn: Conn,
}

#[derive(clap::Args, Clone)]
pub struct FixArgs {
    /// Apply the safe fixes without asking.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Show what would be done and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Also run the few fixes that need sudo and are additive (start the service, load a kernel module, enable IP
    /// forwarding). You are asked for your password by sudo itself. Without this flag sudo is never run.
    #[arg(long)]
    pub sudo: bool,
    /// Also look at the Macs of this fleet inventory. Their problems are printed with the command to run on each Mac;
    /// fixes are only applied to this machine.
    #[arg(long, short = 'f', value_name = "FILE")]
    pub inventory: Option<PathBuf>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub conn: ConnLite,
}

/// The ssh options `fix` needs when it looks at a fleet (the full `Conn` has `--json` and `--dry-run` of its own).
#[derive(clap::Args, Clone, Default)]
pub struct ConnLite {
    /// ssh private key for Macs that do not set their own.
    #[arg(long, short = 'i', value_name = "FILE")]
    pub identity: Option<PathBuf>,
    /// Trust a host key the first time it is seen.
    #[arg(long)]
    pub accept_new_hostkey: bool,
}

/// What this machine's daemon and state look like, from the global flags and the config.
pub struct Env {
    pub listen: String,
    pub state_dir: PathBuf,
    pub remote: crate::remote::Remote,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- adapters: the existing doctors into the model --------------------------------------------------------------------

fn sev_from_vz(s: crate::vz_doctor::Status) -> Severity {
    match s {
        crate::vz_doctor::Status::Ok => Severity::Ok,
        crate::vz_doctor::Status::Warn => Severity::Warn,
        crate::vz_doctor::Status::Fail => Severity::Fail,
    }
}

pub fn from_doctor(checks: &[crate::doctor::Check]) -> Vec<RawCheck> {
    use crate::doctor::Level;
    checks
        .iter()
        .map(|c| RawCheck {
            source: "doctor".into(),
            name: c.name.into(),
            severity: match c.level {
                Level::Ok | Level::Skip => Severity::Ok,
                Level::Warn => Severity::Warn,
                Level::Fail => Severity::Fail,
            },
            detail: c.detail.clone(),
            fix: c.fix.clone(),
            path: None,
        })
        .collect()
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn from_vz(source: &str, checks: &[crate::vz_doctor::Check]) -> Vec<RawCheck> {
    checks
        .iter()
        .map(|c| RawCheck {
            source: source.into(),
            name: c.name.into(),
            severity: sev_from_vz(c.status),
            detail: c.detail.clone(),
            fix: c.fix.clone(),
            path: None,
        })
        .collect()
}

pub fn from_findings(findings: &[rem::Finding]) -> Vec<RawCheck> {
    findings
        .iter()
        .map(|f| RawCheck {
            source: "fleet".into(),
            name: f.id.into(),
            severity: match f.level {
                rem::Level::Ok => Severity::Ok,
                rem::Level::Warn => Severity::Warn,
                rem::Level::Error => Severity::Fail,
            },
            detail: f.msg.clone(),
            fix: (!f.fix.is_empty()).then(|| f.fix.clone()),
            path: None,
        })
        .collect()
}

/// One Mac of a fleet, from what its probe printed. Pure: the ssh call is made by the caller.
pub fn fleet_facts(
    target: &Target,
    facts: &Facts,
    sf: &SchedFacts,
    wanted: Option<&str>,
) -> NodeFacts {
    let mut n = NodeFacts {
        name: target.name.clone(),
        platform: Platform::Mac,
        reachable: true,
        wanted_version: wanted.map(str::to_owned),
        ..Default::default()
    };
    if !facts.known {
        n.reachable = false;
        n.unreachable_reason = Some("the probe returned nothing".into());
        return n;
    }
    n.installed = Some(facts.fluxctl_version.is_some());
    n.version = facts.fluxctl_version.clone();
    n.daemon_up = facts.fluxctl_version.as_ref().map(|_| facts.daemon_healthy);
    n.disk_free_gib = facts.disk_free_gib;
    n.mem_total_mib = sf.memsize_mib;
    n.checks = from_findings(&rem::check(
        facts,
        Want {
            enroll: false,
            k3s: target.k3s,
        },
    ));
    if let Some(vms) = &facts.vms_json {
        health::apply_vm_list(&mut n, vms, sf.ncpu);
        let view = HostView::from_facts(target, facts, sf);
        if view.unavailable.is_none() && sf.memsize_mib.is_some() {
            n.mem_free_mib = Some(view.caps.free_memory_mib);
        }
    }
    if target.k3s {
        n.k3s = Some(facts.k3s_state());
    }
    n
}

/// Reach one Mac and read its facts. A Mac that cannot be reached is a finding, not an error.
pub fn probe_fleet_host(t: &dyn Transport, target: &Target, wanted: Option<&str>) -> NodeFacts {
    let script = format!("{}{}", rem::probe_script(), sched_probe_script());
    let fail = |why: String| NodeFacts {
        name: target.name.clone(),
        platform: Platform::Mac,
        reachable: false,
        unreachable_reason: Some(why),
        ..Default::default()
    };
    match t.run(target, &rem::remote(&script), None) {
        Err(e) => fail(format!("{e:#}")),
        Ok(o) if o.ssh_failed() => fail(explain_ssh_failure(target, &o.stderr)),
        Ok(o) if !o.ok() => fail(format!(
            "the probe failed: {}",
            o.stderr.lines().last().unwrap_or("no output")
        )),
        Ok(o) => fleet_facts(
            target,
            &rem::parse_facts(&o.stdout),
            &parse_sched_facts(&o.stdout),
            wanted,
        ),
    }
}

// ---- gathering --------------------------------------------------------------------------------------------------------

/// This machine: the host checks, then what the daemon says (when it answers).
pub async fn gather_local(env: &Env) -> NodeFacts {
    let mut f = NodeFacts {
        name: health::hostname(),
        platform: if cfg!(target_os = "macos") {
            Platform::Mac
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Unknown
        },
        local: true,
        reachable: true,
        installed: Some(true),
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        mem_total_mib: None,
        disk_free_gib: health::free_gib(&env.state_dir),
        backups_known: true,
        last_backup_epoch: health::newest_mtime(&env.state_dir.join("backups")),
        ..Default::default()
    };
    let doctor = crate::doctor::gather(&env.listen, &env.state_dir, false).await;
    let daemon_ok = !doctor
        .iter()
        .any(|c| c.name == "daemon" && c.level == crate::doctor::Level::Fail);
    f.daemon_up = Some(daemon_ok);
    f.checks.extend(from_doctor(&doctor));
    #[cfg(target_os = "macos")]
    {
        use crate::mac_doctor as md;
        let store = crate::mac_store::default_root().unwrap_or_default();
        let facts = md::gather(
            &store,
            Some(md::DaemonFacts {
                reachable: daemon_ok,
                ..Default::default()
            }),
        );
        let mut raws = from_vz("vz", &md::evaluate(&facts));
        // The store directory is the one thing here that `fix` may create.
        for r in raws
            .iter_mut()
            .filter(|r| r.name.contains("store location") && r.severity != Severity::Ok)
        {
            if !store.exists() {
                r.path = Some(store.display().to_string());
            }
        }
        f.checks.extend(raws);
        let hf = crate::mac_host::collect(
            &crate::mac_host::RealSh,
            crate::mac_host::uid(),
            &crate::mac_host::user(),
            crate::mac_host::keepawake_loaded(),
        );
        f.checks
            .extend(from_vz("host", &crate::mac_host::headless_checks(&hf)));
    }
    if daemon_ok {
        if let Ok(vms) = env.remote.call(reqwest::Method::GET, "/v1/vms", None).await {
            let cpus = std::thread::available_parallelism()
                .ok()
                .and_then(|n| u32::try_from(n.get()).ok());
            health::apply_vm_list(&mut f, &vms, cpus);
        }
        if let Ok(m) = env
            .remote
            .call(reqwest::Method::GET, "/v1/host/memory", None)
            .await
        {
            f.mem_total_mib = m["mem_total_mib"].as_u64();
            f.mem_free_mib = m["mem_available_mib"].as_u64();
        }
    }
    f
}

/// Every machine to report on: this one (unless `no_local`) and the Macs of the inventory.
pub async fn gather(
    env: &Env,
    inventory: Option<&Path>,
    no_local: bool,
    conn: &Conn,
) -> Result<Vec<NodeFacts>> {
    let mut nodes = vec![];
    if !no_local {
        nodes.push(gather_local(env).await);
    }
    if let Some(path) = inventory {
        let inv = Inventory::load(path)?;
        let mut targets = inv.targets()?;
        for t in &mut targets {
            if t.identity.is_none() {
                t.identity = conn.identity.clone();
            }
        }
        let tr = crate::mac_fleet::transport(conn, Some(&inv));
        let wanted = inv.version.clone();
        let jobs = conn.jobs.max(1);
        let tr_ref: &dyn Transport = tr.as_ref();
        nodes.extend(par_map(&targets, jobs, |t| {
            probe_fleet_host(tr_ref, t, wanted.as_deref())
        }));
    }
    Ok(nodes)
}

/// Gateway API problems of the FluxVM clusters this user has a kubeconfig for (`~/.kube/fluxvm-NAME.yaml`, written by
/// `fluxctl kube up`). Reads only, with that kubeconfig and never the default one; a cluster that cannot be reached, or
/// has no Gateway API, adds nothing. Blocking (kubectl), so call it from `spawn_blocking`.
pub fn gateway_items_for_known_clusters(home: &Path) -> Vec<Item> {
    use crate::gateway::kube::{Cluster, Selection, SystemKubectl};
    let mut names: Vec<String> = std::fs::read_dir(home.join(".kube"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let f = e.file_name().to_string_lossy().into_owned();
            f.strip_prefix("fluxvm-")?
                .strip_suffix(".yaml")
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    let mut items = vec![];
    for name in names.into_iter().take(5) {
        let sel = Selection {
            cluster: Some(name.clone()),
            kube_context: None,
        };
        let Ok(prefix) = sel.prefix(Some(home)) else {
            continue;
        };
        if let Some(facts) = Cluster::new(prefix, &SystemKubectl).facts(&name) {
            items.extend(health::gateway_items(&facts));
        }
    }
    items
}

async fn gateway_items_now() -> Vec<Item> {
    let Ok(home) = crate::docker_host::home() else {
        return vec![];
    };
    tokio::task::spawn_blocking(move || gateway_items_for_known_clusters(&home))
        .await
        .unwrap_or_default()
}

// ---- rendering --------------------------------------------------------------------------------------------------------

fn color_on() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn light(s: Severity, color: bool) -> String {
    let (word, code) = match s {
        Severity::Ok => ("green", "32"),
        Severity::Warn => ("yellow", "33"),
        Severity::Fail => ("red", "31"),
    };
    if color {
        format!("\x1b[{code}m● {word}\x1b[0m")
    } else {
        format!("({word})")
    }
}

fn fix_line(f: &Fix, machine: &str) -> String {
    let mut how = match (&f.command, &f.steps) {
        (Some(c), _) => format!("`{c}`"),
        (None, Some(s)) => s.clone(),
        _ => "see `fluxctl explain`".into(),
    };
    let mut flags = vec![];
    if f.safe {
        flags.push("safe: `fluxctl fix` does this".to_owned());
    }
    if f.needs_sudo {
        flags.push("needs sudo".into());
    }
    if f.needs_restart {
        flags.push("needs a restart".into());
    }
    if f.needs_login {
        flags.push("needs someone logged in".into());
    }
    if f.changes_security {
        flags.push("a security setting: never changed automatically".into());
    }
    if !flags.is_empty() {
        how.push_str(&format!("  [{}]", flags.join("; ")));
    }
    let _ = machine;
    how
}

pub fn render_status(h: &ClusterHealth, verbose: bool, color: bool) -> String {
    let mut o = String::new();
    o.push_str(&format!("{}  {}\n", light(h.light, color), h.headline));
    o.push_str(&format!("   {}\n\n", h.capacity.summary));
    for i in &h.items {
        o.push_str(&render_item(i, color));
    }
    for m in &h.machines {
        let tag = if m.local { " (this machine)" } else { "" };
        o.push_str(&format!(
            "{} {}{tag}: {}\n",
            light(m.light, color),
            m.name,
            m.headline
        ));
        o.push_str(&format!("      {}\n", m.capacity.summary));
        let mut extra = vec![format!("last backup: {}", m.last_backup)];
        if let Some(v) = &m.version {
            extra.push(match &m.update {
                Some(u) => format!(
                    "FluxVM {v} (update to {} available: `fluxctl mac-fleet upgrade`)",
                    u.wanted
                ),
                None => format!("FluxVM {v}"),
            });
        }
        o.push_str(&format!("      {}\n", extra.join(" · ")));
        if m.light == Severity::Ok && !verbose {
            o.push_str(&format!("      {} checks passed\n", m.checks_passed));
        }
        for i in &m.items {
            o.push_str(&render_item(i, color));
        }
        o.push('\n');
    }
    o.push_str(&format!("Next: {}\n", h.next_action.text));
    o
}

fn render_item(i: &Item, color: bool) -> String {
    let mut o = format!("   {} {}\n", light(i.severity, color), i.message);
    if let Some(c) = &i.cause {
        o.push_str(&format!("        why: {c}\n"));
    }
    if let Some(f) = &i.fix {
        o.push_str(&format!("        fix: {}\n", fix_line(f, &i.machine)));
    }
    if let Some(e) = &i.explain {
        o.push_str(&format!("        more: `fluxctl explain {e}`\n"));
    }
    o
}

// ---- fix --------------------------------------------------------------------------------------------------------------

/// What `fix` decided to do with one item.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// A safe fix on this machine: will be applied.
    Apply,
    /// A sudo fix from the short allow-list, on this machine: applied only with `--sudo`.
    Sudo,
    /// Something to do by hand (or on another machine); the command is printed.
    Manual,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Planned {
    pub item: String,
    pub machine: String,
    pub message: String,
    pub disposition: Disposition,
    pub command: Option<String>,
    pub steps: Option<String>,
    #[serde(skip)]
    pub action: Option<FixAction>,
    pub why_manual: Option<String>,
}

/// The only sudo commands `--sudo` will run: additive and easy to undo.
const SUDO_ALLOW: [&str; 3] = ["sudo systemctl start ", "sudo modprobe ", "sudo sysctl -w "];

pub fn sudo_allowed(f: &Fix) -> bool {
    f.needs_sudo
        && !f.changes_security
        && !f.needs_login
        && !f.safe
        && f.command.as_deref().is_some_and(|c| {
            SUDO_ALLOW.iter().any(|p| c.starts_with(p))
                && !c.contains(['&', ';', '|', '`', '$', '>', '<', '\n'])
        })
}

pub fn plan(h: &ClusterHealth) -> Vec<Planned> {
    let mut out = vec![];
    for i in h.all_items() {
        let Some(fix) = &i.fix else { continue };
        let local = h.machines.iter().any(|m| m.name == i.machine && m.local);
        let (disposition, why) = if fix.safe && local {
            (Disposition::Apply, None)
        } else if fix.safe {
            (
                Disposition::Manual,
                Some(format!(
                    "it is on {}, not on this machine: run it there",
                    i.machine
                )),
            )
        } else if sudo_allowed(fix) && local {
            (Disposition::Sudo, None)
        } else if fix.changes_security {
            (
                Disposition::Manual,
                Some("a security setting: FluxVM never changes it".into()),
            )
        } else if fix.needs_sudo {
            (
                Disposition::Manual,
                Some("needs sudo and is not on the short list `--sudo` may run".into()),
            )
        } else if fix.needs_login {
            (Disposition::Manual, Some("needs a person to log in".into()))
        } else {
            (
                Disposition::Manual,
                Some("FluxVM cannot be sure this is safe to do by itself".into()),
            )
        };
        out.push(Planned {
            item: i.id.clone(),
            machine: i.machine.clone(),
            message: i.message.clone(),
            disposition,
            command: fix.command.clone(),
            steps: fix.steps.clone(),
            action: fix.action.clone(),
            why_manual: why,
        });
    }
    out
}

/// What applying a fix needs from the machine. The real one acts; tests record.
pub trait Applier {
    fn start_service(&self) -> Result<String>;
    fn create_dir(&self, path: &Path) -> Result<String>;
    fn pull_image(&self, reference: &str) -> Result<String>;
    fn run_sudo(&self, command: &str) -> Result<String>;
}

pub struct RealApplier;

fn run_cmd(program: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if !out.status.success() {
        bail!("{program} {} failed: {}", args.join(" "), text.trim());
    }
    Ok(text.trim().to_owned())
}

impl Applier for RealApplier {
    fn start_service(&self) -> Result<String> {
        if !cfg!(target_os = "macos") {
            bail!("starting the service by itself is only done for the macOS LaunchAgent");
        }
        let uid = unsafe { libc::getuid() };
        let label = crate::launch_agent::LABEL;
        let target = format!("gui/{uid}/{label}");
        // Already bootstrapped: kickstart. Otherwise load the plist, which exists when `fluxctl service install` ran.
        if run_cmd("launchctl", &["print", &target]).is_ok() {
            run_cmd("launchctl", &["kickstart", "-k", &target])
        } else {
            let home = std::env::var("HOME").context("HOME is not set")?;
            let plist = format!("{home}/Library/LaunchAgents/{label}.plist");
            if !Path::new(&plist).exists() {
                bail!("no LaunchAgent is installed at {plist}: run `fluxctl service install`");
            }
            run_cmd("launchctl", &["bootstrap", &format!("gui/{uid}"), &plist])
        }
    }
    fn create_dir(&self, path: &Path) -> Result<String> {
        std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
        Ok(format!("created {}", path.display()))
    }
    fn pull_image(&self, reference: &str) -> Result<String> {
        let exe = crate::stable_exe::stable_exe().context("locating fluxctl")?;
        let out = std::process::Command::new(exe)
            .args(["mac", "pull", reference])
            .output()?;
        if !out.status.success() {
            bail!(
                "pull failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(format!("pulled {reference}"))
    }
    fn run_sudo(&self, command: &str) -> Result<String> {
        // Terminal attached so sudo can ask for the password itself.
        let st = std::process::Command::new("sh")
            .args(["-c", command])
            .status()?;
        if !st.success() {
            bail!("`{command}` exited with {st}");
        }
        Ok(format!("ran `{command}`"))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Applied {
    pub ts: u64,
    pub machine: String,
    pub item: String,
    pub what: String,
    pub ok: bool,
    pub output: String,
}

pub fn apply(plan: &[Planned], allow_sudo: bool, a: &dyn Applier) -> Vec<Applied> {
    let mut out = vec![];
    for p in plan {
        let (what, res) = match (&p.disposition, &p.action) {
            (Disposition::Apply, Some(FixAction::StartService)) => {
                ("start the FluxVM service".to_owned(), a.start_service())
            }
            (Disposition::Apply, Some(FixAction::CreateDir { path })) => (
                format!("create directory {path}"),
                a.create_dir(Path::new(path)),
            ),
            (Disposition::Apply, Some(FixAction::PullImage { reference })) => {
                (format!("pull image {reference}"), a.pull_image(reference))
            }
            (Disposition::Sudo, _) if allow_sudo => {
                let c = p.command.clone().unwrap_or_default();
                (format!("run `{c}`"), a.run_sudo(&c))
            }
            _ => continue,
        };
        out.push(Applied {
            ts: now(),
            machine: p.machine.clone(),
            item: p.item.clone(),
            what,
            ok: res.is_ok(),
            output: match res {
                Ok(s) => s,
                Err(e) => format!("{e:#}"),
            },
        });
    }
    out
}

/// Where the record of changes is kept: `<state_dir>/health/fix-log.jsonl`, or `~/.config/fluxvm/fix-log.jsonl` when the
/// state directory is not writable (a root-owned daemon).
pub fn journal_path(state_dir: &Path) -> PathBuf {
    let primary = state_dir.join("health");
    if std::fs::create_dir_all(&primary).is_ok()
        && primary
            .metadata()
            .is_ok_and(|m| !m.permissions().readonly())
    {
        let p = primary.join("fix-log.jsonl");
        if std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .is_ok()
        {
            return p;
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".config/fluxvm/fix-log.jsonl")
}

pub fn record(path: &Path, applied: &[Applied]) -> Result<()> {
    if applied.is_empty() {
        return Ok(());
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for a in applied {
        writeln!(f, "{}", serde_json::to_string(a)?)?;
    }
    Ok(())
}

pub fn render_plan(plan: &[Planned], allow_sudo: bool, dry: bool) -> String {
    let mut o = String::new();
    let will: Vec<&Planned> = plan
        .iter()
        .filter(|p| {
            p.disposition == Disposition::Apply
                || (p.disposition == Disposition::Sudo && allow_sudo)
        })
        .collect();
    if will.is_empty() {
        o.push_str("Nothing FluxVM can safely fix by itself.\n");
    } else {
        o.push_str(if dry { "Would do:\n" } else { "Will do:\n" });
        for p in &will {
            o.push_str(&format!("  [{}] {}\n", p.machine, p.message));
            o.push_str(&format!(
                "      {}\n",
                p.command.as_deref().or(p.steps.as_deref()).unwrap_or("")
            ));
        }
    }
    let manual: Vec<&Planned> = plan
        .iter()
        .filter(|p| !will.iter().any(|w| w.item == p.item))
        .collect();
    if !manual.is_empty() {
        o.push_str("\nFor you to do:\n");
        for p in manual {
            let how = match (&p.command, &p.steps) {
                (Some(c), _) => format!("on {}: {c}", p.machine),
                (None, Some(s)) => s.clone(),
                _ => String::new(),
            };
            let note = match (&p.disposition, allow_sudo) {
                (Disposition::Sudo, false) => "run it yourself, or re-run with `--sudo`".to_owned(),
                _ => p.why_manual.clone().unwrap_or_default(),
            };
            o.push_str(&format!("  - {}\n      {how}\n      ({note})\n", p.message));
        }
    }
    o
}

// ---- commands ---------------------------------------------------------------------------------------------------------

pub async fn run_status(a: &StatusArgs, env: &Env) -> Result<i32> {
    if a.conn.dry_run {
        bail!("`cluster status` only reads; there is nothing to dry-run");
    }
    let nodes = gather(env, a.inventory.as_deref(), a.no_local, &a.conn).await?;
    let h = health::assess_with(&nodes, gateway_items_now().await, now());
    if a.conn.json {
        println!("{}", serde_json::to_string_pretty(&h)?);
    } else {
        print!("{}", render_status(&h, a.verbose, color_on()));
    }
    Ok(i32::from(h.light == Severity::Fail))
}

pub async fn run_fix(a: &FixArgs, env: &Env, state_dir: &Path) -> Result<i32> {
    let conn = Conn {
        identity: a.conn.identity.clone(),
        known_hosts: None,
        accept_new_hostkey: a.conn.accept_new_hostkey,
        jobs: 8,
        connect_timeout: 10,
        dry_run: false,
        json: false,
        step_timeout: 1800,
    };
    let nodes = gather(env, a.inventory.as_deref(), false, &conn).await?;
    let h = health::assess_with(&nodes, gateway_items_now().await, now());
    let p = plan(&h);
    let will_change = p.iter().any(|x| {
        x.disposition == Disposition::Apply || (x.disposition == Disposition::Sudo && a.sudo)
    });
    if a.dry_run || !will_change {
        if a.json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"schema": health::SCHEMA, "dry_run": a.dry_run, "plan": p, "applied": []})
                )?
            );
        } else {
            print!("{}", render_plan(&p, a.sudo, a.dry_run));
        }
        return Ok(i32::from(h.light == Severity::Fail));
    }
    if !a.json {
        print!("{}", render_plan(&p, a.sudo, false));
    }
    if !a.yes {
        if !std::io::stdin().is_terminal() {
            bail!("not a terminal: pass --yes to apply the fixes above, or --dry-run to only look");
        }
        eprint!("\nApply these? [y/N] ");
        std::io::stderr().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        if !matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Nothing changed.");
            return Ok(0);
        }
    }
    let applied = apply(&p, a.sudo, &RealApplier);
    let log = journal_path(state_dir);
    if let Err(e) = record(&log, &applied) {
        eprintln!(
            "warning: could not write the change log {}: {e:#}",
            log.display()
        );
    }
    // Look again, so the answer is the state the user is left with.
    let after = health::assess_with(
        &gather(env, a.inventory.as_deref(), false, &conn).await?,
        gateway_items_now().await,
        now(),
    );
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"schema": health::SCHEMA, "dry_run": false, "plan": p, "applied": applied, "after": after, "log": log})
            )?
        );
    } else {
        println!("\nWhat I changed:");
        for x in &applied {
            println!(
                "  {} [{}] {}{}",
                if x.ok { "done  " } else { "FAILED" },
                x.machine,
                x.what,
                if x.ok {
                    String::new()
                } else {
                    format!(": {}", x.output)
                }
            );
        }
        println!("  (recorded in {})\n", log.display());
        print!("{}", render_status(&after, false, color_on()));
    }
    Ok(i32::from(
        after.light == Severity::Fail || applied.iter().any(|x| !x.ok),
    ))
}

/// `--json` documents for scripts: the schema of both commands is `health::SCHEMA`.
#[allow(dead_code)]
pub fn schema_note() -> Value {
    json!({"schema": health::SCHEMA})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_ssh::fake::{FakeTransport, failed, ok};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Rec(Mutex<Vec<String>>);
    impl Applier for Rec {
        fn start_service(&self) -> Result<String> {
            self.0.lock().unwrap().push("start".into());
            Ok("started".into())
        }
        fn create_dir(&self, p: &Path) -> Result<String> {
            self.0
                .lock()
                .unwrap()
                .push(format!("mkdir {}", p.display()));
            Ok("made".into())
        }
        fn pull_image(&self, r: &str) -> Result<String> {
            self.0.lock().unwrap().push(format!("pull {r}"));
            Ok("pulled".into())
        }
        fn run_sudo(&self, c: &str) -> Result<String> {
            self.0.lock().unwrap().push(format!("sudo {c}"));
            Ok("ran".into())
        }
    }

    fn healthy_mac(name: &str, local: bool) -> NodeFacts {
        NodeFacts {
            name: name.into(),
            platform: Platform::Mac,
            local,
            reachable: true,
            installed: Some(true),
            daemon_up: Some(true),
            vms_total: Some(0),
            macos_in_use: Some(0),
            cpus_free: Some(8),
            mem_free_mib: Some(12288),
            mem_total_mib: Some(16384),
            disk_free_gib: Some(200),
            backups_known: true,
            ..Default::default()
        }
    }

    fn broken_cluster() -> ClusterHealth {
        let mut a = healthy_mac("mini-01", true);
        a.daemon_up = Some(false);
        let mut missing = RawCheck {
            source: "vz".into(),
            name: "store location".into(),
            severity: Severity::Fail,
            detail: "missing".into(),
            path: Some("/tmp/fluxvm-store".into()),
            ..Default::default()
        };
        missing.fix = None;
        a.checks.push(missing);
        a.checks.push(RawCheck {
            source: "host".into(),
            name: "FileVault (host)".into(),
            severity: Severity::Warn,
            detail: "on".into(),
            ..Default::default()
        });
        let mut b = healthy_mac("mini-02", false);
        b.daemon_up = Some(false);
        let mut l = NodeFacts {
            name: "box".into(),
            platform: Platform::Linux,
            reachable: true,
            daemon_up: Some(true),
            ..Default::default()
        };
        l.checks.push(RawCheck {
            source: "doctor".into(),
            name: "ip forwarding".into(),
            severity: Severity::Warn,
            detail: "0".into(),
            ..Default::default()
        });
        l.checks.push(RawCheck {
            source: "doctor".into(),
            name: "kvm".into(),
            severity: Severity::Fail,
            detail: "no".into(),
            ..Default::default()
        });
        health::assess(&[a, b, l], 1_800_000_000)
    }

    #[test]
    fn the_plan_separates_safe_sudo_and_manual() {
        let p = plan(&broken_cluster());
        let get = |item: &str| {
            p.iter()
                .find(|x| x.item.contains(item))
                .unwrap_or_else(|| panic!("{item} in {p:?}"))
        };
        assert_eq!(get("mini-01/daemon_down").disposition, Disposition::Apply);
        assert_eq!(get("mini-01/store_problem").disposition, Disposition::Apply);
        // a safe fix on another machine is never applied from here
        let remote = get("mini-02/daemon_down");
        assert_eq!(remote.disposition, Disposition::Manual);
        assert!(remote.why_manual.as_ref().unwrap().contains("mini-02"));
        // security settings are never applied, with or without --sudo
        assert_eq!(
            get("mini-01/security_setting").disposition,
            Disposition::Manual
        );
        // the additive sudo fix is on the list, the group change is not
        assert_eq!(
            get("box/ip_forward").disposition,
            Disposition::Manual,
            "box is not local here"
        );
    }

    #[test]
    fn without_sudo_nothing_with_sudo_runs_and_security_never_does() {
        let mut h = broken_cluster();
        // make the Linux box local so its sudo fixes are in scope
        for m in &mut h.machines {
            m.local = m.name == "box";
        }
        let p = plan(&h);
        let rec = Rec::default();
        let applied = apply(&p, false, &rec);
        assert!(
            applied.is_empty() || applied.iter().all(|a| !a.what.contains("sudo")),
            "{applied:?}"
        );
        assert!(rec.0.lock().unwrap().iter().all(|c| !c.starts_with("sudo")));
        let rec = Rec::default();
        let applied = apply(&p, true, &rec);
        let ran = rec.0.lock().unwrap().clone();
        assert!(
            ran.iter()
                .any(|c| c == "sudo sudo sysctl -w net.ipv4.ip_forward=1"),
            "{ran:?}"
        );
        // kvm needs `usermod` + a login: not on the list
        assert!(!ran.iter().any(|c| c.contains("usermod")));
        assert!(applied.iter().all(|a| a.ok));
    }

    #[test]
    fn only_the_three_safe_actions_ever_run() {
        let h = broken_cluster();
        let p = plan(&h);
        let rec = Rec::default();
        let applied = apply(&p, true, &rec);
        let ran = rec.0.lock().unwrap().clone();
        let mut sorted = ran.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec!["mkdir /tmp/fluxvm-store".to_string(), "start".to_string()],
            "{ran:?}"
        );
        assert_eq!(applied.len(), 2);
        assert!(!ran.iter().any(|c| c.contains("FileVault")));
    }

    #[test]
    fn sudo_allow_list_is_narrow_and_rejects_shell_tricks() {
        let f = |c: &str| Fix {
            command: Some(c.into()),
            needs_sudo: true,
            ..Default::default()
        };
        assert!(sudo_allowed(&f("sudo systemctl start fluxvm")));
        assert!(sudo_allowed(&f("sudo modprobe tun vhost_net")));
        assert!(!sudo_allowed(&f("sudo systemctl start fluxvm; rm -rf /")));
        assert!(!sudo_allowed(&f("sudo rm -rf /")));
        assert!(!sudo_allowed(&f("sudo usermod -aG kvm $USER")));
        let mut sec = f("sudo sysctl -w a=1");
        sec.changes_security = true;
        assert!(!sudo_allowed(&sec));
        let mut login = f("sudo sysctl -w a=1");
        login.needs_login = true;
        assert!(!sudo_allowed(&login));
    }

    #[test]
    fn a_failed_fix_is_reported_not_hidden() {
        struct Bad;
        impl Applier for Bad {
            fn start_service(&self) -> Result<String> {
                bail!("launchctl said no")
            }
            fn create_dir(&self, _: &Path) -> Result<String> {
                bail!("read-only")
            }
            fn pull_image(&self, _: &str) -> Result<String> {
                bail!("offline")
            }
            fn run_sudo(&self, _: &str) -> Result<String> {
                bail!("no")
            }
        }
        let applied = apply(&plan(&broken_cluster()), false, &Bad);
        assert_eq!(applied.len(), 2);
        assert!(applied.iter().all(|a| !a.ok && !a.output.is_empty()));
    }

    #[test]
    fn what_was_changed_is_recorded_as_json_lines() {
        let d = tempfile::tempdir().unwrap();
        let log = journal_path(d.path());
        assert!(log.starts_with(d.path()));
        let applied = apply(&plan(&broken_cluster()), false, &Rec::default());
        record(&log, &applied).unwrap();
        record(&log, &applied).unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert_eq!(text.lines().count(), 4);
        let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["machine"], "mini-01");
        assert_eq!(first["ok"], true);
    }

    #[test]
    fn the_real_applier_creates_a_directory_and_nothing_else_here() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("a/b/c");
        RealApplier.create_dir(&target).unwrap();
        assert!(target.is_dir());
        // creating it again is fine (idempotent)
        RealApplier.create_dir(&target).unwrap();
    }

    #[test]
    fn the_plan_text_lists_what_will_happen_and_what_is_left_to_you() {
        let t = render_plan(&plan(&broken_cluster()), false, true);
        assert!(t.starts_with("Would do:"), "{t}");
        assert!(t.contains("For you to do:"));
        assert!(t.contains("never changes it"));
        assert!(t.contains("mini-02"));
    }

    #[test]
    fn status_text_is_plain_english_with_a_next_step() {
        let t = render_status(&broken_cluster(), false, false);
        assert!(t.starts_with("(red)  "), "{t}");
        assert!(t.contains("(this machine)"));
        assert!(t.contains("Next: "));
        assert!(t.contains("fluxctl explain DAEMON_DOWN"));
        let healthy = health::assess(&[healthy_mac("m", true)], 1);
        let t = render_status(&healthy, false, false);
        assert!(
            t.contains("m is healthy") && t.contains("Next: Nothing to do"),
            "{t}"
        );
    }

    const PROBE_OK: &str = "os=Darwin\narch=arm64\nmacos=15.2\nuser=ci\nconsole_user=ci\ndisk_free_gib=40\nfluxctl_version=0.4.20\ndaemon_loaded=yes\ndaemon_healthy=yes\nncpu=10\nmemsize=17179869184\npower=ac\nvms_json={\"items\":[{\"status\":\"running\",\"request\":{\"vcpus\":4,\"memory_mib\":8192,\"apple\":{\"guest_os\":\"macos\"}}},{\"status\":\"running\",\"request\":{\"vcpus\":2,\"memory_mib\":4096,\"apple\":{\"guest_os\":\"macos\"}}}]}\n";

    #[test]
    fn a_fleet_mac_becomes_facts_with_slots_room_and_an_update() {
        let t = Target::parse("ci@mini-01").unwrap();
        let tr = FakeTransport::new(|_, _| ok(PROBE_OK));
        let n = probe_fleet_host(&tr, &t, Some("0.4.21"));
        assert!(n.reachable && n.installed == Some(true));
        assert_eq!(
            (n.macos_in_use, n.vms_total, n.disk_free_gib),
            (Some(2), Some(2), Some(40))
        );
        assert_eq!(n.cpus_free, Some(4));
        let h = health::assess(&[n], 1_800_000_000);
        let m = &h.machines[0];
        assert!(
            m.items
                .iter()
                .any(|i| i.message == "2 of 2 macOS slots are in use on mini-01"),
            "{:?}",
            m.items
        );
        assert_eq!(m.update.as_ref().unwrap().wanted, "0.4.21");
        assert_eq!(m.last_backup, "not reported");
    }

    #[test]
    fn an_unreachable_mac_is_a_finding_with_the_ssh_reason() {
        let t = Target::parse("ci@mini-09").unwrap();
        let tr = FakeTransport::new(|_, _| {
            failed(
                255,
                "ssh: connect to host mini-09 port 22: Connection timed out",
            )
        });
        let n = probe_fleet_host(&tr, &t, None);
        assert!(!n.reachable);
        let h = health::assess(&[n], 1);
        assert_eq!(h.light, Severity::Fail);
        assert_eq!(
            h.machines[0].items[0].explain.as_deref(),
            Some("HOST_UNREACHABLE")
        );
    }

    #[test]
    fn a_stopped_fleet_daemon_is_a_manual_item_here() {
        let t = Target::parse("ci@mini-01").unwrap();
        let tr = FakeTransport::new(|_, _| {
            ok(&PROBE_OK.replace("daemon_healthy=yes", "daemon_healthy=no"))
        });
        let h = health::assess(&[probe_fleet_host(&tr, &t, None)], 1);
        let p = plan(&h);
        assert!(
            p.iter()
                .any(|x| x.item.contains("daemon_down") && x.disposition == Disposition::Manual)
        );
    }

    #[test]
    fn no_kubeconfig_means_no_gateway_look_at_any_cluster() {
        let dir = std::env::temp_dir().join(format!("fluxvm-gw-health-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".kube")).unwrap();
        // the default kubeconfig and unrelated files are never used
        std::fs::write(dir.join(".kube").join("config"), "x").unwrap();
        std::fs::write(dir.join(".kube").join("other.yaml"), "x").unwrap();
        assert!(gateway_items_for_known_clusters(&dir).is_empty());
        assert!(gateway_items_for_known_clusters(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
