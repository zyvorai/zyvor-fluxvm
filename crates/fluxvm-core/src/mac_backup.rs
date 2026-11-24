// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Backups of `fluxctl mac` instances and named volumes: one compressed archive plus a manifest with its sha256 and what it holds.
//! Shared by `fluxctl mac backup ...` and the Studio API (`/v1/mac/backups`), so both write and read the same files.
//!
//! Layout, under `FLUXVM_MAC_BACKUPS` (default `~/.fluxvm/mac-backups`), one flat folder:
//!
//! * `ID.tar.gz`   the archive (an instance: `disk.raw`, `hardware.bin`, `auxiliary.bin`; a volume: `data/...`);
//! * `ID.json`     the manifest, written last: a backup exists once its manifest does, and the manifest carries the archive's
//!   sha256, so a half-written archive is never listed as a backup;
//! * `ID.part`     the archive while it is being written; `ID.error` why a background backup failed;
//! * `schedule.json` the automatic backups (`fluxctl mac backup schedule`).
//!
//! What is NOT captured is listed in every manifest and printed by the commands: FluxVM's secret values, registry logins, saved
//! memory. A volume's extended attributes and ACLs are not kept (the archive stores files, permissions and symlinks).
//!
//! Disk safety: a Mac's internal disk is often almost full, so a backup is refused up front, in plain words with the amount needed,
//! when the free space would drop below the data's size plus a 2 GiB reserve.
//!
//! Not run against a real Mac; the tests use temporary folders and fake free-space figures.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const SCHEMA: u32 = 1;
/// Free space kept after a backup, so macOS itself never runs out of room.
pub const RESERVE_BYTES: u64 = 2 << 30;
pub const DEFAULT_KEEP: u32 = 7;
/// Automatic safety-net backups (taken before a restore replaces something) kept per instance or volume.
pub const AUTO_KEEP: usize = 3;
/// A `.part` file this old belongs to a backup that was interrupted.
const STALE_PART_SECS: u64 = 24 * 3600;
const SCHEDULE_FILE: &str = "schedule.json";
const RESTART_LABEL: &str = "fluxvm.mac.restart";

/// What a backup never holds, as printed to people.
pub const EXCLUDED: &[&str] = &[
    "FluxVM secret values (`fluxctl mac secret`, `-e` environment values): they are never read into a backup",
    "registry logins (`fluxctl mac login`)",
    "saved memory of a running guest (a backup is of the stopped disk)",
];

const SECRET_WORDS: [&str; 5] = ["secret", "token", "password", "credential", "passwd"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupError {
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BackupError {}

pub type Result<T> = std::result::Result<T, BackupError>;

fn err<T>(code: &'static str, message: impl Into<String>) -> Result<T> {
    Err(BackupError {
        code,
        message: message.into(),
    })
}

fn io_err(what: &str, e: &std::io::Error) -> BackupError {
    let full = e.raw_os_error() == Some(28);
    BackupError {
        code: "MAC_BACKUP_IO",
        message: format!(
            "{what}: {e}.{}",
            if full {
                " The disk is full: free room or point FLUXVM_MAC_BACKUPS at a bigger disk"
            } else {
                " Check that the backups folder is writable and the disk has room"
            }
        ),
    }
}

pub fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f >= K * K * K {
        format!("{:.1} GiB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MiB", f / (K * K))
    } else if f >= K {
        format!("{:.1} KiB", f / K)
    } else {
        format!("{b} B")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Instance,
    Volume,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Instance => "instance",
            Kind::Volume => "volume",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
    /// Files below a folder (1 for a plain file).
    pub files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    pub host: u16,
    pub guest: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceMeta {
    pub labels: BTreeMap<String, String>,
    pub restart_policy: String,
    pub vcpus: Option<u64>,
    pub memory_mib: Option<u64>,
    pub forwards: Vec<Forward>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeMeta {
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub created_unix: u64,
    /// A safety-net backup taken before a restore replaced something; kept separately from `--keep`.
    #[serde(default)]
    pub automatic: bool,
    #[serde(default)]
    pub note: Option<String>,
    pub archive: String,
    pub sha256: String,
    pub archive_bytes: u64,
    /// Allocated bytes of what was captured, before compression.
    pub source_bytes: u64,
    pub files: Vec<FileEntry>,
    /// Plain-English list of what the backup holds.
    pub captured: Vec<String>,
    pub excluded: Vec<String>,
    #[serde(default)]
    pub instance: Option<InstanceMeta>,
    #[serde(default)]
    pub volume: Option<VolumeMeta>,
}

/// One thing to put in the archive: a file or a folder, under `archive_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub archive_path: String,
    pub path: PathBuf,
}

/// What to back up, worked out from an instance record or a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub kind: Kind,
    pub name: String,
    pub sources: Vec<Source>,
    pub instance: Option<InstanceMeta>,
    pub volume: Option<VolumeMeta>,
    pub captured: Vec<String>,
    pub excluded: Vec<String>,
}

fn looks_secret(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| k.contains(w))
}

fn split_secret_labels(
    labels: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut keep = BTreeMap::new();
    let mut dropped = Vec::new();
    for (k, v) in labels {
        if looks_secret(k) {
            dropped.push(format!("label {k} (its name looks like a secret)"));
        } else {
            keep.insert(k.clone(), v.clone());
        }
    }
    (keep, dropped)
}

/// A plan for one instance from its VM record (`GET /v1/vms/{id}`). Only a stopped instance has a consistent disk.
pub fn instance_plan(vm: &Value) -> Result<Plan> {
    let name = vm["name"].as_str().unwrap_or_default().to_string();
    let status = vm["status"].as_str().unwrap_or("unknown");
    if !matches!(status, "stopped" | "failed") {
        return err(
            "MAC_BACKUP_RUNNING",
            format!(
                "{name} is {status}: a disk that is being written is not a consistent backup. Stop it first with `fluxctl mac stop {name}`, back it up, then start it again"
            ),
        );
    }
    let Some(disk) = vm["disk"].as_str().map(PathBuf::from) else {
        return err(
            "MAC_BACKUP_SOURCE",
            format!("the daemon has no disk recorded for {name}"),
        );
    };
    let ws = vm["workspace"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| disk.parent().map(Path::to_path_buf).unwrap_or_default());
    let sources = vec![
        Source {
            archive_path: "disk.raw".into(),
            path: disk,
        },
        Source {
            archive_path: "hardware.bin".into(),
            path: ws.join("hardware.bin"),
        },
        Source {
            archive_path: "auxiliary.bin".into(),
            path: ws.join("auxiliary.bin"),
        },
    ];
    for s in &sources {
        if !s.path.is_file() {
            return err(
                "MAC_BACKUP_SOURCE",
                format!(
                    "{} is not readable here: back up on the Mac whose daemon owns {name} (or the instance has not finished being created)",
                    s.path.display()
                ),
            );
        }
    }
    let raw: BTreeMap<String, String> = vm["labels"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let (labels, mut excluded) = split_secret_labels(&raw);
    let restart_policy = raw
        .get(RESTART_LABEL)
        .cloned()
        .unwrap_or_else(|| "no".into());
    let forwards = vm["request"]["network"]["forwards"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| {
                    Some(Forward {
                        host: u16::try_from(f["host_port"].as_u64()?).ok()?,
                        guest: u16::try_from(f["guest_port"].as_u64()?).ok()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let (vcpus, memory_mib) = (
        vm["request"]["vcpus"].as_u64(),
        vm["request"]["memory_mib"].as_u64(),
    );
    let mut captured = vec![
        "the instance's disk and macOS hardware identity (disk.raw, hardware.bin, auxiliary.bin)".to_string(),
        format!("its {} label(s), including the restart policy ({restart_policy})", labels.len()),
        "its size (CPUs, memory) and published ports".to_string(),
        "everything stored inside the guest's disk, including any keychain items or files you put there".to_string(),
    ];
    captured.dedup();
    excluded.extend(EXCLUDED.iter().map(|s| s.to_string()));
    Ok(Plan {
        kind: Kind::Instance,
        name,
        sources,
        instance: Some(InstanceMeta {
            labels,
            restart_policy,
            vcpus,
            memory_mib,
            forwards,
        }),
        volume: None,
        captured,
        excluded,
    })
}

/// A plan for a named volume: its data folder and its labels.
pub fn volume_plan(name: &str, data: &Path, labels: &BTreeMap<String, String>) -> Result<Plan> {
    if !data.is_dir() {
        return err(
            "MAC_BACKUP_SOURCE",
            format!(
                "the data folder of volume {name} ({}) does not exist here",
                data.display()
            ),
        );
    }
    let (labels, mut excluded) = split_secret_labels(labels);
    excluded.extend(EXCLUDED.iter().take(2).map(|s| s.to_string()));
    excluded.push(
        "extended attributes and ACLs of the files (files, permissions and symlinks are kept)"
            .into(),
    );
    Ok(Plan {
        kind: Kind::Volume,
        name: name.to_string(),
        sources: vec![Source {
            archive_path: "data".into(),
            path: data.to_path_buf(),
        }],
        instance: None,
        volume: Some(VolumeMeta { labels }),
        captured: vec![
            "every file in the volume's data folder, with permissions and symlinks".into(),
            "the volume's labels".into(),
        ],
        excluded,
    })
}

// ------------------------------------------------------------------------------------------------------------ folders

pub fn default_root() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_BACKUPS").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".fluxvm")
            .join("mac-backups"),
    )
}

pub fn free_bytes(path: &Path) -> Option<u64> {
    crate::k3s_backups::free_bytes(path)
}

fn alloc_bytes(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    let mut n = m.blocks() * 512;
    if m.is_dir()
        && let Ok(rd) = std::fs::read_dir(p)
    {
        n += rd.flatten().map(|e| alloc_bytes(&e.path())).sum::<u64>();
    }
    n
}

/// Allocated bytes a plan's sources hold: what [`check_room`] is asked about.
pub fn needed_bytes(plan: &Plan) -> u64 {
    plan.sources.iter().map(|s| alloc_bytes(&s.path)).sum()
}

fn count_files(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if m.is_dir() {
        std::fs::read_dir(p)
            .map(|rd| rd.flatten().map(|e| count_files(&e.path())).sum())
            .unwrap_or(0)
    } else {
        1
    }
}

fn slug(n: &str) -> String {
    let s: String = n
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    s.trim_matches(['-', '.']).chars().take(48).collect()
}

fn stamp(now: u64) -> String {
    chrono::DateTime::from_timestamp(now as i64, 0)
        .unwrap_or_default()
        .format("%Y%m%d-%H%M%S")
        .to_string()
}

/// A fresh id such as `instance-ci-20261124-101500` (a counter is added when one exists already).
pub fn new_id(root: &Path, kind: Kind, name: &str, now: u64) -> String {
    let base = format!("{}-{}-{}", kind.word(), slug(name), stamp(now));
    let taken = |id: &str| {
        ["json", "part", "tar.gz"]
            .iter()
            .any(|e| root.join(format!("{id}.{e}")).exists())
    };
    let mut id = base.clone();
    let mut n = 2;
    while taken(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// An id names files in the backups folder, so it may not name anything else.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && id != "schedule"
}

fn path_of(root: &Path, id: &str, ext: &str) -> PathBuf {
    root.join(format!("{id}.{ext}"))
}

// ------------------------------------------------------------------------------------------------------------ create

struct HashWrite<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> Write for HashWrite<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `needed` bytes of data can be backed up into a folder with `free` bytes left. `None` (unknown) is allowed: the write
/// itself would fail with a clear message.
pub fn check_room(free: Option<u64>, needed: u64) -> Result<()> {
    check_room_for(free, needed, "this backup")
}

/// [`check_room`] with the thing being written named in the message (`this restore`).
pub fn check_room_for(free: Option<u64>, needed: u64, what: &str) -> Result<()> {
    let Some(free) = free else { return Ok(()) };
    let want = needed.saturating_add(RESERVE_BYTES);
    if free >= want {
        return Ok(());
    }
    err(
        "MAC_BACKUP_NO_ROOM",
        format!(
            "not enough free disk for {what}: it holds up to {} (compressed it is usually smaller) and {} must stay free for macOS, so about {} is needed, but only {} is free. Free some room (`fluxctl mac system prune --dry-run` shows what can go, `fluxctl mac backup rm ID` removes an old backup) or keep backups on another disk: `export FLUXVM_MAC_BACKUPS=/Volumes/Big/fluxvm-backups`",
            human_bytes(needed),
            human_bytes(RESERVE_BYTES),
            human_bytes(want),
            human_bytes(free)
        ),
    )
}

pub struct Request {
    pub plan: Plan,
    /// How many ordinary backups of this instance or volume to keep (newest first).
    pub keep: u32,
    pub automatic: bool,
    pub note: Option<String>,
    pub now: u64,
    /// A name from [`new_id`] when the caller already told someone the id (the API does).
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub manifest: Manifest,
    /// Ids of older backups deleted by the retention count.
    pub pruned: Vec<String>,
}

/// Write the backup. `free` gives the free bytes of a path (the real one is [`free_bytes`]; tests pass a fake).
pub fn create(root: &Path, req: Request, free: &dyn Fn(&Path) -> Option<u64>) -> Result<Created> {
    let Request {
        plan,
        keep,
        automatic,
        note,
        now,
        id,
    } = req;
    for s in &plan.sources {
        if std::fs::symlink_metadata(&s.path).is_err() {
            return err(
                "MAC_BACKUP_SOURCE",
                format!("{} does not exist", s.path.display()),
            );
        }
    }
    let source_bytes = needed_bytes(&plan);
    check_room(free(root), source_bytes)?;
    std::fs::create_dir_all(root).map_err(|e| io_err("cannot create the backups folder", &e))?;
    let id = id.unwrap_or_else(|| new_id(root, plan.kind, &plan.name, now));
    let (part, archive) = (path_of(root, &id, "part"), path_of(root, &id, "tar.gz"));
    let written = write_archive(&part, &plan.sources);
    let (sha256, archive_bytes) = match written {
        Ok(x) => x,
        Err(e) => {
            std::fs::remove_file(&part).ok();
            return Err(e);
        }
    };
    if let Err(e) = std::fs::rename(&part, &archive) {
        std::fs::remove_file(&part).ok();
        return Err(io_err("cannot finish the archive", &e));
    }
    let files = plan
        .sources
        .iter()
        .map(|s| FileEntry {
            path: s.archive_path.clone(),
            bytes: alloc_bytes(&s.path),
            files: count_files(&s.path),
        })
        .collect();
    let manifest = Manifest {
        schema: SCHEMA,
        id: id.clone(),
        kind: plan.kind,
        name: plan.name,
        created_unix: now,
        automatic,
        note,
        archive: format!("{id}.tar.gz"),
        sha256,
        archive_bytes,
        source_bytes,
        files,
        captured: plan.captured,
        excluded: plan.excluded,
        instance: plan.instance,
        volume: plan.volume,
    };
    if let Err(e) = write_json(&path_of(root, &id, "json"), &manifest) {
        std::fs::remove_file(&archive).ok();
        return Err(e);
    }
    std::fs::remove_file(path_of(root, &id, "error")).ok();
    let pruned = prune(root, &manifest.name, manifest.kind, keep, now);
    Ok(Created { manifest, pruned })
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec_pretty(v).map_err(|e| BackupError {
        code: "MAC_BACKUP_IO",
        message: e.to_string(),
    })?;
    let r = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    r.map_err(|e| {
        std::fs::remove_file(&tmp).ok();
        io_err(&format!("cannot write {}", path.display()), &e)
    })
}

fn write_archive(part: &Path, sources: &[Source]) -> Result<(String, u64)> {
    let file = std::fs::File::create(part)
        .map_err(|e| io_err(&format!("cannot create {}", part.display()), &e))?;
    let hw = HashWrite {
        inner: file,
        hasher: Sha256::new(),
        bytes: 0,
    };
    let gz = flate2::write::GzEncoder::new(hw, flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    tar.follow_symlinks(false);
    let r = (|| -> std::io::Result<HashWrite<std::fs::File>> {
        for s in sources {
            if std::fs::symlink_metadata(&s.path)?.is_dir() {
                tar.append_dir_all(&s.archive_path, &s.path)?;
            } else {
                tar.append_path_with_name(&s.path, &s.archive_path)?;
            }
        }
        let gz = tar.into_inner()?;
        let hw = gz.finish()?;
        hw.inner.sync_all()?;
        Ok(hw)
    })();
    let hw = r.map_err(|e| io_err("cannot write the archive", &e))?;
    Ok((hex(&hw.hasher.finalize()), hw.bytes))
}

/// Note a background backup's failure, so `ls` can show it.
pub fn record_failure(root: &Path, id: &str, message: &str) {
    std::fs::remove_file(path_of(root, id, "part")).ok();
    if std::fs::create_dir_all(root).is_ok() {
        std::fs::write(path_of(root, id, "error"), message).ok();
    }
}

/// Mark a background backup as started, so `ls` shows it as running.
pub fn record_started(root: &Path, id: &str) {
    if std::fs::create_dir_all(root).is_ok() {
        std::fs::File::create(path_of(root, id, "part")).ok();
    }
}

// ------------------------------------------------------------------------------------------------------------ list

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Ok,
    Running,
    Failed,
    Damaged,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub id: String,
    pub state: State,
    pub manifest: Option<Manifest>,
    pub error: Option<String>,
}

fn load_manifest(root: &Path, id: &str) -> std::result::Result<Manifest, String> {
    let p = path_of(root, id, "json");
    let bytes = std::fs::read(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a backup manifest: {e}", p.display()))
}

/// Every backup in the folder, newest first; running and failed background backups come with them.
pub fn list(root: &Path, now: u64) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return vec![];
    };
    let mut out: Vec<Entry> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((id, ext)) = name.split_once('.') else {
            continue;
        };
        if !valid_id(id) {
            continue;
        }
        match ext {
            "json" => out.push(match load_manifest(root, id) {
                Ok(m) if path_of(root, id, "tar.gz").is_file() => Entry {
                    id: id.into(),
                    state: State::Ok,
                    manifest: Some(m),
                    error: None,
                },
                Ok(m) => Entry {
                    id: id.into(),
                    state: State::Damaged,
                    error: Some(format!(
                        "the archive {} is missing; remove this entry with `fluxctl mac backup rm {id}`",
                        m.archive
                    )),
                    manifest: Some(m),
                },
                Err(msg) => Entry {
                    id: id.into(),
                    state: State::Damaged,
                    manifest: None,
                    error: Some(msg),
                },
            }),
            "part" if !path_of(root, id, "json").exists() => {
                let age = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| now.saturating_sub(d.as_secs()));
                let stale = age > STALE_PART_SECS;
                out.push(Entry {
                    id: id.into(),
                    state: if stale { State::Failed } else { State::Running },
                    manifest: None,
                    error: stale.then(|| "the backup was interrupted (the daemon or the Mac stopped); remove it and run it again".into()),
                });
            }
            "error" => out.push(Entry {
                id: id.into(),
                state: State::Failed,
                manifest: None,
                error: std::fs::read_to_string(e.path()).ok(),
            }),
            _ => {}
        }
    }
    out.sort_by(|a, b| {
        let t = |e: &Entry| e.manifest.as_ref().map_or(u64::MAX, |m| m.created_unix);
        t(b).cmp(&t(a)).then(a.id.cmp(&b.id))
    });
    out
}

/// A backup by exact id, or by instance or volume name (`ci`, `instance:ci`, `volume:cache`): the newest good one.
pub fn resolve(root: &Path, spec: &str, now: u64) -> Result<Manifest> {
    let all = list(root, now);
    if let Some(e) = all.iter().find(|e| e.id == spec) {
        return match &e.manifest {
            Some(m) if e.state == State::Ok => Ok(m.clone()),
            _ => err(
                "MAC_BACKUP_NOT_FOUND",
                format!(
                    "backup {spec} is not usable ({}): {}",
                    match e.state {
                        State::Running => "still being written",
                        State::Failed => "it failed",
                        _ => "it is damaged",
                    },
                    e.error
                        .as_deref()
                        .unwrap_or("wait for it to finish, then retry")
                ),
            ),
        };
    }
    let (kind, name) = match spec.split_once(':') {
        Some(("instance", n)) => (Some(Kind::Instance), n),
        Some(("volume", n)) => (Some(Kind::Volume), n),
        _ => (None, spec),
    };
    let mut hits: Vec<&Manifest> = all
        .iter()
        .filter(|e| e.state == State::Ok)
        .filter_map(|e| e.manifest.as_ref())
        .filter(|m| m.name == name && kind.is_none_or(|k| k == m.kind))
        .collect();
    hits.sort_by_key(|m| std::cmp::Reverse(m.created_unix));
    let kinds: std::collections::BTreeSet<Kind> = hits.iter().map(|m| m.kind).collect();
    if kinds.len() > 1 {
        return err(
            "MAC_BACKUP_NOT_FOUND",
            format!(
                "{name:?} names both an instance and a volume. Say which: `instance:{name}` or `volume:{name}`, or give a backup id from `fluxctl mac backup ls`"
            ),
        );
    }
    match hits.first() {
        Some(m) => Ok((*m).clone()),
        None => err(
            "MAC_BACKUP_NOT_FOUND",
            format!(
                "no backup {spec:?}. List them with `fluxctl mac backup ls`, or make one with `fluxctl mac backup create NAME`"
            ),
        ),
    }
}

// ------------------------------------------------------------------------------------------------------------ verify

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    pub id: String,
    pub sha256: String,
    pub archive_bytes: u64,
    pub entries: u64,
    pub unpacked_bytes: u64,
}

pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// Check the archive against the manifest's sha256 and read it to the end. Restore calls this before it touches anything.
pub fn verify(root: &Path, m: &Manifest) -> Result<VerifyReport> {
    let archive = path_of(root, &m.id, "tar.gz");
    let got = sha256_file(&archive).map_err(|e| {
        BackupError {
            code: "MAC_BACKUP_CHECKSUM",
            message: format!(
                "cannot read {}: {e}. The backup {} cannot be used; remove it with `fluxctl mac backup rm {}`",
                archive.display(),
                m.id,
                m.id
            ),
        }
    })?;
    if !got.eq_ignore_ascii_case(&m.sha256) {
        return err(
            "MAC_BACKUP_CHECKSUM",
            format!(
                "backup {} does not match its checksum (manifest {}, file {}): the archive is damaged or was changed, so nothing was restored. Use an older backup (`fluxctl mac backup ls`) and remove this one with `fluxctl mac backup rm {}`",
                m.id, m.sha256, got, m.id
            ),
        );
    }
    let f = std::fs::File::open(&archive).map_err(|e| io_err("cannot open the archive", &e))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    let (mut entries, mut bytes) = (0u64, 0u64);
    let bad = |e: std::io::Error| BackupError {
        code: "MAC_BACKUP_CHECKSUM",
        message: format!(
            "backup {} has the right checksum but cannot be read as an archive ({e}); remove it with `fluxctl mac backup rm {}`",
            m.id, m.id
        ),
    };
    for e in ar.entries().map_err(bad)? {
        let e = e.map_err(bad)?;
        entries += 1;
        bytes += e.header().size().unwrap_or(0);
    }
    Ok(VerifyReport {
        id: m.id.clone(),
        sha256: got,
        archive_bytes: m.archive_bytes,
        entries,
        unpacked_bytes: bytes,
    })
}

/// Verify, then unpack into `dest` (an empty folder the caller made). `tar` refuses paths that climb out of `dest`.
pub fn extract(root: &Path, m: &Manifest, dest: &Path) -> Result<VerifyReport> {
    let rep = verify(root, m)?;
    unpack(root, m, dest)?;
    Ok(rep)
}

/// Unpack without checking again: for a caller that has just called [`verify`] (reading a large archive twice costs minutes).
pub fn unpack(root: &Path, m: &Manifest, dest: &Path) -> Result<()> {
    let f = std::fs::File::open(path_of(root, &m.id, "tar.gz"))
        .map_err(|e| io_err("cannot open the archive", &e))?;
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(f));
    ar.set_preserve_permissions(true);
    ar.unpack(dest)
        .map_err(|e| io_err(&format!("cannot unpack into {}", dest.display()), &e))
}

// ------------------------------------------------------------------------------------------------------------ delete

pub fn remove(root: &Path, id: &str) -> Result<()> {
    if !valid_id(id) {
        return err("MAC_BACKUP_NOT_FOUND", format!("{id:?} is not a backup id"));
    }
    let mut any = false;
    // The manifest goes first: without it the archive is no longer a listed backup.
    for ext in ["json", "tar.gz", "part", "error", "tmp"] {
        let p = path_of(root, id, ext);
        if p.exists() {
            any = true;
            std::fs::remove_file(&p)
                .map_err(|e| io_err(&format!("cannot delete {}", p.display()), &e))?;
        }
    }
    if !any {
        return err(
            "MAC_BACKUP_NOT_FOUND",
            format!("no backup {id:?}. List them with `fluxctl mac backup ls`"),
        );
    }
    Ok(())
}

/// Ids retention would delete for one instance or volume: ordinary backups beyond `keep`, safety-net ones beyond [`AUTO_KEEP`].
pub fn prune_plan(entries: &[Entry], name: &str, kind: Kind, keep: u32) -> Vec<String> {
    let mut mine: Vec<&Manifest> = entries
        .iter()
        .filter(|e| e.state == State::Ok)
        .filter_map(|e| e.manifest.as_ref())
        .filter(|m| m.name == name && m.kind == kind)
        .collect();
    mine.sort_by_key(|m| std::cmp::Reverse((m.created_unix, m.id.clone())));
    let mut out = Vec::new();
    for (auto, n) in [(false, keep.max(1) as usize), (true, AUTO_KEEP)] {
        out.extend(
            mine.iter()
                .filter(|m| m.automatic == auto)
                .skip(n)
                .map(|m| m.id.clone()),
        );
    }
    out
}

pub fn prune(root: &Path, name: &str, kind: Kind, keep: u32, now: u64) -> Vec<String> {
    let plan = prune_plan(&list(root, now), name, kind, keep);
    plan.into_iter()
        .filter(|id| remove(root, id).is_ok())
        .collect()
}

// ------------------------------------------------------------------------------------------------------------ schedule

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub kind: Kind,
    pub name: String,
    pub every_secs: u64,
    pub keep: u32,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run_unix: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
}

fn yes() -> bool {
    true
}

/// `30m`, `24h`, `7d`: how often an automatic backup is taken (5 minutes to 90 days).
pub fn parse_every(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    let bad = || BackupError {
        code: "MAC_BACKUP_SCHEDULE",
        message: format!(
            "{s:?} is not an interval. Use a number and a unit: 30m, 24h or 7d (between 5 minutes and 90 days). Example: `fluxctl mac backup schedule ci --every 24h`"
        ),
    };
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num.parse().map_err(|_| bad())?;
    let secs = n.checked_mul(match unit {
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(bad()),
    });
    match secs {
        Some(v) if (300..=90 * 86400).contains(&v) => Ok(v),
        _ => Err(bad()),
    }
}

pub fn every_text(secs: u64) -> String {
    match secs {
        s if s % 86400 == 0 => format!("{}d", s / 86400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s => format!("{}m", s / 60),
    }
}

pub fn load_schedules(root: &Path) -> Vec<ScheduleEntry> {
    std::fs::read(root.join(SCHEDULE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save_schedules(root: &Path, list: &[ScheduleEntry]) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|e| io_err("cannot create the backups folder", &e))?;
    write_json(&root.join(SCHEDULE_FILE), &list)
}

/// Add or replace the schedule of one instance or volume.
pub fn set_schedule(
    root: &Path,
    kind: Kind,
    name: &str,
    every_secs: u64,
    keep: u32,
    now: u64,
) -> Result<ScheduleEntry> {
    if !(1..=1000).contains(&keep) {
        return err("MAC_BACKUP_SCHEDULE", "--keep must be between 1 and 1000");
    }
    let mut all = load_schedules(root);
    let last = list(root, now)
        .iter()
        .filter_map(|e| e.manifest.as_ref())
        .filter(|m| m.kind == kind && m.name == name && !m.automatic)
        .map(|m| m.created_unix)
        .max();
    all.retain(|s| !(s.kind == kind && s.name == name));
    let e = ScheduleEntry {
        kind,
        name: name.into(),
        every_secs,
        keep,
        enabled: true,
        last_run_unix: last,
        last_error: None,
    };
    all.push(e.clone());
    all.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    save_schedules(root, &all)?;
    Ok(e)
}

/// Remove a schedule; false when there was none.
pub fn clear_schedule(root: &Path, kind: Kind, name: &str) -> Result<bool> {
    let mut all = load_schedules(root);
    let before = all.len();
    all.retain(|s| !(s.kind == kind && s.name == name));
    if all.len() == before {
        return Ok(false);
    }
    save_schedules(root, &all)?;
    Ok(true)
}

pub fn is_due(s: &ScheduleEntry, now: u64) -> bool {
    s.enabled
        && s.last_run_unix
            .is_none_or(|t| now >= t.saturating_add(s.every_secs))
}

#[derive(Debug)]
pub struct DueResult {
    pub kind: Kind,
    pub name: String,
    pub outcome: Result<Created>,
}

/// Take every backup whose schedule is due. `plan_for` finds the instance or volume (it is how the CLI and the daemon each look
/// things up); a failure is noted on the schedule and retried after one interval, not every minute.
pub fn run_due(
    root: &Path,
    now: u64,
    free: &dyn Fn(&Path) -> Option<u64>,
    plan_for: &dyn Fn(Kind, &str) -> Result<Plan>,
) -> Vec<DueResult> {
    let mut out = Vec::new();
    for s in load_schedules(root).into_iter().filter(|s| is_due(s, now)) {
        let outcome = plan_for(s.kind, &s.name).and_then(|plan| {
            create(
                root,
                Request {
                    plan,
                    keep: s.keep,
                    automatic: false,
                    note: Some(format!("scheduled, every {}", every_text(s.every_secs))),
                    now,
                    id: None,
                },
                free,
            )
        });
        mark_ran(
            root,
            s.kind,
            &s.name,
            now,
            outcome.as_ref().err().map(|e| e.message.clone()),
        );
        out.push(DueResult {
            kind: s.kind,
            name: s.name,
            outcome,
        });
    }
    out
}

/// Record that a scheduled run was attempted (so a failing one is retried only after the interval, not every minute).
pub fn mark_ran(root: &Path, kind: Kind, name: &str, now: u64, error: Option<String>) {
    let mut all = load_schedules(root);
    if let Some(s) = all.iter_mut().find(|s| s.kind == kind && s.name == name) {
        s.last_run_unix = Some(now);
        s.last_error = error;
        save_schedules(root, &all).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn instance_vm(dir: &Path, status: &str) -> Value {
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("disk.raw"), vec![7u8; 8192]).unwrap();
        std::fs::write(ws.join("hardware.bin"), b"hw").unwrap();
        std::fs::write(ws.join("auxiliary.bin"), b"aux").unwrap();
        json!({"id": "i1", "name": "ci", "status": status,
            "disk": ws.join("disk.raw"), "workspace": ws,
            "labels": {"fluxvm.mac": "instance", "fluxvm.mac.restart": "always", "team": "ios", "api_token": "x"},
            "request": {"vcpus": 4, "memory_mib": 8192, "network": {"forwards": [{"host_port": 2222, "guest_port": 22}]}}})
    }

    fn plenty(_: &Path) -> Option<u64> {
        Some(1 << 40)
    }

    fn make(root: &Path, vm: &Value, keep: u32, now: u64) -> Created {
        create(
            root,
            Request {
                plan: instance_plan(vm).unwrap(),
                keep,
                automatic: false,
                note: None,
                now,
                id: None,
            },
            &plenty,
        )
        .unwrap()
    }

    #[test]
    fn a_running_instance_is_refused_with_the_command_to_stop_it() {
        let d = tempfile::tempdir().unwrap();
        let e = instance_plan(&instance_vm(d.path(), "running")).unwrap_err();
        assert_eq!(e.code, "MAC_BACKUP_RUNNING");
        assert!(e.message.contains("fluxctl mac stop ci"), "{e}");
    }

    #[test]
    fn the_manifest_says_what_is_in_and_what_is_left_out() {
        let d = tempfile::tempdir().unwrap();
        let vm = instance_vm(d.path(), "stopped");
        let c = make(&d.path().join("b"), &vm, 7, 1_800_000_000);
        let m = &c.manifest;
        let i = m.instance.as_ref().unwrap();
        assert_eq!(i.restart_policy, "always");
        assert_eq!((i.vcpus, i.memory_mib), (Some(4), Some(8192)));
        assert_eq!(
            i.forwards,
            [Forward {
                host: 2222,
                guest: 22
            }]
        );
        assert!(i.labels.contains_key("team") && !i.labels.contains_key("api_token"));
        let ex = m.excluded.join("\n");
        assert!(
            ex.contains("api_token") && ex.contains("secret") && ex.contains("registry logins"),
            "{ex}"
        );
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.sha256.len(), 64);
        assert!(m.id.starts_with("instance-ci-2027"), "{}", m.id);
    }

    #[test]
    fn verify_passes_then_catches_a_flipped_byte() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let c = make(&root, &instance_vm(d.path(), "stopped"), 7, 100);
        let r = verify(&root, &c.manifest).unwrap();
        assert!(r.entries >= 3 && r.unpacked_bytes >= 8192);
        let p = root.join(&c.manifest.archive);
        let mut b = std::fs::read(&p).unwrap();
        let n = b.len() / 2;
        b[n] ^= 0xff;
        std::fs::write(&p, b).unwrap();
        let e = verify(&root, &c.manifest).unwrap_err();
        assert_eq!(e.code, "MAC_BACKUP_CHECKSUM");
        assert!(e.message.contains("nothing was restored"), "{e}");
    }

    #[test]
    fn extract_gives_the_files_back_byte_for_byte() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let c = make(&root, &instance_vm(d.path(), "stopped"), 7, 100);
        let out = d.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        extract(&root, &c.manifest, &out).unwrap();
        assert_eq!(
            std::fs::read(out.join("disk.raw")).unwrap(),
            vec![7u8; 8192]
        );
        assert_eq!(std::fs::read(out.join("auxiliary.bin")).unwrap(), b"aux");
    }

    #[test]
    fn a_volume_round_trips_with_symlinks_and_nested_files() {
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("v/data");
        std::fs::create_dir_all(data.join("a/b")).unwrap();
        std::fs::write(data.join("a/b/f.txt"), b"hello").unwrap();
        std::os::unix::fs::symlink("a/b/f.txt", data.join("link")).unwrap();
        let labels = BTreeMap::from([("k".to_string(), "v".to_string())]);
        let root = d.path().join("b");
        let c = create(
            &root,
            Request {
                plan: volume_plan("cache", &data, &labels).unwrap(),
                keep: 7,
                automatic: false,
                note: None,
                now: 5,
                id: None,
            },
            &plenty,
        )
        .unwrap();
        assert_eq!(c.manifest.files[0].files, 2);
        let out = d.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        extract(&root, &c.manifest, &out).unwrap();
        assert_eq!(std::fs::read(out.join("data/a/b/f.txt")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read_link(out.join("data/link")).unwrap(),
            Path::new("a/b/f.txt")
        );
    }

    #[test]
    fn low_disk_refuses_before_writing_and_says_how_much() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let vm = instance_vm(d.path(), "stopped");
        let e = create(
            &root,
            Request {
                plan: instance_plan(&vm).unwrap(),
                keep: 7,
                automatic: false,
                note: None,
                now: 1,
                id: None,
            },
            &|_| Some(1 << 30),
        )
        .unwrap_err();
        assert_eq!(e.code, "MAC_BACKUP_NO_ROOM");
        assert!(
            e.message.contains("about 2.0 GiB is needed")
                && e.message.contains("only 1.0 GiB is free"),
            "{e}"
        );
        assert!(e.message.contains("FLUXVM_MAC_BACKUPS"), "{e}");
        assert!(!root.exists(), "nothing was written");
        assert!(check_room(None, 1 << 50).is_ok());
        assert!(check_room(Some(3 << 30), 1 << 30).is_ok());
        assert!(check_room(Some(3 << 30), (1 << 30) + 1).is_err());
    }

    #[test]
    fn retention_keeps_the_newest_n_and_three_safety_nets() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let vm = instance_vm(d.path(), "stopped");
        let mut last = None;
        for t in 1..=5u64 {
            last = Some(make(&root, &vm, 3, t * 1000));
        }
        let ids: Vec<String> = list(&root, 9000)
            .into_iter()
            .map(|e| e.manifest.unwrap().created_unix.to_string())
            .collect();
        assert_eq!(ids, ["5000", "4000", "3000"]);
        assert_eq!(last.unwrap().pruned.len(), 1);
        for t in 6..=10u64 {
            create(
                &root,
                Request {
                    plan: instance_plan(&vm).unwrap(),
                    keep: 3,
                    automatic: true,
                    note: Some("before restore".into()),
                    now: t * 1000,
                    id: None,
                },
                &plenty,
            )
            .unwrap();
        }
        let all = list(&root, 20_000);
        let auto = all
            .iter()
            .filter(|e| e.manifest.as_ref().unwrap().automatic)
            .count();
        assert_eq!((all.len(), auto), (6, 3), "3 ordinary + 3 automatic");
    }

    #[test]
    fn resolve_by_id_name_and_kind_and_refuses_a_clash() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let vm = instance_vm(d.path(), "stopped");
        let old = make(&root, &vm, 7, 100);
        let new = make(&root, &vm, 7, 200);
        assert_eq!(resolve(&root, "ci", 300).unwrap().id, new.manifest.id);
        assert_eq!(
            resolve(&root, &old.manifest.id, 300).unwrap().id,
            old.manifest.id
        );
        let data = d.path().join("v/data");
        std::fs::create_dir_all(&data).unwrap();
        create(
            &root,
            Request {
                plan: volume_plan("ci", &data, &BTreeMap::new()).unwrap(),
                keep: 7,
                automatic: false,
                note: None,
                now: 400,
                id: None,
            },
            &plenty,
        )
        .unwrap();
        let e = resolve(&root, "ci", 500).unwrap_err();
        assert!(e.message.contains("instance:ci"), "{e}");
        assert_eq!(
            resolve(&root, "instance:ci", 500).unwrap().id,
            new.manifest.id
        );
        assert_eq!(
            resolve(&root, "nope", 500).unwrap_err().code,
            "MAC_BACKUP_NOT_FOUND"
        );
    }

    #[test]
    fn list_shows_running_failed_and_damaged_entries() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let c = make(&root, &instance_vm(d.path(), "stopped"), 7, 100);
        record_started(&root, "instance-x-1");
        record_failure(&root, "instance-y-1", "no room");
        std::fs::remove_file(root.join(&c.manifest.archive)).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let states: BTreeMap<String, State> = list(&root, now)
            .into_iter()
            .map(|e| (e.id, e.state))
            .collect();
        assert_eq!(states["instance-x-1"], State::Running);
        assert_eq!(states["instance-y-1"], State::Failed);
        assert_eq!(states[&c.manifest.id], State::Damaged);
        // a stale .part is a failure, not "running" forever
        assert_eq!(
            list(&root, now + 3 * 86400)
                .into_iter()
                .find(|e| e.id == "instance-x-1")
                .unwrap()
                .state,
            State::Failed
        );
        assert!(remove(&root, "instance-x-1").is_ok());
        assert_eq!(
            remove(&root, "../etc").unwrap_err().code,
            "MAC_BACKUP_NOT_FOUND"
        );
    }

    #[test]
    fn every_parses_units_and_bounds() {
        assert_eq!(parse_every("24h").unwrap(), 86400);
        assert_eq!(parse_every("30M").unwrap(), 1800);
        assert_eq!(parse_every("7d").unwrap(), 7 * 86400);
        for bad in ["", "24", "h", "1m", "100d", "x5h", "5 h x", "-1h"] {
            assert_eq!(
                parse_every(bad).unwrap_err().code,
                "MAC_BACKUP_SCHEDULE",
                "{bad}"
            );
        }
        assert_eq!(every_text(86400), "1d");
        assert_eq!(every_text(5400), "90m");
    }

    #[test]
    fn schedules_persist_and_come_due_by_the_interval() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let e = set_schedule(&root, Kind::Instance, "ci", 3600, 5, 1000).unwrap();
        assert!(is_due(&e, 1000), "never run: due on the next run-due");
        mark_ran(&root, Kind::Instance, "ci", 1000, None);
        let s = load_schedules(&root);
        assert_eq!(s.len(), 1);
        assert!(!is_due(&s[0], 1000 + 3599) && is_due(&s[0], 1000 + 3600));
        set_schedule(&root, Kind::Instance, "ci", 7200, 2, 2000).unwrap();
        assert_eq!(load_schedules(&root).len(), 1, "replaced, not added");
        assert!(clear_schedule(&root, Kind::Instance, "ci").unwrap());
        assert!(!clear_schedule(&root, Kind::Instance, "ci").unwrap());
        assert!(set_schedule(&root, Kind::Volume, "v", 3600, 0, 1).is_err());
    }

    #[test]
    fn run_due_backs_up_what_is_due_and_notes_failures() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        let vm = instance_vm(d.path(), "stopped");
        set_schedule(&root, Kind::Instance, "ci", 3600, 7, 0).unwrap();
        set_schedule(&root, Kind::Instance, "gone", 3600, 7, 0).unwrap();
        let plan_for = |k: Kind, n: &str| -> Result<Plan> {
            if n == "ci" {
                instance_plan(&vm)
            } else {
                err("MAC_BACKUP_SOURCE", format!("no {} {n}", k.word()))
            }
        };
        let r = run_due(&root, 10_000, &plenty, &plan_for);
        assert_eq!(r.len(), 2);
        assert!(r.iter().find(|x| x.name == "ci").unwrap().outcome.is_ok());
        let sched = load_schedules(&root);
        let gone = sched.iter().find(|s| s.name == "gone").unwrap();
        assert!(
            gone.last_error
                .as_deref()
                .unwrap()
                .contains("no instance gone")
        );
        assert_eq!(gone.last_run_unix, Some(10_000));
        // not due again until the interval passes
        assert!(run_due(&root, 10_100, &plenty, &plan_for).is_empty());
        assert_eq!(run_due(&root, 10_000 + 3600, &plenty, &plan_for).len(), 2);
        let ci = list(&root, 20_000)
            .into_iter()
            .filter(|e| {
                e.manifest
                    .as_ref()
                    .is_some_and(|m| m.note.as_deref().is_some_and(|n| n.contains("every 1h")))
            })
            .count();
        assert_eq!(ci, 2);
    }
}
