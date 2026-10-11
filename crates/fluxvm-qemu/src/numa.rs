// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Guest NUMA layout bound to host NUMA nodes: one memory backend per
//! guest node with `host-nodes=H,policy=bind`, and vCPU threads pinned to
//! the node's host CPUs once QEMU is up.

use crate::qmp;
use anyhow::{Context, Result, bail};
use fluxvm_core::model::{CreateVmRequest, MemdevBinding, VcpuAffinity, VmNumaNode, VmNumaStatus};
use fluxvm_core::topology::{format_cpu_list, parse_cpu_list};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// How guest RAM is backed; picks the `memory-backend-*` type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBacking {
    Ram,
    Hugepages,
    /// memfd with `share=on`, required by virtiofs and `shared_memory`.
    Shared,
}

/// The bound guest nodes for `req`: `req.numa` as given, or a single node
/// holding the whole VM when only the legacy `numa_node` is set. Empty
/// when the VM has no host NUMA binding.
pub fn plan(req: &CreateVmRequest) -> Vec<VmNumaNode> {
    if !req.numa.is_empty() {
        return req.numa.clone();
    }
    match (req.numa_node, &req.cpuset) {
        (Some(node), None) => vec![VmNumaNode {
            host_node: u32::from(node),
            vcpus: (0..u32::from(req.vcpus)).collect(),
            host_cpus: Vec::new(),
            memory_mib: req.memory_mib,
        }],
        _ => Vec::new(),
    }
}

/// `-object memory-backend-*` and `-numa node` args for `plan`. vCPU slots
/// above `vcpus` (hotplug headroom up to `max_vcpus`) go on the last node
/// so every possible CPU has a node.
pub fn args(plan: &[VmNumaNode], backing: MemoryBacking, vcpus: u8, max_vcpus: u8) -> Vec<String> {
    let mut a = Vec::new();
    for (i, node) in plan.iter().enumerate() {
        let bind = format!("host-nodes={},policy=bind", node.host_node);
        let size = node.memory_mib;
        let object = match backing {
            MemoryBacking::Ram => format!("memory-backend-ram,id=numa-mem{i},size={size}M,{bind}"),
            MemoryBacking::Hugepages => format!(
                "memory-backend-file,id=numa-mem{i},size={size}M,mem-path=/dev/hugepages,share=on,prealloc=on,{bind}"
            ),
            MemoryBacking::Shared => {
                format!("memory-backend-memfd,id=numa-mem{i},size={size}M,share=on,{bind}")
            }
        };
        let mut cpus = node.vcpus.clone();
        if i + 1 == plan.len() {
            cpus.extend(u32::from(vcpus)..u32::from(max_vcpus));
        }
        cpus.sort_unstable();
        let mut numa = format!("node,nodeid={i},memdev=numa-mem{i}");
        for range in format_cpu_list(&cpus).split(',').filter(|r| !r.is_empty()) {
            numa.push_str(",cpus=");
            numa.push_str(range);
        }
        a.extend(["-object".into(), object, "-numa".into(), numa]);
    }
    a
}

/// Host CPUs for `node`: its explicit `host_cpus`, else every CPU of
/// `host_node` from sysfs under `sys_root`.
pub fn host_cpus(sys_root: &Path, node: &VmNumaNode) -> Result<Vec<u32>> {
    if !node.host_cpus.is_empty() {
        return Ok(node.host_cpus.clone());
    }
    let path = sys_root.join(format!(
        "devices/system/node/node{}/cpulist",
        node.host_node
    ));
    let list = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        // Kernels without CONFIG_NUMA have no node directory; node 0 is
        // then every online CPU.
        Err(_) if node.host_node == 0 => {
            std::fs::read_to_string(sys_root.join("devices/system/cpu/online"))
                .with_context(|| format!("read {}", path.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let cpus = parse_cpu_list(list.trim());
    if cpus.is_empty() {
        bail!("host NUMA node {} has no CPUs", node.host_node);
    }
    Ok(cpus)
}

/// Guest node index holding `vcpu`; hotplugged vCPUs sit on the last node.
fn node_of(plan: &[VmNumaNode], vcpu: u32) -> Option<usize> {
    plan.iter()
        .position(|n| n.vcpus.contains(&vcpu))
        .or_else(|| plan.len().checked_sub(1))
}

/// `(cpu-index, thread-id)` for each vCPU from `query-cpus-fast`.
async fn vcpu_threads(socket: &Path, timeout: Duration) -> Result<Vec<(u32, i32)>> {
    let value = qmp::execute(socket, "query-cpus-fast", None, timeout).await?;
    let entries = value
        .as_array()
        .context("query-cpus-fast did not return an array")?;
    entries
        .iter()
        .map(|e| {
            let index = e.get("cpu-index").and_then(Value::as_u64);
            let tid = e.get("thread-id").and_then(Value::as_i64);
            match (index, tid) {
                (Some(i), Some(t)) => Ok((i as u32, t as i32)),
                _ => bail!("query-cpus-fast entry without cpu-index/thread-id: {e}"),
            }
        })
        .collect()
}

/// Pins every vCPU thread of the QEMU behind `socket` to its guest node's
/// host CPUs. Returns what was applied.
pub async fn pin_vcpus(
    socket: &Path,
    plan: &[VmNumaNode],
    sys_root: &Path,
    timeout: Duration,
) -> Result<Vec<VcpuAffinity>> {
    if plan.is_empty() {
        return Ok(Vec::new());
    }
    let cpus = plan
        .iter()
        .map(|n| host_cpus(sys_root, n))
        .collect::<Result<Vec<_>>>()?;
    let mut applied = Vec::new();
    for (vcpu, tid) in vcpu_threads(socket, timeout).await? {
        let Some(i) = node_of(plan, vcpu) else {
            continue;
        };
        set_affinity(tid, &cpus[i]).with_context(|| {
            format!(
                "pin vCPU {vcpu} (thread {tid}) to {}",
                format_cpu_list(&cpus[i])
            )
        })?;
        applied.push(VcpuAffinity {
            vcpu,
            thread_id: tid,
            host_node: plan[i].host_node,
            host_cpus: cpus[i].clone(),
        });
    }
    Ok(applied)
}

/// Live placement: each vCPU thread's affinity as the kernel reports it
/// and each memory backend's binding as QEMU reports it.
pub async fn status(
    socket: &Path,
    pid: u32,
    plan: &[VmNumaNode],
    timeout: Duration,
) -> Result<VmNumaStatus> {
    let mut vcpus = Vec::new();
    for (vcpu, tid) in vcpu_threads(socket, timeout).await? {
        let path = format!("/proc/{pid}/task/{tid}/status");
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
        vcpus.push(VcpuAffinity {
            vcpu,
            thread_id: tid,
            host_node: node_of(plan, vcpu).map_or(0, |i| plan[i].host_node),
            host_cpus: cpus_allowed(&text),
        });
    }
    let memdevs = qmp::execute(socket, "query-memdev", None, timeout).await?;
    Ok(VmNumaStatus {
        nodes: plan.to_vec(),
        vcpus,
        memdevs: parse_memdevs(&memdevs),
    })
}

fn cpus_allowed(proc_status: &str) -> Vec<u32> {
    proc_status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        .map(|v| parse_cpu_list(v.trim()))
        .unwrap_or_default()
}

fn parse_memdevs(value: &Value) -> Vec<MemdevBinding> {
    value
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|m| MemdevBinding {
            id: m
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            size_mib: m.get("size").and_then(Value::as_u64).unwrap_or(0) / (1024 * 1024),
            host_nodes: m
                .get("host-nodes")
                .and_then(Value::as_array)
                .map(|n| {
                    n.iter()
                        .filter_map(Value::as_u64)
                        .map(|n| n as u32)
                        .collect()
                })
                .unwrap_or_default(),
            policy: m
                .get("policy")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn set_affinity(tid: i32, cpus: &[u32]) -> Result<()> {
    // SAFETY: cpu_set_t is plain data; CPU_ZERO/CPU_SET only write within it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &cpu in cpus {
        if cpu as usize >= libc::CPU_SETSIZE as usize {
            bail!("host CPU {cpu} is beyond CPU_SETSIZE");
        }
        unsafe { libc::CPU_SET(cpu as usize, &mut set) };
    }
    // SAFETY: `set` is a valid cpu_set_t of the size passed.
    let rc = unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_affinity(_tid: i32, _cpus: &[u32]) -> Result<()> {
    bail!("vCPU pinning requires Linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(host: u32, vcpus: &[u32], mem: u64) -> VmNumaNode {
        VmNumaNode {
            host_node: host,
            vcpus: vcpus.to_vec(),
            host_cpus: Vec::new(),
            memory_mib: mem,
        }
    }

    #[test]
    fn two_nodes_bind_memory_and_put_hotplug_slots_last() {
        let plan = [node(0, &[0, 1], 1024), node(1, &[2, 3], 1024)];
        let a = args(&plan, MemoryBacking::Ram, 4, 8);
        assert_eq!(
            a,
            [
                "-object",
                "memory-backend-ram,id=numa-mem0,size=1024M,host-nodes=0,policy=bind",
                "-numa",
                "node,nodeid=0,memdev=numa-mem0,cpus=0-1",
                "-object",
                "memory-backend-ram,id=numa-mem1,size=1024M,host-nodes=1,policy=bind",
                "-numa",
                "node,nodeid=1,memdev=numa-mem1,cpus=2-7",
            ]
        );
    }

    #[test]
    fn interleaved_vcpus_emit_one_cpus_per_range() {
        let plan = [node(1, &[0, 2], 512), node(0, &[1, 3], 512)];
        let a = args(&plan, MemoryBacking::Hugepages, 4, 4);
        assert_eq!(
            a[1],
            "memory-backend-file,id=numa-mem0,size=512M,mem-path=/dev/hugepages,share=on,prealloc=on,host-nodes=1,policy=bind"
        );
        assert_eq!(a[3], "node,nodeid=0,memdev=numa-mem0,cpus=0,cpus=2");
        assert_eq!(a[7], "node,nodeid=1,memdev=numa-mem1,cpus=1,cpus=3");
        let shared = args(&plan[..1], MemoryBacking::Shared, 2, 2);
        assert!(
            shared[1]
                .starts_with("memory-backend-memfd,id=numa-mem0,size=512M,share=on,host-nodes=1")
        );
    }

    #[test]
    fn host_cpus_come_from_sysfs_unless_given() {
        let dir = tempfile::tempdir().unwrap();
        let n1 = dir.path().join("devices/system/node/node1");
        std::fs::create_dir_all(&n1).unwrap();
        std::fs::write(n1.join("cpulist"), "8-11,20\n").unwrap();
        assert_eq!(
            host_cpus(dir.path(), &node(1, &[0], 1)).unwrap(),
            [8, 9, 10, 11, 20]
        );
        let mut given = node(1, &[0], 1);
        given.host_cpus = vec![9];
        assert_eq!(host_cpus(dir.path(), &given).unwrap(), [9]);
        assert!(host_cpus(dir.path(), &node(2, &[0], 1)).is_err());
        std::fs::create_dir_all(dir.path().join("devices/system/cpu")).unwrap();
        std::fs::write(dir.path().join("devices/system/cpu/online"), "0-3\n").unwrap();
        assert_eq!(
            host_cpus(dir.path(), &node(0, &[0], 1)).unwrap(),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn hotplugged_vcpus_belong_to_the_last_node() {
        let plan = [node(0, &[0], 1), node(1, &[1], 1)];
        assert_eq!(node_of(&plan, 0), Some(0));
        assert_eq!(node_of(&plan, 5), Some(1));
        assert_eq!(node_of(&[], 0), None);
    }

    #[test]
    fn parses_proc_status_and_memdevs() {
        assert_eq!(
            cpus_allowed("Name:\tCPU 0/KVM\nCpus_allowed_list:\t0-2,7\n"),
            [0, 1, 2, 7]
        );
        let m = serde_json::json!([{
            "id": "numa-mem0", "size": 1073741824u64, "host-nodes": [1],
            "policy": "bind", "merge": true, "dump": true, "prealloc": false
        }]);
        assert_eq!(
            parse_memdevs(&m),
            [MemdevBinding {
                id: "numa-mem0".into(),
                size_mib: 1024,
                host_nodes: vec![1],
                policy: "bind".into(),
            }]
        );
    }
}
