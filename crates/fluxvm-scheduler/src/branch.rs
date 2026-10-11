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
//! Not here yet: committing a branch's disk back, rebasing, flattening, a disk diff.

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
}
