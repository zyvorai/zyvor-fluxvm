// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac runner`: stopping cleanly and cleaning up after a crash.
//!
//! * SIGINT, SIGTERM and SIGHUP set a flag. The supervisor stops starting runners, and a watcher deletes every guest and
//!   unregisters every runner this process still has in flight. A second signal exits at once (status 130).
//! * At start, a sweep removes what a previous run left behind: `fleet-*` guests on the Macs and `fluxvm-*` runners on
//!   GitHub that no live lease and no running job accounts for.
//!
//! Not run against GitHub or a real Mac: see docs/mac-runner.md.

use crate::mac_fleet_remote::{PRELUDE, remote};
use crate::mac_fleet_run::delete_instance;
use crate::mac_fleet_ssh::{Target, Transport};
use crate::mac_runner::{Github, NAME_PREFIX};
use serde_json::Value;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// Guest name prefix of `mac-fleet run` and the runner (`fleet-<id>`).
pub const GUEST_PREFIX: &str = "fleet-";

static STOP: AtomicBool = AtomicBool::new(false);
static SIGNALS: AtomicU32 = AtomicU32::new(0);
/// Hook for tests: a signal that is not a stop request is only counted.
static OTHER: AtomicU32 = AtomicU32::new(0);

/// True once a stop was requested by a signal.
pub fn requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

#[cfg(test)]
pub fn reset_for_test() {
    STOP.store(false, Ordering::SeqCst);
    SIGNALS.store(0, Ordering::SeqCst);
}

extern "C" fn on_signal(sig: libc::c_int) {
    // Async-signal-safe only: atomics and _exit.
    if sig == libc::SIGINT || sig == libc::SIGTERM || sig == libc::SIGHUP {
        STOP.store(true, Ordering::SeqCst);
        if SIGNALS.fetch_add(1, Ordering::SeqCst) >= 1 {
            unsafe { libc::_exit(130) };
        }
    } else {
        OTHER.fetch_add(1, Ordering::SeqCst);
    }
}

/// Route SIGINT, SIGTERM and SIGHUP to the stop flag.
pub fn install_signals() {
    install_for(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]);
}

fn install_for(sigs: &[libc::c_int]) {
    for &s in sigs {
        unsafe {
            libc::signal(s, on_signal as *const () as libc::sighandler_t);
        }
    }
}

// ---- what is in flight -----------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Live {
    /// The job id: runner `fluxvm-<id>`, guest `fleet-<id>`.
    pub id: String,
    pub runner_id: Option<u64>,
    pub target: Option<Target>,
}

static LIVE: Mutex<Vec<Live>> = Mutex::new(Vec::new());

fn with_live<R>(f: impl FnOnce(&mut Vec<Live>) -> R) -> R {
    f(&mut LIVE.lock().unwrap_or_else(|e| e.into_inner()))
}

/// A runner was registered for job `id`.
pub fn track_runner(id: &str, runner_id: u64) {
    with_live(|v| {
        v.retain(|l| l.id != id);
        v.push(Live {
            id: id.into(),
            runner_id: Some(runner_id),
            target: None,
        });
    });
}

/// The job's guest is about to be created on `target`.
pub fn track_target(id: &str, target: &Target) {
    with_live(|v| {
        if let Some(l) = v.iter_mut().find(|l| l.id == id) {
            l.target = Some(target.clone());
        }
    });
}

pub fn untrack(id: &str) {
    with_live(|v| v.retain(|l| l.id != id));
}

pub fn live_snapshot() -> Vec<Live> {
    with_live(|v| v.clone())
}

pub fn is_live(id: &str) -> bool {
    with_live(|v| v.iter().any(|l| l.id == id))
}

/// Delete every in-flight guest and unregister every in-flight runner. Returns what it did, one line each.
pub fn shutdown_cleanup(gh: &dyn Github, tr: &dyn Transport, live: &[Live]) -> Vec<String> {
    let mut out = vec![];
    for l in live {
        if let Some(t) = &l.target {
            let name = format!("{GUEST_PREFIX}{}", l.id);
            let ok = delete_instance(tr, t, &name);
            out.push(format!(
                "{name}: {} on {}",
                if ok { "guest deleted" } else { "guest not deleted (gone already or Mac unreachable)" },
                t.label()
            ));
        }
        if let Some(r) = l.runner_id {
            let ok = gh.remove(r).is_ok();
            out.push(format!(
                "{NAME_PREFIX}{}: {}",
                l.id,
                if ok { "runner unregistered" } else { "runner not unregistered (gone already)" }
            ));
        }
    }
    out
}

// ---- the startup sweep -----------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSeen {
    pub target: Target,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSeen {
    pub id: u64,
    pub name: String,
    pub busy: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepPlan {
    pub guests: Vec<GuestSeen>,
    pub runners: Vec<RunnerSeen>,
}

/// Orphans: `fleet-<id>` guests and idle `fluxvm-<id>` runners whose id `keep` does not claim (a live lease of another
/// process, or a job of this one). A busy runner is never touched: it is running a job.
pub fn plan_sweep(guests: &[GuestSeen], runners: &[RunnerSeen], keep: &dyn Fn(&str) -> bool) -> SweepPlan {
    SweepPlan {
        guests: guests
            .iter()
            .filter(|g| g.name.strip_prefix(GUEST_PREFIX).is_some_and(|id| !id.is_empty() && !keep(id)))
            .cloned()
            .collect(),
        runners: runners
            .iter()
            .filter(|r| !r.busy && r.name.strip_prefix(NAME_PREFIX).is_some_and(|id| !id.is_empty() && !keep(id)))
            .cloned()
            .collect(),
    }
}

/// Instances this tool created: named `fleet-*` and labelled `fluxvm.owner=mac-fleet`.
pub fn parse_guests(target: &Target, ps_json: &str) -> Vec<GuestSeen> {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(ps_json.trim()) else {
        return vec![];
    };
    rows.iter()
        .filter_map(|r| {
            let name = r["name"].as_str()?;
            let owner = r["labels"]["fluxvm.owner"].as_str();
            (name.starts_with(GUEST_PREFIX) && owner == Some("mac-fleet")).then(|| GuestSeen {
                target: target.clone(),
                name: name.to_owned(),
            })
        })
        .collect()
}

pub fn list_guests(tr: &dyn Transport, target: &Target) -> Vec<GuestSeen> {
    let script = format!("{PRELUDE}[ -n \"$FLUXCTL\" ] && \"$FLUXCTL\" mac ps -a --json 2>/dev/null\n");
    match tr.run_timed(target, &remote(&script), None, Some(Duration::from_secs(60))) {
        Ok(o) if o.ok() => parse_guests(target, &o.stdout),
        _ => vec![],
    }
}

/// Look, then remove. `keep` claims ids that are somebody's. Returns one line per action.
pub fn sweep(
    gh: &dyn Github,
    tr: &dyn Transport,
    targets: &[Target],
    keep: &dyn Fn(&str) -> bool,
    dry: bool,
) -> Vec<String> {
    let guests: Vec<GuestSeen> = targets.iter().flat_map(|t| list_guests(tr, t)).collect();
    let runners: Vec<RunnerSeen> = gh
        .runners()
        .map(|v| {
            v.into_iter()
                .map(|r| RunnerSeen { id: r.id, name: r.name, busy: r.busy })
                .collect()
        })
        .unwrap_or_default();
    let plan = plan_sweep(&guests, &runners, keep);
    let mut out = vec![];
    for g in &plan.guests {
        if dry {
            out.push(format!("sweep: would delete orphaned guest {} on {}", g.name, g.target.label()));
        } else {
            let ok = delete_instance(tr, &g.target, &g.name);
            out.push(format!(
                "sweep: orphaned guest {} on {} {}",
                g.name,
                g.target.label(),
                if ok { "deleted" } else { "could not be deleted" }
            ));
        }
    }
    for r in &plan.runners {
        if dry {
            out.push(format!("sweep: would unregister orphaned runner {}", r.name));
        } else {
            let ok = gh.remove(r.id).is_ok();
            out.push(format!(
                "sweep: orphaned runner {} {}",
                r.name,
                if ok { "unregistered" } else { "could not be unregistered" }
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_fleet_ssh::fake::{FakeTransport, ok};
    use crate::mac_runner::{Jit, RunnerInfo};
    use anyhow::Result;

    struct Gh {
        runners: Vec<RunnerInfo>,
        removed: Mutex<Vec<u64>>,
    }
    impl Github for Gh {
        fn queued(&self, _: &[String]) -> Result<Option<usize>> {
            Ok(Some(0))
        }
        fn busy(&self) -> Result<usize> {
            Ok(0)
        }
        fn jit(&self, _: &str, _: &[String]) -> Result<Jit> {
            unreachable!()
        }
        fn remove(&self, id: u64) -> Result<()> {
            self.removed.lock().unwrap().push(id);
            Ok(())
        }
        fn runners(&self) -> Result<Vec<RunnerInfo>> {
            Ok(self.runners.clone())
        }
    }

    fn t() -> Target {
        Target::parse("admin@mini-1.local").unwrap()
    }

    #[test]
    fn signals_set_the_flag_and_only_stop_signals_do() {
        reset_for_test();
        install_for(&[libc::SIGUSR2]);
        let before = OTHER.load(Ordering::SeqCst);
        unsafe { libc::raise(libc::SIGUSR2) };
        assert!(OTHER.load(Ordering::SeqCst) > before);
        assert!(!requested());
        // The same handler, called as the OS would for a stop signal.
        on_signal(libc::SIGTERM);
        assert!(requested());
        reset_for_test();
    }

    #[test]
    fn the_sweep_takes_orphans_and_spares_busy_runners_and_claimed_ids() {
        let ps = r#"[
          {"name":"fleet-aaa","labels":{"fluxvm.owner":"mac-fleet"},"status":"running"},
          {"name":"fleet-bbb","labels":{"fluxvm.owner":"mac-fleet"},"status":"running"},
          {"name":"fleet-ccc","labels":{},"status":"running"},
          {"name":"my-vm","labels":{"fluxvm.owner":"mac-fleet"},"status":"running"}]"#;
        let tr = FakeTransport::new(move |_, c| if c.contains("mac ps") { ok(ps) } else { ok("") });
        let gh = Gh {
            runners: vec![
                RunnerInfo { id: 1, name: "fluxvm-aaa".into(), busy: false, status: "offline".into() },
                RunnerInfo { id: 2, name: "fluxvm-bbb".into(), busy: true, status: "online".into() },
                RunnerInfo { id: 3, name: "fluxvm-zzz".into(), busy: false, status: "online".into() },
                RunnerInfo { id: 4, name: "somebody-else".into(), busy: false, status: "online".into() },
            ],
            removed: Default::default(),
        };
        // zzz belongs to another supervisor's live lease
        let lines = sweep(&gh, &tr, &[t()], &|id| id == "zzz", false);
        assert_eq!(*gh.removed.lock().unwrap(), vec![1], "idle orphan only; busy, claimed and foreign runners stay");
        let calls = tr.calls_for("mini-1.local");
        let rms: Vec<_> = calls.iter().filter(|c| c.command.contains("mac rm")).collect();
        assert_eq!(rms.len(), 2, "fleet-aaa and fleet-bbb; not unlabelled fleet-ccc, not my-vm: {lines:?}");
        assert!(lines.iter().all(|l| l.starts_with("sweep:")));
        // a dry sweep changes nothing
        gh.removed.lock().unwrap().clear();
        let n = tr.calls.lock().unwrap().len();
        let lines = sweep(&gh, &tr, &[t()], &|_| false, true);
        assert!(lines.iter().all(|l| l.contains("would")));
        assert!(gh.removed.lock().unwrap().is_empty());
        assert_eq!(tr.calls.lock().unwrap().len(), n + 1, "only the listing");
    }

    #[test]
    fn shutdown_deletes_guests_and_unregisters_runners() {
        let tr = FakeTransport::new(|_, _| ok(""));
        let gh = Gh { runners: vec![], removed: Default::default() };
        track_runner("job1", 11);
        track_target("job1", &t());
        track_runner("job2", 12); // never placed: only the runner goes
        let live: Vec<Live> = live_snapshot().into_iter().filter(|l| l.id.starts_with("job")).collect();
        let lines = shutdown_cleanup(&gh, &tr, &live);
        assert_eq!(*gh.removed.lock().unwrap(), vec![11, 12]);
        let calls = tr.calls_for("mini-1.local");
        assert_eq!(calls.iter().filter(|c| c.command.contains("mac rm") && c.command.contains("fleet-job1")).count(), 1);
        assert_eq!(lines.len(), 3);
        untrack("job1");
        untrack("job2");
        assert!(!is_live("job1"));
    }
}
