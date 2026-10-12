// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Sentinel Set 7S: per-VM device-cgroup + outbound-IP hardening for the
//! QEMU/VMM process itself, layered onto the same `fluxvm.slice/{id}.scope`
//! cgroup `fluxvm-cgroup::CgroupManager::create_and_migrate` already creates
//! for cpu/memory/pids/io control. See `bpf/fluxvm_qemu_device.bpf.c` and
//! `bpf/fluxvm_qemu_egress.bpf.c` for what each program actually enforces
//! and why a general host-process LSM was deliberately not built instead.

use anyhow::{Context, Result, bail};
use fluxvm_core::config::DataplaneConfig;
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};
use uuid::Uuid;

use crate::ebpf::{bpftool_map_update, require_bpftool, run, vm_pin_dir};

const BPF_DEVCG_DEV_BLOCK: u32 = 1;
const BPF_DEVCG_DEV_CHAR: u32 = 2;
const BPF_DEVCG_ACC_MKNOD: u32 = 1;
const BPF_DEVCG_ACC_READ: u32 = 2;
const BPF_DEVCG_ACC_WRITE: u32 = 4;
const BPF_DEVCG_ACC_ALL: u32 = BPF_DEVCG_ACC_MKNOD | BPF_DEVCG_ACC_READ | BPF_DEVCG_ACC_WRITE;
/// Must match `FLUXVM_QEMU_MAX_DEVICES` in `bpf/fluxvm_qemu_device.bpf.c`.
const MAX_DEVICES: usize = 8;

/// One allowlist entry: a device number, its type and the access allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DevRule {
    major: u32,
    minor: u32,
    dev_type: u32,
    access: u32,
}

impl DevRule {
    fn char((major, minor): (u32, u32)) -> Self {
        Self {
            major,
            minor,
            dev_type: BPF_DEVCG_DEV_CHAR,
            access: BPF_DEVCG_ACC_ALL,
        }
    }

    fn block((major, minor): (u32, u32)) -> Self {
        Self {
            major,
            minor,
            dev_type: BPF_DEVCG_DEV_BLOCK,
            access: BPF_DEVCG_ACC_READ | BPF_DEVCG_ACC_WRITE,
        }
    }
}

fn qemu_pin_dir(root: &Path, id: Uuid) -> PathBuf {
    vm_pin_dir(root, id).join("qemu")
}

/// glibc `<sys/sysmacros.h>` major()/minor() encoding — not exposed by the
/// `libc` crate (they're header macros in glibc, not real ABI symbols), so
/// reimplemented directly against `st_rdev`'s documented bit layout.
fn major_minor(rdev: u64) -> (u32, u32) {
    let major = (((rdev >> 8) & 0xfff) as u32) | (((rdev >> 32) & 0xffff_f000) as u32);
    let minor = ((rdev & 0xff) as u32) | (((rdev >> 12) & 0xffff_ff00) as u32);
    (major, minor)
}

fn resolve_device(path: &str) -> Result<Option<(u32, u32)>> {
    match fs::metadata(path) {
        Ok(meta) => Ok(Some(major_minor(meta.rdev()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("statting {path}")),
    }
}

/// Resolves `/dev/vfio/<group>` for a PCI BDF address (`0000:01:00.0`-style,
/// as used in `CreateVmRequest::vfio_devices`/`-device vfio-pci,host=...`)
/// via its `iommu_group` sysfs symlink. Returns `None` rather than erroring
/// when the device or its group can't be resolved: a broken passthrough
/// device should fail loudly at the actual `-device vfio-pci` QEMU argument
/// (`crates/fluxvm-qemu`), not silently as a missing device-cgroup allowlist
/// entry that a caller might not even notice.
fn resolve_vfio_group_device(pci_bdf: &str) -> Option<String> {
    let link = format!("/sys/bus/pci/devices/{pci_bdf}/iommu_group");
    let target = fs::read_link(link).ok()?;
    let group = target.file_name()?.to_str()?;
    Some(format!("/dev/vfio/{group}"))
}

/// The block device `path` resolves to, or `None` for a regular file (or a
/// path that no longer exists).
fn resolve_block_device(path: &Path) -> Option<(u32, u32)> {
    let meta = fs::metadata(path).ok()?;
    meta.file_type()
        .is_block_device()
        .then(|| major_minor(meta.rdev()))
}

fn check_device_count(rules: &[DevRule]) -> Result<()> {
    if rules.len() > MAX_DEVICES {
        bail!(
            "VM requests {} device-cgroup allowlist entries, more than the eBPF map supports ({MAX_DEVICES})",
            rules.len()
        );
    }
    Ok(())
}

/// The allowlist for a VM: the fixed character devices, its VFIO groups and
/// the host block devices its disks are (a root disk or data disk on
/// `/dev/...`, directly or through a symlink).
fn collect_device_rules(vfio_devices: &[String], disks: &[PathBuf]) -> Result<Vec<DevRule>> {
    let mut rules = Vec::new();
    for path in ["/dev/kvm", "/dev/vhost-vsock", "/dev/net/tun"] {
        match resolve_device(path)? {
            Some(rule) => rules.push(DevRule::char(rule)),
            None => tracing::warn!(
                path,
                "Set 7S device-cgroup allowlist target does not exist on this host"
            ),
        }
    }
    for bdf in vfio_devices {
        if let Some(vfio_path) = resolve_vfio_group_device(bdf)
            && let Some(rule) = resolve_device(&vfio_path)?
        {
            rules.push(DevRule::char(rule));
        }
    }
    for disk in disks {
        if let Some(dev) = resolve_block_device(disk) {
            let rule = DevRule::block(dev);
            if !rules.contains(&rule) {
                rules.push(rule);
            }
        }
    }
    check_device_count(&rules)?;
    Ok(rules)
}

fn encode_device_rules(rules: &[DevRule]) -> Vec<u8> {
    // Must match struct fluxvm_qemu_dev_rules in bpf/fluxvm_qemu_device.bpf.c:
    // u32 n, then MAX_DEVICES x (major, minor, dev_type, access_mask).
    let mut value = Vec::with_capacity(4 + MAX_DEVICES * 16);
    value.extend_from_slice(&(rules.len() as u32).to_ne_bytes());
    for r in rules {
        value.extend_from_slice(&r.major.to_ne_bytes());
        value.extend_from_slice(&r.minor.to_ne_bytes());
        value.extend_from_slice(&r.dev_type.to_ne_bytes());
        value.extend_from_slice(&r.access.to_ne_bytes());
    }
    for _ in rules.len()..MAX_DEVICES {
        value.extend_from_slice(&[0u8; 16]);
    }
    value
}

/// Attach both Set 7S programs to `cgroup_path` (already created by
/// `fluxvm_cgroup::CgroupManager::create_and_migrate`). Called right after
/// that succeeds. Errors here are the caller's to treat as best-effort (like
/// `VmManager::attach_cgroup` already treats cgroup creation itself) — this
/// is defense-in-depth layered on top of the cgroup, not the cgroup/VM
/// launch itself, so a failure should be logged, not fail the VM.
///
/// `disks` are the VM's root and data disk paths; those that are host block
/// devices are allowed read-write.
pub fn attach(
    cfg: &DataplaneConfig,
    id: Uuid,
    cgroup_path: &Path,
    vfio_devices: &[String],
    disks: &[PathBuf],
) -> Result<()> {
    require_bpftool()?;
    let obj_dir = cfg
        .bpf_object
        .parent()
        .context("dataplane bpf_object path has no parent directory")?;
    let device_obj = obj_dir.join("fluxvm_qemu_device.bpf.o");
    let egress_obj = obj_dir.join("fluxvm_qemu_egress.bpf.o");
    if !device_obj.exists() || !egress_obj.exists() {
        bail!(
            "Set 7S eBPF objects not found next to {} (expected fluxvm_qemu_device.bpf.o and fluxvm_qemu_egress.bpf.o)",
            cfg.bpf_object.display()
        );
    }

    let pin_dir = qemu_pin_dir(&cfg.pin_root, id);
    // Best-effort cleanup of a stale prior attach (e.g. a crashed daemon
    // reattaching on reconcile) before creating fresh pins.
    let _ = detach_inner(&pin_dir, cgroup_path);
    let prog_dir = pin_dir.join("progs");
    let map_dir = pin_dir.join("maps");
    fs::create_dir_all(&prog_dir).with_context(|| format!("creating {}", prog_dir.display()))?;
    fs::create_dir_all(&map_dir).with_context(|| format!("creating {}", map_dir.display()))?;

    let device_pin = prog_dir.join("fluxvm_qemu_device");
    if let Err(e) = run(
        "bpftool",
        &[
            "prog".into(),
            "load".into(),
            device_obj.display().to_string(),
            device_pin.display().to_string(),
            "type".into(),
            "cgroup/dev".into(),
            "pinmaps".into(),
            map_dir.display().to_string(),
        ],
    ) {
        let _ = fs::remove_dir_all(&pin_dir);
        return Err(e).context("loading fluxvm_qemu_device");
    }

    let rules = match collect_device_rules(vfio_devices, disks) {
        Ok(rules) => rules,
        Err(e) => {
            let _ = fs::remove_dir_all(&pin_dir);
            return Err(e);
        }
    };
    if let Err(e) = bpftool_map_update(
        &map_dir.join("fluxvm_qemu_dev"),
        &0u32.to_ne_bytes(),
        &encode_device_rules(&rules),
    ) {
        let _ = fs::remove_dir_all(&pin_dir);
        return Err(e).context("populating fluxvm_qemu_dev");
    }

    let egress_pin = prog_dir.join("fluxvm_qemu_egress");
    if let Err(e) = run(
        "bpftool",
        &[
            "prog".into(),
            "load".into(),
            egress_obj.display().to_string(),
            egress_pin.display().to_string(),
            "type".into(),
            "cgroup_skb/egress".into(),
        ],
    ) {
        let _ = fs::remove_dir_all(&pin_dir);
        return Err(e).context("loading fluxvm_qemu_egress");
    }

    if let Err(e) = run(
        "bpftool",
        &[
            "cgroup".into(),
            "attach".into(),
            cgroup_path.display().to_string(),
            "device".into(),
            "pinned".into(),
            device_pin.display().to_string(),
        ],
    ) {
        let _ = fs::remove_dir_all(&pin_dir);
        return Err(e).context("attaching fluxvm_qemu_device to cgroup");
    }
    if let Err(e) = run(
        "bpftool",
        &[
            "cgroup".into(),
            "attach".into(),
            cgroup_path.display().to_string(),
            "egress".into(),
            "pinned".into(),
            egress_pin.display().to_string(),
        ],
    ) {
        let _ = run(
            "bpftool",
            &[
                "cgroup".into(),
                "detach".into(),
                cgroup_path.display().to_string(),
                "device".into(),
                "pinned".into(),
                device_pin.display().to_string(),
            ],
        );
        let _ = fs::remove_dir_all(&pin_dir);
        return Err(e).context("attaching fluxvm_qemu_egress to cgroup");
    }

    Ok(())
}

/// Rewrite a running VM's device allowlist for its current `disks`: before a
/// block device is hot-added, and after one is detached. A VM without the
/// Set 7S programs (legacy dataplane, or attach failed) is left alone.
pub fn update(
    cfg: &DataplaneConfig,
    id: Uuid,
    vfio_devices: &[String],
    disks: &[PathBuf],
) -> Result<()> {
    let map = qemu_pin_dir(&cfg.pin_root, id)
        .join("maps")
        .join("fluxvm_qemu_dev");
    if !map.exists() {
        return Ok(());
    }
    let rules = collect_device_rules(vfio_devices, disks)?;
    bpftool_map_update(&map, &0u32.to_ne_bytes(), &encode_device_rules(&rules))
        .context("updating fluxvm_qemu_dev")
}

/// Detach both programs from `cgroup_path` and remove their pins. Call this
/// before removing the cgroup itself (cgroup v2 detaches attached programs
/// automatically on cgroup removal, but leaves the bpffs pins behind).
pub fn detach(cfg: &DataplaneConfig, id: Uuid, cgroup_path: &Path) -> Result<()> {
    detach_inner(&qemu_pin_dir(&cfg.pin_root, id), cgroup_path)
}

fn detach_inner(pin_dir: &Path, cgroup_path: &Path) -> Result<()> {
    let prog_dir = pin_dir.join("progs");
    for (attach_type, name) in [
        ("device", "fluxvm_qemu_device"),
        ("egress", "fluxvm_qemu_egress"),
    ] {
        let pin = prog_dir.join(name);
        if pin.exists() {
            let _ = run(
                "bpftool",
                &[
                    "cgroup".into(),
                    "detach".into(),
                    cgroup_path.display().to_string(),
                    attach_type.into(),
                    "pinned".into(),
                    pin.display().to_string(),
                ],
            );
        }
    }
    if pin_dir.exists() {
        fs::remove_dir_all(pin_dir).with_context(|| format!("removing {}", pin_dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // LANANA numbers are a Linux convention; macOS encodes device numbers differently.
    #[cfg(target_os = "linux")]
    #[test]
    fn major_minor_matches_known_devices() {
        // /dev/null and /dev/zero are LANANA-fixed at 1:3 and 1:5.
        let null = fs::metadata("/dev/null");
        let zero = fs::metadata("/dev/zero");
        if let (Ok(null), Ok(zero)) = (null, zero) {
            assert_eq!(major_minor(null.rdev()), (1, 3));
            assert_eq!(major_minor(zero.rdev()), (1, 5));
        }
    }

    #[test]
    fn encode_device_rules_matches_bpf_struct_layout() {
        let value = encode_device_rules(&[DevRule::char((10, 232)), DevRule::block((259, 1))]);
        assert_eq!(value.len(), 4 + MAX_DEVICES * 16);
        assert_eq!(&value[0..4], &2u32.to_ne_bytes());
        assert_eq!(&value[4..8], &10u32.to_ne_bytes());
        assert_eq!(&value[8..12], &232u32.to_ne_bytes());
        assert_eq!(&value[12..16], &BPF_DEVCG_DEV_CHAR.to_ne_bytes());
        assert_eq!(&value[16..20], &BPF_DEVCG_ACC_ALL.to_ne_bytes());
        assert_eq!(&value[20..24], &259u32.to_ne_bytes());
        assert_eq!(&value[28..32], &BPF_DEVCG_DEV_BLOCK.to_ne_bytes());
        assert_eq!(
            &value[32..36],
            &(BPF_DEVCG_ACC_READ | BPF_DEVCG_ACC_WRITE).to_ne_bytes()
        );
        // Unused trailing slots are zeroed.
        assert!(value[36..].iter().all(|&b| b == 0));
    }

    #[test]
    fn too_many_devices_is_rejected() {
        let over_cap: Vec<DevRule> = (0..MAX_DEVICES as u32 + 1)
            .map(|i| DevRule::char((10, i)))
            .collect();
        assert!(check_device_count(&over_cap).is_err());
        let at_cap: Vec<DevRule> = (0..MAX_DEVICES as u32)
            .map(|i| DevRule::char((10, i)))
            .collect();
        assert!(check_device_count(&at_cap).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn regular_files_are_not_block_rules() {
        let dir = std::env::temp_dir().join(format!("qemu-cgroup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("disk.raw");
        fs::write(&file, b"").unwrap();
        let link = dir.join("link.raw");
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert_eq!(resolve_block_device(&file), None);
        assert_eq!(resolve_block_device(&link), None);
        assert_eq!(resolve_block_device(&dir.join("missing")), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
