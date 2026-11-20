// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Backup chain verification and test-restore.
//!
//! [`verify_chain`] re-reads everything a restore of one chain point would read, without restoring:
//! each manifest up to the full backup, every layer object (size and SHA-256 against the manifest,
//! decrypting first when the layer is encrypted and a key was given), and that the chain is complete
//! and consistent (parent links, sequences, same chain id, every disk present in every layer set).
//! It reports every problem it finds rather than stopping at the first, like a Proxmox Backup Server
//! verify job.
//!
//! [`test_restore_chain`] goes further: it rebuilds the disks in a scratch directory the way a restore
//! would and checks the result. With `qemu-img` it stacks the layers, flattens them and runs
//! `qemu-img check` and `info` on the outcome; without it, it checks each layer's qcow2 header and
//! virtual size. Nothing is written to the object store or to any VM.
//!
//! Content problems are returned in the [`ChainReport`]; `Err` is only for setup failures (the scratch
//! directory, the key).

use crate::crypto;
use crate::manifest::{DiskLayer, Manifest, ManifestKind, ParentRef};
use crate::{ObjectStore, validate_key_segment};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Longest chain walked before giving up (a loop in the parent links would otherwise never end).
pub const MAX_CHAIN: usize = 1000;

/// A layer key the caller can decrypt with.
#[derive(Clone)]
pub struct DecryptKey {
    pub key_id: String,
    /// The raw 32 bytes (`crypto::parse_key` of the base64 form).
    pub key: [u8; 32],
}

#[derive(Clone, Default)]
pub struct VerifyOptions {
    /// Synthetic fulls of the chain; one consolidating a parent replaces the rest of the walk.
    pub synthetic_bases: Vec<String>,
    /// Without it, encrypted layers are only checked for size, and counted as `unverified_layers`.
    pub key: Option<DecryptKey>,
    /// Existing directory for temporary downloads; a private subdirectory is made and removed.
    pub scratch: PathBuf,
}

#[derive(Clone, Default)]
pub struct TestRestoreOptions {
    pub verify: VerifyOptions,
    /// `qemu-img` binary. `None`, or a binary that cannot be run, selects header checks.
    pub qemu_img: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IssueKind {
    ManifestMissing,
    ManifestInvalid,
    /// Keys, ids or layer paths that do not agree with the manifest they belong to.
    ManifestInconsistent,
    /// A parent link, sequence, chain id or base that does not hold together.
    ChainBroken,
    LayerMissing,
    SizeMismatch,
    DigestMismatch,
    DecryptFailed,
    /// The restored image failed its check.
    ImageInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Issue {
    pub kind: IssueKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_id: Option<String>,
    /// The object key the problem is about.
    pub key: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportKind {
    Verify,
    TestRestore,
}

/// How a test-restore checked the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageCheck {
    /// Layers stacked and flattened, then `qemu-img check` and `info`.
    QemuImg,
    /// qcow2 header and virtual size of every layer.
    Header,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskCheck {
    pub name: String,
    pub virtual_size: u64,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainReport {
    pub kind: ReportKind,
    pub manifest_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<String>,
    /// No issues. Warnings do not count.
    pub ok: bool,
    pub started_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub backups_checked: usize,
    pub layers_checked: usize,
    /// Encrypted layers whose digest could not be checked (no key given).
    pub unverified_layers: usize,
    pub bytes_read: u64,
    pub issues: Vec<Issue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_check: Option<ImageCheck>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskCheck>,
}

impl ChainReport {
    fn new(kind: ReportKind, manifest_key: &str) -> Self {
        Self {
            kind,
            manifest_key: manifest_key.into(),
            chain_id: None,
            ok: false,
            started_at: Utc::now(),
            duration_ms: 0,
            backups_checked: 0,
            layers_checked: 0,
            unverified_layers: 0,
            bytes_read: 0,
            issues: vec![],
            warnings: vec![],
            image_check: None,
            disks: vec![],
        }
    }

    fn issue(
        &mut self,
        kind: IssueKind,
        m: Option<&Manifest>,
        key: &str,
        detail: impl Into<String>,
    ) {
        self.issues.push(Issue {
            kind,
            backup_id: m.map(|m| m.backup_id.clone()),
            key: key.into(),
            detail: detail.into(),
        });
    }

    fn finish(mut self, t0: std::time::Instant) -> Self {
        self.ok = self.issues.is_empty();
        self.duration_ms = t0.elapsed().as_millis() as u64;
        self
    }

    /// One line for logs and events.
    pub fn summary(&self) -> String {
        let what = match self.kind {
            ReportKind::Verify => "verify",
            ReportKind::TestRestore => "test-restore",
        };
        if self.ok {
            format!(
                "{what} ok: {} backups, {} layers, {} bytes read{}",
                self.backups_checked,
                self.layers_checked,
                self.bytes_read,
                if self.unverified_layers > 0 {
                    format!(
                        ", {} encrypted layers not digest-checked",
                        self.unverified_layers
                    )
                } else {
                    String::new()
                }
            )
        } else {
            let first = &self.issues[0];
            format!(
                "{what} FAILED: {} issue(s), first: {:?} {}: {}",
                self.issues.len(),
                first.kind,
                first.key,
                first.detail
            )
        }
    }
}

/// A manifest and the key it was read from, newest first while walking.
struct Link {
    key: String,
    m: Manifest,
}

fn manifest_dir(key: &str) -> Option<&str> {
    key.strip_suffix("/manifest.json")
}

/// Checks that a manifest agrees with the key it was stored under and with itself.
fn check_manifest(r: &mut ChainReport, key: &str, m: &Manifest) {
    // `<prefix>/<chain>/<sequence>-<backup>/manifest.json`
    let expect_tail = format!(
        "{}/{:08}-{}/manifest.json",
        m.chain_id, m.sequence, m.backup_id
    );
    if !key.ends_with(&expect_tail) {
        r.issue(
            IssueKind::ManifestInconsistent,
            Some(m),
            key,
            format!("stored at a key that does not match its chain/sequence/backup id (expected …/{expect_tail})"),
        );
    }
    for (what, v) in [("chain id", &m.chain_id), ("backup id", &m.backup_id)] {
        if let Err(e) = validate_key_segment(what, v) {
            r.issue(IssueKind::ManifestInconsistent, Some(m), key, e.to_string());
        }
    }
    if m.disks.is_empty() {
        r.issue(
            IssueKind::ManifestInconsistent,
            Some(m),
            key,
            "manifest lists no disks",
        );
    }
    let dir = manifest_dir(key);
    let mut seen = std::collections::BTreeSet::new();
    for d in &m.disks {
        if !seen.insert(d.name.as_str()) {
            r.issue(
                IssueKind::ManifestInconsistent,
                Some(m),
                key,
                format!("disk {:?} listed twice", d.name),
            );
        }
        if let Some(dir) = dir {
            let want = format!("{dir}/{}.qcow2", d.name);
            if d.key != want {
                r.issue(
                    IssueKind::ManifestInconsistent,
                    Some(m),
                    &d.key,
                    format!("layer of disk {:?} is not at {want}", d.name),
                );
            }
        }
        if d.virtual_size == 0 {
            r.issue(
                IssueKind::ManifestInconsistent,
                Some(m),
                &d.key,
                format!("disk {:?} has virtual size 0", d.name),
            );
        }
        if d.sha256.len() != 64 || !d.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            r.issue(
                IssueKind::ManifestInconsistent,
                Some(m),
                &d.key,
                format!("disk {:?} has no valid sha256 digest", d.name),
            );
        }
    }
}

/// Reads the manifests of the chain ending at `manifest_key`, newest first, recording any that are
/// missing or unreadable. The walk ends at a full (or synthetic full) backup, or where it breaks.
async fn walk(
    store: &dyn ObjectStore,
    manifest_key: &str,
    synthetic_bases: &[String],
    r: &mut ChainReport,
) -> Vec<Link> {
    let mut bases: Vec<Link> = vec![];
    for key in synthetic_bases {
        match store.get_bytes(key).await {
            Ok(b) => match Manifest::from_bytes(&b) {
                Ok(m) if m.kind == ManifestKind::SyntheticFull => bases.push(Link {
                    key: key.clone(),
                    m,
                }),
                Ok(_) => r.issue(IssueKind::ChainBroken, None, key, "not a synthetic full"),
                Err(e) => r.issue(IssueKind::ManifestInvalid, None, key, format!("{e:#}")),
            },
            Err(e) => r.issue(IssueKind::ManifestMissing, None, key, format!("{e:#}")),
        }
    }
    let stands_in = |chain: &str, p: &ParentRef| {
        bases.iter().position(|b| {
            b.m.chain_id == chain
                && b.m.sequence == p.sequence
                && b.m.consolidates.contains(&p.backup_id)
        })
    };
    let mut links: Vec<Link> = vec![];
    let mut key = manifest_key.to_string();
    loop {
        if links.len() >= MAX_CHAIN {
            r.issue(
                IssueKind::ChainBroken,
                None,
                &key,
                format!("chain is longer than {MAX_CHAIN} backups (loop in the parent links?)"),
            );
            break;
        }
        if links.iter().any(|l| l.key == key) {
            r.issue(
                IssueKind::ChainBroken,
                None,
                &key,
                "parent links form a loop",
            );
            break;
        }
        let bytes = match store.get_bytes(&key).await {
            Ok(b) => b,
            Err(e) => {
                r.issue(IssueKind::ManifestMissing, None, &key, format!("{e:#}"));
                break;
            }
        };
        let m = match Manifest::from_bytes(&bytes) {
            Ok(m) => m,
            Err(e) => {
                r.issue(IssueKind::ManifestInvalid, None, &key, format!("{e:#}"));
                break;
            }
        };
        let parent = m.parent.clone();
        let chain = m.chain_id.clone();
        links.push(Link {
            key: key.clone(),
            m,
        });
        match parent {
            None => break,
            Some(p) => match stands_in(&chain, &p) {
                Some(i) => {
                    let b = &bases[i];
                    links.push(Link {
                        key: b.key.clone(),
                        m: b.m.clone(),
                    });
                    break;
                }
                None => key = p.manifest_key,
            },
        }
    }
    links
}

/// Cross-manifest checks on a chain read newest first.
fn check_links(r: &mut ChainReport, links: &[Link]) {
    let Some(top) = links.first() else { return };
    r.chain_id = Some(top.m.chain_id.clone());
    for l in links {
        check_manifest(r, &l.key, &l.m);
    }
    for pair in links.windows(2) {
        let (child, parent) = (&pair[0], &pair[1]);
        let Some(want) = child.m.parent.as_ref() else {
            continue;
        };
        if parent.m.chain_id != top.m.chain_id {
            r.issue(
                IssueKind::ChainBroken,
                Some(&parent.m),
                &parent.key,
                format!(
                    "belongs to chain {}, not {}",
                    parent.m.chain_id, top.m.chain_id
                ),
            );
        }
        let consolidated = parent.m.kind == ManifestKind::SyntheticFull
            && parent.m.consolidates.contains(&want.backup_id);
        if (want.backup_id != parent.m.backup_id && !consolidated)
            || want.sequence != parent.m.sequence
        {
            r.issue(
                IssueKind::ChainBroken,
                Some(&child.m),
                &child.key,
                format!(
                    "names parent {} (sequence {}) but the chain holds {} (sequence {})",
                    want.backup_id, want.sequence, parent.m.backup_id, parent.m.sequence
                ),
            );
        }
        if parent.m.sequence >= child.m.sequence {
            r.issue(
                IssueKind::ChainBroken,
                Some(&child.m),
                &child.key,
                format!(
                    "sequence {} does not increase over its parent's {}",
                    child.m.sequence, parent.m.sequence
                ),
            );
        }
    }
    let base = links.last().unwrap();
    if base.m.kind.needs_parent() {
        // The walk ended on an incremental: its parent could not be read (already reported) or the
        // chain has no full backup at its base.
        r.issue(
            IssueKind::ChainBroken,
            Some(&base.m),
            &base.key,
            "chain has no full backup at its base (parent not readable)",
        );
    }
    // A restore of the top point needs every disk of the top in every layer set below it.
    for l in &links[1..] {
        for d in &top.m.disks {
            if l.m.disk(&d.name).is_none() {
                r.issue(
                    IssueKind::ChainBroken,
                    Some(&l.m),
                    &l.key,
                    format!(
                        "has no layer for disk {:?}, which the restore point {} has",
                        d.name, top.m.backup_id
                    ),
                );
            }
        }
    }
}

/// Downloads one layer to `dest`, checks its stored size and digest, and decrypts it in place when it
/// is encrypted and a key is available. Returns true when `dest` holds the plaintext layer and every
/// check on it passed; an encrypted layer without a usable key is only size-checked and returns false.
async fn fetch_layer(
    store: &dyn ObjectStore,
    r: &mut ChainReport,
    m: &Manifest,
    layer: &DiskLayer,
    key: Option<&DecryptKey>,
    dest: &Path,
) -> bool {
    let stored = dest.with_extension("stored");
    if let Err(e) = store.get_file(&layer.key, &stored).await {
        r.issue(
            IssueKind::LayerMissing,
            Some(m),
            &layer.key,
            format!("{e:#}"),
        );
        return false;
    }
    r.layers_checked += 1;
    let size = tokio::fs::metadata(&stored)
        .await
        .map(|x| x.len())
        .unwrap_or(0);
    r.bytes_read += size;
    let mut ok = true;
    if size != layer.stored_size {
        ok = false;
        r.issue(
            IssueKind::SizeMismatch,
            Some(m),
            &layer.key,
            format!("{size} bytes stored, manifest says {}", layer.stored_size),
        );
    }
    let plain = match &layer.encryption {
        None => {
            let _ = tokio::fs::rename(&stored, dest).await;
            Some(dest.to_path_buf())
        }
        Some(e) => match key {
            Some(k) if k.key_id == e.key_id => {
                let (src, dst, raw) = (stored.clone(), dest.to_path_buf(), k.key);
                let res =
                    tokio::task::spawn_blocking(move || crypto::decrypt_file(&src, &dst, &raw))
                        .await;
                let _ = tokio::fs::remove_file(&stored).await;
                match res {
                    Ok(Ok(())) => Some(dest.to_path_buf()),
                    Ok(Err(err)) => {
                        ok = false;
                        r.issue(
                            IssueKind::DecryptFailed,
                            Some(m),
                            &layer.key,
                            format!("{err:#}"),
                        );
                        None
                    }
                    Err(err) => {
                        ok = false;
                        r.issue(
                            IssueKind::DecryptFailed,
                            Some(m),
                            &layer.key,
                            err.to_string(),
                        );
                        None
                    }
                }
            }
            other => {
                r.unverified_layers += 1;
                r.warnings.push(match other {
                    Some(k) => format!(
                        "{} is encrypted with key {:?}, not {:?}: digest not checked",
                        layer.key, e.key_id, k.key_id
                    ),
                    None => format!(
                        "{} is encrypted with key {:?} and no key was given: digest not checked",
                        layer.key, e.key_id
                    ),
                });
                let _ = tokio::fs::remove_file(&stored).await;
                None
            }
        },
    };
    if let Some(p) = &plain {
        let p2 = p.clone();
        match tokio::task::spawn_blocking(move || crypto::sha256_file(&p2)).await {
            Ok(Ok(sha)) if sha == layer.sha256 => {}
            Ok(Ok(sha)) => {
                ok = false;
                r.issue(
                    IssueKind::DigestMismatch,
                    Some(m),
                    &layer.key,
                    format!("sha256 {sha}, manifest says {}", layer.sha256),
                );
            }
            Ok(Err(e)) => {
                ok = false;
                r.issue(
                    IssueKind::LayerMissing,
                    Some(m),
                    &layer.key,
                    format!("{e:#}"),
                );
            }
            Err(e) => {
                ok = false;
                r.issue(
                    IssueKind::DigestMismatch,
                    Some(m),
                    &layer.key,
                    e.to_string(),
                );
            }
        }
    }
    plain.is_some() && ok
}

fn scratch_dir(opts: &VerifyOptions, what: &str) -> Result<PathBuf> {
    let dir = opts.scratch.join(format!(
        "{what}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating scratch directory {}", dir.display()))?;
    Ok(dir)
}

/// Verifies the chain ending at `manifest_key`: manifests, layer objects and chain completeness. Never
/// restores or writes anything but temporary files under `opts.scratch`.
pub async fn verify_chain(
    store: &dyn ObjectStore,
    manifest_key: &str,
    opts: &VerifyOptions,
) -> Result<ChainReport> {
    let t0 = std::time::Instant::now();
    let mut r = ChainReport::new(ReportKind::Verify, manifest_key);
    let dir = scratch_dir(opts, "verify")?;
    let links = walk(store, manifest_key, &opts.synthetic_bases, &mut r).await;
    r.backups_checked = links.len();
    check_links(&mut r, &links);
    for l in links.iter().rev() {
        for layer in &l.m.disks {
            let dest = dir.join(format!("{:08}-{}.qcow2", l.m.sequence, layer.name));
            fetch_layer(store, &mut r, &l.m, layer, opts.key.as_ref(), &dest).await;
            let _ = tokio::fs::remove_file(&dest).await;
        }
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(r.finish(t0))
}

// ---- test-restore ----------------------------------------------------------------------------------

const QCOW2_MAGIC: [u8; 4] = *b"QFI\xfb";

/// Checks a qcow2 header: magic, version 2 or 3, and the virtual size (header bytes 24..32).
pub fn check_qcow2_header(path: &Path, want_size: u64) -> std::result::Result<(), String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("opening layer: {e}"))?;
    let mut h = [0u8; 32];
    f.read_exact(&mut h)
        .map_err(|_| "shorter than a qcow2 header".to_string())?;
    if h[..4] != QCOW2_MAGIC {
        return Err("not a qcow2 image (bad magic)".into());
    }
    let version = u32::from_be_bytes(h[4..8].try_into().unwrap());
    if !(2..=3).contains(&version) {
        return Err(format!("unsupported qcow2 version {version}"));
    }
    let size = u64::from_be_bytes(h[24..32].try_into().unwrap());
    if size != want_size {
        return Err(format!("virtual size {size}, manifest says {want_size}"));
    }
    Ok(())
}

async fn run_qemu_img(bin: &str, args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
    tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {bin}"))
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

/// Restores the chain ending at `manifest_key` into a scratch directory and checks the image. The
/// chain is verified first (a corrupt or incomplete chain is reported and not restored).
pub async fn test_restore_chain(
    store: &dyn ObjectStore,
    manifest_key: &str,
    opts: &TestRestoreOptions,
) -> Result<ChainReport> {
    let t0 = std::time::Instant::now();
    let mut r = ChainReport::new(ReportKind::TestRestore, manifest_key);
    let dir = scratch_dir(&opts.verify, "test-restore")?;
    let links = walk(store, manifest_key, &opts.verify.synthetic_bases, &mut r).await;
    r.backups_checked = links.len();
    check_links(&mut r, &links);
    let chain_ok = r.issues.is_empty();

    // Download and check every layer (base first), keeping the plaintext ones for stacking.
    let mut qemu_img = opts.qemu_img.clone();
    if let Some(bin) = &qemu_img
        && tokio::process::Command::new(bin)
            .arg("--version")
            .output()
            .await
            .is_err()
    {
        r.warnings
            .push(format!("{bin} cannot be run; checking qcow2 headers only"));
        qemu_img = None;
    }
    let mut layers: Vec<(usize, String, PathBuf, u64)> = vec![]; // (link index, disk, path, virtual size)
    let mut all_plain = true;
    for (i, l) in links.iter().enumerate().rev() {
        for layer in &l.m.disks {
            let dest = dir.join(format!("{:08}-{}.qcow2", l.m.sequence, layer.name));
            let got =
                fetch_layer(store, &mut r, &l.m, layer, opts.verify.key.as_ref(), &dest).await;
            match got {
                true => {
                    if let Err(e) = check_qcow2_header(&dest, layer.virtual_size) {
                        r.issue(IssueKind::ImageInvalid, Some(&l.m), &layer.key, e);
                    }
                    layers.push((i, layer.name.clone(), dest, layer.virtual_size));
                }
                false => all_plain = false,
            }
        }
    }
    r.image_check = Some(if qemu_img.is_some() {
        ImageCheck::QemuImg
    } else {
        ImageCheck::Header
    });

    let top = links.first().map(|l| &l.m);
    if let Some(top) = top {
        for d in &top.disks {
            let mut check = DiskCheck {
                name: d.name.clone(),
                virtual_size: d.virtual_size,
                ok: false,
                detail: String::new(),
            };
            if !chain_ok || !all_plain {
                check.detail =
                    "not restored: the chain failed verification or has unverifiable layers".into();
                r.disks.push(check);
                continue;
            }
            // Layers of this disk, base first (links are newest first, so larger index = older).
            let mut mine: Vec<&(usize, String, PathBuf, u64)> =
                layers.iter().filter(|(_, n, _, _)| *n == d.name).collect();
            mine.sort_by_key(|(i, ..)| std::cmp::Reverse(*i));
            let Some(bin) = qemu_img.as_deref() else {
                check.ok = mine.len() == links.len();
                check.detail = format!("{} layer header(s) checked", mine.len());
                r.disks.push(check);
                continue;
            };
            match restore_disk(bin, &mine, &dir, &d.name, d.virtual_size).await {
                Ok(detail) => {
                    check.ok = true;
                    check.detail = detail;
                }
                Err(e) => {
                    check.detail = format!("{e:#}");
                    r.issue(
                        IssueKind::ImageInvalid,
                        Some(top),
                        &d.key,
                        check.detail.clone(),
                    );
                }
            }
            r.disks.push(check);
        }
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(r.finish(t0))
}

/// Stacks `layers` (base first) with `qemu-img rebase -u`, flattens the top into a standalone image
/// and checks it.
async fn restore_disk(
    bin: &str,
    layers: &[&(usize, String, PathBuf, u64)],
    dir: &Path,
    name: &str,
    want_size: u64,
) -> Result<String> {
    let mut prev: Option<&Path> = None;
    for (_, _, path, _) in layers {
        if let Some(p) = prev {
            let out = run_qemu_img(
                bin,
                &[
                    "rebase".as_ref(),
                    "-u".as_ref(),
                    "-f".as_ref(),
                    "qcow2".as_ref(),
                    "-b".as_ref(),
                    p.as_os_str(),
                    "-F".as_ref(),
                    "qcow2".as_ref(),
                    path.as_os_str(),
                ],
            )
            .await?;
            if !out.status.success() {
                anyhow::bail!("stacking layers: {}", lossy(&out.stderr));
            }
        }
        prev = Some(path);
    }
    let top = prev.context("no layers to restore")?;
    let restored = dir.join(format!("{name}.restored.qcow2"));
    let out = run_qemu_img(
        bin,
        &[
            "convert".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            "-O".as_ref(),
            "qcow2".as_ref(),
            top.as_os_str(),
            restored.as_os_str(),
        ],
    )
    .await?;
    if !out.status.success() {
        anyhow::bail!("restoring to scratch: {}", lossy(&out.stderr));
    }
    let out = run_qemu_img(
        bin,
        &[
            "check".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            restored.as_os_str(),
        ],
    )
    .await?;
    // Exit 3 means leaked clusters: wasted space, not corruption.
    let leaked = out.status.code() == Some(3);
    if !out.status.success() && !leaked {
        anyhow::bail!("qemu-img check: {}", lossy(&out.stdout));
    }
    let out = run_qemu_img(
        bin,
        &[
            "info".as_ref(),
            "--output=json".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            restored.as_os_str(),
        ],
    )
    .await?;
    let info: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing qemu-img info")?;
    let size = info["virtual-size"].as_u64().unwrap_or(0);
    if size != want_size {
        anyhow::bail!("restored virtual size {size}, manifest says {want_size}");
    }
    Ok(format!(
        "restored and checked ({} layers{})",
        layers.len(),
        if leaked { ", leaked clusters" } else { "" }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{DiskLayer, MANIFEST_VERSION};
    use crate::mem::MemStore;
    use crate::{PutOptions, backup_dir};
    use sha2::{Digest, Sha256};

    const VSIZE: u64 = 1 << 20;

    /// A 512-byte stand-in for a qcow2 layer: valid header, zero padding, distinct tail.
    fn fake_qcow2(tag: u8) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[..4].copy_from_slice(&QCOW2_MAGIC);
        b[4..8].copy_from_slice(&3u32.to_be_bytes());
        b[24..32].copy_from_slice(&VSIZE.to_be_bytes());
        b[500] = tag;
        b
    }

    fn manifest_key(seq: u64, id: &str) -> String {
        format!("{}/manifest.json", backup_dir("ns/vm", "c1", seq, id))
    }

    /// Stores a backup of `data` as the layer of disk `root`, returning its manifest.
    async fn put_backup(
        store: &MemStore,
        seq: u64,
        id: &str,
        kind: ManifestKind,
        parent: Option<&str>,
        data: Vec<u8>,
    ) -> Manifest {
        let dir = backup_dir("ns/vm", "c1", seq, id);
        let key = format!("{dir}/root.qcow2");
        let m = Manifest {
            version: MANIFEST_VERSION,
            backup_id: id.into(),
            chain_id: "c1".into(),
            sequence: seq,
            kind,
            parent: parent.map(|p| ParentRef {
                backup_id: p.into(),
                sequence: seq - 1,
                manifest_key: manifest_key(seq - 1, p),
            }),
            vm_name: "vm".into(),
            created_at: Utc::now(),
            quiesced: false,
            disks: vec![DiskLayer {
                name: "root".into(),
                key: key.clone(),
                virtual_size: VSIZE,
                stored_size: data.len() as u64,
                changed_bytes: 512,
                sha256: hex::encode(Sha256::digest(&data)),
                encryption: None,
            }],
            consolidates: vec![],
            retain_until: None,
        };
        let o = PutOptions::default();
        store.put_bytes(&key, data, &o).await.unwrap();
        store
            .put_bytes(&manifest_key(seq, id), m.to_bytes().unwrap(), &o)
            .await
            .unwrap();
        m
    }

    async fn chain3(store: &MemStore) {
        put_backup(store, 1, "b1", ManifestKind::Full, None, fake_qcow2(1)).await;
        put_backup(
            store,
            2,
            "b2",
            ManifestKind::Incremental,
            Some("b1"),
            fake_qcow2(2),
        )
        .await;
        put_backup(
            store,
            3,
            "b3",
            ManifestKind::Incremental,
            Some("b2"),
            fake_qcow2(3),
        )
        .await;
    }

    fn opts(dir: &Path) -> VerifyOptions {
        VerifyOptions {
            scratch: dir.to_path_buf(),
            ..Default::default()
        }
    }

    fn kinds(r: &ChainReport) -> Vec<IssueKind> {
        r.issues.iter().map(|i| i.kind).collect()
    }

    #[tokio::test]
    async fn an_intact_chain_verifies() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        chain3(&store).await;
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert!(r.ok, "{:?}", r.issues);
        assert_eq!((r.backups_checked, r.layers_checked), (3, 3));
        assert_eq!(r.bytes_read, 3 * 512);
        assert_eq!(r.chain_id.as_deref(), Some("c1"));
        assert!(r.summary().starts_with("verify ok"));
        // Scratch space is cleaned up.
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_corrupt_object_is_a_digest_mismatch() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        chain3(&store).await;
        let key = format!("{}/root.qcow2", backup_dir("ns/vm", "c1", 2, "b2"));
        let mut bad = fake_qcow2(2);
        bad[300] ^= 0xff;
        store.overwrite(&key, bad);
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert!(!r.ok);
        assert_eq!(kinds(&r), [IssueKind::DigestMismatch]);
        assert_eq!(r.issues[0].backup_id.as_deref(), Some("b2"));
        assert_eq!(r.issues[0].key, key);
        // A truncated object is a size mismatch as well.
        store.overwrite(&key, vec![0u8; 10]);
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert!(kinds(&r).contains(&IssueKind::SizeMismatch));
        assert!(r.summary().contains("FAILED"));
    }

    #[tokio::test]
    async fn a_missing_chain_link_is_reported() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        chain3(&store).await;
        store.delete(&manifest_key(2, "b2")).await.unwrap();
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert!(!r.ok);
        let k = kinds(&r);
        assert!(k.contains(&IssueKind::ManifestMissing), "{k:?}");
        assert!(k.contains(&IssueKind::ChainBroken), "{k:?}");
        assert_eq!(r.backups_checked, 1);
    }

    #[tokio::test]
    async fn a_missing_layer_and_a_bad_parent_are_reported() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        chain3(&store).await;
        store
            .delete(&format!(
                "{}/root.qcow2",
                backup_dir("ns/vm", "c1", 1, "b1")
            ))
            .await
            .unwrap();
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert_eq!(kinds(&r), [IssueKind::LayerMissing]);

        // b3 claims a parent whose id does not match what the key holds.
        let mut m3 =
            Manifest::from_bytes(&store.get_bytes(&manifest_key(3, "b3")).await.unwrap()).unwrap();
        m3.parent.as_mut().unwrap().backup_id = "other".into();
        store.overwrite(&manifest_key(3, "b3"), m3.to_bytes().unwrap());
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert!(kinds(&r).contains(&IssueKind::ChainBroken));

        // A manifest that is not JSON.
        store.overwrite(&manifest_key(3, "b3"), b"nope".to_vec());
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert_eq!(kinds(&r), [IssueKind::ManifestInvalid]);
    }

    #[tokio::test]
    async fn a_manifest_under_the_wrong_key_is_inconsistent() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        chain3(&store).await;
        let moved = manifest_key(1, "b1").replace("00000001-b1", "00000009-zz");
        store
            .put_bytes(
                &moved,
                store.get_bytes(&manifest_key(1, "b1")).await.unwrap(),
                &PutOptions::default(),
            )
            .await
            .unwrap();
        let r = verify_chain(&store, &moved, &opts(tmp.path()))
            .await
            .unwrap();
        assert!(
            kinds(&r).contains(&IssueKind::ManifestInconsistent),
            "{:?}",
            r.issues
        );
    }

    #[tokio::test]
    async fn encrypted_layers_need_the_key_to_be_digest_checked() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        let key = [7u8; 32];
        // One encrypted full backup.
        let plain = fake_qcow2(9);
        let (p, e) = (tmp.path().join("p"), tmp.path().join("e"));
        std::fs::write(&p, &plain).unwrap();
        crypto::encrypt_file(&p, &e, &key).unwrap();
        let enc = std::fs::read(&e).unwrap();
        let m = put_backup(&store, 1, "b1", ManifestKind::Full, None, enc.clone()).await;
        let mut m = m;
        m.disks[0].sha256 = hex::encode(Sha256::digest(&plain));
        m.disks[0].encryption = Some(crate::LayerEncryption {
            algorithm: crypto::ALGORITHM.into(),
            key_id: "k1".into(),
        });
        store.overwrite(&manifest_key(1, "b1"), m.to_bytes().unwrap());
        let scratch = tempfile::tempdir().unwrap();

        let r = verify_chain(&store, &manifest_key(1, "b1"), &opts(scratch.path()))
            .await
            .unwrap();
        assert!(
            r.ok && r.unverified_layers == 1 && r.warnings.len() == 1,
            "{r:?}"
        );

        let mut o = opts(scratch.path());
        o.key = Some(DecryptKey {
            key_id: "k1".into(),
            key,
        });
        let r = verify_chain(&store, &manifest_key(1, "b1"), &o)
            .await
            .unwrap();
        assert!(r.ok && r.unverified_layers == 0, "{r:?}");

        // A flipped ciphertext byte fails authentication.
        let mut bad = enc;
        let n = bad.len() - 20;
        bad[n] ^= 1;
        store.overwrite(&m.disks[0].key, bad);
        let r = verify_chain(&store, &manifest_key(1, "b1"), &o)
            .await
            .unwrap();
        assert!(kinds(&r).contains(&IssueKind::DecryptFailed), "{r:?}");

        // The wrong key is reported as such, not as corruption.
        let mut wrong = opts(scratch.path());
        wrong.key = Some(DecryptKey {
            key_id: "k2".into(),
            key,
        });
        let r = verify_chain(&store, &manifest_key(1, "b1"), &wrong)
            .await
            .unwrap();
        assert_eq!(r.unverified_layers, 1);
    }

    #[tokio::test]
    async fn test_restore_checks_headers_without_qemu_img() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        chain3(&store).await;
        let o = TestRestoreOptions {
            verify: opts(tmp.path()),
            qemu_img: None,
        };
        let r = test_restore_chain(&store, &manifest_key(3, "b3"), &o)
            .await
            .unwrap();
        assert!(r.ok, "{:?}", r.issues);
        assert_eq!(r.image_check, Some(ImageCheck::Header));
        assert_eq!(r.kind, ReportKind::TestRestore);
        assert_eq!(r.disks.len(), 1);
        assert!(r.disks[0].ok);

        // A layer that is not a qcow2 image, but whose digest matches its manifest.
        let junk = vec![1u8; 512];
        put_backup(&store, 3, "b3", ManifestKind::Incremental, Some("b2"), junk).await;
        let r = test_restore_chain(&store, &manifest_key(3, "b3"), &o)
            .await
            .unwrap();
        assert!(!r.ok);
        assert!(kinds(&r).contains(&IssueKind::ImageInvalid));

        // A binary that cannot be run falls back to header checks with a warning.
        let o = TestRestoreOptions {
            verify: opts(tmp.path()),
            qemu_img: Some("/nonexistent/qemu-img".into()),
        };
        put_backup(
            &store,
            3,
            "b3",
            ManifestKind::Incremental,
            Some("b2"),
            fake_qcow2(3),
        )
        .await;
        let r = test_restore_chain(&store, &manifest_key(3, "b3"), &o)
            .await
            .unwrap();
        assert!(r.ok && r.image_check == Some(ImageCheck::Header) && !r.warnings.is_empty());
    }

    #[tokio::test]
    async fn test_restore_does_not_restore_a_broken_chain() {
        let (store, tmp) = (MemStore::new(), tempfile::tempdir().unwrap());
        chain3(&store).await;
        store.delete(&manifest_key(1, "b1")).await.unwrap();
        let o = TestRestoreOptions {
            verify: opts(tmp.path()),
            qemu_img: None,
        };
        let r = test_restore_chain(&store, &manifest_key(3, "b3"), &o)
            .await
            .unwrap();
        assert!(!r.ok);
        assert!(r.disks.iter().all(|d| !d.ok));
    }

    #[tokio::test]
    async fn a_dir_store_works_the_same() {
        let (root, tmp) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = crate::dir::DirStore::new(root.path().to_path_buf());
        let mem = MemStore::new();
        chain3(&mem).await;
        for k in mem.keys() {
            store
                .put_bytes(&k, mem.get_bytes(&k).await.unwrap(), &PutOptions::default())
                .await
                .unwrap();
        }
        let r = verify_chain(&store, &manifest_key(3, "b3"), &opts(tmp.path()))
            .await
            .unwrap();
        assert!(r.ok, "{:?}", r.issues);
    }
}
