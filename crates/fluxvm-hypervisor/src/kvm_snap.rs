// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! In-tree KVM snapshot format (`FLUXKVM1` / v2–v5).
//!
//! - **v2**: all vCPUs + 256-byte reserved virtio watermark (zeros).
//! - **v3**: all vCPUs + packed virtio device live-state (queue rings /
//!   status / features) so restore can reattach backends without cold
//!   re-init of queue pointers. Disk/TAP paths still come from boot config.
//! - **v4**: v3 plus one `KVM_MP_STATE` u32 per vCPU, so restored APs come
//!   back runnable (or still waiting for SIPI) exactly as they were.
//! - **v5**: v4 plus tagged, length-prefixed full-fidelity state (see
//!   [`crate::kvm_state`]): per-vCPU XSAVE/XCRS/MSRs/LAPIC/events/debug regs/TSC
//!   kHz and VM-wide clock/PIC/IOAPIC/PIT. v2-v4 restores are register-only
//!   and log a warning.

use crate::devices::virtio_mmio::{QueueState, VirtioState};
use crate::error::{FluxError, Result};
use crate::kvm::{KvmRegs, KvmSregs, KvmVm};
use crate::kvm_state::{self, VcpuState, VmState};
use crate::memory::GuestMemory;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"FLUXKVM1";
pub const VERSION: u32 = 5;
const V2_WATERMARK: usize = 256;
const MAX_SNAPSHOT_VCPUS: usize = 256;
const MAX_SNAPSHOT_DEVICES: usize = 64;
const MAX_VIRTIO_QUEUES: u32 = crate::devices::virtio_mmio::MAX_QUEUES as u32;

pub struct SnapCmd {
    pub vmstate: std::path::PathBuf,
    pub mem: std::path::PathBuf,
    pub reply: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
}

pub fn is_flux_kvm_vmstate(path: &Path) -> bool {
    let mut buf = [0u8; 8];
    match File::open(path).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => &buf == MAGIC,
        Err(_) => false,
    }
}

// ... existing write_virtio_v3 / read_virtio_v3 / dump / CpuSnapshot / load_cpu unchanged ...

pub fn load_memory_into(mem: &mut GuestMemory, mem_path: &Path) -> Result<()> {
    let file = File::open(mem_path).map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    let actual = file
        .metadata()
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?
        .len();
    if actual != mem.len() as u64 {
        return Err(FluxError::Hypervisor(format!(
            "snapshot RAM size {} does not match guest RAM {}",
            actual,
            mem.len()
        )));
    }

    // Prefer demand-paged userfaultfd restore when explicitly requested.
    if std::env::var("FLUXVM_KVM_UFFD").ok().as_deref() == Some("1") {
        match mem.setup_uffd_restore(&file) {
            Ok(()) => {
                eprintln!("[kvm-engine] restore using userfaultfd demand paging");
                return Ok(());
            }
            Err(e) => {
                eprintln!("[kvm-engine] uffd restore unavailable ({e}), falling back");
            }
        }
    }

    // Preferred: map the snapshot privately so children share clean pages and
    // only copy what they write. Any failure falls back to the eager copy.
    match mem.remap_private_file(&file) {
        Ok(()) => return Ok(()),
        Err(e) => eprintln!("[kvm-engine] file-backed restore unavailable, copying RAM: {e}"),
    }
    let mut reader = std::io::BufReader::new(file);
    reader
        .read_exact(mem.as_slice_mut())
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    Ok(())
}

// ... rest of file (restore_vcpus, restore_virtio, tests) unchanged ...
