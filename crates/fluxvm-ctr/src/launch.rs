// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The launcher: turns a [`ContainerSpec`] into a running container on Linux.
//!
//! `run`: namespaces (mount, pid, ipc, uts, net, and user when unprivileged), cgroup v2 limits when no service manager
//! applied them, the image rootfs bind-mounted (read-only on request) with `/proc`, `/dev`, `/tmp` and `/run`,
//! `pivot_root`, dropped capabilities, `no_new_privs`, the seccomp filter, then the entrypoint as PID 1.
//! `exec`: joins a running container's namespaces and runs a command with the same restrictions.
//! The launcher is single-threaded on purpose: `setns(CLONE_NEWUSER)` requires it.

use crate::spec::{ContainerSpec, Net, Seccomp};
use anyhow::{Context, Result, bail};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

static CHILD: AtomicI32 = AtomicI32::new(0);
static SIGNAL_SEEN: AtomicI32 = AtomicI32::new(0);

fn cstr(p: &Path) -> Result<CString> {
    CString::new(p.as_os_str().as_bytes()).context("path contains a NUL byte")
}

fn errno() -> io::Error {
    io::Error::last_os_error()
}

fn mount(
    src: &str,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let s = CString::new(src).unwrap();
    let t = CString::new(target.as_os_str().as_bytes()).unwrap();
    let f = fstype.map(|f| CString::new(f).unwrap());
    let d = data.map(|d| CString::new(d).unwrap());
    let r = unsafe {
        libc::mount(
            s.as_ptr(),
            t.as_ptr(),
            f.as_ref().map_or(std::ptr::null(), |f| f.as_ptr()),
            flags,
            d.as_ref().map_or(std::ptr::null(), |d| d.as_ptr().cast()),
        )
    };
    if r == 0 { Ok(()) } else { Err(errno()) }
}

pub fn cap_number(name: &str) -> Option<u32> {
    const NAMES: &[&str] = &[
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_DAC_READ_SEARCH",
        "CAP_FOWNER",
        "CAP_FSETID",
        "CAP_KILL",
        "CAP_SETGID",
        "CAP_SETUID",
        "CAP_SETPCAP",
        "CAP_LINUX_IMMUTABLE",
        "CAP_NET_BIND_SERVICE",
        "CAP_NET_BROADCAST",
        "CAP_NET_ADMIN",
        "CAP_NET_RAW",
        "CAP_IPC_LOCK",
        "CAP_IPC_OWNER",
        "CAP_SYS_MODULE",
        "CAP_SYS_RAWIO",
        "CAP_SYS_CHROOT",
        "CAP_SYS_PTRACE",
        "CAP_SYS_PACCT",
        "CAP_SYS_ADMIN",
        "CAP_SYS_BOOT",
        "CAP_SYS_NICE",
        "CAP_SYS_RESOURCE",
        "CAP_SYS_TIME",
        "CAP_SYS_TTY_CONFIG",
        "CAP_MKNOD",
        "CAP_LEASE",
        "CAP_AUDIT_WRITE",
        "CAP_AUDIT_CONTROL",
        "CAP_SETFCAP",
        "CAP_MAC_OVERRIDE",
        "CAP_MAC_ADMIN",
        "CAP_SYSLOG",
        "CAP_WAKE_ALARM",
        "CAP_BLOCK_SUSPEND",
        "CAP_AUDIT_READ",
        "CAP_PERFMON",
        "CAP_BPF",
        "CAP_CHECKPOINT_RESTORE",
    ];
    NAMES.iter().position(|n| *n == name).map(|i| i as u32)
}

/// Drop every capability from the bounding set that `keep` does not list, then clear the rest for a non-root process.
fn restrict_capabilities(keep: &[String]) -> Result<()> {
    let last: u32 = fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(40);
    let keep: Vec<u32> = keep
        .iter()
        .map(|c| cap_number(c).with_context(|| format!("unknown capability {c}")))
        .collect::<Result<_>>()?;
    for cap in 0..=last {
        if !keep.contains(&cap)
            && unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) } != 0
        {
            let e = errno();
            // EINVAL: this kernel has fewer capabilities than the table.
            if e.raw_os_error() != Some(libc::EINVAL) {
                bail!("dropping capability {cap} from the bounding set: {e}");
            }
        }
    }
    Ok(())
}

fn apply_seccomp(spec: &ContainerSpec) -> Result<()> {
    if spec.hardening.seccomp == Seccomp::Unconfined {
        return Ok(());
    }
    let progs = fluxvm_procbox::seccomp::compile(fluxvm_procbox::SeccompMode::Errno, false)?;
    fluxvm_procbox::seccomp::apply(&progs).context("installing the seccomp filter")
}

fn parse_user(user: &str) -> Option<(u32, u32)> {
    if user.is_empty() {
        return None;
    }
    let mut it = user.split(':');
    let uid: u32 = it.next()?.parse().ok()?;
    let gid: u32 = it.next().map_or(Some(uid), |g| g.parse().ok())?;
    Some((uid, gid))
}

fn drop_identity(spec: &ContainerSpec, rootless: bool) -> Result<()> {
    let Some((uid, gid)) = parse_user(&spec.user) else {
        return Ok(());
    };
    if rootless && (uid != 0 || gid != 0) {
        bail!(
            "a rootless container maps only your own uid to root; user {} needs a root daemon (or newuidmap, not supported yet)",
            spec.user
        );
    }
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 && !rootless {
            bail!("setgroups: {}", errno());
        }
        if libc::setgid(gid) != 0 {
            bail!("setgid({gid}): {}", errno());
        }
        if libc::setuid(uid) != 0 {
            bail!("setuid({uid}): {}", errno());
        }
    }
    Ok(())
}

fn exec_argv(spec: &ContainerSpec) -> io::Error {
    let mut cmd = std::process::Command::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]).env_clear();
    let mut has_path = false;
    for e in &spec.env {
        if let Some((k, v)) = e.split_once('=') {
            has_path |= k == "PATH";
            cmd.env(k, v);
        }
    }
    if !has_path {
        cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
    }
    cmd.env("HOSTNAME", &spec.name);
    cmd.current_dir(&spec.workdir);
    cmd.exec()
}

/// Write the cgroup limits when no service manager did (`cgroup_dir` set), then move this process in.
fn setup_cgroup(spec: &ContainerSpec) -> Result<()> {
    let Some(dir) = &spec.cgroup_dir else {
        return Ok(());
    };
    let files = crate::render::cgroup_files(spec);
    fs::create_dir_all(dir).with_context(|| format!("creating cgroup {}", dir.display()))?;
    if let Some(parent) = dir.parent() {
        let need: Vec<String> = crate::render::controllers_needed(spec)
            .iter()
            .map(|c| format!("+{c}"))
            .collect();
        if !need.is_empty() {
            // Best effort: the parent may already have them enabled, or be delegated to us with them.
            let _ = fs::write(parent.join("cgroup.subtree_control"), need.join(" "));
        }
    }
    for (file, value) in files {
        fs::write(dir.join(&file), &value).with_context(|| {
            format!(
                "setting {file}={value} on {}: the limit was requested, so it is refused rather than ignored (is the controller delegated to this user? run the daemon with Delegate=yes)",
                dir.display()
            )
        })?;
    }
    fs::write(dir.join("cgroup.procs"), std::process::id().to_string())
        .context("moving the launcher into its cgroup")?;
    Ok(())
}

fn write_uid_map(rootless: bool, uid: u32, gid: u32) -> Result<()> {
    if !rootless {
        return Ok(());
    }
    fs::write("/proc/self/setgroups", "deny").context("setgroups deny")?;
    fs::write("/proc/self/uid_map", format!("0 {uid} 1")).context("writing uid_map")?;
    fs::write("/proc/self/gid_map", format!("0 {gid} 1")).context("writing gid_map")?;
    Ok(())
}

fn mkdir_p(p: &Path) {
    let _ = fs::create_dir_all(p);
}

/// Everything between "new mount namespace" and "pivot_root into the image".
fn setup_root(spec: &ContainerSpec) -> Result<()> {
    let root = &spec.rootfs;
    mount(
        "none",
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
    .context("making / private")?;
    mount(
        &root.display().to_string(),
        root,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )
    .with_context(|| format!("bind-mounting the rootfs {}", root.display()))?;
    if spec.hardening.read_only_root {
        // A remount of a bind mount must keep the flags the kernel locked in a user namespace.
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let c = cstr(root)?;
        let locked = if unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0 {
            st.f_flag
                & (libc::ST_NOSUID
                    | libc::ST_NODEV
                    | libc::ST_NOEXEC
                    | libc::ST_NOATIME
                    | libc::ST_RELATIME)
        } else {
            0
        };
        let mut flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY;
        if locked & libc::ST_NOSUID != 0 {
            flags |= libc::MS_NOSUID;
        }
        if locked & libc::ST_NODEV != 0 {
            flags |= libc::MS_NODEV;
        }
        if locked & libc::ST_NOEXEC != 0 {
            flags |= libc::MS_NOEXEC;
        }
        if locked & libc::ST_NOATIME != 0 {
            flags |= libc::MS_NOATIME;
        }
        if locked & libc::ST_RELATIME != 0 {
            flags |= libc::MS_RELATIME;
        }
        mount("none", root, None, flags, None).context("remounting the rootfs read-only")?;
    }
    let proc_ = root.join("proc");
    mkdir_p(&proc_);
    mount(
        "proc",
        &proc_,
        Some("proc"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )
    .context("mounting /proc")?;
    let dev = root.join("dev");
    mkdir_p(&dev);
    mount(
        "tmpfs",
        &dev,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NOEXEC,
        Some("mode=755,size=1m"),
    )
    .context("mounting /dev")?;
    for node in ["null", "zero", "full", "random", "urandom", "tty"] {
        let target = dev.join(node);
        fs::File::create(&target).with_context(|| format!("creating /dev/{node}"))?;
        mount(&format!("/dev/{node}"), &target, None, libc::MS_BIND, None)
            .with_context(|| format!("binding /dev/{node}"))?;
    }
    for (link, to) in [
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("fd", "/proc/self/fd"),
    ] {
        let _ = std::os::unix::fs::symlink(to, dev.join(link));
    }
    let shm = dev.join("shm");
    mkdir_p(&shm);
    let _ = mount(
        "shm",
        &shm,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        Some("mode=1777,size=64m"),
    );
    for (d, mode) in [("tmp", "mode=1777,size=256m"), ("run", "mode=755,size=64m")] {
        let p = root.join(d);
        mkdir_p(&p);
        mount(
            d,
            &p,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV,
            Some(mode),
        )
        .with_context(|| format!("mounting /{d}"))?;
    }
    if spec.net == Net::Host {
        let target = root.join("etc/resolv.conf");
        if target.exists() {
            let _ = mount(
                "/etc/resolv.conf",
                &target,
                None,
                libc::MS_BIND | libc::MS_RDONLY,
                None,
            );
        }
    }
    // pivot_root(".", "."): the old root is stacked under the new one and then detached; no put_old directory needed.
    std::env::set_current_dir(root).context("entering the rootfs")?;
    let dot = CString::new(".").unwrap();
    if unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) } != 0 {
        bail!("pivot_root: {}", errno());
    }
    if unsafe { libc::umount2(dot.as_ptr(), libc::MNT_DETACH) } != 0 {
        bail!("detaching the old root: {}", errno());
    }
    std::env::set_current_dir("/")?;
    Ok(())
}

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL_SEEN.store(sig, Ordering::SeqCst);
    let pid = CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, sig) };
    }
}

fn install_forwarding() {
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_signal as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Wait for `pid`, escalating a forwarded stop signal to SIGKILL after `grace`. Returns the shell-style exit code.
fn wait_for(pid: i32, grace: std::time::Duration) -> i32 {
    let mut killed_at: Option<std::time::Instant> = None;
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else if libc::WIFSIGNALED(status) {
                128 + libc::WTERMSIG(status)
            } else {
                1
            };
        }
        if r < 0 && errno().raw_os_error() != Some(libc::EINTR) {
            return 1;
        }
        if SIGNAL_SEEN.load(Ordering::SeqCst) != 0 {
            let at = *killed_at.get_or_insert_with(std::time::Instant::now);
            if at.elapsed() >= grace {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Run the container to completion and return its exit code. Writes `<state_dir>/pid` (the container's PID 1 as seen
/// from the host) and `<state_dir>/exit`.
pub fn run(spec: &ContainerSpec) -> Result<i32> {
    spec.validate().map_err(anyhow::Error::msg)?;
    if spec.net == Net::Host
        && spec
            .hardening
            .capabilities
            .iter()
            .any(|c| c == "CAP_NET_ADMIN")
    {
        bail!(
            "CAP_NET_ADMIN with the host network namespace would let the container reconfigure the host network"
        );
    }
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let rootless = uid != 0;
    let _ = fs::remove_file(spec.state_dir.join("exit"));
    setup_cgroup(spec)?;

    // The mount namespace is NOT created here: the child makes its own (see `child`), so its pivot_root does not take
    // the launcher's view of the host filesystem (state_dir, cgroups) away.
    let mut flags = libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS;
    if spec.net == Net::None {
        flags |= libc::CLONE_NEWNET;
    }
    if rootless {
        flags |= libc::CLONE_NEWUSER;
    }
    if unsafe { libc::unshare(flags) } != 0 {
        let e = errno();
        if rootless && matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) {
            bail!(
                "unshare({}) was refused ({e}). A rootless container needs an unprivileged user namespace; on Ubuntu 23.10+ AppArmor restricts that (kernel.apparmor_restrict_unprivileged_userns=1): install deploy/apparmor/fluxvm-container-run, or run the daemon as root. `fluxctl doctor` shows which",
                if rootless {
                    "NEWUSER|NEWPID|NEWIPC|NEWUTS"
                } else {
                    ""
                }
            );
        }
        bail!("unshare: {e}");
    }
    write_uid_map(rootless, uid, gid).map_err(|e| {
        anyhow::anyhow!(
            "{e:#}. A rootless container needs an unprivileged user namespace that keeps its capabilities; on Ubuntu 23.10+ AppArmor restricts that (kernel.apparmor_restrict_unprivileged_userns=1): install deploy/apparmor/fluxvm-container-run, or run the daemon as root. `fluxctl doctor` shows which"
        )
    })?;
    install_forwarding();
    // SAFETY: the launcher has one thread; the child only calls async-signal-safe-ish setup and then exec.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", errno());
    }
    if pid == 0 {
        // PID 1 of the new pid namespace.
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            )
        };
        let code = match child(spec, rootless) {
            Ok(never) => match never {},
            Err(e) => {
                eprintln!("fluxvm-container-run: {e:#}");
                126
            }
        };
        unsafe { libc::_exit(code) };
    }
    CHILD.store(pid, Ordering::SeqCst);
    if let Err(e) = fs::write(spec.state_dir.join("pid"), pid.to_string()) {
        eprintln!(
            "fluxvm-container-run: recording the pid in {}: {e}",
            spec.state_dir.display()
        );
    }
    let code = wait_for(pid, std::time::Duration::from_secs(10));
    if let Err(e) = fs::write(spec.state_dir.join("exit"), code.to_string()) {
        eprintln!(
            "fluxvm-container-run: recording the exit code in {}: {e}",
            spec.state_dir.display()
        );
    }
    Ok(code)
}

fn child(spec: &ContainerSpec, rootless: bool) -> Result<std::convert::Infallible> {
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        bail!("unshare(NEWNS): {}", errno());
    }
    let host = CString::new(spec.name.as_str())?;
    if unsafe { libc::sethostname(host.as_ptr(), spec.name.len()) } != 0 {
        bail!("sethostname: {}", errno());
    }
    setup_root(spec)?;
    restrict_capabilities(&spec.hardening.capabilities)?;
    drop_identity(spec, rootless)?;
    if spec.hardening.no_new_privileges
        && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
    {
        bail!("no_new_privs: {}", errno());
    }
    apply_seccomp(spec)?;
    Err(anyhow::Error::from(exec_argv(spec)).context(format!("executing {}", spec.argv[0])))
}

/// Move this process into the cgroup of host pid `pid`. Refused, not skipped: an exec that escaped the memory limit
/// would make the limit meaningless.
fn join_cgroup_of(pid: i32) -> Result<()> {
    let raw = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("reading /proc/{pid}/cgroup (is the container running?)"))?;
    let rel = crate::probe::parse_self_cgroup(&raw)
        .context("the container is not in a cgroup v2 hierarchy")?;
    let mine = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|s| crate::probe::parse_self_cgroup(&s));
    if mine.as_deref() == Some(rel.as_str()) {
        return Ok(());
    }
    let dir = format!("/sys/fs/cgroup{}", rel.trim_end_matches('/'));
    fs::write(format!("{dir}/cgroup.procs"), std::process::id().to_string()).with_context(|| {
        format!("joining the container's cgroup {dir}: run the daemon where it can write to it (a unit with Delegate=yes, or as root)")
    })
}

/// Join the namespaces of the container whose init has host pid `pid` (user first: it grants the capabilities the
/// others need). Afterwards this process's root is the container's root.
fn join_namespaces(pid: i32) -> Result<()> {
    let ns = |n: &str| PathBuf::from(format!("/proc/{pid}/ns/{n}"));
    let open = |n: &str| -> Result<i32> {
        let c = cstr(&ns(n))?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            bail!(
                "opening {}: {} (is the container running?)",
                ns(n).display(),
                errno()
            );
        }
        Ok(fd)
    };
    let same = |n: &str| -> bool {
        match (
            fs::metadata(ns(n)),
            fs::metadata(format!("/proc/self/ns/{n}")),
        ) {
            (Ok(a), Ok(b)) => {
                std::os::unix::fs::MetadataExt::ino(&a) == std::os::unix::fs::MetadataExt::ino(&b)
            }
            _ => false,
        }
    };
    // Open every namespace first: once the mount namespace is joined, /proc is the container's, not the host's.
    let mut fds: Vec<(&str, i32, i32)> = Vec::new();
    for (name, flag) in [
        ("user", libc::CLONE_NEWUSER),
        ("mnt", libc::CLONE_NEWNS),
        ("pid", libc::CLONE_NEWPID),
        ("ipc", libc::CLONE_NEWIPC),
        ("uts", libc::CLONE_NEWUTS),
        ("net", libc::CLONE_NEWNET),
    ] {
        if !same(name) {
            fds.push((name, open(name)?, flag));
        }
    }
    for (name, fd, flag) in fds {
        if unsafe { libc::setns(fd, flag) } != 0 {
            bail!("setns({name}): {}", errno());
        }
        unsafe { libc::close(fd) };
    }
    Ok(())
}

/// Read `path` as the container sees it. Returns `(mode, bytes)`; refuses anything over `max` bytes.
pub fn read_in(pid: i32, path: &str, max: u64) -> Result<(u32, Vec<u8>)> {
    use std::io::Read;
    join_namespaces(pid)?;
    let f = fs::File::open(path).with_context(|| format!("opening {path} in the container"))?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        bail!("{path} is not a regular file");
    }
    if meta.len() > max {
        bail!("{path} is {} bytes; the limit is {max}", meta.len());
    }
    let mut buf = Vec::new();
    f.take(max + 1).read_to_end(&mut buf)?;
    Ok((std::os::unix::fs::MetadataExt::mode(&meta) & 0o7777, buf))
}

/// Write `data` to `path` as the container sees it, creating parent directories.
pub fn write_in(pid: i32, path: &str, mode: u32, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    join_namespaces(pid)?;
    if let Some(parent) = Path::new(path).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {} in the container", parent.display()))?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("writing {path} in the container"))?;
    f.write_all(data)?;
    Ok(())
}

/// Run `argv` inside the running container whose init has host pid `pid`, with the container's restrictions.
pub fn exec_in(spec: &ContainerSpec, pid: i32, argv: &[String]) -> Result<i32> {
    if argv.is_empty() {
        bail!("exec needs a command");
    }
    let rootless = unsafe { libc::geteuid() } != 0;
    // Same cgroup as the container, so the limits cover what exec starts. Before the mount namespace changes,
    // while /sys/fs/cgroup is still the host's.
    join_cgroup_of(pid)?;
    join_namespaces(pid)?;
    install_forwarding();
    let pid_child = unsafe { libc::fork() };
    if pid_child < 0 {
        bail!("fork: {}", errno());
    }
    if pid_child == 0 {
        unsafe {
            libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            )
        };
        let mut s = spec.clone();
        s.argv = argv.to_vec();
        let r: Result<std::convert::Infallible> = (|| {
            restrict_capabilities(&s.hardening.capabilities)?;
            drop_identity(&s, rootless)?;
            if s.hardening.no_new_privileges
                && unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
            {
                bail!("no_new_privs: {}", errno());
            }
            apply_seccomp(&s)?;
            Err(anyhow::Error::from(exec_argv(&s)).context(format!("executing {}", s.argv[0])))
        })();
        let e = match r {
            Ok(never) => match never {},
            Err(e) => e,
        };
        eprintln!("fluxvm-container-run: {e:#}");
        unsafe { libc::_exit(127) };
    }
    CHILD.store(pid_child, Ordering::SeqCst);
    Ok(wait_for(pid_child, std::time::Duration::from_secs(5)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_numbers_match_the_kernel_header() {
        assert_eq!(cap_number("CAP_CHOWN"), Some(0));
        assert_eq!(cap_number("CAP_SYS_ADMIN"), Some(21));
        assert_eq!(cap_number("CAP_NET_RAW"), Some(13));
        assert_eq!(cap_number("CAP_CHECKPOINT_RESTORE"), Some(40));
        assert_eq!(cap_number("CAP_NOPE"), None);
        for c in crate::spec::DEFAULT_CAPS {
            assert!(cap_number(c).is_some(), "{c}");
        }
    }

    #[test]
    fn users() {
        assert_eq!(parse_user(""), None);
        assert_eq!(parse_user("1000"), Some((1000, 1000)));
        assert_eq!(parse_user("1000:50"), Some((1000, 50)));
        assert_eq!(parse_user("x"), None);
    }

    #[test]
    fn a_rootless_container_for_another_uid_is_refused() {
        let s = ContainerSpec {
            user: "1000".into(),
            ..Default::default()
        };
        assert!(
            drop_identity(&s, true)
                .unwrap_err()
                .to_string()
                .contains("root daemon")
        );
    }
}
