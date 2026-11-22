// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! What this host can do for each runtime, with a fix hint for everything it cannot.

use crate::render::Manager;
use crate::runtime::HostSupport;
use serde::Serialize;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

/// Whether an unprivileged process can create a user namespace (needed by a rootless container).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Userns {
    Allowed,
    /// Ubuntu 23.10+: `kernel.apparmor_restrict_unprivileged_userns=1` lets only programs with an AppArmor profile do it.
    RestrictedByApparmor,
    /// `user.max_user_namespaces = 0` or `kernel.unprivileged_userns_clone = 0`.
    Disabled,
    Unknown,
}

/// Raw facts, so the verdicts are testable without a Linux host.
#[derive(Debug, Clone, Serialize)]
pub struct HostFacts {
    pub linux: bool,
    pub root: bool,
    pub uid: u32,
    pub systemd_pid1: bool,
    /// A user manager (`systemd --user`) answers on the session bus.
    pub systemd_user_manager: bool,
    pub cgroup_v2: bool,
    /// Controllers the daemon's own cgroup may hand to children (`cgroup.controllers` of its cgroup).
    pub delegated_controllers: Vec<String>,
    /// The daemon's cgroup directory is writable by it (it can create child cgroups).
    pub cgroup_writable: bool,
    pub userns: Userns,
    pub overlayfs: bool,
    pub seccomp: bool,
    pub landlock_abi: Option<u32>,
    pub kvm: bool,
    pub launcher: Option<PathBuf>,
    pub kernel: String,
}

impl HostFacts {
    /// Which manager would run containers on this host.
    pub fn manager(&self) -> Manager {
        if self.systemd_pid1 && self.root {
            Manager::SystemdSystem
        } else if self.systemd_pid1 && self.systemd_user_manager {
            Manager::SystemdUser
        } else {
            Manager::Direct
        }
    }

    /// Whether containers can be created: namespaces (root, or an unprivileged user namespace) and the launcher.
    pub fn container_support(&self) -> Result<(), String> {
        if !self.linux {
            return Err(
                "native containers are Linux only (on a Mac, every container is its own VM)".into(),
            );
        }
        if !self.cgroup_v2 {
            return Err("cgroup v2 is not mounted at /sys/fs/cgroup (boot with systemd.unified_cgroup_hierarchy=1)".into());
        }
        if !self.seccomp {
            return Err("the kernel has no seccomp filter support".into());
        }
        if !self.root {
            match self.userns {
                Userns::Allowed | Userns::Unknown => {}
                Userns::RestrictedByApparmor => {
                    return Err("rootless containers need a user namespace, which AppArmor restricts here (kernel.apparmor_restrict_unprivileged_userns=1); install the fluxvm-container-run AppArmor profile (deploy/apparmor/fluxvm-container-run) or run the daemon as root".into());
                }
                Userns::Disabled => {
                    return Err("rootless containers need unprivileged user namespaces, which are disabled (user.max_user_namespaces=0 or kernel.unprivileged_userns_clone=0); enable them or run the daemon as root".into());
                }
            }
        }
        if self.launcher.is_none() {
            return Err(
                "the fluxvm-container-run launcher was not found next to the daemon or on PATH"
                    .into(),
            );
        }
        Ok(())
    }

    /// A microVM per container image on this host.
    pub fn vm_image_support(&self) -> Result<(), String> {
        if !self.linux {
            // The Mac side decides this itself (vz); the scheduler overrides the answer there.
            return Err("not Linux".into());
        }
        Err("one microVM per container image is not implemented on Linux backends yet (only on macOS/vz); a plain VM sandbox without an image works".into())
    }

    /// A VM sandbox without an image (Debian cloud image, template) on the Linux backends.
    pub fn vm_plain_support(&self) -> Result<(), String> {
        if !self.linux {
            return Err("not Linux".into());
        }
        if !self.kvm {
            return Err("/dev/kvm is missing or not accessible to this user (add the user to the kvm group, or enable virtualization)".into());
        }
        Ok(())
    }

    pub fn support(&self) -> HostSupport {
        HostSupport {
            container: self.container_support(),
            vm_image: self.vm_image_support(),
            vm_plain: self.vm_plain_support(),
        }
    }

    /// Lines for `fluxctl doctor`: `(level, name, detail, fix)`, level one of ok/warn/fail.
    pub fn doctor(&self) -> Vec<DoctorLine> {
        let mut v = Vec::new();
        let line =
            |level: &'static str, name: &'static str, detail: String, fix: &str| DoctorLine {
                level,
                name,
                detail,
                fix: fix.to_string(),
            };
        if !self.linux {
            v.push(line(
                "ok",
                "container runtime",
                "not applicable: macOS runs every container in its own VM".into(),
                "",
            ));
            return v;
        }
        let m = self.manager();
        v.push(line(
            "ok",
            "manager",
            match m {
                Manager::SystemdSystem => "systemd system manager: containers are transient system units with Delegate=yes".into(),
                Manager::SystemdUser => "systemd user manager: containers are transient user units (systemd-run --user) with Delegate=yes".into(),
                Manager::Direct => "no systemd manager available: the launcher creates and fills the cgroup itself".into(),
            },
            "",
        ));
        if m == Manager::Direct && self.systemd_pid1 && !self.root {
            v.push(line(
                "warn",
                "systemd user manager",
                "systemd is PID 1 but no user manager answered".into(),
                "run the daemon from a login session or `loginctl enable-linger $USER`; until then the daemon manages cgroups itself",
            ));
        }
        if !self.systemd_pid1 {
            v.push(line(
                "warn",
                "systemd",
                "systemd is not PID 1 (OpenRC, a container or WSL)".into(),
                "supported through the direct manager; restart policy and journald logs come from the daemon instead",
            ));
        }
        if self.cgroup_v2 {
            let det = if self.delegated_controllers.is_empty() {
                "no controllers are delegated to the daemon's cgroup".to_string()
            } else {
                format!(
                    "controllers available to containers: {}",
                    self.delegated_controllers.join(" ")
                )
            };
            let missing: Vec<&str> = ["memory", "cpu", "pids"]
                .into_iter()
                .filter(|c| !self.delegated_controllers.iter().any(|d| d == c))
                .collect();
            if missing.is_empty() && self.cgroup_writable {
                v.push(line("ok", "cgroup v2 delegation", det, ""));
            } else {
                v.push(line(
                    "warn",
                    "cgroup v2 delegation",
                    format!("{det}; missing: {}; writable: {}", if missing.is_empty() { "none".into() } else { missing.join(" ") }, self.cgroup_writable),
                    "run the daemon as a unit with Delegate=yes (systemd/fluxvm.service does) so \"failed to delegate cgroup controllers\" goes away; limits that need a missing controller are refused, not ignored",
                ));
            }
        } else {
            v.push(line(
                "fail",
                "cgroup v2",
                "not mounted".into(),
                "boot with systemd.unified_cgroup_hierarchy=1 or mount cgroup2 at /sys/fs/cgroup",
            ));
        }
        match (self.root, self.userns) {
            (true, _) => v.push(line("ok", "namespaces", "running as root: no user namespace needed (rootful containers)".into(), "")),
            (false, Userns::Allowed) => v.push(line("ok", "rootless", "unprivileged user namespaces are allowed".into(), "")),
            (false, Userns::Unknown) => v.push(line("warn", "rootless", "could not tell whether user namespaces are allowed".into(), "")),
            (false, Userns::RestrictedByApparmor) => v.push(line(
                "fail",
                "rootless",
                "AppArmor restricts unprivileged user namespaces (kernel.apparmor_restrict_unprivileged_userns=1)".into(),
                "install deploy/apparmor/fluxvm-container-run into /etc/apparmor.d and run `apparmor_parser -r` on it, or run the daemon as root, or set the sysctl to 0",
            )),
            (false, Userns::Disabled) => v.push(line(
                "fail",
                "rootless",
                "unprivileged user namespaces are disabled".into(),
                "sysctl user.max_user_namespaces=15000 (and kernel.unprivileged_userns_clone=1 on Debian)",
            )),
        }
        v.push(line(
            if self.seccomp { "ok" } else { "fail" },
            "seccomp",
            if self.seccomp {
                "available".into()
            } else {
                "not available".into()
            },
            if self.seccomp {
                ""
            } else {
                "use a kernel built with CONFIG_SECCOMP_FILTER"
            },
        ));
        v.push(line(
            if self.overlayfs { "ok" } else { "warn" },
            "overlayfs",
            if self.overlayfs { "available (planned for layered rootfs; today a read-only shared rootfs or a copy is used)".into() } else { "not available".into() },
            if self.overlayfs { "" } else { "modprobe overlay; copies are used meanwhile" },
        ));
        match &self.launcher {
            Some(p) => v.push(line("ok", "launcher", p.display().to_string(), "")),
            None => v.push(line("fail", "launcher", "fluxvm-container-run not found".into(), "install it next to the fluxvm daemon (it ships in the Linux package) or put it on PATH")),
        }
        match self.container_support() {
            Ok(()) => v.push(line("ok", "runtime=container", "ready".into(), "")),
            Err(why) => v.push(line(
                "fail",
                "runtime=container",
                why,
                "see the failing checks above",
            )),
        }
        match self.vm_plain_support() {
            Ok(()) => v.push(line(
                "ok",
                "runtime=vm (VM sandbox)",
                format!(
                    "/dev/kvm usable{}",
                    self.landlock_abi
                        .map(|a| format!("; landlock ABI {a}"))
                        .unwrap_or_default()
                ),
                "",
            )),
            Err(why) => v.push(line("warn", "runtime=vm (VM sandbox)", why, "")),
        }
        v.push(line(
            "warn",
            "runtime=vm (container image)",
            "one microVM per container image is macOS/vz only so far".into(),
            "on Linux use `--runtime container` for images (trusted code) or `fluxctl sandbox create` for a VM sandbox",
        ));
        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorLine {
    pub level: &'static str,
    pub name: &'static str,
    pub detail: String,
    pub fix: String,
}

/// Decide the user-namespace state from the three kernel knobs (`None` = file missing or unreadable).
pub fn userns_state(
    max_user_namespaces: Option<u64>,
    unprivileged_userns_clone: Option<u64>,
    apparmor_restrict: Option<u64>,
) -> Userns {
    if max_user_namespaces == Some(0) || unprivileged_userns_clone == Some(0) {
        return Userns::Disabled;
    }
    if apparmor_restrict == Some(1) {
        return Userns::RestrictedByApparmor;
    }
    if max_user_namespaces.is_none() && unprivileged_userns_clone.is_none() {
        return Userns::Unknown;
    }
    Userns::Allowed
}

/// `cpu memory pids` → vec.
pub fn parse_controllers(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// The cgroup path of this process from `/proc/self/cgroup` (`0::/user.slice/...`).
pub fn parse_self_cgroup(s: &str) -> Option<String> {
    s.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
}

#[cfg(target_os = "linux")]
fn read_u64(p: &str) -> Option<u64> {
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

/// Find the launcher: `FLUXVM_CONTAINER_RUN`, next to the current executable, then PATH.
pub fn find_launcher() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FLUXVM_CONTAINER_RUN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let p = dir.join(crate::LAUNCHER);
        if p.is_file() {
            return Some(p);
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(crate::LAUNCHER))
            .find(|p| p.is_file())
    })
}

#[cfg(target_os = "linux")]
fn access(path: &str, mode: i32) -> bool {
    std::ffi::CString::new(path)
        .map(|c| unsafe { libc::access(c.as_ptr(), mode) } == 0)
        .unwrap_or(false)
}

/// Probe the host. Reads only `/proc`, `/sys` and `/run`; changes nothing.
pub fn probe() -> HostFacts {
    #[cfg(not(target_os = "linux"))]
    {
        HostFacts {
            linux: false,
            root: false,
            uid: 0,
            systemd_pid1: false,
            systemd_user_manager: false,
            cgroup_v2: false,
            delegated_controllers: vec![],
            cgroup_writable: false,
            userns: Userns::Unknown,
            overlayfs: false,
            seccomp: false,
            landlock_abi: None,
            kvm: false,
            launcher: find_launcher(),
            kernel: String::new(),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let uid = unsafe { libc::geteuid() };
        let cgroup_v2 = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
        let own = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|s| parse_self_cgroup(&s))
            .unwrap_or_else(|| "/".into());
        let dir = format!("/sys/fs/cgroup{}", own.trim_end_matches('/'));
        let delegated = std::fs::read_to_string(format!("{dir}/cgroup.controllers"))
            .map(|s| parse_controllers(&s))
            .unwrap_or_default();
        let runtime_dir =
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{uid}"));
        HostFacts {
            linux: true,
            root: uid == 0,
            uid,
            systemd_pid1: Path::new("/run/systemd/system").exists(),
            systemd_user_manager: Path::new(&format!("{runtime_dir}/systemd")).exists()
                || Path::new(&format!("{runtime_dir}/bus")).exists(),
            cgroup_v2,
            delegated_controllers: delegated,
            cgroup_writable: access(&format!("{dir}/cgroup.procs"), libc::W_OK)
                && access(&dir, libc::W_OK),
            userns: userns_state(
                read_u64("/proc/sys/user/max_user_namespaces"),
                read_u64("/proc/sys/kernel/unprivileged_userns_clone"),
                read_u64("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"),
            ),
            overlayfs: std::fs::read_to_string("/proc/filesystems")
                .map(|s| s.lines().any(|l| l.trim_end().ends_with("overlay")))
                .unwrap_or(false),
            seccomp: fluxvm_procbox::seccomp::available(),
            landlock_abi: {
                let a = fluxvm_procbox::landlock::kernel_abi();
                (a > 0).then_some(a)
            },
            kvm: access("/dev/kvm", libc::R_OK | libc::W_OK),
            launcher: find_launcher(),
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> HostFacts {
        HostFacts {
            linux: true,
            root: false,
            uid: 1000,
            systemd_pid1: true,
            systemd_user_manager: true,
            cgroup_v2: true,
            delegated_controllers: vec!["cpu".into(), "memory".into(), "pids".into()],
            cgroup_writable: true,
            userns: Userns::Allowed,
            overlayfs: true,
            seccomp: true,
            landlock_abi: Some(7),
            kvm: true,
            launcher: Some(PathBuf::from("/usr/bin/fluxvm-container-run")),
            kernel: "7.0.0".into(),
        }
    }

    #[test]
    fn userns_knobs() {
        assert_eq!(userns_state(Some(100), None, Some(0)), Userns::Allowed);
        assert_eq!(
            userns_state(Some(100), Some(1), Some(1)),
            Userns::RestrictedByApparmor
        );
        assert_eq!(userns_state(Some(0), None, None), Userns::Disabled);
        assert_eq!(userns_state(Some(10), Some(0), None), Userns::Disabled);
        assert_eq!(userns_state(None, None, None), Userns::Unknown);
    }

    #[test]
    fn parsers() {
        assert_eq!(
            parse_controllers("cpu memory pids\n"),
            ["cpu", "memory", "pids"]
        );
        assert_eq!(
            parse_self_cgroup("0::/user.slice/a.service\n").unwrap(),
            "/user.slice/a.service"
        );
        assert!(parse_self_cgroup("1:name=x:/\n").is_none());
    }

    #[test]
    fn manager_choice() {
        let mut f = facts();
        assert_eq!(f.manager(), Manager::SystemdUser);
        f.root = true;
        assert_eq!(f.manager(), Manager::SystemdSystem);
        f.systemd_pid1 = false;
        assert_eq!(f.manager(), Manager::Direct);
        let mut g = facts();
        g.systemd_user_manager = false;
        assert_eq!(g.manager(), Manager::Direct);
    }

    #[test]
    fn container_verdicts_have_a_hint() {
        assert!(facts().container_support().is_ok());
        let mut f = facts();
        f.userns = Userns::RestrictedByApparmor;
        let e = f.container_support().unwrap_err();
        assert!(
            e.contains("AppArmor") && e.contains("deploy/apparmor"),
            "{e}"
        );
        f.root = true;
        assert!(
            f.container_support().is_ok(),
            "root does not need a user namespace"
        );
        let mut f = facts();
        f.userns = Userns::Disabled;
        assert!(
            f.container_support()
                .unwrap_err()
                .contains("max_user_namespaces")
        );
        let mut f = facts();
        f.launcher = None;
        assert!(f.container_support().unwrap_err().contains("launcher"));
        let mut f = facts();
        f.cgroup_v2 = false;
        assert!(f.container_support().unwrap_err().contains("cgroup v2"));
        let mut f = facts();
        f.linux = false;
        assert!(f.container_support().unwrap_err().contains("Linux only"));
    }

    #[test]
    fn vm_support_reports_the_image_gap_plainly() {
        let f = facts();
        assert!(f.vm_plain_support().is_ok());
        assert!(
            f.vm_image_support()
                .unwrap_err()
                .contains("not implemented on Linux")
        );
        let mut f = facts();
        f.kvm = false;
        assert!(f.vm_plain_support().unwrap_err().contains("/dev/kvm"));
    }

    #[test]
    fn doctor_lists_every_runtime_and_the_delegation_fix() {
        let mut f = facts();
        f.delegated_controllers = vec!["pids".into()];
        let d = f.doctor();
        let by = |n: &str| d.iter().find(|l| l.name == n).unwrap().clone();
        assert_eq!(by("cgroup v2 delegation").level, "warn");
        assert!(by("cgroup v2 delegation").fix.contains("Delegate=yes"));
        assert_eq!(by("runtime=container").level, "ok");
        assert_eq!(by("runtime=vm (VM sandbox)").level, "ok");
        assert_eq!(by("runtime=vm (container image)").level, "warn");
        let mut f = facts();
        f.userns = Userns::RestrictedByApparmor;
        let d = f.doctor();
        assert_eq!(
            d.iter().find(|l| l.name == "rootless").unwrap().level,
            "fail"
        );
        assert_eq!(
            d.iter()
                .find(|l| l.name == "runtime=container")
                .unwrap()
                .level,
            "fail"
        );
    }

    #[test]
    fn no_systemd_is_reported_as_the_direct_fallback() {
        let mut f = facts();
        f.systemd_pid1 = false;
        f.systemd_user_manager = false;
        let d = f.doctor();
        assert!(
            d.iter()
                .any(|l| l.name == "systemd" && l.fix.contains("direct manager"))
        );
        assert!(
            d[0].detail
                .contains("launcher creates and fills the cgroup")
        );
    }
}
