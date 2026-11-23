// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Host facts that decide whether a Mac can run a VM right now: the macOS build, whether the login session is unlocked,
//! whether something holds sleep off, thermal throttling and free disk. Read-only: every probe is a command that needs no
//! sudo, and every decision is a pure function over captured text so it is tested from fixtures.
//!
//! Used by the runner launch path (restore guards), the `fluxvm-agent node` heartbeat (placement) and admission.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// File in a snapshot directory that records the macOS build the state was saved on.
pub const HOST_BUILD_FILE: &str = "host-build.txt";

/// The command to run when a restore is refused for a locked session.
pub const HEADLESS_CHECK_HINT: &str = "fluxctl mac host headless-check";

/// A Mac that has been throttled to at most this CPU speed limit (percent) is not given new VMs.
pub const THERMAL_ADMISSION_LIMIT: u32 = 80;

/// Facts a node publishes in its heartbeat. Every field is optional so an older agent (which sends none) and a probe that
/// failed both read as "unknown", which placement never penalises.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHealth {
    /// The login session exists and its screen is not locked. Restoring saved VM state needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_unlocked: Option<bool>,
    /// Something (caffeinate, the keep-awake agent, Screen Sharing) is holding system sleep off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_held: Option<bool>,
    /// `pmset -g therm` `CPU_Speed_Limit` when below 100, else `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thermal_limit: Option<u32>,
    /// Free space on the data volume, MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_disk_mib: Option<u64>,
    /// `sw_vers -buildVersion`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_build: Option<String>,
}

// ------------------------------------------------------------------ parsers

/// `Some(true)` when the console session's screen is locked, `Some(false)` when unlocked, and `Some(true)` for the login
/// window (no session at all: a restore cannot work there either). `None` when the text is not `ioreg` output.
pub fn session_locked(ioreg: &str) -> Option<bool> {
    let users = ioreg
        .lines()
        .find(|l| l.contains("\"IOConsoleUsers\""))
        .map(|l| l.split_once('=').map_or("", |x| x.1).trim());
    match users {
        None => ioreg.contains("IOConsole").then_some(true).or(None),
        Some(u) if !u.contains('{') => Some(true),
        Some(u) => Some(u.contains("\"CGSSessionScreenIsLocked\"=Yes")),
    }
}

/// `pmset -g therm`: the CPU speed limit when it is below 100.
pub fn thermal_limit(text: &str) -> Option<u32> {
    text.lines().find_map(|l| {
        let n = l
            .trim()
            .strip_prefix("CPU_Speed_Limit")?
            .trim_start()
            .strip_prefix('=')?
            .trim()
            .parse::<u32>()
            .ok()?;
        (n < 100).then_some(n)
    })
}

/// `pmset -g`: whether a `(sleep prevented by ...)` note is present.
pub fn sleep_held(pmset_g: &str) -> bool {
    pmset_g.contains("sleep prevented by")
}

/// `df -k` free space in MiB (the `Available` column of the first data row).
pub fn free_disk_mib(df: &str) -> Option<u64> {
    let row = df.lines().nth(1)?;
    let kib: u64 = row.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib / 1024)
}

// ------------------------------------------------------------------ probes

/// Runs a read-only command and returns stdout when it exits 0.
#[cfg(target_os = "macos")]
fn read(program: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn read(_program: &str, _args: &[&str]) -> Option<String> {
    None
}

/// The host's macOS build (`24G90`), or `None` off macOS or when `sw_vers` cannot run.
pub fn current_os_build() -> Option<String> {
    read("/usr/bin/sw_vers", &["-buildVersion"])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Reads the session lock state of this Mac (`None` when unknown, for example off macOS).
pub fn probe_session_locked() -> Option<bool> {
    read("ioreg", &["-n", "Root", "-d1", "-r"]).and_then(|t| session_locked(&t))
}

/// Reads only the thermal limit (one short command), for admission on the VM create path.
pub fn probe_thermal_limit() -> Option<u32> {
    read("pmset", &["-g", "therm"]).and_then(|t| thermal_limit(&t))
}

/// Probes every fact. Blocking (runs a few short commands); call from `spawn_blocking`.
pub fn probe() -> HostHealth {
    HostHealth {
        session_unlocked: probe_session_locked().map(|l| !l),
        sleep_held: read("pmset", &["-g"]).map(|t| sleep_held(&t)),
        thermal_limit: read("pmset", &["-g", "therm"]).and_then(|t| thermal_limit(&t)),
        free_disk_mib: read("df", &["-k", "/System/Volumes/Data"])
            .or_else(|| read("df", &["-k", "/"]))
            .and_then(|t| free_disk_mib(&t)),
        os_build: current_os_build(),
    }
}

/// Free space on the data volume, MiB. Blocking (runs `df`); `None` off macOS or when `df` cannot be read.
pub fn probe_free_disk_mib() -> Option<u64> {
    read("df", &["-k", "/System/Volumes/Data"])
        .or_else(|| read("df", &["-k", "/"]))
        .and_then(|t| free_disk_mib(&t))
}

// ------------------------------------------------------------------ decisions

/// Free space a checkpoint keeps in reserve however small it is: the disk is an APFS clone (free until the live disk and the
/// checkpoint diverge), but the guest keeps writing, and a full volume corrupts a running guest.
pub const CHECKPOINT_RESERVE_MIB: u64 = 4096;

/// Whether there is room to save a checkpoint. A checkpoint with memory writes about the guest's RAM to disk on top of the
/// reserve; a disk-only one needs the reserve only. `None` free space (probe failed) never blocks. The text names the next step.
pub fn checkpoint_space_blocker(
    free_mib: Option<u64>,
    guest_memory_mib: u64,
    with_memory: bool,
) -> Option<String> {
    let free = free_mib?;
    let need = CHECKPOINT_RESERVE_MIB + if with_memory { guest_memory_mib } else { 0 };
    if free >= need {
        return None;
    }
    Some(format!(
        "not enough free disk to save a checkpoint: {free} MiB free on this Mac, about {need} MiB needed{}. Delete checkpoints \
         you do not need (`fluxctl mac checkpoints`, then `fluxctl mac checkpoint rm NAME TAG`) or instances (`fluxctl mac rm NAME`), \
         then retry",
        if with_memory {
            " (the guest's memory is written out with it; `--disk-only` skips that)"
        } else {
            ""
        }
    ))
}

/// What a restore of saved state should do given the build it was saved on and the host's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreVerdict {
    /// Same build, or no way to tell (an old snapshot with no record, or the host build is unreadable): restore.
    Allow,
    /// The saved state is from another macOS build. Saved state is bound to the OS that wrote it, so a restore may fail.
    Mismatch { saved: String, host: String },
}

/// Compares the recorded build with the host's. A missing record is [`RestoreVerdict::Allow`]: snapshots written before this
/// check existed keep working, and the framework's own validation still applies.
pub fn restore_verdict(saved: Option<&str>, host: Option<&str>) -> RestoreVerdict {
    match (saved.map(str::trim), host.map(str::trim)) {
        (Some(s), Some(h)) if !s.is_empty() && !h.is_empty() && s != h => {
            RestoreVerdict::Mismatch {
                saved: s.to_owned(),
                host: h.to_owned(),
            }
        }
        _ => RestoreVerdict::Allow,
    }
}

/// The recorded build in snapshot directory `dir`, if any.
pub fn read_recorded_build(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(HOST_BUILD_FILE))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Writes the host's current build into snapshot directory `dir`. Best effort: a snapshot is never failed over this.
pub fn record_build(dir: &Path) {
    if let Some(b) = current_os_build() {
        let _ = std::fs::write(dir.join(HOST_BUILD_FILE), format!("{b}\n"));
    }
}

/// How to treat a restore across a macOS update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnBuildMismatch {
    /// Boot cold from the snapshot's disk and say so (default).
    ColdBoot,
    /// Fail the launch (`FLUXVM_RESTORE_ON_OS_UPDATE=refuse`).
    Refuse,
}

pub fn on_build_mismatch_from(value: Option<&str>) -> OnBuildMismatch {
    match value.map(str::trim) {
        Some("refuse") => OnBuildMismatch::Refuse,
        _ => OnBuildMismatch::ColdBoot,
    }
}

/// Environment variable that skips the locked-session check on restore (`1`, `true`, `yes` or `on`).
pub const SKIP_SESSION_CHECK_ENV: &str = "FLUXVM_SKIP_SESSION_CHECK";
/// Environment variable that lets a throttled Mac take a new VM anyway (`1`, `true`, `yes` or `on`).
pub const IGNORE_THERMAL_ENV: &str = "FLUXVM_IGNORE_THERMAL";

/// Whether an override variable's value turns it on. Anything else, including unset, empty and `0`, leaves the guard on.
pub fn env_flag(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Whether `name` is set to an enabled value in this process's environment.
pub fn env_flag_set(name: &str) -> bool {
    env_flag(std::env::var(name).ok().as_deref())
}

/// The message for a restore across macOS builds.
pub fn mismatch_message(saved: &str, host: &str) -> String {
    format!(
        "the saved VM state was written on macOS build {saved} but this Mac now runs build {host}; Virtualization.framework \
         state is not portable across macOS updates"
    )
}

/// The message for a restore attempted from a locked session.
pub fn locked_message() -> String {
    format!(
        "restoring saved VM state needs an unlocked login session and this Mac's session is locked; unlock it (or enable \
         automatic login) and retry, and run `{HEADLESS_CHECK_HINT}` to see what to change. If the check is wrong for this Mac, \
         set {SKIP_SESSION_CHECK_ENV}=1 on the daemon to skip it"
    )
}

/// Decisions for placement and admission over published facts.
impl HostHealth {
    /// [`Self::admission_blockers`], or nothing when the operator set `FLUXVM_IGNORE_THERMAL` (`ignore_thermal`).
    pub fn admission_blockers_unless(&self, ignore_thermal: bool) -> Vec<String> {
        if ignore_thermal {
            Vec::new()
        } else {
            self.admission_blockers()
        }
    }

    /// Reasons this Mac should not get a new VM right now. Unknown facts never count.
    pub fn admission_blockers(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(n) = self.thermal_limit
            && n < THERMAL_ADMISSION_LIMIT
        {
            out.push(format!(
                "thermally throttled (CPU speed limit {n}%, admission stops below {THERMAL_ADMISSION_LIMIT}%)"
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_space_needs_the_reserve_and_the_memory_when_it_is_saved() {
        assert!(checkpoint_space_blocker(None, 8192, true).is_none());
        assert!(checkpoint_space_blocker(Some(20_000), 8192, true).is_none());
        let m = checkpoint_space_blocker(Some(10_000), 8192, true).unwrap();
        assert!(
            m.contains("10000 MiB free")
                && m.contains("12288 MiB needed")
                && m.contains("--disk-only")
        );
        assert!(m.contains("fluxctl mac checkpoint rm"));
        // The same Mac can still take a disk-only checkpoint.
        assert!(checkpoint_space_blocker(Some(10_000), 8192, false).is_none());
        let m = checkpoint_space_blocker(Some(100), 8192, false).unwrap();
        assert!(m.contains("4096 MiB needed") && !m.contains("--disk-only"));
    }

    #[test]
    fn override_variables_turn_on_only_for_explicit_values() {
        for on in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(env_flag(Some(on)), "{on:?} should enable the override");
        }
        for off in ["", "0", "false", "no", "off", "2", "enable"] {
            assert!(!env_flag(Some(off)), "{off:?} must not enable the override");
        }
        assert!(!env_flag(None));
    }

    #[test]
    fn the_override_names_are_the_documented_ones() {
        assert_eq!(SKIP_SESSION_CHECK_ENV, "FLUXVM_SKIP_SESSION_CHECK");
        assert_eq!(IGNORE_THERMAL_ENV, "FLUXVM_IGNORE_THERMAL");
        assert!(locked_message().contains(SKIP_SESSION_CHECK_ENV));
    }

    #[test]
    fn a_throttled_mac_is_admitted_only_with_the_thermal_override() {
        let hot = HostHealth {
            thermal_limit: Some(50),
            ..Default::default()
        };
        assert_eq!(
            hot.admission_blockers_unless(false),
            hot.admission_blockers()
        );
        assert!(!hot.admission_blockers_unless(false).is_empty());
        assert!(hot.admission_blockers_unless(true).is_empty());
        let cool = HostHealth::default();
        assert!(cool.admission_blockers_unless(false).is_empty());
    }

    const IOREG_LOCKED: &str = include_str!("host_health_fixtures/ioreg_locked.txt");
    const IOREG_UNLOCKED: &str = include_str!("host_health_fixtures/ioreg_unlocked.txt");
    const IOREG_LOGINWINDOW: &str = include_str!("host_health_fixtures/ioreg_loginwindow.txt");

    #[test]
    fn session_lock_from_ioreg() {
        assert_eq!(session_locked(IOREG_LOCKED), Some(true));
        assert_eq!(session_locked(IOREG_UNLOCKED), Some(false));
        assert_eq!(session_locked(IOREG_LOGINWINDOW), Some(true));
        assert_eq!(session_locked("garbage"), None);
    }

    #[test]
    fn thermal_limit_only_below_100() {
        assert_eq!(
            thermal_limit("CPU_Scheduler_Limit = 100\nCPU_Speed_Limit = 70\n"),
            Some(70)
        );
        assert_eq!(thermal_limit("CPU_Speed_Limit \t= 100\n"), None);
        assert_eq!(
            thermal_limit("Note: No thermal warning level has been recorded\n"),
            None
        );
    }

    #[test]
    fn sleep_held_and_disk() {
        assert!(sleep_held(
            " sleep 0 (sleep prevented by caffeinate, powerd)\n"
        ));
        assert!(!sleep_held(" sleep 1\n"));
        let df = "Filesystem 1024-blocks Used Available Capacity iused ifree %iused Mounted on\n/dev/disk3s5 482746452 416546668 34973776 93% 1 2 1% /System/Volumes/Data\n";
        assert_eq!(free_disk_mib(df), Some(34973776 / 1024));
        assert_eq!(free_disk_mib("nothing"), None);
    }

    #[test]
    fn build_mismatch_is_flagged_only_when_both_known_and_different() {
        assert_eq!(
            restore_verdict(Some("24G90"), Some("24G90")),
            RestoreVerdict::Allow
        );
        assert_eq!(restore_verdict(None, Some("24G90")), RestoreVerdict::Allow);
        assert_eq!(restore_verdict(Some("24G90"), None), RestoreVerdict::Allow);
        assert_eq!(
            restore_verdict(Some("24G90\n"), Some("25A5")),
            RestoreVerdict::Mismatch {
                saved: "24G90".into(),
                host: "25A5".into()
            }
        );
    }

    #[test]
    fn recorded_build_round_trips_and_policy_defaults_to_cold_boot() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_recorded_build(d.path()), None);
        std::fs::write(d.path().join(HOST_BUILD_FILE), "24G90\n").unwrap();
        assert_eq!(read_recorded_build(d.path()).as_deref(), Some("24G90"));
        assert_eq!(on_build_mismatch_from(None), OnBuildMismatch::ColdBoot);
        assert_eq!(
            on_build_mismatch_from(Some("refuse")),
            OnBuildMismatch::Refuse
        );
        assert!(mismatch_message("a", "b").contains("macOS build a"));
        assert!(locked_message().contains("headless-check"));
    }

    #[test]
    fn throttling_blocks_admission_and_unknown_does_not() {
        let hot = HostHealth {
            thermal_limit: Some(50),
            ..Default::default()
        };
        assert_eq!(hot.admission_blockers().len(), 1);
        let mild = HostHealth {
            thermal_limit: Some(90),
            ..Default::default()
        };
        assert!(mild.admission_blockers().is_empty());
        assert!(HostHealth::default().admission_blockers().is_empty());
    }

    #[test]
    fn old_heartbeat_without_health_decodes() {
        let h: HostHealth = serde_json::from_str("{}").unwrap();
        assert_eq!(h, HostHealth::default());
        assert_eq!(serde_json::to_string(&h).unwrap(), "{}");
    }
}
