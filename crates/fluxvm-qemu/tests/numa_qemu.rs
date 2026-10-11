// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Runs a real QEMU with a host-bound guest NUMA node, pins its vCPU
//! threads and reads the result back. Set FLUXVM_QEMU_NUMA_TEST=1 on a
//! Linux host with qemu-system-x86_64 (TCG is used, so /dev/kvm access is
//! not needed).

use fluxvm_core::model::VmNumaNode;
use fluxvm_qemu::numa::{self, MemoryBacking};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[tokio::test]
async fn qemu_binds_memory_and_pins_vcpus() {
    if std::env::var("FLUXVM_QEMU_NUMA_TEST").as_deref() != Ok("1") {
        eprintln!("skipped: set FLUXVM_QEMU_NUMA_TEST=1");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let qmp = dir.path().join("qmp.sock");
    let host_cpus = numa::host_cpus(
        Path::new("/sys"),
        &VmNumaNode {
            host_node: 0,
            vcpus: vec![0],
            host_cpus: vec![],
            memory_mib: 1,
        },
    )
    .unwrap();
    let want: Vec<u32> = host_cpus.iter().copied().take(1).collect();
    let plan = [VmNumaNode {
        host_node: 0,
        vcpus: vec![0, 1],
        host_cpus: want.clone(),
        memory_mib: 128,
    }];
    let mut child = Command::new("qemu-system-x86_64")
        .args([
            "-machine",
            "q35,accel=tcg",
            "-smp",
            "cpus=2,maxcpus=2",
            "-m",
            "128M",
        ])
        .args(numa::args(&plan, MemoryBacking::Ram, 2, 2))
        .args(["-nodefaults", "-display", "none", "-S"])
        .arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn qemu-system-x86_64");
    let pid = child.id();

    let result = async {
        let applied =
            numa::pin_vcpus(&qmp, &plan, Path::new("/sys"), Duration::from_secs(20)).await?;
        let status = numa::status(&qmp, pid, &plan, Duration::from_secs(10)).await?;
        anyhow::Ok((applied, status))
    }
    .await;
    let _ = child.kill();
    let _ = child.wait();
    let (applied, status) = result.unwrap();

    assert_eq!(applied.len(), 2);
    assert_eq!(status.vcpus.len(), 2);
    for v in &status.vcpus {
        assert_eq!(v.host_cpus, want, "vCPU {} affinity", v.vcpu);
    }
    let mem = status
        .memdevs
        .iter()
        .find(|m| m.id == "numa-mem0")
        .expect("numa-mem0 memdev");
    assert_eq!(mem.policy, "bind");
    assert_eq!(mem.host_nodes, [0]);
    assert_eq!(mem.size_mib, 128);
}
