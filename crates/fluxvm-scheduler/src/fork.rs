// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Live fork of a running flux-vm VM into N new VMs.
//!
//! One `SnapshotSave` of the parent (a pause of a few milliseconds plus a
//! reflink of its rootfs), then every child is restored from it:
//!
//! * `snap.mem` and `snap.vmstate` are shared read-only. The in-tree KVM
//!   engine maps `snap.mem` `MAP_PRIVATE` (clean pages shared between
//!   children, written pages copied on write) and falls back to an eager copy;
//!   either way the files can be unlinked once every child runs.
//! * each child gets its own reflink of `snap.rootfs` as its `root.raw`, its
//!   own workspace, vsock socket and CID, and its own tap from
//!   `fluxvm_network::prepare`.
//!
//! Only the KVM engine is forkable: a Firecracker vmstate names the parent's
//! drive and tap by absolute host path, so a restored child would open the
//! parent's disk. The guest keeps the parent's MAC and IP, which is why fork
//! requires per-VM network namespaces (or no tap at all): two VMs with one
//! MAC never share an L2 segment. After resume, `fork_child` calls the guest
//! agent's `ResetIdentity` so each child gets its own hostname, machine-id and
//! fresh entropy (best effort: a failure is logged loudly and audited).

use crate::journal::{ForkPhase, OpGuard, OpKind, Resource};
use crate::{FIRST_GUEST_CID, VmManager, audit_event, validate_vm_name};
use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use fluxvm_core::model::{BackendKind, NetworkSpec, StorageBackend, VmRecord, VmStatus};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

/// Upper bound on children per call.
pub const MAX_FORK_COUNT: u32 = 32;

/// Label every child carries, naming the VM it was forked from.
pub const FORKED_FROM_LABEL: &str = "fluxvm.dev/forked-from";

/// How hard a fork tries to make each child a different machine from its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForkIdentityMode {
    /// Leave the child exactly as the parent was (debugging, deterministic replay).
    Preserve,
    /// Try to reset hostname, machine-id, entropy, host keys, cloud-init, DHCP, network state and the agent token;
    /// a failure is logged and audited and the child is kept (the behaviour before fork v2).
    BestEffort,
    /// Every step must be confirmed or the whole fork is rolled back. When the vsock agent is absent or too old to
    /// confirm the steps, the reset is run over SSH; when that is not possible either, the fork is refused.
    #[default]
    Unique,
}

/// What network each child gets. A child resumes the parent's saved NIC, so the choices are limited to what the saved
/// device state tolerates; see `check_vz_forkable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForkNetworkMode {
    /// Whatever isolates children: the parent's own per-VM usernet, or none.
    #[default]
    Auto,
    /// Require that every child owns its network (a per-VM usernet or none); refuse otherwise.
    Isolated,
    /// Require the parent to have no network.
    None,
    /// Accept exactly what the backend's own rules accept, as before fork v2.
    Preserve,
}

fn one() -> u32 {
    1
}

/// What to fork and how: the request body of `POST /v1/vms/{id}/fork`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ForkSpec {
    #[serde(default = "one")]
    pub count: u32,
    #[serde(default, alias = "namePrefix")]
    pub name_prefix: Option<String>,
    #[serde(default)]
    pub identity: ForkIdentityMode,
    #[serde(default)]
    pub network: ForkNetworkMode,
    /// Children restored at the same time (vz only; the KVM path is sequential). 1..=32, default `min(count, 8)`.
    #[serde(default)]
    pub parallelism: Option<u8>,
}

impl Default for ForkSpec {
    fn default() -> Self {
        Self {
            count: 1,
            name_prefix: None,
            identity: ForkIdentityMode::default(),
            network: ForkNetworkMode::default(),
            parallelism: None,
        }
    }
}

/// How one child's identity was reset.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityOutcome {
    /// `agent`, `ssh`, `preserved`, or `none` when a best-effort reset did not run.
    pub via: String,
    /// Steps the guest confirmed.
    pub applied: Vec<String>,
    /// Whether the child's agent token was replaced (and the record updated).
    pub token_rotated: bool,
    pub ms: u64,
}

/// Per-child timings and identity result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkChildReport {
    pub id: Uuid,
    pub name: String,
    /// Admission, file cloning and record insertion.
    pub prepare_ms: u64,
    /// `start_from_snapshot`: the runner restoring the saved state.
    pub restore_ms: u64,
    pub identity: IdentityOutcome,
    pub total_ms: u64,
}

/// The result of a fork: the children and where the time went.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkReport {
    pub children: Vec<VmRecord>,
    pub reports: Vec<ForkChildReport>,
    /// How long the parent was held for the save and its disk clone (an upper bound on its pause).
    pub parent_snapshot_ms: u64,
    pub total_ms: u64,
    pub parallelism: u8,
}

/// The identity steps `Unique` requires the guest to confirm.
const REQUIRED_STEPS: &[&str] = &[
    "hostname",
    "machine-id",
    "entropy",
    "ssh-host-keys",
    "cloud-init",
    "dhcp",
    "network-state",
];

/// The address a usernet guest always leases (see `fluxvm-usernet`).
const USERNET_GUEST_IP: &str = "10.0.2.15";

impl VmManager {
    /// Fork running VM `id` into `count` new running VMs named `{name_prefix}-{n}` with the pre-v2 best-effort identity
    /// reset (internal callers such as speculation). Use [`Self::fork_vm_spec`] for the full contract.
    pub async fn fork_vm(
        self: &Arc<Self>,
        id: Uuid,
        count: u32,
        name_prefix: Option<String>,
        actor: Option<&str>,
    ) -> Result<Vec<VmRecord>> {
        let spec = ForkSpec {
            count,
            name_prefix,
            identity: ForkIdentityMode::BestEffort,
            network: ForkNetworkMode::Preserve,
            parallelism: None,
        };
        Ok(self.fork_vm_spec(id, spec, actor).await?.children)
    }

    /// Fork running VM `id` as `spec` says. All-or-nothing: if any child fails (including a failed identity reset under
    /// [`ForkIdentityMode::Unique`]), every child created by this call is deleted. The parent keeps running throughout.
    pub async fn fork_vm_spec(
        self: &Arc<Self>,
        id: Uuid,
        spec: ForkSpec,
        actor: Option<&str>,
    ) -> Result<ForkReport> {
        let r = self.fork_vm_spec_inner(id, spec, actor).await;
        match &r {
            Ok(rep) => fluxvm_core::metrics::record_fork(
                rep.children.len() as u64,
                rep.parent_snapshot_ms,
                rep.total_ms,
                rep.reports.iter().map(|c| c.restore_ms).sum(),
                rep.reports.iter().map(|c| c.identity.ms).sum(),
            ),
            Err(_) => fluxvm_core::metrics::record_fork_failed(),
        }
        r
    }

    async fn fork_vm_spec_inner(
        self: &Arc<Self>,
        id: Uuid,
        spec: ForkSpec,
        actor: Option<&str>,
    ) -> Result<ForkReport> {
        let started = Instant::now();
        let src = self.get(id).await?;
        if let Some(p) = spec.parallelism
            && !(1..=MAX_FORK_COUNT as u8).contains(&p)
        {
            bail!("parallelism must be between 1 and {MAX_FORK_COUNT}");
        }
        if src.backend == BackendKind::Vz {
            return self.fork_vm_vz(src, spec, actor, started).await;
        }
        check_forkable(&src, spec.count)?;
        self.admit_fork_batch(&src, spec.count).await?;
        let count = spec.count;
        let prefix = spec
            .name_prefix
            .clone()
            .unwrap_or_else(|| format!("{}-fork", src.name));
        let names: Vec<String> = (1..=count).map(|n| format!("{prefix}-{n}")).collect();
        for name in &names {
            validate_vm_name(name)?;
        }

        let tag = format!("fork-{}", &Uuid::new_v4().simple().to_string()[..12]);
        // Journaled so a crash rolls back the parent's fork snapshot and every
        // child recorded below (fork is all-or-nothing).
        let op = OpGuard::begin(
            &self.cfg.state_dir,
            OpKind::Fork,
            Uuid::new_v4(),
            id,
            vec![Resource::SnapshotDir {
                path: src.workspace.join("snapshots").join(&tag),
            }],
        )
        .context("journaling fork intent")?;
        op.fork_expect(count).context("journaling fork size")?;
        let snap_started = Instant::now();
        self.create_vm_snapshot(id, &tag).await?;
        let parent_snapshot_ms = snap_started.elapsed().as_millis() as u64;
        let meta = src.workspace.join("snapshots").join(&tag).join("snap");
        let result = self
            .fork_children(&src, &meta, &tag, &names, actor, &op, spec.identity)
            .await;
        if let Err(e) = self.delete_vm_snapshot(id, &tag).await {
            tracing::warn!(vm = %id, tag, error = %e, "removing fork snapshot failed");
        }
        // Children are either all running or already deleted by now.
        op.finish();
        let done = result?;
        let (children, reports): (Vec<_>, Vec<_>) = done.into_iter().unzip();
        let ids: Vec<String> = children.iter().map(|c| c.id.to_string()).collect();
        audit_event(
            "vm.fork",
            &[
                ("vm_id", &id.to_string()),
                ("count", &count.to_string()),
                ("children", &ids.join(",")),
                ("identity", &format!("{:?}", spec.identity)),
                ("parent_snapshot_ms", &parent_snapshot_ms.to_string()),
            ],
        );
        Ok(ForkReport {
            children,
            reports,
            parent_snapshot_ms,
            total_ms: started.elapsed().as_millis() as u64,
            parallelism: 1,
        })
    }

    /// Tenant and host admission for one more child of `src` (the same limits a create of that size meets).
    async fn admit_fork_child(&self, src: &VmRecord) -> Result<()> {
        self.admit_fork_batch(src, 1).await
    }

    /// Admission for `count` children at once: the checks a create of that size meets, against the ledger as it would
    /// be once the first `count - 1` children exist. Done before the parent is paused, so a batch that cannot fit is
    /// refused up front instead of failing child by child.
    async fn admit_fork_batch(&self, src: &VmRecord, count: u32) -> Result<()> {
        let mut ledger = self.store.quota_ledger().await?;
        let extra = u64::from(count.saturating_sub(1));
        if extra > 0 {
            let (v, m) = (
                u64::from(src.request.vcpus) * extra,
                src.request.memory_mib * extra,
            );
            ledger.host.vms += extra;
            ledger.host.vcpus += v;
            ledger.host.memory_mib += m;
            if let Some(t) = src.request.tenant.as_deref() {
                let e = ledger.tenants.entry(t.to_string()).or_default();
                e.vms += extra;
                e.vcpus += v;
                e.memory_mib += m;
            }
        }
        if let Some(tenant) = src.request.tenant.as_deref()
            && !self.cfg.policy.tenants.is_empty()
        {
            fluxvm_core::policy::enforce_tenant_totals(
                tenant,
                src.request.vcpus,
                src.request.memory_mib,
                &self.cfg.policy,
                &ledger,
            )?;
        }
        fluxvm_core::policy::enforce_host_totals(
            src.request.vcpus,
            src.request.memory_mib,
            &self.cfg.policy,
            &fluxvm_core::policy::ledger_for_host_admission(&ledger),
        )?;
        Ok(())
    }

    /// Fork a running `vz` (Apple Virtualization.framework) Linux guest into `count` running children.
    ///
    /// One runner `save` pauses the parent for a moment and clones its disk (`snapshot_save`); every child gets APFS
    /// clones of that state and disk and the parent's machine identifier and MAC (the saved device state is bound to
    /// them), and is restored through the ordinary `start_from_snapshot`, up to `parallelism` at a time. Children
    /// share the parent's MAC and address, so each must own its network: `network.mode = none`, or `apple.usernet` (a
    /// private userspace NAT per VM). Restoring the saved state onto a different attachment than it was saved with
    /// (the Mac's NAT to a usernet NIC) was tried on hardware: the VM runs but the NIC comes up disconnected.
    async fn fork_vm_vz(
        self: &Arc<Self>,
        src: VmRecord,
        spec: ForkSpec,
        actor: Option<&str>,
        started: Instant,
    ) -> Result<ForkReport> {
        check_vz_forkable(&src, spec.count)?;
        check_fork_network_mode(&src, spec.network)?;
        self.admit_fork_batch(&src, spec.count).await?;
        let id = src.id;
        let count = spec.count;
        let prefix = spec
            .name_prefix
            .clone()
            .unwrap_or_else(|| format!("{}-fork", src.name));
        let names: Vec<String> = (1..=count).map(|n| format!("{prefix}-{n}")).collect();
        for name in &names {
            validate_vm_name(name)?;
        }
        let parallelism = spec
            .parallelism
            .unwrap_or_else(|| count.min(8) as u8)
            .clamp(1, MAX_FORK_COUNT as u8);
        let tag = format!("fork-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let op = Arc::new(
            OpGuard::begin(
                &self.cfg.state_dir,
                OpKind::Fork,
                Uuid::new_v4(),
                id,
                vec![Resource::SnapshotDir {
                    path: src.workspace.join("snapshots").join(&tag),
                }],
            )
            .context("journaling fork intent")?,
        );
        op.fork_expect(count).context("journaling fork size")?;
        let snap_started = Instant::now();
        self.create_vm_snapshot(id, &tag).await?;
        let parent_snapshot_ms = snap_started.elapsed().as_millis() as u64;

        let created: Arc<std::sync::Mutex<Vec<Uuid>>> = Arc::default();
        let sem = Arc::new(tokio::sync::Semaphore::new(usize::from(parallelism)));
        let mut set = tokio::task::JoinSet::new();
        let src = Arc::new(src);
        let actor_owned = actor.map(str::to_owned);
        for name in names {
            let (this, src, tag, op, sem, created, actor) = (
                Arc::clone(self),
                Arc::clone(&src),
                tag.clone(),
                Arc::clone(&op),
                Arc::clone(&sem),
                Arc::clone(&created),
                actor_owned.clone(),
            );
            let identity = spec.identity;
            set.spawn(async move {
                let _permit = sem.acquire_owned().await;
                let r = this
                    .fork_child_vz(&src, &tag, &name, actor.as_deref(), &op, identity, &created)
                    .await;
                (name, r)
            });
        }
        let mut done: Vec<(VmRecord, ForkChildReport)> = Vec::new();
        let mut failure: Option<anyhow::Error> = None;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((_, Ok(child))) => done.push(child),
                Ok((name, Err(e))) => {
                    if failure.is_none() {
                        failure = Some(e.context(format!("forking child {name}")));
                        // Children not yet started need not be; the ones in flight finish and are rolled back below.
                        sem.close();
                    }
                }
                Err(join) => {
                    if failure.is_none() {
                        failure = Some(anyhow::anyhow!("a fork task failed: {join}"));
                        sem.close();
                    }
                }
            }
        }
        if let Some(e) = failure {
            // All-or-nothing, as for the KVM fork: every child this call created, started or not.
            let ids: Vec<Uuid> = created.lock().map(|g| g.clone()).unwrap_or_default();
            for child in ids {
                let _ = self.delete(child).await;
            }
            if let Err(e) = self.delete_vm_snapshot(id, &tag).await {
                tracing::warn!(vm = %id, tag, error = %e, "removing fork snapshot failed");
            }
            if let Ok(op) = Arc::try_unwrap(op) {
                op.finish();
            }
            return Err(e);
        }
        if let Err(e) = self.delete_vm_snapshot(id, &tag).await {
            tracing::warn!(vm = %id, tag, error = %e, "removing fork snapshot failed");
        }
        if let Ok(op) = Arc::try_unwrap(op) {
            op.finish();
        }
        done.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let (children, reports): (Vec<_>, Vec<_>) = done.into_iter().unzip();
        let ids: Vec<String> = children.iter().map(|c| c.id.to_string()).collect();
        audit_event(
            "vm.fork",
            &[
                ("vm_id", &id.to_string()),
                ("count", &count.to_string()),
                ("children", &ids.join(",")),
                ("backend", "vz"),
                ("identity", &format!("{:?}", spec.identity)),
                ("parallelism", &parallelism.to_string()),
                ("parent_snapshot_ms", &parent_snapshot_ms.to_string()),
            ],
        );
        Ok(ForkReport {
            children,
            reports,
            parent_snapshot_ms,
            total_ms: started.elapsed().as_millis() as u64,
            parallelism,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn fork_child_vz(
        self: &Arc<Self>,
        src: &VmRecord,
        tag: &str,
        name: &str,
        actor: Option<&str>,
        op: &OpGuard,
        identity: ForkIdentityMode,
        created: &std::sync::Mutex<Vec<Uuid>>,
    ) -> Result<(VmRecord, ForkChildReport)> {
        let t0 = Instant::now();
        self.admit_fork_child(src).await?;
        let id = Uuid::new_v4();
        op.record(Resource::ForkChild { vm_id: id })
            .context("journaling fork child")?;
        if let Ok(mut g) = created.lock() {
            g.push(id);
        }
        let workspace = self.cfg.state_dir.join("instances").join(id.to_string());
        std::fs::create_dir_all(&workspace)?;
        let (parent_ws, child_ws, tag_owned) =
            (src.workspace.clone(), workspace.clone(), tag.to_string());
        let disk = tokio::task::spawn_blocking(move || {
            fluxvm_apple::prepare_fork_child(&parent_ws, &tag_owned, &child_ws)
        })
        .await
        .context("fork child file preparation panicked")?;
        let disk = match disk {
            Ok(d) => d,
            Err(e) => {
                let _ = self.delete(id).await;
                let _ = std::fs::remove_dir_all(&workspace);
                return Err(e.context("preparing the child's files"));
            }
        };
        let usernet = src
            .request
            .apple
            .as_ref()
            .is_some_and(|a| a.usernet.is_some());
        if usernet {
            // The restored guest keeps the lease the gateway gave its parent and does not print it again; tell the host.
            let _ = std::fs::write(
                fluxvm_apple::ip_file(&workspace),
                format!("{USERNET_GUEST_IP}\n"),
            );
        }
        let mut req = src.request.clone();
        req.name = name.to_string();
        req.loadvm_tag = None;
        if let Some(actor) = actor {
            req.created_by_token = Some(actor.to_string());
        }
        let mut labels = src.labels.clone();
        labels.insert(FORKED_FROM_LABEL.to_string(), src.id.to_string());
        let mut record = src.clone();
        record.id = id;
        record.name = req.name.clone();
        record.status = VmStatus::Stopped;
        record.pid = None;
        record.created_at = Utc::now();
        record.expires_at = req
            .ttl_seconds
            .map(|s| Utc::now() + Duration::seconds(s as i64));
        record.workspace = workspace.clone();
        record.disk = disk;
        record.seed_disk = src
            .seed_disk
            .as_ref()
            .map(|_| workspace.join("seed.img"))
            .filter(|p| p.is_file());
        record.control_socket = None;
        record.log_path = workspace.join("console.log");
        record.error = None;
        record.request = req;
        record.tap_name = None;
        record.netns = None;
        record.vsock_socket = None;
        record.dhcp_leasefile = None;
        record.guest_ip = None;
        record.labels = labels;
        let needs_cid = src.guest_cid.is_some();
        if let Err(e) = self
            .store
            .insert_with_cid(record, needs_cid, FIRST_GUEST_CID)
            .await
        {
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(e);
        }
        let prepare_ms = t0.elapsed().as_millis() as u64;
        let _ = op.fork_phase(id, ForkPhase::Prepared);
        let t1 = Instant::now();
        let started = self.start_from_snapshot(id, tag).await;
        let _ = std::fs::remove_dir_all(workspace.join("snapshots").join(tag));
        let restore_ms = t1.elapsed().as_millis() as u64;
        let vm = match started {
            Ok(vm) => vm,
            Err(e) => {
                let _ = self.delete(id).await;
                return Err(e);
            }
        };
        let _ = op.fork_phase(id, ForkPhase::Restored);
        let outcome = match self.reset_child_identity(&vm, name, identity).await {
            Ok(o) => o,
            Err(e) => {
                let _ = self.delete(id).await;
                return Err(e.context("identity reset"));
            }
        };
        let _ = op.fork_phase(id, ForkPhase::IdentityReset);
        // Re-read: the reset may have rotated the token on the stored record.
        let vm = self.get(id).await.unwrap_or(vm);
        let report = ForkChildReport {
            id,
            name: name.to_string(),
            prepare_ms,
            restore_ms,
            identity: outcome,
            total_ms: t0.elapsed().as_millis() as u64,
        };
        Ok((vm, report))
    }

    async fn fork_children(
        self: &Arc<Self>,
        src: &VmRecord,
        meta: &Path,
        tag: &str,
        names: &[String],
        actor: Option<&str>,
        op: &OpGuard,
        identity: ForkIdentityMode,
    ) -> Result<Vec<(VmRecord, ForkChildReport)>> {
        let spec = fluxvm_hypervisor::snapshot::load_spec(meta)?;
        if spec.boot.engine != fluxvm_hypervisor::api::FluxVmEngine::Kvm {
            bail!(
                "fork needs the in-tree KVM engine (fluxvm_engine = \"kvm\"): a Firecracker \
                 snapshot names the parent's disk and tap by path, so a child would share them"
            );
        }
        let mut children = Vec::with_capacity(names.len());
        for name in names {
            match self
                .fork_child(src, &spec, tag, name, actor, op, identity)
                .await
            {
                Ok(child) => children.push(child),
                Err(e) => {
                    for (child, _) in &children {
                        let _ = self.delete(child.id).await;
                    }
                    return Err(e.context(format!("forking child {name}")));
                }
            }
        }
        Ok(children)
    }

    async fn fork_child(
        self: &Arc<Self>,
        src: &VmRecord,
        spec: &fluxvm_hypervisor::api::SnapshotSpec,
        tag: &str,
        name: &str,
        actor: Option<&str>,
        op: &OpGuard,
        identity: ForkIdentityMode,
    ) -> Result<(VmRecord, ForkChildReport)> {
        let t0 = Instant::now();
        self.admit_fork_child(src).await?;

        let id = Uuid::new_v4();
        op.record(Resource::ForkChild { vm_id: id })
            .context("journaling fork child")?;
        let workspace = self.cfg.state_dir.join("instances").join(id.to_string());
        std::fs::create_dir_all(&workspace)?;
        let disk = workspace.join("root.raw");
        let mut req = src.request.clone();
        req.name = name.to_string();
        req.loadvm_tag = None;
        if let Some(actor) = actor {
            req.created_by_token = Some(actor.to_string());
        }
        if let NetworkSpec::Tap { mac, tap_name, .. } = &mut req.network {
            *mac = None;
            *tap_name = None;
        }
        let mut labels = src.labels.clone();
        labels.insert(FORKED_FROM_LABEL.to_string(), src.id.to_string());
        let placeholder = VmRecord {
            id,
            name: req.name.clone(),
            backend: BackendKind::FluxVm,
            status: VmStatus::Stopped,
            pid: None,
            created_at: Utc::now(),
            expires_at: req
                .ttl_seconds
                .map(|s| Utc::now() + Duration::seconds(s as i64)),
            workspace: workspace.clone(),
            disk: disk.clone(),
            seed_disk: None,
            tap_name: None,
            control_socket: None,
            log_path: workspace.join("console.log"),
            error: None,
            request: req,
            guest_cid: None,
            jail_path: None,
            vsock_socket: None,
            qga_socket: None,
            cgroup_path: None,
            netns: None,
            lvm_lv: None,
            nbd_pid: None,
            virtiofsd_pids: Vec::new(),
            swtpm_pid: None,
            dhcp_leasefile: None,
            guest_ip: None,
            requested_security_profile: src.requested_security_profile,
            achieved_security_profile: src.achieved_security_profile,
            security_evidence: None,
            labels,
        };
        let needs_cid = src.guest_cid.is_some();
        let mut record = self
            .store
            .insert_with_cid(placeholder, needs_cid, FIRST_GUEST_CID)
            .await?;

        let prepared: Result<()> = async {
            fluxvm_hypervisor::snapshot::clone_cow(&spec.disk_path, &disk)
                .with_context(|| format!("cloning snapshot rootfs to {}", disk.display()))?;
            let vsock = record.guest_cid.map(|_| workspace.join("vsock.sock"));
            record.vsock_socket = vsock.clone();
            let network = fluxvm_network::prepare(&self.cfg, id, &record.request.network).await?;
            let mut boot = spec.boot.clone();
            boot.rootfs = disk.clone();
            boot.seed = None;
            boot.tap = network.tap_name.clone();
            boot.vsock_cid = record.guest_cid;
            boot.vsock_uds = vsock;
            let child_spec = fluxvm_hypervisor::api::SnapshotSpec {
                memory_path: spec.memory_path.clone(),
                disk_path: disk.clone(),
                vmstate_path: spec.vmstate_path.clone(),
                boot,
            };
            let dir = workspace.join("snapshots").join(tag);
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("snap"), serde_json::to_vec_pretty(&child_spec)?)?;
            self.store.update(record.clone()).await?;
            Ok(())
        }
        .await;
        if let Err(e) = prepared {
            let _ = self.delete(id).await;
            return Err(e);
        }

        let prepare_ms = t0.elapsed().as_millis() as u64;
        let _ = op.fork_phase(id, ForkPhase::Prepared);
        let t1 = Instant::now();
        let started = self.start_from_snapshot(id, tag).await;
        let _ = std::fs::remove_dir_all(workspace.join("snapshots").join(tag));
        let restore_ms = t1.elapsed().as_millis() as u64;
        let vm = match started {
            Ok(vm) => vm,
            Err(e) => {
                let _ = self.delete(id).await;
                return Err(e);
            }
        };
        let _ = op.fork_phase(id, ForkPhase::Restored);
        let outcome = match self.reset_child_identity(&vm, name, identity).await {
            Ok(o) => o,
            Err(e) => {
                let _ = self.delete(id).await;
                return Err(e.context("identity reset"));
            }
        };
        let _ = op.fork_phase(id, ForkPhase::IdentityReset);
        let vm = self.get(id).await.unwrap_or(vm);
        let report = ForkChildReport {
            id,
            name: name.to_string(),
            prepare_ms,
            restore_ms,
            identity: outcome,
            total_ms: t0.elapsed().as_millis() as u64,
        };
        Ok((vm, report))
    }
}

/// How long to keep retrying the identity reset while the resumed guest's
/// agent starts answering again.
const IDENTITY_RESET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long the SSH fallback waits for sshd before giving up on it.
const SSH_FALLBACK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

impl VmManager {
    /// Give a resumed child its own identity, as `mode` says, and report how.
    ///
    /// A fork resumes the parent's memory, so without this every child shares its hostname, machine-id, SSH host keys,
    /// cloud-init instance, DHCP identity, RNG state and agent token. [`ForkIdentityMode::Unique`] fails unless the
    /// guest confirmed every step in `REQUIRED_STEPS` (an older agent ignores the new fields and omits their names,
    /// which is how it is recognised). The agent path rotates the agent's token too; the stored record changes only
    /// once the guest confirmed `agent-token`, so the host never holds a token the guest does not.
    async fn reset_child_identity(
        self: &Arc<Self>,
        vm: &VmRecord,
        hostname: &str,
        mode: ForkIdentityMode,
    ) -> Result<IdentityOutcome> {
        use base64::Engine;
        use fluxvm_guest_protocol::{AgentRequest, AgentResponse};

        let t0 = Instant::now();
        if mode == ForkIdentityMode::Preserve {
            return Ok(IdentityOutcome {
                via: "preserved".into(),
                ..Default::default()
            });
        }
        let strict = mode == ForkIdentityMode::Unique;
        // 32 bytes of host entropy and a fresh token; UUIDs avoid a direct rand dependency.
        let mut entropy = Vec::with_capacity(32);
        entropy.extend_from_slice(Uuid::new_v4().as_bytes());
        entropy.extend_from_slice(Uuid::new_v4().as_bytes());
        let entropy_b64 = base64::engine::general_purpose::STANDARD.encode(&entropy);
        let new_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());

        let mut last_problem = String::new();
        if vm.request.agent.as_ref().is_some_and(|a| a.enabled) {
            let request = AgentRequest::ResetIdentity {
                hostname: Some(hostname.to_string()),
                regenerate_machine_id: true,
                reseed_entropy: true,
                entropy_base64: Some(entropy_b64.clone()),
                regenerate_ssh_host_keys: true,
                reset_cloud_init: true,
                reset_dhcp_identity: true,
                flush_network_state: true,
                new_agent_token: Some(new_token.clone()),
            };
            let deadline = tokio::time::Instant::now() + IDENTITY_RESET_TIMEOUT;
            let answer = loop {
                let attempt = fluxvm_vsock_client::call(
                    vm,
                    request.clone(),
                    std::time::Duration::from_secs(5),
                )
                .await;
                match attempt {
                    Ok(AgentResponse::IdentityReset { applied, failures }) => {
                        break Ok((applied, failures));
                    }
                    Ok(AgentResponse::Error { message }) => {
                        break Err(anyhow::anyhow!("guest agent error: {message}"));
                    }
                    Ok(other) => break Err(anyhow::anyhow!("unexpected response: {other:?}")),
                    Err(e) if tokio::time::Instant::now() >= deadline => break Err(e),
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
                }
            };
            match answer {
                Ok((applied, failures)) => {
                    // The guest has the new token once it says so, whatever else failed: record it first.
                    let rotated = applied.iter().any(|s| s == "agent-token");
                    if rotated && let Ok(mut rec) = self.get(vm.id).await {
                        if let Some(a) = rec.request.agent.as_mut() {
                            a.token = Some(new_token.clone());
                        }
                        let _ = self.store.update(rec).await;
                    }
                    let missing: Vec<&str> = REQUIRED_STEPS
                        .iter()
                        .copied()
                        .filter(|r| !applied.iter().any(|a| a == r))
                        .collect();
                    if failures.is_empty() && missing.is_empty() {
                        let o = IdentityOutcome {
                            via: "agent".into(),
                            applied,
                            token_rotated: rotated,
                            ms: t0.elapsed().as_millis() as u64,
                        };
                        audit_event(
                            "vm.fork.identity-reset",
                            &[("vm_id", &vm.id.to_string()), ("via", "agent")],
                        );
                        return Ok(o);
                    }
                    last_problem = format!(
                        "the guest agent confirmed {} but not {}{}",
                        if applied.is_empty() {
                            "nothing".to_string()
                        } else {
                            applied.join(",")
                        },
                        if missing.is_empty() {
                            "(nothing missing)".to_string()
                        } else {
                            missing.join(",")
                        },
                        if failures.is_empty() {
                            String::new()
                        } else {
                            format!("; failures: {}", failures.join("; "))
                        }
                    );
                    if !strict {
                        return Ok(IdentityOutcome {
                            via: "agent".into(),
                            applied,
                            token_rotated: rotated,
                            ms: t0.elapsed().as_millis() as u64,
                        });
                    }
                }
                Err(e) => last_problem = format!("{e:#}"),
            }
        } else {
            last_problem = "the guest agent is not enabled".into();
        }

        // Not confirmed over the agent. Under `Unique`, try SSH before giving up.
        if strict
            && vm.backend == BackendKind::Vz
            && let Some(o) = self
                .reset_identity_over_ssh(vm, hostname, &entropy_b64, t0)
                .await
        {
            return Ok(o);
        }
        let problem = format!(
            "{last_problem}; identity=unique needs the vsock agent (or SSH for a vz guest) to confirm {}. Use \
             identity=best-effort or preserve to fork this guest anyway",
            REQUIRED_STEPS.join(", ")
        );
        fluxvm_core::metrics::record_fork_identity_failed();
        tracing::error!(vm = %vm.id, error = %problem, "fork child identity reset FAILED");
        audit_event(
            "vm.fork.identity-reset-failed",
            &[("vm_id", &vm.id.to_string()), ("error", &problem)],
        );
        if strict {
            bail!("{problem}");
        }
        Ok(IdentityOutcome {
            via: "none".into(),
            ms: t0.elapsed().as_millis() as u64,
            ..Default::default()
        })
    }

    /// The SSH fallback of a strict reset: the same steps as a shell script run as root (or through `sudo -n`).
    /// `None` when sshd cannot be reached or a step is not confirmed. The agent's token is not rotated this way (an
    /// absent or old agent is exactly why we are here), which is reported through `token_rotated: false`.
    async fn reset_identity_over_ssh(
        &self,
        vm: &VmRecord,
        hostname: &str,
        entropy_b64: &str,
        t0: Instant,
    ) -> Option<IdentityOutcome> {
        use fluxvm_apple::ssh;
        let guest = self.vz_guest(vm).ok()?;
        ssh::wait_ready(&guest, SSH_FALLBACK_WAIT).await.ok()?;
        let script = ssh_reset_script(hostname, entropy_b64);
        let out = ssh::exec(&guest, &script, IDENTITY_RESET_TIMEOUT)
            .await
            .ok()?;
        let applied: Vec<String> = out
            .stdout
            .lines()
            .filter_map(|l| l.strip_prefix("STEP:"))
            .map(str::to_owned)
            .collect();
        let missing = REQUIRED_STEPS
            .iter()
            .any(|r| !applied.iter().any(|a| a == r));
        if out.exit_code != 0 || missing {
            tracing::warn!(vm = %vm.id, stdout = %out.stdout.trim(), stderr = %out.stderr.trim(), "SSH identity reset incomplete");
            return None;
        }
        audit_event(
            "vm.fork.identity-reset",
            &[("vm_id", &vm.id.to_string()), ("via", "ssh")],
        );
        Some(IdentityOutcome {
            via: "ssh".into(),
            applied,
            token_rotated: false,
            ms: t0.elapsed().as_millis() as u64,
        })
    }
}

/// The reset script for the SSH fallback. `hostname` is a validated VM name (`[a-z0-9-]`) and `entropy_b64` is base64,
/// so neither needs quoting. Each confirmed step prints `STEP:<name>`; a step that cannot be confirmed prints nothing.
fn ssh_reset_script(hostname: &str, entropy_b64: &str) -> String {
    format!(
        r#"set -u
S=""
[ "$(id -u)" = 0 ] || S="sudo -n"
step() {{ echo "STEP:$1"; }}
printf %s '{entropy_b64}' | base64 -d | $S tee /dev/urandom >/dev/null 2>&1 && step entropy
{{ $S hostname {hostname} && echo {hostname} | $S tee /etc/hostname >/dev/null; }} 2>/dev/null && step hostname
$S rm -f /etc/machine-id
{{ $S systemd-machine-id-setup >/dev/null 2>&1 || {{ tr -d - </proc/sys/kernel/random/uuid | $S tee /etc/machine-id >/dev/null; }}; }} && step machine-id
if [ -d /etc/ssh ]; then
  $S sh -c 'rm -f /etc/ssh/ssh_host_*_key /etc/ssh/ssh_host_*_key.pub; ssh-keygen -A >/dev/null 2>&1; systemctl try-reload-or-restart ssh.service sshd.service >/dev/null 2>&1; true' && step ssh-host-keys
else step ssh-host-keys; fi
if command -v cloud-init >/dev/null 2>&1; then $S cloud-init clean --logs >/dev/null 2>&1 && step cloud-init; else step cloud-init; fi
if systemctl is-active --quiet systemd-networkd 2>/dev/null; then
  $S networkctl reload >/dev/null 2>&1
  $S networkctl reconfigure $(ls /sys/class/net | grep -v '^lo$') >/dev/null 2>&1 && step dhcp
else step dhcp; fi
{{ $S ip neigh flush all; $S ip route flush cache; }} >/dev/null 2>&1 && step network-state
exit 0
"#
    )
}

/// `network` against what a vz parent allows.
fn check_fork_network_mode(src: &VmRecord, mode: ForkNetworkMode) -> Result<()> {
    let isolated = match &src.request.network {
        NetworkSpec::None => true,
        NetworkSpec::User { .. } => src
            .request
            .apple
            .as_ref()
            .is_some_and(|a| a.usernet.is_some()),
        _ => false,
    };
    match mode {
        ForkNetworkMode::Auto | ForkNetworkMode::Preserve => Ok(()),
        ForkNetworkMode::Isolated if isolated => Ok(()),
        ForkNetworkMode::Isolated => bail!(
            "network=isolated needs each child to own its network (a per-VM apple.usernet, or none); this guest is on \
             the Mac's NAT, so its children would share one MAC and address"
        ),
        ForkNetworkMode::None if matches!(src.request.network, NetworkSpec::None) => Ok(()),
        ForkNetworkMode::None => bail!(
            "network=none needs a guest created without a network card: a saved state cannot lose its NIC on restore"
        ),
    }
}

/// What a `vz` fork needs. Children resume the parent's saved device state, so they keep its MAC and address: each
/// must have a network of its own (none, or a per-VM userspace NAT), nothing may bind a fixed host port, and nothing
/// may be shared that two running VMs cannot both own.
pub(crate) fn check_vz_forkable(src: &VmRecord, count: u32) -> Result<()> {
    use fluxvm_core::model::AppleGuest;
    if count == 0 || count > MAX_FORK_COUNT {
        bail!("count must be between 1 and {MAX_FORK_COUNT}");
    }
    if src.status != VmStatus::Running && src.status != VmStatus::Paused {
        bail!(
            "fork needs a running or paused VM (status={:?})",
            src.status
        );
    }
    if src.request.storage != StorageBackend::Default {
        bail!("fork supports local file-backed disks only");
    }
    let apple = src.request.apple.clone().unwrap_or_default();
    if apple.guest_os == AppleGuest::Macos {
        bail!("fork of a macOS guest is not supported: its auxiliary storage is bound to one VM");
    }
    if !apple.extra_disks.is_empty() {
        bail!("fork of a vz guest with extra disks is not supported: a child would share them");
    }
    if apple.vmnet.is_some() || apple.bridge_interface.is_some() || !apple.networks.is_empty() {
        bail!(
            "fork needs a network of its own per child: bridged, vmnet and private-network guests would put two VMs \
             with one MAC on a shared segment"
        );
    }
    match &src.request.network {
        NetworkSpec::None => {}
        NetworkSpec::User { forwards } if forwards.is_empty() && apple.usernet.is_some() => {}
        NetworkSpec::User { .. } => {
            bail!(
                "fork needs network mode none, or mode user with apple.usernet and no forwards: on the Mac's NAT the \
                 children would share the parent's MAC and address, and a saved state cannot be restored onto a \
                 different network attachment (tried on hardware: the child runs but its NIC comes up disconnected). \
                 Create the guest with apple.usernet to make it forkable"
            );
        }
        _ => bail!("fork of a vz guest needs network mode none or user"),
    }
    if apple
        .usernet
        .as_ref()
        .is_some_and(|u| !u.port_forwards.is_empty())
    {
        bail!(
            "fork is refused while apple.usernet has port forwards: every child would bind the same host port"
        );
    }
    Ok(())
}

pub(crate) fn check_forkable(src: &VmRecord, count: u32) -> Result<()> {
    if count == 0 || count > MAX_FORK_COUNT {
        bail!("count must be between 1 and {MAX_FORK_COUNT}");
    }
    if src.backend != BackendKind::FluxVm {
        bail!(
            "fork supports the flux-vm backend only (backend={:?})",
            src.backend
        );
    }
    if src.status != VmStatus::Running && src.status != VmStatus::Paused {
        bail!(
            "fork needs a running or paused VM (status={:?})",
            src.status
        );
    }
    if src.request.storage != StorageBackend::Default {
        bail!("fork supports local file-backed disks only");
    }
    match &src.request.network {
        NetworkSpec::None | NetworkSpec::User { .. } => {}
        NetworkSpec::Tap {
            netns: true,
            direct: None,
            extra,
            ..
        } if extra.is_empty() => {}
        _ => bail!(
            "fork needs network mode none, user, or tap with netns: true and no extra NICs; \
             children keep the parent's MAC and IP, so they must not share an L2 segment"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(network: serde_json::Value, backend: &str, status: &str) -> VmRecord {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(),
            "name": "parent",
            "backend": backend,
            "status": status,
            "pid": null,
            "created_at": "2026-01-01T00:00:00Z",
            "expires_at": null,
            "workspace": "/tmp/w",
            "disk": "/tmp/w/root.raw",
            "seed_disk": null,
            "tap_name": null,
            "control_socket": null,
            "log_path": "/tmp/w/console.log",
            "error": null,
            "request": {
                "name": "parent",
                "image": "/img",
                "vcpus": 1,
                "memory_mib": 256,
                "backend": backend,
                "network": network
            }
        }))
        .unwrap()
    }

    #[test]
    fn rejects_shared_l2_and_bad_counts() {
        let netns = serde_json::json!({"mode": "tap", "netns": true});
        let bridged = serde_json::json!({"mode": "tap", "bridge": "br0"});
        let ok = record(netns.clone(), "flux-vm", "running");
        assert!(format!("{:#}", check_forkable(&ok, 0).unwrap_err()).contains("count"));
        assert!(
            format!("{:#}", check_forkable(&ok, MAX_FORK_COUNT + 1).unwrap_err()).contains("count")
        );
        let err = check_forkable(&record(bridged, "flux-vm", "running"), 2).unwrap_err();
        assert!(format!("{err:#}").contains("netns"));
        let err = check_forkable(&record(netns.clone(), "qemu", "running"), 2).unwrap_err();
        assert!(format!("{err:#}").contains("flux-vm"));
        let err = check_forkable(&record(netns, "flux-vm", "stopped"), 2).unwrap_err();
        assert!(format!("{err:#}").contains("running"));
    }

    fn vz_record(extra: serde_json::Value, status: &str) -> VmRecord {
        let mut r = serde_json::to_value(record(serde_json::json!({"mode": "user"}), "vz", status))
            .unwrap();
        let req = r["request"].as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            req.insert(k.clone(), v.clone());
        }
        serde_json::from_value(r).unwrap()
    }

    #[test]
    fn fork_spec_defaults_are_strict_and_old_bodies_still_parse() {
        let d: ForkSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(d.count, 1);
        assert_eq!(d.identity, ForkIdentityMode::Unique);
        assert_eq!(d.network, ForkNetworkMode::Auto);
        assert!(d.parallelism.is_none());
        // The pre-v2 REST body.
        let old: ForkSpec = serde_json::from_str(r#"{"count":3,"namePrefix":"w"}"#).unwrap();
        assert_eq!((old.count, old.name_prefix.as_deref()), (3, Some("w")));
        let new: ForkSpec = serde_json::from_str(
            r#"{"count":2,"identity":"best-effort","network":"isolated","parallelism":4}"#,
        )
        .unwrap();
        assert_eq!(new.identity, ForkIdentityMode::BestEffort);
        assert_eq!(new.network, ForkNetworkMode::Isolated);
        assert_eq!(new.parallelism, Some(4));
        // A typo is an error, not a silently ignored (and therefore unsafe) default.
        assert!(serde_json::from_str::<ForkSpec>(r#"{"identiy":"preserve"}"#).is_err());
        assert!(serde_json::from_str::<ForkSpec>(r#"{"identity":"maybe"}"#).is_err());
    }

    #[test]
    fn network_mode_is_checked_against_the_parent() {
        let un = vz_record(serde_json::json!({"apple": {"usernet": {}}}), "running");
        let nat = vz_record(serde_json::json!({}), "running");
        for mode in [ForkNetworkMode::Auto, ForkNetworkMode::Preserve] {
            assert!(check_fork_network_mode(&un, mode).is_ok());
            assert!(check_fork_network_mode(&nat, mode).is_ok());
        }
        assert!(check_fork_network_mode(&un, ForkNetworkMode::Isolated).is_ok());
        let err = check_fork_network_mode(&nat, ForkNetworkMode::Isolated).unwrap_err();
        assert!(format!("{err:#}").contains("NAT"));
        // `none` only for a guest that has no NIC to lose.
        assert!(check_fork_network_mode(&un, ForkNetworkMode::None).is_err());
    }

    #[test]
    fn the_ssh_reset_script_confirms_every_required_step() {
        let script = ssh_reset_script("app-fork-1", "QUJD");
        for step in REQUIRED_STEPS {
            assert!(
                script.contains(&format!("step {step}")),
                "script never confirms {step}"
            );
        }
        // Inputs are validated names and base64, so they may sit unquoted; make sure nothing else is spliced in.
        assert!(script.contains("hostname app-fork-1"));
        assert!(script.contains("printf %s 'QUJD'"));
        assert!(script.starts_with("set -u"));
    }

    #[test]
    fn vz_fork_admission_rules() {
        let un = serde_json::json!({"usernet": {}});
        let ok = vz_record(serde_json::json!({"apple": un}), "running");
        assert!(check_vz_forkable(&ok, 2).is_ok());
        assert!(format!("{:#}", check_vz_forkable(&ok, 0).unwrap_err()).contains("count"));

        // The Mac's NAT: children would share the parent's MAC and address.
        let nat = vz_record(serde_json::json!({}), "running");
        let err = check_vz_forkable(&nat, 2).unwrap_err();
        assert!(format!("{err:#}").contains("apple.usernet"), "{err:#}");

        // Host port forwards would be bound by every child.
        let fwd = vz_record(
            serde_json::json!({"apple": {"usernet": {"port_forwards": [
                {"protocol": "tcp", "host_port": 8080, "guest_port": 80}]}}}),
            "running",
        );
        assert!(format!("{:#}", check_vz_forkable(&fwd, 1).unwrap_err()).contains("port forwards"));

        let stopped = vz_record(serde_json::json!({"apple": un}), "stopped");
        assert!(format!("{:#}", check_vz_forkable(&stopped, 1).unwrap_err()).contains("running"));

        let mac = vz_record(
            serde_json::json!({"apple": {"guest_os": "macos", "usernet": {}}}),
            "running",
        );
        assert!(format!("{:#}", check_vz_forkable(&mac, 1).unwrap_err()).contains("macOS"));

        let disks = vz_record(
            serde_json::json!({"apple": {"usernet": {}, "extra_disks": [
                {"name": "d", "kind": "image", "path": "/tmp/d.raw"}]}}),
            "running",
        );
        assert!(format!("{:#}", check_vz_forkable(&disks, 1).unwrap_err()).contains("extra disks"));

        let none = {
            let mut r =
                serde_json::to_value(record(serde_json::json!({"mode": "none"}), "vz", "running"))
                    .unwrap();
            r["request"]["backend"] = "vz".into();
            serde_json::from_value::<VmRecord>(r).unwrap()
        };
        assert!(check_vz_forkable(&none, 3).is_ok());
    }
}
