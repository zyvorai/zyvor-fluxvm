// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Changed-block-tracking backups to object storage.
//!
//! Each backup is one QMP `transaction` that, per disk, adds a persistent
//! checkpoint bitmap `kairon-cbt-<chain>-<seq>` and starts a
//! `blockdev-backup` into a local qcow2 layer: `sync=full` for a full,
//! `sync=bitmap` over the parent checkpoint for an incremental. The parent
//! bitmap is only read, so it keeps every write since the parent backup; it
//! is dropped once the new layer is safely uploaded, and on failure the new
//! checkpoint is dropped instead. Leftover checkpoints from interrupted runs
//! are subsets of the parent and are simply removed.
//!
//! A running QEMU VM is backed up through its own QMP socket; a stopped VM
//! through a short-lived `qemu-storage-daemon` on its disk files, which
//! reads and updates the same persistent bitmaps.
//!
//! Layers go to S3 (or a directory) under
//! `<prefix>/<chain>/<seq>-<backup>/{manifest.json,<disk>.qcow2}`, optionally
//! AES-256-GCM encrypted. Restore downloads the chain, stacks the layers
//! with `qemu-img rebase -u` and converts the top into the VM's disks.
//!
//! Jobs are pollable and outlive the request: their state is a file under
//! `state_dir/cbt-jobs`. Credentials and keys arrive per request and are
//! never written there.

use crate::{VmManager, audit_event, backup};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use fluxvm_backup_s3::{
    DiskLayer, LayerEncryption, Manifest, ManifestKind, ObjectStore, ParentRef, PutOptions, Target,
    crypto, manifest::MANIFEST_FILE, manifest::MANIFEST_VERSION,
};
use fluxvm_core::model::{BackendKind, BackupQuiesce, VmRecord, VmStatus};
use fluxvm_qemu::block::{self, BackupSync, Bitmap, DiskBackup};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

pub use fluxvm_backup_s3::crypto::ALGORITHM as ENCRYPTION_ALGORITHM;

pub const BITMAP_PREFIX: &str = "kairon-cbt-";
const QMP_TIMEOUT: Duration = Duration::from_secs(30);
const JOB_POLL: Duration = Duration::from_millis(500);
/// Chains longer than this are refused rather than walked.
const MAX_CHAIN: usize = 1000;

/// The checkpoint bitmap a backup at `sequence` of `chain` leaves behind.
pub fn checkpoint_name(chain: &str, sequence: u64) -> String {
    format!("{BITMAP_PREFIX}{chain}-{sequence}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CbtMode {
    Full,
    Incremental,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncryptionKey {
    pub key_id: String,
    /// Base64 of 32 bytes.
    pub key: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CbtBackupRequest {
    pub target: Target,
    /// Key prefix, e.g. `<namespace>/<machine>`.
    pub prefix: String,
    pub chain_id: String,
    pub sequence: u64,
    pub backup_id: String,
    pub mode: CbtMode,
    /// Required for an incremental.
    #[serde(default)]
    pub parent: Option<ParentRef>,
    #[serde(default)]
    pub quiesce: BackupQuiesce,
    #[serde(default)]
    pub encryption: Option<EncryptionKey>,
    #[serde(default)]
    pub retain_until: Option<DateTime<Utc>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CbtRestoreRequest {
    pub target: Target,
    /// The manifest of the chain point to restore.
    pub manifest_key: String,
    #[serde(default)]
    pub encryption: Option<EncryptionKey>,
    /// Synthetic fulls of this chain; one consolidating a parent replaces
    /// the rest of the walk.
    #[serde(default)]
    pub synthetic_bases: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CbtSynthesizeRequest {
    pub target: Target,
    pub prefix: String,
    /// The chain point to consolidate.
    pub manifest_key: String,
    /// Id of the synthetic full.
    pub backup_id: String,
    /// Decrypts the chain and encrypts the synthetic full.
    #[serde(default)]
    pub encryption: Option<EncryptionKey>,
    #[serde(default)]
    pub retain_until: Option<DateTime<Utc>>,
    /// Synthetic fulls of this chain; one consolidating a parent replaces
    /// the rest of the walk.
    #[serde(default)]
    pub synthetic_bases: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CbtDeleteRequest {
    pub target: Target,
    pub manifest_keys: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CbtDeleteResult {
    pub deleted: Vec<String>,
    pub not_found: Vec<String>,
    /// Objects still under Object Lock retention.
    pub retention_held: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CbtJobKind {
    Backup,
    Restore,
    Synthesize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CbtJobState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CbtJob {
    pub id: Uuid,
    pub kind: CbtJobKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<Uuid>,
    pub state: CbtJobState,
    /// `preparing`, `copying`, `uploading`, `downloading`, `writing`, `done`.
    pub phase: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The incremental could not run (bitmap missing, inconsistent after an
    /// unclean stop, or a new disk); the next backup must be a full.
    #[serde(default)]
    pub full_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<Manifest>,
    /// Disks left out of a backup or restore, with the reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedDisk>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedDisk {
    pub name: String,
    pub reason: String,
}

/// An incremental that can't run; the caller should take a full.
#[derive(Debug)]
pub struct FullRequired(pub String);

impl std::fmt::Display for FullRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "full backup required: {}", self.0)
    }
}

impl std::error::Error for FullRequired {}

/// What to do with the bitmaps on the disks before a backup.
#[derive(Debug, PartialEq, Eq)]
pub struct BitmapPlan {
    /// `(node, bitmap)` checkpoints to drop first: leftovers of interrupted
    /// runs, and any stale bitmap named like the new checkpoint.
    pub stale: Vec<(String, String)>,
    pub sync: BackupSync,
}

/// Plans the bitmaps for a backup of `nodes`. An incremental needs the
/// parent checkpoint, consistent and idle, on every disk.
pub fn plan_bitmaps(
    bitmaps: &[Bitmap],
    nodes: &[String],
    chain: &str,
    sequence: u64,
    mode: CbtMode,
    parent_sequence: Option<u64>,
) -> Result<BitmapPlan> {
    let checkpoint = checkpoint_name(chain, sequence);
    let parent = match mode {
        CbtMode::Full => None,
        CbtMode::Incremental => {
            let p = parent_sequence.context("an incremental backup needs a parent")?;
            if p >= sequence {
                bail!("parent sequence {p} must be below sequence {sequence}");
            }
            Some(checkpoint_name(chain, p))
        }
    };
    if let Some(parent) = &parent {
        for node in nodes {
            match bitmaps
                .iter()
                .find(|b| &b.node == node && &b.name == parent)
            {
                None => {
                    return Err(
                        FullRequired(format!("disk {node} has no checkpoint {parent}")).into(),
                    );
                }
                Some(b) if b.inconsistent => {
                    return Err(FullRequired(format!(
                        "checkpoint {parent} on {node} is inconsistent (the VM stopped uncleanly)"
                    ))
                    .into());
                }
                Some(b) if b.busy => {
                    bail!("checkpoint {parent} on {node} is in use by another job")
                }
                Some(_) => {}
            }
        }
    }
    let stale = bitmaps
        .iter()
        .filter(|b| nodes.contains(&b.node))
        .filter(|b| {
            b.name == checkpoint
                || (parent.is_some()
                    && Some(&b.name) != parent.as_ref()
                    && b.name.starts_with(&format!("{BITMAP_PREFIX}{chain}-")))
        })
        .map(|b| (b.node.clone(), b.name.clone()))
        .collect();
    Ok(BitmapPlan {
        stale,
        sync: match parent {
            None => BackupSync::Full,
            Some(from) => BackupSync::Incremental { from },
        },
    })
}

/// Bitmaps to drop once a backup leaving `checkpoint` is stored: every
/// other CBT checkpoint on the backed-up disks, whatever its chain.
pub fn superseded(bitmaps: &[Bitmap], nodes: &[String], checkpoint: &str) -> Vec<(String, String)> {
    bitmaps
        .iter()
        .filter(|b| {
            nodes.contains(&b.node) && b.name.starts_with(BITMAP_PREFIX) && b.name != checkpoint
        })
        .map(|b| (b.node.clone(), b.name.clone()))
        .collect()
}

/// Checks a chain read top-down (`chain[0]` is the requested point) and
/// returns it base-first.
pub fn order_chain(mut chain: Vec<Manifest>) -> Result<Vec<Manifest>> {
    let Some(top) = chain.first() else {
        bail!("empty backup chain");
    };
    let chain_id = top.chain_id.clone();
    for pair in chain.windows(2) {
        let (child, parent) = (&pair[0], &pair[1]);
        if parent.chain_id != chain_id {
            bail!(
                "backup {} belongs to chain {}, not {chain_id}",
                parent.backup_id,
                parent.chain_id
            );
        }
        let want = child
            .parent
            .as_ref()
            .context("chain continues past a base")?;
        let stands_in = parent.kind == ManifestKind::SyntheticFull
            && parent.consolidates.contains(&want.backup_id);
        if (want.backup_id != parent.backup_id && !stands_in) || want.sequence != parent.sequence {
            bail!(
                "backup {} names parent {} but the chain holds {}",
                child.backup_id,
                want.backup_id,
                parent.backup_id
            );
        }
        if parent.sequence >= child.sequence {
            bail!("chain sequences must increase towards {}", top.backup_id);
        }
    }
    let base = chain.last().unwrap();
    if base.kind.needs_parent() {
        bail!("chain of {chain_id} has no full backup at its base");
    }
    chain.reverse();
    Ok(chain)
}

fn jobs_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("cbt-jobs")
}

fn work_root(state_dir: &Path) -> PathBuf {
    state_dir.join("cbt-work")
}

/// Jobs running in this process; a `running` job file not listed here was
/// cut short by a restart.
fn live_jobs() -> &'static Mutex<HashSet<Uuid>> {
    static LIVE: std::sync::OnceLock<Mutex<HashSet<Uuid>>> = std::sync::OnceLock::new();
    LIVE.get_or_init(Default::default)
}

/// VMs with a backup or restore in flight.
fn busy_vms() -> &'static Mutex<HashSet<Uuid>> {
    static BUSY: std::sync::OnceLock<Mutex<HashSet<Uuid>>> = std::sync::OnceLock::new();
    BUSY.get_or_init(Default::default)
}

struct VmClaim(Uuid);

impl VmClaim {
    fn take(id: Uuid) -> Result<Self> {
        if !busy_vms()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id)
        {
            bail!("VM {id} already has a CBT backup or restore running");
        }
        Ok(Self(id))
    }
}

impl Drop for VmClaim {
    fn drop(&mut self) {
        busy_vms()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

struct JobHandle {
    path: PathBuf,
    job: Mutex<CbtJob>,
}

impl JobHandle {
    fn create(state_dir: &Path, kind: CbtJobKind, vm_id: Option<Uuid>) -> Result<Arc<Self>> {
        let dir = jobs_dir(state_dir);
        std::fs::create_dir_all(&dir)?;
        let job = CbtJob {
            id: Uuid::new_v4(),
            kind,
            vm_id,
            state: CbtJobState::Running,
            phase: "preparing".into(),
            bytes_done: 0,
            bytes_total: 0,
            created_at: Utc::now(),
            finished_at: None,
            error: None,
            full_required: false,
            manifest_key: None,
            manifest: None,
            skipped: vec![],
        };
        live_jobs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(job.id);
        let h = Arc::new(Self {
            path: dir.join(format!("{}.json", job.id)),
            job: Mutex::new(job),
        });
        h.save();
        Ok(h)
    }

    fn update(&self, f: impl FnOnce(&mut CbtJob)) {
        f(&mut self.job.lock().unwrap_or_else(|p| p.into_inner()));
        self.save();
    }

    fn save(&self) {
        let job = self.job.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Err(e) = write_job(&self.path, &job) {
            tracing::warn!(job=%job.id, "writing CBT job state failed: {e:#}");
        }
    }

    fn snapshot(&self) -> CbtJob {
        self.job.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn finish(&self, result: Result<()>) {
        self.update(|j| {
            j.finished_at = Some(Utc::now());
            match result {
                Ok(()) => {
                    j.state = CbtJobState::Succeeded;
                    j.phase = "done".into();
                }
                Err(e) => {
                    j.state = CbtJobState::Failed;
                    j.full_required = e.downcast_ref::<FullRequired>().is_some();
                    j.error = Some(format!("{e:#}"));
                }
            }
        });
        let id = self.snapshot().id;
        live_jobs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }
}

/// A job still marked running but not running in this process was cut
/// short by a restart; it is reported (and recorded) as failed.
fn load_job(state_dir: &Path, id: Uuid) -> Result<Option<CbtJob>> {
    let path = jobs_dir(state_dir).join(format!("{id}.json"));
    let mut job: CbtJob = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if job.state == CbtJobState::Running
        && !live_jobs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&id)
    {
        job.state = CbtJobState::Failed;
        job.error = Some("interrupted by a FluxVM restart".into());
        job.finished_at = Some(Utc::now());
        let _ = write_job(&path, &job);
    }
    Ok(Some(job))
}

fn write_job(path: &Path, job: &CbtJob) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(job)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The disks a backup of `vm` covers, as `(name, block node, file)`, and
/// those it leaves out.
fn cbt_disks(vm: &VmRecord) -> (Vec<(String, String, PathBuf)>, Vec<SkippedDisk>) {
    let mut disks = vec![(
        fluxvm_qemu::disks::ROOT_DISK.to_string(),
        fluxvm_qemu::disks::ROOT_NODE.to_string(),
        vm.disk.clone(),
    )];
    let mut skipped = vec![];
    for (name, path) in fluxvm_qemu::disks::data_disks(&vm.workspace) {
        let reason = if fluxvm_qemu::disks::is_external(&path) {
            Some("attached from an existing image or volume")
        } else if backup::data_disk_format(&path) != "qcow2" {
            Some("raw disks have no persistent dirty bitmaps")
        } else if !path.is_file() {
            Some("not a disk file")
        } else {
            None
        };
        match reason {
            Some(r) => skipped.push(SkippedDisk {
                name,
                reason: r.into(),
            }),
            None => disks.push((name.clone(), fluxvm_qemu::disks::node_name(&name), path)),
        }
    }
    (disks, skipped)
}

/// Why `vm` can't take a CBT backup, if it can't.
pub fn cbt_refusal(vm: &VmRecord) -> Option<String> {
    if vm.backend != BackendKind::Qemu {
        return Some(format!(
            "CBT backup needs the QEMU engine (this VM uses {:?})",
            vm.backend
        ));
    }
    if backup::root_disk_format(vm) != "qcow2" || !vm.disk.is_file() {
        return Some("CBT backup needs a qcow2 root disk file (raw, NBD and Ceph roots have no dirty bitmaps)".into());
    }
    if !matches!(
        vm.status,
        VmStatus::Running | VmStatus::Paused | VmStatus::Stopped | VmStatus::Failed
    ) {
        return Some(format!("VM is {:?}", vm.status));
    }
    None
}

/// A QMP endpoint for the VM's disks: the VM's own monitor, or a
/// `qemu-storage-daemon` holding the files of a stopped VM.
struct BlockHost {
    socket: PathBuf,
    daemon: Option<tokio::process::Child>,
}

impl BlockHost {
    async fn daemon(
        binary: &str,
        work: &Path,
        disks: &[(String, String, PathBuf)],
    ) -> Result<Self> {
        let socket = work.join("qsd.sock");
        let mut cmd = tokio::process::Command::new(binary);
        for (_, node, file) in disks {
            cmd.arg("--blockdev").arg(format!(
                "driver=file,node-name={node}-file,filename={}",
                fluxvm_core::backend::path_arg(file)
            ));
            cmd.arg("--blockdev")
                .arg(format!("driver=qcow2,node-name={node},file={node}-file"));
        }
        cmd.arg("--chardev")
            .arg(format!(
                "socket,path={},server=on,wait=off,id=m",
                fluxvm_core::backend::path_arg(&socket)
            ))
            .args(["--monitor", "chardev=m"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("starting {binary}"))?;
        for _ in 0..100 {
            if socket.exists() {
                return Ok(Self {
                    socket,
                    daemon: Some(child),
                });
            }
            if let Some(status) = child.try_wait()? {
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = s.read_to_string(&mut err).await;
                }
                bail!("{binary} exited ({status}): {}", err.trim());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        bail!("{binary} did not open its monitor socket")
    }

    /// Stops the storage daemon cleanly, so persistent bitmaps are flushed.
    async fn close(mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = block::quit(&self.socket, QMP_TIMEOUT).await;
            if tokio::time::timeout(Duration::from_secs(30), child.wait())
                .await
                .is_err()
            {
                tracing::warn!("qemu-storage-daemon did not quit; killing it");
                let _ = child.kill().await;
            }
        }
    }
}

async fn drop_bitmaps(socket: &Path, bitmaps: &[(String, String)]) {
    for (node, name) in bitmaps {
        if let Err(e) = block::remove_bitmap(socket, node, name, QMP_TIMEOUT).await {
            tracing::warn!(node, bitmap = name, "dropping CBT bitmap failed: {e:#}");
        }
    }
}

async fn qemu_img(binary: &str, args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
    let out = tokio::process::Command::new(binary)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {binary}"))?;
    if !out.status.success() {
        bail!(
            "{binary} {}: {}",
            args.first()
                .map(|a| a.to_string_lossy())
                .unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

async fn virtual_size(binary: &str, disk: &Path) -> Result<u64> {
    let out = qemu_img(
        binary,
        &[
            "info".as_ref(),
            "-U".as_ref(),
            "--output=json".as_ref(),
            disk.as_os_str(),
        ],
    )
    .await?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    v["virtual-size"]
        .as_u64()
        .with_context(|| format!("no virtual size for {}", disk.display()))
}

async fn create_layer(binary: &str, path: &Path, size: u64) -> Result<()> {
    qemu_img(
        binary,
        &[
            "create".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            "-o".as_ref(),
            "compression_type=zstd".as_ref(),
            path.as_os_str(),
            size.to_string().as_ref(),
        ],
    )
    .await
    .map(drop)
}

fn layer_encryption(key: Option<&EncryptionKey>) -> Result<Option<([u8; 32], LayerEncryption)>> {
    key.map(|k| {
        Ok((
            crypto::parse_key(&k.key)?,
            LayerEncryption {
                algorithm: crypto::ALGORITHM.into(),
                key_id: k.key_id.clone(),
            },
        ))
    })
    .transpose()
}

/// Uploads `layer` as `key`, encrypted when asked, and describes it.
async fn upload_layer(
    store: &dyn ObjectStore,
    key: &str,
    layer: &Path,
    enc: Option<&([u8; 32], LayerEncryption)>,
    opts: &PutOptions,
) -> Result<(u64, String)> {
    let stored = match enc {
        Some((k, _)) => {
            let out = layer.with_extension("enc");
            let (src, dst, k) = (layer.to_path_buf(), out.clone(), *k);
            tokio::task::spawn_blocking(move || crypto::encrypt_file(&src, &dst, &k)).await??;
            out
        }
        None => layer.to_path_buf(),
    };
    let s = stored.clone();
    let sha = tokio::task::spawn_blocking(move || crypto::sha256_file(&s)).await??;
    let size = tokio::fs::metadata(&stored).await?.len();
    store.put_file(key, &stored, opts).await?;
    Ok((size, sha))
}

/// The synthetic full among `bases` that consolidates `parent`, if any.
fn synthetic_for<'a>(
    bases: &'a [(String, Manifest)],
    chain: &str,
    parent: &ParentRef,
) -> Option<&'a (String, Manifest)> {
    bases.iter().find(|(_, b)| {
        b.kind == ManifestKind::SyntheticFull
            && b.chain_id == chain
            && b.sequence == parent.sequence
            && b.consolidates.contains(&parent.backup_id)
    })
}

/// Reads the chain ending at `manifest_key`, base first, with each
/// manifest's key. The walk stops early at any synthetic full in
/// `synthetic_bases` that consolidates the parent it is about to read, so
/// layers a synthetic full replaced may already be deleted.
async fn read_chain(
    store: &dyn ObjectStore,
    manifest_key: &str,
    synthetic_bases: &[String],
) -> Result<Vec<(String, Manifest)>> {
    let mut bases = vec![];
    for key in synthetic_bases {
        let m = Manifest::from_bytes(
            &store
                .get_bytes(key)
                .await
                .with_context(|| format!("reading synthetic full manifest {key}"))?,
        )?;
        if m.kind != ManifestKind::SyntheticFull {
            bail!("{key} is not a synthetic full");
        }
        bases.push((key.clone(), m));
    }
    let mut keys = vec![];
    let mut manifests: Vec<Manifest> = vec![];
    let mut key = manifest_key.to_string();
    loop {
        if keys.len() >= MAX_CHAIN {
            bail!("backup chain is longer than {MAX_CHAIN}");
        }
        let m = Manifest::from_bytes(
            &store
                .get_bytes(&key)
                .await
                .with_context(|| format!("reading manifest {key}"))?,
        )?;
        let next = m.parent.clone();
        let chain = m.chain_id.clone();
        keys.push(key);
        manifests.push(m);
        match next {
            Some(p) => match synthetic_for(&bases, &chain, &p) {
                Some((k, base)) => {
                    keys.push(k.clone());
                    manifests.push(base.clone());
                    break;
                }
                None => key = p.manifest_key,
            },
            None => break,
        }
    }
    let ordered = order_chain(manifests)?;
    keys.reverse();
    Ok(keys.into_iter().zip(ordered).collect())
}

/// Downloads every layer of disk `name` along `chain` into `work`, verifies
/// and decrypts them, stacks them with `qemu-img rebase -u`, and returns the
/// top layer.
async fn stack_disk(
    qemu_img_bin: &str,
    store: &dyn ObjectStore,
    chain: &[(String, Manifest)],
    name: &str,
    work: &Path,
    key: Option<&EncryptionKey>,
    job: &JobHandle,
) -> Result<PathBuf> {
    let mut prev: Option<PathBuf> = None;
    for (_, m) in chain {
        let layer = m
            .disk(name)
            .with_context(|| format!("backup {} has no layer for disk {name}", m.backup_id))?;
        let stored = work.join(format!("{:08}-{name}.stored", m.sequence));
        store
            .get_file(&layer.key, &stored)
            .await
            .with_context(|| format!("downloading {}", layer.key))?;
        let s = stored.clone();
        let sha = tokio::task::spawn_blocking(move || crypto::sha256_file(&s)).await??;
        if sha != layer.sha256 {
            bail!(
                "{} is corrupt: sha256 {sha}, manifest says {}",
                layer.key,
                layer.sha256
            );
        }
        let qcow = work.join(format!("{:08}-{name}.qcow2", m.sequence));
        match &layer.encryption {
            Some(e) => {
                let k = key.with_context(|| {
                    format!(
                        "{} is encrypted with key {:?}; pass that key",
                        layer.key, e.key_id
                    )
                })?;
                if k.key_id != e.key_id {
                    bail!(
                        "{} is encrypted with key {:?}, not {:?}",
                        layer.key,
                        e.key_id,
                        k.key_id
                    );
                }
                let raw = crypto::parse_key(&k.key)?;
                let (src, dst) = (stored.clone(), qcow.clone());
                tokio::task::spawn_blocking(move || crypto::decrypt_file(&src, &dst, &raw))
                    .await??;
                let _ = tokio::fs::remove_file(&stored).await;
            }
            None => tokio::fs::rename(&stored, &qcow).await?,
        }
        if let Some(p) = &prev {
            qemu_img(
                qemu_img_bin,
                &[
                    "rebase".as_ref(),
                    "-u".as_ref(),
                    "-f".as_ref(),
                    "qcow2".as_ref(),
                    "-b".as_ref(),
                    p.as_os_str(),
                    "-F".as_ref(),
                    "qcow2".as_ref(),
                    qcow.as_os_str(),
                ],
            )
            .await?;
        }
        job.update(|j| j.bytes_done += layer.stored_size);
        prev = Some(qcow);
    }
    prev.context("empty backup chain")
}

fn validate_ids(prefix: &str, chain: &str, backup_id: &str) -> Result<()> {
    fluxvm_backup_s3::validate_prefix(prefix)?;
    fluxvm_backup_s3::validate_key_segment("chain id", chain)?;
    fluxvm_backup_s3::validate_key_segment("backup id", backup_id)
}

impl VmManager {
    fn spawn_cbt_job<F, Fut>(
        self: &Arc<Self>,
        kind: CbtJobKind,
        vm_id: Option<Uuid>,
        claim: Option<VmClaim>,
        run: F,
    ) -> Result<CbtJob>
    where
        F: FnOnce(Arc<Self>, Arc<JobHandle>, PathBuf) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let job = JobHandle::create(&self.cfg.state_dir, kind, vm_id)?;
        let snapshot = job.snapshot();
        let me = self.clone();
        tokio::spawn(async move {
            let _claim = claim;
            let work = work_root(&me.cfg.state_dir).join(snapshot.id.to_string());
            let result = match tokio::fs::create_dir_all(&work).await {
                Ok(()) => run(me.clone(), job.clone(), work.clone()).await,
                Err(e) => Err(e.into()),
            };
            let _ = tokio::fs::remove_dir_all(&work).await;
            if let Err(e) = &result {
                tracing::warn!(job=%snapshot.id, kind=?kind, "CBT job failed: {e:#}");
            }
            job.finish(result);
        });
        Ok(snapshot)
    }

    /// A CBT job by id.
    pub fn cbt_job(&self, id: Uuid) -> Result<Option<CbtJob>> {
        load_job(&self.cfg.state_dir, id)
    }

    /// Starts a full or incremental backup of VM `id`.
    pub async fn start_cbt_backup(
        self: &Arc<Self>,
        id: Uuid,
        req: CbtBackupRequest,
    ) -> Result<CbtJob> {
        validate_ids(&req.prefix, &req.chain_id, &req.backup_id)?;
        if req.mode == CbtMode::Incremental && req.parent.is_none() {
            bail!("an incremental backup needs a parent");
        }
        layer_encryption(req.encryption.as_ref())?;
        req.target.open()?;
        let vm = self.get(id).await?;
        if let Some(why) = cbt_refusal(&vm) {
            bail!(why);
        }
        let claim = VmClaim::take(id)?;
        self.spawn_cbt_job(
            CbtJobKind::Backup,
            Some(id),
            Some(claim),
            move |me, job, work| async move { me.run_cbt_backup(id, req, &job, &work).await },
        )
    }

    async fn run_cbt_backup(
        &self,
        id: Uuid,
        req: CbtBackupRequest,
        job: &JobHandle,
        work: &Path,
    ) -> Result<()> {
        let vm = self.get(id).await?;
        if let Some(why) = cbt_refusal(&vm) {
            bail!(why);
        }
        let store = req.target.open()?;
        let enc = layer_encryption(req.encryption.as_ref())?;
        let (disks, skipped) = cbt_disks(&vm);
        job.update(|j| j.skipped = skipped);
        let nodes: Vec<String> = disks.iter().map(|(_, n, _)| n.clone()).collect();
        let live = matches!(vm.status, VmStatus::Running | VmStatus::Paused);
        let host = if live {
            BlockHost {
                socket: vm.workspace.join("qmp.sock"),
                daemon: None,
            }
        } else {
            BlockHost::daemon(&self.cfg.qemu_storage_daemon_binary, work, &disks).await?
        };
        let result = self
            .cbt_backup_on(
                &vm,
                &req,
                &host,
                &disks,
                &nodes,
                store.as_ref(),
                enc.as_ref(),
                job,
                work,
            )
            .await;
        host.close().await;
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn cbt_backup_on(
        &self,
        vm: &VmRecord,
        req: &CbtBackupRequest,
        host: &BlockHost,
        disks: &[(String, String, PathBuf)],
        nodes: &[String],
        store: &dyn ObjectStore,
        enc: Option<&([u8; 32], LayerEncryption)>,
        job: &JobHandle,
        work: &Path,
    ) -> Result<()> {
        let sock = &host.socket;
        let bitmaps = block::query_bitmaps(sock, QMP_TIMEOUT).await?;
        let plan = plan_bitmaps(
            &bitmaps,
            nodes,
            &req.chain_id,
            req.sequence,
            req.mode,
            req.parent.as_ref().map(|p| p.sequence),
        )?;
        drop_bitmaps(sock, &plan.stale).await;

        let checkpoint = checkpoint_name(&req.chain_id, req.sequence);
        let mut members = vec![];
        let mut layers = vec![];
        for (name, node, file) in disks {
            let size = virtual_size(&self.cfg.qemu_img_binary, file).await?;
            let layer = work.join(format!("{name}.qcow2"));
            create_layer(&self.cfg.qemu_img_binary, &layer, size).await?;
            let target = format!("cbt-target-{node}");
            block::add_target(sock, &target, &layer, QMP_TIMEOUT).await?;
            members.push(DiskBackup {
                node: node.clone(),
                target,
                job_id: format!("cbt-{node}"),
            });
            layers.push((name.clone(), size, layer));
        }

        let quiesced = self
            .backup_freeze(vm, req.quiesce)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(vm=%vm.id, "{e:#}");
                false
            });
        let started = block::transaction(
            sock,
            block::backup_transaction(&members, &checkpoint, &plan.sync),
            QMP_TIMEOUT,
        )
        .await;
        if quiesced {
            let qga = Self::qga_socket_for(vm)?;
            let thawed =
                tokio::task::spawn_blocking(move || fluxvm_image::qga::fsfreeze_thaw(&qga)).await;
            if !matches!(thawed, Ok(Ok(_))) {
                tracing::error!(vm=%vm.id, "thawing guest filesystems after CBT backup failed: {thawed:?}");
            }
        }
        let ids: Vec<String> = members.iter().map(|m| m.job_id.clone()).collect();
        let copied = match started {
            Ok(()) => {
                job.update(|j| j.phase = "copying".into());
                block::wait_jobs(sock, &ids, JOB_POLL, QMP_TIMEOUT, |jobs| {
                    let (done, total) = jobs
                        .iter()
                        .fold((0, 0), |(d, t), j| (d + j.current, t + j.total));
                    job.update(|j| {
                        j.bytes_done = done;
                        j.bytes_total = total;
                    });
                })
                .await
            }
            Err(e) => Err(e),
        };
        for m in &members {
            if let Err(e) = block::remove_target(sock, &m.target, QMP_TIMEOUT).await {
                tracing::warn!(node = m.node, "removing CBT backup target failed: {e:#}");
            }
        }
        let rollback = async |e: anyhow::Error| -> Result<()> {
            let ours: Vec<(String, String)> = nodes
                .iter()
                .map(|n| (n.clone(), checkpoint.clone()))
                .collect();
            drop_bitmaps(sock, &ours).await;
            Err(e)
        };
        let finished = match copied {
            Ok(j) => j,
            Err(e) => return rollback(e).await,
        };

        let dir =
            fluxvm_backup_s3::backup_dir(&req.prefix, &req.chain_id, req.sequence, &req.backup_id);
        let opts = PutOptions {
            retain_until: req.retain_until,
        };
        job.update(|j| {
            j.phase = "uploading".into();
            j.bytes_done = 0;
            j.bytes_total = layers.iter().map(|(_, s, _)| *s).sum();
        });
        let mut uploaded = vec![];
        let mut described = vec![];
        let mut upload = async || -> Result<Manifest> {
            for ((name, size, layer), m) in layers.iter().zip(&members) {
                let key = format!("{dir}/{name}.qcow2");
                let (stored_size, sha256) = upload_layer(store, &key, layer, enc, &opts).await?;
                uploaded.push(key.clone());
                let changed = finished
                    .iter()
                    .find(|j| j.id == m.job_id)
                    .map(|j| j.total)
                    .unwrap_or(0);
                described.push(DiskLayer {
                    name: name.clone(),
                    key,
                    virtual_size: *size,
                    stored_size,
                    changed_bytes: changed,
                    sha256,
                    encryption: enc.map(|(_, e)| e.clone()),
                });
                job.update(|j| j.bytes_done += size);
            }
            let manifest = Manifest {
                version: MANIFEST_VERSION,
                backup_id: req.backup_id.clone(),
                chain_id: req.chain_id.clone(),
                sequence: req.sequence,
                kind: match req.mode {
                    CbtMode::Full => ManifestKind::Full,
                    CbtMode::Incremental => ManifestKind::Incremental,
                },
                parent: if req.mode == CbtMode::Incremental {
                    req.parent.clone()
                } else {
                    None
                },
                vm_name: vm.request.name.clone(),
                created_at: Utc::now(),
                quiesced,
                disks: described.clone(),
                consolidates: vec![],
                retain_until: req.retain_until,
            };
            let key = format!("{dir}/{MANIFEST_FILE}");
            store.put_bytes(&key, manifest.to_bytes()?, &opts).await?;
            Ok(manifest)
        };
        let manifest = match upload().await {
            Ok(m) => m,
            Err(e) => {
                for key in &uploaded {
                    let _ = store.delete(key).await;
                }
                return rollback(e).await;
            }
        };

        // Stored: from here on the new checkpoint is the only one needed.
        match block::query_bitmaps(sock, QMP_TIMEOUT).await {
            Ok(now) => drop_bitmaps(sock, &superseded(&now, nodes, &checkpoint)).await,
            Err(e) => tracing::warn!(vm=%vm.id, "listing bitmaps after CBT backup failed: {e:#}"),
        }
        let key = format!("{dir}/{MANIFEST_FILE}");
        audit_event(
            "vm.cbt_backup",
            &[("vm_id", &vm.id.to_string()), ("manifest", &key)],
        );
        job.update(|j| {
            j.manifest_key = Some(key);
            j.manifest = Some(manifest);
        });
        Ok(())
    }

    /// Restores the chain point at `req.manifest_key` into stopped VM `id`.
    /// The restored disks carry no bitmaps, so the next backup is a full.
    pub async fn start_cbt_restore(
        self: &Arc<Self>,
        id: Uuid,
        req: CbtRestoreRequest,
    ) -> Result<CbtJob> {
        let vm = self.get(id).await?;
        if let Some(why) = backup::backup_refusal(
            vm.backend,
            vm.request.storage,
            vm.disk.is_file(),
            vm.status,
            true,
        ) {
            bail!(why);
        }
        req.target.open()?;
        let claim = VmClaim::take(id)?;
        self.spawn_cbt_job(
            CbtJobKind::Restore,
            Some(id),
            Some(claim),
            move |me, job, work| async move { me.run_cbt_restore(id, req, &job, &work).await },
        )
    }

    async fn run_cbt_restore(
        &self,
        id: Uuid,
        req: CbtRestoreRequest,
        job: &JobHandle,
        work: &Path,
    ) -> Result<()> {
        let store = req.target.open()?;
        job.update(|j| j.phase = "downloading".into());
        let chain = read_chain(store.as_ref(), &req.manifest_key, &req.synthetic_bases).await?;
        let (_, top) = chain.last().context("empty chain")?.clone();
        job.update(|j| {
            j.bytes_total = chain.iter().map(|(_, m)| m.stored_bytes()).sum();
        });
        let vm = self.get(id).await?;
        let mut plan = vec![];
        let mut skipped = vec![];
        for layer in &top.disks {
            let dst = if layer.name == fluxvm_qemu::disks::ROOT_DISK {
                (vm.disk.clone(), backup::root_disk_format(&vm))
            } else {
                if fluxvm_qemu::disks::validate_disk_name(&layer.name).is_err() {
                    skipped.push(SkippedDisk {
                        name: layer.name.clone(),
                        reason: "invalid disk name".into(),
                    });
                    continue;
                }
                match fluxvm_qemu::disks::data_disks(&vm.workspace)
                    .into_iter()
                    .find(|(n, _)| *n == layer.name)
                {
                    Some((_, p)) if fluxvm_qemu::disks::is_external(&p) => {
                        skipped.push(SkippedDisk {
                            name: layer.name.clone(),
                            reason: "attached from an existing image".into(),
                        });
                        continue;
                    }
                    Some((_, p)) => {
                        let f = backup::data_disk_format(&p).to_string();
                        (p, f)
                    }
                    None => {
                        let dir = fluxvm_qemu::disks::disks_dir(&vm.workspace);
                        std::fs::create_dir_all(&dir)?;
                        (
                            dir.join(format!("{}.qcow2", layer.name)),
                            "qcow2".to_string(),
                        )
                    }
                }
            };
            plan.push((layer.name.clone(), dst));
        }
        job.update(|j| j.skipped = skipped);
        let mut tops = vec![];
        for (name, _) in &plan {
            tops.push(
                stack_disk(
                    &self.cfg.qemu_img_binary,
                    store.as_ref(),
                    &chain,
                    name,
                    work,
                    req.encryption.as_ref(),
                    job,
                )
                .await?,
            );
        }
        job.update(|j| j.phase = "writing".into());
        let vm = self.get(id).await?;
        if let Some(why) = backup::backup_refusal(
            vm.backend,
            vm.request.storage,
            vm.disk.is_file(),
            vm.status,
            true,
        ) {
            bail!(why);
        }
        for ((_, (dst, format)), src) in plan.iter().zip(&tops) {
            self.copy_backup_disk(src, dst, format).await?;
        }
        audit_event(
            "vm.cbt_restore",
            &[("vm_id", &id.to_string()), ("manifest", &req.manifest_key)],
        );
        job.update(|j| {
            j.manifest_key = Some(req.manifest_key.clone());
            j.manifest = Some(top);
        });
        Ok(())
    }

    /// Consolidates the chain ending at `req.manifest_key` into one
    /// standalone layer per disk, stored as a synthetic full at the same
    /// chain point. The VM's checkpoint for that point stays valid, so the
    /// next incremental can name the synthetic full as its parent.
    pub fn start_cbt_synthesize(self: &Arc<Self>, req: CbtSynthesizeRequest) -> Result<CbtJob> {
        fluxvm_backup_s3::validate_prefix(&req.prefix)?;
        fluxvm_backup_s3::validate_key_segment("backup id", &req.backup_id)?;
        layer_encryption(req.encryption.as_ref())?;
        req.target.open()?;
        self.spawn_cbt_job(
            CbtJobKind::Synthesize,
            None,
            None,
            move |me, job, work| async move { me.run_cbt_synthesize(req, &job, &work).await },
        )
    }

    async fn run_cbt_synthesize(
        &self,
        req: CbtSynthesizeRequest,
        job: &JobHandle,
        work: &Path,
    ) -> Result<()> {
        let store = req.target.open()?;
        let enc = layer_encryption(req.encryption.as_ref())?;
        job.update(|j| j.phase = "downloading".into());
        let chain = read_chain(store.as_ref(), &req.manifest_key, &req.synthetic_bases).await?;
        let (_, top) = chain.last().context("empty chain")?.clone();
        job.update(|j| j.bytes_total = chain.iter().map(|(_, m)| m.stored_bytes()).sum());
        let dir =
            fluxvm_backup_s3::backup_dir(&req.prefix, &top.chain_id, top.sequence, &req.backup_id);
        let opts = PutOptions {
            retain_until: req.retain_until,
        };
        let mut disks = vec![];
        let mut uploaded = vec![];
        let result: Result<Manifest> = async {
            for layer in &top.disks {
                let stacked = stack_disk(
                    &self.cfg.qemu_img_binary,
                    store.as_ref(),
                    &chain,
                    &layer.name,
                    work,
                    req.encryption.as_ref(),
                    job,
                )
                .await?;
                let flat = work.join(format!("{}.flat.qcow2", layer.name));
                qemu_img(
                    &self.cfg.qemu_img_binary,
                    &[
                        "convert".as_ref(),
                        "-f".as_ref(),
                        "qcow2".as_ref(),
                        "-O".as_ref(),
                        "qcow2".as_ref(),
                        "-c".as_ref(),
                        "-o".as_ref(),
                        "compression_type=zstd".as_ref(),
                        stacked.as_os_str(),
                        flat.as_os_str(),
                    ],
                )
                .await?;
                job.update(|j| j.phase = "uploading".into());
                let key = format!("{dir}/{}.qcow2", layer.name);
                let (stored_size, sha256) =
                    upload_layer(store.as_ref(), &key, &flat, enc.as_ref(), &opts).await?;
                uploaded.push(key.clone());
                disks.push(DiskLayer {
                    name: layer.name.clone(),
                    key,
                    virtual_size: layer.virtual_size,
                    stored_size,
                    changed_bytes: 0,
                    sha256,
                    encryption: enc.as_ref().map(|(_, e)| e.clone()),
                });
                for f in [stacked, flat] {
                    let _ = tokio::fs::remove_file(f).await;
                }
            }
            let manifest = Manifest {
                version: MANIFEST_VERSION,
                backup_id: req.backup_id.clone(),
                chain_id: top.chain_id.clone(),
                sequence: top.sequence,
                kind: ManifestKind::SyntheticFull,
                parent: None,
                vm_name: top.vm_name.clone(),
                created_at: Utc::now(),
                quiesced: top.quiesced,
                disks: disks.clone(),
                consolidates: chain.iter().map(|(_, m)| m.backup_id.clone()).collect(),
                retain_until: req.retain_until,
            };
            store
                .put_bytes(
                    &format!("{dir}/{MANIFEST_FILE}"),
                    manifest.to_bytes()?,
                    &opts,
                )
                .await?;
            Ok(manifest)
        }
        .await;
        let manifest = match result {
            Ok(m) => m,
            Err(e) => {
                for key in &uploaded {
                    let _ = store.delete(key).await;
                }
                return Err(e);
            }
        };
        let key = format!("{dir}/{MANIFEST_FILE}");
        audit_event("vm.cbt_synthesize", &[("manifest", &key)]);
        job.update(|j| {
            j.manifest_key = Some(key);
            j.manifest = Some(manifest);
        });
        Ok(())
    }

    /// Deletes the backups whose manifests are listed: their layers, then
    /// the manifest. Objects under Object Lock retention are reported, not
    /// forced; their manifest is kept so a later delete can finish.
    pub async fn cbt_delete(&self, req: CbtDeleteRequest) -> Result<CbtDeleteResult> {
        let store = req.target.open()?;
        let mut out = CbtDeleteResult::default();
        for mkey in &req.manifest_keys {
            if !mkey.ends_with(&format!("/{MANIFEST_FILE}")) {
                bail!("{mkey:?} is not a backup manifest key");
            }
            let manifest = match store.get_bytes(mkey).await {
                Ok(b) => Some(Manifest::from_bytes(&b)?),
                Err(_) => None,
            };
            let mut held = false;
            for layer in manifest.iter().flat_map(|m| &m.disks) {
                match store.delete(&layer.key).await? {
                    fluxvm_backup_s3::DeleteOutcome::Deleted => out.deleted.push(layer.key.clone()),
                    fluxvm_backup_s3::DeleteOutcome::NotFound => {
                        out.not_found.push(layer.key.clone())
                    }
                    fluxvm_backup_s3::DeleteOutcome::RetentionHeld => {
                        held = true;
                        out.retention_held.push(layer.key.clone());
                    }
                }
            }
            if held {
                out.retention_held.push(mkey.clone());
                continue;
            }
            match store.delete(mkey).await? {
                fluxvm_backup_s3::DeleteOutcome::Deleted => out.deleted.push(mkey.clone()),
                fluxvm_backup_s3::DeleteOutcome::NotFound => out.not_found.push(mkey.clone()),
                fluxvm_backup_s3::DeleteOutcome::RetentionHeld => {
                    out.retention_held.push(mkey.clone())
                }
            }
        }
        audit_event(
            "vm.cbt_delete",
            &[("manifests", &req.manifest_keys.len().to_string())],
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bm(node: &str, name: &str) -> Bitmap {
        Bitmap {
            node: node.into(),
            name: name.into(),
            persistent: true,
            inconsistent: false,
            busy: false,
            count: 0,
            granularity: 65536,
        }
    }

    fn nodes() -> Vec<String> {
        vec!["root".into(), "data-db".into()]
    }

    #[test]
    fn incremental_reads_the_parent_and_drops_leftover_checkpoints() {
        let maps = vec![
            bm("root", "kairon-cbt-c1-3"),
            bm("data-db", "kairon-cbt-c1-3"),
            bm("root", "kairon-cbt-c1-4"),
            bm("root", "kairon-cbt-old-9"),
            bm("root", "user-bitmap"),
        ];
        let plan = plan_bitmaps(&maps, &nodes(), "c1", 4, CbtMode::Incremental, Some(3)).unwrap();
        assert_eq!(
            plan.sync,
            BackupSync::Incremental {
                from: "kairon-cbt-c1-3".into()
            }
        );
        assert_eq!(
            plan.stale,
            vec![("root".to_string(), "kairon-cbt-c1-4".to_string())]
        );
    }

    #[test]
    fn a_missing_or_inconsistent_parent_requires_a_full() {
        let only_root = vec![bm("root", "kairon-cbt-c1-3")];
        let err =
            plan_bitmaps(&only_root, &nodes(), "c1", 4, CbtMode::Incremental, Some(3)).unwrap_err();
        assert!(err.downcast_ref::<FullRequired>().is_some(), "{err}");
        assert!(err.to_string().contains("data-db"));

        let mut dirty = vec![
            bm("root", "kairon-cbt-c1-3"),
            bm("data-db", "kairon-cbt-c1-3"),
        ];
        dirty[1].inconsistent = true;
        let err =
            plan_bitmaps(&dirty, &nodes(), "c1", 4, CbtMode::Incremental, Some(3)).unwrap_err();
        assert!(err.downcast_ref::<FullRequired>().is_some());

        dirty[1].inconsistent = false;
        dirty[1].busy = true;
        let err =
            plan_bitmaps(&dirty, &nodes(), "c1", 4, CbtMode::Incremental, Some(3)).unwrap_err();
        assert!(
            err.downcast_ref::<FullRequired>().is_none(),
            "busy is transient: {err}"
        );

        assert!(plan_bitmaps(&[], &nodes(), "c1", 3, CbtMode::Incremental, Some(3)).is_err());
        assert!(plan_bitmaps(&[], &nodes(), "c1", 3, CbtMode::Incremental, None).is_err());
    }

    #[test]
    fn a_full_keeps_older_chains_until_it_is_stored() {
        let maps = vec![bm("root", "kairon-cbt-c1-3"), bm("root", "kairon-cbt-c2-0")];
        let plan = plan_bitmaps(&maps, &nodes(), "c2", 0, CbtMode::Full, None).unwrap();
        assert_eq!(plan.sync, BackupSync::Full);
        assert_eq!(
            plan.stale,
            vec![("root".to_string(), "kairon-cbt-c2-0".to_string())]
        );

        let after = vec![
            bm("root", "kairon-cbt-c1-3"),
            bm("root", "kairon-cbt-c2-0"),
            bm("data-db", "kairon-cbt-c1-3"),
            bm("data-other", "kairon-cbt-c1-3"),
            bm("root", "user-bitmap"),
        ];
        assert_eq!(
            superseded(&after, &nodes(), "kairon-cbt-c2-0"),
            vec![
                ("root".to_string(), "kairon-cbt-c1-3".to_string()),
                ("data-db".to_string(), "kairon-cbt-c1-3".to_string()),
            ]
        );
    }

    fn manifest(id: &str, seq: u64, kind: ManifestKind, parent: Option<(&str, u64)>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            backup_id: id.into(),
            chain_id: "c1".into(),
            sequence: seq,
            kind,
            parent: parent.map(|(p, s)| ParentRef {
                backup_id: p.into(),
                sequence: s,
                manifest_key: format!("k/{p}"),
            }),
            vm_name: "vm".into(),
            created_at: Utc::now(),
            quiesced: false,
            disks: vec![],
            consolidates: vec![],
            retain_until: None,
        }
    }

    #[test]
    fn chains_are_checked_and_returned_base_first() {
        let top_down = vec![
            manifest("b2", 2, ManifestKind::Incremental, Some(("b1", 1))),
            manifest("b1", 1, ManifestKind::Incremental, Some(("b0", 0))),
            manifest("b0", 0, ManifestKind::Full, None),
        ];
        let ordered = order_chain(top_down.clone()).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|m| m.backup_id.as_str())
                .collect::<Vec<_>>(),
            ["b0", "b1", "b2"]
        );

        let mut wrong_parent = top_down.clone();
        wrong_parent[0].parent.as_mut().unwrap().backup_id = "bX".into();
        assert!(order_chain(wrong_parent).is_err());

        let mut other_chain = top_down.clone();
        other_chain[2].chain_id = "c9".into();
        assert!(order_chain(other_chain).is_err());

        let synthetic_base = vec![
            manifest("b3", 3, ManifestKind::Incremental, Some(("s2", 2))),
            manifest("s2", 2, ManifestKind::SyntheticFull, None),
        ];
        assert_eq!(order_chain(synthetic_base).unwrap().len(), 2);

        // b3 names b2; a synthetic full consolidating b2 may stand in.
        let mut stand_in = manifest("s2", 2, ManifestKind::SyntheticFull, None);
        stand_in.consolidates = vec!["b0".into(), "b1".into(), "b2".into()];
        let chain = vec![
            manifest("b3", 3, ManifestKind::Incremental, Some(("b2", 2))),
            stand_in.clone(),
        ];
        assert_eq!(order_chain(chain).unwrap()[0].backup_id, "s2");
        stand_in.consolidates = vec!["b1".into()];
        let chain = vec![
            manifest("b3", 3, ManifestKind::Incremental, Some(("b2", 2))),
            stand_in,
        ];
        assert!(order_chain(chain).is_err());
    }

    fn have(bin: &str) -> bool {
        std::process::Command::new(bin)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn sh(bin: &str, args: &[&str]) -> Vec<u8> {
        let out = std::process::Command::new(bin).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{bin} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// The guest-visible bytes of a disk.
    fn content(disk: &Path) -> String {
        let raw = disk.with_extension("check.raw");
        sh(
            "qemu-img",
            &[
                "convert",
                "-U",
                "-O",
                "raw",
                disk.to_str().unwrap(),
                raw.to_str().unwrap(),
            ],
        );
        let sum = crypto::sha256_file(&raw).unwrap();
        std::fs::remove_file(raw).unwrap();
        sum
    }

    async fn wait(m: &VmManager, job: CbtJob) -> CbtJob {
        for _ in 0..1200 {
            let j = m.cbt_job(job.id).unwrap().unwrap();
            if j.state != CbtJobState::Running {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("CBT job {} did not finish", job.id);
    }

    /// Full, 8 MiB incremental, restore, forced full after restore,
    /// synthetic full and delete, through `qemu-storage-daemon` on a stopped
    /// VM with a root and a data disk. Skipped without the QEMU tools.
    #[tokio::test]
    async fn stopped_vm_chain_round_trips_through_qemu() {
        if !["qemu-storage-daemon", "qemu-img", "qemu-io"]
            .iter()
            .all(|b| have(b))
        {
            eprintln!("skipping: QEMU tools not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = fluxvm_core::config::Config::default();
        cfg.state_dir = dir.path().join("state");
        cfg.run_dir = dir.path().join("run");
        let m = VmManager::new(cfg).unwrap();
        let ws = dir.path().join("ws");
        let disks = fluxvm_qemu::disks::disks_dir(&ws);
        std::fs::create_dir_all(&disks).unwrap();
        let root = ws.join("root.qcow2");
        let data = disks.join("logs.qcow2");
        for (disk, size) in [(&root, "64M"), (&data, "16M")] {
            sh(
                "qemu-img",
                &["create", "-q", "-f", "qcow2", disk.to_str().unwrap(), size],
            );
        }
        sh(
            "qemu-io",
            &["-c", "write -P 0x11 0 16M", root.to_str().unwrap()],
        );
        sh(
            "qemu-io",
            &["-c", "write -P 0x44 0 4M", data.to_str().unwrap()],
        );
        let id = Uuid::new_v4();
        m.store
            .insert(VmRecord {
                id,
                name: "cbt".into(),
                backend: BackendKind::Qemu,
                status: VmStatus::Stopped,
                pid: None,
                created_at: Utc::now(),
                expires_at: None,
                workspace: ws.clone(),
                disk: root.clone(),
                seed_disk: None,
                tap_name: None,
                control_socket: None,
                log_path: ws.join("console.log"),
                error: None,
                request: crate::tests::req(BackendKind::Qemu, None, None),
                guest_cid: None,
                jail_path: None,
                vsock_socket: None,
                qga_socket: None,
                cgroup_path: None,
                netns: None,
                lvm_lv: None,
                nbd_pid: None,
                virtiofsd_pids: vec![],
                swtpm_pid: None,
                dhcp_leasefile: None,
                guest_ip: None,
                requested_security_profile: Default::default(),
                achieved_security_profile: Default::default(),
                security_evidence: None,
                labels: Default::default(),
            })
            .await
            .unwrap();
        let target: Target = serde_json::from_value(
            serde_json::json!({"type": "dir", "path": dir.path().join("bucket")}),
        )
        .unwrap();
        use base64::Engine;
        let key = EncryptionKey {
            key_id: "k1".into(),
            key: base64::engine::general_purpose::STANDARD.encode([3u8; 32]),
        };
        let backup = |seq: u64, id_: &str, mode, parent: Option<ParentRef>| CbtBackupRequest {
            target: target.clone(),
            prefix: "ns/cbt".into(),
            chain_id: "c1".into(),
            sequence: seq,
            backup_id: id_.into(),
            mode,
            parent,
            quiesce: BackupQuiesce::Never,
            encryption: Some(key.clone()),
            retain_until: None,
        };
        let parent_of = |j: &CbtJob| {
            let mf = j.manifest.as_ref().unwrap();
            Some(ParentRef {
                backup_id: mf.backup_id.clone(),
                sequence: mf.sequence,
                manifest_key: j.manifest_key.clone().unwrap(),
            })
        };

        let full = wait(
            &m,
            m.start_cbt_backup(id, backup(0, "b0", CbtMode::Full, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(full.state, CbtJobState::Succeeded, "{:?}", full.error);
        assert_eq!(full.manifest.as_ref().unwrap().disks.len(), 2);

        sh(
            "qemu-io",
            &["-c", "write -P 0x22 32M 8M", root.to_str().unwrap()],
        );
        let incr = wait(
            &m,
            m.start_cbt_backup(id, backup(1, "b1", CbtMode::Incremental, parent_of(&full)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(incr.state, CbtJobState::Succeeded, "{:?}", incr.error);
        let mf = incr.manifest.clone().unwrap();
        let changed = mf.disk("root").unwrap().changed_bytes;
        assert!(
            (8 << 20..=9 << 20).contains(&changed),
            "root changed {changed}"
        );
        assert_eq!(mf.disk("logs").unwrap().changed_bytes, 0);

        let syn = wait(
            &m,
            m.start_cbt_synthesize(CbtSynthesizeRequest {
                target: target.clone(),
                prefix: "ns/cbt".into(),
                manifest_key: incr.manifest_key.clone().unwrap(),
                backup_id: "s1".into(),
                encryption: Some(key.clone()),
                retain_until: None,
                synthetic_bases: vec![],
            })
            .unwrap(),
        )
        .await;
        assert_eq!(syn.state, CbtJobState::Succeeded, "{:?}", syn.error);
        let smf = syn.manifest.clone().unwrap();
        assert_eq!((smf.kind, smf.sequence), (ManifestKind::SyntheticFull, 1));
        assert_eq!(smf.consolidates, ["b0", "b1"]);

        // The checkpoint survives synthesis: b2 still parents on b1.
        sh(
            "qemu-io",
            &["-c", "write -P 0x77 40M 4M", root.to_str().unwrap()],
        );
        sh(
            "qemu-io",
            &["-c", "write -P 0x88 8M 1M", data.to_str().unwrap()],
        );
        let incr2 = wait(
            &m,
            m.start_cbt_backup(id, backup(2, "b2", CbtMode::Incremental, parent_of(&incr)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(incr2.state, CbtJobState::Succeeded, "{:?}", incr2.error);
        let want = (content(&root), content(&data));

        let deleted = m
            .cbt_delete(CbtDeleteRequest {
                target: target.clone(),
                manifest_keys: vec![
                    full.manifest_key.clone().unwrap(),
                    incr.manifest_key.clone().unwrap(),
                ],
            })
            .await
            .unwrap();
        assert_eq!(deleted.deleted.len(), 6, "{deleted:?}");

        sh(
            "qemu-io",
            &["-c", "write -P 0x33 0 48M", root.to_str().unwrap()],
        );
        sh(
            "qemu-io",
            &["-c", "write -P 0x55 0 12M", data.to_str().unwrap()],
        );
        let restore = |manifest_key: String, bases: Vec<String>| CbtRestoreRequest {
            target: target.clone(),
            manifest_key,
            encryption: Some(key.clone()),
            synthetic_bases: bases,
        };
        let b2 = incr2.manifest_key.clone().unwrap();
        let broken = wait(
            &m,
            m.start_cbt_restore(id, restore(b2.clone(), vec![]))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            broken.state,
            CbtJobState::Failed,
            "b1 is gone without the synthetic full"
        );
        let r = wait(
            &m,
            m.start_cbt_restore(id, restore(b2, vec![syn.manifest_key.clone().unwrap()]))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(r.state, CbtJobState::Succeeded, "{:?}", r.error);
        assert_eq!((content(&root), content(&data)), want);

        // The restored disks carry no checkpoint.
        let after = wait(
            &m,
            m.start_cbt_backup(id, backup(3, "b3", CbtMode::Incremental, parent_of(&incr2)))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(after.state, CbtJobState::Failed);
        assert!(after.full_required, "{:?}", after.error);
    }

    #[test]
    fn job_files_written_by_a_dead_process_read_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let live = JobHandle::create(dir.path(), CbtJobKind::Backup, None).unwrap();
        let id = live.snapshot().id;
        assert_eq!(
            load_job(dir.path(), id).unwrap().unwrap().state,
            CbtJobState::Running
        );

        live.finish(Err(FullRequired("bitmap gone".into()).into()));
        let done = load_job(dir.path(), id).unwrap().unwrap();
        assert_eq!(done.state, CbtJobState::Failed);
        assert!(done.full_required);

        let orphan = JobHandle::create(dir.path(), CbtJobKind::Restore, None).unwrap();
        let oid = orphan.snapshot().id;
        live_jobs().lock().unwrap().remove(&oid);
        let job = load_job(dir.path(), oid).unwrap().unwrap();
        assert_eq!(job.state, CbtJobState::Failed);
        assert!(!job.full_required);
        assert!(job.error.unwrap().contains("restart"));
        assert!(load_job(dir.path(), Uuid::new_v4()).unwrap().is_none());
    }
}
