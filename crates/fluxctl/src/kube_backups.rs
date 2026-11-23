// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl kube backup`, `backups`, `backup verify|prune|schedule|run-due` and `kube restore BACKUP`: safe, automatic
//! backups of a FluxVM k3s cluster (`fluxctl kube cluster`), built on the cluster's own datastore backup and restore
//! (`kube_apple::lifecycle::backup` and `restore`). What this adds:
//!
//! * a catalogue (`kube backups`: name, kind, age, size) and a verify step that proves a file is whole and holds what a
//!   restore needs, run on every new backup too, so a backup that could not be restored is never kept;
//! * a schedule, on by default for new clusters (every day, newest 7 kept), run by the daemon (`run-due`) or any cron;
//! * disk safety: a backup is only taken if the last one's size plus 2 GiB stays free, old scheduled backups are
//!   removed to make room, and manual backups are never removed automatically;
//! * a backup before every upgrade and before every restore;
//! * `--to DIR` or `--to s3://BUCKET/PREFIX` for a copy somewhere else.
//!
//! The rules (file layout, retention, room, verify) are `fluxvm_core::k3s_backups`, shared with the Studio Backups card.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use fluxvm_core::k3s_backups::{self as kb, BackupEntry, Kind, Room, Schedule, VerifyReport};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::kube_apple::host::Host;
use crate::kube_apple::lifecycle;
use crate::kube_apple::model::{Datastore, Phase, Store};

const DEFAULT_CLUSTER: &str = fluxvm_core::up::CLUSTER_NAME;

/// An error that ends with the command that explains it: `fluxctl explain CODE`.
fn coded(message: impl Into<String>, code: &str) -> anyhow::Error {
    debug_assert!(
        fluxvm_core::health::code(code).is_some(),
        "unknown code {code}"
    );
    anyhow::anyhow!("{}\n  more: fluxctl explain {code}", message.into())
}

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct BackupArgs {
    #[command(subcommand)]
    pub sub: Option<BackupSub>,
    /// The cluster (default: `default`, or the only one there is).
    #[arg(long)]
    pub cluster: Option<String>,
    /// Also keep a copy in this folder, or upload one to `s3://BUCKET/PREFIX` (endpoint and keys from FLUXVM_S3_ENDPOINT,
    /// AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION). The copy is verified and holds the cluster token.
    #[arg(long, value_name = "DIR|s3://BUCKET/PREFIX")]
    pub to: Option<String>,
    /// Print the result as JSON (what the Studio Backups card reads).
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum BackupSub {
    /// Check that a backup is whole and restorable without touching the cluster: checksum, complete archive, token,
    /// datastore. Default: the newest backup.
    Verify {
        /// A name from `kube backups`, or a path.
        backup: Option<String>,
        #[arg(long)]
        cluster: Option<String>,
    },
    /// Apply the retention policy now: delete scheduled and pre-upgrade backups beyond the newest N. Manual backups stay.
    Prune {
        #[arg(long)]
        cluster: Option<String>,
    },
    /// Show or change the automatic backups: `fluxctl kube backup schedule --every 6 --keep 14`, `--off`, `--on`.
    Schedule {
        #[arg(long)]
        cluster: Option<String>,
        #[arg(long)]
        off: bool,
        #[arg(long, conflicts_with = "off")]
        on: bool,
        /// Hours between backups (1 to 720).
        #[arg(long, value_name = "HOURS")]
        every: Option<u32>,
        /// Scheduled backups to keep.
        #[arg(long)]
        keep: Option<u32>,
        /// Pre-upgrade and pre-restore backups to keep.
        #[arg(long)]
        keep_pre_upgrade: Option<u32>,
    },
    /// Take a scheduled backup of every cluster whose schedule says one is due. Cheap when nothing is due, so the daemon
    /// (or a cron line) can run it every hour.
    RunDue {
        #[arg(long)]
        cluster: Option<String>,
    },
}

// ---- cluster and file resolution ---------------------------------------------------------------------------------------

pub fn pick_cluster(store: &Store, given: Option<&str>) -> Result<String> {
    if let Some(c) = given {
        if !kb::valid_name(c) {
            bail!("{c:?} is not a cluster name");
        }
        return Ok(c.to_owned());
    }
    let all = store.list()?;
    if all.iter().any(|s| s.name == DEFAULT_CLUSTER) || all.is_empty() {
        return Ok(DEFAULT_CLUSTER.to_owned());
    }
    if all.len() == 1 {
        return Ok(all[0].name.clone());
    }
    bail!(
        "there is more than one cluster ({}); pick one with --cluster NAME",
        all.iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A backup named on the command line: a path, or a file name in the cluster's backups folder.
pub fn resolve(store: &Store, cluster: &str, arg: &str) -> Result<PathBuf> {
    let direct = Path::new(arg);
    let p = if arg.contains('/') || direct.is_file() {
        direct.to_path_buf()
    } else if kb::valid_name(arg) {
        store.backups_dir(cluster).join(arg)
    } else {
        bail!("{arg:?} is not a backup name");
    };
    if !p.is_file() {
        return Err(coded(
            format!("there is no backup {arg} for cluster {cluster}"),
            "BACKUP_NOT_FOUND",
        ));
    }
    Ok(p)
}

pub fn newest(store: &Store, cluster: &str) -> Result<PathBuf> {
    kb::list(&store.backups_dir(cluster), cluster, now())
        .into_iter()
        .next()
        .map(|e| PathBuf::from(e.path))
        .ok_or_else(|| {
            coded(
                format!("cluster {cluster} has no backups yet; `fluxctl kube backup` takes one"),
                "BACKUP_NOT_FOUND",
            )
        })
}

fn datastore_word(d: Datastore) -> &'static str {
    match d {
        Datastore::Sqlite => "sqlite",
        Datastore::Etcd => "etcd",
    }
}

// ---- taking a backup ---------------------------------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct Taken {
    pub cluster: String,
    pub file: PathBuf,
    pub kind: Kind,
    pub bytes: u64,
    pub sha256: String,
    pub datastore: String,
    pub verified: VerifyReport,
    /// The offsite copy, when `--to` asked for one.
    pub copied_to: Option<String>,
    /// Old scheduled or pre-upgrade backups removed by retention or to make room.
    pub removed: Vec<String>,
}

/// Where `--to` points.
#[derive(Debug, PartialEq, Eq)]
pub enum Dest {
    Dir(PathBuf),
    S3 { bucket: String, prefix: String },
}

impl Dest {
    pub fn parse(s: &str) -> Result<Dest> {
        if let Some(rest) = s.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            let prefix = prefix.trim_matches('/');
            if bucket.is_empty() {
                bail!("--to s3:// needs a bucket: s3://BUCKET/PREFIX");
            }
            if !prefix.is_empty() {
                fluxvm_backup_s3::validate_prefix(prefix)?;
            }
            return Ok(Dest::S3 {
                bucket: bucket.into(),
                prefix: prefix.into(),
            });
        }
        Ok(Dest::Dir(PathBuf::from(s)))
    }
}

fn s3_target(bucket: &str) -> Result<fluxvm_backup_s3::Target> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let endpoint = env("FLUXVM_S3_ENDPOINT").context("set FLUXVM_S3_ENDPOINT (for example https://s3.eu-central-1.amazonaws.com) to upload to S3")?;
    Ok(fluxvm_backup_s3::Target::S3(
        fluxvm_backup_s3::s3::S3Target {
            endpoint,
            bucket: bucket.into(),
            region: env("AWS_REGION").unwrap_or_else(|| "us-east-1".into()),
            access_key_id: env("AWS_ACCESS_KEY_ID")
                .context("set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY to upload to S3")?,
            secret_access_key: env("AWS_SECRET_ACCESS_KEY")
                .context("set AWS_SECRET_ACCESS_KEY to upload to S3")?,
            session_token: env("AWS_SESSION_TOKEN"),
            path_style: env("FLUXVM_S3_VIRTUAL_HOSTED").is_none(),
        },
    ))
}

/// Puts the backup and its checksum under `PREFIX/CLUSTER/`.
pub async fn upload(
    store: &dyn fluxvm_backup_s3::ObjectStore,
    prefix: &str,
    cluster: &str,
    file: &Path,
) -> Result<String> {
    let name = file
        .file_name()
        .and_then(|f| f.to_str())
        .context("backup file name")?;
    let key = if prefix.is_empty() {
        format!("{cluster}/{name}")
    } else {
        format!("{prefix}/{cluster}/{name}")
    };
    let opts = fluxvm_backup_s3::PutOptions::default();
    store.put_file(&key, file, &opts).await?;
    store
        .put_file(&format!("{key}.sha256"), &kb::sidecar(file), &opts)
        .await?;
    Ok(key)
}

fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Takes a backup without holding the cluster's lock (the caller holds it, as `upgrade` does).
pub async fn take<H: Host>(
    h: &H,
    store: &Store,
    cluster: &str,
    kind: Kind,
    to: Option<&Dest>,
) -> Result<Taken> {
    let st = store.require(cluster)?;
    let ds = datastore_word(st.datastore);
    let dir = store.backups_dir(cluster);
    private_dir(&dir)?;
    let (target_dir, offsite) = match to {
        Some(Dest::Dir(d)) => {
            private_dir(d)?;
            (d.clone(), None)
        }
        Some(Dest::S3 { bucket, prefix }) => {
            // Fail on missing credentials before the cluster is touched.
            (dir.clone(), Some((s3_target(bucket)?, prefix.clone())))
        }
        None => (dir.clone(), None),
    };

    // Room: only ever remove what retention may remove, and never go under the reserve.
    let mut removed = vec![];
    let entries = kb::list(&dir, cluster, now());
    let in_catalogue = target_dir == dir;
    match kb::room(
        kb::free_bytes(&target_dir),
        if in_catalogue { &entries } else { &[] },
    ) {
        Room::Enough => {}
        Room::MakeRoom(names) => {
            for n in names {
                if kb::delete(&dir.join(&n)).is_ok() {
                    removed.push(n);
                }
            }
        }
        Room::Short { free, needed } => {
            return Err(coded(
                format!(
                    "not enough disk space for a backup in {}: {} MiB free, {} MiB needed (the last backup's size plus a 2 GiB reserve); nothing was changed",
                    target_dir.display(),
                    free / 1048576,
                    needed / 1048576
                ),
                "BACKUP_DISK_LOW",
            ));
        }
    }

    let file = target_dir.join(kind.file_name(cluster, &h.stamp()));
    let b = lifecycle::backup(h, store, cluster, Some(file.clone())).await?;
    let verified = kb::verify_file(&file, Some(ds));
    if !verified.restorable {
        let _ = kb::delete(&file);
        let problem = verified.problem().unwrap_or_default();
        let code = if problem.starts_with("checksum") {
            "BACKUP_CHECKSUM_MISMATCH"
        } else {
            "BACKUP_NOT_RESTORABLE"
        };
        return Err(coded(
            format!(
                "the backup was taken but would not restore ({problem}), so it was deleted; the cluster is unchanged"
            ),
            code,
        ));
    }
    let mut copied_to = None;
    if let Some((target, prefix)) = offsite {
        let key = upload(&*target.open()?, &prefix, cluster, &file)
            .await
            .context("uploading the backup (it is kept locally)")?;
        copied_to = Some(key);
    }
    // Retention (scheduled and pre-upgrade only) after the new one exists.
    if in_catalogue {
        let sched = kb::load_schedule(&dir).unwrap_or_default();
        for n in kb::prune_plan(&kb::list(&dir, cluster, now()), &sched) {
            if kb::delete(&dir.join(&n)).is_ok() {
                removed.push(n);
            }
        }
    }
    Ok(Taken {
        cluster: cluster.into(),
        file,
        kind,
        bytes: b.bytes as u64,
        sha256: b.sha256,
        datastore: ds.into(),
        verified,
        copied_to,
        removed,
    })
}

/// [`take`] while holding the cluster's lock, for `kube backup` and the schedule.
pub async fn take_exclusive<H: Host>(
    h: &H,
    store: &Store,
    cluster: &str,
    kind: Kind,
    to: Option<&Dest>,
) -> Result<Taken> {
    store.require(cluster)?;
    let _lock = store.lock(cluster)?;
    take(h, store, cluster, kind, to).await
}

// ---- restoring ---------------------------------------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct Restored {
    pub cluster: String,
    pub backup: PathBuf,
    /// The backup of the state that was replaced; `None` when that could not be taken (the cluster was already broken).
    pub safety_backup: Option<PathBuf>,
    pub safety_note: Option<String>,
    pub restore: lifecycle::RestoreReport,
}

/// Verifies `file`, saves the current state, and replaces the datastore with it.
pub async fn restore<H: Host>(
    h: &H,
    store: &Store,
    cluster: &str,
    file: &Path,
    yes: bool,
) -> Result<Restored> {
    let st = store.require(cluster)?;
    if !yes {
        return Err(coded(
            format!(
                "restoring replaces everything in cluster {cluster} with the state in {}; add --yes to do it (check first with `fluxctl kube backup verify`)",
                file.display()
            ),
            "RESTORE_NEEDS_CONFIRM",
        ));
    }
    let v = kb::verify_file(file, Some(datastore_word(st.datastore)));
    if !v.restorable {
        let problem = v.problem().unwrap_or_default();
        let code = if problem.starts_with("checksum") {
            "BACKUP_CHECKSUM_MISMATCH"
        } else {
            "BACKUP_NOT_RESTORABLE"
        };
        return Err(coded(
            format!(
                "{} cannot be restored ({problem}); nothing was changed",
                file.display()
            ),
            code,
        ));
    }
    let _lock = store.lock(cluster)?;
    // The state about to be replaced is saved first. If the cluster is too broken for that, say so and go on: a broken
    // cluster is the usual reason to restore (SQLite also keeps its old directory in the guest).
    let (safety_backup, safety_note) = match take(h, store, cluster, Kind::PreRestore, None).await {
        Ok(t) => (Some(t.file), None),
        Err(e) => (
            None,
            Some(format!(
                "no backup of the current state could be taken ({e:#}); the restore went ahead"
            )),
        ),
    };
    // `lifecycle::restore` takes the lock itself, so release ours first.
    drop(_lock);
    let restore = lifecycle::restore(h, store, cluster, file, None, true).await?;
    Ok(Restored {
        cluster: cluster.into(),
        backup: file.to_path_buf(),
        safety_backup,
        safety_note,
        restore,
    })
}

// ---- the catalogue -----------------------------------------------------------------------------------------------------

#[derive(Serialize, Debug)]
pub struct Catalogue {
    pub schema: u32,
    pub cluster: String,
    pub dir: String,
    pub schedule: Option<Schedule>,
    pub backups: Vec<BackupEntry>,
    pub total_bytes: u64,
    pub disk_free_bytes: Option<u64>,
}

pub fn catalogue(store: &Store, cluster: &str) -> Catalogue {
    let dir = store.backups_dir(cluster);
    let backups = kb::list(&dir, cluster, now());
    Catalogue {
        schema: kb::SCHEMA,
        cluster: cluster.into(),
        schedule: kb::load_schedule(&dir),
        total_bytes: backups.iter().map(|b| b.bytes).sum(),
        disk_free_bytes: kb::free_bytes(&dir),
        dir: dir.to_string_lossy().into_owned(),
        backups,
    }
}

fn size(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
    } else if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64)
    } else {
        format!("{} KiB", b.div_ceil(1024))
    }
}

pub fn render_catalogue(c: &Catalogue) -> String {
    let mut s = String::new();
    if c.backups.is_empty() {
        s.push_str(&format!(
            "{}: no backups yet. `fluxctl kube backup` takes one.\n",
            c.cluster
        ));
    } else {
        s.push_str(&format!(
            "{:<44} {:<14} {:>10} {:>9}\n",
            "BACKUP", "KIND", "AGE", "SIZE"
        ));
        for b in &c.backups {
            let mark = if b.has_checksum {
                ""
            } else {
                "  (no checksum: cannot be restored)"
            };
            s.push_str(&format!(
                "{:<44} {:<14} {:>10} {:>9}{mark}\n",
                b.name,
                b.kind.word(),
                fluxvm_core::health::human_age(b.age_secs),
                size(b.bytes)
            ));
        }
        s.push_str(&format!(
            "{} backup(s), {} in {}\n",
            c.backups.len(),
            size(c.total_bytes),
            c.dir
        ));
    }
    match &c.schedule {
        Some(sc) if sc.enabled => s.push_str(&format!(
            "automatic: every {} h, keeping the newest {} (and {} before upgrades)\n",
            sc.every_hours, sc.keep, sc.keep_pre_upgrade
        )),
        Some(_) => s.push_str("automatic backups are off: `fluxctl kube backup schedule --on`\n"),
        None => s.push_str("automatic backups are not set up for this cluster: `fluxctl kube backup schedule --on`\n"),
    }
    s
}

pub fn render_verify(v: &VerifyReport) -> String {
    let mut s = String::new();
    for c in &v.checks {
        s.push_str(&format!(
            "  [{}] {}: {}\n",
            if c.ok { "ok" } else { "FAIL" },
            c.name,
            c.detail
        ));
    }
    s.push_str(&if v.restorable {
        format!(
            "{} is restorable ({}). Only a restore proves the data inside is consistent.\n",
            v.file,
            v.datastore.as_deref().unwrap_or("?")
        )
    } else {
        format!(
            "{} would NOT restore.\n  more: fluxctl explain {}\n",
            v.file,
            if v.problem().is_some_and(|p| p.starts_with("checksum")) {
                "BACKUP_CHECKSUM_MISMATCH"
            } else {
                "BACKUP_NOT_RESTORABLE"
            }
        )
    });
    s
}

// ---- schedule ----------------------------------------------------------------------------------------------------------

/// Called after a cluster is created: a new cluster backs itself up every day unless told otherwise. Best effort.
pub fn enable_default(store: &Store, cluster: &str) {
    let dir = store.backups_dir(cluster);
    if kb::load_schedule(&dir).is_none() {
        let _ = private_dir(&dir).and_then(|()| Ok(kb::save_schedule(&dir, &Schedule::default())?));
    }
}

#[derive(Serialize, Debug)]
pub struct DueReport {
    pub cluster: String,
    pub action: String,
    pub detail: Option<String>,
}

/// The clusters to look at: the named one, or every cluster with a schedule.
pub async fn run_due<H: Host>(h: &H, store: &Store, only: Option<&str>) -> Result<Vec<DueReport>> {
    let mut out = vec![];
    for st in store.list()? {
        if only.is_some_and(|c| c != st.name) || !kb::valid_name(&st.name) {
            continue;
        }
        let dir = store.backups_dir(&st.name);
        let rep = |action: &str, detail: Option<String>| DueReport {
            cluster: st.name.clone(),
            action: action.into(),
            detail,
        };
        let Some(sched) = kb::load_schedule(&dir) else {
            out.push(rep("skipped", Some("no automatic backups set up".into())));
            continue;
        };
        if st.external_url.is_some() || st.phase != Phase::Ready {
            out.push(rep(
                "skipped",
                Some("the cluster is not running here".into()),
            ));
            continue;
        }
        if !kb::is_due(&sched, &kb::list(&dir, &st.name, now())) {
            out.push(rep("not due", None));
            continue;
        }
        match take_exclusive(h, store, &st.name, Kind::Auto, None).await {
            Ok(t) => out.push(rep("backed up", Some(t.file.display().to_string()))),
            Err(e) => out.push(rep("failed", Some(format!("{e:#}")))),
        }
    }
    Ok(out)
}

/// Deletes what retention allows for one cluster.
pub fn prune(store: &Store, cluster: &str) -> Vec<String> {
    let dir = store.backups_dir(cluster);
    let sched = kb::load_schedule(&dir).unwrap_or_default();
    kb::prune_plan(&kb::list(&dir, cluster, now()), &sched)
        .into_iter()
        .filter(|n| kb::delete(&dir.join(n)).is_ok())
        .collect()
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

/// Everything that needs no VM: verify, prune, schedule, list. `None` when the command needs the cluster.
pub fn run_offline(store: &Store, a: &BackupSub) -> Result<()> {
    match a {
        BackupSub::Verify { backup, cluster } => {
            let c = pick_cluster(store, cluster.as_deref())?;
            let file = match backup {
                Some(b) => resolve(store, &c, b)?,
                None => newest(store, &c)?,
            };
            let expect = store.load(&c)?.map(|s| datastore_word(s.datastore));
            let v = kb::verify_file(&file, expect);
            print!("{}", render_verify(&v));
            if !v.restorable {
                std::process::exit(1);
            }
            Ok(())
        }
        BackupSub::Prune { cluster } => {
            let c = pick_cluster(store, cluster.as_deref())?;
            print_json(&serde_json::json!({"cluster": c, "removed": prune(store, &c)}))
        }
        BackupSub::Schedule {
            cluster,
            off,
            on,
            every,
            keep,
            keep_pre_upgrade,
        } => {
            let c = pick_cluster(store, cluster.as_deref())?;
            let dir = store.backups_dir(&c);
            let mut s = kb::load_schedule(&dir).unwrap_or_else(|| Schedule {
                enabled: false,
                ..Schedule::default()
            });
            let changed =
                *off || *on || every.is_some() || keep.is_some() || keep_pre_upgrade.is_some();
            if changed {
                s.enabled = if *off {
                    false
                } else if *on || every.is_some() || keep.is_some() {
                    true
                } else {
                    s.enabled
                };
                if let Some(e) = every {
                    s.every_hours = *e;
                }
                if let Some(k) = keep {
                    s.keep = *k;
                }
                if let Some(k) = keep_pre_upgrade {
                    s.keep_pre_upgrade = *k;
                }
                s.valid().map_err(|e| anyhow::anyhow!(e))?;
                private_dir(&dir)?;
                kb::save_schedule(&dir, &s)?;
            }
            print_json(&s)
        }
        BackupSub::RunDue { .. } => unreachable!("run-due needs the cluster"),
    }
}

// ---- command entry points ----------------------------------------------------------------------------------------------

/// A `kube restore` argument that names a k3s backup rather than a checkpoint label (labels are lowercase letters, digits
/// and dashes, so neither a path nor `.tar.gz` can be one).
pub fn is_backup_arg(s: &str) -> bool {
    s.ends_with(".tar.gz") || s.contains('/')
}

fn open_store() -> Result<Store> {
    Ok(Store::new(crate::kube_apple::model::default_root()?))
}

/// `kube backups`.
pub fn cmd_list(cluster: Option<&str>, json: bool) -> Result<()> {
    let store = open_store()?;
    let c = pick_cluster(&store, cluster)?;
    let cat = catalogue(&store, &c);
    if json {
        print_json(&cat)
    } else {
        print!("{}", render_catalogue(&cat));
        Ok(())
    }
}

/// The subcommands that need no VM: verify, prune, schedule.
pub fn cmd_offline(sub: &BackupSub) -> Result<()> {
    run_offline(&open_store()?, sub)
}

/// `kube backup` (a backup now) and `kube backup run-due`.
pub async fn cmd_backup<A: crate::run::VmApi>(api: &A, a: BackupArgs, json: bool) -> Result<()> {
    let store = open_store()?;
    let h = crate::kube_apple::host::RealHost::new(api, &store, None)?;
    if let Some(BackupSub::RunDue { cluster }) = &a.sub {
        let r = run_due(&h, &store, cluster.as_deref()).await?;
        if json {
            return print_json(&r);
        }
        for d in &r {
            println!(
                "{}: {}{}",
                d.cluster,
                d.action,
                d.detail
                    .as_deref()
                    .map(|x| format!(" ({x})"))
                    .unwrap_or_default()
            );
        }
        if r.iter().any(|d| d.action == "failed") {
            bail!("a scheduled backup failed; run `fluxctl kube backup` to see why");
        }
        return Ok(());
    }
    let c = pick_cluster(&store, a.cluster.as_deref())?;
    let to = a.to.as_deref().map(Dest::parse).transpose()?;
    let t = take_exclusive(&h, &store, &c, Kind::Manual, to.as_ref()).await?;
    if json {
        return print_json(&t);
    }
    println!(
        "backed up {c} to {} ({}, verified restorable)",
        t.file.display(),
        size(t.bytes)
    );
    if let Some(k) = &t.copied_to {
        println!("copy uploaded as {k}");
    }
    if !t.removed.is_empty() {
        println!(
            "removed {} old backup(s) by retention: {}",
            t.removed.len(),
            t.removed.join(", ")
        );
    }
    println!(
        "it contains the cluster token: keep it private. Restore with `fluxctl kube restore {} --yes`.",
        t.file.file_name().and_then(|f| f.to_str()).unwrap_or("")
    );
    Ok(())
}

/// `kube restore BACKUP|--latest`.
pub async fn cmd_restore<A: crate::run::VmApi>(
    api: &A,
    backup: Option<&str>,
    latest: bool,
    cluster: Option<&str>,
    yes: bool,
    json: bool,
) -> Result<()> {
    let store = open_store()?;
    let c = pick_cluster(&store, cluster)?;
    let file = match (backup, latest) {
        (Some(b), false) => resolve(&store, &c, b)?,
        (_, true) => newest(&store, &c)?,
        (None, false) => bail!("name a backup or use --latest (`fluxctl kube backups` lists them)"),
    };
    let h = crate::kube_apple::host::RealHost::new(api, &store, None)?;
    let r = restore(&h, &store, &c, &file, yes).await?;
    if json {
        return print_json(&r);
    }
    println!("restored {c} from {}", file.display());
    match (&r.safety_backup, &r.safety_note) {
        (Some(f), _) => println!("the state it replaced was saved first: {}", f.display()),
        (_, Some(n)) => println!("{n}"),
        _ => {}
    }
    println!("{}", r.restore.note);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_apple::model::Store;

    #[test]
    fn destinations_parse() {
        assert_eq!(
            Dest::parse("/mnt/big").unwrap(),
            Dest::Dir("/mnt/big".into())
        );
        assert_eq!(
            Dest::parse("s3://bk/fluxvm/team").unwrap(),
            Dest::S3 {
                bucket: "bk".into(),
                prefix: "fluxvm/team".into()
            }
        );
        assert_eq!(
            Dest::parse("s3://bk").unwrap(),
            Dest::S3 {
                bucket: "bk".into(),
                prefix: "".into()
            }
        );
        assert!(Dest::parse("s3://").is_err());
        assert!(Dest::parse("s3://bk/../x").is_err());
    }

    #[test]
    fn a_new_cluster_gets_the_default_schedule_once() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path().to_path_buf());
        enable_default(&store, "dev");
        let dir = store.backups_dir("dev");
        assert_eq!(kb::load_schedule(&dir), Some(Schedule::default()));
        kb::save_schedule(
            &dir,
            &Schedule {
                every_hours: 6,
                ..Schedule::default()
            },
        )
        .unwrap();
        enable_default(&store, "dev");
        assert_eq!(
            kb::load_schedule(&dir).unwrap().every_hours,
            6,
            "a chosen policy is not overwritten"
        );
    }

    #[test]
    fn resolving_names_and_missing_backups() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path().to_path_buf());
        let dir = store.backups_dir("dev");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dev-20260101-000000.tar.gz"), b"x").unwrap();
        assert!(resolve(&store, "dev", "dev-20260101-000000.tar.gz").is_ok());
        let e = resolve(&store, "dev", "nope.tar.gz")
            .unwrap_err()
            .to_string();
        assert!(e.contains("fluxctl explain BACKUP_NOT_FOUND"), "{e}");
        assert!(newest(&store, "dev").is_ok());
        assert!(newest(&store, "other").is_err());
        assert!(resolve(&store, "dev", "..").is_err());
    }

    #[test]
    fn catalogue_text_says_what_is_there_and_whether_it_is_automatic() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path().to_path_buf());
        let c = catalogue(&store, "dev");
        let t = render_catalogue(&c);
        assert!(
            t.contains("no backups yet") && t.contains("not set up"),
            "{t}"
        );
        enable_default(&store, "dev");
        std::fs::write(
            store
                .backups_dir("dev")
                .join("dev-auto-20260101-000000.tar.gz"),
            vec![0u8; 2048],
        )
        .unwrap();
        let t = render_catalogue(&catalogue(&store, "dev"));
        assert!(t.contains("scheduled") && t.contains("every 24 h"), "{t}");
    }

    #[test]
    fn schedule_command_changes_and_validates() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path().to_path_buf());
        let sub = |every, off| BackupSub::Schedule {
            cluster: Some("dev".into()),
            off,
            on: false,
            every,
            keep: None,
            keep_pre_upgrade: None,
        };
        run_offline(&store, &sub(Some(6), false)).unwrap();
        let s = kb::load_schedule(&store.backups_dir("dev")).unwrap();
        assert!(s.enabled && s.every_hours == 6);
        run_offline(&store, &sub(None, true)).unwrap();
        assert!(
            !kb::load_schedule(&store.backups_dir("dev"))
                .unwrap()
                .enabled
        );
        assert!(run_offline(&store, &sub(Some(0), false)).is_err());
    }

    #[test]
    fn prune_removes_only_scheduled_beyond_keep() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::new(d.path().to_path_buf());
        let dir = store.backups_dir("dev");
        kb::save_schedule(
            &dir,
            &Schedule {
                keep: 1,
                ..Schedule::default()
            },
        )
        .unwrap();
        for (i, n) in [
            "dev-auto-20260101-000000",
            "dev-auto-20260102-000000",
            "dev-20260101-000000",
        ]
        .iter()
        .enumerate()
        {
            let p = dir.join(format!("{n}.tar.gz"));
            std::fs::write(&p, b"x").unwrap();
            let t = std::time::SystemTime::now()
                - std::time::Duration::from_secs(1000 - i as u64 * 100);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        let gone = prune(&store, "dev");
        assert_eq!(gone, ["dev-auto-20260101-000000.tar.gz"]);
        assert!(dir.join("dev-20260101-000000.tar.gz").exists());
    }

    #[tokio::test]
    async fn offsite_upload_writes_the_backup_and_its_checksum() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("dev-20260101-000000.tar.gz");
        std::fs::write(&f, b"data").unwrap();
        std::fs::write(kb::sidecar(&f), b"abc  dev-20260101-000000.tar.gz\n").unwrap();
        let store = fluxvm_backup_s3::mem::MemStore::new();
        let key = upload(&store, "team", "dev", &f).await.unwrap();
        assert_eq!(key, "team/dev/dev-20260101-000000.tar.gz");
        assert_eq!(store.keys().len(), 2);
    }
}
