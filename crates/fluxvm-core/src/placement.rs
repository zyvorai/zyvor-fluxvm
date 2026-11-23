// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! NUMA, huge-page and passthrough placement advice for one VM on one host, for `fluxctl place advise`.
//!
//! Input is the host graph the daemon already builds (`fluxvm_core::topology::HostTopology`, saved from
//! `GET /v1/host/topology` or written as a fixture) and the VM spec. The output is advice, never an action: which
//! host NUMA node can hold the VM (memory, vCPUs, huge pages, the NUMA node of each passthrough device), a CPU set
//! made of whole cores, and what to fix when nothing fits. It reads two documents and writes text; it does not
//! touch sysfs, `/dev/kvm` or a hypervisor.
//!
//! What it cannot know: which CPUs and how much memory other VMs already pin (the graph reports node free memory
//! and free huge pages, not CPU use), so a suggested `cpuset` is "these cores of that node", to be checked against
//! the VMs already running. Guidance comes from the kernel hugetlb documentation and the QEMU/KubeVirt/Proxmox
//! NUMA guides summarised in `docs/RESEARCH.md`.

use crate::config_lint::{Finding, Severity};
use crate::host_compat::VmNeeds;
use crate::topology::{HostTopology, NumaNodeInfo, format_cpu_list};
use serde::Serialize;
use std::collections::BTreeSet;

pub const MAX_NODES: usize = 1024;
pub const MAX_DEVICES: usize = 8192;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NodeFit {
    pub node: u32,
    pub fits: bool,
    pub cpus: usize,
    pub free_memory_mib: u64,
    pub free_hugepage_mib: u64,
    /// Why the node does not fit (empty when it does).
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq, Eq)]
pub struct Suggested {
    /// `numa_node` of the VM spec (QEMU only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    /// `cpuset` of the VM spec: whole cores of that node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpuset: Option<String>,
    /// Whether to set `hugepages`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hugepages: Option<bool>,
    /// Nodes to span when no single node holds the VM, nearest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub span_nodes: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlacementAdvice {
    pub recommended_node: Option<u32>,
    pub nodes: Vec<NodeFit>,
    pub suggested: Suggested,
    pub findings: Vec<Finding>,
}

fn f(sev: Severity, key: &str, message: String, fix: Option<String>) -> Finding {
    Finding {
        severity: sev,
        key: key.to_string(),
        message,
        fix,
    }
}

fn free_hugepage_mib(n: &NumaNodeInfo) -> u64 {
    n.hugepages
        .iter()
        .map(|p| p.free.saturating_mul(p.size_kib) / 1024)
        .fold(0u64, u64::saturating_add)
}

fn clip(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Whole cores first: thread-sibling groups are taken together so the VM never shares a core with another tenant.
/// An odd vCPU count takes one extra sibling and leaves it unused by this VM, which the advice says.
fn pick_cpus(node: &NumaNodeInfo, vcpus: usize) -> Option<(Vec<u32>, bool)> {
    if vcpus == 0 || vcpus > node.cpus.len() {
        return None;
    }
    let mut picked: Vec<u32> = Vec::new();
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut groups: Vec<Vec<u32>> = node
        .thread_siblings
        .iter()
        .map(|g| {
            g.iter()
                .copied()
                .filter(|c| node.cpus.contains(c))
                .collect::<Vec<_>>()
        })
        .filter(|g| !g.is_empty())
        .collect();
    for g in &groups {
        seen.extend(g.iter().copied());
    }
    groups.extend(
        node.cpus
            .iter()
            .filter(|c| !seen.contains(c))
            .map(|c| vec![*c]),
    );
    groups.sort_by_key(|g| g[0]);
    let mut split_core = false;
    for g in groups {
        if picked.len() >= vcpus {
            break;
        }
        let take = (vcpus - picked.len()).min(g.len());
        if take < g.len() {
            split_core = true;
        }
        picked.extend(g.into_iter().take(take));
    }
    picked.sort_unstable();
    Some((picked, split_core))
}

/// Computes the advice. `topo` is untrusted: node and device counts are capped.
pub fn advise(topo: &HostTopology, vm: &VmNeeds) -> PlacementAdvice {
    let mut findings: Vec<Finding> = Vec::new();
    let mut nodes: Vec<&NumaNodeInfo> = topo.numa_nodes.iter().take(MAX_NODES).collect();
    nodes.sort_by_key(|n| n.id);
    nodes.dedup_by_key(|n| n.id);
    if topo.numa_nodes.len() > MAX_NODES || topo.devices.len() > MAX_DEVICES {
        findings.push(f(
            Severity::Warning,
            "topology",
            format!("the host graph is larger than the limits ({MAX_NODES} nodes, {MAX_DEVICES} devices); the rest is ignored"),
            None,
        ));
    }
    if nodes.is_empty() {
        findings.push(f(
            Severity::Info,
            "topology.numaNodes",
            "the host description has no NUMA nodes; no placement advice".into(),
            None,
        ));
        return PlacementAdvice {
            recommended_node: None,
            nodes: vec![],
            suggested: Suggested::default(),
            findings,
        };
    }

    // Passthrough devices pin the VM to the node they hang off.
    let mut device_nodes: BTreeSet<u32> = BTreeSet::new();
    let mut spec_devices: BTreeSet<String> = vm
        .vfio_devices
        .iter()
        .map(|d| d.to_ascii_lowercase())
        .collect();
    for d in vm.vfio_devices.iter().take(64) {
        let key = d.to_ascii_lowercase();
        let Some(dev) = topo
            .devices
            .iter()
            .take(MAX_DEVICES)
            .find(|x| x.bdf.eq_ignore_ascii_case(&key))
        else {
            findings.push(f(
                Severity::Warning,
                "vfio_devices",
                format!("`{}` is not in the host description, so its NUMA node and IOMMU group were not checked", clip(d)),
                None,
            ));
            continue;
        };
        if dev.allocated {
            findings.push(f(
                Severity::Error,
                "vfio_devices",
                format!(
                    "`{}` (or a function in its IOMMU group) is already assigned to a VM",
                    clip(d)
                ),
                Some("choose another device".into()),
            ));
        }
        match dev.numa_node {
            Some(n) => {
                device_nodes.insert(n);
            }
            None => findings.push(f(
                Severity::Info,
                "vfio_devices",
                format!("`{}` reports no NUMA node (single-node host or firmware gap); locality was not considered", clip(d)),
                None,
            )),
        }
        let others: Vec<&str> = dev
            .iommu_members
            .iter()
            .filter(|m| {
                !m.eq_ignore_ascii_case(&dev.bdf) && !spec_devices.contains(&m.to_ascii_lowercase())
            })
            .map(String::as_str)
            .take(8)
            .collect();
        if !others.is_empty() {
            findings.push(f(
                Severity::Warning,
                "vfio_devices",
                format!(
                    "`{}` shares IOMMU group {} with {}; a VFIO group is assigned as a whole, so those functions (bridges excepted) must be bound to vfio-pci too and cannot stay with the host",
                    clip(d),
                    dev.iommu_group.map_or_else(|| "?".to_string(), |g| g.to_string()),
                    others.iter().map(|o| clip(o)).collect::<Vec<_>>().join(", ")
                ),
                Some("add the group members to vfio_devices or pick a device alone in its group".into()),
            ));
            spec_devices.extend(others.iter().map(|o| o.to_ascii_lowercase()));
        }
    }
    if device_nodes.len() > 1 {
        findings.push(f(
            Severity::Warning,
            "vfio_devices",
            format!(
                "the passthrough devices sit on different NUMA nodes ({}); some DMA will cross the interconnect",
                device_nodes.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            ),
            Some("choose devices on one node".into()),
        ));
    }

    // Per-node fit.
    let vcpus = vm.vcpus as usize;
    let fits: Vec<NodeFit> = nodes
        .iter()
        .map(|n| {
            let hp = free_hugepage_mib(n);
            let mut reasons = Vec::new();
            if n.cpus.len() < vcpus {
                reasons.push(format!("{} CPU(s), the VM has {vcpus} vCPUs", n.cpus.len()));
            }
            if n.free_memory_mib < vm.memory_mib {
                reasons.push(format!(
                    "{} MiB free, the VM needs {}",
                    n.free_memory_mib, vm.memory_mib
                ));
            }
            if vm.hugepages && hp < vm.memory_mib {
                reasons.push(format!(
                    "{hp} MiB of free huge pages, the VM needs {}",
                    vm.memory_mib
                ));
            }
            if !device_nodes.is_empty() && !device_nodes.contains(&n.id) {
                reasons.push("a passthrough device sits on another node".into());
            }
            NodeFit {
                node: n.id,
                fits: reasons.is_empty(),
                cpus: n.cpus.len(),
                free_memory_mib: n.free_memory_mib,
                free_hugepage_mib: hp,
                reasons,
            }
        })
        .collect();

    // Most free memory first (spreads load), lowest id breaks ties.
    let mut ranking: Vec<&NodeFit> = fits.iter().filter(|x| x.fits).collect();
    ranking.sort_by(|a, b| {
        b.free_memory_mib
            .cmp(&a.free_memory_mib)
            .then(a.node.cmp(&b.node))
    });
    let mut suggested = Suggested::default();
    let recommended = ranking.first().map(|x| x.node);

    // An explicit request in the spec is checked against the same table.
    let requested: Vec<u32> = vm
        .numa_node
        .into_iter()
        .chain(vm.numa_host_nodes.iter().copied())
        .collect();
    for r in &requested {
        match fits.iter().find(|x| x.node == *r) {
            None => findings.push(f(
                Severity::Error,
                "numa_node",
                format!("the VM is bound to NUMA node {r}, which the host does not have"),
                Some(format!(
                    "use one of: {}",
                    fits.iter()
                        .map(|x| x.node.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            )),
            Some(x) if !x.fits && vm.numa_host_nodes.is_empty() => findings.push(f(
                Severity::Error,
                "numa_node",
                format!(
                    "the VM is bound to node {r}, which does not fit: {}",
                    x.reasons.join("; ")
                ),
                recommended.map(|n| format!("bind it to node {n} instead")),
            )),
            _ => {}
        }
    }

    if let Some(n) = recommended {
        let node = nodes
            .iter()
            .find(|x| x.id == n)
            .copied()
            .expect("recommended node exists");
        suggested.numa_node = Some(n);
        if let Some((cpus, split)) = pick_cpus(node, vcpus) {
            suggested.cpuset = Some(format_cpu_list(&cpus));
            if split {
                findings.push(f(
                    Severity::Info,
                    "cpuset",
                    format!(
                        "{vcpus} vCPUs is an odd number of hardware threads: the last core is shared with a thread the VM does not use; leave it idle or give the VM an even count"
                    ),
                    None,
                ));
            }
        }
        findings.push(f(
            Severity::Info,
            "numa_node",
            format!(
                "node {n} holds the VM: {} MiB free of {} needed, {} CPUs{}",
                node.free_memory_mib,
                vm.memory_mib,
                node.cpus.len(),
                if device_nodes.contains(&n) {
                    ", and it is local to the passthrough device"
                } else {
                    ""
                }
            ),
            None,
        ));
        findings.push(f(
            Severity::Info,
            "cpuset",
            "the host graph does not say which CPUs other VMs pin; check the suggested set against running VMs".into(),
            None,
        ));
        let hp = free_hugepage_mib(node);
        if vm.hugepages {
            suggested.hugepages = Some(true);
        } else if hp >= vm.memory_mib {
            suggested.hugepages = Some(true);
            findings.push(f(
                Severity::Info,
                "hugepages",
                format!("node {n} has {hp} MiB of free huge pages; `hugepages: true` would back the whole VM and cut TLB misses (pinned memory cannot be swapped or ballooned)"),
                None,
            ));
        }
    } else {
        // Nothing fits on one node: suggest spanning the nearest nodes, if the device allows it.
        let anchor = device_nodes
            .iter()
            .next()
            .copied()
            .or_else(|| nodes.iter().max_by_key(|n| n.free_memory_mib).map(|n| n.id));
        if let Some(a) = anchor {
            let an = nodes
                .iter()
                .find(|n| n.id == a)
                .copied()
                .expect("anchor exists");
            let mut order: Vec<&NumaNodeInfo> = nodes.clone();
            order.sort_by_key(|n| {
                let d = an.distances.get(n.id as usize).copied().unwrap_or(u32::MAX);
                (u32::from(n.id != a), d, n.id)
            });
            let (mut mem, mut cpus, mut hp, mut span) = (0u64, 0usize, 0u64, Vec::new());
            for n in order {
                span.push(n.id);
                mem = mem.saturating_add(n.free_memory_mib);
                cpus += n.cpus.len();
                hp = hp.saturating_add(free_hugepage_mib(n));
                if mem >= vm.memory_mib && cpus >= vcpus && (!vm.hugepages || hp >= vm.memory_mib) {
                    break;
                }
            }
            let enough =
                mem >= vm.memory_mib && cpus >= vcpus && (!vm.hugepages || hp >= vm.memory_mib);
            if enough && span.len() > 1 {
                let worst = span
                    .iter()
                    .filter_map(|n| an.distances.get(*n as usize).copied())
                    .max();
                findings.push(f(
                    Severity::Warning,
                    "numa",
                    format!(
                        "no single node holds the VM; spanning nodes {} would{}",
                        span.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
                        worst.map_or(String::new(), |d| format!(" (farthest SLIT distance {d}; local is usually 10)"))
                    ),
                    Some("describe the split with the `numa` list so the guest sees the real topology, or reduce the VM".into()),
                ));
                suggested.span_nodes = span;
            } else {
                let short = if mem < vm.memory_mib {
                    format!(
                        "the whole host has {mem} MiB free in reachable nodes, the VM needs {}",
                        vm.memory_mib
                    )
                } else if vm.hugepages && hp < vm.memory_mib {
                    format!(
                        "only {hp} MiB of huge pages are free host-wide, the VM needs {}",
                        vm.memory_mib
                    )
                } else {
                    format!("the host has {cpus} CPUs, the VM has {vcpus} vCPUs")
                };
                findings.push(f(
                    Severity::Error,
                    "numa",
                    format!("the VM does not fit on this host: {short}"),
                    Some(if vm.hugepages {
                        "reserve more huge pages (best at boot, `hugepages=N`), shrink the VM, or choose another host".into()
                    } else {
                        "shrink the VM or choose another host".into()
                    }),
                ));
            }
        }
    }

    // Alignment of the memory size with the pool page size.
    if vm.hugepages {
        let sizes: BTreeSet<u64> = nodes
            .iter()
            .flat_map(|n| n.hugepages.iter())
            .map(|p| p.size_kib)
            .collect();
        if sizes.is_empty() {
            findings.push(f(
                Severity::Error,
                "hugepages",
                "the VM asks for huge pages but the host has no huge page pool".into(),
                Some(
                    "reserve pages: vm.nr_hugepages, or `hugepages=N` on the kernel command line"
                        .into(),
                ),
            ));
        } else if sizes
            .iter()
            .all(|s| !(vm.memory_mib * 1024).is_multiple_of(*s))
        {
            findings.push(f(
                Severity::Warning,
                "memory_mib",
                format!(
                    "{} MiB is not a multiple of any huge page size on this host ({} KiB); the remainder cannot be backed",
                    vm.memory_mib,
                    sizes.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
                ),
                Some("round memory_mib up to a whole number of pages".into()),
            ));
        }
    }
    findings.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.key.cmp(&b.key)));
    PlacementAdvice {
        recommended_node: recommended,
        nodes: fits,
        suggested,
        findings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::BackendKind;
    use crate::topology::{DeviceClass, HugepagePool, PciDeviceInfo};

    fn node(id: u32, cpus: std::ops::Range<u32>, free: u64, hp_free: u64) -> NumaNodeInfo {
        let cpus: Vec<u32> = cpus.collect();
        NumaNodeInfo {
            id,
            thread_siblings: cpus.chunks(2).map(<[u32]>::to_vec).collect(),
            cpus,
            memory_mib: 65536,
            free_memory_mib: free,
            hugepages: vec![HugepagePool {
                size_kib: 2048,
                total: hp_free,
                free: hp_free,
            }],
            distances: if id == 0 { vec![10, 21] } else { vec![21, 10] },
        }
    }
    fn gpu(bdf: &str, node: u32) -> PciDeviceInfo {
        PciDeviceInfo {
            bdf: bdf.into(),
            class: DeviceClass::Gpu,
            class_id: 0x030000,
            vendor_id: 0x10de,
            device_id: 1,
            driver: None,
            numa_node: Some(node),
            pcie_root: "pci0000:00".into(),
            iommu_group: Some(7),
            iommu_members: vec![bdf.into(), "0000:3b:00.1".into()],
            sriov: None,
            physfn: None,
            netdevs: vec![],
            link_speed_mbps: None,
            size_bytes: None,
            vram_gib: None,
            allocated: false,
        }
    }
    fn host() -> HostTopology {
        HostTopology {
            numa_nodes: vec![node(0, 0..8, 20000, 0), node(1, 8..16, 30000, 8192)],
            devices: vec![gpu("0000:3b:00.0", 1)],
        }
    }
    fn vm(vcpus: u32, mem: u64) -> VmNeeds {
        VmNeeds {
            backend: BackendKind::Qemu,
            vcpus,
            memory_mib: mem,
            cpu_template: None,
            hugepages: false,
            numa_node: None,
            numa_host_nodes: vec![],
            vfio_devices: vec![],
            cpuset: None,
        }
    }
    fn errors(a: &PlacementAdvice) -> Vec<&str> {
        a.findings
            .iter()
            .filter(|x| x.severity == Severity::Error)
            .map(|x| x.key.as_str())
            .collect()
    }

    #[test]
    fn picks_the_node_with_most_free_memory_and_whole_cores() {
        let a = advise(&host(), &vm(4, 8192));
        assert_eq!(a.recommended_node, Some(1));
        assert_eq!(a.suggested.numa_node, Some(1));
        assert_eq!(a.suggested.cpuset.as_deref(), Some("8-11"));
        assert!(errors(&a).is_empty());
        // node 1 has 16 GiB of free huge pages, so they are offered
        assert_eq!(a.suggested.hugepages, Some(true));
    }

    #[test]
    fn odd_vcpus_split_a_core_and_say_so() {
        let a = advise(&host(), &vm(3, 1024));
        assert_eq!(a.suggested.cpuset.as_deref(), Some("8-10"));
        assert!(
            a.findings
                .iter()
                .any(|x| x.key == "cpuset" && x.message.contains("odd"))
        );
    }

    #[test]
    fn passthrough_device_pins_the_node() {
        let mut v = vm(2, 1024);
        v.vfio_devices = vec!["0000:3B:00.0".into()];
        let a = advise(&host(), &v);
        assert_eq!(a.recommended_node, Some(1));
        let n0 = a.nodes.iter().find(|n| n.node == 0).unwrap();
        assert!(!n0.fits && n0.reasons[0].contains("passthrough"));
        // the second function of the IOMMU group is flagged
        assert!(
            a.findings
                .iter()
                .any(|x| x.message.contains("IOMMU group 7"))
        );
    }

    #[test]
    fn allocated_device_is_an_error() {
        let mut t = host();
        t.devices[0].allocated = true;
        let mut v = vm(2, 1024);
        v.vfio_devices = vec!["0000:3b:00.0".into()];
        assert_eq!(errors(&advise(&t, &v)), vec!["vfio_devices"]);
    }

    #[test]
    fn spans_nearest_nodes_when_nothing_fits_alone() {
        let a = advise(&host(), &vm(4, 40000));
        assert_eq!(a.recommended_node, None);
        assert_eq!(a.suggested.span_nodes, vec![1, 0]);
        assert!(
            a.findings
                .iter()
                .any(|x| x.key == "numa" && x.severity == Severity::Warning)
        );
        assert!(errors(&a).is_empty());
    }

    #[test]
    fn too_big_for_the_host_is_an_error() {
        let a = advise(&host(), &vm(4, 900_000));
        assert_eq!(errors(&a), vec!["numa"]);
        let a = advise(&host(), &vm(64, 1024));
        assert_eq!(errors(&a), vec!["numa"]);
    }

    #[test]
    fn explicit_bad_node_and_hugepage_shortfall() {
        let mut v = vm(2, 8192);
        v.numa_node = Some(0);
        v.hugepages = true;
        let a = advise(&host(), &v);
        // node 0 has no huge pages; node 1 is recommended
        assert_eq!(a.recommended_node, Some(1));
        assert!(errors(&a).contains(&"numa_node"));
        v.numa_node = Some(9);
        assert!(errors(&advise(&host(), &v)).contains(&"numa_node"));
    }

    #[test]
    fn hugepage_alignment_and_missing_pool() {
        let mut v = vm(2, 1025);
        v.hugepages = true;
        let a = advise(&host(), &v);
        assert!(a.findings.iter().any(|x| x.key == "memory_mib"));
        let mut t = host();
        for n in &mut t.numa_nodes {
            n.hugepages.clear();
        }
        assert!(errors(&advise(&t, &v)).contains(&"hugepages"));
    }

    #[test]
    fn empty_and_oversized_graphs() {
        let a = advise(&HostTopology::default(), &vm(1, 1));
        assert!(a.recommended_node.is_none() && errors(&a).is_empty());
        let t = HostTopology {
            numa_nodes: (0..(MAX_NODES as u32 + 50))
                .map(|i| node(i, 0..2, 10, 0))
                .collect(),
            devices: vec![],
        };
        let a = advise(&t, &vm(1, 1));
        assert!(a.nodes.len() <= MAX_NODES);
        assert!(a.findings.iter().any(|x| x.key == "topology"));
    }

    #[test]
    fn control_characters_in_device_names_are_not_echoed() {
        let mut v = vm(1, 1);
        v.vfio_devices = vec!["\u{1b}[2Jevil".into()];
        let a = advise(&host(), &v);
        assert!(a.findings.iter().all(|x| !x.message.contains('\u{1b}')));
    }
}
