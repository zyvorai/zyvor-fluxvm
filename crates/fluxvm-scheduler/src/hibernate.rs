//! Hibernate a QEMU VM to a state directory, resume it on the next `start`,
//! and import a hibernated VM from shared storage on another node.
//! See docs/hibernation.md.

use crate::live_migration::{HOTPLUGGED_LABEL, clear_hotplug_labels};
use crate::{FIRST_GUEST_CID, VmManager, audit_event, backend};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use fluxvm_core::model::{
    BackendKind, HIBERNATION_FORMAT, HibernationCheck, HibernationInfo, HibernationManifest,
    HostHibernationFacts, NetworkSpec, StorageBackend, VmRecord, VmStatus,
    hibernation_incompatibilities,
};
use fluxvm_core::process;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

pub const STATE_FILE: &str = "state.bin";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const RECORD_FILE: &str = "record.json";
const VARS_FILE: &str = "ovmf_vars.fd";
const SEED_FILE: &str = "seed.img";
/// Names the host that imported the state, so the original node's later
/// delete leaves it for the importer.
const IMPORTED_FILE: &str = "imported-by";

const SAVE_DEADLINE: Duration = Duration::from_secs(30 * 60);
const LOAD_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Why `record` cannot hibernate; `None` when it can.
pub fn hibernate_unsupported(record: &VmRecord) -> Option<String> {
    let req = &record.request;
    if record.backend != BackendKind::Qemu {
        return Some(format!(
            "hibernation supports the qemu backend only (backend={:?})",
            record.backend
        ));
    }
    if record.labels.contains_key(HOTPLUGGED_LABEL) {
        return Some(
            "VM has hot-plugged CPUs, memory or NICs since it started; restart it before hibernating".into(),
        );
    }
    if req.tpm == Some(true) {
        return Some("VMs with a vTPM cannot hibernate yet".into());
    }
    if !req.vfio_devices.is_empty() {
        return Some("VMs with VFIO passthrough devices cannot hibernate".into());
    }
    if !req.shared_folders.is_empty() {
        return Some("VMs with virtiofs shares cannot hibernate".into());
    }
    if req.security_profile.is_confidential() {
        return Some("confidential VMs cannot hibernate".into());
    }
    if fluxvm_qemu::disks::has_vhost_user(&record.workspace) {
        return Some(
            "VMs with vhost-user-blk disks cannot hibernate (the disk lives in qemu-storage-daemon)".into(),
        );
    }
    None
}

/// Why a hibernated `record` cannot be resumed on a different node.
pub fn import_unsupported(record: &VmRecord) -> Option<String> {
    if let Some(why) = hibernate_unsupported(record) {
        return Some(why);
    }
    let req = &record.request;
    if !matches!(
        req.storage,
        StorageBackend::Shared | StorageBackend::CephRbdInPlace
    ) {
        return Some(format!(
            "resuming on another node needs the disk on shared storage (storage={:?})",
            req.storage
        ));
    }
    if !req.data_disks.is_empty() {
        return Some("VMs with data disks resume only on the node that hibernated them".into());
    }
    if req.cdroms.iter().any(|c| !c.is_ejected()) {
        return Some("VMs with install media attached resume only on their own node".into());
    }
    if req.network.is_direct() || matches!(req.network, NetworkSpec::Macvtap { .. }) {
        return Some("direct-datapath and macvtap VMs resume only on their own node".into());
    }
    None
}

fn shared_storage(record: &VmRecord) -> bool {
    matches!(
        record.request.storage,
        StorageBackend::Shared | StorageBackend::CephRbdInPlace
    )
}

fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// Removes a hibernation directory unless another host imported it.
pub(crate) fn remove_state_dir(dir: &Path) {
    if let Ok(by) = fs::read_to_string(dir.join(IMPORTED_FILE))
        && by.trim() != hostname()
    {
        return;
    }
    if let Err(e) = fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(dir = %dir.display(), error = %e, "removing hibernation state");
    }
}

impl VmManager {
    fn hibernation_path(&self, id: Uuid) -> PathBuf {
        self.cfg.hibernation_dir().join(id.to_string())
    }

    pub async fn host_hibernation_facts(&self) -> Result<HostHibernationFacts> {
        fluxvm_qemu::hibernate::host_facts(&self.cfg.qemu_binary, &self.cfg.hibernation_dir()).await
    }

    pub async fn check_hibernation(
        &self,
        manifest: &HibernationManifest,
    ) -> Result<HibernationCheck> {
        let host = self.host_hibernation_facts().await?;
        let reasons = hibernation_incompatibilities(manifest, &host);
        Ok(HibernationCheck {
            compatible: reasons.is_empty(),
            reasons,
        })
    }

    async fn require_compatible(&self, manifest: &HibernationManifest) -> Result<()> {
        let check = self.check_hibernation(manifest).await?;
        if !check.compatible {
            bail!(
                "hibernated state is not compatible with this host: {}",
                check.reasons.join("; ")
            );
        }
        Ok(())
    }

    /// Saves the VM's RAM, vCPU and device state to
    /// `<hibernation_dir>/<id>/`, then stops QEMU and frees its resources.
    /// The next [`Self::start`] restores the state.
    pub async fn hibernate(self: &Arc<Self>, id: Uuid) -> Result<VmRecord> {
        let vm = self.get(id).await?;
        if vm.hibernation.is_some() && vm.status == VmStatus::Stopped {
            return Ok(vm);
        }
        if !matches!(vm.status, VmStatus::Running | VmStatus::Paused) || vm.pid.is_none() {
            bail!(
                "VM {id} is {:?}; only a running or paused VM can hibernate",
                vm.status
            );
        }
        if let Some(why) = hibernate_unsupported(&vm) {
            bail!(why);
        }
        let facts = self
            .host_hibernation_facts()
            .await
            .context("collecting host facts for the hibernation manifest")?;
        let dir = self.hibernation_path(id);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("clearing stale {}", dir.display()))?;
        }
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let qmp = vm.workspace.join("qmp.sock");
        let state_bytes =
            match fluxvm_qemu::hibernate::save_state(&qmp, &dir.join(STATE_FILE), SAVE_DEADLINE)
                .await
            {
                Ok(n) => n,
                Err(e) => {
                    let _ = fs::remove_dir_all(&dir);
                    return Err(e.context(format!("hibernating {id}")));
                }
            };
        let manifest = HibernationManifest {
            format: HIBERNATION_FORMAT,
            vm_id: id,
            hibernated_at: Utc::now(),
            node: hostname(),
            fluxvm_version: env!("CARGO_PKG_VERSION").to_string(),
            host: facts,
            vcpus: vm.request.vcpus,
            memory_mib: vm.request.memory_mib,
            state_bytes,
            shared_storage: shared_storage(&vm),
        };
        let info = HibernationInfo {
            dir: dir.clone(),
            manifest: manifest.clone(),
        };
        let mut saved = vm.clone();
        saved.status = VmStatus::Stopped;
        saved.pid = None;
        saved.hibernation = Some(info.clone());
        let written: Result<()> = (|| {
            write_json(&dir.join(MANIFEST_FILE), &manifest)?;
            let vars = vm.workspace.join(VARS_FILE);
            if vars.is_file() {
                fs::copy(&vars, dir.join(VARS_FILE)).context("copying OVMF vars")?;
            }
            if let Some(seed) = vm.seed_disk.as_ref().filter(|s| s.is_file()) {
                fs::copy(seed, dir.join(SEED_FILE)).context("copying the cloud-init seed")?;
            }
            write_json(&dir.join(RECORD_FILE), &saved)
        })();
        if let Err(e) = written {
            let _ = fs::remove_dir_all(&dir);
            if vm.status == VmStatus::Running {
                let _ = backend(vm.backend)?.resume(&self.cfg, &vm).await;
            }
            return Err(e.context(format!("hibernating {id}")));
        }

        if let Err(e) = fluxvm_qemu::hibernate::quit(&qmp).await {
            tracing::warn!(vm = %id, error = %e, "QMP quit after hibernate; terminating");
        }
        if let Some(pid) = vm.pid {
            process::wait_for_exit(pid, 100).await;
        }
        let mut vm = self.stop_with(id, false).await?;
        vm.hibernation = Some(info);
        vm.error = None;
        self.store.update(vm.clone()).await?;
        audit_event(
            "vm.hibernate",
            &[
                ("vm_id", &id.to_string()),
                ("state_bytes", &state_bytes.to_string()),
                ("dir", &dir.display().to_string()),
            ],
        );
        Ok(vm)
    }

    /// Checks a hibernated VM can resume here; `start_impl` calls this
    /// before launching.
    pub(crate) async fn prepare_resume(&self, vm: &VmRecord) -> Result<PathBuf> {
        let Some(h) = &vm.hibernation else {
            bail!("VM {} is not hibernated", vm.id);
        };
        let state = h.dir.join(STATE_FILE);
        if !state.is_file() {
            bail!(
                "hibernated state {} is gone (resumed elsewhere or discarded); discard the hibernation to boot fresh",
                state.display()
            );
        }
        self.require_compatible(&h.manifest).await?;
        Ok(state)
    }

    /// Loads the saved state into a QEMU just launched with `-incoming defer`.
    pub(crate) async fn finish_resume(&self, vm: &VmRecord, pid: u32, state: &Path) -> Result<()> {
        let qmp = vm.workspace.join("qmp.sock");
        process::wait_for_socket_ready(
            pid,
            &qmp,
            Duration::from_secs(15),
            "QEMU resuming from hibernation",
            &vm.log_path,
        )
        .await?;
        fluxvm_qemu::hibernate::load_state(&qmp, state, LOAD_DEADLINE).await
    }

    /// Drops a hibernated VM's saved state; the next `start` boots from disk.
    pub async fn discard_hibernation(&self, id: Uuid) -> Result<VmRecord> {
        let mut vm = self.get(id).await?;
        let Some(h) = vm.hibernation.take() else {
            return Ok(vm);
        };
        remove_state_dir(&h.dir);
        self.store.update(vm.clone()).await?;
        audit_event("vm.hibernation.discard", &[("vm_id", &id.to_string())]);
        Ok(vm)
    }

    /// Registers a VM hibernated on another node from the shared
    /// `<hibernation_dir>/<id>/`, as a stopped hibernated VM with the same id.
    /// `start` then resumes it.
    pub async fn import_hibernated(self: &Arc<Self>, id: Uuid) -> Result<VmRecord> {
        if self.get(id).await.is_ok() {
            bail!("VM {id} already exists on this host");
        }
        let dir = self.hibernation_path(id);
        let source: VmRecord = read_json(&dir.join(RECORD_FILE))?;
        let manifest: HibernationManifest = read_json(&dir.join(MANIFEST_FILE))?;
        if source.id != id || manifest.vm_id != id {
            bail!("{} describes a different VM", dir.display());
        }
        if !dir.join(STATE_FILE).is_file() {
            bail!("{} has no saved state", dir.display());
        }
        if let Some(why) = import_unsupported(&source) {
            bail!(why);
        }
        self.require_compatible(&manifest).await?;
        if source.request.storage == StorageBackend::Shared {
            fluxvm_image::storage::validate_shared_disk(&source.disk)?;
            if !source.disk.exists() {
                bail!("disk {} is not visible on this host", source.disk.display());
            }
        }
        if let Some(cid) = source.guest_cid
            && self
                .store
                .list()
                .await
                .iter()
                .any(|v| v.guest_cid == Some(cid))
        {
            bail!("vsock CID {cid} saved in the guest state is in use on this host");
        }
        let ledger = self.store.quota_ledger().await?;
        fluxvm_core::policy::enforce_host_totals(
            source.request.vcpus,
            source.request.memory_mib,
            &self.cfg.policy,
            &fluxvm_core::policy::ledger_for_host_admission(&ledger),
        )?;

        let workspace = self.cfg.state_dir.join("instances").join(id.to_string());
        fs::create_dir_all(&workspace)?;
        let copied: Result<Option<PathBuf>> = (|| {
            if dir.join(VARS_FILE).is_file() {
                fs::copy(dir.join(VARS_FILE), workspace.join(VARS_FILE))
                    .context("copying OVMF vars")?;
            }
            match &source.seed_disk {
                Some(src) => {
                    let name = src.file_name().context("seed disk has no file name")?;
                    let dst = workspace.join(name);
                    fs::copy(dir.join(SEED_FILE), &dst).context("copying the cloud-init seed")?;
                    Ok(Some(dst))
                }
                None => Ok(None),
            }
        })();
        let seed_disk = match copied {
            Ok(s) => s,
            Err(e) => {
                let _ = fs::remove_dir_all(&workspace);
                return Err(e);
            }
        };
        let mut labels = source.labels.clone();
        clear_hotplug_labels(&mut labels);
        let record = VmRecord {
            status: VmStatus::Stopped,
            pid: None,
            log_path: workspace.join("console.log"),
            workspace,
            seed_disk,
            tap_name: None,
            control_socket: None,
            error: None,
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
            labels,
            hibernation: Some(HibernationInfo {
                dir: dir.clone(),
                manifest,
            }),
            ..source
        };
        let record = self
            .store
            .insert_with_cid(record, false, FIRST_GUEST_CID)
            .await?;
        if let Err(e) = fs::write(dir.join(IMPORTED_FILE), hostname()) {
            tracing::warn!(vm = %id, error = %e, "recording hibernation import");
        }
        audit_event(
            "vm.hibernation.import",
            &[
                ("vm_id", &id.to_string()),
                (
                    "from",
                    &record
                        .hibernation
                        .as_ref()
                        .map(|h| h.manifest.node.clone())
                        .unwrap_or_default(),
                ),
            ],
        );
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::model::CreateVmRequest;

    fn record(storage: StorageBackend) -> VmRecord {
        let req: CreateVmRequest = serde_json::from_value(serde_json::json!({
            "name": "h",
            "backend": "qemu",
            "image": "/srv/shared/h.raw",
            "vcpus": 1,
            "memory_mib": 512,
            "storage": storage,
        }))
        .unwrap();
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "name": "h",
            "backend": "qemu",
            "status": "running",
            "created_at": Utc::now(),
            "workspace": "/tmp/ws",
            "disk": "/srv/shared/h.raw",
            "log_path": "/tmp/ws/console.log",
            "request": req,
        }))
        .unwrap()
    }

    #[test]
    fn plain_qemu_vm_can_hibernate_but_not_move() {
        let r = record(StorageBackend::Default);
        assert_eq!(hibernate_unsupported(&r), None);
        assert!(import_unsupported(&r).unwrap().contains("shared storage"));
    }

    #[test]
    fn shared_storage_vm_can_move() {
        let r = record(StorageBackend::Shared);
        assert_eq!(import_unsupported(&r), None);
    }

    #[test]
    fn unsupported_devices_are_refused() {
        let mut tpm = record(StorageBackend::Shared);
        tpm.request.tpm = Some(true);
        assert!(hibernate_unsupported(&tpm).unwrap().contains("vTPM"));
        let mut fc = record(StorageBackend::Shared);
        fc.backend = BackendKind::Firecracker;
        assert!(hibernate_unsupported(&fc).unwrap().contains("qemu"));
        let mut hot = record(StorageBackend::Shared);
        hot.labels.insert(HOTPLUGGED_LABEL.into(), "true".into());
        assert!(hibernate_unsupported(&hot).unwrap().contains("restart"));
    }

    #[test]
    fn foreign_import_marker_keeps_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("h");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(IMPORTED_FILE),
            "some-other-host-that-is-not-this-one",
        )
        .unwrap();
        remove_state_dir(&dir);
        assert!(dir.exists());
        fs::write(dir.join(IMPORTED_FILE), hostname()).unwrap();
        remove_state_dir(&dir);
        assert!(!dir.exists());
    }
}
