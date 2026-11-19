// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl backup-verify verify|test-restore|job ...`: check object-storage backup chains.
//!
//! These run in this process against the object store (they need no daemon): `verify` re-reads a
//! chain and checks digests, manifests and completeness; `test-restore` also restores it into a
//! scratch directory and checks the image; `job` manages the scheduled verify jobs the daemon runs
//! (`<state_dir>/backup-verify-jobs.json`). See docs/backup-verify.md.
//!
//! Each report is printed as JSON on stdout and a one-line summary goes to stderr. Exit status: 0 when
//! every chain checked clean, 3 when any check found a problem, 1 when a check could not run at all.

use crate::output::OutputFormat;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Args, Subcommand, ValueEnum};
use fluxvm_backup_s3::verify::DecryptKey;
use fluxvm_scheduler::backup_verify::{
    self as bv, CheckRequest, JobKey, JobMode, JobTarget, VerifyJob,
};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Exit status when a check ran and found a problem.
pub const EXIT_VERIFY_FAILED: i32 = 3;

#[derive(Subcommand)]
pub enum BackupVerifyCommand {
    /// Re-read a backup chain and check digests, manifests and chain completeness, without restoring.
    Verify(CheckArgs),
    /// Restore a chain into a scratch directory and check the image (`qemu-img check`, or qcow2 header
    /// and size checks when `qemu-img` is not available).
    TestRestore(CheckArgs),
    /// Scheduled verify jobs, run by the daemon.
    Job {
        #[command(subcommand)]
        command: JobCommand,
    },
}

#[derive(Args)]
pub struct CheckArgs {
    /// Backups in this directory (a local or mounted object store).
    #[arg(long, conflicts_with = "target", required_unless_present = "target")]
    dir: Option<PathBuf>,
    /// A JSON file with the backup target, the same shape as the REST `target`
    /// (`{"type":"s3","endpoint":...}` or `{"type":"dir","path":...}`).
    #[arg(long)]
    target: Option<PathBuf>,
    /// Manifest key of the newest backup to check, e.g. `ns/db/c1/00000003-b3/manifest.json` (repeatable).
    #[arg(long = "manifest", required = true)]
    manifests: Vec<String>,
    /// Synthetic full of the chain, by manifest key (repeatable).
    #[arg(long = "synthetic-base")]
    synthetic_bases: Vec<String>,
    /// Name of the encryption key the layers were written with.
    #[arg(long, requires = "key_file")]
    key_id: Option<String>,
    /// File holding the base64 of the 32-byte key (never pass keys on the command line).
    #[arg(long, requires = "key_id")]
    key_file: Option<PathBuf>,
    /// Scratch directory for downloads; default `<state_dir>/backup-verify-work`.
    #[arg(long)]
    scratch: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ModeArg {
    Verify,
    TestRestore,
}

#[derive(Subcommand)]
pub enum JobCommand {
    /// Add (or with `--replace`, change) a scheduled verify job.
    Add(JobAddArgs),
    /// List jobs with the last result of each chain point.
    #[command(visible_alias = "list")]
    Ls,
    /// Delete a job and its recorded results.
    #[command(visible_aliases = ["delete", "remove"])]
    Rm { name: String },
    /// Run a job now. Chain points verified clean within the job's outdated-after window are skipped
    /// unless `--force`.
    Run {
        name: String,
        #[arg(long)]
        force: bool,
    },
}

#[derive(Args)]
pub struct JobAddArgs {
    name: String,
    /// Backups in this directory.
    #[arg(
        long,
        conflicts_with = "s3_endpoint",
        required_unless_present = "s3_endpoint"
    )]
    dir: Option<PathBuf>,
    #[arg(long, requires_all = ["s3_bucket", "access_key_id_env", "secret_access_key_env"])]
    s3_endpoint: Option<String>,
    #[arg(long)]
    s3_bucket: Option<String>,
    #[arg(long, default_value = "us-east-1")]
    s3_region: String,
    /// Virtual-hosted addressing (`bucket.endpoint`) instead of path style.
    #[arg(long)]
    s3_virtual_hosted: bool,
    /// Name of the daemon environment variable holding the S3 access key id. Credentials are never
    /// stored in the job.
    #[arg(long)]
    access_key_id_env: Option<String>,
    #[arg(long)]
    secret_access_key_env: Option<String>,
    #[arg(long)]
    session_token_env: Option<String>,
    /// Manifest key of the newest backup of a chain to check (repeatable).
    #[arg(long = "manifest", required = true)]
    manifests: Vec<String>,
    #[arg(long = "synthetic-base")]
    synthetic_bases: Vec<String>,
    #[arg(long, value_enum, default_value = "verify")]
    mode: ModeArg,
    /// How often, e.g. `6h`, `1d`, `30m`, or seconds. At least 60s.
    #[arg(long, value_parser = parse_duration)]
    every: u64,
    /// Skip a chain point that already verified clean within `--outdated-after`.
    #[arg(long)]
    ignore_verified: bool,
    /// With `--ignore-verified`: how long a clean result counts, e.g. `7d`.
    #[arg(long, value_parser = parse_duration, default_value = "7d")]
    outdated_after: u64,
    #[arg(long, requires = "key_env")]
    key_id: Option<String>,
    /// Name of the daemon environment variable holding the base64 encryption key.
    #[arg(long, requires = "key_id")]
    key_env: Option<String>,
    #[arg(long)]
    description: Option<String>,
    /// Create the job disabled.
    #[arg(long)]
    disabled: bool,
    #[arg(long)]
    replace: bool,
}

/// `30m`, `6h`, `1d`, `90s` or plain seconds.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{s:?} is not a duration: use 30m, 6h, 1d or seconds"))
}

fn read_key(a: &CheckArgs) -> Result<Option<DecryptKey>> {
    let (Some(id), Some(file)) = (&a.key_id, &a.key_file) else {
        return Ok(None);
    };
    let b64 =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    Ok(Some(DecryptKey {
        key_id: id.clone(),
        key: fluxvm_backup_s3::crypto::parse_key(b64.trim())?,
    }))
}

async fn check(a: CheckArgs, mode: JobMode, state_dir: &Path, qemu_img: &str) -> Result<i32> {
    let store = match (&a.dir, &a.target) {
        (Some(d), _) => bv::dir_store(d),
        (None, Some(t)) => bv::target_from_file(t)?.open()?,
        (None, None) => bail!("give --dir or --target"),
    };
    let key = read_key(&a)?;
    let scratch = a
        .scratch
        .clone()
        .unwrap_or_else(|| state_dir.join("backup-verify-work"));
    let mut failed = false;
    let mut reports = vec![];
    for m in &a.manifests {
        let r = bv::check_chain(
            CheckRequest {
                store: store.as_ref(),
                manifest_key: m,
                mode,
                synthetic_bases: a.synthetic_bases.clone(),
                key: key.clone(),
                scratch: &scratch,
                qemu_img,
            },
            "-",
        )
        .await?;
        eprintln!("{m}: {}", r.summary());
        failed |= !r.ok;
        reports.push(r);
    }
    println!("{}", serde_json::to_string_pretty(&reports)?);
    Ok(if failed { EXIT_VERIFY_FAILED } else { 0 })
}

fn job_from_args(a: JobAddArgs) -> Result<VerifyJob> {
    let target = match (a.dir, a.s3_endpoint) {
        (Some(path), _) => JobTarget::Dir { path },
        (None, Some(endpoint)) => JobTarget::S3 {
            endpoint,
            bucket: a.s3_bucket.context("--s3-bucket")?,
            region: a.s3_region,
            path_style: !a.s3_virtual_hosted,
            access_key_id_env: a.access_key_id_env.context("--access-key-id-env")?,
            secret_access_key_env: a.secret_access_key_env.context("--secret-access-key-env")?,
            session_token_env: a.session_token_env,
        },
        (None, None) => bail!("give --dir or --s3-endpoint"),
    };
    Ok(VerifyJob {
        name: a.name,
        description: a.description,
        target,
        manifest_keys: a.manifests,
        mode: match a.mode {
            ModeArg::Verify => JobMode::Verify,
            ModeArg::TestRestore => JobMode::TestRestore,
        },
        interval_secs: a.every,
        ignore_verified: a.ignore_verified,
        outdated_after_secs: if a.ignore_verified {
            a.outdated_after
        } else {
            0
        },
        encryption: a
            .key_id
            .zip(a.key_env)
            .map(|(key_id, key_env)| JobKey { key_id, key_env }),
        synthetic_bases: a.synthetic_bases,
        enabled: !a.disabled,
        created_at: Utc::now(),
    })
}

/// Runs the command and returns the process exit status.
pub async fn run(
    command: BackupVerifyCommand,
    state_dir: &Path,
    qemu_img: &str,
    _format: OutputFormat,
) -> Result<i32> {
    match command {
        BackupVerifyCommand::Verify(a) => check(a, JobMode::Verify, state_dir, qemu_img).await,
        BackupVerifyCommand::TestRestore(a) => {
            check(a, JobMode::TestRestore, state_dir, qemu_img).await
        }
        BackupVerifyCommand::Job { command } => match command {
            JobCommand::Add(a) => {
                let replace = a.replace;
                let job = bv::save_job(state_dir, job_from_args(a)?, replace)?;
                println!("{}", serde_json::to_string_pretty(&job)?);
                Ok(0)
            }
            JobCommand::Ls => {
                let state = bv::load_state(state_dir)?;
                let items: Vec<_> = bv::load_jobs(state_dir)?
                    .into_values()
                    .map(|j| json!({"job": &j, "state": state.get(&j.name)}))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"items": items}))?
                );
                Ok(0)
            }
            JobCommand::Rm { name } => {
                bv::delete_job(state_dir, &name)?;
                println!("{}", json!({"ok": true, "deleted": name}));
                Ok(0)
            }
            JobCommand::Run { name, force } => {
                let job = bv::load_jobs(state_dir)?
                    .remove(&name)
                    .with_context(|| format!("verify job {name:?} not found"))?;
                let run = bv::run_job(state_dir, qemu_img, &job, force).await?;
                for r in &run.reports {
                    eprintln!("{}: {}", r.manifest_key, r.summary());
                }
                println!("{}", serde_json::to_string_pretty(&run)?);
                Ok(if run.ok { 0 } else { EXIT_VERIFY_FAILED })
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: BackupVerifyCommand,
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30m"), Ok(1800));
        assert_eq!(parse_duration("6h"), Ok(21600));
        assert_eq!(parse_duration("1d"), Ok(86400));
        assert_eq!(parse_duration("90"), Ok(90));
        for bad in ["", "0", "x", "-1h", "1w"] {
            assert!(parse_duration(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn arguments_parse() {
        let c = Cli::try_parse_from([
            "x",
            "verify",
            "--dir",
            "/b",
            "--manifest",
            "a/b/c/00000001-x/manifest.json",
        ])
        .unwrap();
        assert!(matches!(c.command, BackupVerifyCommand::Verify(_)));
        // Needs a source and a manifest; a key id needs its file.
        assert!(Cli::try_parse_from(["x", "verify", "--manifest", "m"]).is_err());
        assert!(Cli::try_parse_from(["x", "verify", "--dir", "/b"]).is_err());
        assert!(
            Cli::try_parse_from([
                "x",
                "verify",
                "--dir",
                "/b",
                "--manifest",
                "m",
                "--key-id",
                "k"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "x",
                "job",
                "add",
                "n",
                "--dir",
                "/b",
                "--manifest",
                "m",
                "--every",
                "6h",
                "--ignore-verified",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["x", "job", "add", "n", "--dir", "/b", "--manifest", "m"])
                .is_err()
        );
    }

    /// The exit status is what scripts and cron see: 0 clean, 3 problems found.
    #[tokio::test]
    async fn exit_status_follows_the_result() {
        use fluxvm_backup_s3::dir::DirStore;
        use fluxvm_backup_s3::manifest::{DiskLayer, MANIFEST_VERSION, Manifest, ManifestKind};
        use fluxvm_backup_s3::{ObjectStore, PutOptions, backup_dir};
        use sha2::{Digest, Sha256};
        let (root, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let store = DirStore::new(root.path().to_path_buf());
        let dir = backup_dir("ns/vm", "c1", 1, "b1");
        let key = format!("{dir}/root.qcow2");
        let layer = vec![5u8; 512];
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
        let mkey = format!("{dir}/manifest.json");
        let o = PutOptions::default();
        store.put_bytes(&key, layer, &o).await.unwrap();
        store
            .put_bytes(&mkey, m.to_bytes().unwrap(), &o)
            .await
            .unwrap();

        let argv = |sub: &str| {
            Cli::try_parse_from([
                "x",
                sub,
                "--dir",
                root.path().to_str().unwrap(),
                "--manifest",
                mkey.as_str(),
            ])
            .unwrap()
            .command
        };
        let code = run(argv("verify"), state.path(), "qemu-img", OutputFormat::Json)
            .await
            .unwrap();
        assert_eq!(code, 0);
        // The layer is not a qcow2 image: verify passes (digest matches), test-restore fails.
        let code = run(
            argv("test-restore"),
            state.path(),
            "/nonexistent/qemu-img",
            OutputFormat::Json,
        )
        .await
        .unwrap();
        assert_eq!(code, EXIT_VERIFY_FAILED);
        std::fs::write(root.path().join(&key), vec![6u8; 512]).unwrap();
        let code = run(argv("verify"), state.path(), "qemu-img", OutputFormat::Json)
            .await
            .unwrap();
        assert_eq!(code, EXIT_VERIFY_FAILED);
    }
}
