// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! VM run strategy: what to do when a running VM's hypervisor process is gone.
//!
//! `reconcile()` is the exit hook. It already notices a Running or Paused VM
//! whose process died and marks it Stopped (an API or CLI stop clears the pid
//! first, so only unrequested exits reach it). [`decide`] is the pure part:
//! given the strategy, why the VM exited and the restart history it returns
//! whether to restart and how long to wait, with exponential backoff and a
//! cap on crash loops. The history is kept per VM in
//! `<state_dir>/supervisor/<id>.json` so a daemon restart does not forget a
//! pending restart. `VmManager::run_pending_restarts` (reaper tick) performs
//! due restarts; nothing here starts a VM.

use chrono::{DateTime, Duration, Utc};
use fluxvm_core::lifecycle::{OnCrash, RunStrategy};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Why the hypervisor process went away, as far as the host can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitReason {
    /// The process vanished and nothing recorded why (the reaper only sees
    /// that the pid is gone). Counts as a failure.
    Unknown,
    /// A guest-initiated clean shutdown was recorded before the exit.
    Clean,
    /// The guest panicked (pvpanic `GUEST_PANICKED`).
    GuestPanic,
    /// The guest watchdog expired (`WATCHDOG`).
    Watchdog,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Clean => "clean",
            Self::GuestPanic => "guest-panic",
            Self::Watchdog => "watchdog",
        }
    }

    fn is_guest_crash(self) -> bool {
        matches!(self, Self::GuestPanic | Self::Watchdog)
    }
}

/// Backoff and crash-loop limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Delay before the first restart; doubles per consecutive restart.
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// After this many consecutive restarts without a stable run, give up.
    pub max_restarts: u32,
    /// A run at least this long resets the restart count.
    pub stable_after: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            base_delay: Duration::seconds(5),
            max_delay: Duration::minutes(5),
            max_restarts: 5,
            stable_after: Duration::minutes(10),
        }
    }
}

impl Policy {
    /// Delay before restart number `n` (1-based): `base * 2^(n-1)`, capped.
    pub fn delay(&self, n: u32) -> Duration {
        let shift = n.saturating_sub(1).min(20);
        let ms = self
            .base_delay
            .num_milliseconds()
            .saturating_mul(1i64 << shift);
        Duration::milliseconds(ms.min(self.max_delay.num_milliseconds()))
    }
}

/// Persisted per-VM restart history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// Consecutive restarts since the last stable run.
    #[serde(default)]
    pub restarts: u32,
    /// When the supervisor last started this VM.
    #[serde(default)]
    pub last_started_at: Option<DateTime<Utc>>,
    /// A restart waiting for its backoff to pass.
    #[serde(default)]
    pub restart_at: Option<DateTime<Utc>>,
    /// The crash-loop cap was hit; no more restarts until reset.
    #[serde(default)]
    pub gave_up: bool,
    /// The newest guest panic or watchdog event, written by the QMP event
    /// watcher so the next exit can be classified.
    #[serde(default)]
    pub last_crash: Option<(ExitReason, DateTime<Utc>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Leave the VM stopped (and say why).
    Stay(StayReason),
    /// Restart once `delay` has passed; this is restart number `attempt`.
    RestartAfter { delay: Duration, attempt: u32 },
    /// The crash-loop cap was reached.
    GiveUp { restarts: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StayReason {
    /// No strategy and no applicable `on_crash`: the behaviour before this
    /// feature existed.
    NoStrategy,
    Manual,
    Halted,
    CleanExit,
    OnCrashPoweroff,
    AlreadyGaveUp,
}

impl StayReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoStrategy => "no-strategy",
            Self::Manual => "manual",
            Self::Halted => "halted",
            Self::CleanExit => "clean-exit",
            Self::OnCrashPoweroff => "on-crash-poweroff",
            Self::AlreadyGaveUp => "already-gave-up",
        }
    }
}

/// Whether the strategy wants a restart for this exit, before backoff.
fn wants_restart(
    strategy: Option<RunStrategy>,
    on_crash: Option<OnCrash>,
    reason: ExitReason,
) -> Result<(), StayReason> {
    match strategy {
        Some(RunStrategy::Halted) => return Err(StayReason::Halted),
        Some(RunStrategy::Manual) => return Err(StayReason::Manual),
        _ => {}
    }
    if reason.is_guest_crash() {
        match on_crash {
            Some(OnCrash::Poweroff) => return Err(StayReason::OnCrashPoweroff),
            // An explicit crash policy of restart restarts even with no strategy.
            Some(OnCrash::Restart) => return Ok(()),
            _ => {}
        }
    }
    match (strategy, reason) {
        (None, _) => Err(StayReason::NoStrategy),
        (Some(RunStrategy::Always), _) => Ok(()),
        (Some(RunStrategy::RerunOnFailure), ExitReason::Clean) => Err(StayReason::CleanExit),
        (Some(RunStrategy::RerunOnFailure), _) => Ok(()),
        // Handled above.
        (Some(RunStrategy::Manual | RunStrategy::Halted), _) => unreachable!(),
    }
}

/// The decision for one exit at `now`, and the state to store. `State`'s
/// `last_started_at` gives the uptime; a run of `policy.stable_after` or longer
/// resets the restart count first.
pub fn decide(
    strategy: Option<RunStrategy>,
    on_crash: Option<OnCrash>,
    reason: ExitReason,
    mut state: State,
    now: DateTime<Utc>,
    policy: &Policy,
) -> (Decision, State) {
    if state
        .last_started_at
        .is_some_and(|s| now - s >= policy.stable_after)
    {
        state.restarts = 0;
        state.gave_up = false;
    }
    if let Err(stay) = wants_restart(strategy, on_crash, reason) {
        state.restart_at = None;
        return (Decision::Stay(stay), state);
    }
    if state.gave_up {
        state.restart_at = None;
        return (Decision::Stay(StayReason::AlreadyGaveUp), state);
    }
    if state.restarts >= policy.max_restarts {
        state.gave_up = true;
        state.restart_at = None;
        let restarts = state.restarts;
        return (Decision::GiveUp { restarts }, state);
    }
    state.restarts += 1;
    let delay = policy.delay(state.restarts);
    state.restart_at = Some(now + delay);
    let attempt = state.restarts;
    (Decision::RestartAfter { delay, attempt }, state)
}

/// Whether a pending restart is due.
pub fn restart_due(state: &State, now: DateTime<Utc>) -> bool {
    state.restart_at.is_some_and(|t| t <= now)
}

/// A crash event older than this is not blamed for a later exit.
pub const CRASH_EVENT_TTL: Duration = Duration::seconds(120);

/// The reason for an exit at `now`: a recent recorded guest crash, else
/// [`ExitReason::Unknown`].
pub fn classify(state: &State, now: DateTime<Utc>) -> ExitReason {
    match state.last_crash {
        Some((r, at)) if now - at <= CRASH_EVENT_TTL && now >= at => r,
        _ => ExitReason::Unknown,
    }
}

// ---- persistence ----

fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("supervisor")
}

fn path(state_dir: &Path, id: Uuid) -> PathBuf {
    dir(state_dir).join(format!("{id}.json"))
}

pub fn load(state_dir: &Path, id: Uuid) -> State {
    std::fs::read(path(state_dir, id))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(state_dir: &Path, id: Uuid, state: &State) -> std::io::Result<()> {
    std::fs::create_dir_all(dir(state_dir))?;
    let tmp = path(state_dir, id).with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(tmp, path(state_dir, id))
}

pub fn forget(state_dir: &Path, id: Uuid) {
    let _ = std::fs::remove_file(path(state_dir, id));
}

/// Every VM id with a pending restart.
pub fn pending(state_dir: &Path) -> Vec<(Uuid, State)> {
    let Ok(rd) = std::fs::read_dir(dir(state_dir)) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let id = Uuid::parse_str(name.strip_suffix(".json")?).ok()?;
            let st = load(state_dir, id);
            st.restart_at.is_some().then_some((id, st))
        })
        .collect()
}

/// Records a guest panic or watchdog event so the following exit is
/// classified (called by the QMP event watcher).
pub fn note_crash(state_dir: &Path, id: Uuid, reason: ExitReason, at: DateTime<Utc>) {
    let mut st = load(state_dir, id);
    st.last_crash = Some((reason, at));
    let _ = save(state_dir, id, &st);
}

#[cfg(test)]
mod tests {
    use super::*;
    use RunStrategy::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-19T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn run(
        strategy: Option<RunStrategy>,
        on_crash: Option<OnCrash>,
        reason: ExitReason,
        st: State,
    ) -> (Decision, State) {
        decide(strategy, on_crash, reason, st, now(), &Policy::default())
    }

    #[test]
    fn unset_strategy_keeps_current_behaviour() {
        for r in [ExitReason::Unknown, ExitReason::Clean, ExitReason::Watchdog] {
            let (d, st) = run(None, None, r, State::default());
            let want = Decision::Stay(StayReason::NoStrategy);
            assert_eq!(d, want);
            assert_eq!(st, State::default());
        }
    }

    #[test]
    fn strategy_matrix() {
        let go = |d: &Decision| matches!(d, Decision::RestartAfter { .. });
        let cases = [
            (Always, ExitReason::Unknown, true),
            (Always, ExitReason::Clean, true),
            (RerunOnFailure, ExitReason::Unknown, true),
            (RerunOnFailure, ExitReason::GuestPanic, true),
            (RerunOnFailure, ExitReason::Clean, false),
            (Manual, ExitReason::Unknown, false),
            (Manual, ExitReason::GuestPanic, false),
            (Halted, ExitReason::Unknown, false),
            (Halted, ExitReason::Clean, false),
        ];
        for (s, r, restart) in cases {
            let (d, _) = run(Some(s), None, r, State::default());
            assert_eq!(go(&d), restart, "{s:?} {r:?} -> {d:?}");
        }
    }

    #[test]
    fn on_crash_overrides_for_guest_crashes_only() {
        // poweroff beats Always for a panic...
        let (d, _) = run(
            Some(Always),
            Some(OnCrash::Poweroff),
            ExitReason::GuestPanic,
            State::default(),
        );
        assert_eq!(d, Decision::Stay(StayReason::OnCrashPoweroff));
        // ...but not for an unknown exit.
        let (d, _) = run(
            Some(Always),
            Some(OnCrash::Poweroff),
            ExitReason::Unknown,
            State::default(),
        );
        assert!(matches!(d, Decision::RestartAfter { .. }));
        // restart works with no strategy at all...
        let (d, _) = run(
            None,
            Some(OnCrash::Restart),
            ExitReason::Watchdog,
            State::default(),
        );
        assert!(matches!(d, Decision::RestartAfter { .. }));
        // ...but never against Manual or Halted.
        let (d, _) = run(
            Some(Manual),
            Some(OnCrash::Restart),
            ExitReason::Watchdog,
            State::default(),
        );
        assert_eq!(d, Decision::Stay(StayReason::Manual));
        let (d, _) = run(
            Some(Halted),
            Some(OnCrash::Restart),
            ExitReason::GuestPanic,
            State::default(),
        );
        assert_eq!(d, Decision::Stay(StayReason::Halted));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let p = Policy::default();
        let secs: Vec<i64> = (1..=8).map(|n| p.delay(n).num_seconds()).collect();
        assert_eq!(secs, [5, 10, 20, 40, 80, 160, 300, 300]);
        assert_eq!(p.delay(0).num_seconds(), 5);
        assert_eq!(p.delay(u32::MAX).num_seconds(), 300);
    }

    #[test]
    fn crash_loop_is_capped() {
        let mut st = State::default();
        let mut t = now();
        let p = Policy::default();
        for attempt in 1..=p.max_restarts {
            let (d, next) = decide(Some(Always), None, ExitReason::Unknown, st, t, &p);
            let Decision::RestartAfter { delay, attempt: a } = d else {
                panic!("attempt {attempt}: {d:?}")
            };
            assert_eq!(a, attempt);
            assert_eq!(next.restart_at, Some(t + delay));
            st = next;
            // The restarted VM dies again after 30 s, well short of stable.
            st.last_started_at = Some(t + delay);
            t = t + delay + Duration::seconds(30);
        }
        let (d, next) = decide(Some(Always), None, ExitReason::Unknown, st, t, &p);
        assert_eq!(d, Decision::GiveUp { restarts: 5 });
        assert!(next.gave_up && next.restart_at.is_none());
        // Once given up it stays given up.
        let (d, _) = decide(Some(Always), None, ExitReason::Unknown, next, t, &p);
        assert_eq!(d, Decision::Stay(StayReason::AlreadyGaveUp));
    }

    #[test]
    fn a_stable_run_resets_the_count() {
        let p = Policy::default();
        let st = State {
            restarts: 4,
            last_started_at: Some(now() - Duration::minutes(11)),
            ..State::default()
        };
        let (d, next) = decide(Some(Always), None, ExitReason::Unknown, st, now(), &p);
        assert_eq!(
            d,
            Decision::RestartAfter {
                delay: Duration::seconds(5),
                attempt: 1
            }
        );
        assert_eq!(next.restarts, 1);
        // Even a VM that had given up is retried after a stable run.
        let st = State {
            restarts: 5,
            gave_up: true,
            last_started_at: Some(now() - Duration::hours(1)),
            ..State::default()
        };
        let (d, _) = decide(Some(Always), None, ExitReason::Unknown, st, now(), &p);
        assert!(matches!(d, Decision::RestartAfter { attempt: 1, .. }));
    }

    #[test]
    fn staying_clears_a_pending_restart() {
        let st = State {
            restart_at: Some(now()),
            ..State::default()
        };
        let (_, next) = run(Some(Manual), None, ExitReason::Unknown, st);
        assert_eq!(next.restart_at, None);
    }

    #[test]
    fn restart_due_waits_for_the_delay() {
        let st = State {
            restart_at: Some(now() + Duration::seconds(10)),
            ..State::default()
        };
        assert!(!restart_due(&st, now()));
        assert!(restart_due(&st, now() + Duration::seconds(10)));
        assert!(!restart_due(&State::default(), now()));
    }

    #[test]
    fn classify_uses_only_a_fresh_crash_event() {
        let st = State {
            last_crash: Some((ExitReason::GuestPanic, now() - Duration::seconds(5))),
            ..State::default()
        };
        assert_eq!(classify(&st, now()), ExitReason::GuestPanic);
        assert_eq!(
            classify(&st, now() + Duration::minutes(10)),
            ExitReason::Unknown
        );
        assert_eq!(classify(&State::default(), now()), ExitReason::Unknown);
    }

    #[test]
    fn state_persists_and_lists_pending() {
        let dir = tempfile::tempdir().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(load(dir.path(), a), State::default());
        let st = State {
            restarts: 2,
            restart_at: Some(now()),
            ..State::default()
        };
        save(dir.path(), a, &st).unwrap();
        save(dir.path(), b, &State::default()).unwrap();
        assert_eq!(load(dir.path(), a), st);
        let p = pending(dir.path());
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0, a);
        note_crash(dir.path(), b, ExitReason::Watchdog, now());
        assert_eq!(
            load(dir.path(), b).last_crash,
            Some((ExitReason::Watchdog, now()))
        );
        forget(dir.path(), a);
        assert!(pending(dir.path()).is_empty());
    }
}
