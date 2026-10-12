// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
//!
//! Mediated devices (vfio-mdev): vGPU profiles a parent device offers, the
//! instances that exist, and explicit create/remove. A VM takes an instance
//! by listing its UUID in `CreateVmRequest.vfio_devices`.
//!
//! Every function takes the filesystem root (`/` in production) so tests can
//! build a fake sysfs.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::gpu::valid_pci_bdf;

const MDEV_BUS: &str = "sys/class/mdev_bus";
const MDEV_DEVICES: &str = "sys/bus/mdev/devices";

/// A vGPU slice backed by a MIG GPU instance, from an NVIDIA profile name
/// such as `A100-3-20C` (3 compute slices, 20 GiB).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigProfile {
    pub slices: u32,
    pub memory_gib: u32,
}

/// One entry of a parent's `mdev_supported_types`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MdevType {
    /// The sysfs directory name, e.g. `nvidia-558` or `mbochs-small`.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_api: Option<String>,
    /// Instances of this type the parent can still create.
    pub available_instances: u32,
    /// Guest video memory per instance, parsed from the description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framebuffer_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mig: Option<MigProfile>,
}

/// A device that can create mediated devices.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MdevParent {
    /// The `mdev_bus` name: a PCI address or a virtual device's name.
    pub parent: String,
    /// True when `parent` is a PCI address.
    pub pci: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    pub types: Vec<MdevType>,
}

/// A mediated device that exists now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MdevInstance {
    pub uuid: String,
    pub parent: String,
    pub type_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iommu_group: Option<u32>,
    /// Some VM lists the UUID in `vfio_devices`.
    pub allocated: bool,
    /// A process has the instance's VFIO group open.
    pub held: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MdevInventory {
    pub parents: Vec<MdevParent>,
    pub instances: Vec<MdevInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MdevCreateRequest {
    pub parent: String,
    #[serde(rename = "type")]
    pub type_id: String,
    /// Chosen by the caller so a retry is idempotent; generated when unset.
    #[serde(default)]
    pub uuid: Option<String>,
}

/// The parent has no instance of the type left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdevShortage {
    pub parent: String,
    pub type_id: String,
}

impl std::fmt::Display for MdevShortage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} has no {} instance available",
            self.parent, self.type_id
        )
    }
}

impl std::error::Error for MdevShortage {}

/// The UUID exists with another parent or type, or is still in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdevConflict(pub String);

impl std::fmt::Display for MdevConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MdevConflict {}

/// A lowercase or uppercase 8-4-4-4-12 hex UUID.
pub fn valid_mdev_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// A sysfs directory name we are willing to join onto a path.
fn valid_sysfs_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':'))
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_hex16(path: &Path) -> Option<u16> {
    u16::from_str_radix(read_trim(path)?.trim_start_matches("0x"), 16).ok()
}

/// Guest video memory from an mdev type description. Knows NVIDIA's
/// `framebuffer=2048M` and the kernel samples' `4 MB video memory`.
pub fn parse_framebuffer_mib(description: &str) -> Option<u64> {
    let lower = description.to_ascii_lowercase();
    if let Some(i) = lower.find("framebuffer=") {
        let rest = &lower[i + "framebuffer=".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let n: u64 = digits.parse().ok()?;
        return match rest[digits.len()..].chars().next() {
            Some('g') => Some(n * 1024),
            Some('m') => Some(n),
            Some('k') => Some(n / 1024),
            _ => Some(n / (1024 * 1024)),
        };
    }
    let i = lower.find("video memory")?;
    let mut words = lower[..i].split_whitespace().rev();
    let unit = words.next()?;
    let n: u64 = words.next()?.parse().ok()?;
    match unit {
        "mb" | "mib" => Some(n),
        "gb" | "gib" => Some(n * 1024),
        _ => None,
    }
}

/// MIG geometry from an NVIDIA vGPU profile name: `<GPU>-<slices>-<GiB>C`,
/// e.g. `GRID A100-3-20C` or `NVIDIA H100-1-10C`.
pub fn parse_mig(name: &str) -> Option<MigProfile> {
    let last = name.split_whitespace().last()?;
    let parts: Vec<&str> = last.split('-').collect();
    if parts.len() < 3 {
        return None;
    }
    let mem = parts[parts.len() - 1];
    let mem = mem.strip_suffix('C').or_else(|| mem.strip_suffix('c'))?;
    let slices: u32 = parts[parts.len() - 2].parse().ok()?;
    let memory_gib: u32 = mem.parse().ok()?;
    (slices > 0 && memory_gib > 0).then_some(MigProfile { slices, memory_gib })
}

fn mdev_types(parent_dir: &Path) -> Vec<MdevType> {
    let Ok(entries) = fs::read_dir(parent_dir.join("mdev_supported_types")) else {
        return vec![];
    };
    let mut out: Vec<MdevType> = entries
        .flatten()
        .map(|e| {
            let dir = e.path();
            let name = read_trim(&dir.join("name"));
            let description = read_trim(&dir.join("description"));
            MdevType {
                id: e.file_name().to_string_lossy().into_owned(),
                framebuffer_mib: description.as_deref().and_then(parse_framebuffer_mib),
                mig: name.as_deref().and_then(parse_mig),
                available_instances: read_trim(&dir.join("available_instances"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                device_api: read_trim(&dir.join("device_api")),
                name,
                description,
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn link_name(path: &Path) -> Option<String> {
    let target = fs::canonicalize(path).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// Reads every mdev parent and instance under `root`. `allocated` are the
/// lowercase UUIDs listed in VMs' `vfio_devices`.
pub fn discover(root: &Path, allocated: &HashSet<String>) -> MdevInventory {
    let mut inv = MdevInventory::default();
    if let Ok(entries) = fs::read_dir(root.join(MDEV_BUS)) {
        for e in entries.flatten() {
            let parent = e.file_name().to_string_lossy().into_owned();
            let dir = e.path();
            let pci = valid_pci_bdf(&parent);
            inv.parents.push(MdevParent {
                vendor_id: pci.then(|| read_hex16(&dir.join("vendor"))).flatten(),
                device_id: pci.then(|| read_hex16(&dir.join("device"))).flatten(),
                numa_node: read_trim(&dir.join("numa_node"))
                    .and_then(|s| s.parse::<i64>().ok())
                    .filter(|n| *n >= 0)
                    .map(|n| n as u32),
                types: mdev_types(&dir),
                parent,
                pci,
            });
        }
    }
    inv.parents.sort_by(|a, b| a.parent.cmp(&b.parent));
    if let Ok(entries) = fs::read_dir(root.join(MDEV_DEVICES)) {
        for e in entries.flatten() {
            let uuid = e.file_name().to_string_lossy().to_ascii_lowercase();
            if !valid_mdev_uuid(&uuid) {
                continue;
            }
            if let Some(inst) = instance(root, &uuid, allocated) {
                inv.instances.push(inst);
            }
        }
    }
    inv.instances.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    inv
}

fn instance(root: &Path, uuid: &str, allocated: &HashSet<String>) -> Option<MdevInstance> {
    let dir = root.join(MDEV_DEVICES).join(uuid);
    let canonical = fs::canonicalize(&dir).ok()?;
    let parent = canonical
        .parent()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    let type_id = link_name(&dir.join("mdev_type"))?;
    let iommu_group = link_name(&dir.join("iommu_group")).and_then(|g| g.parse().ok());
    let held = root == Path::new("/") && iommu_group.is_some_and(crate::gpu::vfio_group_held);
    Some(MdevInstance {
        allocated: allocated.contains(uuid),
        uuid: uuid.to_string(),
        parent,
        type_id,
        iommu_group,
        held,
    })
}

fn type_dir(root: &Path, parent: &str, type_id: &str) -> Result<PathBuf> {
    if !valid_sysfs_name(parent) {
        bail!("invalid mdev parent {parent:?}");
    }
    if !valid_sysfs_name(type_id) {
        bail!("invalid mdev type {type_id:?}");
    }
    let parent_dir = root.join(MDEV_BUS).join(parent);
    if !parent_dir.exists() {
        bail!("{parent} is not an mdev parent on this host");
    }
    let dir = parent_dir.join("mdev_supported_types").join(type_id);
    if !dir.exists() {
        bail!("{parent} does not offer mdev type {type_id}");
    }
    Ok(dir)
}

/// Creates a mediated device. Idempotent: an existing instance with the
/// same UUID, parent and type is returned as is.
pub fn create(
    root: &Path,
    req: &MdevCreateRequest,
    allocated: &HashSet<String>,
) -> Result<MdevInstance> {
    let uuid = match &req.uuid {
        Some(u) if valid_mdev_uuid(u) => u.to_ascii_lowercase(),
        Some(u) => bail!("invalid mdev UUID {u:?}"),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let dir = type_dir(root, &req.parent, &req.type_id)?;
    if let Some(existing) = instance(root, &uuid, allocated) {
        if existing.parent == req.parent && existing.type_id == req.type_id {
            return Ok(existing);
        }
        return Err(MdevConflict(format!(
            "mdev {uuid} already exists as {} on {}",
            existing.type_id, existing.parent
        ))
        .into());
    }
    let available: u32 = read_trim(&dir.join("available_instances"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if available == 0 {
        return Err(MdevShortage {
            parent: req.parent.clone(),
            type_id: req.type_id.clone(),
        }
        .into());
    }
    fs::write(dir.join("create"), &uuid)
        .with_context(|| format!("creating {} mdev {uuid} on {}", req.type_id, req.parent))?;
    instance(root, &uuid, allocated)
        .with_context(|| format!("mdev {uuid} did not appear after create"))
}

/// Removes a mediated device. Absent is success; one a VM lists or a
/// process holds is refused.
pub fn remove(root: &Path, uuid: &str, allocated: &HashSet<String>) -> Result<()> {
    if !valid_mdev_uuid(uuid) {
        bail!("invalid mdev UUID {uuid:?}");
    }
    let uuid = uuid.to_ascii_lowercase();
    let Some(inst) = instance(root, &uuid, allocated) else {
        return Ok(());
    };
    if inst.allocated {
        return Err(MdevConflict(format!(
            "mdev {uuid} is listed on a VM; delete the VM first"
        ))
        .into());
    }
    if inst.held {
        return Err(MdevConflict(format!("mdev {uuid} is open by a process")).into());
    }
    fs::write(root.join(MDEV_DEVICES).join(&uuid).join("remove"), "1")
        .with_context(|| format!("removing mdev {uuid}"))
}

/// The live host's mediated devices.
pub fn discover_host(allocated: &HashSet<String>) -> MdevInventory {
    discover(Path::new("/"), allocated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write(root: &Path, rel: &str, data: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    /// A PCI vGPU parent and the virtual `mbochs` sample parent.
    fn host() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        let gpu = "sys/devices/pci0000:00/0000:00:01.0/0000:01:00.0";
        write(r, &format!("{gpu}/vendor"), "0x10de\n");
        write(r, &format!("{gpu}/device"), "0x20b5\n");
        write(r, &format!("{gpu}/numa_node"), "1\n");
        let t1 = format!("{gpu}/mdev_supported_types/nvidia-558");
        write(r, &format!("{t1}/name"), "GRID A100-3-20C\n");
        write(
            r,
            &format!("{t1}/description"),
            "num_heads=1, frl_config=60, framebuffer=20480M, max_resolution=4096x2160, max_instance=2\n",
        );
        write(r, &format!("{t1}/available_instances"), "2\n");
        write(r, &format!("{t1}/device_api"), "vfio-pci\n");
        let mb = "sys/devices/virtual/mbochs/mbochs";
        write(r, &format!("{mb}/numa_node"), "-1\n");
        for (id, mbytes, avail) in [("mbochs-small", 4, 64), ("mbochs-large", 64, 0)] {
            let d = format!("{mb}/mdev_supported_types/{id}");
            write(r, &format!("{d}/name"), &format!("{id}\n"));
            write(
                r,
                &format!("{d}/description"),
                &format!("virtual display, {mbytes} MB video memory\n"),
            );
            write(
                r,
                &format!("{d}/available_instances"),
                &format!("{avail}\n"),
            );
            write(r, &format!("{d}/device_api"), "vfio-pci\n");
        }
        fs::create_dir_all(r.join(MDEV_BUS)).unwrap();
        symlink(r.join(gpu), r.join(MDEV_BUS).join("0000:01:00.0")).unwrap();
        symlink(r.join(mb), r.join(MDEV_BUS).join("mbochs")).unwrap();
        fs::create_dir_all(r.join(MDEV_DEVICES)).unwrap();
        t
    }

    /// What the kernel does on a write to `create`.
    fn fake_create(r: &Path, parent_rel: &str, type_id: &str, uuid: &str, group: u32) {
        let inst = r.join(parent_rel).join(uuid);
        fs::create_dir_all(&inst).unwrap();
        symlink(
            r.join(parent_rel)
                .join("mdev_supported_types")
                .join(type_id),
            inst.join("mdev_type"),
        )
        .unwrap();
        let g = r.join(format!("sys/kernel/iommu_groups/{group}"));
        fs::create_dir_all(&g).unwrap();
        symlink(&g, inst.join("iommu_group")).unwrap();
        symlink(&inst, r.join(MDEV_DEVICES).join(uuid)).unwrap();
    }

    const U1: &str = "5a3c1d2e-0000-4000-8000-000000000001";

    #[test]
    fn parses_framebuffer_in_nvidia_and_sample_formats() {
        assert_eq!(
            parse_framebuffer_mib("num_heads=4, framebuffer=2048M, max_instance=8"),
            Some(2048)
        );
        assert_eq!(parse_framebuffer_mib("framebuffer=4G"), Some(4096));
        assert_eq!(
            parse_framebuffer_mib("virtual display, 16 MB video memory"),
            Some(16)
        );
        assert_eq!(
            parse_framebuffer_mib("virtual display, 1 GB video memory"),
            Some(1024)
        );
        assert_eq!(parse_framebuffer_mib("a serial port"), None);
    }

    #[test]
    fn parses_mig_backed_profile_names_only() {
        assert_eq!(
            parse_mig("GRID A100-3-20C"),
            Some(MigProfile {
                slices: 3,
                memory_gib: 20
            })
        );
        assert_eq!(
            parse_mig("NVIDIA H100-1-10C"),
            Some(MigProfile {
                slices: 1,
                memory_gib: 10
            })
        );
        assert_eq!(parse_mig("GRID T4-4Q"), None);
        assert_eq!(parse_mig("NVIDIA A100-40C"), None);
        assert_eq!(parse_mig("mbochs-small"), None);
    }

    #[test]
    fn uuid_check_rejects_paths_and_short_strings() {
        assert!(valid_mdev_uuid(U1));
        assert!(valid_mdev_uuid(&U1.to_ascii_uppercase()));
        assert!(!valid_mdev_uuid(
            "../../../etc/passwd/aaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(!valid_mdev_uuid("0000:01:00.0"));
    }

    #[test]
    fn discovers_pci_and_virtual_parents_with_types() {
        let t = host();
        let inv = discover(t.path(), &HashSet::new());
        assert_eq!(inv.parents.len(), 2);
        let gpu = &inv.parents[0];
        assert_eq!((gpu.parent.as_str(), gpu.pci), ("0000:01:00.0", true));
        assert_eq!(
            (gpu.vendor_id, gpu.device_id, gpu.numa_node),
            (Some(0x10de), Some(0x20b5), Some(1))
        );
        let ty = &gpu.types[0];
        assert_eq!(ty.id, "nvidia-558");
        assert_eq!(ty.framebuffer_mib, Some(20480));
        assert_eq!(
            ty.mig,
            Some(MigProfile {
                slices: 3,
                memory_gib: 20
            })
        );
        assert_eq!(ty.available_instances, 2);
        let mb = &inv.parents[1];
        assert_eq!(
            (mb.parent.as_str(), mb.pci, mb.vendor_id, mb.numa_node),
            ("mbochs", false, None, None)
        );
        let ids: Vec<_> = mb
            .types
            .iter()
            .map(|t| (t.id.as_str(), t.framebuffer_mib, t.available_instances))
            .collect();
        assert_eq!(
            ids,
            [("mbochs-large", Some(64), 0), ("mbochs-small", Some(4), 64)]
        );
        assert!(inv.instances.is_empty());
    }

    #[test]
    fn create_writes_the_uuid_and_refuses_bad_input() {
        let t = host();
        let r = t.path();
        let req = |parent: &str, ty: &str, uuid: Option<&str>| MdevCreateRequest {
            parent: parent.into(),
            type_id: ty.into(),
            uuid: uuid.map(Into::into),
        };
        // The fake sysfs has no kernel behind it, so the instance never appears.
        let err = create(r, &req("mbochs", "mbochs-small", Some(U1)), &HashSet::new()).unwrap_err();
        assert!(err.to_string().contains("did not appear"), "{err:#}");
        assert_eq!(
            fs::read_to_string(r.join(
                "sys/devices/virtual/mbochs/mbochs/mdev_supported_types/mbochs-small/create"
            ))
            .unwrap(),
            U1
        );
        let err = create(r, &req("mbochs", "mbochs-large", Some(U1)), &HashSet::new()).unwrap_err();
        assert!(err.downcast_ref::<MdevShortage>().is_some(), "{err:#}");
        for (p, ty, u) in [
            ("../mbochs", "mbochs-small", None),
            ("mbochs", "../../x", None),
            ("mbochs", "mbochs-small", Some("not-a-uuid")),
            ("0000:02:00.0", "nvidia-558", None),
            ("mbochs", "mbochs-huge", None),
        ] {
            assert!(
                create(r, &req(p, ty, u), &HashSet::new()).is_err(),
                "{p} {ty} {u:?}"
            );
        }
    }

    #[test]
    fn create_is_idempotent_and_detects_a_uuid_clash() {
        let t = host();
        let r = t.path();
        let mb = "sys/devices/virtual/mbochs/mbochs";
        fake_create(r, mb, "mbochs-small", U1, 12);
        let req = MdevCreateRequest {
            parent: "mbochs".into(),
            type_id: "mbochs-small".into(),
            uuid: Some(U1.to_ascii_uppercase()),
        };
        let inst = create(r, &req, &HashSet::new()).unwrap();
        assert_eq!(
            (
                inst.uuid.as_str(),
                inst.parent.as_str(),
                inst.type_id.as_str(),
                inst.iommu_group
            ),
            (U1, "mbochs", "mbochs-small", Some(12))
        );
        let other = MdevCreateRequest {
            type_id: "mbochs-large".into(),
            ..req
        };
        let err = create(r, &other, &HashSet::new()).unwrap_err();
        assert!(err.downcast_ref::<MdevConflict>().is_some(), "{err:#}");

        let allocated: HashSet<String> = [U1.to_string()].into();
        let inv = discover(r, &allocated);
        assert_eq!(inv.instances.len(), 1);
        assert!(inv.instances[0].allocated && !inv.instances[0].held);
    }

    #[test]
    fn remove_is_idempotent_and_refuses_an_allocated_instance() {
        let t = host();
        let r = t.path();
        let mb = "sys/devices/virtual/mbochs/mbochs";
        fake_create(r, mb, "mbochs-small", U1, 12);
        let allocated: HashSet<String> = [U1.to_string()].into();
        let err = remove(r, U1, &allocated).unwrap_err();
        assert!(err.downcast_ref::<MdevConflict>().is_some(), "{err:#}");
        remove(r, U1, &HashSet::new()).unwrap();
        assert_eq!(
            fs::read_to_string(r.join(mb).join(U1).join("remove")).unwrap(),
            "1"
        );
        remove(r, "5a3c1d2e-0000-4000-8000-0000000000ff", &HashSet::new()).unwrap();
        assert!(remove(r, "../x", &HashSet::new()).is_err());
    }
}
