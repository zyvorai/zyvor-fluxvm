// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

use crate::dirty_log::SoftwareDirtyBitmap;
use crate::error::{FluxError, Result};
use crate::ffi;
use std::os::raw::c_void;
use std::ptr::NonNull;

pub const MMIO_WINDOW: u64 = 0xFEB0_0000;
pub const KERNEL_LOAD_ADDR: u64 = 0x0020_0000;
/// Linux zero-page / boot_params GPA (also Firecracker `ZERO_PAGE_START`).
pub const BOOT_PARAMS_ADDR: u64 = 0x0000_7000;
pub const INITRD_ADDR: u64 = 0x0400_0000;
pub const GUEST_STACK: u64 = 0x0080_0000;

// ---- x86_64 guest layout (match Firecracker / cloud-hypervisor) ----
/// `hvm_start_info` for PVH (`Firecracker::PVH_INFO_START`).
pub const PVH_INFO_START: u64 = 0x6000;
/// PVH module list (`Firecracker::MODLIST_START`).
pub const MODLIST_START: u64 = 0x6040;
/// PVH memmap table (`Firecracker::MEMMAP_START`); overlaps zero-page GPA
/// because the two boot paths are mutually exclusive.
pub const MEMMAP_START: u64 = 0x7000;
/// Start of high usable RAM.
pub const HIMEM_START: u64 = 0x0010_0000;
/// EBDA-ish system-data window start (MP table, etc.).
pub const SYSTEM_MEM_START: u64 = 0x0009_fc00;
/// RSDP placeholder address; system reserved runs `[SYSTEM_MEM_START, RSDP_ADDR)`.
pub const RSDP_ADDR: u64 = 0x000e_0000;
pub const SYSTEM_MEM_SIZE: u64 = RSDP_ADDR - SYSTEM_MEM_START;
pub const IOAPIC_ADDR: u64 = 0xfec0_0000;
/// PCIe ECAM window reserved in e820/memmap (Firecracker layout).
pub const PCI_MMCONFIG_SIZE: u64 = 256 << 20;
pub const PCI_MMCONFIG_START: u64 = IOAPIC_ADDR - PCI_MMCONFIG_SIZE;
/// Three-page Intel VT-x quirk region (`Firecracker::KVM_TSS_ADDRESS`).
/// Must sit outside guest RAM / MMIO slots; required with in-kernel irqchip.
pub const KVM_TSS_ADDRESS: u64 = 0xfffb_d000;
/// One-page identity-mapped region for `KVM_SET_IDENTITY_MAP_ADDR`, placed
/// immediately below `KVM_TSS_ADDRESS` (Firecracker's `KVM_IDENTITY_MAP_START`
/// layout) so the two reserved regions sit contiguously outside guest RAM.
pub const KVM_IDENTITY_MAP_ADDRESS: u64 = KVM_TSS_ADDRESS - 0x1000;

pub struct GuestMemory {
    ptr: NonNull<u8>,
    len: usize,
    /// userfaultfd pager serving this mapping after a demand-paged restore
    /// (`FLUXVM_KVM_UFFD=1`). Shared by clones; stopped when the owning
    /// mapping is unmapped.
    #[cfg(target_os = "linux")]
    uffd: Option<std::sync::Arc<crate::uffd::UffdPager>>,
    /// Host-side writes (`write_at`, balloon discards) for incremental
    /// snapshots; off until the first full snapshot of a VM with
    /// `FLUXVM_KVM_DIRTY_LOG=1`. Shared by clones.
    host_dirty: std::sync::Arc<SoftwareDirtyBitmap>,
}

unsafe impl Send for GuestMemory {}
unsafe impl Sync for GuestMemory {}

impl GuestMemory {
    pub fn allocate(len: usize) -> Result<Self> {
        if len == 0 || !len.is_multiple_of(4096) {
            return Err(FluxError::Memory(
                "size must be non-zero and 4 KiB aligned".into(),
            ));
        }
        let dense = std::env::var("FLUXVM_KVM_LOCK_MEM").ok().as_deref() == Some("1");
        let mut flags = ffi::MAP_SHARED | ffi::MAP_ANONYMOUS;
        if dense {
            flags |= ffi::MAP_POPULATE;
        } else {
            flags |= ffi::MAP_NORESERVE;
        }
        // Firecracker-style optional hugepage backing.
        #[cfg(target_os = "linux")]
        if std::env::var("FLUXVM_HUGEPAGES").ok().as_deref() == Some("1") {
            flags |= libc::MAP_HUGETLB as i32;
        }
        let p = unsafe {
            ffi::mmap(
                std::ptr::null_mut(),
                len,
                ffi::PROT_READ | ffi::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };
        if p as usize == ffi::MAP_FAILED {
            return Err(FluxError::Memory("mmap guest ram failed".into()));
        }
        if dense {
            let _ = unsafe { ffi::mlock(p as *const c_void, len) };
        }
        Ok(Self {
            ptr: NonNull::new(p as *mut u8).unwrap(),
            len,
            #[cfg(target_os = "linux")]
            uffd: None,
            host_dirty: std::sync::Arc::new(SoftwareDirtyBitmap::new(len)),
        })
    }

    /// Restore path: replace the guest RAM mapping, in place, with a
    /// `MAP_PRIVATE` mapping of `file` (a snapshot's `snap.mem`). Pages are
    /// faulted from the page cache on first touch, so N children restored
    /// from one snapshot share every page none of them has written and only
    /// copy-on-write the ones they dirty. The host address does not change,
    /// so the KVM memslot and every `host_ptr()` copy stay valid.
    ///
    /// Must run before any vCPU or device thread touches RAM. Errors (and
    /// leaves the anonymous mapping untouched) when the file size differs or
    /// when mlock/hugepage backing was requested, since a file mapping can
    /// honour neither; callers fall back to an eager copy.
    ///
    /// The snapshot file must not be truncated or rewritten in place while a
    /// child runs (that would SIGBUS the guest). Unlinking it is fine.
    #[cfg(target_os = "linux")]
    pub fn remap_private_file(&mut self, file: &std::fs::File) -> Result<()> {
        use std::os::fd::AsRawFd;
        let flag = |name: &str| std::env::var(name).ok().as_deref() == Some("1");
        if flag("FLUXVM_KVM_LOCK_MEM") || flag("FLUXVM_HUGEPAGES") {
            return Err(FluxError::Memory(
                "file-backed restore is incompatible with mlock/hugepage guest RAM".into(),
            ));
        }
        if flag("FLUXVM_KVM_EAGER_RESTORE") {
            return Err(FluxError::Memory(
                "FLUXVM_KVM_EAGER_RESTORE=1 forces the eager copy".into(),
            ));
        }
        let size = file
            .metadata()
            .map_err(|e| FluxError::Memory(e.to_string()))?
            .len();
        if size != self.len as u64 {
            return Err(FluxError::Memory(format!(
                "snapshot RAM size {size} does not match guest RAM {}",
                self.len
            )));
        }
        let p = unsafe {
            libc::mmap(
                self.ptr.as_ptr() as *mut libc::c_void,
                self.len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_FIXED | libc::MAP_NORESERVE,
                file.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED || p as usize != self.ptr.as_ptr() as usize {
            return Err(FluxError::Memory(format!(
                "mmap snapshot RAM failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Demand-paged restore via userfaultfd (Linux only).
    ///
    /// Opt-in with `FLUXVM_KVM_UFFD=1` (see `kvm_snap::load_memory_into`).
    /// Replaces the guest RAM mapping, in place (`MAP_FIXED`, same host
    /// address, so the KVM memslot and `host_ptr()` copies stay valid), with
    /// an empty anonymous private mapping, registers it with userfaultfd in
    /// MISSING mode and starts a handler thread that answers each fault with
    /// `UFFDIO_COPY` from `snap_file` (`UFFDIO_ZEROPAGE` for zero windows),
    /// prefetching `FLUXVM_KVM_UFFD_PREFETCH_PAGES` pages (default 16).
    /// Faults KVM takes on guest RAM go through the same handler.
    ///
    /// Must run before any vCPU or device thread touches RAM. Every check
    /// that can fail without side effects (env, size, `userfaultfd` /
    /// `UFFDIO_API`) runs before the mapping is replaced, so most errors
    /// leave RAM untouched; an error after the remap leaves an empty
    /// anonymous mapping that the caller's fallback (`remap_private_file`
    /// or the eager copy) overwrites completely. The handler lives as long
    /// as this mapping. Like the `MAP_PRIVATE` path, `snap_file` must not be
    /// truncated or rewritten while the guest runs.
    #[cfg(target_os = "linux")]
    pub fn setup_uffd_restore(&mut self, snap_file: &std::fs::File) -> Result<()> {
        use crate::uffd::{PagerConfig, UffdPager, Userfaultfd};
        let flag = |name: &str| std::env::var(name).ok().as_deref() == Some("1");
        if flag("FLUXVM_KVM_LOCK_MEM") || flag("FLUXVM_HUGEPAGES") {
            return Err(FluxError::Memory(
                "userfaultfd restore is incompatible with mlock/hugepage guest RAM".into(),
            ));
        }
        if flag("FLUXVM_KVM_EAGER_RESTORE") {
            return Err(FluxError::Memory(
                "FLUXVM_KVM_EAGER_RESTORE=1 forces the eager copy".into(),
            ));
        }
        if self.uffd.is_some() {
            return Err(FluxError::Memory(
                "guest RAM already has a userfaultfd pager".into(),
            ));
        }
        let size = snap_file
            .metadata()
            .map_err(|e| FluxError::Memory(e.to_string()))?
            .len();
        if size != self.len as u64 {
            return Err(FluxError::Memory(format!(
                "snapshot RAM size {size} does not match guest RAM {}",
                self.len
            )));
        }
        // Kernel-mode faults are required: KVM populates guest RAM through
        // get_user_pages, never from user mode.
        let uffd = Userfaultfd::open(false).map_err(|e| FluxError::Memory(e.to_string()))?;
        // A fresh mapping guarantees every page is missing (anything the
        // boot path wrote before the restore is discarded).
        let p = unsafe {
            libc::mmap(
                self.ptr.as_ptr() as *mut libc::c_void,
                self.len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED || !std::ptr::eq(p as *const u8, self.ptr.as_ptr()) {
            return Err(FluxError::Memory(format!(
                "mmap empty guest RAM for userfaultfd failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let pager = UffdPager::start(
            uffd,
            self.ptr.as_ptr() as u64,
            self.len as u64,
            snap_file,
            PagerConfig::from_env(),
        )
        .map_err(|e| FluxError::Memory(format!("userfaultfd register/handler: {e}")))?;
        self.uffd = Some(std::sync::Arc::new(pager));
        Ok(())
    }

    /// Counters of the userfaultfd pager, when this RAM was restored with one.
    #[cfg(target_os = "linux")]
    pub fn uffd_pager(&self) -> Option<&crate::uffd::UffdPager> {
        self.uffd.as_deref()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn setup_uffd_restore(&mut self, _snap_file: &std::fs::File) -> Result<()> {
        Err(FluxError::Memory("userfaultfd requires Linux".into()))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn remap_private_file(&mut self, _file: &std::fs::File) -> Result<()> {
        Err(FluxError::Memory("file-backed restore needs Linux".into()))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn host_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    pub fn write_at(&mut self, gpa: u64, data: &[u8]) -> Result<()> {
        let start = gpa as usize;
        let end = start
            .checked_add(data.len())
            .ok_or_else(|| FluxError::Memory("GPA overflow".into()))?;
        if end > self.len {
            return Err(FluxError::Memory(format!(
                "write GPA {gpa:#x}+{} past RAM {}",
                data.len(),
                self.len
            )));
        }
        self.as_slice_mut()[start..end].copy_from_slice(data);
        self.host_dirty.mark_range(gpa, data.len());
        Ok(())
    }

    /// Pages written from the host since tracking was enabled (see
    /// [`crate::dirty_log`]). Writes through `as_slice_mut()` or raw
    /// `host_ptr()` copies are not recorded.
    pub fn host_dirty(&self) -> &SoftwareDirtyBitmap {
        &self.host_dirty
    }

    pub fn read_at(&self, gpa: u64, buf: &mut [u8]) -> Result<()> {
        let start = usize::try_from(gpa)
            .map_err(|_| FluxError::Memory("GPA does not fit host address space".into()))?;
        let end = start
            .checked_add(buf.len())
            .ok_or_else(|| FluxError::Memory("GPA overflow".into()))?;
        if end > self.len {
            return Err(FluxError::Memory(format!(
                "read GPA {gpa:#x}+{} past RAM {}",
                buf.len(),
                self.len
            )));
        }
        buf.copy_from_slice(&self.as_slice()[start..end]);
        Ok(())
    }

    pub fn read_u16(&self, gpa: u64) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read_at(gpa, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn write_u16(&mut self, gpa: u64, v: u16) -> Result<()> {
        self.write_at(gpa, &v.to_le_bytes())
    }

    /// Firecracker balloon inflate: discard host backing for a guest page.
    pub fn advise_dontneed(&self, gpa: u64, len: usize) {
        let start = gpa as usize;
        if start.saturating_add(len) > self.len {
            return;
        }
        // The discarded range reads back as zeros (or, after a uffd restore,
        // as the snapshot again): either way it changed under KVM's log.
        self.host_dirty.mark_range(gpa, len);
        #[cfg(target_os = "linux")]
        unsafe {
            let _ = libc::madvise(
                self.ptr.as_ptr().add(start) as *mut libc::c_void,
                len,
                libc::MADV_DONTNEED,
            );
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (start, len);
    }

    /// Balloon deflate: force page present again.
    pub fn touch_page(&self, gpa: u64) {
        let start = gpa as usize;
        if start + 1 > self.len {
            return;
        }
        unsafe {
            let p = self.ptr.as_ptr().add(start);
            std::ptr::read_volatile(p);
        }
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        // Stop the fault handler before its region disappears.
        #[cfg(target_os = "linux")]
        if let Some(pager) = self.uffd.take() {
            pager.shutdown();
        }
        unsafe {
            ffi::munmap(self.ptr.as_ptr() as *mut c_void, self.len);
        }
    }
}

impl Clone for GuestMemory {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr,
            len: self.len,
            #[cfg(target_os = "linux")]
            uffd: self.uffd.clone(),
            host_dirty: std::sync::Arc::clone(&self.host_dirty),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn read_at_rejects_overflowing_guest_address() {
        let mem = GuestMemory::allocate(4096).unwrap();
        let mut buf = [0u8; 16];
        assert!(mem.read_at(u64::MAX, &mut buf).is_err());
    }

    /// With `FLUXVM_KVM_UFFD=1` restore either serves the snapshot through
    /// userfaultfd or refuses (no kernel-fault uffd for unprivileged
    /// processes when `vm.unprivileged_userfaultfd=0`). Either way the
    /// contents the caller ends up with must be right: a refusal must leave
    /// RAM untouched so the `MAP_PRIVATE` / eager fallback can run.
    #[test]
    fn uffd_restore_serves_snapshot_or_leaves_ram_untouched() {
        use std::io::Write;
        let len = 16 * 4096;
        let mut snap = vec![0u8; len];
        for (i, b) in snap.iter_mut().enumerate() {
            *b = (i / 4096) as u8 ^ 0x5a;
        }
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&snap).unwrap();
        let mut mem = GuestMemory::allocate(len).unwrap();
        mem.write_at(0, &[0x11; 4096]).unwrap();
        match mem.setup_uffd_restore(&f) {
            Ok(()) => {
                assert_eq!(mem.as_slice(), &snap[..]);
                assert!(mem.uffd_pager().is_some());
            }
            Err(e) => {
                eprintln!("uffd restore refused (fallback path): {e}");
                assert!(mem.uffd_pager().is_none());
                assert_eq!(&mem.as_slice()[..4096], &[0x11; 4096]);
                mem.remap_private_file(&f).unwrap();
                assert_eq!(mem.as_slice(), &snap[..]);
            }
        }
    }

    #[test]
    fn uffd_restore_rejects_size_mismatch_without_touching_ram() {
        use std::io::Write;
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[1u8; 4096]).unwrap();
        let mut mem = GuestMemory::allocate(2 * 4096).unwrap();
        mem.write_at(0, &[9u8; 8]).unwrap();
        assert!(mem.setup_uffd_restore(&f).is_err());
        assert_eq!(&mem.as_slice()[..8], &[9u8; 8]);
    }
}
