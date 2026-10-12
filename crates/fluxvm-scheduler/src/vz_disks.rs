// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Extra disks of `vz` Linux guests (`apple.extra_disks`): image files, host block devices and NBD exports on virtio, NVMe or
//! USB controllers. Virtualization.framework fixes a VM's devices when it starts, so a change applies at the next start, except
//! a USB image disk, which is also hot-attached to a running guest (macOS 15) and hot-detached again.

use crate::VmManager;
use anyhow::{Context, Result, bail};
use fluxvm_core::model::{
    AppleDisk, AppleDiskController, AppleDiskKind, AppleGuest, BackendKind, StorageBackend,
    VmDiskInfo, VmRecord, VmStatus,
};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

/// Largest disk a vz create-time attach or a root grow may name.
const MAX_DISK_GIB: u64 = 16 * 1024;

/// Most extra disks per VM (virtio and NVMe each take a PCI slot).
const MAX_EXTRA_DISKS: usize = 16;
/// `{"<disk name>": {"uuid": "<runner usb id>", "pid": <runner pid>}}`: disks hot-attached to the running guest.
const HOTPLUG_FILE: &str = "usb-hotplug.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct Hotplug {
    uuid: String,
    /// The runner the disk was attached to; after a restart the id means nothing.
    pid: Option<u32>,
}

fn read_hotplug(workspace: &Path) -> std::collections::BTreeMap<String, Hotplug> {
    std::fs::read(workspace.join(HOTPLUG_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_hotplug(
    workspace: &Path,
    map: &std::collections::BTreeMap<String, Hotplug>,
) -> Result<()> {
    let path = workspace.join(HOTPLUG_FILE);
    if map.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    std::fs::write(&path, serde_json::to_vec(map)?)
        .with_context(|| format!("writing {}", path.display()))
}

fn controller_name(c: AppleDiskController) -> &'static str {
    match c {
        AppleDiskController::Virtio => "virtio",
        AppleDiskController::Nvme => "nvme",
        AppleDiskController::Usb => "usb",
    }
}

fn kind_name(k: AppleDiskKind) -> &'static str {
    match k {
        AppleDiskKind::Image => "raw",
        AppleDiskKind::Block => "block",
        AppleDiskKind::Nbd => "nbd",
        AppleDiskKind::Overlay => "overlay",
    }
}

fn sizes(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map_or((0, 0), |m| (m.len(), m.blocks() * 512))
}

/// `GET /v1/vms/{id}/disks` for a `vz` VM: `root`, then each extra disk.
pub(crate) fn list(vm: &VmRecord) -> Vec<VmDiskInfo> {
    let (size, allocated) = sizes(&vm.disk);
    let mut out = vec![VmDiskInfo {
        name: "root".into(),
        path: vm.disk.clone(),
        bus: "virtio".into(),
        format: "raw".into(),
        size_bytes: size,
        allocated_bytes: allocated,
    }];
    let extra = vm
        .request
        .apple
        .as_ref()
        .map(|a| a.extra_disks.as_slice())
        .unwrap_or_default();
    for (i, d) in extra.iter().enumerate() {
        let (size_bytes, allocated_bytes) = match d.kind {
            AppleDiskKind::Image => sizes(&d.path),
            _ => (0, 0),
        };
        out.push(VmDiskInfo {
            name: d.name_at(i),
            path: match d.kind {
                AppleDiskKind::Nbd => d.url.clone().unwrap_or_default().into(),
                _ => d.path.clone(),
            },
            bus: controller_name(d.controller).into(),
            format: kind_name(d.kind).into(),
            size_bytes,
            allocated_bytes,
        });
    }
    out
}

/// A comparable form of `p`: symlinks resolved when the file (or, for an overlay not yet created, its directory) exists,
/// otherwise the path with `.`/`..` folded away.
fn canon(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    if let (Some(dir), Some(file)) = (p.parent(), p.file_name())
        && let Ok(d) = std::fs::canonicalize(dir)
    {
        return d.join(file);
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Refuses a disk set whose `overlay` files are not exclusively this VM's. An overlay is written by whichever guest has
/// it attached, so it must not also be another VM record's overlay, base image, extra image or root disk (running or
/// not: a stopped record is a start away from corrupting it), nor the base of another overlay of the same VM. Equally,
/// an overlay's own base must not be a file another VM's overlay writes to (a snapshot chain parent in use).
///
/// `this` is the VM the disks belong to (skipped among `others`; `None` for a VM not yet recorded).
pub(crate) fn check_overlay_exclusive(
    disks: &[AppleDisk],
    this: Option<Uuid>,
    others: &[VmRecord],
) -> Result<()> {
    let mine: Vec<(String, PathBuf, PathBuf)> = disks
        .iter()
        .enumerate()
        .filter(|(_, d)| d.kind == AppleDiskKind::Overlay)
        .filter_map(|(i, d)| Some((d.name_at(i), canon(d.overlay.as_ref()?), canon(&d.path))))
        .collect();
    if mine.is_empty() {
        return Ok(());
    }
    for (i, (name, overlay, _)) in mine.iter().enumerate() {
        for (j, (other, other_overlay, base)) in mine.iter().enumerate() {
            if i < j && overlay == other_overlay {
                bail!(
                    "overlay file {} is used by both disk {name:?} and disk {other:?}",
                    overlay.display()
                );
            }
            if i != j && overlay == base {
                bail!(
                    "overlay file {} of disk {name:?} is the base image of disk {other:?}",
                    overlay.display()
                );
            }
        }
    }
    for vm in others.iter().filter(|v| Some(v.id) != this) {
        let who = || format!("VM {} ({}, {:?})", vm.name, vm.id, vm.status);
        for (name, overlay, base) in &mine {
            if canon(&vm.disk) == *overlay {
                bail!(
                    "overlay file {} of disk {name:?} is the root disk of {}",
                    overlay.display(),
                    who()
                );
            }
            let theirs = vm
                .request
                .apple
                .as_ref()
                .map(|a| a.extra_disks.as_slice())
                .unwrap_or_default();
            for (j, d) in theirs.iter().enumerate() {
                let dname = d.name_at(j);
                if d.kind == AppleDiskKind::Overlay
                    && let Some(o) = d.overlay.as_ref().map(|o| canon(o))
                {
                    if o == *overlay {
                        bail!(
                            "overlay file {} of disk {name:?} is already the overlay of disk {dname:?} of {}",
                            overlay.display(),
                            who()
                        );
                    }
                    if o == *base {
                        bail!(
                            "base image {} of disk {name:?} is the overlay of disk {dname:?} of {}, which may still write to it",
                            base.display(),
                            who()
                        );
                    }
                }
                if matches!(d.kind, AppleDiskKind::Image | AppleDiskKind::Overlay)
                    && !d.path.as_os_str().is_empty()
                    && canon(&d.path) == *overlay
                {
                    bail!(
                        "overlay file {} of disk {name:?} is the {} of disk {dname:?} of {}",
                        overlay.display(),
                        if d.kind == AppleDiskKind::Image {
                            "image"
                        } else {
                            "base image"
                        },
                        who()
                    );
                }
            }
        }
    }
    Ok(())
}

impl VmManager {
    /// [`check_overlay_exclusive`] against every VM record in the store.
    pub(crate) async fn ensure_vz_overlays_exclusive(
        &self,
        disks: &[AppleDisk],
        this: Option<Uuid>,
    ) -> Result<()> {
        if !disks.iter().any(|d| d.kind == AppleDiskKind::Overlay) {
            return Ok(());
        }
        check_overlay_exclusive(disks, this, &self.store.list().await)
    }

    async fn vz_linux_vm(&self, id: Uuid) -> Result<VmRecord> {
        let vm = self.get(id).await?;
        if vm.backend != BackendKind::Vz {
            bail!("not a vz VM");
        }
        if vm
            .request
            .apple
            .as_ref()
            .is_some_and(|a| a.guest_os == AppleGuest::Macos)
        {
            bail!("extra disks are a Linux-guest feature on vz");
        }
        Ok(vm)
    }

    pub async fn list_vz_disks(&self, id: Uuid) -> Result<Vec<VmDiskInfo>> {
        Ok(list(&self.vz_linux_vm(id).await?))
    }

    /// Adds extra disk `name` to a `vz` VM: `disk` as given, or with `size_gib` a new sparse image in the VM's workspace.
    pub async fn attach_vz_disk(
        &self,
        id: Uuid,
        name: &str,
        mut disk: AppleDisk,
        size_gib: Option<u64>,
    ) -> Result<VmDiskInfo> {
        let mut vm = self.vz_linux_vm(id).await?;
        disk.name = Some(name.to_owned());
        let apple = vm.request.apple.get_or_insert_with(Default::default);
        if apple.extra_disks.len() >= MAX_EXTRA_DISKS {
            bail!("at most {MAX_EXTRA_DISKS} extra disks");
        }
        if apple
            .extra_disks
            .iter()
            .enumerate()
            .any(|(i, d)| d.name_at(i) == name)
        {
            bail!("disk {name:?} already exists");
        }
        match (size_gib, disk.kind) {
            (Some(gib), AppleDiskKind::Image) if disk.path.as_os_str().is_empty() => {
                if gib == 0 || gib > MAX_DISK_GIB {
                    bail!("size_gib must be 1-{MAX_DISK_GIB}");
                }
                fluxvm_apple::validate_disk(&AppleDisk {
                    path: "/placeholder".into(),
                    ..disk.clone()
                })?;
                let dir = vm.workspace.join("disks");
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{name}.raw"));
                let f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .with_context(|| format!("creating {}", path.display()))?;
                f.set_len(gib << 30)?;
                disk.path = path;
            }
            (Some(_), _) => bail!(
                "size_gib creates a new image disk; it cannot be combined with path, url or kind"
            ),
            (None, AppleDiskKind::Image | AppleDiskKind::Block) => {
                disk.path = self.check_disk_source(&disk.path)?;
            }
            (None, AppleDiskKind::Overlay) => {
                disk.path = self.check_disk_source(&disk.path)?;
                // The overlay file may not exist yet; its directory is what has to be allowed.
                let ov = disk.overlay.clone().unwrap_or_default();
                if let (Some(dir), Some(file)) = (ov.parent(), ov.file_name()) {
                    disk.overlay = Some(self.check_disk_source(dir)?.join(file));
                }
            }
            (None, AppleDiskKind::Nbd) => {}
        }
        fluxvm_apple::validate_disk(&disk)?;
        apple.extra_disks.push(disk.clone());
        fluxvm_apple::validate_request(&vm.request)?;
        // Serialised with other creates/attaches so two VMs cannot both claim one overlay file.
        let _overlay_guard = self.vz_overlay_lock.lock().await;
        if disk.kind == AppleDiskKind::Overlay {
            let all = vm
                .request
                .apple
                .as_ref()
                .map(|a| a.extra_disks.clone())
                .unwrap_or_default();
            self.ensure_vz_overlays_exclusive(&all, Some(id)).await?;
        }
        let live = matches!(vm.status, VmStatus::Running | VmStatus::Paused)
            && disk.kind == AppleDiskKind::Image
            && disk.controller == AppleDiskController::Usb;
        if live {
            let uuid = fluxvm_apple::usb_attach(&vm, &disk.path, disk.read_only, disk.usb_bus)
                .await
                .context("hot-attaching the USB disk")?;
            let mut map = read_hotplug(&vm.workspace);
            map.insert(name.to_owned(), Hotplug { uuid, pid: vm.pid });
            write_hotplug(&vm.workspace, &map)?;
        }
        if !live && matches!(vm.status, VmStatus::Running | VmStatus::Paused) {
            tracing::warn!(
                vm = %id,
                disk = name,
                "vz can only hot-attach USB image disks; this disk applies at the next boot"
            );
        }
        self.store.update(vm.clone()).await?;
        crate::audit_event(
            "vm.disk.attach",
            &[
                ("vm_id", &id.to_string()),
                ("disk", name),
                ("kind", kind_name(disk.kind)),
                ("live", if live { "true" } else { "false" }),
            ],
        );
        let idx = vm.request.apple.as_ref().map_or(0, |a| a.extra_disks.len());
        list(&vm)
            .into_iter()
            .nth(idx)
            .context("the new disk is missing from the list")
    }

    /// Removes extra disk `name`: at once when it was hot-attached to the running guest, otherwise at the next start. A new
    /// image made by `attach_vz_disk` is deleted with it once nothing uses it.
    /// Grows a stopped `vz` VM's root disk (`PATCH /v1/vms/{id}/disks/root`) to `size_gib`, macOS or Linux guest.
    /// Grow-only. This extends the file only: the guest still has to grow its own partition or APFS container
    /// (`diskutil apfs resizeContainer` for macOS), which is not automated.
    pub async fn resize_vz_disk(&self, id: Uuid, name: &str, size_gib: u64) -> Result<VmDiskInfo> {
        let vm = self.get(id).await?;
        let info = grow_root_disk(&vm, name, size_gib)?;
        crate::audit_event(
            "vm.disk.resize",
            &[
                ("vm_id", &id.to_string()),
                ("disk", "root"),
                ("size_gib", &size_gib.to_string()),
            ],
        );
        Ok(info)
    }

    pub async fn detach_vz_disk(&self, id: Uuid, name: &str) -> Result<()> {
        let mut vm = self.vz_linux_vm(id).await?;
        let mut hotplug = read_hotplug(&vm.workspace);
        let mut live = false;
        if let Some(h) = hotplug.remove(name) {
            if matches!(vm.status, VmStatus::Running | VmStatus::Paused) && h.pid == vm.pid {
                fluxvm_apple::usb_detach(&vm, &h.uuid)
                    .await
                    .context("hot-detaching the USB disk")?;
                live = true;
            }
            write_hotplug(&vm.workspace, &hotplug)?;
        }
        let workspace_disks = vm.workspace.join("disks");
        let apple = vm.request.apple.get_or_insert_with(Default::default);
        let Some(i) = apple
            .extra_disks
            .iter()
            .enumerate()
            .position(|(i, d)| d.name_at(i) == name)
        else {
            bail!("disk {name:?} not found");
        };
        let removed = apple.extra_disks.remove(i);
        // Unnamed disks are numbered by position; keep the others' names stable.
        for (j, d) in apple.extra_disks.iter_mut().enumerate() {
            if d.name.is_none() {
                d.name = Some(format!("disk{}", if j >= i { j + 1 } else { j }));
            }
        }
        let unused = live || matches!(vm.status, VmStatus::Stopped | VmStatus::Failed);
        self.store.update(vm).await?;
        if unused && removed.path.starts_with(&workspace_disks) {
            let _ = std::fs::remove_file(&removed.path);
        }
        crate::audit_event(
            "vm.disk.detach",
            &[
                ("vm_id", &id.to_string()),
                ("disk", name),
                ("live", if live { "true" } else { "false" }),
            ],
        );
        Ok(())
    }
}

/// The checks and the `set_len` behind [`VmManager::resize_vz_disk`].
fn grow_root_disk(vm: &VmRecord, name: &str, size_gib: u64) -> Result<VmDiskInfo> {
    if vm.backend != BackendKind::Vz {
        bail!("not a vz VM");
    }
    if name != "root" {
        bail!("only the root disk of a vz VM can be grown (disk {name:?})");
    }
    if !matches!(vm.status, VmStatus::Stopped | VmStatus::Failed) {
        bail!(
            "stop the VM first: a vz root disk is grown offline (status={:?})",
            vm.status
        );
    }
    if vm.request.apple.as_ref().is_some_and(|a| a.asif_overlay) {
        bail!(
            "root disk resize is not supported with apple.asif_overlay: the overlay layer is sized separately"
        );
    }
    if vm.request.storage != StorageBackend::Default || !vm.disk.is_file() {
        bail!("root disk resize needs a local file-backed disk");
    }
    if !(1..=MAX_DISK_GIB).contains(&size_gib) {
        bail!("size_gib must be 1-{MAX_DISK_GIB}");
    }
    let new_bytes = size_gib << 30;
    let current = std::fs::metadata(&vm.disk)
        .with_context(|| format!("reading {}", vm.disk.display()))?
        .len();
    if new_bytes <= current {
        bail!(
            "resize only grows: the disk is already {} bytes, asked for {new_bytes}",
            current
        );
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(&vm.disk)
        .and_then(|f| f.set_len(new_bytes))
        .with_context(|| format!("growing {}", vm.disk.display()))?;
    Ok(list(vm).remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::model::VmStatus;

    fn overlay(base: &str, ovl: &str) -> AppleDisk {
        serde_json::from_value(serde_json::json!({"kind": "overlay", "path": base, "overlay": ovl}))
            .unwrap()
    }

    fn vm(name: &str, status: VmStatus, root: &str, disks: Vec<AppleDisk>) -> VmRecord {
        let mut v: VmRecord = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "name": name, "backend": "vz", "status": "stopped", "pid": null,
            "created_at": "2026-01-01T00:00:00Z", "expires_at": null, "workspace": "/nonexistent/w",
            "disk": root, "seed_disk": null, "tap_name": null, "control_socket": null,
            "log_path": "/nonexistent/w/console.log", "error": null,
            "request": {"name": name, "image": "/img", "vcpus": 1, "memory_mib": 512, "backend": "vz"},
        }))
        .unwrap();
        v.status = status;
        let mut a: fluxvm_core::model::AppleSpec = Default::default();
        a.extra_disks = disks;
        v.request.apple = Some(a);
        v
    }

    #[test]
    fn overlay_may_not_be_shared_with_another_vm_running_or_not() {
        let a = vm(
            "a",
            VmStatus::Stopped,
            "/nonexistent/a.raw",
            vec![overlay("/b/base.raw", "/o/data.ovl")],
        );
        // Another VM, same base, its own overlay: fine.
        assert!(
            check_overlay_exclusive(
                &[overlay("/b/base.raw", "/o/other.ovl")],
                None,
                std::slice::from_ref(&a)
            )
            .is_ok()
        );
        // Same overlay file as a stopped VM: refused, naming it.
        let err = check_overlay_exclusive(
            &[overlay("/b/base.raw", "/o/data.ovl")],
            None,
            std::slice::from_ref(&a),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("/o/data.ovl") && err.contains("VM a"), "{err}");
        // A running one too, and `./`/`..` spellings do not hide it.
        let mut r = a.clone();
        r.status = VmStatus::Running;
        assert!(
            check_overlay_exclusive(&[overlay("/b/base.raw", "/o/x/../data.ovl")], None, &[r])
                .is_err()
        );
        // The VM's own record is not a conflict.
        assert!(
            check_overlay_exclusive(&[overlay("/b/base.raw", "/o/data.ovl")], Some(a.id), &[a])
                .is_ok()
        );
    }

    #[test]
    fn chain_parents_and_other_disks_are_protected() {
        let a = vm(
            "a",
            VmStatus::Running,
            "/nonexistent/a.raw",
            vec![overlay("/b/base.raw", "/o/a.ovl")],
        );
        // Layering on top of an overlay another VM is still writing.
        let err = check_overlay_exclusive(
            &[overlay("/o/a.ovl", "/o/child.ovl")],
            None,
            std::slice::from_ref(&a),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("still write"), "{err}");
        // Writing into another VM's base image.
        assert!(
            check_overlay_exclusive(
                &[overlay("/b/x.raw", "/b/base.raw")],
                None,
                std::slice::from_ref(&a)
            )
            .is_err()
        );
        // Another VM's root disk.
        assert!(
            check_overlay_exclusive(&[overlay("/b/x.raw", "/nonexistent/a.raw")], None, &[a])
                .is_err()
        );
    }

    #[test]
    fn a_vm_cannot_reuse_one_overlay_twice() {
        let two = [
            overlay("/b/base.raw", "/o/a.ovl"),
            overlay("/b/base2.raw", "/o/a.ovl"),
        ];
        assert!(check_overlay_exclusive(&two, None, &[]).is_err());
        let chain = [
            overlay("/b/base.raw", "/o/a.ovl"),
            overlay("/o/a.ovl", "/o/b.ovl"),
        ];
        assert!(check_overlay_exclusive(&chain, None, &[]).is_err());
        // Non-overlay disks are none of this check's business.
        let image: AppleDisk =
            serde_json::from_value(serde_json::json!({"path": "/o/a.ovl"})).unwrap();
        assert!(check_overlay_exclusive(&[image], None, &[]).is_ok());
    }

    #[test]
    fn listing_names_and_describes_each_disk() {
        let mut vm: VmRecord = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "name": "x", "backend": "vz", "status": "stopped", "pid": null,
            "created_at": "2026-01-01T00:00:00Z", "expires_at": null, "workspace": "/tmp/w",
            "disk": "/nonexistent/root.raw", "seed_disk": null, "tap_name": null, "control_socket": null,
            "log_path": "/tmp/w/console.log", "error": null,
            "request": {"name": "x", "image": "/img", "vcpus": 1, "memory_mib": 512, "backend": "vz"},
        }))
        .unwrap();
        vm.request.apple = Some(
            serde_json::from_value(serde_json::json!({
                "extra_disks": [
                    {"path": "/nonexistent/a.raw", "controller": "nvme"},
                    {"name": "net", "kind": "nbd", "url": "nbd://h:10809/v"},
                ]
            }))
            .unwrap(),
        );
        let l = list(&vm);
        let rows: Vec<_> = l
            .iter()
            .map(|d| (d.name.as_str(), d.bus.as_str(), d.format.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("root", "virtio", "raw"),
                ("disk0", "nvme", "raw"),
                ("net", "virtio", "nbd")
            ]
        );
        assert_eq!(l[2].path, Path::new("nbd://h:10809/v"));
    }

    fn grow_fixture(status: VmStatus, gib: u64) -> (tempfile::TempDir, VmRecord) {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk.raw");
        std::fs::File::create(&disk)
            .unwrap()
            .set_len(gib << 30)
            .unwrap();
        (dir, vm("g", status, disk.to_str().unwrap(), vec![]))
    }

    #[test]
    fn a_stopped_root_disk_grows_and_never_shrinks() {
        let (_d, v) = grow_fixture(VmStatus::Stopped, 1);
        let info = grow_root_disk(&v, "root", 2).unwrap();
        assert_eq!(info.size_bytes, 2 << 30);
        assert_eq!(std::fs::metadata(&v.disk).unwrap().len(), 2 << 30);
        for same_or_less in [2, 1] {
            let err = grow_root_disk(&v, "root", same_or_less).unwrap_err();
            assert!(err.to_string().contains("only grows"), "{err}");
        }
    }

    #[test]
    fn grow_is_refused_while_running_for_data_disks_and_with_an_asif_overlay() {
        let (_d, mut v) = grow_fixture(VmStatus::Running, 1);
        let err = grow_root_disk(&v, "root", 2).unwrap_err().to_string();
        assert!(err.contains("stop the VM first"), "{err}");
        v.status = VmStatus::Stopped;
        let err = grow_root_disk(&v, "data", 2).unwrap_err().to_string();
        assert!(err.contains("only the root disk"), "{err}");
        v.request.apple.as_mut().unwrap().asif_overlay = true;
        let err = grow_root_disk(&v, "root", 2).unwrap_err().to_string();
        assert!(err.contains("asif_overlay"), "{err}");
        assert_eq!(std::fs::metadata(&v.disk).unwrap().len(), 1 << 30);
    }
}
