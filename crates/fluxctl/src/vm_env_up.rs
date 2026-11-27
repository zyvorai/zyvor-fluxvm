// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl templates run|down` for a VM environment (docs/vm-templates.md): look, plan, and only then act. A run is
//! safe to repeat: a missing VM is created, a stopped one started, a running one only checked. A VM that was not made by
//! the same template is never touched, and nothing is deleted or rewound without `--recreate`, `--reset` or `--delete`.
//!
//! The flow is written against [`EnvApi`] (the daemon or the local state directory) and [`World`] (this machine and SSH),
//! so the tests drive it with fakes and never start anything.

use crate::cli_errors::coded;
use crate::run::{self, VmApi};
use crate::vm_env::{self, Check, EnvTemplate, L_HASH, L_TEMPLATE, Overrides};
use crate::vm_env_plan::{
    self as plan, Action, Existing, HostChecks, HostFacts, ImageState, Input, Plan, READY_SNAPSHOT,
    Wants,
};
use crate::vm_net::Backend;
use anyhow::{Context, Result, bail};
use fluxvm_core::config::Config;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The key a dry run shows when the user has none yet (a real run generates one).
const NO_KEY: &str = "(a new key, ~/.ssh/fluxvm_ed25519, is generated)";
/// How long one readiness check may take, and how often it is tried.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
const CHECK_TRIES: u32 = 3;
/// Printed by the guest: cloud-init's own exit code (0 done, 2 done with recoverable errors) and why.
const CLOUD_INIT_SCRIPT: &str = "command -v cloud-init >/dev/null 2>&1 || exit 0\n\
cloud-init status --wait >/dev/null 2>&1\nc=$?\n[ $c = 0 ] || cloud-init status --long 2>&1 | head -n 30\nexit $c\n";

/// The `templates run|down` flags that only VM environments take.
#[derive(clap::Args, Debug, Clone, Default, PartialEq)]
#[command(next_help_heading = "VM environments")]
pub struct VmFlags {
    /// The VM's name (default: the template's name).
    #[arg(long = "vm", value_name = "NAME")]
    pub vm: Option<String>,
    /// Virtual CPUs, instead of the template's.
    #[arg(long)]
    pub cpus: Option<u8>,
    /// Memory in MiB, instead of the template's.
    #[arg(long)]
    pub memory_mib: Option<u64>,
    /// Disk size in GiB, instead of the template's.
    #[arg(long)]
    pub disk_gib: Option<u64>,
    /// Forward a port of this machine to the guest: [ADDR:]HOST:GUEST (TCP, on 127.0.0.1 unless ADDR says otherwise).
    /// Repeatable.
    #[arg(short = 'p', long = "publish", value_name = "HOST:GUEST")]
    pub publish: Vec<String>,
    /// Share a folder of this machine into the guest: HOST:GUEST[:ro]. Repeatable. Linux needs virtiofsd.
    #[arg(long = "share", value_name = "HOST:GUEST[:ro]")]
    pub share: Vec<String>,
    /// macOS only: join a private VM-to-VM network; VMs on the same one reach each other directly. Repeatable.
    #[arg(long = "network", value_name = "NAME")]
    pub network: Vec<String>,
    /// Print the plan and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Delete this template's VM and its disk, then create it again. Asks first.
    #[arg(long, conflicts_with = "reset")]
    pub recreate: bool,
    /// Once ready, save memory and disk as the snapshot `ready` (kept if it exists).
    #[arg(long)]
    pub snapshot: bool,
    /// Put the VM back to its `ready` snapshot; what was written since is lost. Asks first.
    #[arg(long, conflicts_with = "snapshot")]
    pub reset: bool,
    /// With `down`: delete the VM and its disk instead of stopping it. Asks first.
    #[arg(long)]
    pub delete: bool,
    /// How long to wait for the guest's first-boot setup, in seconds (default 1200 on macOS, 1800 on Linux).
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<u64>,
}

impl VmFlags {
    /// Flags that mean nothing for this subcommand are refused rather than ignored.
    pub fn check_for(&self, down: bool) -> Result<()> {
        if down {
            let only = Self {
                vm: self.vm.clone(),
                delete: self.delete,
                dry_run: self.dry_run,
                ..Self::default()
            };
            if *self != only {
                bail!(
                    "`templates down` takes only --vm, --delete, --dry-run, --yes and --json for a VM"
                );
            }
        } else if self.delete {
            bail!("--delete is for `fluxctl templates down`");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub template: EnvTemplate,
    /// What the user typed to name the template (a built-in name or a file), for the commands printed afterwards.
    pub reference: String,
    pub flags: VmFlags,
    pub down: bool,
    pub yes: bool,
    pub json: bool,
}

/// What the flow needs from a daemon beyond [`VmApi`].
pub trait EnvApi: VmApi {
    /// Every VM record as JSON.
    async fn records(&self) -> Result<Vec<Value>>;
    async fn resume_vm(&self, id: Uuid) -> Result<()>;
}

impl EnvApi for run::Local<'_> {
    async fn records(&self) -> Result<Vec<Value>> {
        self.0
            .list()
            .await
            .iter()
            .map(|v| Ok(serde_json::to_value(v)?))
            .collect()
    }
    async fn resume_vm(&self, id: Uuid) -> Result<()> {
        self.0.resume(id).await.map(drop)
    }
}

impl EnvApi for crate::remote::Remote {
    async fn records(&self) -> Result<Vec<Value>> {
        self.list_vms(None).await
    }
    async fn resume_vm(&self, id: Uuid) -> Result<()> {
        self.vm_op(id, "resume").await.map(drop)
    }
}

/// This machine and the guest.
pub trait World {
    /// The backend and guest CPU architecture VMs get here.
    fn platform(&self) -> (Backend, String);
    /// The login to create in a new VM.
    fn user(&self) -> String;
    /// The public key a new VM accepts. With `generate` false nothing is written: `None` when there is no key yet.
    fn ssh_key(&self, generate: bool) -> Result<Option<String>>;
    async fn facts(&self, definition: &Value, template: &EnvTemplate, creating: bool) -> HostFacts;
    /// The address SSH answers on, once it does, waiting at most about `budget`.
    async fn address<A: VmApi>(
        &self,
        api: &A,
        id: Uuid,
        user: &str,
        budget: Duration,
    ) -> Result<String>;
    /// Runs a script in the guest: its exit code (255 when SSH itself failed) and output.
    async fn exec(
        &self,
        ip: &str,
        user: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<(i32, String)>;
    async fn pause_between_tries(&self);
    fn confirm(&self) -> Result<bool>;
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub name: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

fn result(name: &str, ok: bool, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name: name.to_owned(),
        ok,
        detail: detail.into(),
    }
}

/// The last lines of guest output, without control characters.
fn tail(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            l.chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect::<String>()
        })
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(4)..].join(" | ")
}

fn parse_overrides(f: &VmFlags, backend: Backend, home: &Path, cwd: &Path) -> Result<Overrides> {
    if backend == Backend::Qemu && !f.network.is_empty() {
        bail!(
            "--network needs the private VM-to-VM network, which only the macOS (vz) backend has; this host uses {}",
            backend.name()
        );
    }
    Ok(Overrides {
        cpus: f.cpus,
        memory_mib: f.memory_mib,
        disk_gib: f.disk_gib,
        forwards: f
            .publish
            .iter()
            .map(|p| run::parse_port(p))
            .collect::<Result<_>>()?,
        shares: f
            .share
            .iter()
            .map(|v| run::parse_volume(v, cwd, Some(home)))
            .collect::<Result<_>>()?,
        networks: f.network.clone(),
    })
}

/// The checks an existing VM must pass besides the template's: its shared folders are mounted and its private
/// network addresses are configured.
fn record_checks(request: &Value) -> Vec<Check> {
    let mut out = Vec::new();
    for s in request["shared_folders"].as_array().into_iter().flatten() {
        if let Some(g) = s["guest_path"].as_str() {
            out.push(Check {
                name: format!("shared folder {g} is mounted"),
                command: format!("mountpoint -q {}", crate::compose::shell_quote(g)),
            });
        }
    }
    for n in request["apple"]["networks"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let (Some(name), Some(address)) = (n["name"].as_str(), n["address"].as_str())
            && address
                .chars()
                .all(|c| c.is_ascii_digit() || "./".contains(c))
        {
            out.push(Check {
                name: format!("private network {name}: {address}"),
                command: format!("ip -4 -o addr show | grep -qF ' {address} '"),
            });
        }
    }
    out
}

/// Waits for SSH and the first-boot setup, then runs the checks. Stops early only when a later step could not work.
async fn readiness<A: EnvApi, W: World>(
    api: &A,
    w: &W,
    id: Uuid,
    user: &str,
    checks: &[Check],
    budget: Duration,
) -> (Option<String>, Vec<CheckResult>) {
    let mut out = Vec::new();
    let started = Instant::now();
    let ip = match w.address(api, id, user, budget).await {
        Ok(ip) => ip,
        Err(e) => {
            out.push(result("SSH answers", false, format!("{e:#}")));
            return (None, out);
        }
    };
    out.push(result("SSH answers", true, ""));
    eprintln!("waiting for the guest's first-boot setup (cloud-init) to finish…");
    loop {
        let left = budget.saturating_sub(started.elapsed());
        match w
            .exec(
                &ip,
                user,
                CLOUD_INIT_SCRIPT,
                left.max(Duration::from_secs(1)),
            )
            .await
        {
            Ok((0, _)) => out.push(result("first-boot setup finished", true, "")),
            Ok((2, text)) => out.push(result(
                "first-boot setup finished",
                true,
                format!("with warnings: {}", tail(&text)),
            )),
            // SSH itself failed: the guest may be restarting its network or itself.
            Ok((255, text)) => {
                if started.elapsed() < budget {
                    w.pause_between_tries().await;
                    continue;
                }
                out.push(result("first-boot setup finished", false, tail(&text)));
                return (Some(ip), out);
            }
            Ok((_, text)) => out.push(result(
                "first-boot setup finished",
                false,
                format!("cloud-init reports an error: {}", tail(&text)),
            )),
            Err(e) => {
                out.push(result(
                    "first-boot setup finished",
                    false,
                    format!("{e:#}; run the command again to keep waiting, or raise --timeout"),
                ));
                return (Some(ip), out);
            }
        }
        break;
    }
    for c in checks {
        let script = format!("( {} ) </dev/null\n", c.command);
        let mut last = result(&c.name, false, "");
        for attempt in 1..=CHECK_TRIES {
            last = match w.exec(&ip, user, &script, CHECK_TIMEOUT).await {
                Ok((0, _)) => result(&c.name, true, ""),
                Ok((code, text)) => result(&c.name, false, format!("exit {code}: {}", tail(&text))),
                Err(e) => result(&c.name, false, format!("{e:#}")),
            };
            if last.ok || attempt == CHECK_TRIES {
                break;
            }
            w.pause_between_tries().await;
        }
        out.push(last);
    }
    (Some(ip), out)
}

async fn find<A: EnvApi>(api: &A, vm: &str, snapshots: bool) -> Result<Vec<Existing>> {
    let mut out = Vec::new();
    for record in api.records().await? {
        if record["name"] != vm {
            continue;
        }
        let id: Uuid = record["id"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .context("a VM record has no id")?;
        let has_ready_snapshot = snapshots && api.has_snapshot(id, READY_SNAPSHOT).await?;
        out.push(Existing {
            id,
            status: record["status"].as_str().unwrap_or("").to_lowercase(),
            record,
            has_ready_snapshot,
        });
    }
    Ok(out)
}

/// `fluxctl …` with the server this run used.
fn fluxctl(facts: &HostFacts) -> String {
    match &facts.server {
        Some(s) => format!("fluxctl --server {s}"),
        None => "fluxctl".into(),
    }
}

fn next_steps(job: &Job, vm: &str, facts: &HostFacts) -> String {
    let f = fluxctl(facts);
    let t = &job.reference;
    let named = match &job.flags.vm {
        Some(v) => format!(" --vm {v}"),
        None => String::new(),
    };
    let mut s = format!(
        "\n  shell    {f} ssh {vm}\n  check    {f} templates run {t}{named}\n  stop     {f} templates down {t}{named}\n  delete   {f} templates down {t}{named} --delete\n"
    );
    if job.flags.snapshot {
        s.push_str(&format!(
            "  reset    {f} templates run {t}{named} --reset\n"
        ));
    }
    s
}

struct Report<'a> {
    plan: &'a Plan,
    dry_run: bool,
    applied: Vec<String>,
    ready: Option<bool>,
    readiness: Vec<CheckResult>,
    address: Option<String>,
    error: Option<String>,
}

impl Report<'_> {
    fn json(&self) -> Value {
        json!({
            "schema": 1,
            "template": self.plan.template,
            "vm": self.plan.vm,
            "dry_run": self.dry_run,
            "changed": !self.applied.is_empty(),
            "applied": self.applied,
            "plan": self.plan,
            "ready": self.ready,
            "readiness": self.readiness,
            "address": self.address,
            "error": self.error,
        })
    }
}

/// The whole flow. Exit code 0: done and ready (or nothing to do, or a dry run that can run). 1: blocked, failed or not
/// ready.
pub async fn run<A: EnvApi, W: World>(
    api: &A,
    w: &W,
    job: &Job,
    home: &Path,
    cwd: &Path,
    out: &mut dyn Write,
) -> Result<i32> {
    let f = &job.flags;
    f.check_for(job.down)?;
    let t = &job.template;
    let vm = f.vm.clone().unwrap_or_else(|| t.name.clone());
    vm_env::check_vm_name(&vm)?;
    let (backend, arch) = w.platform();
    let overrides = parse_overrides(f, backend, home, cwd)?;
    let wants = Wants {
        down: job.down,
        delete: f.delete,
        recreate: f.recreate,
        reset: f.reset,
        snapshot: f.snapshot,
    };
    let existing = find(api, &vm, f.snapshot || f.reset).await?;
    let define = |key: &str| vm_env::definition(t, &overrides, &vm, backend, &arch, &w.user(), key);
    let key = w.ssh_key(false)?;
    let mut definition = define(key.as_deref().unwrap_or(NO_KEY));
    let creating = !job.down && (existing.is_empty() || f.recreate);
    let facts = w.facts(&definition, t, creating).await;
    // The login of a VM that exists is the one it was made with.
    let user = match &existing[..] {
        [e] if !f.recreate => e.record["request"]["cloud_init"]["user"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| w.user()),
        _ => w.user(),
    };
    let mut checks = t.checks_for(&facts.arch, &user);
    checks.extend(record_checks(match &existing[..] {
        [e] if !f.recreate => &e.record["request"],
        _ => &definition,
    }));
    let p = plan::assess(&Input {
        template: t,
        vm: &vm,
        definition: &definition,
        existing: &existing,
        wants,
        facts: &facts,
        checks,
    });
    if !job.json {
        write!(out, "{}", plan::render(&p))?;
    }
    let mut report = Report {
        plan: &p,
        dry_run: f.dry_run,
        applied: Vec::new(),
        ready: None,
        readiness: Vec::new(),
        address: None,
        error: None,
    };
    if f.dry_run || p.blocked {
        if p.blocked {
            report.error = Some("blocked".into());
        }
        if job.json {
            writeln!(out, "{}", serde_json::to_string_pretty(&report.json())?)?;
        } else if p.blocked {
            writeln!(
                out,
                "\nNothing was changed. Fix each [blocked] line first (`fluxctl explain TEMPLATE_VM_BLOCKED`)."
            )?;
        } else {
            writeln!(out, "\nDry run: nothing was changed.")?;
        }
        return Ok(i32::from(p.blocked));
    }
    if p.action.destroys() && !job.yes && !w.confirm()? {
        writeln!(out, "Nothing changed.")?;
        return Ok(0);
    }

    let current = existing.first().map(|e| e.id);
    let acted: Result<Option<Uuid>> = async {
        Ok(match (p.action, current) {
            (Action::Create | Action::Recreate, old) => {
                // Only now may a key be generated.
                let key = w
                    .ssh_key(true)?
                    .context("no SSH public key could be read or generated")?;
                definition = define(&key);
                if let Some(id) = old {
                    eprintln!("deleting {vm}…");
                    api.delete(id)
                        .await
                        .with_context(|| format!("deleting {vm}"))?;
                    report.applied.push(format!("deleted {vm}"));
                }
                eprintln!("creating {vm} (the first time its image is downloaded)…");
                let id = api
                    .create(vm_env::create_request(&definition, &t.name, facts.backend))
                    .await
                    .with_context(|| format!("creating {vm}"))?;
                report.applied.push(format!("created {vm}"));
                // Visible in `fluxctl list`; ownership itself rides in the request.
                let labels = BTreeMap::from([
                    (L_TEMPLATE.to_owned(), t.name.clone()),
                    (L_HASH.to_owned(), crate::stack::spec_hash(&definition)),
                ]);
                if let Err(e) = api.set_labels(id, labels).await {
                    eprintln!("could not label {vm} (not needed to work): {e:#}");
                }
                Some(id)
            }
            (Action::Start, Some(id)) => {
                eprintln!("starting {vm}…");
                api.start(id).await.with_context(|| {
                    format!("starting {vm} (--recreate starts over and deletes its disk)")
                })?;
                report.applied.push(format!("started {vm}"));
                Some(id)
            }
            (Action::Resume, Some(id)) => {
                api.resume_vm(id)
                    .await
                    .with_context(|| format!("resuming {vm}"))?;
                report.applied.push(format!("resumed {vm}"));
                Some(id)
            }
            (Action::Reset, Some(id)) => {
                if matches!(existing[0].status.as_str(), "running" | "paused") {
                    eprintln!("stopping {vm}…");
                    api.stop(id)
                        .await
                        .with_context(|| format!("stopping {vm}"))?;
                }
                eprintln!("restoring {vm} to `{READY_SNAPSHOT}`…");
                api.restore(id, READY_SNAPSHOT)
                    .await
                    .with_context(|| format!("restoring {vm}"))?;
                report
                    .applied
                    .push(format!("restored {vm} to {READY_SNAPSHOT}"));
                Some(id)
            }
            (Action::Stop, Some(id)) => {
                api.stop(id)
                    .await
                    .with_context(|| format!("stopping {vm}"))?;
                report.applied.push(format!("stopped {vm}"));
                None
            }
            (Action::Delete, Some(id)) => {
                api.delete(id)
                    .await
                    .with_context(|| format!("deleting {vm}"))?;
                report.applied.push(format!("deleted {vm}"));
                None
            }
            (Action::Check, id) => id,
            _ => None,
        })
    }
    .await;

    match acted {
        Err(e) => report.error = Some(format!("{e:#}")),
        Ok(Some(id)) => {
            // The record now holds what the daemon filled in (private network addresses).
            let record = api
                .records()
                .await?
                .into_iter()
                .find(|r| r["id"] == id.to_string().as_str());
            let mut checks = t.checks_for(&facts.arch, &user);
            if let Some(r) = &record {
                checks.extend(record_checks(&r["request"]));
            }
            let default = if facts.backend == Backend::Vz {
                1200
            } else {
                1800
            };
            let budget = Duration::from_secs(f.timeout.unwrap_or(default));
            let (ip, results) = readiness(api, w, id, &user, &checks, budget).await;
            let ready = results.iter().all(|r| r.ok);
            report.address = ip;
            report.readiness = results;
            report.ready = Some(ready);
            if ready && f.snapshot {
                let saved: Result<bool> = async {
                    if api.has_snapshot(id, READY_SNAPSHOT).await? {
                        return Ok(false);
                    }
                    eprintln!("saving the snapshot `{READY_SNAPSHOT}`…");
                    api.snapshot(id, READY_SNAPSHOT).await?;
                    Ok(true)
                }
                .await;
                match saved {
                    Ok(true) => report
                        .applied
                        .push(format!("saved snapshot {READY_SNAPSHOT}")),
                    Ok(false) => {}
                    Err(e) => {
                        report.error = Some(format!(
                            "the environment is ready, but the snapshot was not saved: {e:#}"
                        ))
                    }
                }
            }
            if !ready {
                report.error = Some("not ready".into());
            }
        }
        Ok(None) => {}
    }

    let code = i32::from(report.error.is_some());
    if job.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&report.json())?)?;
        return Ok(code);
    }
    if !report.readiness.is_empty() {
        writeln!(out, "\nReadiness:")?;
        for r in &report.readiness {
            let sep = if r.detail.is_empty() { "" } else { ": " };
            writeln!(
                out,
                "  {} {}{sep}{}",
                if r.ok { "[ok]  " } else { "[fail]" },
                r.name,
                r.detail
            )?;
        }
    }
    match (report.ready, &report.error) {
        (Some(true), None) => {
            let at = report.address.as_deref().unwrap_or("?");
            writeln!(out, "\n{vm} is ready (SSH at {at}).")?;
            if report.applied.is_empty() {
                writeln!(out, "Nothing was changed.")?;
            }
            write!(out, "{}", next_steps(job, &vm, &facts))?;
        }
        (Some(false), _) => writeln!(
            out,
            "\n{vm} is NOT ready. It is left running: look with `{} ssh {vm}`, then run the same command again; --recreate starts over and deletes its disk.\n  more: fluxctl explain TEMPLATE_VM_NOT_READY",
            fluxctl(&facts)
        )?,
        (_, Some(e)) => writeln!(out, "\nFailed: {e}")?,
        (None, None) if report.applied.is_empty() => {
            writeln!(out, "\nNothing to do. Nothing was changed.")?
        }
        (None, None) => writeln!(out, "\nDone: {}.", report.applied.join(", "))?,
    }
    Ok(code)
}

// ---- the real machine --------------------------------------------------------------------------------------------------

/// Whether a `--server` URL points at this machine.
fn is_loopback(server: &str) -> bool {
    let rest = server.split_once("://").map_or(server, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback())
}

fn installed(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

fn nested_kvm() -> Option<bool> {
    ["kvm_intel", "kvm_amd"].iter().find_map(|m| {
        std::fs::read_to_string(format!("/sys/module/{m}/parameters/nested"))
            .ok()
            .map(|v| matches!(v.trim(), "Y" | "y" | "1"))
    })
}

pub struct RealWorld {
    /// The local configuration, when this process runs the VM itself.
    pub cfg: Option<Config>,
    /// The daemon in use, as the user would name it with `--server`.
    pub server: Option<String>,
    pub home: PathBuf,
}

impl World for RealWorld {
    fn platform(&self) -> (Backend, String) {
        (Backend::host(), std::env::consts::ARCH.to_owned())
    }

    fn user(&self) -> String {
        std::env::var("USER")
            .ok()
            .filter(|u| {
                !u.is_empty()
                    && u != "root"
                    && u.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            })
            .unwrap_or_else(|| "fluxvm".into())
    }

    fn ssh_key(&self, generate: bool) -> Result<Option<String>> {
        if generate {
            return run::ssh_key(&self.home).map(|(k, _)| Some(k));
        }
        Ok(["id_ed25519", "id_ecdsa", "id_rsa", "fluxvm_ed25519"]
            .iter()
            .find_map(|n| {
                std::fs::read_to_string(self.home.join(".ssh").join(format!("{n}.pub"))).ok()
            })
            .map(|k| k.trim().to_owned()))
    }

    async fn facts(&self, definition: &Value, template: &EnvTemplate, creating: bool) -> HostFacts {
        let (backend, arch) = self.platform();
        let remote = self.server.as_deref().is_some_and(|s| !is_loopback(s));
        let mut facts = HostFacts {
            backend,
            os: std::env::consts::OS.to_owned(),
            arch,
            server: self.server.clone(),
            remote,
            host: None,
        };
        let Some(cfg) = self.cfg.as_ref() else {
            return facts;
        };
        let probe = fluxvm_core::up::probe(&cfg.state_dir);
        let apple = match backend {
            Backend::Vz => {
                fluxvm_apple::prime_host_capabilities().await;
                Some(fluxvm_apple::host_capabilities().map_err(|e| format!("{e:#}")))
            }
            Backend::Qemu => None,
        };
        let invalid = (backend == Backend::Vz && creating)
            .then(|| {
                serde_json::from_value(definition.clone())
                    .map_err(anyhow::Error::from)
                    .and_then(|r| fluxvm_apple::validate_request(&r))
                    .err()
                    .map(|e| format!("{e:#}"))
            })
            .flatten();
        let image = if fluxvm_image::builtin::find(&template.image).is_none() {
            ImageState::Other
        } else if fluxvm_image::builtin::cached_names(cfg).contains(&template.image) {
            ImageState::Cached
        } else {
            ImageState::Download
        };
        let missing_tools = match backend {
            Backend::Vz => Vec::new(),
            Backend::Qemu => [
                &cfg.qemu_binary,
                &cfg.qemu_img_binary,
                &cfg.cloud_localds_binary,
            ]
            .into_iter()
            .filter(|p| !installed(p))
            .cloned()
            .collect(),
        };
        let busy_ports = definition["network"]["forwards"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| u16::try_from(f["host_port"].as_u64()?).ok())
            .filter(|p| !crate::vm_net::loopback_port_free(*p))
            .collect();
        facts.host = Some(HostChecks {
            apple,
            kvm: probe.kvm,
            nested_kvm: nested_kvm(),
            missing_tools,
            virtiofsd: installed(&cfg.virtiofsd_binary),
            disk_free_gib: probe.disk_free_gib,
            mem_total_mib: probe.mem_total_mib,
            image,
            busy_ports,
            invalid,
        });
        facts
    }

    async fn address<A: VmApi>(
        &self,
        api: &A,
        id: Uuid,
        user: &str,
        budget: Duration,
    ) -> Result<String> {
        let started = Instant::now();
        loop {
            let attempt = Instant::now();
            match run::wait_ssh(api, id, user, &run::generated_identity()).await {
                Ok(ip) => return Ok(ip),
                // One wait gives up after a few minutes on a Mac, but a first boot that installs packages reports its
                // address only after them. A quick failure is something else and is returned at once.
                Err(e)
                    if started.elapsed() < budget
                        && attempt.elapsed() >= Duration::from_secs(60) =>
                {
                    eprintln!("still waiting: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn exec(
        &self,
        ip: &str,
        user: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<(i32, String)> {
        use tokio::io::AsyncWriteExt;
        let mut c =
            tokio::process::Command::from(run::ssh_base(ip, user, &run::generated_identity()));
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "sh",
            "-s",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
        let mut child = c.spawn().context("running ssh")?;
        let mut stdin = child.stdin.take().context("ssh stdin")?;
        // A failed connection closes the pipe early; the exit status says so.
        let _ = stdin.write_all(script.as_bytes()).await;
        drop(stdin);
        let out = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("no answer within {}s", timeout.as_secs()))??;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok((out.status.code().unwrap_or(255), text))
    }

    async fn pause_between_tries(&self) {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    fn confirm(&self) -> Result<bool> {
        crate::up::ask_on_terminal()
    }
}

/// Held while a run changes a VM, so two runs cannot create it twice.
struct Lock(#[allow(dead_code)] std::fs::File);

fn lock(home: &Path, vm: &str) -> Result<Lock> {
    use std::os::fd::AsRawFd;
    let dir = crate::templates_cmd::data_dir(home).join("locks");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{vm}.lock")))?;
    // SAFETY: flock on a descriptor this function owns; the lock ends when the file is closed.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(coded(
            format!("another `fluxctl templates` command is working on {vm}; wait for it"),
            "TEMPLATE_VM_BLOCKED",
        ));
    }
    Ok(Lock(file))
}

/// The entry point from the command line, for the local state directory (`cfg`) or a daemon (`server`).
pub async fn run_command<A: EnvApi>(
    api: &A,
    cfg: Option<Config>,
    server: Option<String>,
    job: &Job,
) -> Result<i32> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    let cwd = std::env::current_dir()?;
    let vm = job.flags.vm.as_deref().unwrap_or(&job.template.name);
    vm_env::check_vm_name(vm)?;
    let _lock = (!job.flags.dry_run).then(|| lock(&home, vm)).transpose()?;
    let world = RealWorld {
        cfg,
        server,
        home: home.clone(),
    };
    run(api, &world, job, &home, &cwd, &mut std::io::stdout()).await
}

#[cfg(test)]
#[path = "vm_env_up_tests.rs"]
mod tests;
