// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl kube agent node install|uninstall|status|run`: one LaunchAgent per worker VM, so a worker that stops (a crash, a
//! manual stop, a reboot) comes back without the cluster supervisor having to be installed or running.
//!
//! This is deliberately small and cautious, because two things that touch one cluster's VMs have destroyed a test cluster's
//! disk before (see `SuperviseLock`):
//! * it only ever **starts** a worker that exists and is not running. It never creates, stops, sleeps, wakes or deletes;
//! * it does nothing while the cluster supervisor holds the cluster lock, while the cluster is asleep or going to sleep, or
//!   while the login session is locked (restoring saved state needs an unlocked session, `deploy/launchd-notes.md`).
//!
//! Not run on hardware: the decision (`decide`) and the plist are tested with fakes.

use crate::docker_host as dh;
use crate::kube_host::{self as kh, KubeOptions};
use crate::run::VmApi;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(clap::Subcommand)]
pub enum NodeAgentCommand {
    /// Write ~/Library/LaunchAgents/dev.zyvor.fluxvm.kube.<name>-w<N>.plist and load it.
    Install(NodeArgs),
    /// Unload and delete it.
    Uninstall(NodeArgs),
    /// Whether it is installed and loaded, its pid and last exit code, as JSON.
    Status(NodeArgs),
    /// What the agent runs: keep this worker started. Not meant to be run by hand.
    Run {
        #[command(flatten)]
        node: NodeArgs,
        /// Seconds between checks.
        #[arg(long, default_value_t = 15)]
        every: u64,
    },
}

#[derive(clap::Args, Clone, Debug)]
pub struct NodeArgs {
    /// The cluster's name (the `--name` of `kube up`).
    #[arg(long, default_value = kh::DEFAULT_NAME)]
    pub name: String,
    /// Which worker: 1 for `<name>-w1`.
    #[arg(long)]
    pub worker: u8,
}

impl NodeArgs {
    pub fn validate(&self) -> Result<()> {
        if !kh::name_ok(&self.name) {
            bail!(
                "--name {:?} must be 1 to 40 lowercase letters, digits or dashes",
                self.name
            );
        }
        if self.worker == 0 || self.worker > kh::MAX_WORKERS {
            bail!("--worker must be 1 to {}", kh::MAX_WORKERS);
        }
        Ok(())
    }
    fn options(&self) -> KubeOptions {
        KubeOptions {
            name: self.name.clone(),
            ..Default::default()
        }
    }
    pub fn label(&self) -> String {
        format!("{}.{}-w{}", kh::AGENT_LABEL, self.name, self.worker)
    }
}

fn paths(home: &Path, n: &NodeArgs) -> (PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents")
            .join(format!("{}.plist", n.label())),
        home.join("Library/Logs")
            .join(format!("fluxvm-kube-{}-w{}.log", n.name, n.worker)),
    )
}

pub fn render(fluxctl: &Path, n: &NodeArgs, log: &Path, env: &[(String, String)]) -> String {
    let args = vec![
        fluxctl.display().to_string(),
        "kube".into(),
        "agent".into(),
        "node".into(),
        "run".into(),
        "--name".into(),
        n.name.clone(),
        "--worker".into(),
        n.worker.to_string(),
    ];
    dh::render_plist_with(&n.label(), &args, log, env, &dh::PlistOpts::default())
}

pub fn install(n: &NodeArgs) -> Result<PathBuf> {
    n.validate()?;
    let (plist, log) = paths(&dh::home()?, n);
    let exe = crate::stable_exe::stable_exe()?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(&plist, render(&exe, n, &log, &dh::carried_env()))?;
    dh::launchctl_load(&n.label(), &plist)?;
    Ok(plist)
}

pub fn status(n: &NodeArgs) -> Result<serde_json::Value> {
    n.validate()?;
    let (plist, log) = paths(&dh::home()?, n);
    crate::launch_agent::status_of(&n.label(), &plist, &log)
}

pub fn uninstall(n: &NodeArgs) -> Result<bool> {
    n.validate()?;
    let (plist, _) = paths(&dh::home()?, n);
    dh::launchctl_unload(&n.label(), &plist)
}

/// What one check of the worker found.
pub struct Facts {
    /// The cluster supervisor (`kube supervise`) holds the cluster lock: it owns every VM of the cluster.
    pub supervisor_running: bool,
    pub asleep: bool,
    pub sleep_in_progress: bool,
    pub session_locked: bool,
    /// The worker VM's id and status, if it exists.
    pub worker: Option<(uuid::Uuid, String)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Nothing to do, and here is why (printed once per change by the loop).
    Wait(&'static str),
    Start(uuid::Uuid),
}

pub fn decide(f: &Facts) -> Action {
    if f.supervisor_running {
        return Action::Wait("the cluster supervisor is running and owns the VMs");
    }
    if f.asleep || f.sleep_in_progress {
        return Action::Wait("the cluster is asleep or going to sleep");
    }
    match &f.worker {
        None => Action::Wait("the worker VM does not exist (create it with `kube up --workers`)"),
        Some((_, s)) if s == "running" => Action::Nothing,
        Some(_) if f.session_locked => {
            Action::Wait("the session is locked: restoring saved state needs it unlocked")
        }
        Some((id, _)) => Action::Start(*id),
    }
}

pub async fn run<A: VmApi>(api: &A, n: &NodeArgs, every: Duration) -> Result<()> {
    n.validate()?;
    let o = n.options();
    let wname = kh::worker_name(&o, n.worker);
    let mut last: Option<&'static str> = None;
    loop {
        let mut told = false;
        let facts = Facts {
            supervisor_running: kh::SuperviseLock::is_held_in(&kh::run_dir(), &o.name),
            asleep: crate::kube_checkpoint::is_asleep(api, &o).await,
            sleep_in_progress: kh::sleep_in_progress(&o),
            session_locked: kh::restore_blocked(&mut told).await,
            worker: api.find(&wname).await.ok().flatten(),
        };
        match decide(&facts) {
            Action::Nothing => last = None,
            Action::Wait(why) => {
                if last != Some(why) {
                    eprintln!("{wname}: waiting: {why}");
                    last = Some(why);
                }
            }
            Action::Start(id) => {
                last = None;
                match api.start(id).await {
                    Ok(()) => eprintln!("{wname}: was not running, started it"),
                    Err(e) => eprintln!("{wname}: could not start, will retry: {e:#}"),
                }
            }
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn facts(status: Option<&str>) -> Facts {
        Facts {
            supervisor_running: false,
            asleep: false,
            sleep_in_progress: false,
            session_locked: false,
            worker: status.map(|s| (Uuid::nil(), s.to_owned())),
        }
    }

    #[test]
    fn a_stopped_worker_is_started_and_a_running_one_is_left_alone() {
        assert_eq!(decide(&facts(Some("stopped"))), Action::Start(Uuid::nil()));
        assert_eq!(decide(&facts(Some("running"))), Action::Nothing);
    }

    #[test]
    fn it_never_acts_beside_a_supervisor_or_a_sleeping_cluster() {
        let mut f = facts(Some("stopped"));
        f.supervisor_running = true;
        assert!(matches!(decide(&f), Action::Wait(_)));
        let mut f = facts(Some("stopped"));
        f.asleep = true;
        assert!(matches!(decide(&f), Action::Wait(_)));
        let mut f = facts(Some("stopped"));
        f.sleep_in_progress = true;
        assert!(matches!(decide(&f), Action::Wait(_)));
    }

    #[test]
    fn a_locked_session_blocks_a_start_and_a_missing_worker_is_not_created() {
        let mut f = facts(Some("stopped"));
        f.session_locked = true;
        assert!(matches!(decide(&f), Action::Wait(w) if w.contains("locked")));
        assert!(matches!(decide(&facts(None)), Action::Wait(w) if w.contains("does not exist")));
    }

    #[test]
    fn the_label_and_plist_are_per_node() {
        let n = NodeArgs {
            name: "dev".into(),
            worker: 2,
        };
        assert_eq!(n.label(), "dev.zyvor.fluxvm.kube.dev-w2");
        let p = render(Path::new("/x/fluxctl"), &n, Path::new("/l"), &[]);
        assert!(p.contains("<string>dev.zyvor.fluxvm.kube.dev-w2</string>"));
        assert!(p.contains("<string>node</string>") && p.contains("<string>--worker</string>"));
    }

    #[test]
    fn bad_names_and_worker_numbers_are_refused() {
        assert!(
            NodeArgs {
                name: "dev".into(),
                worker: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            NodeArgs {
                name: "dev".into(),
                worker: 99
            }
            .validate()
            .is_err()
        );
        assert!(
            NodeArgs {
                name: "Bad Name".into(),
                worker: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            NodeArgs {
                name: "dev".into(),
                worker: 1
            }
            .validate()
            .is_ok()
        );
    }
}
