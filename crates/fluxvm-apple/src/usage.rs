// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Host-side accounting of a VM. Virtualization.framework exposes no per-VM statistics, and the runner process alone is
//! not the guest: on macOS the guest's vCPUs and memory live in a separate
//! `com.apple.Virtualization.VirtualMachine` XPC service (parent launchd, responsible pid the user's shell, so neither
//! the parent nor `responsibility_get_pid_responsible_for_pid` links it to the runner). The reliable public link is the
//! open files: the service holds the VM's root disk and `efi.bin` open, and those paths sit in the VM's own workspace.
//! [`vm_usage`] matches on that and sums runner and service; with no match the numbers are the runner process only.

use anyhow::{Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RunnerUsage {
    /// Resident set size, bytes.
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

/// When process `pid` started, as Unix seconds (`proc_pidinfo` `PROC_PIDTBSDINFO`). `None` when the process is gone or
/// not ours to inspect, and off macOS.
pub fn process_started_at(pid: u32) -> Option<i64> {
    #[cfg(target_os = "macos")]
    {
        let pid = i32::try_from(pid).ok()?;
        // SAFETY: `info` is a zeroed proc_bsdinfo of the size passed; proc_pidinfo fills it for PROC_PIDTBSDINFO.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (n == size).then(|| info.pbi_start_tvsec as i64)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        None
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

/// Where Virtualization.framework runs a guest: one `com.apple.Virtualization.VirtualMachine` XPC service process per
/// VM, parented to launchd (not to the runner), so the runner's own numbers never move with the guest.
pub const VZ_SERVICE_BINARY: &str = "com.apple.Virtualization.VirtualMachine";

/// Whether `path` is the executable of the Virtualization XPC service.
pub fn is_vz_service_path(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == VZ_SERVICE_BINARY)
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|n| n == "com.apple.Virtualization.VirtualMachine.xpc")
}

/// Pure attribution: for each workspace, the service pids that hold a file under it open. A service belongs to the
/// workspace with the longest matching prefix among its open files; one whose files match more than one workspace
/// (it cannot be told apart) is attributed to none.
pub fn attribute_services(
    workspaces: &[PathBuf],
    services: &[(u32, Vec<PathBuf>)],
) -> Vec<Vec<u32>> {
    let mut out = vec![Vec::new(); workspaces.len()];
    for (pid, files) in services {
        let mut hits: Vec<usize> = Vec::new();
        for f in files {
            let best = workspaces
                .iter()
                .enumerate()
                .filter(|(_, w)| f.starts_with(w))
                .max_by_key(|(_, w)| w.components().count());
            if let Some((i, _)) = best
                && !hits.contains(&i)
            {
                hits.push(i);
            }
        }
        if let [i] = hits.as_slice() {
            out[*i].push(*pid);
        }
    }
    out
}

/// Runner plus attributed service numbers for one VM.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VmUsage {
    /// Runner process plus every attributed Virtualization service process, summed.
    pub usage: RunnerUsage,
    /// The attributed service pids; empty means the numbers are the runner process only.
    pub service_pids: Vec<u32>,
}

/// Measures several VMs at once: each `(runner pid, workspace)` target gets the runner's usage summed with that of the
/// Virtualization XPC service that has one of the workspace's files (root disk, `efi.bin`) open. The scan of the
/// service processes is done once for all targets. `None` for a target whose runner cannot be read.
pub fn vm_usage(targets: &[(u32, PathBuf)]) -> Vec<Option<VmUsage>> {
    let workspaces: Vec<PathBuf> = targets
        .iter()
        .map(|(_, w)| std::fs::canonicalize(w).unwrap_or_else(|_| w.clone()))
        .collect();
    let services = list_service_files();
    let attributed = attribute_services(&workspaces, &services);
    targets
        .iter()
        .zip(attributed)
        .map(|((pid, _), service_pids)| {
            let mut usage = runner_usage(*pid).ok()?;
            let mut used = Vec::new();
            for sp in service_pids {
                if let Ok(u) = runner_usage(sp) {
                    usage.rss_bytes += u.rss_bytes;
                    usage.cpu_user_seconds += u.cpu_user_seconds;
                    usage.cpu_system_seconds += u.cpu_system_seconds;
                    used.push(sp);
                }
            }
            Some(VmUsage {
                usage,
                service_pids: used,
            })
        })
        .collect()
}

/// Paths of the regular files process `pid` holds open (`proc_pidfdinfo`, `PROC_PIDFDVNODEPATHINFO`). Empty when the
/// process is gone or not ours to inspect.
#[cfg(target_os = "macos")]
fn open_file_paths(pid: libc::pid_t) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // sizeof(struct vnode_fdinfowithpath) = 1200; vip_path starts at offset 176 (verified against the SDK header).
    const FDINFO_WITH_PATH: usize = 1200;
    const PATH_OFFSET: usize = 176;
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
    // SAFETY: size query only.
    let sz = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if sz <= 0 {
        return Vec::new();
    }
    let n_fds = sz as usize / std::mem::size_of::<libc::proc_fdinfo>() + 16;
    // SAFETY: proc_fdinfo is plain old data.
    let mut fds: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; n_fds];
    // SAFETY: `fds` is writable for the byte size passed.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as libc::c_int,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    fds.truncate(got as usize / std::mem::size_of::<libc::proc_fdinfo>());
    let mut files = Vec::new();
    for fd in fds
        .iter()
        .filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
    {
        let mut buf = [0u8; FDINFO_WITH_PATH];
        // SAFETY: `buf` is writable for the length passed.
        let r = unsafe {
            libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                buf.as_mut_ptr().cast(),
                FDINFO_WITH_PATH as libc::c_int,
            )
        };
        if (r as usize) < FDINFO_WITH_PATH {
            return Vec::new();
        }
        let raw = &buf[PATH_OFFSET..];
        let len = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        if len > 0 {
            files.push(PathBuf::from(std::ffi::OsStr::from_bytes(&raw[..len])));
        }
    }
    files
}

/// Every Virtualization XPC service process of this user with the paths of the regular files it holds open. Empty off
/// macOS or when the process table cannot be read.
#[cfg(target_os = "macos")]
fn list_service_files() -> Vec<(u32, Vec<PathBuf>)> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: a null buffer asks for the required size only.
    let need = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if need <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0 as libc::pid_t; need as usize / std::mem::size_of::<libc::pid_t>() + 64];
    // SAFETY: `pids` is writable for the byte size passed.
    let got = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if got <= 0 {
        return Vec::new();
    }
    pids.truncate(got as usize);
    let mut out = Vec::new();
    for pid in pids.into_iter().filter(|p| *p > 0) {
        let mut exe = [0u8; 4096];
        // SAFETY: `exe` is writable for the length passed.
        let n = unsafe { libc::proc_pidpath(pid, exe.as_mut_ptr().cast(), exe.len() as u32) };
        if n <= 0 {
            continue;
        }
        let exe = Path::new(std::ffi::OsStr::from_bytes(&exe[..n as usize]));
        if !is_vz_service_path(exe) {
            continue;
        }
        out.push((pid as u32, open_file_paths(pid)));
    }
    out
}

#[cfg(not(target_os = "macos"))]
fn list_service_files() -> Vec<(u32, Vec<PathBuf>)> {
    Vec::new()
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

#[cfg(test)]
mod attribution_tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn recognises_the_service_executable() {
        assert!(is_vz_service_path(&p(
            "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine"
        )));
        // The runner, and a look-alike outside an .xpc bundle, are not it.
        assert!(!is_vz_service_path(&p("/x/fluxvm-vz-runner")));
        assert!(!is_vz_service_path(&p(
            "/tmp/com.apple.Virtualization.VirtualMachine"
        )));
    }

    #[test]
    fn attributes_by_open_files_in_the_workspace() {
        let ws = vec![p("/state/instances/a"), p("/state/instances/b")];
        let services = vec![
            (
                10,
                vec![
                    p("/usr/lib/dyld"),
                    p("/state/instances/a/efi.bin"),
                    p("/state/instances/a/root.raw"),
                ],
            ),
            (11, vec![p("/state/instances/b/root.raw")]),
            (12, vec![p("/usr/lib/dyld")]),
        ];
        assert_eq!(attribute_services(&ws, &services), vec![vec![10], vec![11]]);
    }

    #[test]
    fn a_workspace_name_is_not_a_prefix_match() {
        // "/state/instances/a" must not claim files of "/state/instances/ab".
        let ws = vec![p("/state/instances/a")];
        let services = vec![(7, vec![p("/state/instances/ab/root.raw")])];
        assert_eq!(attribute_services(&ws, &services), vec![Vec::<u32>::new()]);
    }

    #[test]
    fn an_ambiguous_service_is_attributed_to_none() {
        let ws = vec![p("/s/a"), p("/s/b")];
        let services = vec![(7, vec![p("/s/a/root.raw"), p("/s/b/root.raw")])];
        assert_eq!(
            attribute_services(&ws, &services),
            vec![Vec::<u32>::new(), Vec::new()]
        );
    }

    #[test]
    fn the_longest_workspace_prefix_wins() {
        let ws = vec![p("/s"), p("/s/vm1")];
        let services = vec![(7, vec![p("/s/vm1/root.raw")])];
        assert_eq!(
            attribute_services(&ws, &services),
            vec![Vec::<u32>::new(), vec![7]]
        );
    }

    #[test]
    fn two_services_of_one_vm_are_both_kept() {
        let ws = vec![p("/s/a")];
        let services = vec![(1, vec![p("/s/a/root.raw")]), (2, vec![p("/s/a/efi.bin")])];
        assert_eq!(attribute_services(&ws, &services), vec![vec![1, 2]]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reads_the_open_files_of_a_process() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("root.raw");
        let _held = std::fs::File::create(&f).unwrap();
        let want = std::fs::canonicalize(&f).unwrap();
        let open = open_file_paths(std::process::id() as libc::pid_t);
        assert!(open.contains(&want), "{want:?} not in {open:?}");
        assert!(open_file_paths(4_000_000).is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_vm_with_no_service_reports_the_runner_alone() {
        let dir = tempfile::tempdir().unwrap();
        let got = vm_usage(&[(std::process::id(), dir.path().to_path_buf())]);
        let u = got[0].as_ref().unwrap();
        assert!(u.service_pids.is_empty());
        assert!(u.usage.rss_bytes > 0);
        assert!(vm_usage(&[(4_000_000, dir.path().to_path_buf())])[0].is_none());
    }
}

#[cfg(test)]
mod started_at_tests {
    use super::process_started_at;

    #[cfg(target_os = "macos")]
    #[test]
    fn this_process_started_in_the_past_few_minutes() {
        let t = process_started_at(std::process::id()).expect("our own start time");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(t <= now && now - t < 6 * 3600, "start {t} vs now {now}");
        // A pid that is not there has no start time.
        assert!(process_started_at(u32::MAX - 1).is_none());
    }
}
