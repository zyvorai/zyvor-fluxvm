// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The `VmManager` side of `run_strategy`, `on_crash` and `power_schedule`: the
//! glue that calls the pure logic in `supervisor` and `power_schedule` and acts
//! on the answer. Kept apart from `lib.rs`, which only calls the four entry
//! points below from `reconcile()`, the reaper tick and `start_autostart_vms`.

use crate::power_schedule::{self, PowerAction};
use crate::supervisor::{self, Decision, ExitReason};
use crate::{VmManager, events};
use chrono::{DateTime, Utc};
use fluxvm_core::lifecycle::RunStrategy;
use fluxvm_core::model::{BackendKind, VmRecord, VmStatus};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use uuid::Uuid;

/// VMs that already have a crash-event watcher task in this process.
fn watched() -> &'static Mutex<HashSet<Uuid>> {
    static W: OnceLock<Mutex<HashSet<Uuid>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// When each VM's power schedule was last evaluated.
fn power_checked() -> &'static Mutex<HashMap<Uuid, DateTime<Utc>>> {
    static W: OnceLock<Mutex<HashMap<Uuid, DateTime<Utc>>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

impl VmManager {
    /// Exit hook, called by `reconcile()` for a VM whose hypervisor process
    /// died without a stop request (`vm` is the record after it was marked
    /// Stopped). Decides, persists the decision and records events; the
    /// restart itself happens in [`Self::run_pending_restarts`].
    pub(crate) async fn on_vm_exit(&self, vm: &VmRecord) {
        if vm.request.run_strategy.is_none() && vm.request.on_crash.is_none() {
            return;
        }
        let now = Utc::now();
        let state_dir = &self.cfg.state_dir;
        let state = supervisor::load(state_dir, vm.id);
        let reason = supervisor::classify(&state, now);
        let (decision, next) = supervisor::decide(
            vm.request.run_strategy,
            vm.request.on_crash,
            reason,
            state,
            now,
            &supervisor::Policy::default(),
        );
        let id = vm.id.to_string();
        let strategy = vm.request.run_strategy.map_or("unset", RunStrategy::as_str);
        let mut fields = vec![
            ("vm_id", id.as_str()),
            ("reason", reason.as_str()),
            ("strategy", strategy),
        ];
        match decision {
            Decision::Stay(why) => {
                // No strategy is the long-standing behaviour: no event noise.
                if why != supervisor::StayReason::NoStrategy {
                    fields.push(("outcome", why.as_str()));
                    events::record("vm.exited", &fields);
                }
            }
            Decision::RestartAfter { delay, attempt } => {
                let (d, a) = (delay.num_seconds().to_string(), attempt.to_string());
                fields.extend([("delay_secs", d.as_str()), ("attempt", a.as_str())]);
                events::record("vm.restart-scheduled", &fields);
            }
            Decision::GiveUp { restarts } => {
                let r = restarts.to_string();
                fields.push(("restarts", r.as_str()));
                events::record("vm.crash-loop", &fields);
                tracing::warn!(vm = %vm.id, restarts, "VM is crash-looping; no more automatic restarts");
                if let Some(mut failed) = self.store.get(vm.id).await {
                    failed.status = VmStatus::Failed;
                    failed.error = Some(format!(
                        "crash loop: restarted {restarts} times without a stable run; \
                         run_strategy gave up"
                    ));
                    let _ = self.store.update(failed).await;
                }
            }
        }
        if let Err(e) = supervisor::save(state_dir, vm.id, &next) {
            tracing::warn!(vm = %vm.id, error = %e, "saving supervisor state failed");
        }
    }

    /// Reaper tick: start every VM whose backoff has passed.
    pub async fn run_pending_restarts(self: &Arc<Self>) {
        let now = Utc::now();
        for (id, mut state) in supervisor::pending(&self.cfg.state_dir) {
            let Some(vm) = self.store.get(id).await else {
                supervisor::forget(&self.cfg.state_dir, id);
                continue;
            };
            if vm.status != VmStatus::Stopped {
                // Started by hand (or deleted/failed) in the meantime.
                state.restart_at = None;
                if matches!(vm.status, VmStatus::Running | VmStatus::Paused) {
                    state.last_started_at = Some(now);
                }
                let _ = supervisor::save(&self.cfg.state_dir, id, &state);
                continue;
            }
            if !supervisor::restart_due(&state, now) {
                continue;
            }
            // Clear first so a failing start is not retried every tick; the
            // next exit (or the failed start below) decides again.
            state.restart_at = None;
            state.last_started_at = Some(now);
            let _ = supervisor::save(&self.cfg.state_dir, id, &state);
            let sid = id.to_string();
            match self.start(id).await {
                Ok(_) => events::record("vm.restarted", &[("vm_id", &sid)]),
                Err(e) => {
                    tracing::warn!(vm = %id, error = %e, "run_strategy restart failed");
                    events::record(
                        "vm.restart-failed",
                        &[("vm_id", &sid), ("error", &format!("{e:#}"))],
                    );
                    // A VM that cannot start counts as one more failed run.
                    self.on_vm_exit(&vm).await;
                }
            }
        }
    }

    /// Reaper tick: start the crash-event watcher for each running QEMU VM
    /// with `on_crash`, once per process.
    pub async fn ensure_crash_watchers(self: &Arc<Self>) {
        for vm in self.store.list().await {
            let Some(on_crash) = vm.request.on_crash else {
                continue;
            };
            if vm.backend != BackendKind::Qemu || vm.status != VmStatus::Running {
                continue;
            }
            if !watched().lock().unwrap().insert(vm.id) {
                continue;
            }
            let me = self.clone();
            tokio::spawn(async move {
                let sock = fluxvm_qemu::crash_watch::event_socket_path(&me.cfg, vm.id);
                let id = vm.id.to_string();
                let state_dir = me.cfg.state_dir.clone();
                let vm_id = vm.id;
                let res = fluxvm_qemu::crash_watch::watch(&sock, on_crash, &vm.workspace, |ev| {
                    use fluxvm_qemu::crash_watch::CrashEvent;
                    events::record(
                        ev.event_name(),
                        &[
                            ("vm_id", &id),
                            ("action", ev.action()),
                            ("on_crash", on_crash.as_str()),
                        ],
                    );
                    let reason = match ev {
                        CrashEvent::Panic { .. } => ExitReason::GuestPanic,
                        CrashEvent::Watchdog { .. } => ExitReason::Watchdog,
                        CrashEvent::GuestShutdown { .. } => ExitReason::Clean,
                    };
                    supervisor::note_crash(&state_dir, vm_id, reason, Utc::now());
                })
                .await;
                if let Err(e) = res {
                    tracing::debug!(vm = %vm_id, error = %e, "crash watcher ended");
                }
                // The VM stopped or the connection dropped; a later start gets a new watcher.
                watched().lock().unwrap().remove(&vm_id);
            });
        }
    }

    /// Reaper tick: apply `power_schedule` start/stop times that fell since the
    /// previous tick.
    pub async fn run_power_schedules(self: &Arc<Self>) {
        let now = Utc::now();
        let default_window = chrono::Duration::seconds(self.cfg.reaper_interval_secs.max(1) as i64);
        for vm in self.store.list().await {
            let Some(sched) = power_schedule::effective(&vm) else {
                continue;
            };
            let since = power_checked()
                .lock()
                .unwrap()
                .insert(vm.id, now)
                .unwrap_or(now - default_window);
            match power_schedule::decide(&sched, vm.status, since, now) {
                PowerAction::None => {}
                PowerAction::Start => {
                    let id = vm.id.to_string();
                    match self.start(vm.id).await {
                        Ok(_) => events::record(
                            "vm.power-schedule",
                            &[("vm_id", &id), ("action", "start")],
                        ),
                        Err(e) => tracing::warn!(vm = %vm.id, error = %e, "scheduled start failed"),
                    }
                }
                PowerAction::Stop => {
                    let id = vm.id.to_string();
                    match self.stop(vm.id).await {
                        Ok(_) => events::record(
                            "vm.power-schedule",
                            &[("vm_id", &id), ("action", "stop")],
                        ),
                        Err(e) => tracing::warn!(vm = %vm.id, error = %e, "scheduled stop failed"),
                    }
                }
            }
        }
    }
}
