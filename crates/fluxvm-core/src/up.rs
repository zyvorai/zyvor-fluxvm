// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl up`: the plan for a new user. Pure rules over [`UpFacts`] (what this machine looks like) produce an
//! [`UpPlan`]: a checklist of steps, each done / to do / warning / blocked, with the plain-English reason and the fix,
//! and the three commands worth running next. `fluxctl up`, `fluxctl up --dry-run` and the Studio Home page
//! (`GET /v1/up/plan`) all use it, so they say the same thing. Codes are the health codes `fluxctl explain` knows.
//!
//! [`probe`] reads the machine (read-only); everything else is pure and unit tested with fixtures.

use crate::health::Platform;
use serde::{Deserialize, Serialize};

/// Version of the JSON document. Adding fields does not bump it.
pub const SCHEMA: u32 = 1;
/// The command a person types (and Studio offers to copy).
pub const COMMAND: &str = "fluxctl up";
/// Name of the default cluster: `fluxctl kube cluster create default`.
pub const CLUSTER_NAME: &str = "default";
/// The VM that runs the cluster's server (`k3s-<name>`), as the daemon lists it.
pub const CLUSTER_VM: &str = "k3s-default";
/// Where the cluster publishes its API server on 127.0.0.1 (the `kube cluster` default).
pub const CLUSTER_API_PORT: u16 = 16443;
/// The default cluster VM gets 4 GiB of memory.
pub const NEED_MEM_MIB: u64 = 4096;
/// Below this much free disk the cluster cannot be created; below `COMFORT_DISK_GIB` it is a warning.
pub const MIN_DISK_GIB: u64 = 10;
pub const COMFORT_DISK_GIB: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Kvm {
    /// Not applicable (not Linux) or not probed.
    #[default]
    Unknown,
    Missing,
    /// `/dev/kvm` exists but this user cannot open it.
    NoAccess,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClusterState {
    #[default]
    Unknown,
    /// No cluster VM exists yet.
    Missing,
    /// An earlier create stopped part way; running it again resumes.
    Incomplete,
    /// The VM exists and is not running.
    Stopped,
    /// The VM runs but the API server does not answer (yet).
    NotAnswering,
    Healthy,
}

/// What the machine looks like. Every number is optional: a probe that could not run is reported, never guessed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UpFacts {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub platform: Platform,
    pub kvm: Kvm,
    /// macOS: whether the hypervisor is available (`kern.hv_support`).
    pub apple_virtualization: Option<bool>,
    pub disk_free_gib: Option<u64>,
    pub mem_total_mib: Option<u64>,
    pub mem_available_mib: Option<u64>,
    /// Programs FluxVM needs that are not on PATH.
    pub missing_tools: Vec<String>,
    /// `None` when the daemon was not asked.
    pub daemon_up: Option<bool>,
    pub cluster: ClusterState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    /// Already fine; `up` changes nothing.
    Done,
    /// `up` will do this.
    Todo,
    /// Works, but deserves a look; does not stop `up`.
    Warn,
    /// `up` cannot continue until the person does the fix.
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub title: String,
    pub state: StepState,
    /// One plain-English line.
    pub detail: String,
    /// A health code `fluxctl explain` knows; present on warnings and blocks.
    pub code: Option<String>,
    /// What to do next, in words or as the exact command.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextCommand {
    pub command: String,
    pub why: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpPlan {
    pub schema: u32,
    /// The command to copy.
    pub command: String,
    pub host: String,
    /// One line: "Ready: nothing to do" / "3 steps to go" / "Cannot start: ...".
    pub headline: String,
    /// The cluster is already healthy and nothing is to do: `fluxctl up` changes nothing.
    pub nothing_to_do: bool,
    /// No step is blocked: `fluxctl up` can run.
    pub can_run: bool,
    pub steps: Vec<Step>,
    pub next_commands: Vec<NextCommand>,
}

fn step(
    id: &str,
    title: &str,
    state: StepState,
    detail: impl Into<String>,
    code: Option<&str>,
    fix: Option<String>,
) -> Step {
    Step {
        id: id.into(),
        title: title.into(),
        state,
        detail: detail.into(),
        code: code.map(str::to_owned),
        fix,
    }
}

fn gib(mib: u64) -> String {
    let tenths = mib * 10 / 1024;
    format!("{}.{} GiB", tenths / 10, tenths % 10)
}

/// How the cluster state follows from the VM status string (`running`, `stopped`, ...) and the API probe.
pub fn cluster_state(vm_status: Option<&str>, api_answers: bool) -> ClusterState {
    match vm_status {
        None => ClusterState::Missing,
        Some("running") if api_answers => ClusterState::Healthy,
        Some("running" | "starting" | "creating" | "booting") => ClusterState::NotAnswering,
        Some(_) => ClusterState::Stopped,
    }
}

/// The status of the VM called `name` in a `GET /v1/vms` document (`{"items": [...]}`).
pub fn vm_status(vms: &serde_json::Value, name: &str) -> Option<String> {
    vms.get("items")?
        .as_array()?
        .iter()
        .find(|v| v["name"].as_str() == Some(name))
        .map(|v| v["status"].as_str().unwrap_or("unknown").to_owned())
}

/// Whether something listens on 127.0.0.1:`port` (the k3s API server the CLI publishes). Read-only.
pub fn api_answers(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(1),
    )
    .is_ok()
}

fn host_step(f: &UpFacts) -> Step {
    let what = format!("{} {}", f.os, f.arch);
    match f.platform {
        Platform::Linux => step(
            "host",
            "This machine",
            StepState::Done,
            format!("{what}: Linux is supported"),
            None,
            None,
        ),
        Platform::Mac if f.arch == "aarch64" || f.arch == "arm64" => step(
            "host",
            "This machine",
            StepState::Done,
            format!("{what}: an Apple silicon Mac is supported"),
            None,
            None,
        ),
        Platform::Mac => step(
            "host",
            "This machine",
            StepState::Warn,
            format!("{what}: an Intel Mac runs Linux VMs but not macOS guests"),
            Some("UNSUPPORTED_HOST"),
            Some("the Kubernetes cluster is a Linux VM and works; use an Apple silicon Mac for macOS guests".into()),
        ),
        Platform::Unknown => step(
            "host",
            "This machine",
            StepState::Blocked,
            format!("{what}: FluxVM runs VMs on Linux (KVM) and on macOS (Apple Virtualization)"),
            Some("UNSUPPORTED_HOST"),
            Some("run `fluxctl up` on a Linux or macOS machine, or use `fluxctl cloud` for a remote one".into()),
        ),
    }
}

fn virtualization_step(f: &UpFacts) -> Step {
    let t = "Hardware virtualization";
    match f.platform {
        Platform::Linux => match f.kvm {
            Kvm::Ready => step("virtualization", t, StepState::Done, "/dev/kvm is present and usable", None, None),
            Kvm::Missing => step(
                "virtualization",
                t,
                StepState::Blocked,
                "/dev/kvm is missing, so VMs cannot run",
                Some("NO_KVM"),
                Some("turn on VT-x/AMD-V in the BIOS (inside a VM: enable nested virtualization), then `sudo modprobe kvm_intel` (or kvm_amd)".into()),
            ),
            Kvm::NoAccess if f.daemon_up == Some(true) => step(
                "virtualization",
                t,
                StepState::Warn,
                "/dev/kvm exists but your user cannot open it; the running daemon is another user and does not need it",
                Some("NO_KVM"),
                Some("only to run VMs as yourself: `sudo usermod -aG kvm $USER`, then log out and in".into()),
            ),
            Kvm::NoAccess => step(
                "virtualization",
                t,
                StepState::Blocked,
                "/dev/kvm exists but your user cannot open it",
                Some("NO_KVM"),
                Some("`sudo usermod -aG kvm $USER`, then log out and in (or start the daemon as root)".into()),
            ),
            Kvm::Unknown => step(
                "virtualization",
                t,
                StepState::Warn,
                "could not check /dev/kvm",
                Some("CHECK_FAILED"),
                Some("run `fluxctl doctor` for details".into()),
            ),
        },
        Platform::Mac => match f.apple_virtualization {
            Some(true) => step("virtualization", t, StepState::Done, "Apple Virtualization is available", None, None),
            Some(false) => step(
                "virtualization",
                t,
                StepState::Blocked,
                "Apple Virtualization is not available on this Mac",
                Some("UNSUPPORTED_HOST"),
                Some("use a Mac that supports the Hypervisor framework (not inside a VM without nested virtualization)".into()),
            ),
            None => step(
                "virtualization",
                t,
                StepState::Warn,
                "could not check Apple Virtualization",
                Some("CHECK_FAILED"),
                Some("run `fluxctl doctor` for details".into()),
            ),
        },
        Platform::Unknown => step(
            "virtualization",
            t,
            StepState::Blocked,
            "no supported hypervisor on this operating system",
            Some("UNSUPPORTED_HOST"),
            None,
        ),
    }
}

fn disk_step(f: &UpFacts) -> Step {
    let t = "Disk space";
    match f.disk_free_gib {
        Some(g) if g < MIN_DISK_GIB => step(
            "disk",
            t,
            StepState::Blocked,
            format!("{g} GiB free; the cluster needs at least {MIN_DISK_GIB} GiB"),
            Some("LOW_DISK"),
            Some("delete VMs, images or backups you no longer need, or move the FluxVM state to a bigger disk".into()),
        ),
        Some(g) if g < COMFORT_DISK_GIB => step(
            "disk",
            t,
            StepState::Warn,
            format!("{g} GiB free; {COMFORT_DISK_GIB} GiB or more is comfortable for the cluster, its images and backups"),
            Some("LOW_DISK"),
            Some("free some space when you can".into()),
        ),
        Some(g) => step("disk", t, StepState::Done, format!("{g} GiB free"), None, None),
        None => step(
            "disk",
            t,
            StepState::Warn,
            "could not read the free disk space",
            Some("CHECK_FAILED"),
            Some("run `fluxctl doctor` for details".into()),
        ),
    }
}

fn memory_step(f: &UpFacts) -> Step {
    let t = "Memory";
    match (f.mem_total_mib, f.mem_available_mib) {
        (Some(t_), _) if t_ < NEED_MEM_MIB => step(
            "memory",
            t,
            StepState::Blocked,
            format!(
                "{} in total; the cluster VM needs {}",
                gib(t_),
                gib(NEED_MEM_MIB)
            ),
            Some("LOW_MEMORY"),
            Some("use a machine with at least 4 GiB of memory (8 GiB is comfortable)".into()),
        ),
        (_, Some(a)) if a < NEED_MEM_MIB => step(
            "memory",
            t,
            StepState::Warn,
            format!(
                "{} free right now; the cluster VM wants {}",
                gib(a),
                gib(NEED_MEM_MIB)
            ),
            Some("LOW_MEMORY"),
            Some("stop a VM or program you are not using, then run `fluxctl up`".into()),
        ),
        (Some(t_), Some(a)) => step(
            "memory",
            t,
            StepState::Done,
            format!("{} free of {}", gib(a), gib(t_)),
            None,
            None,
        ),
        _ => step(
            "memory",
            t,
            StepState::Warn,
            "could not read the memory",
            Some("CHECK_FAILED"),
            Some("run `fluxctl doctor` for details".into()),
        ),
    }
}

fn tools_step(f: &UpFacts) -> Step {
    let t = "Required programs";
    if f.missing_tools.is_empty() {
        return step(
            "tools",
            t,
            StepState::Done,
            "everything FluxVM needs is installed",
            None,
            None,
        );
    }
    let miss: Vec<&str> = f.missing_tools.iter().map(String::as_str).collect();
    let fix = match f.platform {
        Platform::Linux => {
            "install them: `sudo apt-get install qemu-system-x86 qemu-utils cloud-image-utils iproute2` (Fedora: `sudo dnf install qemu-kvm qemu-img cloud-utils iproute`), or run the FluxVM installer"
        }
        _ => {
            "reinstall FluxVM (the install.sh one-liner or `brew install zyvorai/tap/fluxvm`) and open a new shell"
        }
    };
    step(
        "tools",
        t,
        StepState::Blocked,
        format!("not installed: {}", miss.join(", ")),
        Some("MISSING_BINARIES"),
        Some(fix.into()),
    )
}

fn daemon_step(f: &UpFacts) -> Step {
    let t = "FluxVM daemon";
    match f.daemon_up {
        Some(true) => step(
            "daemon",
            t,
            StepState::Done,
            "the daemon is running and will be reused",
            None,
            None,
        ),
        Some(false) => {
            let fix = if f.platform == Platform::Mac {
                "`fluxctl up` starts the LaunchAgent for you; or `fluxctl fix`"
            } else {
                "`sudo systemctl start fluxvm`, then `journalctl -u fluxvm -n 50` if it stops again"
            };
            step(
                "daemon",
                t,
                StepState::Warn,
                "the daemon is not answering; the command line works without it, Studio needs it",
                Some("DAEMON_DOWN"),
                Some(fix.into()),
            )
        }
        None => step("daemon", t, StepState::Done, "not checked", None, None),
    }
}

fn cluster_step(f: &UpFacts) -> Step {
    let t = "Kubernetes cluster";
    let name = CLUSTER_NAME;
    match f.cluster {
        ClusterState::Healthy => step(
            "cluster",
            t,
            StepState::Done,
            format!("the single-server k3s cluster {name} is running and its API answers"),
            None,
            None,
        ),
        ClusterState::Missing => step(
            "cluster",
            t,
            StepState::Todo,
            format!(
                "create the single-server k3s cluster {name} with `fluxctl kube cluster create {name}` (2 CPUs, 4 GiB, 30 GiB disk); k3s is downloaded and its checksum verified first, so the first run takes a few minutes"
            ),
            None,
            None,
        ),
        ClusterState::Incomplete => step(
            "cluster",
            t,
            StepState::Todo,
            format!(
                "an earlier create of the cluster {name} stopped part way; resume it (nothing is created twice)"
            ),
            Some("K3S_NOT_READY"),
            None,
        ),
        ClusterState::Stopped => step(
            "cluster",
            t,
            StepState::Todo,
            format!("start the stopped cluster {name}"),
            Some("K3S_NOT_READY"),
            None,
        ),
        ClusterState::NotAnswering => step(
            "cluster",
            t,
            StepState::Todo,
            format!("the cluster {name} is running but its API does not answer yet; wait for it"),
            Some("K3S_NOT_READY"),
            Some("if it never answers, `fluxctl kube doctor` names the failing check".into()),
        ),
        ClusterState::Unknown => step(
            "cluster",
            t,
            StepState::Todo,
            format!("look for the cluster {name}, then create or start it"),
            None,
            None,
        ),
    }
}

/// The three commands worth running once the cluster is up.
pub fn next_commands() -> Vec<NextCommand> {
    vec![
        NextCommand {
            command: format!(
                "KUBECONFIG=$(fluxctl kube cluster kubeconfig {CLUSTER_NAME}) kubectl get nodes"
            ),
            why: "point kubectl at the new cluster and see its node (needs kubectl)".into(),
        },
        NextCommand {
            command: "fluxctl cluster status".into(),
            why: "see in plain English whether everything is fine, and what to do if not".into(),
        },
        NextCommand {
            command: "fluxctl dashboard".into(),
            why: "open Studio, the web UI: machines, the cluster, images".into(),
        },
    ]
}

/// Judge the facts. The order of the steps is the order `fluxctl up` works through them.
pub fn assess(f: &UpFacts) -> UpPlan {
    let steps = vec![
        host_step(f),
        virtualization_step(f),
        tools_step(f),
        disk_step(f),
        memory_step(f),
        daemon_step(f),
        cluster_step(f),
    ];
    let blocked: Vec<&Step> = steps
        .iter()
        .filter(|s| s.state == StepState::Blocked)
        .collect();
    let todo = steps.iter().filter(|s| s.state == StepState::Todo).count();
    let can_run = blocked.is_empty();
    let nothing_to_do = can_run && todo == 0 && f.cluster == ClusterState::Healthy;
    let headline = if let Some(b) = blocked.first() {
        format!(
            "Cannot start yet: {} ({})",
            b.detail,
            b.title.to_lowercase()
        )
    } else if nothing_to_do {
        "Ready: your cluster is already up and healthy; nothing to do".to_owned()
    } else {
        match todo {
            1 => "1 step to go".to_owned(),
            n => format!("{n} steps to go"),
        }
    };
    UpPlan {
        schema: SCHEMA,
        command: COMMAND.into(),
        host: f.hostname.clone(),
        headline,
        nothing_to_do,
        can_run,
        steps,
        next_commands: next_commands(),
    }
}

// ---- probing -----------------------------------------------------------------------------------------------------------

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file()))
}

/// Programs FluxVM needs on this platform (the same list `fluxctl doctor` requires, without `fluxctl` itself).
pub fn required_tools(platform: Platform, arch: &str) -> Vec<&'static str> {
    match platform {
        Platform::Mac => vec!["fluxvm-vz-runner"],
        Platform::Linux => vec![
            if arch == "aarch64" {
                "qemu-system-aarch64"
            } else {
                "qemu-system-x86_64"
            },
            "qemu-img",
            "ip",
            "cloud-localds",
        ],
        Platform::Unknown => vec![],
    }
}

/// Read this machine. Read-only; `daemon_up` and `cluster` are filled in by the caller, which knows how to ask.
pub fn probe(state_dir: &std::path::Path) -> UpFacts {
    let platform = if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Unknown
    };
    let arch = std::env::consts::ARCH.to_owned();
    let kvm = if cfg!(target_os = "linux") {
        if !std::path::Path::new("/dev/kvm").exists() {
            Kvm::Missing
        } else if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
        {
            Kvm::Ready
        } else {
            Kvm::NoAccess
        }
    } else {
        Kvm::Unknown
    };
    let apple_virtualization = if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "kern.hv_support"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
    } else {
        None
    };
    let pressure = crate::pressure_admission::sample_host();
    UpFacts {
        hostname: crate::health::hostname(),
        os: std::env::consts::OS.to_owned(),
        platform,
        kvm,
        apple_virtualization,
        disk_free_gib: crate::health::free_gib(state_dir),
        mem_total_mib: pressure.mem_total_mib,
        mem_available_mib: pressure.mem_available_mib,
        missing_tools: required_tools(platform, &arch)
            .into_iter()
            .filter(|t| !on_path(t))
            .map(str::to_owned)
            .collect(),
        arch,
        daemon_up: None,
        cluster: ClusterState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> UpFacts {
        UpFacts {
            hostname: "box".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            platform: Platform::Linux,
            kvm: Kvm::Ready,
            apple_virtualization: None,
            disk_free_gib: Some(200),
            mem_total_mib: Some(16384),
            mem_available_mib: Some(9000),
            missing_tools: vec![],
            daemon_up: Some(true),
            cluster: ClusterState::Missing,
        }
    }
    fn state(p: &UpPlan, id: &str) -> StepState {
        p.steps.iter().find(|s| s.id == id).unwrap().state
    }

    #[test]
    fn a_fresh_good_linux_host_has_one_step_to_go() {
        let p = assess(&good());
        assert!(p.can_run && !p.nothing_to_do);
        assert_eq!(p.headline, "1 step to go");
        assert_eq!(state(&p, "cluster"), StepState::Todo);
        assert_eq!(p.command, "fluxctl up");
        assert_eq!(p.next_commands.len(), 3);
    }

    #[test]
    fn a_healthy_cluster_means_nothing_to_do() {
        let mut f = good();
        f.cluster = ClusterState::Healthy;
        let p = assess(&f);
        assert!(p.nothing_to_do && p.can_run);
        assert!(p.headline.contains("nothing to do"));
        assert!(p.steps.iter().all(|s| s.state == StepState::Done));
    }

    #[test]
    fn a_stopped_or_unanswering_cluster_is_a_todo_with_a_code() {
        for c in [
            ClusterState::Stopped,
            ClusterState::NotAnswering,
            ClusterState::Incomplete,
        ] {
            let mut f = good();
            f.cluster = c;
            let p = assess(&f);
            let s = p.steps.iter().find(|s| s.id == "cluster").unwrap();
            assert_eq!(s.state, StepState::Todo);
            assert_eq!(s.code.as_deref(), Some("K3S_NOT_READY"));
            assert!(!p.nothing_to_do);
        }
    }

    #[test]
    fn next_commands_name_the_default_cluster_and_exist() {
        let n = next_commands();
        assert_eq!(n.len(), 3);
        assert!(n[0].command.contains("kube cluster kubeconfig default"));
    }

    #[test]
    fn missing_kvm_blocks_and_says_what_to_do() {
        let mut f = good();
        f.kvm = Kvm::Missing;
        let p = assess(&f);
        assert!(!p.can_run);
        let s = p.steps.iter().find(|s| s.id == "virtualization").unwrap();
        assert_eq!(s.state, StepState::Blocked);
        assert_eq!(s.code.as_deref(), Some("NO_KVM"));
        assert!(s.fix.as_deref().unwrap().contains("BIOS"));
        assert!(p.headline.starts_with("Cannot start yet"));
    }

    #[test]
    fn kvm_without_access_is_only_a_warning_when_the_daemon_runs() {
        let mut f = good();
        f.kvm = Kvm::NoAccess;
        assert_eq!(state(&assess(&f), "virtualization"), StepState::Warn);
        f.daemon_up = Some(false);
        assert_eq!(state(&assess(&f), "virtualization"), StepState::Blocked);
    }

    #[test]
    fn disk_thresholds() {
        let mut f = good();
        f.disk_free_gib = Some(9);
        assert_eq!(state(&assess(&f), "disk"), StepState::Blocked);
        f.disk_free_gib = Some(10);
        assert_eq!(state(&assess(&f), "disk"), StepState::Warn);
        f.disk_free_gib = Some(29);
        assert_eq!(state(&assess(&f), "disk"), StepState::Warn);
        f.disk_free_gib = Some(30);
        assert_eq!(state(&assess(&f), "disk"), StepState::Done);
        f.disk_free_gib = None;
        assert_eq!(state(&assess(&f), "disk"), StepState::Warn);
    }

    #[test]
    fn memory_thresholds() {
        let mut f = good();
        f.mem_total_mib = Some(2048);
        f.mem_available_mib = Some(1500);
        let p = assess(&f);
        assert_eq!(state(&p, "memory"), StepState::Blocked);
        assert!(
            p.steps
                .iter()
                .find(|s| s.id == "memory")
                .unwrap()
                .detail
                .contains("2.0 GiB in total")
        );
        f.mem_total_mib = Some(8192);
        f.mem_available_mib = Some(1000);
        assert_eq!(state(&assess(&f), "memory"), StepState::Warn);
        f.mem_available_mib = Some(4096);
        assert_eq!(state(&assess(&f), "memory"), StepState::Done);
        f.mem_total_mib = None;
        f.mem_available_mib = None;
        assert_eq!(state(&assess(&f), "memory"), StepState::Warn);
    }

    #[test]
    fn missing_tools_block_with_an_install_line() {
        let mut f = good();
        f.missing_tools = vec!["qemu-img".into(), "ip".into()];
        let p = assess(&f);
        let s = p.steps.iter().find(|s| s.id == "tools").unwrap();
        assert_eq!(s.state, StepState::Blocked);
        assert!(s.detail.contains("qemu-img, ip"));
        assert_eq!(s.code.as_deref(), Some("MISSING_BINARIES"));
    }

    #[test]
    fn a_down_daemon_warns_and_does_not_block() {
        let mut f = good();
        f.daemon_up = Some(false);
        let p = assess(&f);
        let s = p.steps.iter().find(|s| s.id == "daemon").unwrap();
        assert_eq!(s.state, StepState::Warn);
        assert!(s.fix.as_deref().unwrap().contains("systemctl start fluxvm"));
        assert!(p.can_run);
        f.platform = Platform::Mac;
        let p = assess(&f);
        assert!(
            p.steps
                .iter()
                .find(|s| s.id == "daemon")
                .unwrap()
                .fix
                .as_deref()
                .unwrap()
                .contains("LaunchAgent")
        );
    }

    #[test]
    fn mac_rules() {
        let mut f = good();
        f.platform = Platform::Mac;
        f.os = "macos".into();
        f.arch = "aarch64".into();
        f.kvm = Kvm::Unknown;
        f.apple_virtualization = Some(true);
        assert!(assess(&f).can_run);
        f.apple_virtualization = Some(false);
        assert!(!assess(&f).can_run);
        f.apple_virtualization = Some(true);
        f.arch = "x86_64".into();
        let p = assess(&f);
        assert_eq!(state(&p, "host"), StepState::Warn);
        assert!(p.can_run);
    }

    #[test]
    fn an_unknown_os_is_blocked() {
        let mut f = good();
        f.platform = Platform::Unknown;
        let p = assess(&f);
        assert!(!p.can_run);
        assert_eq!(state(&p, "host"), StepState::Blocked);
    }

    #[test]
    fn every_warning_and_block_has_a_known_explain_code_and_a_fix() {
        let mut cases = vec![];
        for kvm in [Kvm::Missing, Kvm::NoAccess, Kvm::Unknown] {
            let mut f = good();
            f.kvm = kvm;
            f.daemon_up = Some(false);
            cases.push(f);
        }
        let mut f = good();
        f.disk_free_gib = Some(1);
        f.mem_total_mib = Some(1024);
        f.missing_tools = vec!["ip".into()];
        f.cluster = ClusterState::Stopped;
        cases.push(f);
        let mut f = good();
        f.platform = Platform::Unknown;
        cases.push(f);
        let mut f = good();
        f.disk_free_gib = None;
        f.mem_total_mib = None;
        f.platform = Platform::Mac;
        f.arch = "x86_64".into();
        f.apple_virtualization = None;
        cases.push(f);
        for f in cases {
            for s in assess(&f).steps {
                if matches!(s.state, StepState::Warn | StepState::Blocked) {
                    let c = s
                        .code
                        .as_deref()
                        .unwrap_or_else(|| panic!("{} has no code", s.id));
                    assert!(crate::health::code(c).is_some(), "{c} unknown");
                    if s.id != "virtualization" || f.platform != Platform::Unknown {
                        assert!(s.fix.is_some(), "{} has no fix", s.id);
                    }
                }
            }
        }
    }

    #[test]
    fn cluster_state_from_the_vm_status() {
        assert_eq!(cluster_state(None, false), ClusterState::Missing);
        assert_eq!(cluster_state(Some("running"), true), ClusterState::Healthy);
        assert_eq!(
            cluster_state(Some("running"), false),
            ClusterState::NotAnswering
        );
        assert_eq!(cluster_state(Some("stopped"), true), ClusterState::Stopped);
    }

    #[test]
    fn the_document_round_trips_and_is_versioned() {
        let p = assess(&good());
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["steps"][0]["state"], "done");
        let back: UpPlan = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn vm_status_finds_the_cluster_by_name() {
        let v = serde_json::json!({"items": [{"name": "a", "status": "stopped"}, {"name": "k3s-default", "status": "running"}]});
        assert_eq!(vm_status(&v, CLUSTER_VM).as_deref(), Some("running"));
        assert_eq!(vm_status(&v, "nope"), None);
        assert_eq!(vm_status(&serde_json::json!({}), "a"), None);
    }

    #[test]
    fn required_tools_follow_the_platform() {
        assert_eq!(
            required_tools(Platform::Mac, "aarch64"),
            vec!["fluxvm-vz-runner"]
        );
        assert!(required_tools(Platform::Linux, "aarch64").contains(&"qemu-system-aarch64"));
        assert!(required_tools(Platform::Linux, "x86_64").contains(&"cloud-localds"));
        assert!(required_tools(Platform::Unknown, "x").is_empty());
    }

    #[test]
    fn probe_reads_this_machine_without_panicking() {
        let f = probe(std::path::Path::new("/tmp"));
        assert!(!f.os.is_empty() && !f.arch.is_empty());
    }
}
