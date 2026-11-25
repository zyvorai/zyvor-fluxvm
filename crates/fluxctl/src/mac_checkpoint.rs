// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Named checkpoints of a macOS instance: `mac checkpoint`, `mac checkpoints`, `mac checkpoint restore|rm`, `mac diff`, and
//! `mac run --from-checkpoint` (docs/macos-containers.md, "Checkpoints").
//!
//! This is a friendly layer over the snapshot code the daemon already has, not a second mechanism. A checkpoint is a snapshot
//! under `<workspace>/snapshots/<tag>/` (`fluxvm_apple::snapshot_save`: the guest's memory and devices plus an APFS clone of its
//! disk, or a disk-only clone), listed and removed through `GET|DELETE /v1/vms/{id}/snapshots`, taken with
//! `POST /v1/vms/{id}/snapshot` and put back with `POST /v1/vms/{id}/restore`. The safety net lives in the daemon, so Studio gets
//! it too: a restore first saves the current state as `auto-before-restore-<time>` (the newest three are kept) and the answer
//! names it, so every restore says how to roll back.
//!
//! Host safety is checked by the daemon before the save: the free-disk check (`host_health::checkpoint_space_blocker`) and the
//! runner's own `save-support`; a restore keeps the locked-session and macOS-build checks it already had.
//!
//! Not run against a real daemon or a macOS VM; the tests use a fake of [`Api`].

use crate::mac_errors::{CODES, MacCode, coded};
use crate::mac_inspect::{human_bytes, status as vm_status};
use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub use fluxvm_apple::is_auto_checkpoint;

#[derive(Subcommand, Debug)]
pub enum MacCkpt {
    /// Save a named checkpoint of an instance: its memory and disk when it runs (instant to come back to), or its disk when it is
    /// stopped. `checkpoint rm`, `checkpoint restore` and `mac diff` work on the names it prints.
    Checkpoint(CheckpointArgs),
    /// List checkpoints with their kind, size and age: of one instance, or of every instance.
    Checkpoints {
        /// Only this instance's checkpoints.
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Compare a checkpoint's disk with the instance's disk now (or with a second checkpoint): how much of it changed. Reads both
    /// disks once, so a large disk takes a while. Needs to run on the Mac that hosts the instance.
    #[command(
        after_help = "Examples:\n  fluxctl mac diff ci warm\n  fluxctl mac diff ci warm before-upgrade\n\nThe comparison is by 1 MiB block of the disk image, so it says how much changed, not which files. Memory is not compared. A running guest keeps writing, so its numbers move; stop it first for an exact answer. If it fails: fluxctl explain MAC_CHECKPOINT_NOT_LOCAL"
    )]
    Diff {
        name: String,
        /// The checkpoint to compare.
        tag: String,
        /// Compare with this checkpoint instead of the current disk.
        other: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug)]
#[command(
    args_conflicts_with_subcommands = true,
    after_help = "Examples:\n  fluxctl mac checkpoint ci warm                  save ci as 'warm' (no name: ckpt-<time>)\n  fluxctl mac checkpoints                         what exists, with size and age\n  fluxctl mac run --from-checkpoint ci:warm       come back to it, instantly when it holds memory\n  fluxctl mac checkpoint restore ci warm          the same without the run\n  fluxctl mac checkpoint rm ci warm\n  fluxctl mac diff ci warm\n\nA running instance is saved with its memory (it pauses for a moment, then carries on); a stopped one, or --disk-only, saves the disk and comes back by booting. Restoring first saves where you are now as auto-before-restore-<time> and tells you how to roll back (--no-safety-checkpoint skips that). The disk part is an APFS clone, so it costs little until the instance and the checkpoint diverge. An instance cannot be called 'rm' or 'restore' here. If it fails: fluxctl explain MAC_CHECKPOINT_EXISTS"
)]
pub struct CheckpointArgs {
    /// The instance (`fluxctl mac ps -a`).
    pub name: Option<String>,
    /// The checkpoint's name: letters, digits, `.`, `_`, `-` (default: ckpt-<UTC time>).
    pub tag: Option<String>,
    /// Save the disk only: no memory, so coming back boots the guest (a macOS boot takes minutes) but nothing needs the runner or
    /// an unlocked screen.
    #[arg(long)]
    pub disk_only: bool,
    #[arg(long)]
    pub json: bool,
    #[command(subcommand)]
    pub sub: Option<CheckpointSub>,
}

#[derive(Subcommand, Debug)]
pub enum CheckpointSub {
    /// Delete a checkpoint (its memory and its disk clone).
    Rm {
        name: String,
        tag: String,
        #[arg(long)]
        json: bool,
    },
    /// Put an instance back to a checkpoint: resumes where it was when the checkpoint holds memory, otherwise boots from its disk.
    /// The state you leave is saved first as `auto-before-restore-<time>`.
    Restore {
        name: String,
        tag: String,
        /// Do not save the current state first. Whatever changed since the checkpoint is lost.
        #[arg(long)]
        no_safety_checkpoint: bool,
        #[arg(long)]
        json: bool,
    },
}

// ----------------------------------------------------------------- pure parts

/// One checkpoint as the daemon lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub tag: String,
    pub created_unix: Option<u64>,
    pub size_bytes: u64,
    /// `None` when the daemon did not say.
    pub has_memory: Option<bool>,
}

impl Checkpoint {
    pub fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            tag: v["tag"].as_str()?.to_string(),
            created_unix: v["created_at"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.timestamp().max(0) as u64),
            size_bytes: v["size_bytes"].as_u64().unwrap_or(0),
            has_memory: v["has_memory"].as_bool(),
        })
    }

    pub fn automatic(&self) -> bool {
        is_auto_checkpoint(&self.tag)
    }

    /// `memory and disk`, `disk only`, or `-` when unknown.
    pub fn kind(&self) -> &'static str {
        match self.has_memory {
            Some(true) => "memory and disk",
            Some(false) => "disk only",
            None => "-",
        }
    }

    pub fn to_json(&self, now: u64) -> Value {
        json!({
            "tag": self.tag,
            "kind": self.kind(),
            "has_memory": self.has_memory,
            "automatic": self.automatic(),
            "size_bytes": self.size_bytes,
            "created_unix": self.created_unix,
            "age_seconds": self.created_unix.map(|c| now.saturating_sub(c)),
        })
    }
}

/// `ckpt-20261123-101500`: the name used when none is given.
pub fn default_tag(now_unix: u64) -> String {
    format!("ckpt-{}", stamp(now_unix))
}

fn stamp(now_unix: u64) -> String {
    chrono::DateTime::from_timestamp(now_unix as i64, 0)
        .unwrap_or_default()
        .format("%Y%m%d-%H%M%S")
        .to_string()
}

/// The daemon's rule for a tag, checked here first so the message can say what to do.
pub fn check_tag(tag: &str) -> Result<()> {
    if tag.is_empty()
        || tag.len() > 128
        || tag.starts_with('.')
        || !tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(coded(
            format!(
                "{tag:?} is not a usable checkpoint name: use 1 to 128 letters, digits, '.', '_' or '-', not starting with '.'. Example: `fluxctl mac checkpoint ci before-upgrade`"
            ),
            "MAC_CHECKPOINT_TAG",
        ));
    }
    Ok(())
}

/// `NAME:TAG` splits at the last colon; a bare word is a tag with no instance named.
pub fn parse_ref(s: &str) -> Result<(Option<String>, String)> {
    let s = s.trim();
    ensure!(
        !s.is_empty(),
        "--from-checkpoint needs a checkpoint: NAME:TAG (see `fluxctl mac checkpoints`)"
    );
    match s.rsplit_once(':') {
        Some((n, t)) if !n.is_empty() && !t.is_empty() => Ok((Some(n.to_string()), t.to_string())),
        Some(_) => bail!(
            "{s:?} is not NAME:TAG. Give both (`fluxctl mac run --from-checkpoint ci:warm`), or just the TAG when only one instance has it"
        ),
        None => Ok((None, s.to_string())),
    }
}

/// `90s`-style age: `just now`, `5m ago`, `3h ago`, `2d ago`.
pub fn age_text(secs: u64) -> String {
    match secs {
        0..=44 => "just now".into(),
        45..=3599 => format!("{}m ago", secs.div_ceil(60).max(1)),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// The `mac checkpoints` table for one instance.
pub fn render_list(name: &str, list: &[Checkpoint], now: u64) -> String {
    let mut out = format!(
        "{:<28}  {:<15}  {:>10}  {:<9}  NOTE\n",
        "CHECKPOINT", "KIND", "SIZE", "TAKEN"
    );
    for c in list {
        out.push_str(&format!(
            "{:<28}  {:<15}  {:>10}  {:<9}  {}\n",
            format!("{name}:{}", c.tag),
            c.kind(),
            human_bytes(c.size_bytes),
            c.created_unix
                .map_or("-".to_string(), |t| age_text(now.saturating_sub(t))),
            if c.automatic() {
                "automatic safety net (newest 3 kept)"
            } else {
                ""
            }
        ));
    }
    out
}

pub const SIZE_NOTE: &str = "Sizes are the files' full sizes: the disk part is an APFS clone that shares storage with the live disk until they diverge, so it costs far less than shown.";

/// What to tell a person after a restore saved their previous state.
pub fn rollback_note(name: &str, safety: &str, with_memory: bool) -> String {
    format!(
        "your state from before the restore is saved as {name}:{safety} ({}). To go back: fluxctl mac checkpoint restore {name} {safety}",
        if with_memory {
            "memory and disk, so rolling back resumes it where it was"
        } else {
            "disk only, so rolling back boots it"
        }
    )
}

/// One file's comparison in `mac diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub file: String,
    pub size_a: u64,
    pub size_b: u64,
    pub block: u64,
    pub blocks_compared: u64,
    pub blocks_changed: u64,
}

impl FileDiff {
    /// Bytes in blocks that differ (an upper bound: blocks are 1 MiB).
    pub fn changed_bytes(&self) -> u64 {
        self.blocks_changed
            .saturating_mul(self.block)
            .min(self.size_a.max(self.size_b))
    }

    pub fn percent(&self) -> f64 {
        if self.blocks_compared == 0 {
            0.0
        } else {
            100.0 * self.blocks_changed as f64 / self.blocks_compared as f64
        }
    }
}

const DIFF_BLOCK: usize = 1 << 20;

fn fill(f: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    use std::io::Read;
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// Compares two files block by block. A block present in only one file (the sizes differ) counts as changed.
pub fn diff_files(a: &Path, b: &Path, label: &str) -> Result<FileDiff> {
    let mut fa = std::fs::File::open(a).with_context(|| format!("opening {}", a.display()))?;
    let mut fb = std::fs::File::open(b).with_context(|| format!("opening {}", b.display()))?;
    let (size_a, size_b) = (fa.metadata()?.len(), fb.metadata()?.len());
    let (mut ba, mut bb) = (vec![0u8; DIFF_BLOCK], vec![0u8; DIFF_BLOCK]);
    let (mut compared, mut changed) = (0u64, 0u64);
    loop {
        let (na, nb) = (fill(&mut fa, &mut ba)?, fill(&mut fb, &mut bb)?);
        if na == 0 && nb == 0 {
            break;
        }
        compared += 1;
        if na != nb || ba[..na] != bb[..nb] {
            changed += 1;
        }
    }
    Ok(FileDiff {
        file: label.to_string(),
        size_a,
        size_b,
        block: DIFF_BLOCK as u64,
        blocks_compared: compared,
        blocks_changed: changed,
    })
}

pub fn render_diff(name: &str, a: &str, b: &str, files: &[FileDiff], running: bool) -> String {
    let mut out = format!("{name}: {a}  vs  {b}\n");
    for f in files {
        if f.blocks_changed == 0 && f.size_a == f.size_b {
            out.push_str(&format!(
                "  {:<20} identical ({})\n",
                f.file,
                human_bytes(f.size_a)
            ));
        } else {
            out.push_str(&format!(
                "  {:<20} {} of {} differ ({:.1}%){}\n",
                f.file,
                human_bytes(f.changed_bytes()),
                human_bytes(f.size_a.max(f.size_b)),
                f.percent(),
                if f.size_a != f.size_b {
                    format!(
                        ", sizes {} and {}",
                        human_bytes(f.size_a),
                        human_bytes(f.size_b)
                    )
                } else {
                    String::new()
                }
            ));
        }
    }
    out.push_str("compared by 1 MiB block of the disk image: how much changed, not which files; memory is not compared\n");
    if running {
        out.push_str("the instance is running and keeps writing, so these numbers move; stop it for an exact answer\n");
    }
    out
}

/// The code table entries of this module, for the error mapper.
fn checkpoint_hint(message: &str) -> Option<&'static MacCode> {
    let m = message.to_ascii_lowercase();
    CODES
        .iter()
        .filter(|c| c.code.starts_with("MAC_CHECKPOINT") || c.code == "MAC_FROM_CHECKPOINT")
        .find(|c| c.signatures.iter().any(|s| m.contains(s)))
}

/// Make a daemon failure readable: our own codes first, then the generic 409/503/404 explanations.
pub fn explain_daemon(e: anyhow::Error, action: &str) -> anyhow::Error {
    let t = format!("{e:#}");
    if let Some(c) = checkpoint_hint(&t) {
        return anyhow::anyhow!("cannot {action}: {t}\n  more: fluxctl explain {}", c.code);
    }
    if let Some(c) = crate::mac_errors::hint_for(&t)
        && c.code == "MAC_DISK_FULL"
    {
        return anyhow::anyhow!("cannot {action}: {t}\n  more: fluxctl explain {}", c.code);
    }
    crate::mac_instance::explain(e, action)
}

// ----------------------------------------------------------------- the daemon

/// What the operations need from the daemon; a fake in tests.
#[allow(async_fn_in_trait)]
pub trait Api {
    /// Every instance (VM records labelled `fluxvm.mac=instance`).
    async fn instances(&self) -> Result<Vec<Value>>;
    async fn list(&self, id: &str) -> Result<Vec<Checkpoint>>;
    async fn save(&self, id: &str, tag: &str, memory: bool) -> Result<()>;
    async fn delete(&self, id: &str, tag: &str) -> Result<()>;
    /// `POST /v1/vms/{id}/restore`: the daemon's answer (the VM record, plus `safety_checkpoint` when it saved one).
    async fn restore(&self, id: &str, tag: &str, safety: bool) -> Result<Value>;
}

impl Api for crate::remote::Remote {
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

    async fn list(&self, id: &str) -> Result<Vec<Checkpoint>> {
        let v = self
            .call(
                reqwest::Method::GET,
                &format!("/v1/vms/{id}/snapshots"),
                None,
            )
            .await?;
        let mut list: Vec<Checkpoint> = v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Checkpoint::from_value)
            .collect();
        list.sort_by(|a, b| b.created_unix.cmp(&a.created_unix).then(a.tag.cmp(&b.tag)));
        Ok(list)
    }

    async fn save(&self, id: &str, tag: &str, memory: bool) -> Result<()> {
        self.call(
            reqwest::Method::POST,
            &format!("/v1/vms/{id}/snapshot"),
            Some(json!({"tag": tag, "memory": memory})),
        )
        .await
        .map(drop)
    }

    async fn delete(&self, id: &str, tag: &str) -> Result<()> {
        self.call(
            reqwest::Method::DELETE,
            &format!("/v1/vms/{id}/snapshots/{tag}"),
            None,
        )
        .await
        .map(drop)
    }

    async fn restore(&self, id: &str, tag: &str, safety: bool) -> Result<Value> {
        self.call(
            reqwest::Method::POST,
            &format!("/v1/vms/{id}/restore"),
            Some(json!({"tag": tag, "safety_checkpoint": safety})),
        )
        .await
    }
}

fn field<'a>(vm: &'a Value, k: &str) -> Result<&'a str> {
    vm[k]
        .as_str()
        .with_context(|| format!("the VM record has no {k}"))
}

async fn instance_named<A: Api>(api: &A, name: &str) -> Result<Value> {
    api.instances()
        .await?
        .into_iter()
        .find(|v| v["name"].as_str() == Some(name))
        .with_context(|| format!("no instance named {name:?} (see `fluxctl mac ps -a`)"))
}

fn missing(name: &str, tag: &str, have: &[Checkpoint]) -> anyhow::Error {
    let names: Vec<&str> = have.iter().map(|c| c.tag.as_str()).collect();
    coded(
        format!(
            "{name} has no checkpoint {tag:?}{}. List them with `fluxctl mac checkpoints {name}`",
            if names.is_empty() {
                " (it has none yet; make one with `fluxctl mac checkpoint ".to_string()
                    + name
                    + " NAME`)"
            } else {
                format!(" (it has: {})", names.join(", "))
            }
        ),
        "MAC_CHECKPOINT_MISSING",
    )
}

// ----------------------------------------------------------------- operations

#[derive(Debug, PartialEq)]
pub struct Saved {
    pub name: String,
    pub tag: String,
    pub memory: bool,
    pub was_running: bool,
}

/// Save a checkpoint. A running instance saves memory unless `disk_only`; a stopped one can only save its disk.
pub async fn save<A: Api>(
    api: &A,
    name: &str,
    tag: Option<&str>,
    disk_only: bool,
    now: u64,
) -> Result<Saved> {
    let tag = tag.map_or_else(|| default_tag(now), str::to_string);
    check_tag(&tag)?;
    let vm = instance_named(api, name).await?;
    let id = field(&vm, "id")?;
    let status = vm_status(&vm);
    let running = matches!(status, "running" | "paused");
    ensure!(
        running || status == "stopped",
        "{name} is {status}: a checkpoint needs it running, paused or stopped. Start it with `fluxctl mac start {name}`"
    );
    if api.list(id).await?.iter().any(|c| c.tag == tag) {
        return Err(coded(
            format!(
                "{name} already has a checkpoint called {tag:?}. Pick another name, or delete it first with `fluxctl mac checkpoint rm {name} {tag}`"
            ),
            "MAC_CHECKPOINT_EXISTS",
        ));
    }
    let memory = running && !disk_only;
    api.save(id, &tag, memory)
        .await
        .map_err(|e| explain_daemon(e, &format!("save checkpoint {name}:{tag}")))?;
    Ok(Saved {
        name: name.to_string(),
        tag,
        memory,
        was_running: running,
    })
}

pub fn saved_text(s: &Saved) -> String {
    let what = if s.memory {
        format!("memory and disk; {} carried on running", s.name)
    } else if s.was_running {
        "disk only, as asked; coming back boots the guest".to_string()
    } else {
        format!(
            "disk only, because {} is stopped; coming back boots the guest. Save while it runs to include memory",
            s.name
        )
    };
    format!(
        "saved checkpoint {n}:{t} ({what}).\ncome back to it: fluxctl mac run --from-checkpoint {n}:{t}   (or: fluxctl mac checkpoint restore {n} {t})",
        n = s.name,
        t = s.tag
    )
}

#[derive(Debug, PartialEq)]
pub struct Restored {
    pub name: String,
    pub id: String,
    pub tag: String,
    pub memory: bool,
    pub safety: Option<(String, bool)>,
}

/// Put `name` back to `tag`. The daemon saves the current state first unless `safety` is false.
pub async fn restore<A: Api>(api: &A, name: &str, tag: &str, safety: bool) -> Result<Restored> {
    check_tag(tag)?;
    let vm = instance_named(api, name).await?;
    let id = field(&vm, "id")?.to_string();
    let have = api.list(&id).await?;
    let Some(c) = have.iter().find(|c| c.tag == tag) else {
        return Err(missing(name, tag, &have));
    };
    let memory = c.has_memory == Some(true);
    let reply = api
        .restore(&id, tag, safety)
        .await
        .map_err(|e| explain_daemon(e, &format!("restore {name} to {tag}")))?;
    let safety = reply["safety_checkpoint"].as_str().map(|t| {
        (
            t.to_string(),
            reply["safety_checkpoint_memory"].as_bool().unwrap_or(false),
        )
    });
    Ok(Restored {
        name: name.to_string(),
        id,
        tag: tag.to_string(),
        memory,
        safety,
    })
}

pub fn restored_text(r: &Restored) -> String {
    let mut s = format!(
        "restored {} to {} ({}).",
        r.name,
        r.tag,
        if r.memory {
            "resumed from saved memory"
        } else {
            "booting from the saved disk; a macOS boot takes minutes"
        }
    );
    match &r.safety {
        Some((t, m)) => s.push_str(&format!("\n{}", rollback_note(&r.name, t, *m))),
        None => s.push_str(
            "\nno safety checkpoint was taken, so what changed since is gone (without --no-safety-checkpoint one is saved first)",
        ),
    }
    s
}

/// Delete a checkpoint.
pub async fn remove<A: Api>(api: &A, name: &str, tag: &str) -> Result<()> {
    check_tag(tag)?;
    let vm = instance_named(api, name).await?;
    let id = field(&vm, "id")?;
    let have = api.list(id).await?;
    if !have.iter().any(|c| c.tag == tag) {
        return Err(missing(name, tag, &have));
    }
    api.delete(id, tag)
        .await
        .map_err(|e| explain_daemon(e, &format!("delete checkpoint {name}:{tag}")))
}

/// Which instance a `--from-checkpoint` names: `NAME:TAG`, or a bare TAG that exactly one instance has.
pub async fn resolve<A: Api>(api: &A, reference: &str) -> Result<(String, String)> {
    let (name, tag) = parse_ref(reference)?;
    if let Some(n) = name {
        return Ok((n, tag));
    }
    let mut owners = Vec::new();
    for vm in api.instances().await? {
        if api
            .list(field(&vm, "id")?)
            .await?
            .iter()
            .any(|c| c.tag == tag)
        {
            owners.push(field(&vm, "name")?.to_string());
        }
    }
    match owners.len() {
        1 => Ok((owners.remove(0), tag)),
        0 => Err(coded(
            format!(
                "no instance has a checkpoint called {tag:?}. See what exists with `fluxctl mac checkpoints`"
            ),
            "MAC_CHECKPOINT_MISSING",
        )),
        _ => Err(coded(
            format!(
                "more than one instance has a checkpoint called {tag:?} ({}). Say which: `--from-checkpoint {}:{tag}`",
                owners.join(", "),
                owners[0]
            ),
            "MAC_CHECKPOINT_AMBIGUOUS",
        )),
    }
}

/// The checkpoints of one instance, or of all of them: `(instance, checkpoints)`.
pub async fn list_for<A: Api>(
    api: &A,
    name: Option<&str>,
) -> Result<Vec<(String, Vec<Checkpoint>)>> {
    let all = api.instances().await?;
    let picked: Vec<&Value> = match name {
        Some(n) => vec![
            all.iter()
                .find(|v| v["name"].as_str() == Some(n))
                .with_context(|| format!("no instance named {n:?} (see `fluxctl mac ps -a`)"))?,
        ],
        None => all.iter().collect(),
    };
    let mut out = Vec::new();
    for vm in picked {
        out.push((
            field(vm, "name")?.to_string(),
            api.list(field(vm, "id")?).await?,
        ));
    }
    Ok(out)
}

/// The two disks `mac diff` compares, or why it cannot: it reads the daemon's files, so it must run on that Mac.
pub fn diff_plan(
    vm: &Value,
    tag: &str,
    other: Option<&str>,
) -> Result<Vec<(String, PathBuf, PathBuf)>> {
    let ws = PathBuf::from(field(vm, "workspace")?);
    let disk = PathBuf::from(field(vm, "disk")?);
    let dir = |t: &str| fluxvm_apple::snapshot_dir(&ws, t);
    if !dir(tag).is_dir() {
        return Err(coded(
            format!(
                "{} is not here: `fluxctl mac diff` reads the instance's disk files, so it must run on the Mac whose daemon owns {} (or the checkpoint {tag:?} does not exist: see `fluxctl mac checkpoints`)",
                dir(tag).display(),
                vm["name"].as_str().unwrap_or("it")
            ),
            "MAC_CHECKPOINT_NOT_LOCAL",
        ));
    }
    let mut out = Vec::new();
    for f in fluxvm_apple::SNAPSHOT_FILES {
        let a = dir(tag).join(f);
        if !a.is_file() {
            continue;
        }
        let b = match other {
            Some(t) => dir(t).join(f),
            None if *f == "disk.raw" => disk.clone(),
            None => ws.join(f),
        };
        if b.is_file() {
            out.push((f.to_string(), a, b));
        }
    }
    ensure!(
        !out.is_empty(),
        "nothing to compare: the checkpoint and the {} have no disk file in common",
        if other.is_some() {
            "other checkpoint"
        } else {
            "instance"
        }
    );
    Ok(out)
}

// ----------------------------------------------------------------- the commands

fn now() -> u64 {
    crate::mac_health::now_unix()
}

pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacCkpt) -> Result<()> {
    match cmd {
        MacCkpt::Checkpoint(a) => checkpoint_cmd(r, a).await,
        MacCkpt::Checkpoints { name, json } => {
            let all = list_for(r, name.as_deref()).await?;
            let t = now();
            if json {
                let doc: Vec<Value> = all
                    .iter()
                    .flat_map(|(n, l)| {
                        l.iter().map(move |c| {
                            let mut v = c.to_json(t);
                            v["instance"] = json!(n);
                            v
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema_version": 1, "checkpoints": doc})
                    )?
                );
                return Ok(());
            }
            let mut any = false;
            for (n, l) in &all {
                if l.is_empty() {
                    continue;
                }
                any = true;
                print!("{}", render_list(n, l, t));
            }
            if any {
                println!("{SIZE_NOTE}");
            } else {
                println!(
                    "no checkpoints yet. Make one: fluxctl mac checkpoint NAME [TAG]  (a running instance keeps its memory, so coming back is instant)"
                );
            }
            Ok(())
        }
        MacCkpt::Diff {
            name,
            tag,
            other,
            json,
        } => {
            let vm = instance_named(r, &name).await?;
            let plan = diff_plan(&vm, &tag, other.as_deref())?;
            eprintln!(
                "comparing {} file(s); this reads both disks once and can take a while",
                plan.len()
            );
            let (n2, t2) = (name.clone(), tag.clone());
            let files = tokio::task::spawn_blocking(move || -> Result<Vec<FileDiff>> {
                plan.iter().map(|(f, a, b)| diff_files(a, b, f)).collect()
            })
            .await
            .context("the comparison stopped")??;
            let b = other.clone().unwrap_or_else(|| "now".into());
            if json {
                let docs: Vec<Value> = files
                    .iter()
                    .map(|f| json!({"file": f.file, "size_a": f.size_a, "size_b": f.size_b, "block_bytes": f.block, "blocks_compared": f.blocks_compared, "blocks_changed": f.blocks_changed, "changed_bytes": f.changed_bytes(), "percent": f.percent()}))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema_version": 1, "instance": n2, "a": t2, "b": b, "files": docs})
                    )?
                );
            } else {
                print!(
                    "{}",
                    render_diff(
                        &n2,
                        &t2,
                        &b,
                        &files,
                        vm_status(&vm) == "running" && other.is_none()
                    )
                );
            }
            Ok(())
        }
    }
}

async fn checkpoint_cmd(r: &crate::remote::Remote, a: CheckpointArgs) -> Result<()> {
    match a.sub {
        Some(CheckpointSub::Rm { name, tag, json }) => {
            remove(r, &name, &tag).await?;
            if json {
                println!("{}", json!({"instance": name, "tag": tag, "removed": true}));
            } else {
                println!("deleted checkpoint {name}:{tag}");
            }
            Ok(())
        }
        Some(CheckpointSub::Restore {
            name,
            tag,
            no_safety_checkpoint,
            json,
        }) => {
            let done = restore(r, &name, &tag, !no_safety_checkpoint).await?;
            print_restored(&done, json)
        }
        None => {
            let name = a.name.context(
                "name the instance: fluxctl mac checkpoint NAME [TAG]  (`fluxctl mac ps -a` lists them)",
            )?;
            let s = save(r, &name, a.tag.as_deref(), a.disk_only, now()).await?;
            if a.json {
                println!(
                    "{}",
                    json!({"instance": s.name, "tag": s.tag, "memory": s.memory, "schema_version": 1})
                );
            } else {
                println!("{}", saved_text(&s));
            }
            Ok(())
        }
    }
}

fn print_restored(done: &Restored, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            json!({"schema_version": 1, "instance": done.name, "tag": done.tag, "resumed_memory": done.memory,
                   "safety_checkpoint": done.safety.as_ref().map(|s| &s.0),
                   "safety_checkpoint_has_memory": done.safety.as_ref().map(|s| s.1)})
        );
    } else {
        println!("{}", restored_text(done));
    }
    Ok(())
}

/// `mac run --from-checkpoint NAME:TAG [-- CMD]`: bring the instance back to the checkpoint, then (with CMD) run it over SSH.
pub async fn run_from_checkpoint(
    r: &crate::remote::Remote,
    reference: &str,
    user: Option<String>,
    wait: u64,
    cmd: Vec<String>,
    json: bool,
) -> Result<()> {
    let (name, tag) = resolve(r, reference).await?;
    let done = restore(r, &name, &tag, true).await?;
    if !json {
        eprintln!("{}", restored_text(&done));
    }
    let ip = if cmd.is_empty() {
        None
    } else {
        let job = crate::mac_instance::GuestJob {
            user: user.unwrap_or_else(|| crate::mac_build_exec::DEFAULT_USER.to_string()),
            wait,
            command: Some(cmd.join(" ")),
            mounts: Vec::new(),
            detach: false,
            tty: crate::mac_exec::Tty::Force,
            shell: false,
        };
        Some(crate::mac_instance::run_in_guest(r, &done.id, &job).await?)
    };
    if json {
        println!(
            "{}",
            json!({"schema_version": 1, "name": done.name, "id": done.id, "from_checkpoint": done.tag,
                   "resumed_memory": done.memory, "ip": ip,
                   "safety_checkpoint": done.safety.as_ref().map(|s| &s.0)})
        );
    } else if ip.is_none() {
        println!("{}", done.name);
    }
    Ok(())
}

/// The flags `--from-checkpoint` cannot be combined with: it brings an existing instance back, it makes no new one.
pub fn check_run_flags(
    image: &str,
    name: Option<&str>,
    publish: &[String],
    rm: bool,
    fleet: bool,
) -> Result<()> {
    let mut bad = Vec::new();
    if !image.is_empty() {
        bad.push("an IMAGE");
    }
    if name.is_some() {
        bad.push("--name");
    }
    if !publish.is_empty() {
        bad.push("-p");
    }
    if rm {
        bad.push("--rm");
    }
    if fleet {
        bad.push("--fleet");
    }
    if bad.is_empty() {
        return Ok(());
    }
    Err(coded(
        format!(
            "--from-checkpoint brings an existing instance back to a saved state, so it cannot be combined with {}. To start a new instance from an image leave --from-checkpoint out; to name the instance use NAME:TAG",
            bad.join(", ")
        ),
        "MAC_FROM_CHECKPOINT",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const T0: u64 = 1_795_000_000;

    #[derive(Default)]
    struct Fake {
        vms: Vec<Value>,
        /// (vm id, tag) -> has_memory
        snaps: RefCell<Vec<(String, Checkpoint)>>,
        calls: RefCell<Vec<String>>,
        restore_reply: RefCell<Option<Result<Value, String>>>,
        save_error: RefCell<Option<String>>,
    }

    fn vm(id: &str, name: &str, status: &str) -> Value {
        json!({"id": id, "name": name, "status": status, "labels": {"fluxvm.mac": "instance"},
               "workspace": format!("/ws/{id}"), "disk": format!("/ws/{id}/disk.raw")})
    }

    fn ck(tag: &str, mem: bool, age: u64) -> Checkpoint {
        Checkpoint {
            tag: tag.into(),
            created_unix: Some(T0 - age),
            size_bytes: 64 << 30,
            has_memory: Some(mem),
        }
    }

    impl Api for Fake {
        async fn instances(&self) -> Result<Vec<Value>> {
            Ok(self.vms.clone())
        }
        async fn list(&self, id: &str) -> Result<Vec<Checkpoint>> {
            Ok(self
                .snaps
                .borrow()
                .iter()
                .filter(|(i, _)| i == id)
                .map(|(_, c)| c.clone())
                .collect())
        }
        async fn save(&self, id: &str, tag: &str, memory: bool) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(format!("save {id} {tag} memory={memory}"));
            if let Some(e) = self.save_error.borrow().clone() {
                bail!("{e}");
            }
            self.snaps
                .borrow_mut()
                .push((id.into(), ck(tag, memory, 0)));
            Ok(())
        }
        async fn delete(&self, id: &str, tag: &str) -> Result<()> {
            self.calls.borrow_mut().push(format!("delete {id} {tag}"));
            self.snaps
                .borrow_mut()
                .retain(|(i, c)| !(i == id && c.tag == tag));
            Ok(())
        }
        async fn restore(&self, id: &str, tag: &str, safety: bool) -> Result<Value> {
            self.calls
                .borrow_mut()
                .push(format!("restore {id} {tag} safety={safety}"));
            match self.restore_reply.borrow().clone() {
                Some(Err(e)) => bail!("{e}"),
                Some(Ok(v)) => Ok(v),
                None => Ok(json!({"id": id})),
            }
        }
    }

    #[test]
    fn tags_default_to_a_timestamp_and_are_checked_in_plain_words() {
        assert_eq!(default_tag(T0), "ckpt-20261118-110640");
        assert!(check_tag("before-upgrade_1.2").is_ok());
        for bad in ["", ".hidden", "a b", "a/b", "a:b", &"x".repeat(129)] {
            let e = check_tag(bad).unwrap_err().to_string();
            assert!(
                e.contains("not a usable checkpoint name") && e.contains("MAC_CHECKPOINT_TAG"),
                "{bad:?}: {e}"
            );
        }
    }

    #[test]
    fn references_split_at_the_last_colon() {
        assert_eq!(
            parse_ref("ci:warm").unwrap(),
            (Some("ci".into()), "warm".into())
        );
        assert_eq!(parse_ref("warm").unwrap(), (None, "warm".into()));
        assert!(
            parse_ref("ci:")
                .unwrap_err()
                .to_string()
                .contains("NAME:TAG")
        );
        assert!(parse_ref(":warm").is_err() && parse_ref(" ").is_err());
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(age_text(5), "just now");
        assert_eq!(age_text(120), "2m ago");
        assert_eq!(age_text(3 * 3600 + 10), "3h ago");
        assert_eq!(age_text(2 * 86400), "2d ago");
    }

    #[test]
    fn the_list_shows_kind_size_age_and_marks_automatic_ones() {
        let l = vec![
            ck("warm", true, 7200),
            ck("auto-before-restore-20261117-100000", false, 30),
        ];
        let t = render_list("ci", &l, T0);
        assert!(
            t.contains("ci:warm") && t.contains("memory and disk") && t.contains("2h ago"),
            "{t}"
        );
        assert!(
            t.contains("64.0 GiB") && t.contains("disk only") && t.contains("just now"),
            "{t}"
        );
        assert!(t.contains("automatic safety net"), "{t}");
        assert_eq!(t.matches("automatic safety net").count(), 1);
        let j = l[1].to_json(T0);
        assert_eq!(
            (j["automatic"].clone(), j["kind"].clone()),
            (json!(true), json!("disk only"))
        );
    }

    #[tokio::test]
    async fn a_running_instance_saves_memory_and_a_stopped_one_only_its_disk() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running"), vm("2", "old", "stopped")],
            ..Default::default()
        };
        let s = save(&f, "ci", Some("warm"), false, T0).await.unwrap();
        assert!(s.memory && s.was_running);
        assert!(saved_text(&s).contains("fluxctl mac run --from-checkpoint ci:warm"));
        let s = save(&f, "ci", Some("cold"), true, T0).await.unwrap();
        assert!(!s.memory && saved_text(&s).contains("as asked"));
        let s = save(&f, "old", None, false, T0).await.unwrap();
        assert!(
            !s.memory
                && s.tag.starts_with("ckpt-")
                && saved_text(&s).contains("because old is stopped")
        );
        assert_eq!(
            *f.calls.borrow(),
            [
                "save 1 warm memory=true",
                "save 1 cold memory=false",
                "save 2 ckpt-20261118-110640 memory=false"
            ]
        );
    }

    #[tokio::test]
    async fn saving_refuses_a_name_in_use_a_bad_name_and_a_starting_instance() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running"), vm("3", "x", "creating")],
            ..Default::default()
        };
        save(&f, "ci", Some("warm"), false, T0).await.unwrap();
        let e = save(&f, "ci", Some("warm"), false, T0)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("already has a checkpoint")
                && e.contains("checkpoint rm ci warm")
                && e.contains("MAC_CHECKPOINT_EXISTS"),
            "{e}"
        );
        assert!(
            save(&f, "ci", Some("a b"), false, T0)
                .await
                .unwrap_err()
                .to_string()
                .contains("MAC_CHECKPOINT_TAG")
        );
        assert!(
            save(&f, "x", Some("t"), false, T0)
                .await
                .unwrap_err()
                .to_string()
                .contains("mac start x")
        );
        assert!(
            save(&f, "nope", Some("t"), false, T0)
                .await
                .unwrap_err()
                .to_string()
                .contains("no instance named")
        );
        assert_eq!(
            f.calls.borrow().len(),
            1,
            "nothing was saved after the refusals"
        );
    }

    #[tokio::test]
    async fn a_daemon_failure_to_save_is_explained_with_its_code() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running")],
            ..Default::default()
        };
        *f.save_error.borrow_mut() = Some("POST /v1/vms/1/snapshot: 500: this VM cannot be saved: the device set has a USB controller".into());
        let e = save(&f, "ci", Some("w"), false, T0)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("cannot save checkpoint ci:w") && e.contains("MAC_CHECKPOINT_NOT_SAVABLE"),
            "{e}"
        );
        *f.save_error.borrow_mut() = Some(
            "POST /v1/vms/1/snapshot: 500: not enough free disk to save a checkpoint: 900 MiB free"
                .into(),
        );
        let e = save(&f, "ci", Some("w"), false, T0)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("MAC_DISK_FULL"), "{e}");
    }

    #[tokio::test]
    async fn restore_names_the_safety_checkpoint_and_how_to_roll_back() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running")],
            ..Default::default()
        };
        f.snaps
            .borrow_mut()
            .push(("1".into(), ck("warm", true, 100)));
        *f.restore_reply.borrow_mut() = Some(Ok(
            json!({"id": "1", "safety_checkpoint": "auto-before-restore-20261118-110640", "safety_checkpoint_memory": true}),
        ));
        let r = restore(&f, "ci", "warm", true).await.unwrap();
        assert!(r.memory);
        let t = restored_text(&r);
        assert!(t.contains("resumed from saved memory"), "{t}");
        assert!(
            t.contains("saved as ci:auto-before-restore-20261118-110640"),
            "{t}"
        );
        assert!(
            t.contains("fluxctl mac checkpoint restore ci auto-before-restore-20261118-110640"),
            "{t}"
        );
        assert_eq!(*f.calls.borrow(), ["restore 1 warm safety=true"]);
    }

    #[tokio::test]
    async fn restore_without_a_safety_checkpoint_says_what_that_means() {
        let f = Fake {
            vms: vec![vm("1", "ci", "stopped")],
            ..Default::default()
        };
        f.snaps
            .borrow_mut()
            .push(("1".into(), ck("cold", false, 100)));
        let r = restore(&f, "ci", "cold", false).await.unwrap();
        let t = restored_text(&r);
        assert!(
            t.contains("booting from the saved disk")
                && t.contains("no safety checkpoint was taken"),
            "{t}"
        );
        assert_eq!(*f.calls.borrow(), ["restore 1 cold safety=false"]);
    }

    #[tokio::test]
    async fn restoring_or_removing_a_missing_checkpoint_lists_what_exists() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running"), vm("2", "bare", "stopped")],
            ..Default::default()
        };
        f.snaps.borrow_mut().push(("1".into(), ck("warm", true, 1)));
        let e = restore(&f, "ci", "nope", true)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("no checkpoint \"nope\"")
                && e.contains("it has: warm")
                && e.contains("MAC_CHECKPOINT_MISSING"),
            "{e}"
        );
        let e = remove(&f, "bare", "x").await.unwrap_err().to_string();
        assert!(
            e.contains("it has none yet") && e.contains("fluxctl mac checkpoint bare NAME"),
            "{e}"
        );
        assert!(
            f.calls.borrow().is_empty(),
            "the daemon was not asked to change anything"
        );
        remove(&f, "ci", "warm").await.unwrap();
        assert!(f.list("1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_safety_net_refusal_from_the_daemon_is_explained() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running")],
            ..Default::default()
        };
        f.snaps.borrow_mut().push(("1".into(), ck("warm", true, 1)));
        *f.restore_reply.borrow_mut() = Some(Err("POST /v1/vms/1/restore: 409 Conflict: could not save your current state first, so nothing was changed: no room".into()));
        let e = restore(&f, "ci", "warm", true)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("MAC_CHECKPOINT_SAFETY") && !e.contains("MAC_SLOT_LIMIT"),
            "{e}"
        );
        assert!(e.contains("nothing was changed"), "{e}");
    }

    #[tokio::test]
    async fn a_bare_tag_finds_its_one_owner_and_a_shared_tag_asks_which() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running"), vm("2", "dev", "stopped")],
            ..Default::default()
        };
        f.snaps.borrow_mut().push(("1".into(), ck("warm", true, 1)));
        f.snaps
            .borrow_mut()
            .push(("2".into(), ck("only-dev", false, 1)));
        assert_eq!(
            resolve(&f, "ci:warm").await.unwrap(),
            ("ci".into(), "warm".into())
        );
        assert_eq!(
            resolve(&f, "only-dev").await.unwrap(),
            ("dev".into(), "only-dev".into())
        );
        assert!(
            resolve(&f, "ghost")
                .await
                .unwrap_err()
                .to_string()
                .contains("MAC_CHECKPOINT_MISSING")
        );
        f.snaps
            .borrow_mut()
            .push(("2".into(), ck("warm", false, 1)));
        let e = resolve(&f, "warm").await.unwrap_err().to_string();
        assert!(
            e.contains("ci, dev")
                && e.contains("MAC_CHECKPOINT_AMBIGUOUS")
                && e.contains("ci:warm"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn listing_covers_one_instance_or_all() {
        let f = Fake {
            vms: vec![vm("1", "ci", "running"), vm("2", "dev", "stopped")],
            ..Default::default()
        };
        f.snaps.borrow_mut().push(("1".into(), ck("a", true, 1)));
        f.snaps.borrow_mut().push(("2".into(), ck("b", false, 1)));
        assert_eq!(list_for(&f, None).await.unwrap().len(), 2);
        let one = list_for(&f, Some("dev")).await.unwrap();
        assert_eq!((one[0].0.as_str(), one[0].1[0].tag.as_str()), ("dev", "b"));
        assert!(list_for(&f, Some("zz")).await.is_err());
    }

    #[test]
    fn diff_counts_the_blocks_that_changed_and_the_size_difference() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        let mut x = vec![0u8; 3 * DIFF_BLOCK];
        std::fs::write(&a, &x).unwrap();
        std::fs::write(&b, &x).unwrap();
        let same = diff_files(&a, &b, "disk.raw").unwrap();
        assert_eq!((same.blocks_compared, same.blocks_changed), (3, 0));
        assert!(render_diff("ci", "warm", "now", &[same], false).contains("identical"));
        x[DIFF_BLOCK + 5] = 9;
        std::fs::write(&b, &x).unwrap();
        let one = diff_files(&a, &b, "disk.raw").unwrap();
        assert_eq!(
            (one.blocks_changed, one.changed_bytes()),
            (1, DIFF_BLOCK as u64)
        );
        assert!((one.percent() - 33.3).abs() < 0.1);
        // The second file grew by half a block: the extra block counts as changed.
        x.extend(vec![1u8; DIFF_BLOCK / 2]);
        std::fs::write(&b, &x).unwrap();
        let grown = diff_files(&a, &b, "disk.raw").unwrap();
        assert_eq!((grown.blocks_compared, grown.blocks_changed), (4, 2));
        let t = render_diff("ci", "warm", "now", &[grown], true);
        assert!(
            t.contains("differ") && t.contains("sizes") && t.contains("keeps writing"),
            "{t}"
        );
        assert!(diff_files(&a, &d.path().join("missing"), "x").is_err());
    }

    #[test]
    fn the_diff_plan_pairs_checkpoint_files_with_the_live_ones_and_needs_local_files() {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path().join("ws");
        let snap = fluxvm_apple::snapshot_dir(&ws, "warm");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(snap.join("disk.raw"), b"x").unwrap();
        std::fs::write(snap.join("efi.bin"), b"x").unwrap();
        std::fs::write(ws.join("disk.raw"), b"y").unwrap();
        std::fs::write(ws.join("efi.bin"), b"y").unwrap();
        let v = json!({"name": "ci", "workspace": ws, "disk": ws.join("disk.raw")});
        let plan = diff_plan(&v, "warm", None).unwrap();
        let names: Vec<&str> = plan.iter().map(|p| p.0.as_str()).collect();
        assert_eq!(names, ["disk.raw", "efi.bin"]);
        assert_eq!(plan[0].2, ws.join("disk.raw"));
        let e = diff_plan(&v, "gone", None).unwrap_err().to_string();
        assert!(
            e.contains("must run on the Mac") && e.contains("MAC_CHECKPOINT_NOT_LOCAL"),
            "{e}"
        );
        let far = json!({"name": "ci", "workspace": "/no/such/ws", "disk": "/no/such/ws/disk.raw"});
        assert!(diff_plan(&far, "warm", None).is_err());
    }

    #[test]
    fn from_checkpoint_refuses_flags_that_would_make_a_new_instance() {
        assert!(check_run_flags("", None, &[], false, false).is_ok());
        let e = check_run_flags("img", Some("n"), &["80:80".into()], true, true)
            .unwrap_err()
            .to_string();
        for w in [
            "an IMAGE",
            "--name",
            "-p",
            "--rm",
            "--fleet",
            "MAC_FROM_CHECKPOINT",
        ] {
            assert!(e.contains(w), "{w}: {e}");
        }
    }

    #[test]
    fn the_commands_parse() {
        use clap::Parser;
        #[derive(Parser)]
        struct P {
            #[command(subcommand)]
            c: MacCkpt,
        }
        let p = |a: &[&str]| {
            P::try_parse_from(std::iter::once("x").chain(a.iter().copied())).map(|p| p.c)
        };
        let Ok(MacCkpt::Checkpoint(a)) = p(&["checkpoint", "ci", "warm", "--disk-only"]) else {
            panic!()
        };
        assert_eq!(
            (
                a.name.as_deref(),
                a.tag.as_deref(),
                a.disk_only,
                a.sub.is_none()
            ),
            (Some("ci"), Some("warm"), true, true)
        );
        let Ok(MacCkpt::Checkpoint(a)) = p(&["checkpoint", "rm", "ci", "warm"]) else {
            panic!()
        };
        assert!(matches!(a.sub, Some(CheckpointSub::Rm { .. })) && a.name.is_none());
        let Ok(MacCkpt::Checkpoint(a)) = p(&[
            "checkpoint",
            "restore",
            "ci",
            "warm",
            "--no-safety-checkpoint",
        ]) else {
            panic!()
        };
        assert!(matches!(
            a.sub,
            Some(CheckpointSub::Restore {
                no_safety_checkpoint: true,
                ..
            })
        ));
        assert!(matches!(
            p(&["checkpoints"]),
            Ok(MacCkpt::Checkpoints { name: None, .. })
        ));
        assert!(matches!(
            p(&["checkpoints", "ci", "--json"]),
            Ok(MacCkpt::Checkpoints { json: true, .. })
        ));
        assert!(matches!(
            p(&["diff", "ci", "warm", "cold"]),
            Ok(MacCkpt::Diff { other: Some(_), .. })
        ));
    }
}
