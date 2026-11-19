// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! VM lifecycle spec types: what the host does when a VM's hypervisor process
//! dies (`run_strategy`), what a guest panic or watchdog expiry does
//! (`on_crash`), and when it powers on and off by the clock (`power_schedule`).
//!
//! Every field is optional on `CreateVmRequest`; unset keeps the behaviour from
//! before they existed. The decision logic lives in `fluxvm-scheduler`
//! (`supervisor`, `power_schedule`); the QEMU arguments in `fluxvm-qemu`
//! (`crash_watch`). JSON keys are snake_case like the rest of the spec, and the
//! camelCase spelling (`runStrategy`, `onCrash`, `powerSchedule`) is accepted.

use serde::{Deserialize, Serialize};

/// What to do when the hypervisor process of a running VM exits without a
/// stop request (the process is gone; `fluxctl stop` and the API never take
/// this path).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStrategy {
    /// Restart on every unrequested exit, clean or not.
    Always,
    /// Restart only when the exit looks like a failure (a crash, a guest
    /// panic, a watchdog reset, or an exit whose cause is unknown).
    #[default]
    RerunOnFailure,
    /// Never restart; `enable`d VMs still start when the daemon boots.
    Manual,
    /// Never restart and never autostart; only an explicit `start` runs it.
    Halted,
}

impl RunStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Always => "Always",
            Self::RerunOnFailure => "RerunOnFailure",
            Self::Manual => "Manual",
            Self::Halted => "Halted",
        }
    }
}

/// What a guest panic (pvpanic) or watchdog expiry does. QEMU only.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OnCrash {
    /// Tear the VM down and start it again (watchdog: reset in place).
    #[default]
    Restart,
    /// Power the VM off and leave it stopped.
    Poweroff,
    /// Pause the guest so it can be inspected.
    Pause,
    /// Pause, and write a guest core into `<workspace>/dumps/` when that
    /// directory already exists; with no such directory it is `pause`.
    CoredumpIfExisting,
}

impl OnCrash {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Poweroff => "poweroff",
            Self::Pause => "pause",
            Self::CoredumpIfExisting => "coredump-if-existing",
        }
    }
}

/// Cron-driven power on and off, in UTC. Five-field cron (`min hour dom month
/// dow`) or `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PowerSchedule {
    /// When to start a stopped VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// When to stop (graceful) a running VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<String>,
}

/// VM label equivalents of `power_schedule` (the `fluxvm.io/snapshot-every`
/// precedent); the spec field wins when both are present.
pub const POWER_START_LABEL: &str = "fluxvm.io/power-start";
pub const POWER_STOP_LABEL: &str = "fluxvm.io/power-stop";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_names() {
        assert_eq!(
            serde_json::from_str::<RunStrategy>("\"RerunOnFailure\"").unwrap(),
            RunStrategy::RerunOnFailure
        );
        assert_eq!(
            serde_json::from_str::<OnCrash>("\"coredump-if-existing\"").unwrap(),
            OnCrash::CoredumpIfExisting
        );
        assert!(serde_json::from_str::<OnCrash>("\"explode\"").is_err());
    }

    #[test]
    fn schedule_round_trips_and_skips_unset() {
        let s: PowerSchedule = serde_json::from_str(r#"{"start":"0 8 * * 1-5"}"#).unwrap();
        assert_eq!(s.stop, None);
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"start":"0 8 * * 1-5"}"#
        );
    }
}
