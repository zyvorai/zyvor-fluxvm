// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac schedule`: run a Mac image on a cron expression, and `mac notify`.
//!
//! * `schedule add|ls|rm|enable|disable` edit `mac-schedules.json` (next to the image store; see `fluxvm_core::mac_schedule`), the
//!   same file Studio and `/v1/mac/schedules` use.
//! * `schedule tick` starts every schedule that is due, once each (`--loop` keeps going). `schedule install` writes a LaunchAgent
//!   (no sudo) that runs the loop, so schedules fire with nobody logged in to a terminal. Without it, run `tick` from cron yourself.
//! * A schedule with a CMD runs `mac run --rm IMAGE -- CMD` and waits for it; the TTL is its deadline (the VM is removed if CMD is
//!   still running then). A schedule without a CMD starts an instance and the TTL guard below removes it.
//! * The TTL guard (`mac run --ttl 2h`, `mac ttl`) is a small detached `fluxctl` process in its own process group, so it survives
//!   the terminal and the fluxctl that started it. It follows the `fluxvm.mac.expires` label, so `mac ttl NAME 4h` and
//!   `mac ttl NAME none` are honoured. If the Mac reboots the guard is gone; `mac supervise` / `mac reap` still enforce the label.
//!
//! Decisions (who is due, when a guard acts) are plain functions or sit behind small traits and are unit tested with fakes.

use crate::mac_errors::coded;
use crate::mac_notify::{self, Event, Targets};
use anyhow::{Context, Result};
use clap::Subcommand;
use fluxvm_core::mac_schedule as ms;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

pub const AGENT_LABEL: &str = "dev.zyvor.fluxvm.mac-schedule";

#[derive(Subcommand, Debug)]
pub enum MacSched {
    /// Run a Mac image on a cron schedule, and look after the schedules. `fluxctl mac schedule add --help` has examples.
    Schedule {
        #[command(subcommand)]
        command: SchedCmd,
    },
    /// Notifications for Mac runs: `notify test` checks your setup, `notify wait NAME` tells you when a detached instance ends.
    Notify {
        #[command(subcommand)]
        command: mac_notify::NotifyCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum SchedCmd {
    /// Add a schedule: NAME, a cron expression in quotes (minute hour day month weekday, local time), the image, and an optional
    /// command after `--`.
    #[command(
        after_help = "Examples:\n  fluxctl mac schedule add nightly \"0 3 * * *\" macos-xcode:15 --notify -- ./build.sh\n  fluxctl mac schedule add warm-vm \"*/30 8-18 * * mon-fri\" macos-base:15 --ttl 25m\n  fluxctl mac schedule add weekly \"@weekly\" macos-base:15 --notify-webhook https://hooks.example.com/abc -- ./report.sh\n\nWith a command the VM is removed when it ends (and after --ttl if it is still running). Without one, an instance is started and removed after --ttl. Schedules fire only while `fluxctl mac schedule tick --loop` runs: `fluxctl mac schedule install` does that at login. If it fails: fluxctl explain MAC_SCHEDULE_INVALID"
    )]
    Add {
        name: String,
        /// `*/15 * * * *`, `0 3 * * mon-fri`, `@hourly`, `@daily`, `@weekly`, `@monthly`.
        cron: String,
        image: String,
        /// Stop and remove the VM after this long (`30m`, `2h`).
        #[arg(long, value_name = "DURATION")]
        ttl: Option<String>,
        /// Show a macOS notification when a run ends.
        #[arg(long)]
        notify: bool,
        /// Also POST the result to this URL (or set FLUXVM_MAC_NOTIFY_WEBHOOK). It is stored mode 0600 and never printed in full.
        #[arg(long, env = "FLUXVM_MAC_NOTIFY_WEBHOOK", hide_env_values = true)]
        notify_webhook: Option<String>,
        /// Add it switched off (`schedule enable NAME` starts it).
        #[arg(long)]
        disabled: bool,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// List the schedules with their next run and how the last one went.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Remove a schedule. A VM it already started is left alone (`fluxctl mac rm NAME`).
    Rm { name: String },
    /// Start firing a schedule that was added with --disabled or switched off.
    Enable { name: String },
    /// Stop a schedule from firing without deleting it.
    Disable { name: String },
    /// Run a schedule once now and wait for it, whatever the cron says. Needs the daemon.
    RunNow { name: String },
    /// Start every schedule that is due. `--loop` keeps checking (this is what the LaunchAgent runs); without it, call it from cron.
    Tick {
        /// Keep running, checking every --every seconds.
        #[arg(long = "loop")]
        keep_going: bool,
        /// Seconds between checks with --loop.
        #[arg(long, default_value_t = 30)]
        every: u64,
        /// Say what would run and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Write and load a LaunchAgent (no sudo) that runs `tick --loop` at login. Carries --server/--context, never a token.
    Install {
        #[arg(long = "daemon", value_name = "URL")]
        daemon: Option<String>,
        #[arg(long = "daemon-context", value_name = "NAME")]
        daemon_context: Option<String>,
    },
    /// Unload the LaunchAgent and delete its plist.
    Uninstall,
    /// Is the LaunchAgent installed and loaded?
    Status,
    /// Run one schedule to the end (started by `tick`).
    #[command(hide = true)]
    Exec { name: String },
    /// Remove an instance when its `fluxvm.mac.expires` label passes (started by `mac run --ttl`).
    #[command(hide = true)]
    Guard {
        name: String,
        #[arg(long)]
        notify: bool,
        #[arg(long, env = "FLUXVM_MAC_NOTIFY_WEBHOOK", hide_env_values = true)]
        notify_webhook: Option<String>,
    },
}

impl MacSched {
    pub fn is_client_side(&self) -> bool {
        match self {
            MacSched::Notify { command } => command.is_client_side(),
            MacSched::Schedule { command } => matches!(
                command,
                SchedCmd::Add { .. }
                    | SchedCmd::Ls { .. }
                    | SchedCmd::Rm { .. }
                    | SchedCmd::Enable { .. }
                    | SchedCmd::Disable { .. }
                    | SchedCmd::Install { .. }
                    | SchedCmd::Uninstall
                    | SchedCmd::Status
                    | SchedCmd::Tick { dry_run: true, .. }
            ),
        }
    }
}

fn path() -> Result<PathBuf> {
    ms::default_path().context("HOME is not set (or set FLUXVM_MAC_SCHEDULES to the schedule file)")
}

fn invalid(m: String) -> anyhow::Error {
    coded(m, "MAC_SCHEDULE_INVALID")
}
fn missing(m: String) -> anyhow::Error {
    coded(m, "MAC_SCHEDULE_MISSING")
}

fn now() -> u64 {
    crate::mac_health::now_unix()
}

fn when(unix: Option<u64>) -> String {
    use chrono::TimeZone;
    unix.and_then(|u| chrono::Local.timestamp_opt(u as i64, 0).single())
        .map_or("-".into(), |d| d.format("%Y-%m-%d %H:%M").to_string())
}

pub fn view(s: &ms::Schedule, now: u64) -> Value {
    s.view(now)
}

pub fn run_client(c: MacSched) -> Result<()> {
    let MacSched::Schedule { command } = c else {
        anyhow::bail!("internal: notify is run elsewhere");
    };
    let p = path()?;
    match command {
        SchedCmd::Add {
            name,
            cron,
            image,
            ttl,
            notify,
            notify_webhook,
            disabled,
            cmd,
        } => {
            let s = ms::Schedule {
                name: name.clone(),
                cron,
                image,
                cmd,
                ttl,
                notify,
                webhook: notify_webhook,
                enabled: !disabled,
                created_unix: 0,
                last_run_unix: None,
                last_status: None,
                last_detail: None,
                run_now: false,
            };
            ms::add(&p, s, now()).map_err(invalid)?;
            let s = ms::load(&p).map_err(anyhow::Error::msg)?;
            let s = s
                .schedules
                .iter()
                .find(|s| s.name == name)
                .context("saved")?;
            println!(
                "added {name}: next run {}{}",
                when(s.next_run(now())),
                if s.enabled { "" } else { " (disabled)" }
            );
            println!(
                "schedules fire while `fluxctl mac schedule tick --loop` runs: `fluxctl mac schedule install` does that at login"
            );
            Ok(())
        }
        SchedCmd::Ls { json } => {
            let f = ms::load(&p).map_err(anyhow::Error::msg)?;
            let n = now();
            if json {
                let items: Vec<Value> = f.schedules.iter().map(|s| view(s, n)).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"schedules": items}))?
                );
            } else if f.schedules.is_empty() {
                println!(
                    "no schedules. Add one: fluxctl mac schedule add nightly \"0 3 * * *\" IMAGE -- CMD"
                );
            } else {
                println!(
                    "{:<18} {:<16} {:<18} {:<16} {:<8} LAST",
                    "NAME", "CRON", "IMAGE", "NEXT", "STATE"
                );
                for s in &f.schedules {
                    println!(
                        "{:<18} {:<16} {:<18} {:<16} {:<8} {}",
                        s.name,
                        s.cron,
                        s.image,
                        when(s.next_run(n)),
                        if s.enabled { "on" } else { "off" },
                        match (&s.last_status, s.last_run_unix) {
                            (Some(st), Some(t)) => format!("{st} {}", when(Some(t))),
                            _ => "never".into(),
                        }
                    );
                }
            }
            Ok(())
        }
        SchedCmd::Rm { name } => {
            ms::remove(&p, &name).map_err(missing)?;
            println!("removed {name}");
            Ok(())
        }
        SchedCmd::Enable { name } => {
            ms::update(&p, &name, |s| s.enabled = true).map_err(missing)?;
            println!("{name} is on");
            Ok(())
        }
        SchedCmd::Disable { name } => {
            ms::update(&p, &name, |s| s.enabled = false).map_err(missing)?;
            println!("{name} is off");
            Ok(())
        }
        SchedCmd::Tick { dry_run: true, .. } => {
            let mut sp = Dry;
            for l in tick_once(&p, &mut sp, now())? {
                println!("{l}");
            }
            Ok(())
        }
        SchedCmd::Install {
            daemon,
            daemon_context,
        } => install(daemon, daemon_context),
        SchedCmd::Uninstall => {
            use crate::docker_host as dh;
            let (plist, _) = agent_paths(&dh::home()?);
            let removed = dh::launchctl_unload(AGENT_LABEL, &plist)?;
            println!("{}", json!({"removed": removed}));
            Ok(())
        }
        SchedCmd::Status => {
            use crate::docker_host as dh;
            let (plist, log) = agent_paths(&dh::home()?);
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::launch_agent::status_of(
                    AGENT_LABEL,
                    &plist,
                    &log
                )?)?
            );
            Ok(())
        }
        _ => anyhow::bail!("internal: not a client-side schedule command"),
    }
}

fn agent_paths(home: &std::path::Path) -> (PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist")),
        home.join("Library/Logs/fluxvm-mac-schedule.log"),
    )
}

pub fn agent_args(exe: &std::path::Path, daemon: Option<&str>, ctx: Option<&str>) -> Vec<String> {
    let mut v = vec![exe.display().to_string()];
    if let Some(d) = daemon {
        v.extend(["--server".into(), d.into()]);
    }
    if let Some(c) = ctx {
        v.extend(["--context".into(), c.into()]);
    }
    v.extend(["mac", "schedule", "tick", "--loop"].map(String::from));
    v
}

fn install(daemon: Option<String>, ctx: Option<String>) -> Result<()> {
    use crate::docker_host as dh;
    let (plist, log) = agent_paths(&dh::home()?);
    let exe = crate::stable_exe::stable_exe().context("finding fluxctl's own path")?;
    let args = agent_args(&exe, daemon.as_deref(), ctx.as_deref());
    std::fs::create_dir_all(plist.parent().context("plist path")?)?;
    std::fs::write(
        &plist,
        dh::render_plist_with(
            AGENT_LABEL,
            &args,
            &log,
            &dh::carried_env(),
            &dh::PlistOpts::default(),
        ),
    )?;
    dh::launchctl_load(AGENT_LABEL, &plist)?;
    println!("{}", json!({"installed": plist, "log": log}));
    Ok(())
}

// ---- tick ----------------------------------------------------------------------------------------------------

/// Starts one schedule's run outside this process.
pub trait Spawner {
    /// False for the dry run: nothing is recorded in the file.
    fn records(&self) -> bool {
        true
    }
    fn spawn_exec(&mut self, name: &str) -> Result<()>;
}

struct Dry;
impl Spawner for Dry {
    fn records(&self) -> bool {
        false
    }
    fn spawn_exec(&mut self, _: &str) -> Result<()> {
        Ok(())
    }
}

/// One pass: every due schedule is marked `running` (so the next pass does not start it again) and handed to the spawner.
/// With the dry spawner the file is left alone.
pub fn tick_once<S: Spawner>(p: &std::path::Path, sp: &mut S, now: u64) -> Result<Vec<String>> {
    let f = ms::load(p).map_err(anyhow::Error::msg)?;
    let mut out = Vec::new();
    for s in f.schedules.iter().filter(|s| s.due(now)) {
        let real = sp.records();
        if real {
            ms::update(p, &s.name, |x| mark_started(x, now)).map_err(anyhow::Error::msg)?;
        }
        match sp.spawn_exec(&s.name) {
            Ok(()) => out.push(format!(
                "{} {}",
                if real { "started" } else { "would start" },
                s.name
            )),
            Err(e) => {
                let msg = format!("{e:#}");
                ms::update(p, &s.name, |x| {
                    x.last_status = Some("failed".into());
                    x.last_detail = Some(msg.clone());
                })
                .ok();
                out.push(format!("could not start {}: {msg}", s.name));
            }
        }
    }
    Ok(out)
}

fn mark_started(s: &mut ms::Schedule, now: u64) {
    s.last_run_unix = Some(now);
    s.last_status = Some("running".into());
    s.last_detail = None;
    s.run_now = false;
}

/// Arguments that point a child `fluxctl` at the same daemon. The token goes in the child's environment, never its argv.
fn child_command(r: &crate::remote::Remote, args: &[&str]) -> Result<std::process::Command> {
    let exe = crate::stable_exe::stable_exe().context("finding fluxctl's own path")?;
    let mut c = std::process::Command::new(exe);
    c.args(["--server", r.base()]).args(args);
    if let Some(t) = r.bearer() {
        c.env("FLUXVM_TOKEN", t);
    }
    Ok(c)
}

fn detach(c: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    // Its own process group: closing the terminal (SIGHUP) or Ctrl-C in it does not reach the child.
    c.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
}

struct RealSpawner<'a> {
    r: &'a crate::remote::Remote,
}
impl Spawner for RealSpawner<'_> {
    fn spawn_exec(&mut self, name: &str) -> Result<()> {
        let mut c = child_command(self.r, &["mac", "schedule", "exec", name])?;
        detach(&mut c);
        let mut child = c.spawn().context("starting `fluxctl mac schedule exec`")?;
        // Reap it when it ends, so a long `--loop` leaves no zombies.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

// ---- running one schedule ------------------------------------------------------------------------------------

fn run_args(s: &ms::Schedule, instance: &str) -> Vec<String> {
    let mut a: Vec<String> = ["mac", "run", "--name", instance].map(String::from).into();
    if !s.cmd.is_empty() {
        a.push("--rm".into());
    } else if let Some(t) = &s.ttl {
        a.extend(["--ttl".into(), t.clone()]);
    }
    a.push(s.image.clone());
    if !s.cmd.is_empty() {
        a.push("--".into());
        a.extend(s.cmd.iter().cloned());
    }
    a
}

fn tail(s: &str) -> String {
    let t = s.trim();
    let n = t.chars().count();
    if n <= 300 {
        t.to_string()
    } else {
        format!("…{}", t.chars().skip(n - 300).collect::<String>())
    }
}

/// How the run ended, in words for `ls` and for the notification.
pub fn summarize(
    ok: bool,
    name: &str,
    timed_out: bool,
    code: Option<i32>,
    instance: &str,
    has_cmd: bool,
) -> Event {
    let detail = if timed_out {
        format!("{instance} was still running at its time limit and was removed")
    } else if ok && has_cmd {
        format!("{instance} ran to the end (exit 0)")
    } else if ok {
        format!("started {instance}")
    } else {
        format!(
            "{instance} failed (exit {}). Look at it with `fluxctl mac schedule ls`, then try it by hand: `fluxctl mac schedule run-now {name}`. If it is a slot or session problem: `fluxctl mac doctor`",
            code.map_or("none".into(), |c| c.to_string())
        )
    };
    Event {
        what: name.into(),
        ok: ok && !timed_out,
        detail,
    }
}

async fn run_schedule(r: &crate::remote::Remote, name: &str) -> Result<Event> {
    let p = path()?;
    let f = ms::load(&p).map_err(anyhow::Error::msg)?;
    let s = f
        .schedules
        .iter()
        .find(|s| s.name == name)
        .cloned()
        .ok_or_else(|| {
            missing(format!(
                "no schedule named {name:?}. See `fluxctl mac schedule ls`"
            ))
        })?;
    let instance = format!("{}-{}", s.name, now());
    let args = run_args(&s, &instance);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut cmd = tokio::process::Command::new(child_command(r, &argv)?.get_program());
    cmd.args(["--server", r.base()])
        .args(&argv)
        .kill_on_drop(true);
    if let Some(t) = r.bearer() {
        cmd.env("FLUXVM_TOKEN", t);
    }
    let limit = match (&s.ttl, s.cmd.is_empty()) {
        (Some(t), false) => Some(Duration::from_secs(
            ms::parse_duration(t).map_err(anyhow::Error::msg)?,
        )),
        _ => None,
    };
    let fut = cmd.output();
    let (out, timed_out) = match limit {
        Some(l) => match tokio::time::timeout(l, fut).await {
            Ok(o) => (Some(o), false),
            Err(_) => (None, true),
        },
        None => (Some(fut.await), false),
    };
    if timed_out && let Ok(vm) = crate::mac_cmd::find_instance(r, &instance).await {
        crate::mac_cmd::delete_instance(r, vm["id"].as_str().unwrap_or_default())
            .await
            .ok();
    }
    let (ok, code, stderr) = match out {
        Some(Ok(o)) => (
            o.status.success(),
            o.status.code(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        ),
        Some(Err(e)) => (false, None, format!("could not start fluxctl: {e}")),
        None => (false, None, String::new()),
    };
    let ev = summarize(ok, &s.name, timed_out, code, &instance, !s.cmd.is_empty());
    let detail = if ok || timed_out {
        ev.detail.clone()
    } else {
        format!("{} | {}", ev.detail, tail(&stderr))
    };
    ms::update(&p, &s.name, |x| {
        x.last_status = Some(if ev.ok { "ok" } else { "failed" }.into());
        x.last_detail = Some(detail.clone());
    })
    .ok();
    let t = Targets {
        desktop: s.notify,
        webhook: s.webhook.clone(),
    };
    if t.any() {
        mac_notify::warn(&mac_notify::deliver(&mac_notify::Real, &t, &ev).await);
    }
    Ok(ev)
}

// ---- TTL guard -----------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Expiry {
    Gone,
    Cleared,
    At(u64),
}

#[derive(Debug, PartialEq, Eq)]
pub enum GuardStep {
    /// Nothing left to guard (instance removed by someone, or the TTL cleared).
    Done,
    Wait(u64),
    Removed,
    Failed(String),
}

pub trait GuardHost {
    async fn expiry(&mut self, name: &str) -> Result<Expiry>;
    async fn remove(&mut self, name: &str) -> Result<()>;
}

/// Longest sleep between looks, so a changed or cleared TTL is noticed.
pub const GUARD_POLL: u64 = 30;

pub async fn guard_step<H: GuardHost>(h: &mut H, name: &str, now: u64) -> GuardStep {
    match h.expiry(name).await {
        // A daemon that is briefly away must not make the guard give up.
        Err(_) => GuardStep::Wait(GUARD_POLL),
        Ok(Expiry::Gone | Expiry::Cleared) => GuardStep::Done,
        Ok(Expiry::At(e)) if now >= e => match h.remove(name).await {
            Ok(()) => GuardStep::Removed,
            Err(err) => GuardStep::Failed(format!("{err:#}")),
        },
        Ok(Expiry::At(e)) => GuardStep::Wait((e - now).min(GUARD_POLL)),
    }
}

struct DaemonGuard<'a> {
    r: &'a crate::remote::Remote,
}
impl GuardHost for DaemonGuard<'_> {
    async fn expiry(&mut self, name: &str) -> Result<Expiry> {
        let vm = match crate::mac_cmd::find_instance(self.r, name).await {
            Ok(v) => v,
            Err(e) if format!("{e}").starts_with("no instance named") => return Ok(Expiry::Gone),
            Err(e) => return Err(e),
        };
        Ok(match crate::mac_reap::expires_at(&vm) {
            Some(e) => Expiry::At(e),
            None => Expiry::Cleared,
        })
    }
    async fn remove(&mut self, name: &str) -> Result<()> {
        let vm = crate::mac_cmd::find_instance(self.r, name).await?;
        crate::mac_cmd::delete_instance(self.r, vm["id"].as_str().unwrap_or_default()).await
    }
}

async fn run_guard(r: &crate::remote::Remote, name: &str, t: Targets) -> Result<()> {
    let mut g = DaemonGuard { r };
    loop {
        match guard_step(&mut g, name, now()).await {
            GuardStep::Done => return Ok(()),
            GuardStep::Wait(s) => tokio::time::sleep(Duration::from_secs(s.max(1))).await,
            GuardStep::Removed => {
                let ev = Event {
                    what: name.into(),
                    ok: true,
                    detail: format!("{name} reached its time limit and was stopped and removed."),
                };
                if t.any() {
                    mac_notify::warn(&mac_notify::deliver(&mac_notify::Real, &t, &ev).await);
                }
                return Ok(());
            }
            GuardStep::Failed(e) => {
                let ev = Event {
                    what: name.into(),
                    ok: false,
                    detail: format!(
                        "{name} reached its time limit but could not be removed ({e}). Remove it with `fluxctl mac rm {name}`"
                    ),
                };
                if t.any() {
                    mac_notify::warn(&mac_notify::deliver(&mac_notify::Real, &t, &ev).await);
                }
                return Err(coded(ev.detail, "MAC_TTL_REMOVE_FAILED"));
            }
        }
    }
}

/// Start a guard for `name` in its own process group. It sees the same daemon and token as this process.
pub fn spawn_guard(r: &crate::remote::Remote, name: &str, t: &Targets) -> Result<()> {
    let mut args = vec!["mac", "schedule", "guard", name];
    if t.desktop {
        args.push("--notify");
    }
    let mut c = child_command(r, &args)?;
    if let Some(w) = &t.webhook {
        c.env("FLUXVM_MAC_NOTIFY_WEBHOOK", w);
    }
    detach(&mut c);
    let mut child = c.spawn().context("starting the TTL guard")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Start `mac notify wait NAME` detached: it tells you when an instance that keeps running stops or is removed.
pub fn spawn_notify_wait(r: &crate::remote::Remote, name: &str, t: &Targets) -> Result<()> {
    let mut args = vec!["mac", "notify", "wait", name];
    if !t.desktop {
        args.push("--no-desktop");
    }
    let mut c = child_command(r, &args)?;
    if let Some(w) = &t.webhook {
        c.env("FLUXVM_MAC_NOTIFY_WEBHOOK", w);
    }
    detach(&mut c);
    let mut child = c.spawn().context("starting the notifier")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// ---- daemon entry --------------------------------------------------------------------------------------------

pub async fn run_daemon(r: &crate::remote::Remote, c: MacSched) -> Result<()> {
    let command = match c {
        MacSched::Notify { command } => return mac_notify::run_daemon(r, command).await,
        MacSched::Schedule { command } => command,
    };
    let p = path()?;
    match command {
        SchedCmd::RunNow { name } | SchedCmd::Exec { name } => {
            // Mark it started here too, so `ls` shows it running and a tick does not start a second one.
            ms::update(&p, &name, |s| mark_started(s, now())).map_err(missing)?;
            let ev = run_schedule(r, &name).await?;
            println!("{}: {}", if ev.ok { "ok" } else { "failed" }, ev.detail);
            if ev.ok {
                Ok(())
            } else {
                anyhow::bail!("{}", ev.detail)
            }
        }
        SchedCmd::Tick {
            keep_going, every, ..
        } => {
            let mut sp = RealSpawner { r };
            loop {
                match tick_once(&p, &mut sp, now()) {
                    Ok(lines) => lines.iter().for_each(|l| println!("{l}")),
                    // A schedule file being edited must not end a service loop.
                    Err(e) if keep_going => eprintln!("schedule tick: {e:#}"),
                    Err(e) => return Err(e),
                }
                if !keep_going {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_secs(every.max(5))).await;
            }
        }
        SchedCmd::Guard {
            name,
            notify,
            notify_webhook,
        } => {
            run_guard(
                r,
                &name,
                Targets {
                    desktop: notify,
                    webhook: notify_webhook,
                },
            )
            .await
        }
        _ => anyhow::bail!("internal: a client-side schedule command reached the daemon path"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(name: &str) -> ms::Schedule {
        ms::Schedule {
            name: name.into(),
            cron: "*/5 * * * *".into(),
            image: "img:1".into(),
            cmd: vec![],
            ttl: Some("2h".into()),
            notify: false,
            webhook: None,
            enabled: true,
            created_unix: 1_000_000,
            last_run_unix: None,
            last_status: None,
            last_detail: None,
            run_now: false,
        }
    }

    #[derive(Default)]
    struct Rec {
        started: Vec<String>,
        fail: bool,
    }
    impl Spawner for Rec {
        fn spawn_exec(&mut self, n: &str) -> Result<()> {
            if self.fail {
                anyhow::bail!("no fluxctl");
            }
            self.started.push(n.into());
            Ok(())
        }
    }

    #[test]
    fn tick_starts_due_schedules_once() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        ms::add(&p, s("a"), 1_000_000).unwrap();
        ms::add(&p, s("b"), 1_000_000).unwrap();
        ms::update(&p, "b", |x| x.enabled = false).unwrap();
        let mut sp = Rec::default();
        let out = tick_once(&p, &mut sp, 1_000_000 + 600).unwrap();
        assert_eq!(sp.started, ["a"]);
        assert_eq!(out, ["started a"]);
        // The same minute again: a is running, b is off.
        let mut sp2 = Rec::default();
        tick_once(&p, &mut sp2, 1_000_000 + 601).unwrap();
        assert!(sp2.started.is_empty());
        let a = ms::load(&p).unwrap().schedules.remove(0);
        assert_eq!(a.last_status.as_deref(), Some("running"));
    }

    #[test]
    fn run_now_flag_fires_once_and_a_spawn_failure_is_recorded() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        let mut x = s("a");
        x.cron = "0 0 1 1 *".into();
        ms::add(&p, x, 1_000_000).unwrap();
        ms::update(&p, "a", |x| x.run_now = true).unwrap();
        let mut sp = Rec {
            fail: true,
            ..Default::default()
        };
        let out = tick_once(&p, &mut sp, 1_000_100).unwrap();
        assert!(out[0].contains("could not start a: no fluxctl"));
        let a = ms::load(&p).unwrap().schedules.remove(0);
        assert!(!a.run_now);
        assert_eq!(a.last_status.as_deref(), Some("failed"));
        assert!(a.last_detail.unwrap().contains("no fluxctl"));
    }

    #[test]
    fn dry_run_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        ms::add(&p, s("a"), 1_000_000).unwrap();
        let out = tick_once(&p, &mut Dry, 1_000_000 + 600).unwrap();
        assert_eq!(out, ["would start a"]);
        assert!(ms::load(&p).unwrap().schedules[0].last_run_unix.is_none());
    }

    #[test]
    fn run_arguments() {
        let a = run_args(&s("n"), "n-1");
        assert_eq!(a, ["mac", "run", "--name", "n-1", "--ttl", "2h", "img:1"]);
        let mut c = s("n");
        c.cmd = vec!["./b.sh".into(), "x".into()];
        assert_eq!(
            run_args(&c, "n-1"),
            [
                "mac", "run", "--name", "n-1", "--rm", "img:1", "--", "./b.sh", "x"
            ]
        );
    }

    #[test]
    fn summaries_say_what_to_do_on_failure() {
        let e = summarize(false, "n", false, Some(3), "n-1", true);
        assert!(!e.ok && e.detail.contains("run-now n") && e.detail.contains("mac doctor"));
        assert!(
            summarize(true, "n", true, None, "n-1", true)
                .detail
                .contains("time limit")
        );
        assert!(!summarize(true, "n", true, None, "n-1", true).ok);
        assert!(summarize(true, "n", false, Some(0), "n-1", false).ok);
    }

    struct G {
        expiry: Option<Result<Expiry, ()>>,
        removed: Vec<String>,
        fail_remove: bool,
    }
    impl GuardHost for G {
        async fn expiry(&mut self, _: &str) -> Result<Expiry> {
            match self.expiry.take().unwrap() {
                Ok(e) => Ok(e),
                Err(()) => anyhow::bail!("daemon away"),
            }
        }
        async fn remove(&mut self, n: &str) -> Result<()> {
            if self.fail_remove {
                anyhow::bail!("busy");
            }
            self.removed.push(n.into());
            Ok(())
        }
    }
    fn g(e: Result<Expiry, ()>) -> G {
        G {
            expiry: Some(e),
            removed: vec![],
            fail_remove: false,
        }
    }

    #[tokio::test]
    async fn the_guard_removes_only_after_the_deadline() {
        let mut h = g(Ok(Expiry::At(1000)));
        assert_eq!(guard_step(&mut h, "x", 970).await, GuardStep::Wait(30));
        let mut h = g(Ok(Expiry::At(1000)));
        assert_eq!(guard_step(&mut h, "x", 995).await, GuardStep::Wait(5));
        assert!(h.removed.is_empty());
        let mut h = g(Ok(Expiry::At(1000)));
        assert_eq!(guard_step(&mut h, "x", 1000).await, GuardStep::Removed);
        assert_eq!(h.removed, ["x"]);
    }

    #[tokio::test]
    async fn the_guard_stops_when_there_is_nothing_to_guard_and_survives_a_dead_daemon() {
        assert_eq!(
            guard_step(&mut g(Ok(Expiry::Gone)), "x", 1).await,
            GuardStep::Done
        );
        assert_eq!(
            guard_step(&mut g(Ok(Expiry::Cleared)), "x", 1).await,
            GuardStep::Done
        );
        assert_eq!(
            guard_step(&mut g(Err(())), "x", 1).await,
            GuardStep::Wait(GUARD_POLL)
        );
        let mut h = g(Ok(Expiry::At(1)));
        h.fail_remove = true;
        assert_eq!(
            guard_step(&mut h, "x", 5).await,
            GuardStep::Failed("busy".into())
        );
    }

    #[test]
    fn the_agent_runs_the_loop_and_carries_no_token() {
        let a = agent_args(
            std::path::Path::new("/x/fluxctl"),
            Some("http://h:1"),
            Some("c"),
        );
        assert_eq!(
            a,
            [
                "/x/fluxctl",
                "--server",
                "http://h:1",
                "--context",
                "c",
                "mac",
                "schedule",
                "tick",
                "--loop"
            ]
        );
        assert!(!a.join(" ").to_lowercase().contains("token"));
    }

    #[test]
    fn views_never_carry_the_webhook() {
        let mut x = s("a");
        x.webhook = Some("https://hooks.example.com/T0K3N/secret".into());
        let v = view(&x, 1_000_100).to_string();
        assert!(!v.contains("T0K3N") && v.contains("hooks.example.com"));
    }
}
