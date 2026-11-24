// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac backup create|ls|restore|rm|verify|schedule|run-due`: backups of macOS instances and named volumes.
//!
//! The archive, manifest, checksum, retention and free-disk rules live in `fluxvm_core::mac_backup`, shared with the Studio API
//! (`/v1/mac/backups`). This file is the command line over them: it finds the instance through the daemon, restores through the
//! same daemon calls `mac fork` uses, and says in plain words what was and was not captured.
//!
//! A restore never overwrites quietly: an existing instance or volume of that name needs `--replace`, and what it replaces is
//! first saved as an automatic backup (`automatic`, newest 3 kept) because the replaced instance's own checkpoints go with it.
//!
//! Not run against a real daemon, Mac or macOS VM; the tests use a fake daemon and temporary folders.

use crate::mac_errors::coded;
use crate::mac_inspect::human_bytes;
use crate::mac_store::{self, Store};
use crate::mac_volume::{self as vol, VolumeStore};
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use fluxvm_core::mac_backup::{self as core, BackupError, Kind, Manifest, Request, State};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const RESTORED_FROM_LABEL: &str = "fluxvm.mac.restored-from";

#[derive(Subcommand, Debug)]
pub enum MacBackup {
    /// Back up, list, verify, restore and schedule backups of macOS instances and named volumes.
    Backup {
        #[command(subcommand)]
        command: BackupCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum BackupCmd {
    /// Back up a STOPPED instance (or, with --volume, a named volume): a compressed archive plus a manifest with its sha256, in
    /// `~/.fluxvm/mac-backups` (`FLUXVM_MAC_BACKUPS` moves it).
    #[command(
        after_help = "Examples:\n  fluxctl mac stop ci && fluxctl mac backup create ci\n  fluxctl mac backup create --volume build-cache --keep 3\n\nWhat it holds: the instance's disk and macOS hardware identity, its labels (including the restart policy), size and published ports; for a volume, its files and labels. What it NEVER holds: FluxVM secret values, registry logins, saved memory. Anything you stored inside the guest's own disk (keychain items, files) is in the disk and so in the backup.\n\nThe Mac's internal disk is often nearly full, so it checks first: the data's size plus 2 GiB must stay free, otherwise it stops and says how much is needed. --keep N (default 7) deletes the oldest ordinary backups of the same instance or volume afterwards. A sparse disk is archived at its full size, so a large disk takes a while. If it fails: fluxctl explain MAC_BACKUP_NO_ROOM"
    )]
    Create {
        /// The instance (`fluxctl mac ps -a`), or the volume with --volume.
        name: String,
        /// NAME is a named volume (`fluxctl mac volume ls`).
        #[arg(long)]
        volume: bool,
        /// Ordinary backups of this instance or volume to keep (the oldest are deleted after this one succeeds).
        #[arg(long, default_value_t = core::DEFAULT_KEEP)]
        keep: u32,
        /// A note stored in the manifest.
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List backups with their size, age and state, the free room on the backups disk and the schedules.
    Ls {
        #[arg(long)]
        json: bool,
    },
    /// Put a backup back: an instance comes back stopped, a volume gets its files. The checksum is verified first.
    #[command(
        after_help = "Examples:\n  fluxctl mac backup restore ci                    newest backup of ci (needs that no instance 'ci' exists)\n  fluxctl mac backup restore ci --replace          replace the existing ci, saving it as an automatic backup first\n  fluxctl mac backup restore instance-ci-20261124-101500 --as ci-copy\n  fluxctl mac backup restore volume:build-cache --replace\n\nBACKUP is an id from `fluxctl mac backup ls`, or a name (the newest backup of it; `instance:NAME` / `volume:NAME` when both exist). Nothing is touched when the checksum does not match. An existing instance or volume of that name is never overwritten without --replace; with it, the current state is first backed up as an automatic backup (an instance must be stopped, a volume unused). The restored instance is created stopped: `fluxctl mac start NAME`. If it fails: fluxctl explain MAC_BACKUP_EXISTS"
    )]
    Restore {
        backup: String,
        /// Overwrite the instance or volume that has this name, after backing it up automatically.
        #[arg(long)]
        replace: bool,
        /// Restore under another name.
        #[arg(long = "as", value_name = "NAME")]
        as_name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete backups by id.
    Rm {
        #[arg(required = true)]
        backups: Vec<String>,
    },
    /// Check a backup's archive against its sha256 and read it to the end (all of them with --all). Needs no daemon.
    Verify {
        backup: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Automatic backups: `schedule NAME --every 24h`, `--off` to stop, no NAME to list. A stopped instance or a volume only.
    #[command(
        after_help = "Examples:\n  fluxctl mac backup schedule ci --every 24h --keep 7\n  fluxctl mac backup schedule --volume build-cache --every 6h\n  fluxctl mac backup schedule ci --off\n  fluxctl mac backup schedule\n\nThe daemon checks the schedules every minute and takes what is due; without a daemon, run `fluxctl mac backup run-due` from cron or launchd. An instance that is running at the time is skipped and tried again after the next interval (a running disk is not a consistent backup). Interval: 5m to 90d, like 30m, 24h, 7d. If it fails: fluxctl explain MAC_BACKUP_SCHEDULE"
    )]
    Schedule {
        name: Option<String>,
        #[arg(long)]
        volume: bool,
        #[arg(long)]
        every: Option<String>,
        #[arg(long, default_value_t = core::DEFAULT_KEEP)]
        keep: u32,
        /// Stop the automatic backups of NAME.
        #[arg(long)]
        off: bool,
        #[arg(long)]
        json: bool,
    },
    /// Take the scheduled backups that are due now. The daemon does this by itself every minute; use this from cron on a Mac
    /// without one.
    RunDue {
        #[arg(long)]
        json: bool,
    },
}

impl MacBackup {
    /// Everything but the commands that read instances from the daemon works without one.
    pub fn is_client_side(&self) -> bool {
        let Self::Backup { command } = self;
        matches!(
            command,
            BackupCmd::Ls { .. }
                | BackupCmd::Rm { .. }
                | BackupCmd::Verify { .. }
                | BackupCmd::Schedule { .. }
                | BackupCmd::Create { volume: true, .. }
        )
    }
}

fn now() -> u64 {
    crate::mac_health::now_unix()
}

/// A core failure with its `fluxctl explain` pointer.
pub fn coded_err(e: BackupError) -> anyhow::Error {
    coded(e.message, e.code)
}

fn root() -> Result<PathBuf> {
    core::default_root()
        .context("HOME is not set (or set FLUXVM_MAC_BACKUPS to the backups folder)")
}

// ----------------------------------------------------------------------------------------------------------- the daemon

/// What the operations need from the daemon; a fake in tests.
#[allow(async_fn_in_trait)]
pub trait Daemon {
    async fn instances(&self) -> Result<Vec<Value>>;
    /// `POST /v1/vms`: the new VM's id.
    async fn create_vm(&self, body: Value) -> Result<String>;
    async fn patch_labels(&self, id: &str, labels: Value) -> Result<()>;
    async fn delete(&self, id: &str) -> Result<()>;
}

impl Daemon for crate::remote::Remote {
    async fn instances(&self) -> Result<Vec<Value>> {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!(
                    "/v1/vms?label={}",
                    crate::remote::encode_query(crate::mac_cmd::INSTANCE_SELECTOR)
                ),
                None,
            )
            .await?;
        Ok(crate::mac_cmd::filter_instances(&v["items"])
            .into_iter()
            .cloned()
            .collect())
    }

    async fn create_vm(&self, body: Value) -> Result<String> {
        let vm = self
            .call(reqwest::Method::POST, "/v1/vms", Some(body))
            .await?;
        Ok(vm["id"]
            .as_str()
            .context("the daemon returned no id")?
            .to_string())
    }

    async fn patch_labels(&self, id: &str, labels: Value) -> Result<()> {
        self.call(
            reqwest::Method::PATCH,
            &format!("/v1/vms/{id}"),
            Some(json!({"labels": labels})),
        )
        .await
        .map(drop)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        crate::mac_cmd::delete_instance(self, id).await
    }
}

fn named<'a>(all: &'a [Value], name: &str) -> Option<&'a Value> {
    all.iter().find(|v| v["name"].as_str() == Some(name))
}

fn no_instance(name: &str) -> anyhow::Error {
    anyhow::anyhow!("no instance named {name:?} (see `fluxctl mac ps -a`)")
}

// ----------------------------------------------------------------------------------------------------------- operations

/// Back up one instance (it must be stopped) or one volume.
pub fn back_up(
    plan: core::Plan,
    root: &Path,
    keep: u32,
    note: Option<String>,
    automatic: bool,
    t: u64,
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Result<core::Created> {
    core::create(
        root,
        Request {
            plan,
            keep,
            automatic,
            note,
            now: t,
            id: None,
        },
        free,
    )
    .map_err(coded_err)
}

pub fn created_text(c: &core::Created) -> String {
    let m = &c.manifest;
    let mut s = format!(
        "backed up {} {} as {}\n  {} on disk (from {}), sha256 {}\n  held:\n",
        m.kind.word(),
        m.name,
        m.id,
        human_bytes(m.archive_bytes),
        human_bytes(m.source_bytes),
        &m.sha256[..16.min(m.sha256.len())]
    );
    for h in &m.captured {
        s.push_str(&format!("    - {h}\n"));
    }
    s.push_str("  NOT in it:\n");
    for x in &m.excluded {
        s.push_str(&format!("    - {x}\n"));
    }
    if !c.pruned.is_empty() {
        s.push_str(&format!(
            "  removed {} older backup(s) beyond --keep: {}\n",
            c.pruned.len(),
            c.pruned.join(", ")
        ));
    }
    s.push_str(&format!(
        "  check it: fluxctl mac backup verify {id}\n  restore:  fluxctl mac backup restore {id}{}",
        if m.kind == Kind::Instance {
            "   (add --replace to overwrite the instance)"
        } else {
            "   (add --replace to overwrite the volume)"
        },
        id = m.id
    ));
    s
}

#[derive(Debug)]
pub struct Restored {
    pub kind: Kind,
    pub name: String,
    pub backup: String,
    /// The automatic backup of what was replaced.
    pub safety: Option<String>,
    pub vm_id: Option<String>,
}

fn target_name(m: &Manifest, as_name: Option<&str>) -> Result<String> {
    let n = as_name.unwrap_or(&m.name).trim().to_string();
    if n.is_empty() || n.contains(char::is_whitespace) || n.contains('/') {
        bail!("{n:?} is not a usable name: no spaces or slashes, for example `--as ci-copy`");
    }
    Ok(n)
}

fn exists_error(kind: Kind, name: &str, backup: &str) -> anyhow::Error {
    coded(
        format!(
            "{} {name:?} already exists, and a restore never overwrites quietly. Restore under another name with `--as {name}-restored`, or replace it with `fluxctl mac backup restore {backup} --replace` (what it holds now is saved as an automatic backup first)",
            kind.word()
        ),
        "MAC_BACKUP_EXISTS",
    )
}

/// Restore an instance backup through the daemon.
#[allow(clippy::too_many_arguments)]
pub async fn restore_instance<D: Daemon>(
    d: &D,
    store: &Store,
    root: &Path,
    m: &Manifest,
    replace: bool,
    as_name: Option<&str>,
    t: u64,
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Result<Restored> {
    // The checksum comes first: nothing below runs for a damaged archive.
    core::verify(root, m).map_err(coded_err)?;
    let name = target_name(m, as_name)?;
    let all = d.instances().await?;
    let existing = named(&all, &name);
    if existing.is_some() && !replace {
        return Err(exists_error(Kind::Instance, &name, &m.id));
    }
    // What the restore will replace is saved before anything is deleted: its own checkpoints go with the instance.
    let mut safety = None;
    if let Some(old) = existing {
        let plan = core::instance_plan(old).map_err(coded_err)?;
        let c = back_up(
            plan,
            root,
            core::AUTO_KEEP as u32,
            Some(format!("automatic, before restoring {}", m.id)),
            true,
            t,
            free,
        )?;
        safety = Some(c.manifest.id);
    }
    core::check_room_for(free(store.root()), m.source_bytes, "this restore").map_err(coded_err)?;
    let scratch = store.scratch("backup-restore");
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    if let Err(e) = core::unpack(root, m, &scratch).map_err(coded_err) {
        std::fs::remove_dir_all(&scratch).ok();
        return Err(e);
    }
    for f in mac_store::REQUIRED_FILES {
        if !scratch.join(f).is_file() {
            std::fs::remove_dir_all(&scratch).ok();
            bail!(
                "backup {} has no {f}: it is not a complete instance backup, so nothing was restored",
                m.id
            );
        }
    }
    let dir = format!("local-{}", crate::mac_cmd::short_id());
    store.adopt(&scratch, &dir)?;
    let key = format!(
        "restored-{}-{}:latest",
        name.to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>(),
        &crate::mac_cmd::short_id()[..6]
    );
    mac_store::normalize(&key)?;
    store.add(
        &key,
        mac_store::Entry {
            dir,
            digest: String::new(),
            source: "backup".into(),
            added_unix: t,
        },
    )?;
    let image_disk = store.resolve(&key)?.join("disk.raw");
    let meta = m
        .instance
        .clone()
        .context("the manifest has no instance part")?;
    let publish: Vec<String> = meta
        .forwards
        .iter()
        .map(|f| format!("{}:{}", f.host, f.guest))
        .collect();
    let mut body = crate::mac_cmd::build_create_body(&name, &image_disk, &publish)?;
    for (k, v) in [("vcpus", meta.vcpus), ("memory_mib", meta.memory_mib)] {
        if let Some(v) = v {
            body[k] = json!(v);
        }
    }
    let undo = |store: &Store| {
        store.remove(&key).ok();
    };
    if let Some(old) = existing {
        let id = old["id"].as_str().context("the VM record has no id")?;
        if let Err(e) = d.delete(id).await {
            undo(store);
            return Err(e.context(format!(
                "could not remove the existing instance {name}; nothing was restored{}",
                safety
                    .as_ref()
                    .map(|s| format!(" (its automatic backup {s} stays)"))
                    .unwrap_or_default()
            )));
        }
    }
    let lost = |e: anyhow::Error| {
        e.context(match &safety {
            Some(s) => format!(
                "the restore failed after the old {name} was removed. Its state is saved as backup {s}: `fluxctl mac backup restore {s}` puts it back"
            ),
            None => format!("the restore of {name} failed"),
        })
    };
    let vm_id = d
        .create_vm(body)
        .await
        .map_err(|e| crate::mac_instance::explain(e, &format!("create {name}")))
        .map_err(|e| {
            undo(store);
            lost(e)
        })?;
    let mut labels = serde_json::Map::new();
    for (k, v) in &meta.labels {
        labels.insert(k.clone(), json!(v));
    }
    labels.insert(crate::mac_cmd::INSTANCE_LABEL.into(), json!("instance"));
    labels.insert("fluxvm.mac.image".into(), json!(key));
    labels.insert(RESTORED_FROM_LABEL.into(), json!(m.id));
    labels.insert(
        crate::mac_inspect::RESTART_LABEL.into(),
        json!(meta.restart_policy),
    );
    // Created, not started: a restart policy must not boot it by itself, as for `mac create`.
    labels.insert(
        crate::mac_lifecycle::NOT_STARTED_LABEL.into(),
        json!("true"),
    );
    if let Err(e) = d.patch_labels(&vm_id, Value::Object(labels)).await {
        d.delete(&vm_id).await.ok();
        undo(store);
        return Err(lost(e));
    }
    Ok(Restored {
        kind: Kind::Instance,
        name,
        backup: m.id.clone(),
        safety,
        vm_id: Some(vm_id),
    })
}

/// Restore a volume backup. `instances` are the daemon's instances (to refuse replacing a volume one of them uses).
#[allow(clippy::too_many_arguments)]
pub fn restore_volume(
    vs: &VolumeStore,
    instances: &[Value],
    root: &Path,
    m: &Manifest,
    replace: bool,
    as_name: Option<&str>,
    t: u64,
    free: &dyn Fn(&Path) -> Option<u64>,
) -> Result<Restored> {
    core::verify(root, m).map_err(coded_err)?;
    let name = target_name(m, as_name)?;
    let existing = vs.get(&name)?;
    let mut safety = None;
    if let Some(v) = &existing {
        if !replace {
            return Err(exists_error(Kind::Volume, &name, &m.id));
        }
        let refs: Vec<&Value> = instances.iter().collect();
        let used = vol::users(&refs, std::slice::from_ref(v))
            .remove(&name)
            .unwrap_or_default();
        vol::check_unused(&name, &used)?;
        let plan = core::volume_plan(&name, &v.data, &v.meta.labels).map_err(coded_err)?;
        let c = back_up(
            plan,
            root,
            core::AUTO_KEEP as u32,
            Some(format!("automatic, before restoring {}", m.id)),
            true,
            t,
            free,
        )?;
        safety = Some(c.manifest.id);
    }
    core::check_room_for(free(vs.root()), m.source_bytes, "this restore").map_err(coded_err)?;
    let tmp = vs
        .root()
        .join(format!(".restore-{}", crate::mac_cmd::short_id()));
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let result = (|| -> Result<()> {
        core::unpack(root, m, &tmp).map_err(coded_err)?;
        let fresh = tmp.join("data");
        if !fresh.is_dir() {
            bail!(
                "backup {} has no data folder: it is not a volume backup, so nothing was restored",
                m.id
            );
        }
        let labels = m
            .volume
            .as_ref()
            .map(|v| v.labels.clone())
            .unwrap_or_default();
        let (v, _) = vs.create(&name, labels)?;
        let old = tmp.join("old");
        std::fs::rename(&v.data, &old).context("setting the current data aside")?;
        if let Err(e) = std::fs::rename(&fresh, &v.data) {
            std::fs::rename(&old, &v.data).ok();
            return Err(anyhow::Error::new(e).context("moving the restored data into place"));
        }
        Ok(())
    })();
    std::fs::remove_dir_all(&tmp).ok();
    result?;
    Ok(Restored {
        kind: Kind::Volume,
        name,
        backup: m.id.clone(),
        safety,
        vm_id: None,
    })
}

pub fn restored_text(r: &Restored) -> String {
    let mut s = match r.kind {
        Kind::Instance => format!(
            "restored instance {} from {} (stopped). Start it: fluxctl mac start {}",
            r.name, r.backup, r.name
        ),
        Kind::Volume => format!(
            "restored volume {} from {}. Mount it with -v {}:/path",
            r.name, r.backup, r.name
        ),
    };
    if let Some(sf) = &r.safety {
        s.push_str(&format!(
            "\nwhat it replaced is saved as {sf} (automatic safety net). To go back: fluxctl mac backup restore {sf} --replace"
        ));
    }
    s
}

// ----------------------------------------------------------------------------------------------------------- list

pub fn state_word(e: &core::Entry) -> &'static str {
    match e.state {
        State::Ok => "ok",
        State::Running => "running",
        State::Failed => "failed",
        State::Damaged => "damaged",
    }
}

pub fn render_list(
    entries: &[core::Entry],
    sched: &[core::ScheduleEntry],
    free: Option<u64>,
    t: u64,
) -> String {
    if entries.is_empty() && sched.is_empty() {
        return "no backups yet. Make one: fluxctl mac backup create NAME   (an instance must be stopped; --volume NAME for a volume)\nAutomatic ones: fluxctl mac backup schedule NAME --every 24h\n".into();
    }
    let mut out = format!(
        "{:<44}  {:<8}  {:<16}  {:>10}  {:<9}  {:<7}  NOTE\n",
        "BACKUP", "KIND", "NAME", "SIZE", "TAKEN", "STATE"
    );
    for e in entries {
        let (kind, name, size, taken, note) = match &e.manifest {
            Some(m) => (
                m.kind.word(),
                m.name.as_str(),
                human_bytes(m.archive_bytes),
                crate::mac_checkpoint::age_text(t.saturating_sub(m.created_unix)),
                if m.automatic {
                    "automatic safety net".to_string()
                } else {
                    m.note.clone().unwrap_or_default()
                },
            ),
            None => ("-", "-", "-".into(), "-".into(), String::new()),
        };
        let note = match (&e.error, e.state) {
            (Some(err), State::Failed | State::Damaged) => {
                err.lines().next().unwrap_or("").to_string()
            }
            _ => note,
        };
        out.push_str(&format!(
            "{:<44}  {:<8}  {:<16}  {:>10}  {:<9}  {:<7}  {}\n",
            e.id,
            kind,
            name,
            size,
            taken,
            state_word(e),
            note
        ));
    }
    if let Some(f) = free {
        out.push_str(&format!(
            "free on the backups disk: {} (a backup needs its data's size plus 2 GiB free)\n",
            human_bytes(f)
        ));
    }
    for s in sched {
        out.push_str(&format!(
            "schedule: {} {} every {}, keep {}{}{}\n",
            s.kind.word(),
            s.name,
            core::every_text(s.every_secs),
            s.keep,
            if s.enabled { "" } else { " (off)" },
            s.last_error
                .as_ref()
                .map(|e| format!("; last run failed: {e}"))
                .unwrap_or_default()
        ));
    }
    out
}

fn entry_json(e: &core::Entry, t: u64) -> Value {
    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
    v["age_seconds"] = json!(
        e.manifest
            .as_ref()
            .map(|m| t.saturating_sub(m.created_unix))
    );
    v
}

// ----------------------------------------------------------------------------------------------------------- commands

fn print_json(v: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn kind_of(volume: bool) -> Kind {
    if volume { Kind::Volume } else { Kind::Instance }
}

fn schedule_cmd(
    root: &Path,
    name: Option<String>,
    volume: bool,
    every: Option<String>,
    keep: u32,
    off: bool,
    json_out: bool,
) -> Result<()> {
    let t = now();
    let Some(name) = name else {
        let s = core::load_schedules(root);
        if json_out {
            return print_json(&json!({"schema_version": 1, "schedules": s}));
        }
        if s.is_empty() {
            println!("no automatic backups. Set one: fluxctl mac backup schedule NAME --every 24h");
        }
        for e in &s {
            println!(
                "{} {} every {}, keep {}{}{}",
                e.kind.word(),
                e.name,
                core::every_text(e.every_secs),
                e.keep,
                if e.enabled { "" } else { " (off)" },
                e.last_error
                    .as_ref()
                    .map(|x| format!("; last run failed: {x}"))
                    .unwrap_or_default()
            );
        }
        return Ok(());
    };
    let kind = kind_of(volume);
    if off {
        let was = core::clear_schedule(root, kind, &name).map_err(coded_err)?;
        println!(
            "{}",
            if was {
                format!("stopped the automatic backups of {} {name}", kind.word())
            } else {
                format!("{} {name} had no schedule", kind.word())
            }
        );
        return Ok(());
    }
    let every = every.ok_or_else(|| {
        coded(
            format!("say how often: `fluxctl mac backup schedule {name} --every 24h` (or `--off`)"),
            "MAC_BACKUP_SCHEDULE",
        )
    })?;
    let secs = core::parse_every(&every).map_err(coded_err)?;
    let e = core::set_schedule(root, kind, &name, secs, keep, t).map_err(coded_err)?;
    if json_out {
        return print_json(&json!({"schema_version": 1, "schedule": e}));
    }
    println!(
        "{} {name} will be backed up every {}, keeping the newest {}.\nThe daemon takes what is due each minute (a running instance is skipped until the next interval); without a daemon run `fluxctl mac backup run-due` from cron.",
        kind.word(),
        core::every_text(secs),
        e.keep
    );
    Ok(())
}

fn verify_cmd(root: &Path, backup: Option<String>, all: bool, json_out: bool) -> Result<()> {
    let t = now();
    let list: Vec<Manifest> = if all {
        core::list(root, t)
            .into_iter()
            .filter(|e| e.state == State::Ok)
            .filter_map(|e| e.manifest)
            .collect()
    } else {
        let spec =
            backup.context("name a backup (an id from `fluxctl mac backup ls`), or use --all")?;
        vec![core::resolve(root, &spec, t).map_err(coded_err)?]
    };
    let mut bad = Vec::new();
    let mut docs = Vec::new();
    for m in &list {
        match core::verify(root, m) {
            Ok(r) => {
                if json_out {
                    docs.push(json!({"id": m.id, "ok": true, "report": r}));
                } else {
                    println!(
                        "{}: ok (sha256 matches, {} entries, {} unpacked)",
                        m.id,
                        r.entries,
                        human_bytes(r.unpacked_bytes)
                    );
                }
            }
            Err(e) => {
                if json_out {
                    docs.push(json!({"id": m.id, "ok": false, "code": e.code, "error": e.message}));
                } else {
                    println!("{}: DAMAGED\n  {}", m.id, e.message);
                }
                bad.push(coded_err(e));
            }
        }
    }
    if json_out {
        print_json(&json!({"schema_version": 1, "results": docs}))?;
    } else if list.is_empty() {
        println!("no backups to verify");
    }
    match bad.len() {
        0 => Ok(()),
        1 => Err(bad.remove(0)),
        n => bail!("{n} backups failed verification (see above)"),
    }
}

/// Commands that need no daemon.
pub fn run_client(cmd: MacBackup) -> Result<()> {
    let MacBackup::Backup { command } = cmd;
    let root = root()?;
    let t = now();
    match command {
        BackupCmd::Ls { json } => {
            let (entries, sched) = (core::list(&root, t), core::load_schedules(&root));
            let free = core::free_bytes(&root);
            if json {
                let docs: Vec<Value> = entries.iter().map(|e| entry_json(e, t)).collect();
                return print_json(&json!({
                    "schema_version": 1, "root": root, "free_bytes": free,
                    "reserve_bytes": core::RESERVE_BYTES,
                    "backups": docs, "schedules": sched,
                }));
            }
            print!("{}", render_list(&entries, &sched, free, t));
            Ok(())
        }
        BackupCmd::Rm { backups } => {
            let mut failed = 0;
            for spec in backups {
                // An id removes exactly that backup; a name is refused, so no one deletes the newest by a typo.
                match core::remove(&root, &spec) {
                    Ok(()) => println!("removed {spec}"),
                    Err(e) => {
                        failed += 1;
                        eprintln!("{:#}", coded_err(e));
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} backup(s) were not removed");
            }
            Ok(())
        }
        BackupCmd::Verify { backup, all, json } => verify_cmd(&root, backup, all, json),
        BackupCmd::Schedule {
            name,
            volume,
            every,
            keep,
            off,
            json,
        } => schedule_cmd(&root, name, volume, every, keep, off, json),
        BackupCmd::Create {
            name,
            volume: true,
            keep,
            note,
            json,
        } => {
            let vs = VolumeStore::open_default()?;
            let v = vs.require(&name)?;
            let plan = core::volume_plan(&name, &v.data, &v.meta.labels).map_err(coded_err)?;
            let c = back_up(plan, &root, keep, note, false, t, &core::free_bytes)?;
            if json {
                return print_json(
                    &json!({"schema_version": 1, "backup": c.manifest, "pruned": c.pruned}),
                );
            }
            println!("{}", created_text(&c));
            println!(
                "note: without the daemon this could not check whether a running instance is writing to the volume; stop it first for a consistent copy"
            );
            Ok(())
        }
        BackupCmd::Create { .. } | BackupCmd::Restore { .. } | BackupCmd::RunDue { .. } => {
            bail!(
                "`fluxctl mac backup create|restore|run-due` of an instance need the daemon: pass --server (or select a context)"
            )
        }
    }
}

/// Commands that read or change instances.
pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacBackup) -> Result<()> {
    let MacBackup::Backup { command } = cmd;
    let root = root()?;
    let t = now();
    match command {
        BackupCmd::Create {
            name,
            volume: false,
            keep,
            note,
            json,
        } => {
            let all = r.instances().await?;
            let vm = named(&all, &name).ok_or_else(|| no_instance(&name))?;
            let plan = core::instance_plan(vm).map_err(coded_err)?;
            let c = back_up(plan, &root, keep, note, false, t, &core::free_bytes)?;
            if json {
                return print_json(
                    &json!({"schema_version": 1, "backup": c.manifest, "pruned": c.pruned}),
                );
            }
            println!("{}", created_text(&c));
            Ok(())
        }
        BackupCmd::Restore {
            backup,
            replace,
            as_name,
            json,
        } => {
            let m = core::resolve(&root, &backup, t).map_err(coded_err)?;
            let rest = match m.kind {
                Kind::Instance => {
                    let store = Store::open(mac_store::default_root()?)?;
                    restore_instance(
                        r,
                        &store,
                        &root,
                        &m,
                        replace,
                        as_name.as_deref(),
                        t,
                        &core::free_bytes,
                    )
                    .await?
                }
                Kind::Volume => {
                    let vs = VolumeStore::open_default()?;
                    let all = r.instances().await?;
                    restore_volume(
                        &vs,
                        &all,
                        &root,
                        &m,
                        replace,
                        as_name.as_deref(),
                        t,
                        &core::free_bytes,
                    )?
                }
            };
            if json {
                return print_json(&json!({
                    "schema_version": 1, "kind": rest.kind.word(), "name": rest.name,
                    "backup": rest.backup, "safety_backup": rest.safety, "id": rest.vm_id,
                }));
            }
            println!("{}", restored_text(&rest));
            Ok(())
        }
        BackupCmd::RunDue { json } => {
            let all = r.instances().await?;
            let vs = VolumeStore::open_default()?;
            let plan_for = |k: Kind, n: &str| -> core::Result<core::Plan> {
                match k {
                    Kind::Instance => match named(&all, n) {
                        Some(vm) => core::instance_plan(vm),
                        None => Err(BackupError {
                            code: "MAC_BACKUP_SOURCE",
                            message: format!("no instance named {n:?} on this daemon"),
                        }),
                    },
                    Kind::Volume => match vs.get(n) {
                        Ok(Some(v)) => core::volume_plan(n, &v.data, &v.meta.labels),
                        _ => Err(BackupError {
                            code: "MAC_BACKUP_SOURCE",
                            message: format!("no volume named {n:?}"),
                        }),
                    },
                }
            };
            let res = core::run_due(&root, t, &core::free_bytes, &plan_for);
            let mut failed = 0;
            let mut docs = Vec::new();
            for x in &res {
                match &x.outcome {
                    Ok(c) => {
                        docs.push(json!({"kind": x.kind.word(), "name": x.name, "ok": true, "backup": c.manifest.id}));
                        if !json {
                            println!(
                                "backed up {} {} as {}",
                                x.kind.word(),
                                x.name,
                                c.manifest.id
                            );
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        docs.push(json!({"kind": x.kind.word(), "name": x.name, "ok": false, "code": e.code, "error": e.message}));
                        if !json {
                            eprintln!("{} {}: {}", x.kind.word(), x.name, e.message);
                        }
                    }
                }
            }
            if json {
                print_json(&json!({"schema_version": 1, "results": docs}))?;
            } else if res.is_empty() {
                println!("nothing is due");
            }
            if failed > 0 {
                bail!(
                    "{failed} scheduled backup(s) failed; see above (`fluxctl mac backup ls` shows the last error per schedule)"
                );
            }
            Ok(())
        }
        other => run_client(MacBackup::Backup { command: other }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::sync::Mutex;

    #[derive(Parser)]
    struct T {
        #[command(subcommand)]
        c: MacBackup,
    }

    fn parse(a: &[&str]) -> MacBackup {
        let mut v = vec!["t"];
        v.extend(a);
        T::try_parse_from(v).unwrap().c
    }

    fn plenty(_: &Path) -> Option<u64> {
        Some(1 << 40)
    }

    #[derive(Default)]
    struct Fake {
        vms: Mutex<Vec<Value>>,
        log: Mutex<Vec<String>>,
        fail_create: bool,
        store_root: PathBuf,
    }

    impl Daemon for Fake {
        async fn instances(&self) -> Result<Vec<Value>> {
            Ok(self.vms.lock().unwrap().clone())
        }
        async fn create_vm(&self, body: Value) -> Result<String> {
            self.log
                .lock()
                .unwrap()
                .push(format!("create {}", body["name"]));
            if self.fail_create {
                bail!("500 boom");
            }
            // The disk the daemon clones must exist when it is asked to.
            assert!(Path::new(body["image"].as_str().unwrap()).is_file());
            let _ = &self.store_root;
            let id = format!("vm-{}", self.vms.lock().unwrap().len() + 1);
            self.vms.lock().unwrap().push(json!({"id": id, "name": body["name"], "status": "stopped", "labels": {"fluxvm.mac": "instance"}, "body": body}));
            Ok(id)
        }
        async fn patch_labels(&self, id: &str, labels: Value) -> Result<()> {
            self.log.lock().unwrap().push(format!("patch {id}"));
            for v in self.vms.lock().unwrap().iter_mut() {
                if v["id"] == id {
                    v["labels"] = labels.clone();
                }
            }
            Ok(())
        }
        async fn delete(&self, id: &str) -> Result<()> {
            self.log.lock().unwrap().push(format!("delete {id}"));
            self.vms.lock().unwrap().retain(|v| v["id"] != id);
            Ok(())
        }
    }

    fn vm(dir: &Path, name: &str, status: &str) -> Value {
        let ws = dir.join(format!("ws-{name}"));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("disk.raw"), name.repeat(2000)).unwrap();
        std::fs::write(ws.join("hardware.bin"), b"hw").unwrap();
        std::fs::write(ws.join("auxiliary.bin"), b"aux").unwrap();
        json!({"id": format!("id-{name}"), "name": name, "status": status,
            "disk": ws.join("disk.raw"), "workspace": ws,
            "labels": {"fluxvm.mac": "instance", "fluxvm.mac.restart": "unless-stopped", "team": "ios"},
            "request": {"vcpus": 6, "memory_mib": 12288, "network": {"forwards": [{"host_port": 2200, "guest_port": 22}]}}})
    }

    #[test]
    fn clap_surface_and_which_commands_need_the_daemon() {
        assert!(parse(&["backup", "ls"]).is_client_side());
        assert!(parse(&["backup", "verify", "--all"]).is_client_side());
        assert!(parse(&["backup", "rm", "a", "b"]).is_client_side());
        assert!(parse(&["backup", "schedule", "ci", "--every", "24h"]).is_client_side());
        assert!(parse(&["backup", "create", "--volume", "v"]).is_client_side());
        assert!(!parse(&["backup", "create", "ci"]).is_client_side());
        assert!(!parse(&["backup", "restore", "ci", "--replace", "--as", "x"]).is_client_side());
        assert!(!parse(&["backup", "run-due"]).is_client_side());
        assert!(T::try_parse_from(["t", "backup", "rm"]).is_err());
        let MacBackup::Backup {
            command: BackupCmd::Create { keep, .. },
        } = parse(&["backup", "create", "ci"])
        else {
            panic!()
        };
        assert_eq!(keep, 7);
    }

    #[tokio::test]
    async fn instance_round_trip_restores_settings_and_refuses_to_overwrite() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("backups");
        let store = Store::open(d.path().join("store")).unwrap();
        let src = vm(d.path(), "ci", "stopped");
        let fake = Fake {
            vms: Mutex::new(vec![src.clone()]),
            ..Default::default()
        };
        let c = back_up(
            core::instance_plan(&src).unwrap(),
            &root,
            7,
            None,
            false,
            1000,
            &plenty,
        )
        .unwrap();
        let text = created_text(&c);
        assert!(
            text.contains("NOT in it")
                && text.contains("registry logins")
                && text.contains("secret"),
            "{text}"
        );
        // 1. the instance still exists: refused, with both ways out
        let e = restore_instance(
            &fake,
            &store,
            &root,
            &c.manifest,
            false,
            None,
            2000,
            &plenty,
        )
        .await
        .unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("--replace") && e.contains("--as") && e.contains("MAC_BACKUP_EXISTS"),
            "{e}"
        );
        assert!(fake.log.lock().unwrap().is_empty(), "nothing was touched");
        // 2. under another name it is created with the settings of the manifest
        let r = restore_instance(
            &fake,
            &store,
            &root,
            &c.manifest,
            false,
            Some("ci2"),
            2000,
            &plenty,
        )
        .await
        .unwrap();
        assert_eq!(r.name, "ci2");
        let vms = fake.vms.lock().unwrap().clone();
        let new = vms.iter().find(|v| v["name"] == "ci2").unwrap();
        assert_eq!(new["body"]["vcpus"], 6);
        assert_eq!(new["body"]["memory_mib"], 12288);
        assert_eq!(new["body"]["network"]["forwards"][0]["host_port"], 2200);
        assert_eq!(new["labels"]["fluxvm.mac.restart"], "unless-stopped");
        assert_eq!(new["labels"]["team"], "ios");
        assert_eq!(
            new["labels"]["fluxvm.mac.restored-from"],
            c.manifest.id.as_str()
        );
        assert_eq!(new["labels"]["fluxvm.mac.notstarted"], "true");
    }

    #[tokio::test]
    async fn replace_saves_the_old_instance_first_and_a_bad_checksum_touches_nothing() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("backups");
        let store = Store::open(d.path().join("store")).unwrap();
        let src = vm(d.path(), "ci", "stopped");
        let fake = Fake {
            vms: Mutex::new(vec![src.clone()]),
            ..Default::default()
        };
        let c = back_up(
            core::instance_plan(&src).unwrap(),
            &root,
            7,
            None,
            false,
            1000,
            &plenty,
        )
        .unwrap();
        // damaged archive first
        let good = std::fs::read(root.join(&c.manifest.archive)).unwrap();
        let mut bad = good.clone();
        let n = bad.len() / 2;
        bad[n] ^= 1;
        std::fs::write(root.join(&c.manifest.archive), &bad).unwrap();
        let e = restore_instance(&fake, &store, &root, &c.manifest, true, None, 2000, &plenty)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("MAC_BACKUP_CHECKSUM"), "{e:#}");
        assert!(
            fake.log.lock().unwrap().is_empty(),
            "nothing was deleted or created"
        );
        std::fs::write(root.join(&c.manifest.archive), good).unwrap();
        // a running instance cannot be replaced (its disk is changing)
        fake.vms.lock().unwrap()[0]["status"] = json!("running");
        let e = restore_instance(&fake, &store, &root, &c.manifest, true, None, 2000, &plenty)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("fluxctl mac stop ci"), "{e:#}");
        fake.vms.lock().unwrap()[0]["status"] = json!("stopped");
        // now it works: old instance backed up as automatic, deleted, new one created
        let r = restore_instance(&fake, &store, &root, &c.manifest, true, None, 3000, &plenty)
            .await
            .unwrap();
        let safety = r.safety.clone().expect("a safety backup");
        let auto = core::resolve(&root, &safety, 4000).unwrap();
        assert!(auto.automatic && auto.name == "ci");
        assert_eq!(
            fake.log.lock().unwrap().as_slice(),
            ["delete id-ci", "create \"ci\"", "patch vm-1"]
        );
        assert!(restored_text(&r).contains(&safety));
    }

    #[tokio::test]
    async fn a_failed_create_after_the_delete_points_at_the_safety_backup() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("backups");
        let store = Store::open(d.path().join("store")).unwrap();
        let src = vm(d.path(), "ci", "stopped");
        let fake = Fake {
            vms: Mutex::new(vec![src.clone()]),
            fail_create: true,
            ..Default::default()
        };
        let c = back_up(
            core::instance_plan(&src).unwrap(),
            &root,
            7,
            None,
            false,
            1000,
            &plenty,
        )
        .unwrap();
        let e = restore_instance(&fake, &store, &root, &c.manifest, true, None, 2000, &plenty)
            .await
            .unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("is saved as backup automatic")
                || e.contains("is saved as backup instance-ci"),
            "{e}"
        );
        assert!(
            store.list().unwrap().is_empty(),
            "the hidden image is dropped"
        );
    }

    #[test]
    fn low_disk_is_refused_in_plain_words_with_the_amount() {
        let d = tempfile::tempdir().unwrap();
        let src = vm(d.path(), "ci", "stopped");
        let e = back_up(
            core::instance_plan(&src).unwrap(),
            &d.path().join("b"),
            7,
            None,
            false,
            1,
            &|_| Some(100 << 20),
        )
        .unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("not enough free disk")
                && e.contains("100.0 MiB is free")
                && e.contains("fluxctl explain MAC_BACKUP_NO_ROOM"),
            "{e}"
        );
    }

    #[test]
    fn volume_round_trip_replace_and_in_use() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("backups");
        let vs = VolumeStore::at(d.path().join("vols"));
        let (v, _) = vs
            .create("cache", [("k".to_string(), "v".to_string())].into())
            .unwrap();
        std::fs::write(v.data.join("a.txt"), b"one").unwrap();
        let c = back_up(
            core::volume_plan("cache", &v.data, &v.meta.labels).unwrap(),
            &root,
            7,
            None,
            false,
            1000,
            &plenty,
        )
        .unwrap();
        std::fs::write(v.data.join("a.txt"), b"two").unwrap();
        std::fs::write(v.data.join("b.txt"), b"extra").unwrap();
        // exists: refused
        let e =
            restore_volume(&vs, &[], &root, &c.manifest, false, None, 2000, &plenty).unwrap_err();
        assert!(format!("{e:#}").contains("MAC_BACKUP_EXISTS"));
        // in use by an instance: refused even with --replace
        let user = json!({"name": "web", "request": {"shared_folders": [{"host_path": v.data}]}});
        let e = restore_volume(&vs, &[user], &root, &c.manifest, true, None, 2000, &plenty)
            .unwrap_err();
        assert!(format!("{e:#}").contains("MAC_VOLUME_IN_USE"), "{e:#}");
        // replace: data is back, the newer state is an automatic backup
        let r = restore_volume(&vs, &[], &root, &c.manifest, true, None, 3000, &plenty).unwrap();
        assert_eq!(std::fs::read(v.data.join("a.txt")).unwrap(), b"one");
        assert!(!v.data.join("b.txt").exists());
        let auto = core::resolve(&root, r.safety.as_ref().unwrap(), 4000).unwrap();
        assert!(auto.automatic);
        // as a new name
        let r2 = restore_volume(
            &vs,
            &[],
            &root,
            &c.manifest,
            false,
            Some("cache2"),
            3500,
            &plenty,
        )
        .unwrap();
        assert_eq!(r2.name, "cache2");
        assert_eq!(
            std::fs::read(vs.data_path("cache2").join("a.txt")).unwrap(),
            b"one"
        );
        assert_eq!(vs.get("cache2").unwrap().unwrap().meta.labels["k"], "v");
        assert!(
            std::fs::read_dir(vs.root())
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().starts_with(".restore")),
            "no temp folder is left"
        );
    }

    #[test]
    fn list_text_shows_size_state_free_space_and_schedules() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("backups");
        let src = vm(d.path(), "ci", "stopped");
        back_up(
            core::instance_plan(&src).unwrap(),
            &root,
            7,
            None,
            false,
            1000,
            &plenty,
        )
        .unwrap();
        core::set_schedule(&root, Kind::Instance, "ci", 86400, 5, 1000).unwrap();
        let t = text_of(&root, 1000 + 7200);
        assert!(
            t.contains("instance")
                && t.contains("2h ago")
                && t.contains("ok")
                && t.contains("free on the backups disk: 10.0 GiB")
                && t.contains("schedule: instance ci every 1d, keep 5"),
            "{t}"
        );
        assert!(render_list(&[], &[], None, 0).contains("no backups yet"));
    }

    fn text_of(root: &Path, t: u64) -> String {
        render_list(
            &core::list(root, t),
            &core::load_schedules(root),
            Some(10 << 30),
            t,
        )
    }

    #[test]
    fn schedule_command_validates_and_toggles() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("b");
        schedule_cmd(
            &root,
            Some("ci".into()),
            false,
            Some("12h".into()),
            3,
            false,
            false,
        )
        .unwrap();
        assert_eq!(core::load_schedules(&root)[0].every_secs, 12 * 3600);
        let e = schedule_cmd(
            &root,
            Some("ci".into()),
            false,
            Some("often".into()),
            3,
            false,
            false,
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("MAC_BACKUP_SCHEDULE"));
        let e = schedule_cmd(&root, Some("ci".into()), false, None, 3, false, false).unwrap_err();
        assert!(format!("{e:#}").contains("--every 24h"));
        schedule_cmd(&root, Some("ci".into()), false, None, 3, true, false).unwrap();
        assert!(core::load_schedules(&root).is_empty());
    }

    #[test]
    fn verify_command_reports_a_damaged_backup_as_an_error() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("backups");
        let src = vm(d.path(), "ci", "stopped");
        let c = back_up(
            core::instance_plan(&src).unwrap(),
            &root,
            7,
            None,
            false,
            1000,
            &plenty,
        )
        .unwrap();
        verify_cmd(&root, Some(c.manifest.id.clone()), false, false).unwrap();
        std::fs::write(root.join(&c.manifest.archive), b"garbage").unwrap();
        let e = verify_cmd(&root, None, true, false).unwrap_err();
        assert!(format!("{e:#}").contains("MAC_BACKUP_CHECKSUM"), "{e:#}");
    }

    #[test]
    fn a_damaged_volume_backup_is_refused_before_the_volume_is_touched() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("backups");
        let vs = VolumeStore::at(d.path().join("vols"));
        let (v, _) = vs.create("cache", Default::default()).unwrap();
        std::fs::write(v.data.join("a.txt"), b"keep me").unwrap();
        let c = back_up(
            core::volume_plan("cache", &v.data, &v.meta.labels).unwrap(),
            &root,
            7,
            None,
            false,
            1000,
            &plenty,
        )
        .unwrap();
        std::fs::write(v.data.join("a.txt"), b"current").unwrap();
        let mut bytes = std::fs::read(root.join(&c.manifest.archive)).unwrap();
        let n = bytes.len() / 2;
        bytes[n] ^= 1;
        std::fs::write(root.join(&c.manifest.archive), bytes).unwrap();
        let e =
            restore_volume(&vs, &[], &root, &c.manifest, true, None, 2000, &plenty).unwrap_err();
        assert!(format!("{e:#}").contains("MAC_BACKUP_CHECKSUM"), "{e:#}");
        assert_eq!(std::fs::read(v.data.join("a.txt")).unwrap(), b"current");
        assert_eq!(
            core::list(&root, 3000).len(),
            1,
            "no automatic backup was taken either"
        );
    }
}
