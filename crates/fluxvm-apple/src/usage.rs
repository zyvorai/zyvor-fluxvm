// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Host-side accounting of a VM's runner process. Virtualization.framework exposes no per-VM statistics, but the
//! runner is one process per VM and owns the guest's memory mapping and vCPU threads, so its resident size and CPU time
//! are a fair host-side measure of what the VM costs. This is the runner process, not guest-visible memory or CPU.

use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RunnerUsage {
    /// Resident set size of the runner process, bytes (includes resident guest RAM).
    pub rss_bytes: u64,
    /// CPU time spent in user mode, seconds.
    pub cpu_user_seconds: f64,
    /// CPU time spent in the kernel on behalf of the runner (includes Hypervisor.framework exits), seconds.
    pub cpu_system_seconds: f64,
}

impl RunnerUsage {
    /// `fluxvm_vm_runner_cpu_seconds_total`: user plus system time.
    pub fn cpu_seconds(&self) -> f64 {
        self.cpu_user_seconds + self.cpu_system_seconds
    }
}

/// Converts Mach absolute-time ticks to seconds with a `mach_timebase_info` ratio (125/3 on Apple silicon, 1/1 on Intel).
/// A zero denominator yields 0 rather than dividing by zero.
pub fn mach_ticks_to_seconds(ticks: u64, numer: u32, denom: u32) -> f64 {
    if denom == 0 {
        return 0.0;
    }
    ticks as f64 * f64::from(numer) / f64::from(denom) / 1e9
}

/// Resident size and CPU time of process `pid` (`proc_pid_rusage`, flavor `RUSAGE_INFO_V4`). Errors when the process
/// is gone or not ours to inspect, and off macOS.
pub fn runner_usage(pid: u32) -> Result<RunnerUsage> {
    #[cfg(target_os = "macos")]
    {
        let pid = i32::try_from(pid).map_err(|_| anyhow::anyhow!("pid {pid} is out of range"))?;
        // SAFETY: `info` is a zeroed rusage_info_v4 that outlives the call; proc_pid_rusage fills it for the
        // RUSAGE_INFO_V4 flavor and takes the pointer as an opaque `rusage_info_t *`.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V4,
                (&mut info as *mut libc::rusage_info_v4).cast(),
            )
        };
        if rc != 0 {
            bail!(
                "proc_pid_rusage({pid}): {}",
                std::io::Error::last_os_error()
            );
        }
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `tb` is a valid out-parameter.
        if unsafe { libc::mach_timebase_info(&mut tb) } != 0 {
            tb = libc::mach_timebase_info { numer: 1, denom: 1 };
        }
        Ok(RunnerUsage {
            rss_bytes: info.ri_resident_size,
            cpu_user_seconds: mach_ticks_to_seconds(info.ri_user_time, tb.numer, tb.denom),
            cpu_system_seconds: mach_ticks_to_seconds(info.ri_system_time, tb.numer, tb.denom),
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        bail!("runner accounting is only available on macOS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_mach_ticks() {
        // Apple silicon: 125/3 ns per tick.
        let s = mach_ticks_to_seconds(24_000_000, 125, 3);
        assert!((s - 1.0).abs() < 1e-9, "{s}");
        assert_eq!(mach_ticks_to_seconds(1_000_000_000, 1, 1), 1.0);
        assert_eq!(mach_ticks_to_seconds(5, 1, 0), 0.0);
    }

    #[test]
    fn cpu_seconds_adds_user_and_system() {
        let u = RunnerUsage {
            rss_bytes: 1,
            cpu_user_seconds: 1.5,
            cpu_system_seconds: 0.25,
        };
        assert_eq!(u.cpu_seconds(), 1.75);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_this_process() {
        let u = runner_usage(std::process::id()).unwrap();
        assert!(u.rss_bytes > 1024 * 1024, "{u:?}");
        assert!(u.cpu_seconds() >= 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_missing_process_is_an_error() {
        // pid_max on macOS is 99999; this is far above it.
        assert!(runner_usage(4_000_000).is_err());
    }
}
