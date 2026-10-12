// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Flux Branch: named, durable checkpoints of a running `vz` Linux guest.
//!
//! A branch is the guest's saved machine state and disk (APFS clones, so it costs nothing until something writes) plus
//! the VM record it was taken from, kept under `<state_dir>/branches/<id>/` **outside the VM's workspace**: deleting
//! the VM does not delete its branches. The directory is laid out like a workspace, which is what lets the same
//! machinery that restores fork children restore a branch: `start` creates `count` new VMs from it, each with its own
//! identity, token and (for `apple.usernet`) its own network.
//!
//! A branch can be started any number of times, now or after the source VM is gone, and a VM started from a branch can
//! itself be branched (`parent_branch` records the lineage). Same restrictions as fork: a vz Linux guest on
//! `apple.usernet` or no network, no extra disks.
//!
//! `prune_branches` removes branches by policy (dry run first). Not here yet: committing a branch's disk back,
//! rebasing, flattening, a disk diff.

use crate::fork::{ForkChildReport, ForkIdentityMode, ForkReport, ForkSpec, MAX_FORK_COUNT};
use crate::journal::{OpGuard, OpKind};
use crate::{VmManager, audit_event, validate_vm_name};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use fluxvm_core::model::{BackendKind, VmRecord, VmStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

/// Label on a VM started from a branch, naming it.
pub const BRANCH_LABEL: &str = "fluxvm.dev/branch";

/// The tag of the checkpoint inside a branch directory.
const CKPT_TAG: &str = "ckpt";

const BRANCH_FILE: &str = "branch.json";

/// What the API shows about a branch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VmBranch {
    pub id: Uuid,
    pub name: String,
    /// The VM it was taken from (which may no longer exist).
    pub source_vm: Uuid,
    pub source_name: String,
    /// Set when the source VM was itself started from a branch.
    pub parent_branch: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub vcpus: u8,
    pub memory_mib: u64,
    /// Logical size of the saved machine state, in bytes.
    pub state_bytes: u64,
}

/// On disk: the public view plus the VM record a start is built from (it carries the source's agent token, so it is
/// never returned by the API, and the directory is 0700).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BranchFile {
    branch: VmBranch,
    record: VmRecord,
}

impl VmManager {
    fn branches_dir(&self) -> PathBuf {
        self.cfg.state_dir.join("branches")
    }

    fn branch_dir(&self, id: Uuid) -> PathBuf {
        self.branches_dir().join(id.to_string())
    }

    /// The record a start from branch `id` would clone, and the branch's name (for plans: nothing is written).
    pub(crate) fn branch_template(&self, id: Uuid) -> Result<(VmRecord, String)> {
        let file = self.read_branch_file(id)?;
        let mut template = file.record;
        template.workspace = self.branch_dir(id);
        template.status = VmStatus::Running;
        template
            .labels
            .insert(BRANCH_LABEL.to_string(), id.to_string());
        Ok((template, file.branch.name))
    }

    fn read_branch_file(&self, id: Uuid) -> Result<BranchFile> {
        let raw = std::fs::read(self.branch_dir(id).join(BRANCH_FILE))
            .with_context(|| format!("branch {id} not found"))?;
        serde_json::from_slice(&raw).with_context(|| format!("reading branch {id}"))
    }

    /// Checkpoint running VM `vm_id` as a branch called `name`. The guest keeps running; it is held only for the
    /// save and its disk clone (the same short pause as a fork).
    pub async fn create_branch(
        self: &Arc<Self>,
        vm_id: Uuid,
        name: String,
        labels: BTreeMap<String, String>,
        actor: Option<&str>,
    ) -> Result<VmBranch> {
        validate_vm_name(&name)?;
        if labels.len() > 16
            || labels
                .iter()
                .any(|(k, v)| k.is_empty() || k.len() > 128 || v.len() > 256)
        {
            bail!("at most 16 labels, keys 1-128 and values up to 256 characters");
        }
        let src = self.get(vm_id).await?;
        if src.backend != BackendKind::Vz {
            bail!("branches need a vz Linux guest (the KVM engine has fork, not branches)");
        }
        crate::fork::check_vz_forkable(&src, 1)?;
        if self
            .list_branches(Some(vm_id))
            .await?
            .iter()
            .any(|b| b.name == name)
        {
            bail!("VM {vm_id} already has a branch named {name:?}");
        }

        let id = Uuid::new_v4();
        let tag = format!("branch-{}", &id.simple().to_string()[..12]);
        let dir = self.branch_dir(id);
        let partial = self.branches_dir().join(format!(".{id}.partial"));
        std::fs::create_dir_all(self.branches_dir())?;
        set_private(&self.branches_dir());

        self.create_vm_snapshot(vm_id, &tag).await?;
        let exported: Result<u64> = async {
            let (ws, p, t) = (src.workspace.clone(), partial.clone(), tag.clone());
            tokio::task::spawn_blocking(move || {
                fluxvm_apple::export_checkpoint(&ws, &t, &p, CKPT_TAG)
            })
            .await
            .context("branch export panicked")??;
            let state =
                fluxvm_apple::snapshot_dir(&partial, CKPT_TAG).join(fluxvm_apple::STATE_FILE);
            Ok(std::fs::metadata(state).map(|m| m.len()).unwrap_or(0))
        }
        .await;
        // The VM's own snapshot has done its job, whatever happened.
        if let Err(e) = self.delete_vm_snapshot(vm_id, &tag).await {
            tracing::warn!(vm = %vm_id, tag, error = %e, "removing the branch's temporary snapshot failed");
        }
        let state_bytes = match exported {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&partial);
                return Err(e.context("exporting the checkpoint"));
            }
        };

        let parent_branch = src
            .labels
            .get(BRANCH_LABEL)
            .and_then(|v| Uuid::parse_str(v).ok());
        let branch = VmBranch {
            id,
            name,
            source_vm: vm_id,
            source_name: src.name.clone(),
            parent_branch,
            created_at: Utc::now(),
            labels,
            vcpus: src.request.vcpus,
            memory_mib: src.request.memory_mib,
            state_bytes,
        };
        let mut record = src;
        record.workspace = dir.clone();
        record.status = VmStatus::Running;
        let file = BranchFile {
            branch: branch.clone(),
            record,
        };
        let written = (|| -> Result<()> {
            set_private(&partial);
            write_private(
                &partial.join(BRANCH_FILE),
                &serde_json::to_vec_pretty(&file)?,
            )?;
            std::fs::rename(&partial, &dir).context("publishing the branch")?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(e);
        }
        audit_event(
            "vm.branch.create",
            &[
                ("vm_id", &vm_id.to_string()),
                ("branch_id", &id.to_string()),
                ("name", &branch.name),
                ("actor", actor.unwrap_or("")),
            ],
        );
        Ok(branch)
    }

    /// Branches, oldest first; all of them, or those of one VM.
    pub async fn list_branches(&self, vm: Option<Uuid>) -> Result<Vec<VmBranch>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(self.branches_dir()) else {
            return Ok(out);
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(id) = Uuid::parse_str(&name) else {
                continue; // `.partial` leftovers and strangers
            };
            if let Ok(f) = self.read_branch_file(id)
                && vm.is_none_or(|v| f.branch.source_vm == v)
            {
                out.push(f.branch);
            }
        }
        out.sort_by_key(|b| b.created_at);
        Ok(out)
    }

    pub async fn get_branch(&self, id: Uuid) -> Result<VmBranch> {
        Ok(self.read_branch_file(id)?.branch)
    }

    /// Removes a branch. VMs already started from it are independent (their files are clones) and are not touched.
    pub async fn delete_branch(&self, id: Uuid, actor: Option<&str>) -> Result<()> {
        let f = self.read_branch_file(id)?;
        let dir = self.branch_dir(id);
        crate::journal::remove_tree(&dir).context("removing the branch")?;
        audit_event(
            "vm.branch.delete",
            &[
                ("branch_id", &id.to_string()),
                ("name", &f.branch.name),
                ("actor", actor.unwrap_or("")),
            ],
        );
        Ok(())
    }

    /// Start `spec.count` new VMs from branch `id` (names `<name_prefix or branch name>-<n>`), as fork would: each is
    /// restored from the checkpoint, gets its own identity, and is reported ready as `spec` says.
    pub async fn start_branch(
        self: &Arc<Self>,
        id: Uuid,
        spec: ForkSpec,
        actor: Option<&str>,
    ) -> Result<ForkReport> {
        let started = Instant::now();
        let file = self.read_branch_file(id)?;
        let post = spec.post()?;
        if let Some(p) = spec.parallelism
            && !(1..=MAX_FORK_COUNT as u8).contains(&p)
        {
            bail!("parallelism must be between 1 and {MAX_FORK_COUNT}");
        }
        let mut template = file.record;
        template.workspace = self.branch_dir(id);
        template.status = VmStatus::Running;
        template
            .labels
            .insert(BRANCH_LABEL.to_string(), id.to_string());
        crate::fork::check_vz_forkable(&template, spec.count)?;
        crate::fork::check_fork_network_mode(&template, spec.network)?;
        // Held until the children are recorded (or the start failed).
        let _reservation = self.admit_fork_batch(&template, spec.count, actor).await?;
        let count = spec.count;
        let prefix = spec
            .name_prefix
            .clone()
            .unwrap_or_else(|| file.branch.name.clone());
        let names: Vec<String> = (1..=count).map(|n| format!("{prefix}-{n}")).collect();
        for n in &names {
            validate_vm_name(n)?;
        }
        let parallelism = spec
            .parallelism
            .unwrap_or_else(|| count.min(8) as u8)
            .clamp(1, MAX_FORK_COUNT as u8);
        // Journaled like a fork: a crash mid-start rolls the batch back whole (or adopts it, if it had finished).
        let op = Arc::new(
            OpGuard::begin(
                &self.cfg.state_dir,
                OpKind::Fork,
                Uuid::new_v4(),
                id,
                vec![],
            )
            .context("journaling branch start")?,
        );
        op.fork_expect(count)
            .context("journaling branch start size")?;
        let result = self
            .fork_vz_children(
                Arc::new(template),
                CKPT_TAG,
                names,
                spec.identity,
                Arc::new(post),
                actor,
                Arc::clone(&op),
                parallelism,
            )
            .await;
        if let Ok(op) = Arc::try_unwrap(op) {
            op.finish();
        }
        let mut done = result?;
        done.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let (children, reports): (Vec<VmRecord>, Vec<ForkChildReport>) = done.into_iter().unzip();
        let ids: Vec<String> = children.iter().map(|c| c.id.to_string()).collect();
        audit_event(
            "vm.branch.start",
            &[
                ("branch_id", &id.to_string()),
                ("count", &count.to_string()),
                ("children", &ids.join(",")),
                (
                    "identity",
                    match spec.identity {
                        ForkIdentityMode::Preserve => "preserve",
                        ForkIdentityMode::BestEffort => "best-effort",
                        ForkIdentityMode::Unique => "unique",
                    },
                ),
                ("total_ms", &started.elapsed().as_millis().to_string()),
                ("report", &crate::fork::report_json(&reports)),
            ],
        );
        let report = ForkReport {
            children,
            reports,
            parent_snapshot_ms: 0,
            total_ms: started.elapsed().as_millis() as u64,
            parallelism,
        };
        fluxvm_core::metrics::record_fork(
            report.children.len() as u64,
            0,
            report.total_ms,
            report.reports.iter().map(|c| c.restore_ms).sum(),
            report.reports.iter().map(|c| c.identity.ms).sum(),
        );
        Ok(report)
    }
}

fn set_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Label that exempts a branch from pruning: `fluxvm.dev/keep: "true"`.
pub const KEEP_LABEL: &str = "fluxvm.dev/keep";

/// Which branches a prune may remove. At least one of `keep_last` and `older_than` is required, so an empty policy can
/// never mean "everything". When both are given a branch must satisfy both (beyond the newest `keep_last` *and* older
/// than `older_than`), the conservative reading.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrunePolicy {
    /// Keep the newest N branches in scope; the rest are candidates.
    #[serde(default)]
    pub keep_last: Option<usize>,
    /// Candidates must be older than this many seconds.
    #[serde(default)]
    pub older_than_secs: Option<u64>,
    /// Limit the scope to branches taken from this VM.
    #[serde(default)]
    pub source_vm: Option<Uuid>,
    /// Limit the scope to branch names starting with this prefix.
    #[serde(default)]
    pub name_prefix: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PruneCandidate {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub state_bytes: u64,
    pub reason: String,
}

/// Pure selection: which of `branches` a prune under `policy` would remove at `now`, and how many in scope were
/// protected by [`KEEP_LABEL`]. Nothing is deleted here.
pub fn select_prune(
    branches: &[VmBranch],
    policy: &PrunePolicy,
    now: DateTime<Utc>,
) -> Result<(Vec<PruneCandidate>, usize)> {
    if policy.keep_last.is_none() && policy.older_than_secs.is_none() {
        bail!("give keep_last, older_than_secs or both; an empty policy would match every branch");
    }
    if policy.keep_last == Some(0) && policy.older_than_secs.is_none() {
        bail!(
            "keep_last of 0 without older_than_secs would remove every branch in scope; use older_than_secs too"
        );
    }
    let mut scope: Vec<&VmBranch> = branches
        .iter()
        .filter(|b| policy.source_vm.is_none_or(|v| b.source_vm == v))
        .filter(|b| {
            policy
                .name_prefix
                .as_deref()
                .is_none_or(|p| b.name.starts_with(p))
        })
        .collect();
    // Newest first, so the first `keep_last` are kept.
    scope.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    let mut out = Vec::new();
    let mut protected = 0;
    for (rank, b) in scope.iter().enumerate() {
        let beyond = policy.keep_last.is_none_or(|n| rank >= n);
        let age = (now - b.created_at).num_seconds().max(0) as u64;
        let old = policy.older_than_secs.is_none_or(|s| age > s);
        if !(beyond && old) {
            continue;
        }
        if b.labels.get(KEEP_LABEL).is_some_and(|v| v == "true") {
            protected += 1;
            continue;
        }
        let mut why = Vec::new();
        if let Some(n) = policy.keep_last {
            why.push(format!("beyond the newest {n}"));
        }
        if let Some(s) = policy.older_than_secs {
            why.push(format!("older than {s} s ({age} s)"));
        }
        out.push(PruneCandidate {
            id: b.id,
            name: b.name.clone(),
            created_at: b.created_at,
            state_bytes: b.state_bytes,
            reason: why.join(", "),
        });
    }
    Ok((out, protected))
}

#[derive(Debug, Clone, Serialize)]
pub struct PruneReport {
    pub dry_run: bool,
    pub selected: Vec<PruneCandidate>,
    pub deleted: Vec<Uuid>,
    pub failed: Vec<(Uuid, String)>,
    pub protected: usize,
    /// Logical bytes of saved state in the selected branches (disk clones share blocks, so space freed can differ).
    pub selected_state_bytes: u64,
}

impl VmManager {
    /// Selects branches under `policy` and, unless `dry_run`, deletes them one by one (a failure on one is recorded
    /// and does not stop the rest). VMs started from a pruned branch are independent and keep running.
    pub async fn prune_branches(
        &self,
        policy: &PrunePolicy,
        dry_run: bool,
        actor: Option<&str>,
    ) -> Result<PruneReport> {
        let all = self.list_branches(None).await?;
        let (selected, protected) = select_prune(&all, policy, Utc::now())?;
        let mut deleted = Vec::new();
        let mut failed = Vec::new();
        if !dry_run {
            for c in &selected {
                match self.delete_branch(c.id, actor).await {
                    Ok(()) => deleted.push(c.id),
                    Err(e) => failed.push((c.id, format!("{e:#}"))),
                }
            }
        }
        Ok(PruneReport {
            dry_run,
            selected_state_bytes: selected.iter().map(|c| c.state_bytes).sum(),
            selected,
            deleted,
            failed,
            protected,
        })
    }
}

/// How branch `b` differs from branch `a`: the saved sizes, the lineage between them and the labels.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BranchDiff {
    pub a: Uuid,
    pub b: Uuid,
    pub a_name: String,
    pub b_name: String,
    pub same_source_vm: bool,
    /// `a` is `b`'s parent, `b` is `a`'s parent, or neither (directly).
    pub lineage: &'static str,
    /// Seconds `b` was taken after `a` (negative when before).
    pub seconds_apart: i64,
    pub vcpus: (u8, u8),
    pub memory_mib: (u64, u64),
    pub state_bytes: (u64, u64),
    pub labels_added: BTreeMap<String, String>,
    pub labels_removed: BTreeMap<String, String>,
    /// key -> (value in `a`, value in `b`)
    pub labels_changed: BTreeMap<String, (String, String)>,
}

/// Pure comparison of two branches' public views.
pub fn diff_branches(a: &VmBranch, b: &VmBranch) -> BranchDiff {
    let lineage = if b.parent_branch == Some(a.id) {
        "a-is-parent-of-b"
    } else if a.parent_branch == Some(b.id) {
        "b-is-parent-of-a"
    } else if a.parent_branch.is_some() && a.parent_branch == b.parent_branch {
        "siblings"
    } else {
        "unrelated"
    };
    let mut added = BTreeMap::new();
    let mut changed = BTreeMap::new();
    for (k, v) in &b.labels {
        match a.labels.get(k) {
            None => {
                added.insert(k.clone(), v.clone());
            }
            Some(old) if old != v => {
                changed.insert(k.clone(), (old.clone(), v.clone()));
            }
            _ => {}
        }
    }
    let removed = a
        .labels
        .iter()
        .filter(|(k, _)| !b.labels.contains_key(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    BranchDiff {
        a: a.id,
        b: b.id,
        a_name: a.name.clone(),
        b_name: b.name.clone(),
        same_source_vm: a.source_vm == b.source_vm,
        lineage,
        seconds_apart: (b.created_at - a.created_at).num_seconds(),
        vcpus: (a.vcpus, b.vcpus),
        memory_mib: (a.memory_mib, b.memory_mib),
        state_bytes: (a.state_bytes, b.state_bytes),
        labels_added: added,
        labels_removed: removed,
        labels_changed: changed,
    }
}

/// Copies directory `from` to `to` file by file, copy-on-write where the filesystem allows, owner-only permissions.
fn clone_tree(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    set_private(to);
    for e in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ty = e.file_type()?;
        if ty.is_dir() {
            clone_tree(&src, &dst)?;
        } else if ty.is_file() {
            fluxvm_apple::clone_file(&src, &dst)?;
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600));
        } else {
            bail!("{} is not a regular file or directory", src.display());
        }
    }
    Ok(())
}

impl VmManager {
    /// Writes branch `id` to the new directory `dest` as a self-contained bundle (the saved state, disk clones and
    /// `branch.json`). The bundle carries the guest's agent token, like the disk does, so it is owner-only: treat it as
    /// the VM's disk. `dest` must not exist.
    pub async fn export_branch(
        &self,
        id: Uuid,
        dest: &std::path::Path,
        actor: Option<&str>,
    ) -> Result<std::path::PathBuf> {
        let file = self.read_branch_file(id)?;
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        let parent = dest
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        if !parent.is_dir() {
            bail!("{} is not a directory", parent.display());
        }
        let name = dest
            .file_name()
            .context("the destination needs a name")?
            .to_string_lossy()
            .into_owned();
        let partial = parent.join(format!(".{name}.partial"));
        let _ = std::fs::remove_dir_all(&partial);
        let (from, p) = (self.branch_dir(id), partial.clone());
        let cloned = tokio::task::spawn_blocking(move || clone_tree(&from, &p))
            .await
            .context("branch export panicked")?;
        if let Err(e) =
            cloned.and_then(|()| std::fs::rename(&partial, dest).context("publishing the bundle"))
        {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(e);
        }
        audit_event(
            "vm.branch.export",
            &[
                ("branch_id", &id.to_string()),
                ("name", &file.branch.name),
                ("dest", &dest.display().to_string()),
                ("actor", actor.unwrap_or("")),
            ],
        );
        Ok(dest.to_path_buf())
    }

    /// Imports a bundle written by [`export_branch`] as a new branch with a fresh id (and `name`, if given). The
    /// parent link is dropped: the parent need not exist here. A saved state restores only on a Mac of the same
    /// hardware model and a compatible macOS; the import cannot check that, a start reports it.
    pub async fn import_branch(
        &self,
        src: &std::path::Path,
        name: Option<String>,
        actor: Option<&str>,
    ) -> Result<VmBranch> {
        let raw = std::fs::read(src.join(BRANCH_FILE))
            .with_context(|| format!("{} is not a branch bundle", src.display()))?;
        let mut file: BranchFile =
            serde_json::from_slice(&raw).context("reading the bundle's branch.json")?;
        let state = fluxvm_apple::snapshot_dir(src, CKPT_TAG).join(fluxvm_apple::STATE_FILE);
        if !state.is_file() {
            bail!("the bundle has no saved state");
        }
        let name = name.unwrap_or_else(|| file.branch.name.clone());
        validate_vm_name(&name)?;
        if self
            .list_branches(Some(file.branch.source_vm))
            .await?
            .iter()
            .any(|b| b.name == name)
        {
            bail!(
                "VM {} already has a branch named {name:?}; pass another name",
                file.branch.source_vm
            );
        }
        let id = Uuid::new_v4();
        let dir = self.branch_dir(id);
        let partial = self.branches_dir().join(format!(".{id}.partial"));
        std::fs::create_dir_all(self.branches_dir())?;
        set_private(&self.branches_dir());
        let (from, p) = (src.to_path_buf(), partial.clone());
        tokio::task::spawn_blocking(move || clone_tree(&from, &p))
            .await
            .context("branch import panicked")??;

        let old_ws = file.record.workspace.clone();
        file.branch.id = id;
        file.branch.name = name;
        file.branch.parent_branch = None;
        file.record.workspace = dir.clone();
        if let Ok(rel) = file.record.disk.strip_prefix(&old_ws) {
            file.record.disk = dir.join(rel);
        }
        let written = (|| -> Result<()> {
            let path = partial.join(BRANCH_FILE);
            std::fs::remove_file(&path)?;
            write_private(&path, &serde_json::to_vec_pretty(&file)?)?;
            std::fs::rename(&partial, &dir).context("publishing the branch")?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&partial);
            return Err(e);
        }
        audit_event(
            "vm.branch.import",
            &[
                ("branch_id", &id.to_string()),
                ("name", &file.branch.name),
                ("from", &src.display().to_string()),
                ("actor", actor.unwrap_or("")),
            ],
        );
        Ok(file.branch)
    }
}

#[cfg(test)]
mod prune_tests {
    use super::*;

    fn b(name: &str, age_secs: i64, source: u128, keep: bool, now: DateTime<Utc>) -> VmBranch {
        let mut labels = BTreeMap::new();
        if keep {
            labels.insert(KEEP_LABEL.to_string(), "true".to_string());
        }
        VmBranch {
            id: Uuid::new_v4(),
            name: name.into(),
            source_vm: Uuid::from_u128(source),
            source_name: "src".into(),
            parent_branch: None,
            created_at: now - chrono::Duration::seconds(age_secs),
            labels,
            vcpus: 2,
            memory_mib: 2048,
            state_bytes: 100,
        }
    }

    fn names(v: &[PruneCandidate]) -> Vec<String> {
        let mut n: Vec<_> = v.iter().map(|c| c.name.clone()).collect();
        n.sort();
        n
    }

    #[test]
    fn an_empty_policy_is_refused() {
        let now = Utc::now();
        assert!(select_prune(&[b("a", 10, 1, false, now)], &PrunePolicy::default(), now).is_err());
        let zero = PrunePolicy {
            keep_last: Some(0),
            ..Default::default()
        };
        assert!(select_prune(&[b("a", 10, 1, false, now)], &zero, now).is_err());
    }

    #[test]
    fn keep_last_keeps_the_newest() {
        let now = Utc::now();
        let all = vec![
            b("old", 300, 1, false, now),
            b("mid", 200, 1, false, now),
            b("new", 100, 1, false, now),
        ];
        let p = PrunePolicy {
            keep_last: Some(2),
            ..Default::default()
        };
        let (sel, prot) = select_prune(&all, &p, now).unwrap();
        assert_eq!(names(&sel), vec!["old"]);
        assert_eq!(prot, 0);
    }

    #[test]
    fn both_limits_must_hold() {
        let now = Utc::now();
        let all = vec![
            b("a", 5000, 1, false, now),
            b("b", 4000, 1, false, now),
            b("c", 10, 1, false, now),
        ];
        // Beyond the newest 1 are a and b; only a is older than 4500 s.
        let p = PrunePolicy {
            keep_last: Some(1),
            older_than_secs: Some(4500),
            ..Default::default()
        };
        let (sel, _) = select_prune(&all, &p, now).unwrap();
        assert_eq!(names(&sel), vec!["a"]);
    }

    #[test]
    fn scope_and_the_keep_label() {
        let now = Utc::now();
        let all = vec![
            b("web-1", 900, 1, false, now),
            b("web-2", 800, 1, true, now),
            b("db-1", 900, 2, false, now),
        ];
        let p = PrunePolicy {
            older_than_secs: Some(60),
            source_vm: Some(Uuid::from_u128(1)),
            ..Default::default()
        };
        let (sel, prot) = select_prune(&all, &p, now).unwrap();
        assert_eq!(
            names(&sel),
            vec!["web-1"],
            "db-1 is out of scope, web-2 is protected"
        );
        assert_eq!(prot, 1);
        let by_prefix = PrunePolicy {
            older_than_secs: Some(60),
            name_prefix: Some("db".into()),
            ..Default::default()
        };
        assert_eq!(
            names(&select_prune(&all, &by_prefix, now).unwrap().0),
            vec!["db-1"]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(dir: &std::path::Path) -> Arc<VmManager> {
        let cfg = fluxvm_core::config::Config {
            state_dir: dir.join("state"),
            run_dir: dir.join("run"),
            ..fluxvm_core::config::Config::default()
        };
        VmManager::new(cfg).unwrap()
    }

    fn record(id: Uuid) -> VmRecord {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": "src", "backend": "vz", "status": "running", "pid": null,
            "created_at": "2026-01-01T00:00:00Z", "expires_at": null,
            "workspace": "/tmp/w", "disk": "/tmp/w/root.raw", "seed_disk": null,
            "tap_name": null, "control_socket": null, "log_path": "/tmp/w/console.log", "error": null,
            "request": {"name": "src", "image": "/i", "vcpus": 1, "memory_mib": 256, "backend": "vz",
                        "network": {"mode": "user"}, "apple": {"usernet": {}},
                        "agent": {"enabled": true, "token": "secret-token-0123456789"}}
        }))
        .unwrap()
    }

    fn put(m: &VmManager, name: &str, source: Uuid, age: i64) -> Uuid {
        let id = Uuid::new_v4();
        let dir = m.branch_dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        let file = BranchFile {
            branch: VmBranch {
                id,
                name: name.into(),
                source_vm: source,
                source_name: "src".into(),
                parent_branch: None,
                created_at: Utc::now() - chrono::Duration::seconds(age),
                labels: BTreeMap::new(),
                vcpus: 1,
                memory_mib: 256,
                state_bytes: 5,
            },
            record: record(source),
        };
        write_private(&dir.join(BRANCH_FILE), &serde_json::to_vec(&file).unwrap()).unwrap();
        id
    }

    #[tokio::test]
    async fn branches_list_by_vm_oldest_first_and_survive_their_vm() {
        let td = tempfile::tempdir().unwrap();
        let m = manager(td.path());
        let (vm_a, vm_b) = (Uuid::new_v4(), Uuid::new_v4());
        let newer = put(&m, "newer", vm_a, 10);
        let older = put(&m, "older", vm_a, 100);
        put(&m, "other", vm_b, 50);
        // Debris that must be ignored, not break listing.
        std::fs::create_dir_all(m.branches_dir().join(".x.partial")).unwrap();
        std::fs::create_dir_all(m.branches_dir().join("not-a-uuid")).unwrap();

        let of_a = m.list_branches(Some(vm_a)).await.unwrap();
        assert_eq!(
            of_a.iter().map(|b| b.id).collect::<Vec<_>>(),
            vec![older, newer]
        );
        assert_eq!(m.list_branches(None).await.unwrap().len(), 3);
        // vm_a was never in the store: the branch is still there and readable.
        assert_eq!(m.get_branch(older).await.unwrap().name, "older");
    }

    #[tokio::test]
    async fn the_api_view_carries_no_token_and_the_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let m = manager(td.path());
        let id = put(&m, "b", Uuid::new_v4(), 1);
        let shown = serde_json::to_string(&m.get_branch(id).await.unwrap()).unwrap();
        assert!(!shown.contains("secret-token"), "{shown}");
        let mode = std::fs::metadata(m.branch_dir(id).join(BRANCH_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn delete_removes_only_that_branch_and_unknown_ids_error() {
        let td = tempfile::tempdir().unwrap();
        let m = manager(td.path());
        let src = Uuid::new_v4();
        let (a, b) = (put(&m, "a", src, 2), put(&m, "b", src, 1));
        m.delete_branch(a, None).await.unwrap();
        assert!(m.get_branch(a).await.is_err());
        assert_eq!(m.get_branch(b).await.unwrap().name, "b");
        assert!(m.delete_branch(a, None).await.is_err());
        assert!(m.get_branch(Uuid::new_v4()).await.is_err());
    }

    #[tokio::test]
    async fn a_branch_of_an_unknown_or_non_vz_vm_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let m = manager(td.path());
        let err = m
            .create_branch(Uuid::new_v4(), "x".into(), BTreeMap::new(), None)
            .await
            .unwrap_err();
        assert!(!format!("{err:#}").is_empty());
        let err = m
            .create_branch(Uuid::new_v4(), "Bad Name".into(), BTreeMap::new(), None)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("name"), "{err:#}");
    }

    fn with_state(m: &VmManager, id: Uuid) {
        let st = fluxvm_apple::snapshot_dir(&m.branch_dir(id), CKPT_TAG);
        std::fs::create_dir_all(&st).unwrap();
        std::fs::write(st.join(fluxvm_apple::STATE_FILE), b"state").unwrap();
        std::fs::write(m.branch_dir(id).join("root.raw"), b"disk").unwrap();
    }

    #[tokio::test]
    async fn export_then_import_makes_an_independent_branch() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path());
        let id = put(&m, "clean", Uuid::new_v4(), 10);
        with_state(&m, id);
        let out = dir.path().join("bundle");
        m.export_branch(id, &out, None).await.unwrap();
        // Refuses to overwrite.
        assert!(m.export_branch(id, &out, None).await.is_err());
        // Same source VM and name: refused until renamed.
        let err = m.import_branch(&out, None, None).await.unwrap_err();
        assert!(err.to_string().contains("already has a branch"), "{err}");
        let imp = m
            .import_branch(&out, Some("clean-copy".into()), None)
            .await
            .unwrap();
        assert_ne!(imp.id, id);
        assert_eq!(imp.name, "clean-copy");
        let f = m.read_branch_file(imp.id).unwrap();
        assert_eq!(f.record.workspace, m.branch_dir(imp.id));
        assert_eq!(f.record.disk, m.branch_dir(imp.id).join("root.raw"));
        assert!(
            fluxvm_apple::snapshot_dir(&m.branch_dir(imp.id), CKPT_TAG)
                .join(fluxvm_apple::STATE_FILE)
                .is_file()
        );
        // Deleting the import leaves the original alone.
        m.delete_branch(imp.id, None).await.unwrap();
        assert!(m.get_branch(id).await.is_ok());
        // A directory that is not a bundle is refused.
        assert!(m.import_branch(dir.path(), None, None).await.is_err());
    }

    #[test]
    fn diff_reports_sizes_lineage_and_labels() {
        let mk = |name: &str, parent: Option<Uuid>, mem: u64, labels: &[(&str, &str)]| VmBranch {
            id: Uuid::new_v4(),
            name: name.into(),
            source_vm: Uuid::nil(),
            source_name: "src".into(),
            parent_branch: parent,
            created_at: Utc::now(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            vcpus: 2,
            memory_mib: mem,
            state_bytes: mem * 10,
        };
        let a = mk("a", None, 512, &[("env", "dev"), ("gone", "1")]);
        let b = mk("b", Some(a.id), 1024, &[("env", "prod"), ("new", "2")]);
        let d = diff_branches(&a, &b);
        assert_eq!(d.lineage, "a-is-parent-of-b");
        assert_eq!(d.memory_mib, (512, 1024));
        assert!(d.same_source_vm);
        assert_eq!(d.labels_added.get("new").map(String::as_str), Some("2"));
        assert_eq!(d.labels_removed.get("gone").map(String::as_str), Some("1"));
        assert_eq!(
            d.labels_changed.get("env"),
            Some(&("dev".to_string(), "prod".to_string()))
        );
        assert_eq!(diff_branches(&b, &a).lineage, "b-is-parent-of-a");
        let c = mk("c", Some(a.id), 512, &[]);
        assert_eq!(diff_branches(&b, &c).lineage, "siblings");
        assert_eq!(
            diff_branches(&a, &mk("z", None, 1, &[])).lineage,
            "unrelated"
        );
    }
}
