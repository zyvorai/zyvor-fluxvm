// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `GET /v1/stats`: one cheap sample of this host and every VM on it, for the fleet's metrics collector.
//!
//! Everything that moves is a **cumulative counter** (CPU ticks and seconds, disk and network bytes) so the reader
//! takes two samples and divides by the time between them; gauges (memory, disk free) are as of now. A figure this
//! host cannot measure is `null`, never a made-up zero.
//!
//! Where the per-VM numbers come from (`source`):
//! * `vz-process`: a `vz` guest on macOS, read with `proc_pid_rusage` from the runner process plus the
//!   Virtualization XPC service that actually runs the guest (see `fluxvm_apple::vm_usage`). CPU, resident memory and
//!   disk bytes; no network (Virtualization.framework exposes no per-VM network counters).
//! * `vz-runner`: as above, but the XPC service could not be attributed, so the numbers are the runner process only
//!   and do not include the guest.
//! * `cgroup`: a Linux backend (QEMU, Cloud Hypervisor, Firecracker) read from the VM's cgroup v2 (CPU, memory,
//!   block I/O), with network bytes from its TAP device when it has one in the host's namespace.
//! * `none`: stopped, or nothing could be read.

use chrono::{DateTime, Utc};
use fluxvm_core::model::{BackendKind, VmRecord, VmStatus};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The host half of a sample.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostStats {
    /// Logical CPUs.
    #[serde(default)]
    pub cpus: u32,
    /// Busy and total CPU time over all cores since boot, in the platform's clock ticks (`/proc/stat` on Linux,
    /// `host_statistics(HOST_CPU_LOAD_INFO)` on macOS). Only the ratio of two samples' differences means anything.
    #[serde(default)]
    pub cpu_busy_ticks: Option<u64>,
    #[serde(default)]
    pub cpu_total_ticks: Option<u64>,
    #[serde(default)]
    pub memory_total_mib: Option<u64>,
    #[serde(default)]
    pub memory_available_mib: Option<u64>,
    /// The volume that holds the state directory (VM disks).
    #[serde(default)]
    pub disk_total_bytes: Option<u64>,
    #[serde(default)]
    pub disk_free_bytes: Option<u64>,
    #[serde(default)]
    pub vm_count: u32,
    #[serde(default)]
    pub vms_running: u32,
}

/// One VM's half of a sample.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VmStats {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub backend: String,
    /// Configured size, so a reader can turn CPU seconds into "percent of its vCPUs".
    #[serde(default)]
    pub vcpus: u32,
    #[serde(default)]
    pub memory_mib: u64,
    #[serde(default)]
    pub cpu_seconds_total: Option<f64>,
    #[serde(default)]
    pub memory_used_bytes: Option<u64>,
    #[serde(default)]
    pub disk_read_bytes_total: Option<u64>,
    #[serde(default)]
    pub disk_write_bytes_total: Option<u64>,
    /// Bytes the guest received and sent.
    #[serde(default)]
    pub net_rx_bytes_total: Option<u64>,
    #[serde(default)]
    pub net_tx_bytes_total: Option<u64>,
    /// `vz-process`, `vz-runner`, `cgroup` or `none`; see the module documentation.
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeStats {
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub host: HostStats,
    #[serde(default)]
    pub vms: Vec<VmStats>,
}

fn label<T: Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Samples the host and every VM (blocking: reads `/proc`, cgroups and process tables).
pub fn sample(vms: &[VmRecord], state_dir: &Path) -> NodeStats {
    let mem = fluxvm_core::pressure_admission::sample_host();
    let vol = crate::storage_report::volume(state_dir);
    let (busy, total) = host_cpu_ticks().unzip();
    let live = |vm: &VmRecord| matches!(vm.status, VmStatus::Running | VmStatus::Paused);
    let host = HostStats {
        cpus: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
        cpu_busy_ticks: busy,
        cpu_total_ticks: total,
        memory_total_mib: mem.mem_total_mib,
        memory_available_mib: mem.mem_available_mib,
        disk_total_bytes: vol.total_bytes,
        disk_free_bytes: vol.free_bytes,
        vm_count: vms.len() as u32,
        vms_running: vms.iter().filter(|v| v.status == VmStatus::Running).count() as u32,
    };

    // vz guests are measured in one pass (the XPC service scan is shared).
    let vz: Vec<(usize, u32, std::path::PathBuf)> = vms
        .iter()
        .enumerate()
        .filter(|(_, vm)| vm.backend == BackendKind::Vz && live(vm))
        .filter_map(|(i, vm)| Some((i, vm.pid?, vm.workspace.clone())))
        .collect();
    let targets: Vec<(u32, std::path::PathBuf)> =
        vz.iter().map(|(_, pid, w)| (*pid, w.clone())).collect();
    let vz_usage = if targets.is_empty() {
        Vec::new()
    } else {
        fluxvm_apple::vm_usage(&targets)
    };

    let mut out = Vec::with_capacity(vms.len());
    for (i, vm) in vms.iter().enumerate() {
        let mut s = VmStats {
            id: vm.id.to_string(),
            name: vm.name.clone(),
            status: label(vm.status),
            backend: label(vm.backend),
            vcpus: u32::from(vm.request.vcpus),
            memory_mib: vm.request.memory_mib,
            source: "none".into(),
            ..Default::default()
        };
        if let Some(k) = vz.iter().position(|(j, _, _)| *j == i) {
            if let Some(Some(u)) = vz_usage.get(k) {
                s.cpu_seconds_total = Some(u.usage.cpu_seconds());
                s.memory_used_bytes = Some(u.usage.rss_bytes);
                s.disk_read_bytes_total = Some(u.usage.disk_read_bytes);
                s.disk_write_bytes_total = Some(u.usage.disk_write_bytes);
                s.source = if u.service_pids.is_empty() {
                    "vz-runner"
                } else {
                    "vz-process"
                }
                .into();
            }
        } else if live(vm)
            && let Some(path) = &vm.cgroup_path
            && let Ok(cg) = fluxvm_cgroup::CgroupManager::from_path(path.clone())
        {
            s.cpu_seconds_total = cg.cpu().get_stat().ok().map(|c| c.usage_usec as f64 / 1e6);
            s.memory_used_bytes = cg.memory().get_current().ok();
            if let Ok(io) = cg.io().get_stat() {
                s.disk_read_bytes_total = Some(io.iter().map(|x| x.rbytes).sum());
                s.disk_write_bytes_total = Some(io.iter().map(|x| x.wbytes).sum());
            }
            if let Some((rx, tx)) = vm.tap_name.as_deref().and_then(tap_bytes) {
                s.net_rx_bytes_total = Some(rx);
                s.net_tx_bytes_total = Some(tx);
            }
            if s.cpu_seconds_total.is_some() || s.memory_used_bytes.is_some() {
                s.source = "cgroup".into();
            }
        }
        out.push(s);
    }
    NodeStats {
        ts: Utc::now(),
        host,
        vms: out,
    }
}

/// A TAP device's counters as the guest sees them: what the host side transmits the guest receives. `None` for a
/// name that is not a plain interface name or a device not visible in this namespace.
fn tap_bytes(tap: &str) -> Option<(u64, u64)> {
    if tap.is_empty()
        || tap.len() > 15
        || !tap
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || tap.starts_with('.')
    {
        return None;
    }
    let read = |f: &str| -> Option<u64> {
        std::fs::read_to_string(format!("/sys/class/net/{tap}/statistics/{f}"))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    Some((read("tx_bytes")?, read("rx_bytes")?))
}

/// `(busy, total)` from the first line of `/proc/stat`: `cpu user nice system idle iowait irq softirq steal ...`.
/// Idle and iowait are idle; guest time is already inside user/nice, so the guest columns are not added twice.
pub fn parse_proc_stat(content: &str) -> Option<(u64, u64)> {
    let line = content.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(|x| x.parse().ok())
        .collect::<Option<Vec<u64>>>()?;
    if v.len() < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v.get(4).copied().unwrap_or(0);
    Some((total.saturating_sub(idle), total))
}

#[cfg(target_os = "linux")]
fn host_cpu_ticks() -> Option<(u64, u64)> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

#[cfg(target_os = "macos")]
fn host_cpu_ticks() -> Option<(u64, u64)> {
    // host_statistics(HOST_CPU_LOAD_INFO): four natural_t tick counters (user, system, idle, nice), summed over every
    // CPU. Declared here rather than through `libc` so the flavor and layout are explicit.
    const HOST_CPU_LOAD_INFO: i32 = 3;
    const CPU_STATE_MAX: u32 = 4;
    const CPU_STATE_IDLE: usize = 2;
    unsafe extern "C" {
        fn mach_host_self() -> u32;
        fn host_statistics(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    static HOST: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: mach_host_self has no preconditions; the port is kept for the life of the process.
    let host = *HOST.get_or_init(|| unsafe { mach_host_self() });
    let mut ticks = [0i32; CPU_STATE_MAX as usize];
    let mut count = CPU_STATE_MAX;
    // SAFETY: `ticks` holds `count` integers, which is what HOST_CPU_LOAD_INFO writes.
    let kr = unsafe { host_statistics(host, HOST_CPU_LOAD_INFO, ticks.as_mut_ptr(), &mut count) };
    if kr != 0 {
        return None;
    }
    // natural_t is unsigned and wraps; read the bits as u32.
    let t: Vec<u64> = ticks.iter().map(|x| u64::from(*x as u32)).collect();
    let total: u64 = t.iter().sum();
    Some((total.saturating_sub(t[CPU_STATE_IDLE]), total))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn host_cpu_ticks() -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_stat_busy_excludes_idle_and_iowait() {
        let s = "cpu  100 5 50 800 20 3 2 0 0 0\ncpu0 50 2 25 400 10 1 1 0 0 0\n";
        // total = 100+5+50+800+20+3+2+0 = 980; idle = 800 + 20.
        assert_eq!(parse_proc_stat(s), Some((160, 980)));
        assert_eq!(parse_proc_stat("intr 1 2 3"), None);
        assert_eq!(parse_proc_stat("cpu  x y z"), None);
    }

    #[test]
    fn tap_names_cannot_escape_sysfs() {
        assert_eq!(tap_bytes("../../etc"), None);
        assert_eq!(tap_bytes(""), None);
        assert_eq!(tap_bytes("a/b"), None);
        assert_eq!(tap_bytes("a-very-long-interface-name"), None);
        // A well-formed name that does not exist is simply unknown.
        assert_eq!(tap_bytes("fvnotthere0"), None);
    }

    #[test]
    fn a_sample_round_trips_and_old_readers_tolerate_missing_fields() {
        let s = NodeStats {
            ts: Utc::now(),
            host: HostStats {
                cpus: 8,
                cpu_busy_ticks: Some(10),
                cpu_total_ticks: Some(100),
                vm_count: 1,
                ..Default::default()
            },
            vms: vec![VmStats {
                id: "a".into(),
                vcpus: 2,
                cpu_seconds_total: Some(1.5),
                source: "cgroup".into(),
                ..Default::default()
            }],
        };
        let back: NodeStats = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let minimal: NodeStats =
            serde_json::from_str(r#"{"ts":"2026-10-12T00:00:00Z","vms":[{"id":"x"}]}"#).unwrap();
        assert_eq!(minimal.vms[0].cpu_seconds_total, None);
    }

    #[test]
    fn stopped_vms_report_nothing() {
        let rec: VmRecord = serde_json::from_value(serde_json::json!({
            "id": "6f1c1d1e-0000-4000-8000-000000000001",
            "name": "v",
            "backend": "qemu",
            "status": "stopped",
            "pid": null,
            "created_at": "2026-10-12T00:00:00Z",
            "expires_at": null,
            "workspace": "/nonexistent",
            "disk": "/nonexistent/d",
            "seed_disk": null,
            "tap_name": null,
            "control_socket": null,
            "log_path": "/nonexistent/log",
            "error": null,
            "request": {"name": "v", "backend": "qemu", "image": "/x"}
        }))
        .unwrap();
        let s = sample(&[rec], Path::new("/"));
        assert_eq!(s.host.vm_count, 1);
        assert_eq!(s.host.vms_running, 0);
        assert_eq!(s.vms[0].status, "stopped");
        assert_eq!(s.vms[0].source, "none");
        assert_eq!(s.vms[0].cpu_seconds_total, None);
    }
}
