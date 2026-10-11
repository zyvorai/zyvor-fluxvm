// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
//!
//! Host hardware graph for topology-aware placement: NUMA nodes (CPUs,
//! thread siblings, memory, hugepages, distances) and the PCI functions an
//! orchestrator may hand to a VM (GPUs, NICs and their SR-IOV functions,
//! NVMe controllers, accelerators), each with its NUMA node, PCIe root and
//! IOMMU group. Read-only; every path is relative to a sysfs root so tests
//! can run against fixture trees.

use crate::gpu::valid_pci_bdf;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostTopology {
    pub numa_nodes: Vec<NumaNodeInfo>,
    pub devices: Vec<PciDeviceInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct NumaNodeInfo {
    pub id: u32,
    /// Online CPUs on the node, ascending.
    pub cpus: Vec<u32>,
    /// Hyperthread sibling groups (each ascending), ordered by first CPU.
    pub thread_siblings: Vec<Vec<u32>>,
    pub memory_mib: u64,
    pub free_memory_mib: u64,
    pub hugepages: Vec<HugepagePool>,
    /// SLIT distances to every node, in node id order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub distances: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HugepagePool {
    pub size_kib: u64,
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum DeviceClass {
    Gpu,
    Nic,
    Nvme,
    Accelerator,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PciDeviceInfo {
    pub bdf: String,
    pub class: DeviceClass,
    pub class_id: u32,
    pub vendor_id: u16,
    pub device_id: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    /// `pciDDDD:BB` for a function on the root complex, else
    /// `pciDDDD:BB/<root port BDF>`: functions under the same root share a
    /// PCIe hierarchy and can talk peer to peer without the host bridge.
    pub pcie_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iommu_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iommu_members: Vec<String>,
    /// SR-IOV physical function: its capacity and current virtual functions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sriov: Option<SriovInfo>,
    /// Set on a virtual function: its physical function's BDF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physfn: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub netdevs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_speed_mbps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vram_gib: Option<u32>,
    /// A member of its IOMMU group is listed in some VM's `vfio_devices`.
    pub allocated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SriovInfo {
    pub total_vfs: u32,
    pub num_vfs: u32,
    pub vfs: Vec<String>,
}

const GPU_CLASSES: [u32; 2] = [0x0300, 0x0302];
const NIC_CLASS: u32 = 0x0200;
const NVME_CLASS: u32 = 0x01_08_02;
const ACCELERATOR_CLASS: u32 = 0x12;

fn classify(class_id: u32) -> Option<DeviceClass> {
    if GPU_CLASSES.contains(&(class_id >> 8)) {
        Some(DeviceClass::Gpu)
    } else if class_id >> 8 == NIC_CLASS {
        Some(DeviceClass::Nic)
    } else if class_id == NVME_CLASS {
        Some(DeviceClass::Nvme)
    } else if class_id >> 16 == ACCELERATOR_CLASS {
        Some(DeviceClass::Accelerator)
    } else {
        None
    }
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn read_hex(path: &Path) -> Option<u32> {
    let s = read_trim(path)?;
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

/// Parses a kernel CPU list such as `0-3,8,10-11`.
pub fn parse_cpu_list(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>())
                    && a <= b
                {
                    out.extend(a..=b);
                }
            }
            None => {
                if let Ok(n) = part.trim().parse() {
                    out.push(n);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Formats CPUs as a kernel CPU list, collapsing runs.
pub fn format_cpu_list(cpus: &[u32]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    parts.join(",")
}

/// `Node 0 MemTotal: 32768 kB` (node meminfo) or `MemTotal: 32768 kB`.
fn meminfo_kib(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.split_once(key)?.1.trim_start_matches(':').trim();
        rest.split_whitespace().next()?.parse().ok()
    })
}

fn thread_siblings(root: &Path, cpus: &[u32]) -> Vec<Vec<u32>> {
    let mut groups: Vec<Vec<u32>> = Vec::new();
    let mut seen = HashSet::new();
    for &cpu in cpus {
        if seen.contains(&cpu) {
            continue;
        }
        let list = read_trim(&root.join(format!(
            "sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        )))
        .map(|s| parse_cpu_list(&s))
        .unwrap_or_default();
        let mut group: Vec<u32> = list.into_iter().filter(|c| cpus.contains(c)).collect();
        if !group.contains(&cpu) {
            group = vec![cpu];
        }
        seen.extend(group.iter().copied());
        groups.push(group);
    }
    groups
}

fn hugepage_pools(dir: &Path) -> Vec<HugepagePool> {
    let mut pools = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return pools;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(size) = name
            .strip_prefix("hugepages-")
            .and_then(|s| s.strip_suffix("kB"))
            .and_then(|s| s.parse().ok())
        else {
            continue;
        };
        pools.push(HugepagePool {
            size_kib: size,
            total: read_u64(&e.path().join("nr_hugepages")).unwrap_or(0),
            free: read_u64(&e.path().join("free_hugepages")).unwrap_or(0),
        });
    }
    pools.sort_by_key(|p| p.size_kib);
    pools
}

fn numa_nodes(root: &Path) -> Vec<NumaNodeInfo> {
    let online = read_trim(&root.join("sys/devices/system/cpu/online"))
        .map(|s| parse_cpu_list(&s))
        .unwrap_or_default();
    let node_dir = root.join("sys/devices/system/node");
    let mut ids: Vec<u32> = fs::read_dir(&node_dir)
        .map(|it| {
            it.flatten()
                .filter_map(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .strip_prefix("node")?
                        .parse()
                        .ok()
                })
                .collect()
        })
        .unwrap_or_default();
    ids.sort_unstable();
    if ids.is_empty() {
        // A kernel without CONFIG_NUMA: one node holding everything.
        let meminfo = read_trim(&root.join("proc/meminfo")).unwrap_or_default();
        return vec![NumaNodeInfo {
            id: 0,
            thread_siblings: thread_siblings(root, &online),
            cpus: online,
            memory_mib: meminfo_kib(&meminfo, "MemTotal").unwrap_or(0) / 1024,
            free_memory_mib: meminfo_kib(&meminfo, "MemFree").unwrap_or(0) / 1024,
            hugepages: hugepage_pools(&root.join("sys/kernel/mm/hugepages")),
            distances: vec![],
        }];
    }
    ids.into_iter()
        .map(|id| {
            let dir = node_dir.join(format!("node{id}"));
            let mut cpus = read_trim(&dir.join("cpulist"))
                .map(|s| parse_cpu_list(&s))
                .unwrap_or_default();
            if !online.is_empty() {
                cpus.retain(|c| online.contains(c));
            }
            let meminfo = read_trim(&dir.join("meminfo")).unwrap_or_default();
            NumaNodeInfo {
                id,
                thread_siblings: thread_siblings(root, &cpus),
                cpus,
                memory_mib: meminfo_kib(&meminfo, "MemTotal").unwrap_or(0) / 1024,
                free_memory_mib: meminfo_kib(&meminfo, "MemFree").unwrap_or(0) / 1024,
                hugepages: hugepage_pools(&dir.join("hugepages")),
                distances: read_trim(&dir.join("distance"))
                    .map(|s| {
                        s.split_whitespace()
                            .filter_map(|d| d.parse().ok())
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect()
}

fn link_bdf(path: &Path) -> Option<String> {
    let target = fs::canonicalize(path).ok()?;
    let name = target.file_name()?.to_string_lossy().to_ascii_lowercase();
    valid_pci_bdf(&name).then_some(name)
}

/// The PCIe hierarchy a function sits under, from its canonical sysfs path
/// `.../devices/pciDDDD:BB/<root port>/.../<bdf>`.
fn pcie_root(canonical: &Path) -> String {
    let comps: Vec<String> = canonical
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let Some(i) = comps
        .iter()
        .position(|c| c.starts_with("pci") && c.contains(':'))
    else {
        return String::new();
    };
    match comps.get(i + 1) {
        Some(port) if i + 2 < comps.len() && valid_pci_bdf(port) => {
            format!("{}/{}", comps[i], port)
        }
        _ => comps[i].clone(),
    }
}

fn iommu_group(dev: &Path) -> (Option<u32>, Vec<String>) {
    let Ok(group) = fs::canonicalize(dev.join("iommu_group")) else {
        return (None, vec![]);
    };
    let id = group
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse().ok());
    let mut members: Vec<String> = fs::read_dir(group.join("devices"))
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().to_ascii_lowercase())
                .filter(|n| valid_pci_bdf(n))
                .collect()
        })
        .unwrap_or_default();
    members.sort();
    (id, members)
}

fn sriov(dev: &Path) -> Option<SriovInfo> {
    let total = read_u64(&dev.join("sriov_totalvfs"))? as u32;
    if total == 0 {
        return None;
    }
    let mut vfs: Vec<(u32, String)> = fs::read_dir(dev)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n: u32 = e
                .file_name()
                .to_string_lossy()
                .strip_prefix("virtfn")?
                .parse()
                .ok()?;
            Some((n, link_bdf(&e.path())?))
        })
        .collect();
    vfs.sort();
    Some(SriovInfo {
        total_vfs: total,
        num_vfs: read_u64(&dev.join("sriov_numvfs")).unwrap_or(vfs.len() as u64) as u32,
        vfs: vfs.into_iter().map(|(_, b)| b).collect(),
    })
}

fn sub_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Bytes across an NVMe controller's namespaces (`size` is in 512-byte
/// sectors).
fn nvme_size(dev: &Path) -> Option<u64> {
    let mut total = 0;
    for ctrl in sub_names(&dev.join("nvme")) {
        let cdir = dev.join("nvme").join(&ctrl);
        for ns in sub_names(&cdir)
            .into_iter()
            .filter(|n| n.starts_with(&ctrl))
        {
            total += read_u64(&cdir.join(ns).join("size")).unwrap_or(0) * 512;
        }
    }
    (total > 0).then_some(total)
}

fn pci_devices(
    root: &Path,
    allocated: &HashSet<String>,
    vram_gib: &BTreeMap<String, u32>,
) -> Vec<PciDeviceInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root.join("sys/bus/pci/devices")) else {
        return out;
    };
    for e in entries.flatten() {
        let bdf = e.file_name().to_string_lossy().to_ascii_lowercase();
        if !valid_pci_bdf(&bdf) {
            continue;
        }
        let dev = e.path();
        let Some(class_id) = read_hex(&dev.join("class")) else {
            continue;
        };
        let Some(class) = classify(class_id) else {
            continue;
        };
        let canonical = fs::canonicalize(&dev).unwrap_or_else(|_| dev.clone());
        let (iommu_group, iommu_members) = iommu_group(&dev);
        let netdevs = sub_names(&dev.join("net"));
        let link_speed_mbps = netdevs.iter().find_map(|n| {
            read_trim(&dev.join("net").join(n).join("speed"))?
                .parse::<i64>()
                .ok()
                .filter(|s| *s > 0)
                .map(|s| s as u32)
        });
        out.push(PciDeviceInfo {
            class,
            class_id,
            vendor_id: read_hex(&dev.join("vendor")).unwrap_or(0) as u16,
            device_id: read_hex(&dev.join("device")).unwrap_or(0) as u16,
            driver: fs::canonicalize(dev.join("driver"))
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
            numa_node: read_trim(&dev.join("numa_node"))
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|n| *n >= 0)
                .map(|n| n as u32),
            pcie_root: pcie_root(&canonical),
            iommu_group,
            allocated: allocated.contains(&bdf)
                || iommu_members.iter().any(|m| allocated.contains(m)),
            iommu_members,
            sriov: sriov(&dev),
            physfn: link_bdf(&dev.join("physfn")),
            netdevs,
            link_speed_mbps,
            size_bytes: if class == DeviceClass::Nvme {
                nvme_size(&dev)
            } else {
                None
            },
            vram_gib: vram_gib.get(&bdf).copied(),
            bdf,
        });
    }
    out.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    out
}

/// Reads the host graph under `root` (`/` in production). `allocated` are
/// BDFs listed in VMs' `vfio_devices`; `vram_gib` is operator-recorded VRAM.
pub fn discover(
    root: &Path,
    allocated: &HashSet<String>,
    vram_gib: &BTreeMap<String, u32>,
) -> HostTopology {
    HostTopology {
        numa_nodes: numa_nodes(root),
        devices: pci_devices(root, allocated, vram_gib),
    }
}

/// The live host graph, with VRAM recorded by GPU binds in `state_dir`.
pub fn discover_host(state_dir: &Path, allocated: &HashSet<String>) -> HostTopology {
    let vram = crate::gpu::load_bind_state(state_dir).vram_gib;
    discover(&PathBuf::from("/"), allocated, &vram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// Adds a PCI function at `sys/devices/<path>`, linked from
    /// `sys/bus/pci/devices` and placed in IOMMU group `group`.
    fn pci(root: &Path, path: &str, class: &str, numa: i32, group: u32) -> PathBuf {
        let dev = root.join("sys/devices").join(path);
        fs::create_dir_all(&dev).unwrap();
        let bdf = path.rsplit('/').next().unwrap();
        fs::write(dev.join("class"), format!("{class}\n")).unwrap();
        fs::write(dev.join("vendor"), "0x10de\n").unwrap();
        fs::write(dev.join("device"), "0x2330\n").unwrap();
        fs::write(dev.join("numa_node"), format!("{numa}\n")).unwrap();
        let g = root.join(format!("sys/kernel/iommu_groups/{group}"));
        fs::create_dir_all(g.join("devices")).unwrap();
        symlink(&dev, g.join("devices").join(bdf)).unwrap();
        symlink(&g, dev.join("iommu_group")).unwrap();
        let bus = root.join("sys/bus/pci/devices");
        fs::create_dir_all(&bus).unwrap();
        symlink(&dev, bus.join(bdf)).unwrap();
        dev
    }

    /// Two NUMA nodes (4 CPUs each, hyperthread pairs); a GPU and an
    /// SR-IOV NIC with two VFs under one root port on node 0; an NVMe on
    /// node 1; and a SATA controller, which is not reported.
    fn two_node_host() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let r = t.path();
        write(r, "sys/devices/system/cpu/online", "0-7\n");
        for (node, cpus, free) in [(0, "0-3", 1000), (1, "4-7", 3000)] {
            let d = format!("sys/devices/system/node/node{node}");
            write(r, &format!("{d}/cpulist"), &format!("{cpus}\n"));
            write(
                r,
                &format!("{d}/meminfo"),
                &format!(
                    "Node {node} MemTotal:  8388608 kB\nNode {node} MemFree:  {} kB\n",
                    free * 1024
                ),
            );
            write(
                r,
                &format!("{d}/distance"),
                if node == 0 { "10 21\n" } else { "21 10\n" },
            );
            write(
                r,
                &format!("{d}/hugepages/hugepages-2048kB/nr_hugepages"),
                "512\n",
            );
            write(
                r,
                &format!("{d}/hugepages/hugepages-2048kB/free_hugepages"),
                "256\n",
            );
            write(
                r,
                &format!("{d}/hugepages/hugepages-1048576kB/nr_hugepages"),
                "0\n",
            );
            write(
                r,
                &format!("{d}/hugepages/hugepages-1048576kB/free_hugepages"),
                "0\n",
            );
        }
        for c in 0..8u32 {
            let pair = c & !1;
            write(
                r,
                &format!("sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list"),
                &format!("{pair},{}\n", pair + 1),
            );
        }
        pci(r, "pci0000:00/0000:00:01.0/0000:01:00.0", "0x030200", 0, 10);
        let pf = pci(r, "pci0000:00/0000:00:01.0/0000:02:00.0", "0x020000", 0, 11);
        write(&pf, "sriov_totalvfs", "8\n");
        write(&pf, "sriov_numvfs", "2\n");
        write(&pf, "net/ens1f0/speed", "100000\n");
        for (i, vf) in ["0000:02:00.2", "0000:02:00.3"].iter().enumerate() {
            let v = pci(
                r,
                &format!("pci0000:00/0000:00:01.0/{vf}"),
                "0x020000",
                0,
                12 + i as u32,
            );
            symlink(&v, pf.join(format!("virtfn{i}"))).unwrap();
            symlink(&pf, v.join("physfn")).unwrap();
        }
        let nvme = pci(r, "pci0000:80/0000:80:02.0/0000:81:00.0", "0x010802", 1, 20);
        write(&nvme, "nvme/nvme0/nvme0n1/size", "2000000\n");
        let drv = r.join("sys/bus/pci/drivers/nvme");
        fs::create_dir_all(&drv).unwrap();
        symlink(&drv, nvme.join("driver")).unwrap();
        pci(r, "pci0000:00/0000:00:17.0", "0x010601", 0, 30);
        t
    }

    #[test]
    fn reads_numa_nodes_with_siblings_memory_and_hugepages() {
        let t = two_node_host();
        let topo = discover(t.path(), &HashSet::new(), &BTreeMap::new());
        assert_eq!(topo.numa_nodes.len(), 2);
        let n1 = &topo.numa_nodes[1];
        assert_eq!(n1.id, 1);
        assert_eq!(n1.cpus, [4, 5, 6, 7]);
        assert_eq!(n1.thread_siblings, [vec![4, 5], vec![6, 7]]);
        assert_eq!((n1.memory_mib, n1.free_memory_mib), (8192, 3000));
        assert_eq!(n1.distances, [21, 10]);
        assert_eq!(
            n1.hugepages,
            [
                HugepagePool {
                    size_kib: 2048,
                    total: 512,
                    free: 256
                },
                HugepagePool {
                    size_kib: 1048576,
                    total: 0,
                    free: 0
                },
            ]
        );
    }

    #[test]
    fn reads_devices_with_roots_sriov_and_nvme() {
        let t = two_node_host();
        let allocated: HashSet<String> = ["0000:02:00.3".to_string()].into();
        let vram: BTreeMap<String, u32> = [("0000:01:00.0".to_string(), 80)].into();
        let topo = discover(t.path(), &allocated, &vram);
        let bdfs: Vec<&str> = topo.devices.iter().map(|d| d.bdf.as_str()).collect();
        assert_eq!(
            bdfs,
            [
                "0000:01:00.0",
                "0000:02:00.0",
                "0000:02:00.2",
                "0000:02:00.3",
                "0000:81:00.0"
            ],
            "the SATA controller is not a placeable device"
        );
        let gpu = &topo.devices[0];
        assert_eq!(gpu.class, DeviceClass::Gpu);
        assert_eq!(gpu.pcie_root, "pci0000:00/0000:00:01.0");
        assert_eq!(
            (gpu.numa_node, gpu.iommu_group, gpu.vram_gib),
            (Some(0), Some(10), Some(80))
        );
        assert_eq!(gpu.iommu_members, ["0000:01:00.0"]);

        let pf = &topo.devices[1];
        assert_eq!(pf.class, DeviceClass::Nic);
        assert_eq!(pf.pcie_root, gpu.pcie_root, "GPU and NIC share a root port");
        let sriov = pf.sriov.as_ref().unwrap();
        assert_eq!((sriov.total_vfs, sriov.num_vfs), (8, 2));
        assert_eq!(sriov.vfs, ["0000:02:00.2", "0000:02:00.3"]);
        assert_eq!(pf.netdevs, ["ens1f0"]);
        assert_eq!(pf.link_speed_mbps, Some(100000));

        assert_eq!(topo.devices[2].physfn.as_deref(), Some("0000:02:00.0"));
        assert!(!topo.devices[2].allocated);
        assert!(topo.devices[3].allocated);

        let nvme = &topo.devices[4];
        assert_eq!(nvme.class, DeviceClass::Nvme);
        assert_eq!(nvme.numa_node, Some(1));
        assert_eq!(nvme.pcie_root, "pci0000:80/0000:80:02.0");
        assert_eq!(nvme.size_bytes, Some(2000000 * 512));
        assert_eq!(nvme.driver.as_deref(), Some("nvme"));
    }

    #[test]
    fn a_kernel_without_numa_reports_one_node() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), "sys/devices/system/cpu/online", "0-3\n");
        write(
            t.path(),
            "proc/meminfo",
            "MemTotal:  4194304 kB\nMemFree:   2097152 kB\n",
        );
        let topo = discover(t.path(), &HashSet::new(), &BTreeMap::new());
        assert_eq!(topo.numa_nodes.len(), 1);
        let n = &topo.numa_nodes[0];
        assert_eq!(n.cpus, [0, 1, 2, 3]);
        assert_eq!(n.thread_siblings.len(), 4);
        assert_eq!((n.memory_mib, n.free_memory_mib), (4096, 2048));
        assert!(topo.devices.is_empty());
    }

    #[test]
    fn offline_cpus_are_left_out() {
        let t = two_node_host();
        write(t.path(), "sys/devices/system/cpu/online", "0-2,4-7\n");
        let topo = discover(t.path(), &HashSet::new(), &BTreeMap::new());
        assert_eq!(topo.numa_nodes[0].cpus, [0, 1, 2]);
        assert_eq!(topo.numa_nodes[0].thread_siblings, [vec![0, 1], vec![2]]);
    }

    #[test]
    fn cpu_lists_round_trip() {
        assert_eq!(parse_cpu_list("0-3,8,10-11\n"), [0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(parse_cpu_list(""), Vec::<u32>::new());
        assert_eq!(parse_cpu_list("5-2,x,7"), [7]);
        assert_eq!(format_cpu_list(&[11, 0, 1, 2, 3, 8, 10]), "0-3,8,10-11");
        assert_eq!(format_cpu_list(&[]), "");
    }

    #[test]
    fn device_classes() {
        assert_eq!(classify(0x030000), Some(DeviceClass::Gpu));
        assert_eq!(classify(0x030200), Some(DeviceClass::Gpu));
        assert_eq!(classify(0x020000), Some(DeviceClass::Nic));
        assert_eq!(classify(0x010802), Some(DeviceClass::Nvme));
        assert_eq!(classify(0x120000), Some(DeviceClass::Accelerator));
        assert_eq!(classify(0x010601), None);
        assert_eq!(classify(0x060400), None);
    }
}
