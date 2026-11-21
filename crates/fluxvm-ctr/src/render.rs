// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! A container as a systemd service, plus the cgroup v2 files and seccomp list the launcher applies.
//!
//! The same property list feeds three outputs, so they cannot drift: the unit text (`--print-unit`,
//! `sandbox inspect`), the `systemd-run` command line the daemon runs, and the golden tests.

use crate::spec::{ContainerSpec, Net, Restart, Seccomp};

/// Who manages the container's cgroup and lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// `systemd-run` on the system manager (daemon runs as root).
    SystemdSystem,
    /// `systemd-run --user` (rootless; needs the user's session or lingering).
    SystemdUser,
    /// No systemd as PID 1 (OpenRC, a container, WSL): the launcher manages the cgroup itself.
    Direct,
}

impl Manager {
    pub fn as_str(self) -> &'static str {
        match self {
            Manager::SystemdSystem => "systemd (system manager)",
            Manager::SystemdUser => "systemd (user manager)",
            Manager::Direct => "direct (no systemd: the launcher manages the cgroup)",
        }
    }
}

/// Syscalls the default seccomp profile denies, in the order the launcher's filter lists them
/// (`fluxvm_procbox::seccomp::denied_syscalls`; a Linux test keeps this list equal to it) plus the
/// namespace calls, which a container never needs after start.
pub const SECCOMP_DENIED: &[&str] = &[
    "ptrace",
    "mount",
    "umount2",
    "pivot_root",
    "chroot",
    "kexec_load",
    "init_module",
    "finit_module",
    "delete_module",
    "bpf",
    "perf_event_open",
    "keyctl",
    "add_key",
    "request_key",
    "reboot",
    "swapon",
    "swapoff",
    "open_by_handle_at",
    "userfaultfd",
    "process_vm_readv",
    "process_vm_writev",
    "unshare",
    "setns",
];

/// `~a b c` in systemd's `SystemCallFilter=` vocabulary: everything except these.
pub fn system_call_filter() -> String {
    format!("~{}", SECCOMP_DENIED.join(" "))
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-:=@%+,".contains(c)) {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

pub fn spec_path(spec: &ContainerSpec) -> String {
    spec.state_dir.join("spec.json").display().to_string()
}

pub fn exec_start(spec: &ContainerSpec) -> String {
    format!(
        "{} run --spec {}",
        quote(&spec.launcher.display().to_string()),
        quote(&spec_path(spec))
    )
}

/// The `[Unit]` properties: description and ordering.
pub fn unit_properties(spec: &ContainerSpec) -> Vec<(String, String)> {
    let mut p = vec![(
        "Description".to_string(),
        format!("FluxVM container {} ({})", spec.name, spec.image),
    )];
    if !spec.after.is_empty() {
        p.push(("After".into(), spec.after.join(" ")));
    }
    if !spec.requires.is_empty() {
        p.push(("Requires".into(), spec.requires.join(" ")));
    }
    p
}

/// The `[Service]` properties systemd itself enforces: cgroup delegation and limits, restart, lifetime, logs.
pub fn service_properties(spec: &ContainerSpec) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = Vec::new();
    let mut add = |k: &str, v: String| p.push((k.to_string(), v));
    add("Type", "exec".into());
    // Delegate=yes hands the launcher its own cgroup subtree, so the container's cgroup is a child of the unit's.
    add("Delegate", "yes".into());
    if let Some(m) = spec.limits.memory_mib {
        add("MemoryMax", format!("{m}M"));
        add("MemorySwapMax", "0".into());
    }
    if let Some(c) = spec.limits.cpu_percent {
        add("CPUQuota", format!("{c}%"));
    }
    if let Some(n) = spec.limits.pids {
        add("TasksMax", n.to_string());
    }
    if let Some(w) = spec.limits.io_weight {
        add("IOWeight", w.to_string());
    }
    match spec.restart {
        Restart::No => add("Restart", "no".into()),
        Restart::OnFailure => add("Restart", "on-failure".into()),
        Restart::Always => add("Restart", "always".into()),
    }
    if spec.restart != Restart::No {
        add("RestartSec", spec.restart_sec.to_string());
    }
    if let Some(t) = spec.ttl_seconds {
        add("RuntimeMaxSec", t.to_string());
    }
    add("KillMode", "control-group".into());
    add("TimeoutStopSec", "15".into());
    if spec.hardening.no_new_privileges {
        add("NoNewPrivileges", "yes".into());
    }
    let log = spec.log_path.display().to_string();
    add("StandardOutput", format!("append:{log}"));
    add("StandardError", format!("append:{log}"));
    add("SyslogIdentifier", spec.name.clone());
    p
}

/// The container profile in systemd's hardening vocabulary, enforced by the launcher inside the container
/// (a service cannot set these for a rootfs it pivots into). Names and values mean what they mean in a unit.
pub fn container_profile(spec: &ContainerSpec, rootless: bool) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = Vec::new();
    let mut add = |k: &str, v: String| p.push((k.to_string(), v));
    add("RootDirectory", spec.rootfs.display().to_string());
    add("MountAPIVFS", "yes".into());
    if spec.hardening.read_only_root {
        add("ReadOnlyPaths", "/".into());
    }
    add("PrivateTmp", "yes".into());
    add("PrivateDevices", "yes".into());
    add("PrivateIPC", "yes".into());
    add("PrivatePIDs", "yes".into());
    add("PrivateUsers", if rootless { "yes" } else { "no" }.into());
    add(
        "PrivateNetwork",
        if spec.net == Net::None { "yes" } else { "no" }.into(),
    );
    add("ProtectHostname", "yes".into());
    add(
        "NoNewPrivileges",
        if spec.hardening.no_new_privileges { "yes" } else { "no" }.into(),
    );
    add(
        "CapabilityBoundingSet",
        if spec.hardening.capabilities.is_empty() {
            String::new()
        } else {
            spec.hardening.capabilities.join(" ")
        },
    );
    match spec.hardening.seccomp {
        Seccomp::Default => {
            add("SystemCallFilter", system_call_filter());
            add("SystemCallErrorNumber", "EPERM".into());
            add("RestrictNamespaces", "yes".into());
        }
        Seccomp::Unconfined => add("SystemCallFilter", "(none: seccomp unconfined)".into()),
    }
    if !spec.user.is_empty() {
        add("User", spec.user.split(':').next().unwrap_or("").to_string());
    }
    add("WorkingDirectory", spec.workdir.clone());
    for e in &spec.env {
        add("Environment", quote(e));
    }
    p
}

/// The unit file for `spec`: what `--print-unit` and `sandbox inspect` show.
pub fn unit_file(spec: &ContainerSpec, rootless: bool) -> String {
    let mut out = String::new();
    out.push_str("# FluxVM container as a systemd service. Read it, copy it, reuse it.\n");
    out.push_str(&format!("# unit: {}\n", spec.unit_name()));
    out.push_str("[Unit]\n");
    for (k, v) in unit_properties(spec) {
        out.push_str(&format!("{k}={v}\n"));
    }
    out.push_str("\n[Service]\n");
    for (k, v) in service_properties(spec) {
        out.push_str(&format!("{k}={v}\n"));
    }
    out.push_str(&format!("ExecStart={}\n", exec_start(spec)));
    out.push_str("\n# Enforced by fluxvm-container-run inside the container, in systemd's own vocabulary.\n");
    out.push_str("# systemd ignores [X-...] sections, so this file loads as is.\n");
    out.push_str("[X-FluxVM-Container]\n");
    for (k, v) in container_profile(spec, rootless) {
        out.push_str(&format!("{k}={v}\n"));
    }
    out
}

/// The `systemd-run` argv that starts `spec` as a transient unit (nothing is written to /etc).
pub fn systemd_run_argv(spec: &ContainerSpec, manager: Manager) -> Vec<String> {
    let mut a: Vec<String> = vec!["systemd-run".into()];
    if manager == Manager::SystemdUser {
        a.push("--user".into());
    }
    a.extend(["--quiet".into(), "--collect".into(), format!("--unit={}", spec.name)]);
    for (k, v) in unit_properties(spec).into_iter().chain(service_properties(spec)) {
        if k == "Type" {
            a.push("--service-type=exec".into());
        } else if k == "Description" {
            a.push(format!("--description={v}"));
        } else {
            a.push(format!("--property={k}={v}"));
        }
    }
    a.push("--".into());
    a.push(spec.launcher.display().to_string());
    a.push("run".into());
    a.push("--spec".into());
    a.push(spec_path(spec));
    a
}

/// cgroup v2 files the launcher writes when no service manager does it (`Manager::Direct`), as `(file, value)`.
pub fn cgroup_files(spec: &ContainerSpec) -> Vec<(String, String)> {
    let mut f = Vec::new();
    if let Some(m) = spec.limits.memory_mib {
        f.push(("memory.max".to_string(), (m * 1024 * 1024).to_string()));
        f.push(("memory.swap.max".to_string(), "0".to_string()));
        f.push(("memory.oom.group".to_string(), "1".to_string()));
    }
    if let Some(c) = spec.limits.cpu_percent {
        f.push(("cpu.max".to_string(), format!("{} 100000", u64::from(c) * 1000)));
    }
    if let Some(n) = spec.limits.pids {
        f.push(("pids.max".to_string(), n.to_string()));
    }
    if let Some(w) = spec.limits.io_weight {
        f.push(("io.weight".to_string(), format!("default {w}")));
    }
    f
}

/// Controllers the limits need, for `cgroup.subtree_control` and the doctor.
pub fn controllers_needed(spec: &ContainerSpec) -> Vec<&'static str> {
    let mut c = Vec::new();
    if spec.limits.memory_mib.is_some() {
        c.push("memory");
    }
    if spec.limits.cpu_percent.is_some() {
        c.push("cpu");
    }
    if spec.limits.pids.is_some() {
        c.push("pids");
    }
    if spec.limits.io_weight.is_some() {
        c.push("io");
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Hardening, Limits};
    use std::path::PathBuf;

    fn spec() -> ContainerSpec {
        ContainerSpec {
            name: "fx-1a2b3c4d".into(),
            image: "alpine:3.22".into(),
            rootfs: PathBuf::from("/var/lib/fluxvm/instances/1a2b/rootfs"),
            argv: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            env: vec!["A=b c".into()],
            workdir: "/".into(),
            user: "1000:1000".into(),
            limits: Limits { memory_mib: Some(256), cpu_percent: Some(50), pids: Some(128), io_weight: Some(100) },
            restart: Restart::OnFailure,
            ttl_seconds: Some(600),
            after: vec!["fx-db.service".into()],
            requires: vec!["fx-db.service".into()],
            state_dir: PathBuf::from("/var/lib/fluxvm/instances/1a2b"),
            log_path: PathBuf::from("/var/lib/fluxvm/instances/1a2b/console.log"),
            launcher: PathBuf::from("/usr/bin/fluxvm-container-run"),
            ..Default::default()
        }
    }

    #[test]
    fn unit_file_golden() {
        let want = "\
# FluxVM container as a systemd service. Read it, copy it, reuse it.
# unit: fx-1a2b3c4d.service
[Unit]
Description=FluxVM container fx-1a2b3c4d (alpine:3.22)
After=fx-db.service
Requires=fx-db.service

[Service]
Type=exec
Delegate=yes
MemoryMax=256M
MemorySwapMax=0
CPUQuota=50%
TasksMax=128
IOWeight=100
Restart=on-failure
RestartSec=1
RuntimeMaxSec=600
KillMode=control-group
TimeoutStopSec=15
NoNewPrivileges=yes
StandardOutput=append:/var/lib/fluxvm/instances/1a2b/console.log
StandardError=append:/var/lib/fluxvm/instances/1a2b/console.log
SyslogIdentifier=fx-1a2b3c4d
ExecStart=/usr/bin/fluxvm-container-run run --spec /var/lib/fluxvm/instances/1a2b/spec.json

# Enforced by fluxvm-container-run inside the container, in systemd's own vocabulary.
# systemd ignores [X-...] sections, so this file loads as is.
[X-FluxVM-Container]
RootDirectory=/var/lib/fluxvm/instances/1a2b/rootfs
MountAPIVFS=yes
ReadOnlyPaths=/
PrivateTmp=yes
PrivateDevices=yes
PrivateIPC=yes
PrivatePIDs=yes
PrivateUsers=no
PrivateNetwork=yes
ProtectHostname=yes
NoNewPrivileges=yes
CapabilityBoundingSet=CAP_CHOWN CAP_DAC_OVERRIDE CAP_FSETID CAP_FOWNER CAP_MKNOD CAP_NET_RAW CAP_SETGID CAP_SETUID CAP_SETFCAP CAP_SETPCAP CAP_NET_BIND_SERVICE CAP_SYS_CHROOT CAP_KILL CAP_AUDIT_WRITE
SystemCallFilter=~ptrace mount umount2 pivot_root chroot kexec_load init_module finit_module delete_module bpf perf_event_open keyctl add_key request_key reboot swapon swapoff open_by_handle_at userfaultfd process_vm_readv process_vm_writev unshare setns
SystemCallErrorNumber=EPERM
RestrictNamespaces=yes
User=1000
WorkingDirectory=/
Environment=\"A=b c\"
";
        assert_eq!(unit_file(&spec(), false), want);
    }

    #[test]
    fn rootless_and_host_net_and_unconfined_change_the_profile() {
        let mut s = spec();
        s.net = Net::Host;
        s.hardening = Hardening { read_only_root: false, seccomp: Seccomp::Unconfined, no_new_privileges: false, capabilities: vec![] };
        let u = unit_file(&s, true);
        assert!(u.contains("PrivateUsers=yes\nPrivateNetwork=no\n"), "{u}");
        assert!(!u.contains("ReadOnlyPaths"));
        assert!(u.contains("CapabilityBoundingSet=\n"));
        assert!(u.contains("SystemCallFilter=(none: seccomp unconfined)"));
        assert!(!u.contains("NoNewPrivileges=yes"));
    }

    #[test]
    fn systemd_run_argv_golden() {
        let mut s = spec();
        s.after.clear();
        s.requires.clear();
        s.limits = Limits { memory_mib: Some(64), ..Default::default() };
        s.restart = Restart::No;
        s.ttl_seconds = None;
        let got = systemd_run_argv(&s, Manager::SystemdUser).join("\n");
        let want = "\
systemd-run
--user
--quiet
--collect
--unit=fx-1a2b3c4d
--description=FluxVM container fx-1a2b3c4d (alpine:3.22)
--service-type=exec
--property=Delegate=yes
--property=MemoryMax=64M
--property=MemorySwapMax=0
--property=Restart=no
--property=KillMode=control-group
--property=TimeoutStopSec=15
--property=NoNewPrivileges=yes
--property=StandardOutput=append:/var/lib/fluxvm/instances/1a2b/console.log
--property=StandardError=append:/var/lib/fluxvm/instances/1a2b/console.log
--property=SyslogIdentifier=fx-1a2b3c4d
--
/usr/bin/fluxvm-container-run
run
--spec
/var/lib/fluxvm/instances/1a2b/spec.json";
        assert_eq!(got, want);
        assert!(!systemd_run_argv(&s, Manager::SystemdSystem).contains(&"--user".to_string()));
    }

    #[test]
    fn cgroup_files_golden() {
        let f = cgroup_files(&spec());
        let got: Vec<String> = f.iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(
            got,
            [
                "memory.max=268435456",
                "memory.swap.max=0",
                "memory.oom.group=1",
                "cpu.max=50000 100000",
                "pids.max=128",
                "io.weight=default 100"
            ]
        );
        assert_eq!(controllers_needed(&spec()), ["memory", "cpu", "pids", "io"]);
        assert!(cgroup_files(&ContainerSpec::default()).is_empty());
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let mut s = spec();
        s.state_dir = PathBuf::from("/var/lib/my fluxvm/x");
        assert!(exec_start(&s).ends_with("run --spec \"/var/lib/my fluxvm/x/spec.json\""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_rendered_filter_is_the_one_the_launcher_applies() {
        let launcher: Vec<&str> = fluxvm_procbox::seccomp::denied_syscalls().iter().map(|(n, _)| *n).collect();
        let rendered: Vec<&str> = SECCOMP_DENIED.iter().copied().take(launcher.len()).collect();
        assert_eq!(launcher, rendered);
        assert_eq!(&SECCOMP_DENIED[launcher.len()..], ["unshare", "setns"]);
    }
}
