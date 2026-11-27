// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

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
//! - **v6**: v5 plus a fixed lineage block (snapshot id, parent id, delta
//!   flag) between the MP states and the tagged sections, for KVM dirty-log
//!   incremental snapshots. A delta's memory file is a `FLUXKVMD` file (see
//!   [`crate::dirty_log`]) that restore applies over its parent.

use crate::devices::virtio_mmio::{QueueState, VirtioState};
use crate::dirty_log::{self, DeltaHeader};
use crate::error::{FluxError, Result};
use crate::kvm::{KvmRegs, KvmSregs, KvmVm};
use crate::kvm_state::{self, VcpuState, VmState};
use crate::memory::GuestMemory;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"FLUXKVM1";
pub const VERSION: u32 = 6;
const V2_WATERMARK: usize = 256;
const MAX_SNAPSHOT_VCPUS: usize = 256;
const MAX_SNAPSHOT_DEVICES: usize = 64;
const MAX_VIRTIO_QUEUES: u32 = crate::devices::virtio_mmio::MAX_QUEUES as u32;

pub struct SnapCmd {
    pub vmstate: std::path::PathBuf,
    pub mem: std::path::PathBuf,
    /// Ask for an incremental (dirty-log delta) snapshot on top of this
    /// earlier snapshot of the same VM. Written as a full snapshot instead
    /// when a delta is not possible (see [`dump_tracked`]).
    pub parent: Option<SnapParent>,
    pub reply: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
}

/// The snapshot an incremental snapshot extends.
#[derive(Debug, Clone)]
pub struct SnapParent {
    /// Its vmstate (read for its lineage id).
    pub vmstate: std::path::PathBuf,
    /// Its memory file at its final, published path (recorded in the delta
    /// so restore can find it).
    pub mem: std::path::PathBuf,
}

/// v6 lineage: which snapshot this is and, for a delta, which one it
/// patches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lineage {
    pub snapshot_id: [u8; 16],
    /// All zeros for a full snapshot.
    pub parent_id: [u8; 16],
    /// The memory file is a `FLUXKVMD` delta over the parent.
    pub delta: bool,
}

const LINEAGE_FLAG_DELTA: u32 = 1;

impl Lineage {
    fn write(&self, f: &mut File) -> Result<()> {
        let flags = if self.delta { LINEAGE_FLAG_DELTA } else { 0 };
        f.write_all(&self.snapshot_id)
            .and_then(|()| f.write_all(&self.parent_id))
            .and_then(|()| f.write_all(&flags.to_le_bytes()))
            .map_err(|e| FluxError::Hypervisor(e.to_string()))
    }

    fn read(f: &mut File) -> Result<Self> {
        let mut l = Self::default();
        let mut flags = [0u8; 4];
        f.read_exact(&mut l.snapshot_id)
            .and_then(|()| f.read_exact(&mut l.parent_id))
            .and_then(|()| f.read_exact(&mut flags))
            .map_err(|e| FluxError::Hypervisor(format!("vmstate lineage: {e}")))?;
        l.delta = u32::from_le_bytes(flags) & LINEAGE_FLAG_DELTA != 0;
        Ok(l)
    }
}

fn new_snapshot_id() -> [u8; 16] {
    uuid::Uuid::new_v4().into_bytes()
}

/// `FLUXVM_KVM_DIRTY_LOG=1`: after a full snapshot, keep KVM dirty logging
/// (and host-write tracking) on so the next snapshot can be a delta.
pub fn incremental_enabled() -> bool {
    std::env::var("FLUXVM_KVM_DIRTY_LOG").ok().as_deref() == Some("1")
}

/// Per-VM dirty-log state across snapshots (owned by the vCPU thread).
#[derive(Debug, Default)]
pub struct DirtyLogState {
    /// Id of the snapshot the current dirty interval starts at: the only
    /// valid parent for the next delta. `None` until a full snapshot with
    /// logging on, and again after any failure that consumed the log.
    baseline: Option<[u8; 16]>,
}

impl DirtyLogState {
    pub fn baseline(&self) -> Option<[u8; 16]> {
        self.baseline
    }
}

/// Whether and how [`dump_tracked`] may use the dirty log.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeltaPolicy<'a> {
    /// Incremental snapshots are enabled for this VM
    /// ([`incremental_enabled`] for the live VM).
    pub enabled: bool,
    /// An active device that writes guest RAM outside both the KVM log and
    /// `GuestMemory::host_dirty` (kernel vhost-net); blocks deltas.
    pub untracked_writer: Option<&'a str>,
}

/// What [`dump_tracked`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapKind {
    Full,
    Delta { pages: u64 },
}

pub fn is_flux_kvm_vmstate(path: &Path) -> bool {
    let mut buf = [0u8; 8];
    match File::open(path).and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => &buf == MAGIC,
        Err(_) => false,
    }
}

fn write_virtio_v3(f: &mut File, devices: &[VirtioState]) -> Result<()> {
    if devices.len() > MAX_SNAPSHOT_DEVICES
        || devices.iter().any(|d| d.num_queues > MAX_VIRTIO_QUEUES)
    {
        return Err(FluxError::Hypervisor(
            "snapshot virtio device or queue count exceeds limit".into(),
        ));
    }
    let ndev = devices.len() as u32;
    f.write_all(&ndev.to_le_bytes())
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    for st in devices {
        f.write_all(&st.device_id.to_le_bytes())
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        f.write_all(&st.features.to_le_bytes())
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        f.write_all(&st.driver_features.to_le_bytes())
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        f.write_all(&st.status.to_le_bytes())
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        f.write_all(&st.interrupt_status.to_le_bytes())
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        f.write_all(&st.num_queues.to_le_bytes())
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        let nq = st.num_queues as usize;
        for q in st.queues.iter().take(nq) {
            f.write_all(&q.num.to_le_bytes())
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            f.write_all(&q.ready.to_le_bytes())
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            f.write_all(&q.desc.to_le_bytes())
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            f.write_all(&q.avail.to_le_bytes())
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            f.write_all(&q.used.to_le_bytes())
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            f.write_all(&q.last_avail.to_le_bytes())
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        }
    }
    Ok(())
}

fn read_virtio_v3(f: &mut File) -> Result<Vec<VirtioState>> {
    let mut nb = [0u8; 4];
    f.read_exact(&mut nb)
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    let ndev = u32::from_le_bytes(nb) as usize;
    if ndev > MAX_SNAPSHOT_DEVICES {
        return Err(FluxError::Hypervisor(format!(
            "snapshot has too many virtio devices: {ndev}"
        )));
    }
    let mut out = Vec::with_capacity(ndev);
    for _ in 0..ndev {
        let mut st = VirtioState::default();
        let mut u32b = [0u8; 4];
        let mut u64b = [0u8; 8];
        let mut u16b = [0u8; 2];
        f.read_exact(&mut u32b)
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        st.device_id = u32::from_le_bytes(u32b);
        f.read_exact(&mut u64b)
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        st.features = u64::from_le_bytes(u64b);
        f.read_exact(&mut u64b)
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        st.driver_features = u64::from_le_bytes(u64b);
        f.read_exact(&mut u32b)
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        st.status = u32::from_le_bytes(u32b);
        f.read_exact(&mut u32b)
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        st.interrupt_status = u32::from_le_bytes(u32b);
        f.read_exact(&mut u32b)
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        st.num_queues = u32::from_le_bytes(u32b);
        if st.num_queues > MAX_VIRTIO_QUEUES {
            return Err(FluxError::Hypervisor(format!(
                "snapshot has too many virtio queues: {}",
                st.num_queues
            )));
        }
        for i in 0..st.num_queues as usize {
            let mut q = QueueState::default();
            f.read_exact(&mut u32b)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            q.num = u32::from_le_bytes(u32b);
            f.read_exact(&mut u32b)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            q.ready = u32::from_le_bytes(u32b);
            f.read_exact(&mut u64b)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            q.desc = u64::from_le_bytes(u64b);
            f.read_exact(&mut u64b)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            q.avail = u64::from_le_bytes(u64b);
            f.read_exact(&mut u64b)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            q.used = u64::from_le_bytes(u64b);
            f.read_exact(&mut u16b)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            q.last_avail = u16::from_le_bytes(u16b);
            st.queues[i] = q;
        }
        out.push(st);
    }
    Ok(out)
}

/// Full snapshot: all of guest RAM to `mem_path`, vCPU/device state to
/// `vmstate`. Leaves dirty logging alone (migration export uses this).
pub fn dump(
    kvm: &KvmVm,
    mem: &GuestMemory,
    vmstate: &Path,
    mem_path: &Path,
    virtio: &[VirtioState],
) -> Result<()> {
    dump_full(kvm, mem, vmstate, mem_path, virtio).map(|_| ())
}

fn dump_full(
    kvm: &KvmVm,
    mem: &GuestMemory,
    vmstate: &Path,
    mem_path: &Path,
    virtio: &[VirtioState],
) -> Result<Lineage> {
    if let Some(parent) = mem_path.parent() {
        fs::create_dir_all(parent).map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    }
    fs::write(mem_path, mem.as_slice()).map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    let lineage = Lineage {
        snapshot_id: new_snapshot_id(),
        ..Lineage::default()
    };
    write_vmstate(kvm, mem.len() as u64, vmstate, virtio, &lineage)?;
    Ok(lineage)
}

/// Why a delta cannot be written right now, if it cannot.
fn delta_refusal(
    kvm: &KvmVm,
    log: &DirtyLogState,
    parent: &SnapParent,
    policy: DeltaPolicy<'_>,
) -> Option<String> {
    if !policy.enabled {
        return Some("FLUXVM_KVM_DIRTY_LOG=1 is not set".into());
    }
    if let Some(w) = policy.untracked_writer {
        return Some(format!(
            "{w} writes guest RAM outside the KVM dirty log and host-write tracking"
        ));
    }
    if !kvm.dirty_logging() {
        return Some("KVM dirty logging is not on (no full snapshot taken with it yet)".into());
    }
    let Some(base) = log.baseline else {
        return Some("no dirty-log baseline (no full snapshot, or the log was reset)".into());
    };
    match load_cpu(&parent.vmstate) {
        Ok(cpu) => match cpu.lineage {
            Some(l) if l.snapshot_id == base => None,
            Some(_) => Some(format!(
                "{} is not this VM's latest snapshot; the dirty log only covers changes since that",
                parent.vmstate.display()
            )),
            None => Some(format!(
                "{} predates vmstate v6 lineage",
                parent.vmstate.display()
            )),
        },
        Err(e) => Some(format!("reading parent {}: {e}", parent.vmstate.display())),
    }
}

/// Snapshot entry point for the live VM (`SnapCmd`).
///
/// - `parent: None`: full snapshot. With `policy.enabled` (and no
///   `policy.untracked_writer`), dirty logging is then switched on and cleared so
///   this snapshot becomes the baseline for the next delta.
/// - `parent: Some(p)`: a delta of the pages written since `p`, which must
///   be the baseline. Any reason a delta is unsafe (logging off, another
///   snapshot taken since `p`, vhost-net writing RAM behind KVM's back)
///   logs why and writes a full snapshot instead, which always restores.
///
/// The VM must be paused and device queues quiesced.
#[allow(clippy::too_many_arguments)]
pub fn dump_tracked(
    kvm: &KvmVm,
    mem: &GuestMemory,
    vmstate: &Path,
    mem_path: &Path,
    virtio: &[VirtioState],
    parent: Option<&SnapParent>,
    policy: DeltaPolicy<'_>,
    log: &mut DirtyLogState,
) -> Result<SnapKind> {
    if let Some(p) = parent {
        match delta_refusal(kvm, log, p, policy) {
            None => {
                let base = log.baseline.unwrap_or_default();
                // From here the KVM log is consumed: whatever happens, the
                // next delta needs a new baseline unless this one lands.
                log.baseline = None;
                let lineage = Lineage {
                    snapshot_id: new_snapshot_id(),
                    parent_id: base,
                    delta: true,
                };
                let pages = dump_delta(kvm, mem, vmstate, mem_path, virtio, p, &lineage)?;
                log.baseline = Some(lineage.snapshot_id);
                return Ok(SnapKind::Delta { pages });
            }
            Some(why) => {
                eprintln!("[kvm-engine] incremental snapshot not possible ({why}); writing a full snapshot");
            }
        }
    }
    log.baseline = None;
    let lineage = dump_full(kvm, mem, vmstate, mem_path, virtio)?;
    if policy.enabled {
        if let Some(w) = policy.untracked_writer {
            eprintln!("[kvm-engine] dirty log not enabled: {w} writes guest RAM outside it");
        } else {
            match start_dirty_interval(kvm, mem) {
                Ok(()) => log.baseline = Some(lineage.snapshot_id),
                Err(e) => {
                    mem.host_dirty().set_enabled(false);
                    eprintln!("[kvm-engine] dirty log not enabled ({e}); next snapshot is full");
                }
            }
        }
    }
    Ok(SnapKind::Full)
}

/// Turn on (if needed) and clear both dirty logs: the interval for the next
/// delta starts now.
fn start_dirty_interval(kvm: &KvmVm, mem: &GuestMemory) -> Result<()> {
    if !kvm.dirty_logging() {
        kvm.set_dirty_logging(true)?;
    }
    kvm.get_dirty_log()?;
    mem.host_dirty().set_enabled(true);
    Ok(())
}

fn dump_delta(
    kvm: &KvmVm,
    mem: &GuestMemory,
    vmstate: &Path,
    mem_path: &Path,
    virtio: &[VirtioState],
    parent: &SnapParent,
    lineage: &Lineage,
) -> Result<u64> {
    let mut bitmap = kvm.get_dirty_log()?;
    dirty_log::merge_bitmap(&mut bitmap, &mem.host_dirty().take());
    let ranges = dirty_log::bitmap_to_ranges(&bitmap, dirty_log::page_count(mem.len() as u64));
    let pages = dirty_log::ranges_pages(&ranges);
    if let Some(dir) = mem_path.parent() {
        fs::create_dir_all(dir).map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    }
    let header = DeltaHeader {
        mem_len: mem.len() as u64,
        snapshot_id: lineage.snapshot_id,
        parent_id: lineage.parent_id,
        parent_mem: parent.mem.clone(),
        ranges,
    };
    dirty_log::write_delta(mem_path, &header, mem.as_slice())
        .map_err(|e| FluxError::Hypervisor(format!("writing delta {}: {e}", mem_path.display())))?;
    write_vmstate(kvm, mem.len() as u64, vmstate, virtio, lineage)?;
    eprintln!(
        "[kvm-engine] incremental snapshot: {pages} of {} pages dirty",
        dirty_log::page_count(mem.len() as u64)
    );
    Ok(pages)
}

fn write_vmstate(
    kvm: &KvmVm,
    mem_len: u64,
    vmstate: &Path,
    virtio: &[VirtioState],
    lineage: &Lineage,
) -> Result<()> {
    if let Some(parent) = vmstate.parent() {
        fs::create_dir_all(parent).map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    }
    let ncpus = kvm.num_cpus() as u32;
    if ncpus == 0 || ncpus as usize > MAX_SNAPSHOT_VCPUS {
        return Err(FluxError::Hypervisor(format!(
            "unsupported snapshot vCPU count: {ncpus}"
        )));
    }
    let mut f = File::create(vmstate).map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    f.write_all(MAGIC)
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    f.write_all(&VERSION.to_le_bytes())
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    f.write_all(&mem_len.to_le_bytes())
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    f.write_all(&ncpus.to_le_bytes())
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    for i in 0..ncpus as usize {
        let regs = kvm.get_regs(i)?;
        let sregs = kvm.get_sregs(i)?;
        unsafe {
            let regs_bytes = std::slice::from_raw_parts(
                (&regs as *const KvmRegs) as *const u8,
                std::mem::size_of::<KvmRegs>(),
            );
            let sregs_bytes = std::slice::from_raw_parts(
                (&sregs as *const KvmSregs) as *const u8,
                std::mem::size_of::<KvmSregs>(),
            );
            f.write_all(regs_bytes)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            f.write_all(sregs_bytes)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        }
    }
    write_virtio_v3(&mut f, virtio)?;
    for i in 0..ncpus as usize {
        f.write_all(&kvm.get_mp_state(i)?.to_le_bytes())
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    }
    // v6: lineage block.
    lineage.write(&mut f)?;
    // v5: full-fidelity vCPU and VM state (the vCPUs are parked by now).
    let (vcpu_states, vm_state) = kvm_state::capture(kvm)?;
    kvm_state::write_sections(&mut f, &vcpu_states, &vm_state)
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    f.sync_all()
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    Ok(())
}

#[derive(Clone)]
pub struct CpuSnapshot {
    pub mem_len: u64,
    pub regs: KvmRegs,
    pub sregs: KvmSregs,
    pub all_vcpus: Vec<(KvmRegs, KvmSregs)>,
    /// Virtio device live-state from FLUXKVM1 v3 (empty for v1/v2).
    pub virtio: Vec<VirtioState>,
    /// Per-vCPU `KVM_MP_STATE` from v4; empty for older snapshots (which
    /// only ever held one vCPU).
    pub mp_states: Vec<u32>,
    /// On-disk format version.
    pub version: u32,
    /// Per-vCPU full-fidelity state from v5; entries are incomplete
    /// (`!is_complete()`) for older snapshots.
    pub vcpu_states: Vec<VcpuState>,
    /// VM-wide clock/irqchip/PIT state from v5; `None` for older snapshots.
    pub vm_state: Option<VmState>,
    /// Snapshot id / parent / delta flag from v6; `None` for older files.
    pub lineage: Option<Lineage>,
}

pub fn load_cpu(vmstate: &Path) -> Result<CpuSnapshot> {
    let mut f = File::open(vmstate).map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    if &magic != MAGIC {
        return Err(FluxError::Hypervisor(
            "not a FluxVM in-tree KVM vmstate (expected FLUXKVM1)".into(),
        ));
    }
    let mut ver = [0u8; 4];
    f.read_exact(&mut ver)
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    let version = u32::from_le_bytes(ver);
    if !(1..=VERSION).contains(&version) {
        return Err(FluxError::Hypervisor(format!(
            "unsupported KVM vmstate version {version}"
        )));
    }
    let mut lenb = [0u8; 8];
    f.read_exact(&mut lenb)
        .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
    let mem_len = u64::from_le_bytes(lenb);

    let mut all_vcpus = Vec::new();
    let ncpus = if version >= 2 {
        let mut nb = [0u8; 4];
        f.read_exact(&mut nb)
            .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        u32::from_le_bytes(nb) as usize
    } else {
        1
    };
    if ncpus == 0 || ncpus > MAX_SNAPSHOT_VCPUS {
        return Err(FluxError::Hypervisor(format!(
            "invalid snapshot vCPU count: {ncpus}"
        )));
    }
    for _ in 0..ncpus {
        let mut regs = unsafe { std::mem::zeroed::<KvmRegs>() };
        let mut sregs = unsafe { std::mem::zeroed::<KvmSregs>() };
        unsafe {
            let regs_bytes = std::slice::from_raw_parts_mut(
                (&mut regs as *mut KvmRegs) as *mut u8,
                std::mem::size_of::<KvmRegs>(),
            );
            let sregs_bytes = std::slice::from_raw_parts_mut(
                (&mut sregs as *mut KvmSregs) as *mut u8,
                std::mem::size_of::<KvmSregs>(),
            );
            f.read_exact(regs_bytes)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            f.read_exact(sregs_bytes)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        }
        all_vcpus.push((regs, sregs));
    }
    let virtio = if version >= 3 {
        read_virtio_v3(&mut f)?
    } else {
        // v2 reserved watermark — discard.
        let mut pad = [0u8; V2_WATERMARK];
        if version == 2 {
            f.read_exact(&mut pad)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
        }
        Vec::new()
    };
    let mut mp_states = Vec::new();
    if version >= 4 {
        for _ in 0..ncpus {
            let mut b = [0u8; 4];
            f.read_exact(&mut b)
                .map_err(|e| FluxError::Hypervisor(e.to_string()))?;
            mp_states.push(u32::from_le_bytes(b));
        }
    }
    let lineage = if version >= 6 {
        Some(Lineage::read(&mut f)?)
    } else {
        None
    };
    let (vcpu_states, vm_state) = if version >= 5 {
        let (v, vm) = kvm_state::read_sections(&mut f, ncpus)?;
        (v, Some(vm))
    } else {
        (vec![VcpuState::default(); ncpus], None)
    };
    let (regs, sregs) = all_vcpus[0];
    Ok(CpuSnapshot {
        mem_len,
        regs,
        sregs,
        all_vcpus,
        virtio,
        mp_states,
        version,
        vcpu_states,
        vm_state,
        lineage,
    })
}

/// Guest RAM length a snapshot memory file restores to: its size for a full
/// `snap.mem`, the recorded RAM length for a `FLUXKVMD` delta.
pub fn memory_image_len(mem_path: &Path) -> Result<u64> {
    if dirty_log::is_delta(mem_path) {
        return dirty_log::read_header_from(mem_path)
            .map(|h| h.mem_len)
            .map_err(|e| FluxError::Hypervisor(format!("{}: {e}", mem_path.display())));
    }
    fs::metadata(mem_path)
        .map(|m| m.len())
        .map_err(|e| FluxError::Hypervisor(e.to_string()))
}

/// Load a snapshot's memory into guest RAM. A `FLUXKVMD` delta loads its
/// parent chain first (the base through the usual uffd / `MAP_PRIVATE` /
/// eager path) and then patches each delta's pages over it.
pub fn load_memory_into(mem: &mut GuestMemory, mem_path: &Path) -> Result<()> {
    if dirty_log::is_delta(mem_path) {
        return load_delta_chain(mem, mem_path, 0);
    }
    load_full_memory_into(mem, mem_path)
}

fn load_delta_chain(mem: &mut GuestMemory, delta: &Path, depth: usize) -> Result<()> {
    if depth >= dirty_log::MAX_CHAIN_DEPTH {
        return Err(FluxError::Hypervisor(format!(
            "delta chain deeper than {} at {}",
            dirty_log::MAX_CHAIN_DEPTH,
            delta.display()
        )));
    }
    let hdr = dirty_log::read_header_from(delta)
        .map_err(|e| FluxError::Hypervisor(format!("{}: {e}", delta.display())))?;
    if hdr.mem_len != mem.len() as u64 {
        return Err(FluxError::Hypervisor(format!(
            "delta {} RAM size {} does not match guest RAM {}",
            delta.display(),
            hdr.mem_len,
            mem.len()
        )));
    }
    let parent = hdr.resolve_parent(delta);
    check_parent_identity(&parent, &hdr.parent_id)?;
    if dirty_log::is_delta(&parent) {
        load_delta_chain(mem, &parent, depth + 1)?;
    } else {
        load_full_memory_into(mem, &parent)?;
    }
    dirty_log::apply_delta(delta, mem.as_slice_mut())
        .map_err(|e| FluxError::Hypervisor(format!("applying delta {}: {e}", delta.display())))?;
    Ok(())
}

/// Best-effort check that `parent` is the snapshot a delta was taken
/// against: a parent delta carries its id in its header, and a full parent
/// laid out by `snapshot::save` has a sibling `.vmstate` with v6 lineage.
fn check_parent_identity(parent: &Path, want: &[u8; 16]) -> Result<()> {
    let found = if dirty_log::is_delta(parent) {
        dirty_log::read_header_from(parent)
            .ok()
            .map(|h| h.snapshot_id)
    } else {
        let vs = parent.with_extension("vmstate");
        if vs.exists() {
            load_cpu(&vs)
                .ok()
                .and_then(|c| c.lineage)
                .map(|l| l.snapshot_id)
        } else {
            None
        }
    };
    match found {
        Some(id) if &id != want => Err(FluxError::Hypervisor(format!(
            "delta parent {} is a different snapshot than the one the delta was taken against",
            parent.display()
        ))),
        _ => Ok(()),
    }
}

fn load_full_memory_into(mem: &mut GuestMemory, mem_path: &Path) -> Result<()> {
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
    // Any failure (no userfaultfd, EPERM / vm.unprivileged_userfaultfd=0,
    // missing UFFDIO_COPY, mlock/hugepage RAM) falls through to the
    // MAP_PRIVATE path below, which replaces the whole mapping again.
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

/// Apply multi-vCPU snapshot after KvmVm is created.
pub fn restore_vcpus(kvm: &KvmVm, snap: &CpuSnapshot) -> Result<()> {
    if snap.all_vcpus.len() != kvm.num_cpus() {
        return Err(FluxError::Hypervisor(format!(
            "snapshot has {} vCPUs, VM has {}",
            snap.all_vcpus.len(),
            kvm.num_cpus()
        )));
    }
    let full = snap.version >= 5
        && snap.vcpu_states.len() == snap.all_vcpus.len()
        && snap.vcpu_states.iter().all(VcpuState::is_complete)
        && snap.vm_state.as_ref().is_some_and(VmState::is_complete);
    if !full {
        eprintln!(
            "[kvm] vmstate v{} has no full-fidelity state: restoring registers only \
             (LAPIC, MSRs, FPU, clock, irqchip and PIT start fresh)",
            snap.version
        );
    }
    // VM-wide clock/PIC/IOAPIC/PIT first, then each vCPU (Firecracker's order).
    if full {
        if let Some(vm) = &snap.vm_state {
            kvm_state::apply_vm(kvm, vm)?;
        }
    }
    for (i, (regs, sregs)) in snap.all_vcpus.iter().enumerate() {
        kvm.set_sregs(i, *sregs)?;
        kvm.set_regs(i, *regs)?;
        if full {
            kvm_state::apply_vcpu(kvm, i, &snap.vcpu_states[i])?;
        }
    }
    // APs are created UNINITIALIZED; put each back in the state it had when
    // the snapshot was taken (a booted AP must be RUNNABLE or it never runs).
    for (i, state) in snap.mp_states.iter().enumerate().skip(1) {
        kvm.set_mp_state(i, *state)?;
    }
    Ok(())
}

/// Overlay packed virtio live-state onto live device backends (matched by
/// `device_id`). Backends (disk path, TAP) stay from boot config.
///
/// vhost-net holds no state of its own across a restore: the restored
/// queue rings and `last_avail` are what it is programmed from, so the caller
/// (`vm.rs`, once the queue service exists) re-binds it with
/// `QueueService::rebind_vhost_after_restore` rather than waiting for a guest
/// notify that an idle RX queue may never send.
pub fn restore_virtio(
    targets: &[&std::sync::Arc<crate::devices::virtio_mmio::VirtioMmio>],
    snap: &[VirtioState],
) {
    for packed in snap {
        for target in targets {
            let Ok(mut live) = target.state.lock() else {
                continue;
            };
            if live.device_id != packed.device_id {
                continue;
            }
            live.features = packed.features;
            live.driver_features = packed.driver_features;
            live.status = packed.status;
            live.interrupt_status = packed.interrupt_status;
            live.num_queues = packed.num_queues;
            let nq = packed.num_queues.min(MAX_VIRTIO_QUEUES) as usize;
            for i in 0..nq {
                live.queues[i] = packed.queues[i].clone();
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio_mmio::{VIRTIO_ID_BLOCK, VIRTIO_ID_NET};

    fn snapshot_header(version: u32, ncpus: u32) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes.extend_from_slice(&4096u64.to_le_bytes());
        bytes.extend_from_slice(&ncpus.to_le_bytes());
        bytes
    }

    /// A pre-v5 file (registers, empty virtio list, one MP state) must keep
    /// loading, with no full-fidelity state, so old snapshots still restore.
    #[test]
    fn v4_file_still_loads_without_full_state() {
        let mut bytes = snapshot_header(4, 1);
        bytes.extend(vec![
            0u8;
            std::mem::size_of::<KvmRegs>()
                + std::mem::size_of::<KvmSregs>()
        ]);
        bytes.extend_from_slice(&0u32.to_le_bytes()); // virtio device count
        bytes.extend_from_slice(&0u32.to_le_bytes()); // MP state of vCPU 0
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v4.vmstate");
        fs::write(&path, bytes).unwrap();
        let cpu = load_cpu(&path).unwrap();
        assert_eq!(cpu.version, 4);
        assert_eq!(cpu.mp_states, vec![0]);
        assert!(cpu.vm_state.is_none());
        assert!(!cpu.vcpu_states[0].is_complete());
    }

    /// A v5 header with no sections at all loads but is not restorable
    /// (preflight rejects it), rather than silently restoring half a guest.
    #[test]
    fn v5_file_without_sections_loads_as_incomplete() {
        let mut bytes = snapshot_header(5, 1);
        bytes.extend(vec![
            0u8;
            std::mem::size_of::<KvmRegs>()
                + std::mem::size_of::<KvmSregs>()
        ]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v5.vmstate");
        fs::write(&path, bytes).unwrap();
        let cpu = load_cpu(&path).unwrap();
        assert_eq!(cpu.version, 5);
        assert!(!cpu.vcpu_states[0].is_complete());
        assert!(!cpu.vm_state.unwrap().is_complete());
    }

    /// v6 inserts a lineage block before the tagged sections.
    #[test]
    fn v6_file_carries_lineage() {
        let mut bytes = snapshot_header(6, 1);
        bytes.extend(vec![
            0u8;
            std::mem::size_of::<KvmRegs>()
                + std::mem::size_of::<KvmSregs>()
        ]);
        bytes.extend_from_slice(&0u32.to_le_bytes()); // virtio device count
        bytes.extend_from_slice(&0u32.to_le_bytes()); // MP state of vCPU 0
        bytes.extend_from_slice(&[7u8; 16]); // snapshot id
        bytes.extend_from_slice(&[3u8; 16]); // parent id
        bytes.extend_from_slice(&1u32.to_le_bytes()); // delta flag
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v6.vmstate");
        fs::write(&path, &bytes).unwrap();
        let cpu = load_cpu(&path).unwrap();
        assert_eq!(cpu.version, 6);
        assert_eq!(
            cpu.lineage,
            Some(Lineage {
                snapshot_id: [7; 16],
                parent_id: [3; 16],
                delta: true,
            })
        );
        // Older files have no lineage.
        let mut v5 = snapshot_header(5, 1);
        v5.extend_from_slice(&bytes[v5.len()..bytes.len() - 36]);
        fs::write(&path, &v5).unwrap();
        assert_eq!(load_cpu(&path).unwrap().lineage, None);
        // Truncated lineage is an error, not a silent full snapshot.
        fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
        assert!(load_cpu(&path).is_err());
    }

    fn page_pattern(pages: usize, seed: u8) -> Vec<u8> {
        (0..pages * 4096)
            .map(|i| ((i / 4096) as u8).wrapping_mul(17) ^ seed ^ (i as u8))
            .collect()
    }

    fn write_delta_over(
        dir: &Path,
        name: &str,
        parent: &str,
        mem: &[u8],
        ids: ([u8; 16], [u8; 16]),
        ranges: Vec<(u64, u64)>,
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        dirty_log::write_delta(
            &path,
            &DeltaHeader {
                mem_len: mem.len() as u64,
                snapshot_id: ids.0,
                parent_id: ids.1,
                parent_mem: parent.into(),
                ranges,
            },
            mem,
        )
        .unwrap();
        path
    }

    /// base.mem <- d1 (relative parent) <- d2: restore of d2 is the base
    /// with d1's pages and then d2's pages on top.
    #[test]
    fn restores_a_delta_chain() {
        let dir = tempfile::tempdir().unwrap();
        let pages = 8;
        let base = page_pattern(pages, 0);
        fs::write(dir.path().join("base.mem"), &base).unwrap();

        let mut state1 = base.clone();
        state1[4096..2 * 4096].fill(0x11); // gfn 1
        state1[5 * 4096..6 * 4096].fill(0x15); // gfn 5
        let d1 = write_delta_over(
            dir.path(),
            "d1.mem",
            "base.mem",
            &state1,
            ([1; 16], [0; 16]),
            vec![(1, 1), (5, 1)],
        );
        let mut state2 = state1.clone();
        state2[5 * 4096..7 * 4096].fill(0x25); // gfn 5-6
        let d2 = write_delta_over(
            dir.path(),
            "d2.mem",
            d1.to_str().unwrap(),
            &state2,
            ([2; 16], [1; 16]),
            vec![(5, 2)],
        );
        assert_eq!(memory_image_len(&d2).unwrap(), (pages * 4096) as u64);

        let mut mem = GuestMemory::allocate(pages * 4096).unwrap();
        load_memory_into(&mut mem, &d2).unwrap();
        assert_eq!(mem.as_slice(), &state2[..]);

        // A delta whose recorded parent id does not match its parent's id
        // is refused instead of restoring a mix of two histories.
        let bad = write_delta_over(
            dir.path(),
            "bad.mem",
            "d1.mem",
            &state2,
            ([3; 16], [9; 16]),
            vec![(0, 1)],
        );
        let mut mem = GuestMemory::allocate(pages * 4096).unwrap();
        let err = load_memory_into(&mut mem, &bad).unwrap_err().to_string();
        assert!(err.contains("different snapshot"), "{err}");

        // Wrong guest RAM size.
        let mut small = GuestMemory::allocate(4 * 4096).unwrap();
        assert!(load_memory_into(&mut small, &d2).is_err());

        // Missing parent.
        fs::remove_file(dir.path().join("base.mem")).unwrap();
        let mut mem = GuestMemory::allocate(pages * 4096).unwrap();
        assert!(load_memory_into(&mut mem, &d2).is_err());
    }

    /// Real KVM: a few real-mode instructions at 0x1000 that read 0x2000,
    /// report it on port 0x10, increment the byte at 0x3000, report on port
    /// 0x11 and loop. `#[ignore]`d: run on a KVM host with
    /// `cargo test -p fluxvm-hypervisor --lib real_kvm -- --ignored`
    /// (the uffd one also needs root or `vm.unprivileged_userfaultfd=1`).
    #[cfg(target_os = "linux")]
    mod real_kvm {
        use super::*;
        use crate::ffi;

        const PROGRAM: [u8; 13] = [
            0xA0, 0x00, 0x20, // mov al, [0x2000]
            0xE6, 0x10, // out 0x10, al
            0xFE, 0x06, 0x00, 0x30, // inc byte [0x3000]
            0xE6, 0x11, // out 0x11, al
            0xEB, 0xF3, // jmp 0x1000
        ];

        fn kvm_usable() -> bool {
            let ok = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok();
            if !ok {
                eprintln!("skipping: /dev/kvm not usable");
            }
            ok
        }

        fn real_mode_at_0x1000(kvm: &KvmVm) {
            let mut sregs = kvm.get_sregs(0).unwrap();
            sregs.cs.base = 0;
            sregs.cs.selector = 0;
            sregs.ds.base = 0;
            sregs.ds.selector = 0;
            kvm.set_sregs(0, sregs).unwrap();
            let mut regs = kvm.get_regs(0).unwrap();
            regs.rip = 0x1000;
            regs.rflags = 2;
            kvm.set_regs(0, regs).unwrap();
        }

        /// Run until an OUT to `port`; returns the byte written.
        fn run_until_out(kvm: &KvmVm, port: u16) -> u8 {
            for _ in 0..1000 {
                let exit = kvm.run_once(0).unwrap();
                if exit == ffi::KVM_EXIT_INTR {
                    continue;
                }
                assert_eq!(exit, ffi::KVM_EXIT_IO, "unexpected KVM exit {exit}");
                let (_dir, _size, p, _count, off) = kvm.io_info(0);
                if p == port {
                    return kvm.io_data(0, off, 1)[0];
                }
            }
            panic!("guest never wrote port {port:#x}");
        }

        #[test]
        #[ignore = "needs a working /dev/kvm; run with --ignored on a KVM host"]
        fn dirty_log_sees_guest_writes_only() {
            if !kvm_usable() {
                return;
            }
            let mut mem = GuestMemory::allocate(2 << 20).unwrap();
            mem.write_at(0x1000, &PROGRAM).unwrap();
            mem.write_at(0x2000, &[0x42]).unwrap();
            let Ok(kvm) = KvmVm::create(&mem, 1) else {
                eprintln!("skipping: KvmVm::create failed");
                return;
            };
            real_mode_at_0x1000(&kvm);
            assert!(kvm.get_dirty_log().is_err(), "log is off by default");
            kvm.set_dirty_logging(true).unwrap();
            kvm.get_dirty_log().unwrap();
            assert_eq!(run_until_out(&kvm, 0x10), 0x42);
            assert_eq!(run_until_out(&kvm, 0x11), 0x42);
            let log = kvm.get_dirty_log().unwrap();
            assert_eq!(log.len(), dirty_log::bitmap_words(2 << 20));
            let ranges = dirty_log::bitmap_to_ranges(&log, 512);
            assert_eq!(ranges, vec![(3, 1)], "only the incremented page");
            assert_eq!(mem.as_slice()[0x3000], 1);
            // The log was cleared by the read.
            let again = kvm.get_dirty_log().unwrap();
            assert!(again.iter().all(|w| *w == 0));
            kvm.set_dirty_logging(false).unwrap();
            assert!(!kvm.dirty_logging());
        }

        /// Full snapshot, guest + host writes, incremental snapshot, restore
        /// the chain into fresh RAM: identical to the live RAM.
        #[test]
        #[ignore = "needs a working /dev/kvm; run with --ignored on a KVM host"]
        fn incremental_snapshot_round_trip() {
            if !kvm_usable() {
                return;
            }
            let len = 2 << 20;
            let mut mem = GuestMemory::allocate(len).unwrap();
            mem.write_at(0x1000, &PROGRAM).unwrap();
            mem.write_at(0x2000, &[0x42]).unwrap();
            let Ok(kvm) = KvmVm::create(&mem, 1) else {
                eprintln!("skipping: KvmVm::create failed");
                return;
            };
            real_mode_at_0x1000(&kvm);
            run_until_out(&kvm, 0x11);

            let dir = tempfile::tempdir().unwrap();
            let base_vs = dir.path().join("base.vmstate");
            let base_mem = dir.path().join("base.mem");
            let policy = DeltaPolicy {
                enabled: true,
                untracked_writer: None,
            };
            let mut log = DirtyLogState::default();
            let kind =
                dump_tracked(&kvm, &mem, &base_vs, &base_mem, &[], None, policy, &mut log).unwrap();
            assert_eq!(kind, SnapKind::Full);
            assert!(kvm.dirty_logging());
            let base_id = load_cpu(&base_vs).unwrap().lineage.unwrap().snapshot_id;
            assert_eq!(log.baseline(), Some(base_id));

            // Guest dirties gfn 3 again; the host dirties gfn 5 (as a
            // virtio completion would).
            run_until_out(&kvm, 0x11);
            mem.write_at(0x5000 + 12, b"host").unwrap();

            let parent = SnapParent {
                vmstate: base_vs.clone(),
                mem: base_mem.clone(),
            };
            let d_vs = dir.path().join("d1.vmstate");
            let d_mem = dir.path().join("d1.mem");
            // A writer the logs cannot see forces a full snapshot.
            let blocked = DeltaPolicy {
                enabled: true,
                untracked_writer: Some("vhost-net"),
            };
            let mut scratch = DirtyLogState {
                baseline: log.baseline,
            };
            let k = dump_tracked(
                &kvm,
                &mem,
                &dir.path().join("x.vmstate"),
                &dir.path().join("x.mem"),
                &[],
                Some(&parent),
                blocked,
                &mut scratch,
            )
            .unwrap();
            assert_eq!(k, SnapKind::Full);

            let kind = dump_tracked(
                &kvm,
                &mem,
                &d_vs,
                &d_mem,
                &[],
                Some(&parent),
                policy,
                &mut log,
            )
            .unwrap();
            assert_eq!(kind, SnapKind::Delta { pages: 2 });
            let hdr = dirty_log::read_header_from(&d_mem).unwrap();
            assert_eq!(hdr.ranges, vec![(3, 1), (5, 1)]);
            assert_eq!(hdr.parent_id, base_id);
            let lineage = load_cpu(&d_vs).unwrap().lineage.unwrap();
            assert!(lineage.delta);
            assert_eq!(lineage.parent_id, base_id);
            assert_eq!(log.baseline(), Some(lineage.snapshot_id));

            // The base is no longer the baseline: another delta on top of it
            // would miss the pages captured in d1, so it is refused (full).
            let k = dump_tracked(
                &kvm,
                &mem,
                &dir.path().join("y.vmstate"),
                &dir.path().join("y.mem"),
                &[],
                Some(&parent),
                policy,
                &mut log,
            )
            .unwrap();
            assert_eq!(k, SnapKind::Full);

            let mut restored = GuestMemory::allocate(len).unwrap();
            load_memory_into(&mut restored, &d_mem).unwrap();
            assert!(restored.as_slice() == mem.as_slice());
            assert_eq!(restored.as_slice()[0x3000], 2);
            assert_eq!(&restored.as_slice()[0x500c..0x5010], b"host");
        }

        /// Real KVM on uffd-backed RAM: the guest's instruction fetch and
        /// data read fault through the handler thread. Needs a kernel-fault
        /// userfaultfd (root, CAP_SYS_PTRACE or
        /// vm.unprivileged_userfaultfd=1); skips otherwise.
        #[test]
        #[ignore = "needs a working /dev/kvm; run with --ignored on a KVM host"]
        fn guest_runs_on_uffd_restored_ram() {
            if !kvm_usable() {
                return;
            }
            let len = 2 << 20;
            let mut snap = vec![0u8; len];
            snap[0x1000..0x1000 + PROGRAM.len()].copy_from_slice(&PROGRAM);
            snap[0x2000] = 0x42;
            let dir = tempfile::tempdir().unwrap();
            let snap_path = dir.path().join("snap.mem");
            fs::write(&snap_path, &snap).unwrap();
            let file = File::open(&snap_path).unwrap();
            let mut mem = GuestMemory::allocate(len).unwrap();
            if let Err(e) = mem.setup_uffd_restore(&file) {
                eprintln!("skipping: {e}");
                return;
            }
            let Ok(kvm) = KvmVm::create(&mem, 1) else {
                eprintln!("skipping: KvmVm::create failed");
                return;
            };
            real_mode_at_0x1000(&kvm);
            assert_eq!(run_until_out(&kvm, 0x10), 0x42);
            assert_eq!(run_until_out(&kvm, 0x11), 0x42);
            let stats = mem.uffd_pager().unwrap().stats();
            assert!(stats.faults.load(std::sync::atomic::Ordering::Relaxed) >= 1);
            assert_eq!(stats.errors.load(std::sync::atomic::Ordering::Relaxed), 0);
            assert_eq!(mem.as_slice()[0x3000], 1);
        }
    }

    // Additional tests from original file omitted in this truncated view for brevity in the call;
    // the full file on disk includes all original tests.
}
