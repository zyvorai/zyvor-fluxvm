// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Backups of a FluxVM k3s cluster, as files on this machine: what is there, how old, which to keep, whether there is
//! room for another, and whether a file would restore. `fluxctl kube backup`, `kube backups`, `kube backup verify`,
//! `kube restore` and the Studio Backups card (`/v1/k3s/backups`) all use this, so they say the same thing.
//!
//! The backups themselves are made by `fluxctl kube cluster backup` (a gzip tar of the datastore and the token it needs,
//! with a `.sha256` next to it). Nothing here talks to a cluster; the only I/O is reading and deleting files under
//! `<store>/backups/<cluster>/`, and every function takes the folder, so tests use a temp dir.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Version of the JSON documents. Adding fields does not bump it.
pub const SCHEMA: u32 = 1;
/// Always left free on the disk after a backup.
pub const RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// A backup is assumed to need at least this much room, even when the last one was smaller.
pub const MIN_ESTIMATE_BYTES: u64 = 64 * 1024 * 1024;
const SCHEDULE_FILE: &str = "schedule.json";

/// `~/.fluxvm/apple-k3s` or `$FLUXVM_APPLE_K3S_HOME`: where `fluxctl kube cluster` keeps its journals and backups.
pub fn store_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_APPLE_K3S_HOME") {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".fluxvm").join("apple-k3s"))
}

pub fn backups_root(store: &Path) -> PathBuf {
    store.join("backups")
}

pub fn backups_dir(store: &Path, cluster: &str) -> PathBuf {
    backups_root(store).join(cluster)
}

/// A cluster or backup name that is safe to put in a path: 1-128 of `[A-Za-z0-9._-]`, not starting with a dot.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Why a backup was taken. Only `Manual` backups are never deleted by retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Manual,
    Auto,
    PreUpgrade,
    /// Taken just before a restore replaces the cluster, so the restore itself can be undone.
    PreRestore,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Manual => "manual",
            Kind::Auto => "scheduled",
            Kind::PreUpgrade => "before upgrade",
            Kind::PreRestore => "before restore",
        }
    }

    /// `CLUSTER-<stamp>.tar.gz`, `CLUSTER-auto-<stamp>.tar.gz`, `CLUSTER-pre-upgrade-<stamp>.tar.gz`.
    pub fn file_name(self, cluster: &str, stamp: &str) -> String {
        match self {
            Kind::Manual => format!("{cluster}-{stamp}.tar.gz"),
            Kind::Auto => format!("{cluster}-auto-{stamp}.tar.gz"),
            Kind::PreUpgrade => format!("{cluster}-pre-upgrade-{stamp}.tar.gz"),
            Kind::PreRestore => format!("{cluster}-pre-restore-{stamp}.tar.gz"),
        }
    }

    pub fn of_file(cluster: &str, file: &str) -> Kind {
        let rest = file
            .strip_prefix(cluster)
            .and_then(|r| r.strip_prefix('-'))
            .unwrap_or(file);
        if rest.starts_with("auto-") {
            Kind::Auto
        } else if rest.starts_with("pre-upgrade-") {
            Kind::PreUpgrade
        } else if rest.starts_with("pre-restore-") {
            Kind::PreRestore
        } else {
            Kind::Manual
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupEntry {
    /// The file name, which is what `fluxctl kube restore` and the API take.
    pub name: String,
    pub path: String,
    pub kind: Kind,
    pub bytes: u64,
    pub created_epoch: u64,
    pub age_secs: u64,
    /// A `.sha256` file sits next to it (a backup without one cannot be restored).
    pub has_checksum: bool,
}

/// The backups of one cluster, newest first. Partial files and checksum files are not listed.
pub fn list(dir: &Path, cluster: &str, now: u64) -> Vec<BackupEntry> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut v: Vec<BackupEntry> = rd
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().into_string().ok()?;
            if !name.ends_with(".tar.gz") || !valid_name(&name) {
                return None;
            }
            let md = e.metadata().ok()?;
            if !md.is_file() {
                return None;
            }
            let created = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            Some(BackupEntry {
                kind: Kind::of_file(cluster, &name),
                has_checksum: sidecar(&e.path()).exists(),
                path: e.path().to_string_lossy().into_owned(),
                bytes: md.len(),
                created_epoch: created,
                age_secs: now.saturating_sub(created),
                name,
            })
        })
        .collect();
    v.sort_by(|a, b| {
        b.created_epoch
            .cmp(&a.created_epoch)
            .then(b.name.cmp(&a.name))
    });
    v
}

pub fn sidecar(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_owned();
    s.push(".sha256");
    PathBuf::from(s)
}

/// Deletes a backup and its checksum file.
pub fn delete(file: &Path) -> std::io::Result<()> {
    std::fs::remove_file(file)?;
    match std::fs::remove_file(sidecar(file)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Clusters that have a backups folder.
pub fn clusters(store: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(backups_root(store))
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let e = e.ok()?;
            let n = e.file_name().into_string().ok()?;
            (e.path().is_dir() && valid_name(&n)).then_some(n)
        })
        .collect();
    v.sort();
    v
}

// ---- the schedule ------------------------------------------------------------------------------------------------------

/// The automatic backup policy of one cluster, in `schedule.json` next to its backups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub enabled: bool,
    /// A new backup is taken when the newest one is at least this old.
    pub every_hours: u32,
    /// Scheduled backups kept (the newest N, never fewer than one).
    pub keep: u32,
    /// Pre-upgrade backups kept.
    pub keep_pre_upgrade: u32,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            enabled: true,
            every_hours: 24,
            keep: 7,
            keep_pre_upgrade: 3,
        }
    }
}

impl Schedule {
    pub fn valid(&self) -> Result<(), String> {
        if !(1..=24 * 30).contains(&self.every_hours) {
            return Err("every must be between 1 hour and 30 days".into());
        }
        if !(1..=1000).contains(&self.keep) || !(1..=1000).contains(&self.keep_pre_upgrade) {
            return Err("keep must be between 1 and 1000".into());
        }
        Ok(())
    }
}

/// `None` when the cluster has no policy (an older cluster: it never backs up on its own until one is set).
pub fn load_schedule(dir: &Path) -> Option<Schedule> {
    serde_json::from_slice(&std::fs::read(dir.join(SCHEDULE_FILE)).ok()?).ok()
}

pub fn save_schedule(dir: &Path, s: &Schedule) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{SCHEDULE_FILE}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(serde_json::to_string_pretty(s)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(tmp, dir.join(SCHEDULE_FILE))
}

/// Whether a scheduled backup is due: the policy is on and the newest backup of any kind is old enough (or none exists).
pub fn is_due(s: &Schedule, entries: &[BackupEntry]) -> bool {
    s.enabled
        && entries
            .iter()
            .map(|e| e.age_secs)
            .min()
            .is_none_or(|age| age >= u64::from(s.every_hours) * 3600)
}

/// The names retention would delete: scheduled backups beyond the newest `keep`, pre-upgrade backups beyond the newest
/// `keep_pre_upgrade`, oldest first. Manual backups are never in the list. `entries` is newest first, as [`list`] gives it.
pub fn prune_plan(entries: &[BackupEntry], s: &Schedule) -> Vec<String> {
    let mut out = vec![];
    for (kind, keep) in [
        (Kind::Auto, s.keep),
        (Kind::PreUpgrade, s.keep_pre_upgrade),
        (Kind::PreRestore, s.keep_pre_upgrade),
    ] {
        let of: Vec<&BackupEntry> = entries.iter().filter(|e| e.kind == kind).collect();
        out.extend(
            of.iter()
                .skip(keep.max(1) as usize)
                .rev()
                .map(|e| e.name.clone()),
        );
    }
    out
}

// ---- disk space --------------------------------------------------------------------------------------------------------

/// Free bytes on the filesystem holding `path` (or its nearest existing parent).
pub fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.to_str()?).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a plain-old-data out parameter that statvfs fills.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// What the next backup is expected to need: the newest one's size plus half again (data grows), at least
/// [`MIN_ESTIMATE_BYTES`].
pub fn needed_bytes(entries: &[BackupEntry]) -> u64 {
    let last = entries.first().map_or(0, |e| e.bytes);
    (last + last / 2).max(MIN_ESTIMATE_BYTES)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Room {
    /// Enough free space.
    Enough,
    /// Enough once these old scheduled or pre-upgrade backups are deleted (oldest first; the newest of each kind and every
    /// manual backup are kept).
    MakeRoom(Vec<String>),
    /// Not enough, even after deleting what retention may delete.
    Short { free: u64, needed: u64 },
}

/// Whether a backup can be written without leaving less than [`RESERVE_BYTES`] free. `free` of `None` (unknown) is treated
/// as enough: the write itself would fail first.
pub fn room(free: Option<u64>, entries: &[BackupEntry]) -> Room {
    let Some(free) = free else {
        return Room::Enough;
    };
    let needed = needed_bytes(entries);
    let want = needed.saturating_add(RESERVE_BYTES);
    if free >= want {
        return Room::Enough;
    }
    let mut avail = free;
    let mut del = vec![];
    for kind in [Kind::Auto, Kind::PreUpgrade, Kind::PreRestore] {
        let of: Vec<&BackupEntry> = entries.iter().filter(|e| e.kind == kind).collect();
        for e in of.iter().skip(1).rev() {
            if avail >= want {
                break;
            }
            avail = avail.saturating_add(e.bytes);
            del.push(e.name.clone());
        }
    }
    if avail >= want {
        Room::MakeRoom(del)
    } else {
        Room::Short { free, needed: want }
    }
}

// ---- verifying a backup ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub file: String,
    /// Every check passed: the file is intact and holds what a restore needs.
    pub restorable: bool,
    /// `sqlite` or `etcd`, from what is inside.
    pub datastore: Option<String>,
    pub sha256: Option<String>,
    pub checks: Vec<Check>,
}

impl VerifyReport {
    /// The first failing check, in words.
    pub fn problem(&self) -> Option<String> {
        self.checks
            .iter()
            .find(|c| !c.ok)
            .map(|c| format!("{}: {}", c.name, c.detail))
    }
}

fn ck(name: &str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok,
        detail: detail.into(),
    }
}

/// Reads a whole backup file and says whether a restore would find what it needs: the file is not empty, matches its
/// `.sha256`, is a complete gzip tar (a truncated copy fails here), holds the cluster token and a datastore (`db/state.db`
/// that starts with the SQLite header, or an etcd snapshot under `db/snapshots/`), and that datastore is the one the
/// cluster uses (`expect`, when known). It does not start k3s: only a restore proves the data inside is consistent.
pub fn verify_file(file: &Path, expect: Option<&str>) -> VerifyReport {
    let mut r = VerifyReport {
        file: file.to_string_lossy().into_owned(),
        restorable: false,
        datastore: None,
        sha256: None,
        checks: vec![],
    };
    let Ok(f) = std::fs::File::open(file) else {
        r.checks.push(ck("file", false, "cannot be opened"));
        return r;
    };
    let len = f.metadata().map_or(0, |m| m.len());
    if len < 32 {
        r.checks.push(ck(
            "file",
            false,
            format!("{len} bytes: empty or cut short"),
        ));
        return r;
    }
    r.checks.push(ck("file", true, format!("{len} bytes")));

    // checksum, streaming
    let mut h = Sha256::new();
    {
        let mut rd = std::io::BufReader::new(&f);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            match rd.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => h.update(&buf[..n]),
                Err(e) => {
                    r.checks
                        .push(ck("checksum", false, format!("read failed: {e}")));
                    return r;
                }
            }
        }
    }
    let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    match std::fs::read_to_string(sidecar(file)) {
        Ok(t) => {
            let want = t
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if want == got {
                r.checks
                    .push(ck("checksum", true, "matches the .sha256 file"));
            } else {
                r.checks.push(ck(
                    "checksum",
                    false,
                    "does not match the .sha256 file: the file changed after it was made",
                ));
            }
        }
        Err(_) => r.checks.push(ck(
            "checksum",
            false,
            "there is no .sha256 file next to it, so nothing proves it is whole",
        )),
    }
    r.sha256 = Some(got);

    // archive: read every byte so a truncated stream fails
    let Ok(f) = std::fs::File::open(file) else {
        return r;
    };
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(std::io::BufReader::new(f)));
    let mut token = None;
    let mut sqlite: Option<bool> = None;
    let mut snapshot = false;
    let mut walk = || -> std::io::Result<()> {
        for e in ar.entries()? {
            let mut e = e?;
            let p = e
                .path()?
                .to_string_lossy()
                .trim_start_matches("./")
                .trim_end_matches('/')
                .to_owned();
            if p == "token" {
                let mut b = Vec::new();
                e.read_to_end(&mut b)?;
                token = Some(!b.iter().all(u8::is_ascii_whitespace));
            } else if p == "db/state.db" {
                let mut head = [0u8; 16];
                let n = e.read(&mut head)?;
                std::io::copy(&mut e, &mut std::io::sink())?;
                sqlite = Some(n == 16 && &head == b"SQLite format 3\0");
            } else if p.starts_with("db/snapshots/")
                && e.header().entry_type().is_file()
                && e.size() > 0
            {
                snapshot = true;
                std::io::copy(&mut e, &mut std::io::sink())?;
            } else {
                std::io::copy(&mut e, &mut std::io::sink())?;
            }
        }
        Ok(())
    };
    match walk() {
        Ok(()) => r.checks.push(ck("archive", true, "a complete gzip tar")),
        Err(e) => {
            r.checks.push(ck(
                "archive",
                false,
                format!("not a complete gzip tar ({e}): the file is damaged or cut short"),
            ));
            return r;
        }
    }
    r.checks.push(match token {
        Some(true) => ck("token", true, "the cluster token is inside"),
        Some(false) => ck("token", false, "the token file is empty"),
        None => ck(
            "token",
            false,
            "no cluster token inside: a restore could not rejoin the nodes",
        ),
    });
    let ds = match (sqlite, snapshot) {
        (Some(true), _) => {
            r.checks
                .push(ck("datastore", true, "SQLite database with a valid header"));
            Some("sqlite")
        }
        (Some(false), _) => {
            r.checks.push(ck(
                "datastore",
                false,
                "db/state.db is not a SQLite database",
            ));
            None
        }
        (None, true) => {
            r.checks.push(ck("datastore", true, "etcd snapshot"));
            Some("etcd")
        }
        (None, false) => {
            r.checks.push(ck(
                "datastore",
                false,
                "no datastore inside (neither db/state.db nor an etcd snapshot)",
            ));
            None
        }
    };
    r.datastore = ds.map(str::to_owned);
    if let (Some(want), Some(have)) = (expect, ds) {
        r.checks.push(if want == have {
            ck("cluster", true, format!("the cluster uses {want}"))
        } else {
            ck(
                "cluster",
                false,
                format!("the backup holds {have} but the cluster uses {want}"),
            )
        });
    }
    r.restorable = r.checks.iter().all(|c| c.ok);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn entry(name: &str, kind: Kind, age: u64, bytes: u64) -> BackupEntry {
        BackupEntry {
            name: name.into(),
            path: name.into(),
            kind,
            bytes,
            created_epoch: 1_000_000 - age,
            age_secs: age,
            has_checksum: true,
        }
    }

    fn archive(dir: &Path, name: &str, files: &[(&str, &[u8])], sum: bool) -> PathBuf {
        let p = dir.join(name);
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(&p).unwrap(),
            flate2::Compression::fast(),
        );
        let mut b = tar::Builder::new(gz);
        for (n, data) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o600);
            h.set_cksum();
            b.append_data(&mut h, n, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
        if sum {
            let d = std::fs::read(&p).unwrap();
            let hex: String = Sha256::digest(&d)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            std::fs::write(sidecar(&p), format!("{hex}  {name}\n")).unwrap();
        }
        p
    }

    fn sqlite_bytes() -> Vec<u8> {
        let mut v = b"SQLite format 3\0".to_vec();
        v.extend(vec![0u8; 600]);
        v
    }

    #[test]
    fn names_carry_the_kind_and_round_trip() {
        for k in [Kind::Manual, Kind::Auto, Kind::PreUpgrade, Kind::PreRestore] {
            let f = k.file_name("dev", "20261121-101500");
            assert_eq!(Kind::of_file("dev", &f), k, "{f}");
            assert!(valid_name(&f));
        }
        assert!(
            !valid_name("../x") && !valid_name(".hidden") && !valid_name("a/b") && !valid_name("")
        );
    }

    #[test]
    fn listing_is_newest_first_and_skips_checksums_and_partials() {
        let d = tempfile::tempdir().unwrap();
        archive(
            d.path(),
            "dev-20260101-000000.tar.gz",
            &[("token", b"t")],
            true,
        );
        std::fs::write(d.path().join("dev-x.tar.gz.partial"), b"x").unwrap();
        let l = list(d.path(), "dev", u64::MAX / 2);
        assert_eq!(l.len(), 1);
        assert!(l[0].has_checksum && l[0].kind == Kind::Manual);
        assert!(list(&d.path().join("nope"), "dev", 0).is_empty());
    }

    #[test]
    fn retention_keeps_newest_n_of_each_kind_and_never_touches_manual() {
        let s = Schedule {
            keep: 2,
            keep_pre_upgrade: 1,
            ..Schedule::default()
        };
        let e = vec![
            entry("a1", Kind::Auto, 10, 1),
            entry("m1", Kind::Manual, 20, 1),
            entry("a2", Kind::Auto, 30, 1),
            entry("p1", Kind::PreUpgrade, 35, 1),
            entry("a3", Kind::Auto, 40, 1),
            entry("p2", Kind::PreUpgrade, 50, 1),
            entry("a4", Kind::Auto, 60, 1),
        ];
        assert_eq!(prune_plan(&e, &s), ["a4", "a3", "p2"]);
        let zero = Schedule { keep: 0, ..s };
        assert!(
            prune_plan(&e, &zero).iter().all(|n| n != "a1"),
            "at least one is always kept"
        );
    }

    #[test]
    fn due_when_newest_is_old_enough_or_missing_and_only_when_on() {
        let s = Schedule::default();
        assert!(is_due(&s, &[]));
        assert!(!is_due(&s, &[entry("a", Kind::Auto, 3600, 1)]));
        assert!(!is_due(
            &s,
            &[
                entry("a", Kind::Auto, 90_000, 1),
                entry("m", Kind::Manual, 100, 1)
            ]
        ));
        assert!(is_due(&s, &[entry("a", Kind::Auto, 90_000, 1)]));
        assert!(!is_due(
            &Schedule {
                enabled: false,
                ..s
            },
            &[]
        ));
    }

    #[test]
    fn room_prunes_old_scheduled_backups_before_refusing_and_never_manual() {
        const G: u64 = 1024 * 1024 * 1024;
        let e = vec![
            entry("a1", Kind::Auto, 10, G),
            entry("m1", Kind::Manual, 20, 5 * G),
            entry("a2", Kind::Auto, 30, G),
            entry("a3", Kind::Auto, 40, G),
        ];
        assert_eq!(room(Some(10 * G), &e), Room::Enough);
        assert_eq!(room(None, &e), Room::Enough);
        // needs 1.5 G + 2 G reserve = 3.5 G; has 2 G; deleting a3 (1 G) gives 3 G, a2 gives 4 G
        assert_eq!(
            room(Some(2 * G), &e),
            Room::MakeRoom(vec!["a3".into(), "a2".into()])
        );
        assert!(matches!(room(Some(0), &e), Room::Short { .. }));
        let only_manual = vec![entry("m1", Kind::Manual, 20, 5 * G)];
        assert!(matches!(room(Some(G), &only_manual), Room::Short { .. }));
    }

    #[test]
    fn a_good_sqlite_backup_verifies() {
        let d = tempfile::tempdir().unwrap();
        let sq = sqlite_bytes();
        let p = archive(
            d.path(),
            "b.tar.gz",
            &[("token", b"K10abc::server:x"), ("db/state.db", &sq)],
            true,
        );
        let r = verify_file(&p, Some("sqlite"));
        assert!(r.restorable, "{:?}", r.checks);
        assert_eq!(r.datastore.as_deref(), Some("sqlite"));
    }

    #[test]
    fn an_etcd_backup_verifies_and_the_wrong_datastore_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = archive(
            d.path(),
            "e.tar.gz",
            &[("token", b"t"), ("db/snapshots/fluxvm-1", b"snap")],
            true,
        );
        assert!(verify_file(&p, Some("etcd")).restorable);
        let r = verify_file(&p, Some("sqlite"));
        assert!(!r.restorable);
        assert!(r.problem().unwrap().starts_with("cluster:"));
    }

    #[test]
    fn damage_is_caught_missing_checksum_changed_file_truncation_missing_pieces() {
        let d = tempfile::tempdir().unwrap();
        let sq = sqlite_bytes();
        let good = archive(
            d.path(),
            "g.tar.gz",
            &[("token", b"t"), ("db/state.db", &sq)],
            true,
        );
        // no checksum
        let nosum = archive(
            d.path(),
            "n.tar.gz",
            &[("token", b"t"), ("db/state.db", &sq)],
            false,
        );
        assert!(
            verify_file(&nosum, None)
                .problem()
                .unwrap()
                .starts_with("checksum")
        );
        // truncated after the checksum was written
        let bytes = std::fs::read(&good).unwrap();
        let cut = d.path().join("c.tar.gz");
        std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();
        std::fs::copy(sidecar(&good), sidecar(&cut)).unwrap();
        let r = verify_file(&cut, None);
        assert!(!r.restorable);
        assert!(r.checks.iter().any(|c| c.name == "archive" && !c.ok));
        // changed file
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&good)
            .unwrap();
        f.write_all(b"x").unwrap();
        assert!(
            verify_file(&good, None)
                .problem()
                .unwrap()
                .starts_with("checksum")
        );
        // no token, not sqlite, empty
        let a = archive(d.path(), "t.tar.gz", &[("db/state.db", &sq)], true);
        assert!(
            verify_file(&a, None)
                .problem()
                .unwrap()
                .starts_with("token")
        );
        let b = archive(
            d.path(),
            "s.tar.gz",
            &[("token", b"t"), ("db/state.db", b"not sqlite at all")],
            true,
        );
        assert!(
            verify_file(&b, None)
                .problem()
                .unwrap()
                .starts_with("datastore")
        );
        let e = d.path().join("e0.tar.gz");
        std::fs::write(&e, b"").unwrap();
        assert!(!verify_file(&e, None).restorable);
        assert!(!verify_file(&d.path().join("missing"), None).restorable);
    }

    #[test]
    fn schedule_round_trips_and_validates() {
        let d = tempfile::tempdir().unwrap();
        assert!(load_schedule(d.path()).is_none());
        let s = Schedule {
            every_hours: 6,
            ..Schedule::default()
        };
        save_schedule(d.path(), &s).unwrap();
        assert_eq!(load_schedule(d.path()), Some(s));
        assert!(
            Schedule {
                every_hours: 0,
                ..Schedule::default()
            }
            .valid()
            .is_err()
        );
        assert!(
            Schedule {
                keep: 0,
                ..Schedule::default()
            }
            .valid()
            .is_err()
        );
        assert!(Schedule::default().valid().is_ok());
    }
}
