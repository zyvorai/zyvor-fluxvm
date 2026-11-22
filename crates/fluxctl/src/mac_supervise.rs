// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac supervise`: enforce what `mac run --restart` only recorded. The daemon never restarts a macOS VM, so this loop does:
//! each tick it lists the instances, starts those whose restart policy says they should be running (with a back-off so a VM that
//! dies at boot is not restarted in a tight loop), runs the healthchecks that are due, and reaps instances whose TTL passed.
//! `supervise --install` writes a LaunchAgent (no sudo) that keeps the loop running; `--uninstall` and `--status` complete it.
//!
//! The tick is generic over [`Host`] and unit tested with a fake; the real host talks to the daemon. Never run against a real daemon.

use crate::mac_health::{self, Health, Status};
use crate::mac_inspect as view;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

pub const AGENT_LABEL: &str = "dev.zyvor.fluxvm.mac-supervise";

/// What the supervisor asks the outside world.
pub(crate) trait Host {
    async fn list(&mut self) -> Result<Vec<Value>>;
    async fn start(&mut self, vm: &Value) -> Result<()>;
    async fn delete(&mut self, vm: &Value) -> Result<()>;
    /// Run the healthcheck when it is due and record the result; `None` when there is none, it is not due, or the VM cannot be probed.
    async fn check_health(&mut self, vm: &Value, now: u64) -> Result<Option<Health>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Started(String),
    StartFailed(String, String),
    Backoff(String, u64),
    Health(String, Status),
    Reaped(String),
    ReapFailed(String, String),
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Started(n) => write!(f, "started {n}"),
            Event::StartFailed(n, e) => write!(f, "could not start {n}: {e}"),
            Event::Backoff(n, s) => write!(f, "{n}: waiting {s}s before the next restart"),
            Event::Health(n, s) => write!(f, "{n}: {}", s.as_str()),
            Event::Reaped(n) => write!(f, "reaped {n} (ttl passed)"),
            Event::ReapFailed(n, e) => write!(f, "could not reap {n}: {e}"),
        }
    }
}

/// Exponential back-off per instance: 5 s, 10 s, ... capped at 5 minutes. Reset when the instance is seen running.
#[derive(Debug, Default)]
pub struct Backoff {
    state: BTreeMap<String, (u32, u64)>,
}

impl Backoff {
    pub const BASE: u64 = 5;
    pub const CAP: u64 = 300;

    /// Seconds still to wait before `name` may be restarted (0 = go).
    pub fn wait_left(&self, name: &str, now: u64) -> u64 {
        self.state
            .get(name)
            .map_or(0, |(_, at)| at.saturating_sub(now))
    }

    pub fn record_attempt(&mut self, name: &str, now: u64) {
        let e = self.state.entry(name.to_string()).or_insert((0, 0));
        e.0 += 1;
        let delay = (Self::BASE << (e.0 - 1).min(10)).min(Self::CAP);
        e.1 = now + delay;
    }

    pub fn reset(&mut self, name: &str) {
        self.state.remove(name);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TickOpts {
    pub dry_run: bool,
    pub reap: bool,
}

/// One pass over all instances.
pub(crate) async fn tick<H: Host>(
    h: &mut H,
    backoff: &mut Backoff,
    now: u64,
    o: TickOpts,
) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    for vm in h.list().await? {
        let name = vm["name"].as_str().unwrap_or("?").to_string();
        if o.reap && crate::mac_reap::expired(&vm, now) {
            if o.dry_run {
                events.push(Event::Reaped(format!("{name} (dry run)")));
            } else {
                events.push(match h.delete(&vm).await {
                    Ok(()) => Event::Reaped(name.clone()),
                    Err(e) => Event::ReapFailed(name.clone(), format!("{e:#}")),
                });
            }
            continue;
        }
        if view::status(&vm) == "running" {
            backoff.reset(&name);
        } else if crate::mac_more::should_restart(&vm) {
            let left = backoff.wait_left(&name, now);
            if left > 0 {
                events.push(Event::Backoff(name.clone(), left));
            } else if o.dry_run {
                events.push(Event::Started(format!("{name} (dry run)")));
            } else {
                backoff.record_attempt(&name, now);
                events.push(match h.start(&vm).await {
                    Ok(()) => Event::Started(name.clone()),
                    Err(e) => Event::StartFailed(name.clone(), format!("{e:#}")),
                });
            }
            continue;
        }
        if !o.dry_run
            && let Some(health) = h.check_health(&vm, now).await?
        {
            let before = mac_health::from_labels(&vm).map(|p| p.status);
            if before != Some(health.status) {
                events.push(Event::Health(name, health.status));
            }
        }
    }
    Ok(events)
}

pub(crate) struct DaemonHost<'a> {
    pub r: &'a crate::remote::Remote,
}

impl Host for DaemonHost<'_> {
    async fn list(&mut self) -> Result<Vec<Value>> {
        self.r.list_vms(Some(crate::mac_cmd::INSTANCE_SELECTOR)).await
    }

    async fn start(&mut self, vm: &Value) -> Result<()> {
        let id = vm["id"].as_str().context("no id")?;
        self.r
            .call(reqwest::Method::POST, &format!("/v1/vms/{id}/start"), None)
            .await
            .map(|_| ())
    }

    async fn delete(&mut self, vm: &Value) -> Result<()> {
        crate::mac_cmd::delete_instance(self.r, vm["id"].as_str().context("no id")?).await
    }

    async fn check_health(&mut self, vm: &Value, now: u64) -> Result<Option<Health>> {
        let Some(hc) = mac_health::healthcheck_of(vm) else {
            return Ok(None);
        };
        if !mac_health::due(mac_health::from_labels(vm).as_ref(), &hc, now) {
            return Ok(None);
        }
        let user = mac_health::user_of(vm);
        let Some(h) = mac_health::probe_vm(&mut mac_health::SshProber, vm, &hc, &user, now).await
        else {
            return Ok(None);
        };
        let id = vm["id"].as_str().context("no id")?;
        self.r
            .call(
                reqwest::Method::PATCH,
                &format!("/v1/vms/{id}"),
                Some(mac_health::labels_patch(&h)),
            )
            .await?;
        Ok(Some(h))
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct SuperviseArgs {
    /// Seconds between passes.
    #[arg(long, default_value_t = 15, value_name = "SECS")]
    pub interval: u64,
    /// One pass, then exit (for cron or a test).
    #[arg(long)]
    pub once: bool,
    /// Say what would be done and change nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Do not reap instances whose TTL passed.
    #[arg(long)]
    pub no_reap: bool,
    /// Write and load a LaunchAgent that runs this loop at login and restarts it. Carries `--server`, `--context` and `$FLUXVM_URL`;
    /// never a token (use a context, which keeps its token in the keychain or a file).
    #[arg(long, conflicts_with_all = ["uninstall", "status"])]
    pub install: bool,
    /// Unload the LaunchAgent and delete its plist.
    #[arg(long, conflicts_with = "status")]
    pub uninstall: bool,
    /// Report whether the LaunchAgent is installed and loaded.
    #[arg(long)]
    pub status: bool,
    /// For `--install`: the daemon URL to write into the agent's arguments.
    #[arg(long = "daemon", value_name = "URL")]
    pub daemon: Option<String>,
    /// For `--install`: the context to write into the agent's arguments.
    #[arg(long = "daemon-context", value_name = "NAME")]
    pub daemon_context: Option<String>,
}

pub fn agent_paths(home: &std::path::Path) -> (PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents")
            .join(format!("{AGENT_LABEL}.plist")),
        home.join("Library/Logs/fluxvm-mac-supervise.log"),
    )
}

/// The agent's command line.
pub fn agent_args(
    exe: &std::path::Path,
    a: &SuperviseArgs,
) -> Vec<String> {
    let mut v = vec![exe.display().to_string()];
    if let Some(d) = &a.daemon {
        v.extend(["--server".into(), d.clone()]);
    }
    if let Some(c) = &a.daemon_context {
        v.extend(["--context".into(), c.clone()]);
    }
    v.extend(["mac".into(), "supervise".into()]);
    v.extend(["--interval".into(), a.interval.to_string()]);
    if a.no_reap {
        v.push("--no-reap".into());
    }
    v
}

/// `--install`, `--uninstall`, `--status`: nothing to do with the daemon.
pub fn agent_command(a: &SuperviseArgs) -> Result<Option<Value>> {
    use crate::docker_host as dh;
    let home = dh::home()?;
    let (plist, log) = agent_paths(&home);
    if a.install {
        let exe = crate::stable_exe::stable_exe().context("finding fluxctl's own path")?;
        let args = agent_args(&exe, a);
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
        return Ok(Some(json!({"installed": plist, "log": log})));
    }
    if a.uninstall {
        let removed = dh::launchctl_unload(AGENT_LABEL, &plist)?;
        return Ok(Some(json!({"removed": removed})));
    }
    if a.status {
        return Ok(Some(crate::launch_agent::status_of(AGENT_LABEL, &plist, &log)?));
    }
    Ok(None)
}

/// `mac supervise` with the daemon.
pub async fn run(r: &crate::remote::Remote, a: SuperviseArgs) -> Result<()> {
    let mut host = DaemonHost { r };
    let mut backoff = Backoff::default();
    let o = TickOpts {
        dry_run: a.dry_run,
        reap: !a.no_reap,
    };
    loop {
        match tick(&mut host, &mut backoff, mac_health::now_unix(), o).await {
            Ok(events) => {
                for e in events {
                    println!("{e}");
                }
            }
            // A daemon that is briefly away must not end a service loop.
            Err(e) if !a.once => eprintln!("supervise: {e:#}"),
            Err(e) => return Err(e),
        }
        if a.once {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(a.interval.max(1))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        vms: Vec<Value>,
        started: Vec<String>,
        deleted: Vec<String>,
        fail_start: bool,
        health: Option<Health>,
    }

    impl Host for Fake {
        async fn list(&mut self) -> Result<Vec<Value>> {
            Ok(self.vms.clone())
        }
        async fn start(&mut self, vm: &Value) -> Result<()> {
            if self.fail_start {
                anyhow::bail!("409 no room");
            }
            self.started.push(vm["name"].as_str().unwrap().into());
            Ok(())
        }
        async fn delete(&mut self, vm: &Value) -> Result<()> {
            self.deleted.push(vm["name"].as_str().unwrap().into());
            Ok(())
        }
        async fn check_health(&mut self, _: &Value, _: u64) -> Result<Option<Health>> {
            Ok(self.health.clone())
        }
    }

    fn vm(name: &str, status: &str, policy: &str) -> Value {
        json!({"name": name, "id": name, "status": status, "labels": {"fluxvm.mac.restart": policy}})
    }

    #[tokio::test]
    async fn a_stopped_always_instance_is_restarted_and_backed_off() {
        let mut h = Fake {
            vms: vec![vm("a", "stopped", "always"), vm("b", "stopped", "no")],
            ..Default::default()
        };
        let mut b = Backoff::default();
        let ev = tick(&mut h, &mut b, 100, TickOpts::default()).await.unwrap();
        assert_eq!(ev, [Event::Started("a".into())]);
        assert_eq!(h.started, ["a"]);
        // The next pass is inside the back-off window.
        let ev = tick(&mut h, &mut b, 102, TickOpts::default()).await.unwrap();
        assert_eq!(ev, [Event::Backoff("a".into(), 3)]);
        assert_eq!(h.started.len(), 1);
        let ev = tick(&mut h, &mut b, 105, TickOpts::default()).await.unwrap();
        assert_eq!(ev, [Event::Started("a".into())]);
    }

    #[tokio::test]
    async fn backoff_doubles_and_resets_when_running() {
        let mut b = Backoff::default();
        b.record_attempt("a", 0);
        assert_eq!(b.wait_left("a", 0), 5);
        b.record_attempt("a", 5);
        assert_eq!(b.wait_left("a", 5), 10);
        for _ in 0..20 {
            b.record_attempt("a", 0);
        }
        assert_eq!(b.wait_left("a", 0), Backoff::CAP);
        let mut h = Fake {
            vms: vec![vm("a", "running", "always")],
            ..Default::default()
        };
        tick(&mut h, &mut b, 1, TickOpts::default()).await.unwrap();
        assert_eq!(b.wait_left("a", 1), 0);
    }

    #[tokio::test]
    async fn unless_stopped_leaves_a_manual_stop_alone_and_failures_are_reported() {
        let mut manual = vm("m", "stopped", "unless-stopped");
        manual["labels"]["fluxvm.mac.stopped"] = json!("true");
        let mut h = Fake {
            vms: vec![manual, vm("c", "failed", "on-failure")],
            fail_start: true,
            ..Default::default()
        };
        let ev = tick(&mut h, &mut Backoff::default(), 0, TickOpts::default())
            .await
            .unwrap();
        assert_eq!(ev.len(), 1);
        assert!(matches!(&ev[0], Event::StartFailed(n, e) if n == "c" && e.contains("409")));
    }

    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let mut h = Fake {
            vms: vec![vm("a", "stopped", "always")],
            ..Default::default()
        };
        let o = TickOpts {
            dry_run: true,
            reap: true,
        };
        let ev = tick(&mut h, &mut Backoff::default(), 0, o).await.unwrap();
        assert_eq!(ev.len(), 1);
        assert!(h.started.is_empty());
    }

    #[tokio::test]
    async fn a_health_change_is_reported_once() {
        let mut v = vm("a", "running", "no");
        let mut h = Fake {
            vms: vec![v.clone()],
            health: Some(Health {
                status: Status::Healthy,
                streak: 0,
                checked_unix: 1,
            }),
            ..Default::default()
        };
        let ev = tick(&mut h, &mut Backoff::default(), 1, TickOpts::default())
            .await
            .unwrap();
        assert_eq!(ev, [Event::Health("a".into(), Status::Healthy)]);
        v["labels"]["fluxvm.mac.health"] = json!("healthy");
        h.vms = vec![v];
        let ev = tick(&mut h, &mut Backoff::default(), 2, TickOpts::default())
            .await
            .unwrap();
        assert!(ev.is_empty());
    }

    #[test]
    fn the_agent_args_carry_the_daemon_and_no_token() {
        let a = SuperviseArgs {
            interval: 20,
            once: false,
            dry_run: false,
            no_reap: true,
            install: true,
            uninstall: false,
            status: false,
            daemon: Some("http://127.0.0.1:7788".into()),
            daemon_context: None,
        };
        let v = agent_args(std::path::Path::new("/opt/fluxctl"), &a);
        assert_eq!(
            v,
            [
                "/opt/fluxctl", "--server", "http://127.0.0.1:7788", "mac", "supervise", "--interval", "20",
                "--no-reap"
            ]
        );
        assert!(!v.iter().any(|s| s.contains("token")));
    }
}
