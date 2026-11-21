// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac-fleet`: leases shared between processes on one machine.
//!
//! `Leases` (mac_fleet_sched) are in-process: two `fluxctl` processes (two runners, a runner and a `mac-fleet run`) each
//! believe a Mac's slot is free until the next probe shows the guest. Here the leases also live in a small JSON file next
//! to the fleet state (`mac-fleet-leases.json`), guarded by an advisory `flock` on `mac-fleet-leases.lock`. A placement
//! takes the lock, reads the other processes' leases, decides, writes its own, and unlocks: the critical section is pure
//! computation (the probes happen before it), so it is short.
//!
//! Limits, on purpose: the lock is a local `flock`, so it coordinates processes on ONE machine and a file system with
//! working locks (not NFS). Two supervisors on two machines are not coordinated; the Mac's own two-guest limit and the
//! next probe remain the backstop. A lease of a process that died is dropped at once (its pid is gone) or at the latest
//! after the lease time to live. Waiting placements poll; there is no first-come-first-served order between processes.
//! `FLUXVM_MAC_FLEET_NO_SHARED_LEASES=1` turns this off.

use crate::mac_fleet_sched::{JobSpec, LEASE_TTL_SECS, Leases, TemplateRef};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rec {
    pub id: String,
    pub host: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub expires: u64,
    pub pid: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileDoc {
    #[serde(default)]
    leases: Vec<Rec>,
}

struct Shared {
    file: PathBuf,
    lock: PathBuf,
    /// Ids imported from other processes at the last `begin`; dropped again before the next import.
    foreign: Vec<String>,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

/// In unit tests sharing is switched on per thread, so the tests of the placer that run beside the tests of this module
/// never see a file they did not ask for.
#[cfg(test)]
thread_local! {
    static ENABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn enabled() -> bool {
    #[cfg(test)]
    {
        ENABLED.with(|e| e.get())
    }
    #[cfg(not(test))]
    {
        true
    }
}

/// The lease and lock file of the installed sharing, taken out of the mutex so that nothing waits on a file lock while
/// holding it (two threads, each holding one of the two, would otherwise wait for each other).
fn paths() -> Option<(PathBuf, PathBuf)> {
    if !enabled() {
        return None;
    }
    SHARED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|s| (s.file.clone(), s.lock.clone()))
}

pub fn files_for(state: &Path) -> (PathBuf, PathBuf) {
    let d = state.parent().map(Path::to_path_buf).unwrap_or_default();
    (d.join("mac-fleet-leases.json"), d.join("mac-fleet-leases.lock"))
}

/// Turn sharing on for this process (the state file's directory holds the lease files). A no-op when
/// `FLUXVM_MAC_FLEET_NO_SHARED_LEASES` is set.
pub fn install(state: &Path) {
    // Unit tests of the placer must not share a file with each other; the tests here call `install_at`.
    if cfg!(test) || std::env::var_os("FLUXVM_MAC_FLEET_NO_SHARED_LEASES").is_some() {
        return;
    }
    install_at(state);
}

fn install_at(state: &Path) {
    let (file, lock) = files_for(state);
    *SHARED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Shared { file, lock, foreign: vec![] });
    #[cfg(test)]
    ENABLED.with(|e| e.set(true));
}

#[cfg(test)]
pub fn uninstall() {
    *SHARED.lock().unwrap_or_else(|e| e.into_inner()) = None;
    ENABLED.with(|e| e.set(false));
}

#[cfg(test)]
fn enable_for_thread() {
    ENABLED.with(|e| e.set(true));
}

/// Held while a placement decides; dropping it unlocks.
pub struct Guard {
    _lock: File,
    doc: FileDoc,
}

fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    // Signal 0 only checks: EPERM still means the process exists.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn read_doc(p: &Path) -> FileDoc {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_doc(p: &Path, d: &FileDoc) -> Result<()> {
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(d)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

fn lock_file(p: &Path) -> Result<File> {
    use std::os::unix::io::AsRawFd;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(p)
        .with_context(|| format!("opening {}", p.display()))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking the lease file");
    }
    Ok(f)
}

fn imported_job(r: &Rec) -> JobSpec {
    JobSpec {
        template: TemplateRef { name: String::new(), digest: None },
        cpus: r.cpus,
        memory_mib: r.memory_mib,
        disk_gib: 0,
        labels: vec![],
        ac_only: false,
        host: None,
    }
}

/// Lock and pull the other processes' live leases into `leases`. `None` when sharing is off.
pub fn begin(leases: &mut Leases, now: u64) -> Result<Option<Guard>> {
    let Some((file, lock_path)) = paths() else { return Ok(None) };
    let lock = lock_file(&lock_path)?;
    let mut doc = read_doc(&file);
    doc.leases.retain(|r| r.expires > now && pid_alive(r.pid));
    let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = g.as_mut() else { return Ok(None) };
    for id in s.foreign.drain(..) {
        leases.release(&id);
    }
    let me = std::process::id();
    for r in doc.leases.iter().filter(|r| r.pid != me) {
        leases.acquire(&r.id, &r.host, &imported_job(r), now);
        s.foreign.push(r.id.clone());
    }
    Ok(Some(Guard { _lock: lock, doc }))
}

/// Write this process's new lease (if the decision placed one) and unlock.
pub fn commit(guard: Option<Guard>, placed: Option<(&str, &str, &JobSpec)>, now: u64) -> Result<()> {
    let Some(mut guard) = guard else { return Ok(()) };
    let Some((file, _)) = paths() else { return Ok(()) };
    if let Some((id, host, job)) = placed {
        guard.doc.leases.retain(|r| r.id != id);
        guard.doc.leases.push(Rec {
            id: id.into(),
            host: host.into(),
            cpus: job.cpus,
            memory_mib: job.memory_mib,
            expires: now + LEASE_TTL_SECS,
            pid: std::process::id(),
        });
    }
    write_doc(&file, &guard.doc)
}

/// Drop this process's lease `id` from the file.
pub fn forget(id: &str) {
    let Some((file, lock_path)) = paths() else { return };
    if let Ok(_lock) = lock_file(&lock_path) {
        let mut doc = read_doc(&file);
        doc.leases.retain(|r| r.id != id);
        let _ = write_doc(&file, &doc);
    }
}

/// The live leases in the file, for `mac-fleet status`.
pub fn live(state: &Path, now: u64) -> Vec<Rec> {
    let (file, _) = files_for(state);
    read_doc(&file)
        .leases
        .into_iter()
        .filter(|r| r.expires > now && pid_alive(r.pid))
        .collect()
}

/// Ids of live leases held by other processes: a guest or runner with such an id is somebody's, not an orphan.
pub fn claimed(state: &Path, now: u64) -> Vec<String> {
    live(state, now).into_iter().map(|r| r.id).collect()
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> JobSpec {
        imported_job(&Rec { id: String::new(), host: String::new(), cpus: 4, memory_mib: 8192, expires: 0, pid: 0 })
    }

    #[test]
    fn a_second_process_sees_the_first_ones_lease() {
        let _t = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state.json");
        install_at(&state);
        // Process A (this one) takes a slot on mini-1.
        let mut a = Leases::default();
        let g = begin(&mut a, 100).unwrap();
        assert!(g.is_some());
        a.acquire("a1", "lt-mac-1", &job(), 100);
        commit(g, Some(("a1", "lt-mac-1", &job())), 100).unwrap();
        // Process B is simulated as a record by another live pid (the parent of this process).
        let (file, _) = files_for(&state);
        let mut doc = read_doc(&file);
        let other = unsafe { libc::getppid() } as u32;
        doc.leases.push(Rec { id: "b1".into(), host: "lt-mac-1".into(), cpus: 4, memory_mib: 8192, expires: 100 + LEASE_TTL_SECS, pid: other });
        write_doc(&file, &doc).unwrap();
        // A looks again: both are counted, once each.
        let g = begin(&mut a, 101).unwrap();
        assert_eq!(a.usage("lt-mac-1", 101).macos, 2);
        commit(g, None, 101).unwrap();
        let g = begin(&mut a, 102).unwrap();
        assert_eq!(a.usage("lt-mac-1", 102).macos, 2, "imports are replaced, not piled up");
        commit(g, None, 102).unwrap();
        assert_eq!(live(&state, 102).len(), 2);
        // Releasing removes it from the file for everybody.
        a.release("a1");
        forget("a1");
        assert_eq!(live(&state, 102).iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["b1"]);
        uninstall();
    }

    #[test]
    fn leases_of_dead_or_expired_processes_are_dropped() {
        let _t = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state.json");
        install_at(&state);
        let (file, _) = files_for(&state);
        let dead = FileDoc {
            leases: vec![
                Rec { id: "dead".into(), host: "lt-m".into(), cpus: 0, memory_mib: 0, expires: 1000, pid: 0x7ffffff0 },
                Rec { id: "old".into(), host: "lt-m".into(), cpus: 0, memory_mib: 0, expires: 50, pid: unsafe { libc::getppid() } as u32 },
            ],
        };
        write_doc(&file, &dead).unwrap();
        let mut l = Leases::default();
        let g = begin(&mut l, 100).unwrap();
        assert_eq!(l.usage("lt-m", 100).macos, 0);
        commit(g, None, 100).unwrap();
        assert!(live(&state, 100).is_empty());
        uninstall();
    }

    #[test]
    fn concurrent_commits_keep_the_file_complete() {
        let _t = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state.json");
        install_at(&state);
        // Eight threads commit at once; the flock serialises read-modify-write, so none is lost.
        std::thread::scope(|s| {
            for i in 0..8 {
                s.spawn(move || {
                    enable_for_thread();
                    let mut l = Leases::default();
                    let g = begin(&mut l, 100).unwrap();
                    commit(g, Some((&format!("t{i}"), "lt-m", &job())), 100).unwrap();
                });
            }
        });
        assert_eq!(live(&state, 100).len(), 8);
        assert_eq!(claimed(&state, 100).len(), 8);
        uninstall();
    }
}
