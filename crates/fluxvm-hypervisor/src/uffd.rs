// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! userfaultfd demand-paged restore (Linux only).
//!
//! Design from `docs/ROADMAP-DENSITY.md`: guest RAM stays an anonymous
//! private mapping with no pages populated, the whole range is registered
//! with `UFFDIO_REGISTER_MODE_MISSING`, and a handler thread answers every
//! missing-page fault with `UFFDIO_COPY` from the snapshot's `snap.mem` (or
//! `UFFDIO_ZEROPAGE` for an all-zero window). Faults raised by KVM itself
//! (EPT violations resolved through `get_user_pages`) take the same path,
//! so no KVM changes are needed.
//!
//! Only `UFFD_EVENT_PAGEFAULT` is requested (no fork/remap/remove events),
//! so the handler never has to track a changing address space.
//!
//! The kernel ABI is bound locally (`linux/userfaultfd.h`); the ioctl numbers
//! and struct sizes are checked in the unit tests.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

// ---- linux/userfaultfd.h ----------------------------------------------

/// `UFFD_API`: the only API version the kernel has ever shipped.
pub const UFFD_API: u64 = 0xAA;
const UFFDIO: u64 = 0xAA;

const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (UFFDIO << 8) | nr
}

const _UFFDIO_REGISTER: u64 = 0x00;
const _UFFDIO_UNREGISTER: u64 = 0x01;
const _UFFDIO_WAKE: u64 = 0x02;
const _UFFDIO_COPY: u64 = 0x03;
const _UFFDIO_ZEROPAGE: u64 = 0x04;
const _UFFDIO_API: u64 = 0x3F;

pub const UFFDIO_API: u64 = ioc(IOC_READ | IOC_WRITE, _UFFDIO_API, 24);
pub const UFFDIO_REGISTER: u64 = ioc(IOC_READ | IOC_WRITE, _UFFDIO_REGISTER, 32);
pub const UFFDIO_UNREGISTER: u64 = ioc(IOC_READ, _UFFDIO_UNREGISTER, 16);
pub const UFFDIO_WAKE: u64 = ioc(IOC_READ, _UFFDIO_WAKE, 16);
pub const UFFDIO_COPY: u64 = ioc(IOC_READ | IOC_WRITE, _UFFDIO_COPY, 40);
pub const UFFDIO_ZEROPAGE: u64 = ioc(IOC_READ | IOC_WRITE, _UFFDIO_ZEROPAGE, 32);
/// `/dev/userfaultfd` `USERFAULTFD_IOC_NEW` (`_IO(0xAA, 0x00)`, Linux 6.1+).
pub const USERFAULTFD_IOC_NEW: u64 = 0xAA00;

pub const UFFDIO_REGISTER_MODE_MISSING: u64 = 1;
pub const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
/// Only handle faults raised from user mode (allowed with
/// `vm.unprivileged_userfaultfd=0`). Useless for KVM, which faults guest
/// RAM in from kernel mode, so the restore path never sets it.
pub const UFFD_USER_MODE_ONLY: i32 = 1;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UffdioApi {
    pub api: u64,
    pub features: u64,
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UffdioRange {
    pub start: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UffdioRegister {
    pub range: UffdioRange,
    pub mode: u64,
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UffdioCopy {
    pub dst: u64,
    pub src: u64,
    pub len: u64,
    pub mode: u64,
    pub copy: i64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UffdioZeropage {
    pub range: UffdioRange,
    pub mode: u64,
    pub zeropage: i64,
}

/// `struct uffd_msg` (32 bytes). For `UFFD_EVENT_PAGEFAULT` the argument
/// union is `{ flags: u64, address: u64, ptid: u32 }`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct UffdMsg {
    pub event: u8,
    pub reserved1: u8,
    pub reserved2: u16,
    pub reserved3: u32,
    pub arg: [u64; 3],
}

impl UffdMsg {
    pub fn pagefault_address(&self) -> u64 {
        self.arg[1]
    }
}

/// Host page size (always 4 KiB on the x86_64 hosts this VMM runs on, but
/// asked rather than assumed).
pub fn page_size() -> usize {
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ps > 0 {
        ps as usize
    } else {
        4096
    }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ---- the uffd itself ----------------------------------------------------

/// An open, API-negotiated userfaultfd.
#[derive(Debug)]
pub struct Userfaultfd {
    fd: OwnedFd,
    zeropage: AtomicBool,
}

impl Userfaultfd {
    /// Create a non-blocking, close-on-exec userfaultfd and complete the
    /// `UFFDIO_API` handshake.
    ///
    /// Tries the `userfaultfd(2)` syscall first and then `/dev/userfaultfd`
    /// (`USERFAULTFD_IOC_NEW`), which is how an unprivileged process gets a
    /// kernel-fault-capable uffd when `vm.unprivileged_userfaultfd=0` and
    /// the device node is accessible. `user_mode_only` sets
    /// `UFFD_USER_MODE_ONLY` (tests only; KVM needs kernel faults).
    pub fn open(user_mode_only: bool) -> io::Result<Self> {
        let mut flags = libc::O_CLOEXEC | libc::O_NONBLOCK;
        if user_mode_only {
            flags |= UFFD_USER_MODE_ONLY;
        }
        let raw = unsafe { libc::syscall(libc::SYS_userfaultfd, flags) } as RawFd;
        let fd = if raw >= 0 {
            unsafe { OwnedFd::from_raw_fd(raw) }
        } else {
            let sys_err = io::Error::last_os_error();
            match Self::open_dev(flags) {
                Ok(fd) => fd,
                Err(dev_err) => {
                    return Err(io::Error::new(
                        sys_err.kind(),
                        format!(
                            "userfaultfd syscall: {sys_err}; /dev/userfaultfd: {dev_err} \
                             (needs vm.unprivileged_userfaultfd=1, CAP_SYS_PTRACE, or access \
                             to /dev/userfaultfd)"
                        ),
                    ))
                }
            }
        };
        let mut api = UffdioApi {
            api: UFFD_API,
            features: 0,
            ioctls: 0,
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_API as _, &mut api) } < 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("UFFDIO_API: {}", io::Error::last_os_error()),
            ));
        }
        let need = (1u64 << _UFFDIO_REGISTER) | (1u64 << _UFFDIO_UNREGISTER);
        if api.ioctls & need != need {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "UFFDIO_API: register/unregister missing (ioctls {:#x})",
                    api.ioctls
                ),
            ));
        }
        Ok(Self {
            fd,
            zeropage: AtomicBool::new(false),
        })
    }

    fn open_dev(flags: i32) -> io::Result<OwnedFd> {
        let dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/userfaultfd")?;
        let raw = unsafe { libc::ioctl(dev.as_raw_fd(), USERFAULTFD_IOC_NEW as _, flags) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// `UFFDIO_REGISTER` `[start, start+len)` in MISSING mode. Fails unless
    /// the kernel offers `UFFDIO_COPY` and `UFFDIO_WAKE` on the range.
    pub fn register_missing(&self, start: u64, len: u64) -> io::Result<()> {
        let mut reg = UffdioRegister {
            range: UffdioRange { start, len },
            mode: UFFDIO_REGISTER_MODE_MISSING,
            ioctls: 0,
        };
        if unsafe { libc::ioctl(self.as_raw_fd(), UFFDIO_REGISTER as _, &mut reg) } < 0 {
            return Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!("UFFDIO_REGISTER: {}", io::Error::last_os_error()),
            ));
        }
        let need = (1u64 << _UFFDIO_COPY) | (1u64 << _UFFDIO_WAKE);
        if reg.ioctls & need != need {
            let _ = self.unregister(start, len);
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "UFFDIO_REGISTER: copy/wake unsupported (ioctls {:#x})",
                    reg.ioctls
                ),
            ));
        }
        self.zeropage.store(
            reg.ioctls & (1u64 << _UFFDIO_ZEROPAGE) != 0,
            Ordering::Relaxed,
        );
        Ok(())
    }

    pub fn unregister(&self, start: u64, len: u64) -> io::Result<()> {
        let mut range = UffdioRange { start, len };
        if unsafe { libc::ioctl(self.as_raw_fd(), UFFDIO_UNREGISTER as _, &mut range) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Wake threads blocked on faults in `[start, start+len)`.
    pub fn wake(&self, start: u64, len: u64) -> io::Result<()> {
        let mut range = UffdioRange { start, len };
        if unsafe { libc::ioctl(self.as_raw_fd(), UFFDIO_WAKE as _, &mut range) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn supports_zeropage(&self) -> bool {
        self.zeropage.load(Ordering::Relaxed)
    }

    /// `UFFDIO_COPY` `len` bytes from `src` to `dst` (both page aligned)
    /// and wake the faulting threads. Retries `EAGAIN` (the address space
    /// changed under the copy) from where the kernel stopped. Returns the
    /// raw errno on failure, with `EEXIST` meaning some page in the range
    /// was already present.
    pub fn copy(&self, dst: u64, src: *const u8, len: u64) -> Result<(), i32> {
        let mut done = 0u64;
        let mut eagain = 0u32;
        while done < len {
            let mut c = UffdioCopy {
                dst: dst + done,
                src: src as u64 + done,
                len: len - done,
                mode: 0,
                copy: 0,
            };
            let r = unsafe { libc::ioctl(self.as_raw_fd(), UFFDIO_COPY as _, &mut c) };
            if r == 0 {
                return Ok(());
            }
            let e = errno();
            if c.copy > 0 {
                done += c.copy as u64;
            }
            if e == libc::EAGAIN && eagain < 1000 {
                eagain += 1;
                std::thread::yield_now();
                continue;
            }
            return Err(e);
        }
        Ok(())
    }

    /// `UFFDIO_ZEROPAGE` `[dst, dst+len)` and wake the faulting threads.
    pub fn zeropage(&self, dst: u64, len: u64) -> Result<(), i32> {
        let mut eagain = 0u32;
        loop {
            let mut z = UffdioZeropage {
                range: UffdioRange { start: dst, len },
                mode: 0,
                zeropage: 0,
            };
            let r = unsafe { libc::ioctl(self.as_raw_fd(), UFFDIO_ZEROPAGE as _, &mut z) };
            if r == 0 {
                return Ok(());
            }
            let e = errno();
            if e == libc::EAGAIN && eagain < 1000 {
                eagain += 1;
                std::thread::yield_now();
                continue;
            }
            return Err(e);
        }
    }
}

// ---- the fault handler thread ---------------------------------------------

/// Counters for one pager, readable while it runs.
#[derive(Debug, Default)]
pub struct UffdStats {
    /// `UFFD_EVENT_PAGEFAULT` messages handled.
    pub faults: AtomicU64,
    /// Pages populated with `UFFDIO_COPY`.
    pub pages_copied: AtomicU64,
    /// Pages populated with `UFFDIO_ZEROPAGE`.
    pub pages_zeroed: AtomicU64,
    /// Faults that raced with an already-present page (`EEXIST`).
    pub already_present: AtomicU64,
    /// Faults that could not be resolved (I/O error, unexpected errno).
    pub errors: AtomicU64,
}

/// Where a pager reads pages from and how much it prefetches.
pub struct PagerConfig {
    /// Offset in the file of the first byte of the registered region.
    pub file_offset: u64,
    /// Pages populated per fault, starting at the faulting page (clamped to
    /// the region). `1` disables prefetch.
    pub prefetch_pages: usize,
}

impl Default for PagerConfig {
    fn default() -> Self {
        Self {
            file_offset: 0,
            prefetch_pages: 16,
        }
    }
}

impl PagerConfig {
    /// `FLUXVM_KVM_UFFD_PREFETCH_PAGES` (default 16, clamped to 1..=512).
    pub fn from_env() -> Self {
        let prefetch_pages = std::env::var("FLUXVM_KVM_UFFD_PREFETCH_PAGES")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(16)
            .clamp(1, 512);
        Self {
            file_offset: 0,
            prefetch_pages,
        }
    }
}

struct PagerShared {
    uffd: Userfaultfd,
    file: File,
    base: u64,
    len: u64,
    file_len: u64,
    page: u64,
    cfg: PagerConfig,
    stats: UffdStats,
}

/// A registered region plus the thread that serves its missing-page faults
/// from a file. Dropping it (or [`UffdPager::shutdown`]) stops the thread;
/// the region must stay mapped until then.
pub struct UffdPager {
    shared: Arc<PagerShared>,
    stop: OwnedFd,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for UffdPager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UffdPager")
            .field("base", &format_args!("{:#x}", self.shared.base))
            .field("len", &self.shared.len)
            .finish()
    }
}

impl UffdPager {
    /// Register `[base, base+len)` on `uffd` and start the handler thread.
    ///
    /// The region must be a mapping with no populated pages (a fresh
    /// anonymous private mapping), page aligned, and must not be touched by
    /// anyone before this returns. `file` is read with `pread`, so the
    /// caller may close its own handle.
    pub fn start(
        uffd: Userfaultfd,
        base: u64,
        len: u64,
        file: &File,
        cfg: PagerConfig,
    ) -> io::Result<Self> {
        let page = page_size() as u64;
        if !base.is_multiple_of(page) || !len.is_multiple_of(page) || len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "uffd region must be page aligned and non-empty",
            ));
        }
        let file = file.try_clone()?;
        let file_len = file.metadata()?.len();
        let stop_raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if stop_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let stop = unsafe { OwnedFd::from_raw_fd(stop_raw) };
        uffd.register_missing(base, len)?;
        let shared = Arc::new(PagerShared {
            uffd,
            file,
            base,
            len,
            file_len,
            page,
            cfg,
            stats: UffdStats::default(),
        });
        let thread_shared = Arc::clone(&shared);
        let stop_fd = stop.as_raw_fd();
        let thread = std::thread::Builder::new()
            .name("fluxvm-uffd".into())
            .spawn(move || run_handler(&thread_shared, stop_fd));
        let thread = match thread {
            Ok(t) => t,
            Err(e) => {
                let _ = shared.uffd.unregister(base, len);
                return Err(e);
            }
        };
        Ok(Self {
            shared,
            stop,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub fn stats(&self) -> &UffdStats {
        &self.shared.stats
    }

    /// Stop the handler thread and wait for it. Idempotent. Any thread still
    /// blocked on a fault in the region stays blocked until the region is
    /// unmapped, so call this only when the VM is gone (or from `Drop`
    /// right before the mapping goes away).
    pub fn shutdown(&self) {
        let one: u64 = 1;
        unsafe {
            libc::write(
                self.stop.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            );
        }
        let handle = self.thread.lock().ok().and_then(|mut t| t.take());
        if let Some(h) = handle {
            if h.thread().id() != std::thread::current().id() {
                let _ = h.join();
            }
        }
    }
}

impl Drop for UffdPager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_handler(s: &PagerShared, stop_fd: RawFd) {
    let page = s.page as usize;
    let window = s.cfg.prefetch_pages.max(1) * page;
    // Page-aligned bounce buffer for UFFDIO_COPY sources.
    let buf = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            window,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if buf == libc::MAP_FAILED {
        eprintln!(
            "[uffd] bounce buffer mmap failed: {}; faults will not be served",
            io::Error::last_os_error()
        );
        return;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, window) };
    let mut msgs = [UffdMsg::default(); 16];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: s.uffd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            eprintln!("[uffd] poll: {}", io::Error::last_os_error());
            break;
        }
        if fds[1].revents != 0 {
            break;
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            eprintln!(
                "[uffd] fd closed or errored (revents {:#x})",
                fds[0].revents
            );
            break;
        }
        if fds[0].revents & libc::POLLIN == 0 {
            continue;
        }
        let n = unsafe {
            libc::read(
                s.uffd.as_raw_fd(),
                msgs.as_mut_ptr() as *mut libc::c_void,
                std::mem::size_of_val(&msgs),
            )
        };
        if n < 0 {
            match errno() {
                libc::EAGAIN | libc::EINTR => continue,
                _ => {
                    eprintln!("[uffd] read: {}", io::Error::last_os_error());
                    break;
                }
            }
        }
        let count = n as usize / std::mem::size_of::<UffdMsg>();
        let mut exiting = false;
        for msg in &msgs[..count] {
            if msg.event != UFFD_EVENT_PAGEFAULT {
                // Not requested at UFFDIO_API time; nothing to do.
                eprintln!("[uffd] ignoring unexpected event {:#x}", msg.event);
                continue;
            }
            s.stats.faults.fetch_add(1, Ordering::Relaxed);
            if !serve_fault(s, msg.pagefault_address(), buf) {
                exiting = true;
                break;
            }
        }
        if exiting {
            break;
        }
    }
    unsafe {
        libc::munmap(buf.as_mut_ptr() as *mut libc::c_void, window);
    }
}

/// Page range `[start, start+len)` (host addresses) to populate for a fault
/// at `addr`: the faulting page plus up to `prefetch_pages - 1` following
/// pages, clamped to the registered region. `None` if `addr` is outside it.
pub fn fault_window(
    addr: u64,
    base: u64,
    region_len: u64,
    page: u64,
    prefetch_pages: usize,
) -> Option<(u64, u64)> {
    if addr < base || addr >= base + region_len {
        return None;
    }
    let start = addr & !(page - 1);
    let want = page * prefetch_pages.max(1) as u64;
    let len = want.min(base + region_len - start);
    Some((start, len))
}

/// Read `buf.len()` bytes at `offset`, zero-filling past end of file.
fn read_window(file: &File, file_len: u64, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let avail = file_len.saturating_sub(offset).min(buf.len() as u64) as usize;
    let (data, tail) = buf.split_at_mut(avail);
    if !data.is_empty() {
        file.read_exact_at(data, offset)?;
    }
    tail.fill(0);
    Ok(())
}

/// Resolve one fault. Returns `false` when the handler should exit
/// (the faulting process is going away).
fn serve_fault(s: &PagerShared, addr: u64, buf: &mut [u8]) -> bool {
    let Some((start, len)) = fault_window(addr, s.base, s.len, s.page, s.cfg.prefetch_pages) else {
        eprintln!("[uffd] fault at {addr:#x} outside registered region");
        s.stats.errors.fetch_add(1, Ordering::Relaxed);
        return true;
    };
    let fault_page = addr & !(s.page - 1);
    let window = &mut buf[..len as usize];
    let offset = s.cfg.file_offset + (start - s.base);
    let mut tries = 0;
    while let Err(e) = read_window(&s.file, s.file_len, offset, window) {
        tries += 1;
        if tries >= 3 {
            // Leave the fault unresolved rather than hand the guest
            // wrong contents; the faulting thread stays blocked.
            eprintln!("[uffd] reading snapshot at {offset:#x}: {e}; fault at {addr:#x} not served");
            s.stats.errors.fetch_add(1, Ordering::Relaxed);
            return true;
        }
    }
    let all_zero = window.iter().all(|b| *b == 0);
    let r = if all_zero && s.uffd.supports_zeropage() {
        s.uffd.zeropage(start, len).map(|()| {
            s.stats
                .pages_zeroed
                .fetch_add(len / s.page, Ordering::Relaxed);
        })
    } else {
        s.uffd.copy(start, window.as_ptr(), len).map(|()| {
            s.stats
                .pages_copied
                .fetch_add(len / s.page, Ordering::Relaxed);
        })
    };
    match r {
        Ok(()) => true,
        Err(libc::EEXIST) => {
            // Part of the prefetch window was already present (an earlier
            // fault's window, or a racing fault). Serve just the faulting
            // page; if that is present too, only a wake is needed.
            let page_off = (fault_page - start) as usize;
            let page_len = s.page as usize;
            let src = window[page_off..page_off + page_len].as_ptr();
            match s.uffd.copy(fault_page, src, s.page) {
                Ok(()) => {
                    s.stats.pages_copied.fetch_add(1, Ordering::Relaxed);
                }
                Err(libc::EEXIST) => {
                    s.stats.already_present.fetch_add(1, Ordering::Relaxed);
                    let _ = s.uffd.wake(fault_page, s.page);
                }
                Err(libc::ESRCH) => return false,
                Err(e) => {
                    eprintln!(
                        "[uffd] UFFDIO_COPY {fault_page:#x}: {}",
                        io::Error::from_raw_os_error(e)
                    );
                    s.stats.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            true
        }
        // The faulting process is exiting.
        Err(libc::ESRCH) => false,
        // Region unmapped/unregistered under us (teardown): nothing to serve.
        Err(libc::ENOENT) => {
            let _ = s.uffd.wake(fault_page, s.page);
            true
        }
        Err(e) => {
            eprintln!(
                "[uffd] populating {start:#x}+{len:#x}: {}",
                io::Error::from_raw_os_error(e)
            );
            s.stats.errors.fetch_add(1, Ordering::Relaxed);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ioctl_numbers_match_linux_headers() {
        // Values from <linux/userfaultfd.h> on x86_64.
        assert_eq!(UFFDIO_API, 0xc018_aa3f);
        assert_eq!(UFFDIO_REGISTER, 0xc020_aa00);
        assert_eq!(UFFDIO_UNREGISTER, 0x8010_aa01);
        assert_eq!(UFFDIO_WAKE, 0x8010_aa02);
        assert_eq!(UFFDIO_COPY, 0xc028_aa03);
        assert_eq!(UFFDIO_ZEROPAGE, 0xc020_aa04);
        assert_eq!(std::mem::size_of::<UffdioApi>(), 24);
        assert_eq!(std::mem::size_of::<UffdioRegister>(), 32);
        assert_eq!(std::mem::size_of::<UffdioCopy>(), 40);
        assert_eq!(std::mem::size_of::<UffdioZeropage>(), 32);
        assert_eq!(std::mem::size_of::<UffdMsg>(), 32);
    }

    #[test]
    fn fault_window_clamps_to_region() {
        let base = 0x10_0000;
        let len = 8 * 4096;
        assert_eq!(fault_window(base - 1, base, len, 4096, 4), None);
        assert_eq!(fault_window(base + len, base, len, 4096, 4), None);
        assert_eq!(
            fault_window(base + 123, base, len, 4096, 4),
            Some((base, 4 * 4096))
        );
        // Window starting at page 6 only has two pages left.
        assert_eq!(
            fault_window(base + 6 * 4096 + 7, base, len, 4096, 4),
            Some((base + 6 * 4096, 2 * 4096))
        );
        // prefetch 0 behaves as 1.
        assert_eq!(
            fault_window(base + 4096, base, len, 4096, 0),
            Some((base + 4096, 4096))
        );
    }

    #[test]
    fn read_window_zero_fills_past_eof() {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&[7u8; 10]).unwrap();
        let mut buf = [0xffu8; 16];
        read_window(&f, 10, 4, &mut buf).unwrap();
        assert_eq!(&buf[..6], &[7u8; 6]);
        assert_eq!(&buf[6..], &[0u8; 10]);
    }

    /// Anonymous private mapping with no pages populated.
    struct Region {
        ptr: *mut u8,
        len: usize,
    }

    impl Region {
        fn new(len: usize) -> Self {
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                    -1,
                    0,
                )
            };
            assert_ne!(p, libc::MAP_FAILED);
            Self {
                ptr: p as *mut u8,
                len,
            }
        }
    }

    impl Drop for Region {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
            }
        }
    }

    /// User-mode-only uffd, which works even with
    /// `vm.unprivileged_userfaultfd=0`. `None` (test skipped) when the
    /// kernel or sandbox forbids userfaultfd entirely.
    fn test_uffd() -> Option<Userfaultfd> {
        match Userfaultfd::open(true) {
            Ok(u) => Some(u),
            Err(e) => {
                eprintln!("skipping: userfaultfd unavailable: {e}");
                None
            }
        }
    }

    fn snapshot_file(pages: usize) -> (File, Vec<u8>) {
        let page = page_size();
        let mut data = vec![0u8; pages * page];
        for (i, chunk) in data.chunks_mut(page).enumerate() {
            // Leave every third page zero to exercise UFFDIO_ZEROPAGE.
            if i % 3 != 2 {
                chunk.fill((i as u8).wrapping_mul(31).wrapping_add(1));
                chunk[..8].copy_from_slice(&(i as u64).to_le_bytes());
            }
        }
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&data).unwrap();
        (f, data)
    }

    #[test]
    fn serves_faults_from_file() {
        let Some(uffd) = test_uffd() else { return };
        let pages = 64;
        let page = page_size();
        let (file, data) = snapshot_file(pages);
        let region = Region::new(pages * page);
        let pager = UffdPager::start(
            uffd,
            region.ptr as u64,
            region.len as u64,
            &file,
            PagerConfig {
                file_offset: 0,
                prefetch_pages: 4,
            },
        )
        .unwrap();
        // Touch from another thread too, so faults race.
        let addr = region.ptr as usize;
        let len = region.len;
        let other = std::thread::spawn(move || {
            let s = unsafe { std::slice::from_raw_parts(addr as *const u8, len) };
            s.iter().rev().step_by(4096).map(|b| *b as u64).sum::<u64>()
        });
        let got = unsafe { std::slice::from_raw_parts(region.ptr, region.len) };
        assert!(got == &data[..], "restored contents differ from snapshot");
        other.join().unwrap();
        let st = pager.stats();
        assert!(st.faults.load(Ordering::Relaxed) > 0);
        // Racing faults can resolve part of a window through the other
        // thread's copy, so this is an upper bound, not an equality.
        let populated =
            st.pages_copied.load(Ordering::Relaxed) + st.pages_zeroed.load(Ordering::Relaxed);
        assert!(populated > 0 && populated <= pages as u64, "{populated}");
        assert_eq!(st.errors.load(Ordering::Relaxed), 0);
        pager.shutdown();
        pager.shutdown(); // idempotent
    }

    #[test]
    fn serves_from_file_offset_and_handles_present_pages() {
        let Some(uffd) = test_uffd() else { return };
        let page = page_size();
        let pages = 8;
        let (file, data) = snapshot_file(pages + 2);
        let region = Region::new(pages * page);
        // Populate page 1 before any fault: page 0's 4-page prefetch
        // window then hits EEXIST and must fall back to page 0 alone.
        let pager = UffdPager::start(
            uffd,
            region.ptr as u64,
            region.len as u64,
            &file,
            PagerConfig {
                file_offset: 2 * page as u64,
                prefetch_pages: 4,
            },
        )
        .unwrap();
        let marker = vec![0xabu8; page];
        // Writes go through a fault too; populate page 1 via the pager's
        // own uffd copy path to make it present with known content.
        pager
            .shared
            .uffd
            .copy(
                region.ptr as u64 + page as u64,
                marker.as_ptr(),
                page as u64,
            )
            .unwrap();
        let got = unsafe { std::slice::from_raw_parts(region.ptr, region.len) };
        assert_eq!(&got[..page], &data[2 * page..3 * page]);
        assert_eq!(&got[page..2 * page], &marker[..]);
        assert_eq!(&got[2 * page..], &data[4 * page..(pages + 2) * page]);
        assert_eq!(pager.stats().errors.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shutdown_is_prompt_with_no_faults() {
        let Some(uffd) = test_uffd() else { return };
        let (file, _) = snapshot_file(4);
        let region = Region::new(4 * page_size());
        let pager = UffdPager::start(
            uffd,
            region.ptr as u64,
            region.len as u64,
            &file,
            PagerConfig::default(),
        )
        .unwrap();
        let t = std::time::Instant::now();
        drop(pager);
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn rejects_unaligned_region() {
        let Some(uffd) = test_uffd() else { return };
        let (file, _) = snapshot_file(1);
        let err =
            UffdPager::start(uffd, 4096 + 1, 4096, &file, PagerConfig::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
