// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Verify and test-restore of object-storage backup chains, one-off and as scheduled jobs.
//!
//! The checks live in `fluxvm_backup_s3::verify`; this module adds what a daemon needs around them:
//! stored *verify jobs*, modelled on Proxmox Backup Server verify jobs, and an event per result.
//!
//! A job names a target, the chain points (manifest keys) to check, how often, and whether to also
//! restore (`test-restore`). `ignore_verified` skips a chain point that already verified clean within
//! `outdated_after_secs` (PBS `ignore-verified` / `outdated-after`), so a frequent schedule re-reads
//! only what is new or stale. Results are kept in `<state_dir>/backup-verify-state.json`.
//!
//! Jobs live in `<state_dir>/backup-verify-jobs.json`, like `vm-templates.json`. Backup credentials
//! and encryption keys are never written there: a job's S3 target and key name *environment
//! variables* of the daemon that hold them.
//!
//! Every run records a `backup.verify` or `backup.test_restore` event (`result=ok|failed`, with the
//! job, manifest, issue count and a one-line summary); failures are also logged at warn level.

use crate::audit_event;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use fluxvm_backup_s3::verify::{
    ChainReport, DecryptKey, TestRestoreOptions, VerifyOptions, test_restore_chain, verify_chain,
};
use fluxvm_backup_s3::{ObjectStore, Target, crypto, dir::DirStore, s3};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Shortest allowed schedule interval.
pub const MIN_INTERVAL_SECS: u64 = 60;
/// How often the daemon loop looks for due jobs.
const TICK: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobMode {
    /// Re-read and check objects; restore nothing.
    #[default]
    Verify,
    /// Also restore into a scratch directory and check the image.
    TestRestore,
}

/// Where a job's backups are. Secrets are named, not stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum JobTarget {
    Dir {
        path: PathBuf,
    },
    #[serde(rename_all = "camelCase")]
    S3 {
        endpoint: String,
        bucket: String,
        #[serde(default = "default_region")]
        region: String,
        #[serde(default = "default_true")]
        path_style: bool,
        /// Environment variable holding the access key id.
        access_key_id_env: String,
        /// Environment variable holding the secret access key.
        secret_access_key_env: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_token_env: Option<String>,
    },
}

fn default_region() -> String {
    "us-east-1".into()
}

fn default_true() -> bool {
    true
}

fn env_secret(var: &str) -> Result<String> {
    std::env::var(var).with_context(|| format!("environment variable {var} is not set"))
}

impl JobTarget {
    /// The one-off `Target` form, with secrets read from the environment.
    pub fn resolve(&self) -> Result<Target> {
        Ok(match self {
            JobTarget::Dir { path } => Target::Dir { path: path.clone() },
            JobTarget::S3 {
                endpoint,
                bucket,
                region,
                path_style,
                access_key_id_env,
                secret_access_key_env,
                session_token_env,
            } => Target::S3(s3::S3Target {
                endpoint: endpoint.clone(),
                bucket: bucket.clone(),
                region: region.clone(),
                access_key_id: env_secret(access_key_id_env)?,
                secret_access_key: env_secret(secret_access_key_env)?,
                session_token: session_token_env.as_deref().map(env_secret).transpose()?,
                path_style: *path_style,
            }),
        })
    }
}

/// A decryption key by name; the key itself comes from the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobKey {
    pub key_id: String,
    /// Environment variable holding the base64 of the 32-byte key.
    pub key_env: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyJob {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub target: JobTarget,
    /// Chain points to check: the manifest key of the newest backup of each chain.
    pub manifest_keys: Vec<String>,
    #[serde(default)]
    pub mode: JobMode,
    pub interval_secs: u64,
    /// Skip a chain point that verified clean less than `outdated_after_secs` ago.
    #[serde(default)]
    pub ignore_verified: bool,
    #[serde(default)]
    pub outdated_after_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<JobKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synthetic_bases: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

impl VerifyJob {
    pub fn validate(&self) -> Result<()> {
        validate_job_name(&self.name)?;
        if self.manifest_keys.is_empty() {
            bail!("a verify job needs at least one manifest key");
        }
        for k in self.manifest_keys.iter().chain(&self.synthetic_bases) {
            if !k.ends_with("/manifest.json") {
                bail!("{k:?} is not a manifest key (…/manifest.json)");
            }
            fluxvm_backup_s3::validate_prefix(k.trim_end_matches("/manifest.json"))?;
        }
        if self.interval_secs < MIN_INTERVAL_SECS {
            bail!("interval_secs must be at least {MIN_INTERVAL_SECS}");
        }
        if self.ignore_verified && self.outdated_after_secs == 0 {
            bail!("ignore_verified needs outdated_after_secs > 0");
        }
        if let JobTarget::S3 {
            access_key_id_env,
            secret_access_key_env,
            ..
        } = &self.target
            && (access_key_id_env.is_empty() || secret_access_key_env.is_empty())
        {
            bail!("an S3 target names the environment variables that hold its credentials");
        }
        Ok(())
    }
}

pub fn validate_job_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 63
        || name.starts_with(['-', '.'])
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid verify job name {name:?}: use 1-63 chars of [A-Za-z0-9._-]");
    }
    Ok(())
}

// ---- storage ---------------------------------------------------------------------------------------

pub fn jobs_path(state_dir: &Path) -> PathBuf {
    state_dir.join("backup-verify-jobs.json")
}

pub fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("backup-verify-state.json")
}

fn work_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("backup-verify-work")
}

fn read<T: Default + for<'de> Deserialize<'de>>(p: &Path) -> Result<T> {
    match std::fs::read_to_string(p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

fn write<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, p).with_context(|| format!("replacing {}", p.display()))
}

pub fn load_jobs(state_dir: &Path) -> Result<BTreeMap<String, VerifyJob>> {
    read(&jobs_path(state_dir))
}

pub fn save_job(state_dir: &Path, job: VerifyJob, replace: bool) -> Result<VerifyJob> {
    job.validate()?;
    let mut all = load_jobs(state_dir)?;
    if !replace && all.contains_key(&job.name) {
        bail!("verify job {:?} already exists", job.name);
    }
    all.insert(job.name.clone(), job.clone());
    write(&jobs_path(state_dir), &all)?;
    Ok(job)
}

pub fn delete_job(state_dir: &Path, name: &str) -> Result<()> {
    let mut all = load_jobs(state_dir)?;
    if all.remove(name).is_none() {
        bail!("verify job {name:?} not found");
    }
    write(&jobs_path(state_dir), &all)?;
    let mut st = load_state(state_dir)?;
    if st.remove(name).is_some() {
        write(&state_path(state_dir), &st)?;
    }
    Ok(())
}

/// The last result for one chain point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainState {
    pub at: DateTime<Utc>,
    pub ok: bool,
    pub issues: usize,
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<DateTime<Utc>>,
    #[serde(default)]
    pub chains: BTreeMap<String, ChainState>,
}

pub fn load_state(state_dir: &Path) -> Result<BTreeMap<String, JobState>> {
    read(&state_path(state_dir))
}

// ---- running ---------------------------------------------------------------------------------------

/// What to check and how, independent of any stored job.
pub struct CheckRequest<'a> {
    pub store: &'a dyn ObjectStore,
    pub manifest_key: &'a str,
    pub mode: JobMode,
    pub synthetic_bases: Vec<String>,
    pub key: Option<DecryptKey>,
    pub scratch: &'a Path,
    pub qemu_img: &'a str,
}

/// Runs one check and records its event. `job` labels the event (`-` for a one-off).
pub async fn check_chain(req: CheckRequest<'_>, job: &str) -> Result<ChainReport> {
    std::fs::create_dir_all(req.scratch)
        .with_context(|| format!("creating {}", req.scratch.display()))?;
    let verify = VerifyOptions {
        synthetic_bases: req.synthetic_bases,
        key: req.key,
        scratch: req.scratch.to_path_buf(),
    };
    let (event, report) = match req.mode {
        JobMode::Verify => (
            "backup.verify",
            verify_chain(req.store, req.manifest_key, &verify).await?,
        ),
        JobMode::TestRestore => (
            "backup.test_restore",
            test_restore_chain(
                req.store,
                req.manifest_key,
                &TestRestoreOptions {
                    verify,
                    qemu_img: Some(req.qemu_img.to_string()),
                },
            )
            .await?,
        ),
    };
    record_event(event, job, &report);
    Ok(report)
}

/// The event for one result: `result=ok|failed`, plus job, manifest, counts and the summary line.
pub fn record_event(event: &str, job: &str, r: &ChainReport) {
    let issues = r.issues.len().to_string();
    let layers = r.layers_checked.to_string();
    let result = if r.ok { "ok" } else { "failed" };
    let summary = r.summary();
    audit_event(
        event,
        &[
            ("job", job),
            ("manifest", &r.manifest_key),
            ("result", result),
            ("issues", &issues),
            ("layers", &layers),
            ("summary", &summary),
        ],
    );
    if !r.ok {
        tracing::warn!(job, manifest = %r.manifest_key, "{summary}");
    }
}

fn job_key(job: &VerifyJob) -> Result<Option<DecryptKey>> {
    job.encryption
        .as_ref()
        .map(|k| {
            Ok(DecryptKey {
                key_id: k.key_id.clone(),
                key: crypto::parse_key(&env_secret(&k.key_env)?)?,
            })
        })
        .transpose()
}

/// Whether `job` is due at `now`: enabled, and not run for `interval_secs`.
pub fn is_due(job: &VerifyJob, state: Option<&JobState>, now: DateTime<Utc>) -> bool {
    job.enabled
        && state.and_then(|s| s.last_run).is_none_or(|t| {
            (now - t).num_seconds() >= i64::try_from(job.interval_secs).unwrap_or(i64::MAX)
        })
}

/// Whether a chain point can be skipped under `ignore_verified`.
pub fn skip_as_verified(job: &VerifyJob, st: Option<&ChainState>, now: DateTime<Utc>) -> bool {
    job.ignore_verified
        && st.is_some_and(|c| {
            c.ok && (now - c.at).num_seconds()
                < i64::try_from(job.outdated_after_secs).unwrap_or(i64::MAX)
        })
}

/// The outcome of one job run.
#[derive(Debug, Serialize)]
pub struct JobRun {
    pub job: String,
    pub ok: bool,
    pub skipped: Vec<String>,
    pub reports: Vec<ChainReport>,
}

/// Runs `job` now: every chain point not skipped by `ignore_verified` (unless `force`), updating the
/// stored state. A chain point that cannot be opened at all counts as a failed result, not an error.
pub async fn run_job(
    state_dir: &Path,
    qemu_img: &str,
    job: &VerifyJob,
    force: bool,
) -> Result<JobRun> {
    let now = Utc::now();
    let mut st = load_state(state_dir)?;
    let js = st.entry(job.name.clone()).or_default().clone();
    let setup = job
        .target
        .resolve()
        .and_then(|t| t.open())
        .and_then(|s| Ok((s, job_key(job)?)));
    let scratch = work_dir(state_dir);
    let mut run = JobRun {
        job: job.name.clone(),
        ok: true,
        skipped: vec![],
        reports: vec![],
    };
    let mut results: Vec<(String, ChainState)> = vec![];
    for key in &job.manifest_keys {
        if !force && skip_as_verified(job, js.chains.get(key), now) {
            run.skipped.push(key.clone());
            continue;
        }
        let state = match &setup {
            Ok((store, dkey)) => {
                let report = check_chain(
                    CheckRequest {
                        store: store.as_ref(),
                        manifest_key: key,
                        mode: job.mode,
                        synthetic_bases: job.synthetic_bases.clone(),
                        key: dkey.clone(),
                        scratch: &scratch,
                        qemu_img,
                    },
                    &job.name,
                )
                .await;
                match report {
                    Ok(r) => {
                        let s = ChainState {
                            at: Utc::now(),
                            ok: r.ok,
                            issues: r.issues.len(),
                            summary: r.summary(),
                        };
                        run.reports.push(r);
                        s
                    }
                    Err(e) => failed_to_run(&job.name, key, job.mode, &e),
                }
            }
            Err(e) => failed_to_run(&job.name, key, job.mode, e),
        };
        run.ok &= state.ok;
        results.push((key.clone(), state));
    }
    // Re-read under no lock but write only this job's entry, so concurrent jobs do not clobber each other.
    let mut st = load_state(state_dir)?;
    let e = st.entry(job.name.clone()).or_default();
    e.last_run = Some(Utc::now());
    e.chains.extend(results);
    write(&state_path(state_dir), &st)?;
    Ok(run)
}

fn failed_to_run(job: &str, key: &str, mode: JobMode, e: &anyhow::Error) -> ChainState {
    let summary = format!("could not run: {e:#}");
    let event = match mode {
        JobMode::Verify => "backup.verify",
        JobMode::TestRestore => "backup.test_restore",
    };
    audit_event(
        event,
        &[
            ("job", job),
            ("manifest", key),
            ("result", "failed"),
            ("summary", &summary),
        ],
    );
    tracing::warn!(job, manifest = key, "{summary}");
    ChainState {
        at: Utc::now(),
        ok: false,
        issues: 1,
        summary,
    }
}

/// Runs every due job, one at a time. Returns the names run.
pub async fn run_due(state_dir: &Path, qemu_img: &str) -> Vec<String> {
    let jobs = match load_jobs(state_dir) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("backup verify jobs unreadable: {e:#}");
            return vec![];
        }
    };
    let state = load_state(state_dir).unwrap_or_default();
    let now = Utc::now();
    let mut ran = vec![];
    for job in jobs.values() {
        if !is_due(job, state.get(&job.name), now) {
            continue;
        }
        if let Err(e) = run_job(state_dir, qemu_img, job, false).await {
            tracing::warn!(job = %job.name, "backup verify job failed to run: {e:#}");
        }
        ran.push(job.name.clone());
    }
    ran
}

/// The daemon's scheduler loop: checks for due verify jobs every 30 seconds.
pub fn spawn(state_dir: PathBuf, qemu_img: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            run_due(&state_dir, &qemu_img).await;
        }
    })
}

/// Opens a `Target` JSON file (the same shape as the REST `target`), for one-off CLI runs.
pub fn target_from_file(path: &Path) -> Result<Target> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// A directory store, for `--dir`.
pub fn dir_store(path: &Path) -> Box<dyn ObjectStore> {
    Box::new(DirStore::new(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_backup_s3::manifest::{DiskLayer, MANIFEST_VERSION, Manifest, ManifestKind};
    use fluxvm_backup_s3::{PutOptions, backup_dir};
    use sha2::{Digest, Sha256};

    fn mkey(seq: u64, id: &str) -> String {
        format!("{}/manifest.json", backup_dir("ns/vm", "c1", seq, id))
    }

    async fn full_backup(root: &Path) -> String {
        let store = DirStore::new(root.to_path_buf());
        let mut layer = vec![0u8; 512];
        layer[..4].copy_from_slice(b"QFI\xfb");
        layer[4..8].copy_from_slice(&3u32.to_be_bytes());
        layer[24..32].copy_from_slice(&(1u64 << 20).to_be_bytes());
        let dir = backup_dir("ns/vm", "c1", 1, "b1");
        let key = format!("{dir}/root.qcow2");
        let m = Manifest {
            version: MANIFEST_VERSION,
            backup_id: "b1".into(),
            chain_id: "c1".into(),
            sequence: 1,
            kind: ManifestKind::Full,
            parent: None,
            vm_name: "vm".into(),
            created_at: Utc::now(),
            quiesced: false,
            disks: vec![DiskLayer {
                name: "root".into(),
                key: key.clone(),
                virtual_size: 1 << 20,
                stored_size: 512,
                changed_bytes: 512,
                sha256: hex::encode(Sha256::digest(&layer)),
                encryption: None,
            }],
            consolidates: vec![],
            retain_until: None,
        };
        let o = PutOptions::default();
        store.put_bytes(&key, layer, &o).await.unwrap();
        store
            .put_bytes(&mkey(1, "b1"), m.to_bytes().unwrap(), &o)
            .await
            .unwrap();
        mkey(1, "b1")
    }

    fn job(root: &Path, key: &str) -> VerifyJob {
        VerifyJob {
            name: "nightly".into(),
            description: None,
            target: JobTarget::Dir {
                path: root.to_path_buf(),
            },
            manifest_keys: vec![key.into()],
            mode: JobMode::Verify,
            interval_secs: 3600,
            ignore_verified: true,
            outdated_after_secs: 86400,
            encryption: None,
            synthetic_bases: vec![],
            enabled: true,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn jobs_are_validated() {
        let j = job(Path::new("/x"), "ns/vm/c1/00000001-b1/manifest.json");
        j.validate().unwrap();
        let mut bad = j.clone();
        bad.name = "a b".into();
        assert!(bad.validate().is_err());
        let mut bad = j.clone();
        bad.manifest_keys = vec![];
        assert!(bad.validate().is_err());
        let mut bad = j.clone();
        bad.manifest_keys = vec!["ns/../x/manifest.json".into()];
        assert!(bad.validate().is_err());
        let mut bad = j.clone();
        bad.manifest_keys = vec!["ns/vm/root.qcow2".into()];
        assert!(bad.validate().is_err());
        let mut bad = j.clone();
        bad.interval_secs = 5;
        assert!(bad.validate().is_err());
        let mut bad = j;
        bad.outdated_after_secs = 0;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn job_files_hold_no_secrets() {
        let state = tempfile::tempdir().unwrap();
        let mut j = job(Path::new("/x"), "ns/vm/c1/00000001-b1/manifest.json");
        j.target = JobTarget::S3 {
            endpoint: "http://minio:9000".into(),
            bucket: "b".into(),
            region: "us-east-1".into(),
            path_style: true,
            access_key_id_env: "FX_TEST_AK".into(),
            secret_access_key_env: "FX_TEST_SK".into(),
            session_token_env: None,
        };
        j.encryption = Some(JobKey {
            key_id: "k1".into(),
            key_env: "FX_TEST_KEY".into(),
        });
        save_job(state.path(), j.clone(), false).unwrap();
        assert!(save_job(state.path(), j.clone(), false).is_err());
        let text = std::fs::read_to_string(jobs_path(state.path())).unwrap();
        assert!(
            text.contains("FX_TEST_SK") && !text.contains("secretAccessKey\""),
            "{text}"
        );
        assert_eq!(load_jobs(state.path()).unwrap()["nightly"], j);
        // Credentials come from the environment, and a missing variable is a clear error.
        let e = j.target.resolve().unwrap_err().to_string();
        assert!(e.contains("FX_TEST_AK"), "{e}");
        delete_job(state.path(), "nightly").unwrap();
        assert!(delete_job(state.path(), "nightly").is_err());
    }

    #[test]
    fn due_and_outdated_follow_pbs_semantics() {
        let j = job(Path::new("/x"), "k/manifest.json");
        let now = Utc::now();
        assert!(is_due(&j, None, now));
        let st = JobState {
            last_run: Some(now - chrono::Duration::seconds(100)),
            chains: BTreeMap::new(),
        };
        assert!(!is_due(&j, Some(&st), now));
        assert!(is_due(&j, Some(&st), now + chrono::Duration::seconds(3600)));
        let mut off = j.clone();
        off.enabled = false;
        assert!(!is_due(&off, None, now));

        let ok = |ago: i64, ok: bool| ChainState {
            at: now - chrono::Duration::seconds(ago),
            ok,
            issues: 0,
            summary: String::new(),
        };
        assert!(skip_as_verified(&j, Some(&ok(100, true)), now));
        assert!(!skip_as_verified(&j, Some(&ok(100_000, true)), now));
        assert!(!skip_as_verified(&j, Some(&ok(100, false)), now));
        assert!(!skip_as_verified(&j, None, now));
    }

    #[tokio::test]
    async fn a_run_records_state_and_ignore_verified_skips_clean_chains() {
        let (root, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let key = full_backup(root.path()).await;
        let j = job(root.path(), &key);
        save_job(state.path(), j.clone(), false).unwrap();

        let run = run_job(state.path(), "qemu-img", &j, false).await.unwrap();
        assert!(run.ok && run.reports.len() == 1 && run.skipped.is_empty());
        let st = load_state(state.path()).unwrap();
        assert!(st["nightly"].chains[&key].ok && st["nightly"].last_run.is_some());

        // Verified clean moments ago: skipped, unless forced.
        let run = run_job(state.path(), "qemu-img", &j, false).await.unwrap();
        assert_eq!((run.reports.len(), run.skipped.len()), (0, 1));
        let run = run_job(state.path(), "qemu-img", &j, true).await.unwrap();
        assert_eq!(run.reports.len(), 1);

        // Corrupt the layer: the next forced run fails and the state says so.
        std::fs::write(
            root.path()
                .join(backup_dir("ns/vm", "c1", 1, "b1"))
                .join("root.qcow2"),
            vec![9u8; 512],
        )
        .unwrap();
        let run = run_job(state.path(), "qemu-img", &j, true).await.unwrap();
        assert!(!run.ok);
        let st = load_state(state.path()).unwrap();
        assert!(!st["nightly"].chains[&key].ok);
        assert!(st["nightly"].chains[&key].summary.contains("FAILED"));
        // A failed chain is never skipped.
        let run = run_job(state.path(), "qemu-img", &j, false).await.unwrap();
        assert_eq!(run.reports.len(), 1);
    }

    #[tokio::test]
    async fn due_jobs_run_and_unopenable_targets_fail_cleanly() {
        let (root, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let key = full_backup(root.path()).await;
        save_job(state.path(), job(root.path(), &key), false).unwrap();
        assert_eq!(run_due(state.path(), "qemu-img").await, ["nightly"]);
        // Not due again for an hour.
        assert!(run_due(state.path(), "qemu-img").await.is_empty());

        // A job whose S3 credentials are not in the environment is a failed result, not a crash.
        let mut j = job(root.path(), &key);
        j.name = "remote".into();
        j.target = JobTarget::S3 {
            endpoint: "http://127.0.0.1:9".into(),
            bucket: "b".into(),
            region: "us-east-1".into(),
            path_style: true,
            access_key_id_env: "FX_DEFINITELY_UNSET_AK".into(),
            secret_access_key_env: "FX_DEFINITELY_UNSET_SK".into(),
            session_token_env: None,
        };
        let run = run_job(state.path(), "qemu-img", &j, true).await.unwrap();
        assert!(!run.ok && run.reports.is_empty());
        let st = load_state(state.path()).unwrap();
        assert!(
            st["remote"].chains[&key]
                .summary
                .contains("FX_DEFINITELY_UNSET_AK")
        );
    }

    #[tokio::test]
    async fn test_restore_mode_runs_through_a_job() {
        let (root, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let key = full_backup(root.path()).await;
        let mut j = job(root.path(), &key);
        j.mode = JobMode::TestRestore;
        // Header checks when qemu-img is unavailable.
        let run = run_job(state.path(), "/nonexistent/qemu-img", &j, true)
            .await
            .unwrap();
        assert!(run.ok, "{:?}", run.reports[0].issues);
        assert_eq!(
            run.reports[0].kind,
            fluxvm_backup_s3::verify::ReportKind::TestRestore
        );
    }
}
