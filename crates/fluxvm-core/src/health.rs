// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The cluster health model: one pure, versioned description of "is my cluster OK, in plain English".
//!
//! Everything that can say something about a machine (the doctor checks, the Mac host checks, the fleet probe, the
//! daemon's VM list, the backups folder) is first reduced to [`NodeFacts`]. [`assess`] turns the facts of every machine
//! into a [`ClusterHealth`]: a traffic light per machine and for the cluster, capacity in human terms, and a list of
//! [`Item`]s. Each item has a one-line message a layperson can read, the cause, and a [`Fix`] that says whether it is
//! safe to apply automatically and whether it needs sudo, a restart or a login. `fluxctl cluster status`,
//! `fluxctl fix` and the Studio Home page (`GET /v1/cluster/health`) all use this module, so they cannot disagree.
//!
//! Nothing here touches the machine: no I/O except [`newest_mtime`], a read-only helper for the backups folder. Every
//! rule is unit tested with fixtures.

use serde::{Deserialize, Serialize};

/// Version of the JSON document. Bumped when a field is removed or changes meaning; adding fields does not bump it.
pub const SCHEMA: u32 = 1;
/// Apple's licence allows two macOS guests per Mac at a time.
pub const MACOS_SLOT_LIMIT: u32 = 2;
/// The "small VM" used to say how much room is left: 2 vCPUs and 4 GiB.
pub const SMALL_VM_CPUS: u32 = 2;
pub const SMALL_VM_MEM_MIB: u64 = 4096;
/// A backup older than this is reported.
pub const STALE_BACKUP_SECS: u64 = 7 * 24 * 3600;
const LOW_DISK_GIB: u64 = 20;
const CRITICAL_DISK_GIB: u64 = 5;
const MACOS_DISK_GIB: u64 = 60;

// ---- types -----------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Ok,
    Warn,
    Fail,
}

impl Severity {
    /// The traffic-light word: green, yellow or red.
    pub fn light(self) -> &'static str {
        match self {
            Severity::Ok => "green",
            Severity::Warn => "yellow",
            Severity::Fail => "red",
        }
    }
}

/// What `fluxctl fix` is allowed to do by itself. Deliberately tiny: each is additive and reversible, and none needs
/// sudo or touches a security setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FixAction {
    /// Start the FluxVM LaunchAgent (macOS) that is installed but stopped.
    StartService,
    /// Create a missing directory (and its parents) that FluxVM wants to write to.
    CreateDir { path: String },
    /// Pull an image that a machine is missing.
    PullImage { reference: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Fix {
    /// The exact command, to run on the machine the item belongs to.
    pub command: Option<String>,
    /// Plain-English steps, when there is no single command.
    pub steps: Option<String>,
    /// Additive and reversible: `fluxctl fix` may apply it without being told which one.
    pub safe: bool,
    /// How `fluxctl fix` applies it; present exactly when `safe`.
    pub action: Option<FixAction>,
    pub needs_sudo: bool,
    pub needs_restart: bool,
    pub needs_login: bool,
    /// Changes a security setting (FileVault, auto-login, the screen lock). Never applied automatically.
    pub changes_security: bool,
}

impl Fix {
    fn command(c: impl Into<String>) -> Self {
        Fix {
            command: Some(c.into()),
            ..Default::default()
        }
    }
    fn steps(s: impl Into<String>) -> Self {
        Fix {
            steps: Some(s.into()),
            ..Default::default()
        }
    }
    fn sudo(mut self) -> Self {
        self.needs_sudo = true;
        self
    }
    fn restart(mut self) -> Self {
        self.needs_restart = true;
        self
    }
    fn login(mut self) -> Self {
        self.needs_login = true;
        self
    }
    fn security(mut self) -> Self {
        self.changes_security = true;
        self
    }
    fn auto(mut self, a: FixAction) -> Self {
        self.safe = true;
        self.action = Some(a);
        self
    }
    /// The invariants every fix must keep (checked by a test over all rules).
    pub fn consistent(&self) -> bool {
        if self.safe {
            self.action.is_some() && !self.needs_sudo && !self.changes_security && !self.needs_login
        } else {
            self.action.is_none() && !(self.changes_security && self.command.is_none() && self.steps.is_none())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Stable within a run: `machine/code` (a number is appended when the same code repeats).
    pub id: String,
    pub machine: String,
    /// Where the finding came from: `doctor`, `vz`, `host`, `fleet`, `capacity`, `daemon`.
    pub source: String,
    pub severity: Severity,
    /// One plain-English line.
    pub message: String,
    pub cause: Option<String>,
    pub fix: Option<Fix>,
    /// A code `fluxctl explain` knows (see [`CODES`]); every warning and failure has one.
    pub explain: Option<String>,
}

/// A check as the existing doctors report it, before the model rewrites it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RawCheck {
    pub source: String,
    pub name: String,
    pub severity: Severity,
    pub detail: String,
    /// The doctor's own fix text (prose, often containing a backticked command).
    pub fix: Option<String>,
    /// A directory involved (the Mac store), so a missing one can be created.
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Mac,
    Linux,
    #[default]
    Unknown,
}

/// Everything known about one machine. Every number is optional: a probe that could not run is reported, never guessed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NodeFacts {
    pub name: String,
    pub platform: Platform,
    /// This machine (the one running `fluxctl`) rather than one reached over ssh.
    #[serde(default)]
    pub local: bool,
    #[serde(default)]
    pub reachable: bool,
    pub unreachable_reason: Option<String>,
    pub version: Option<String>,
    /// The version the fleet inventory wants (or a newer release), when known.
    pub wanted_version: Option<String>,
    /// `None` when the daemon was not asked.
    pub daemon_up: Option<bool>,
    pub installed: Option<bool>,
    pub vms_total: Option<u32>,
    pub vms_running: Option<u32>,
    /// macOS guests that hold a slot (running, paused or starting).
    pub macos_in_use: Option<u32>,
    pub cpus_total: Option<u32>,
    pub cpus_free: Option<u32>,
    pub mem_total_mib: Option<u64>,
    pub mem_free_mib: Option<u64>,
    pub disk_free_gib: Option<u64>,
    /// Unix seconds of the newest backup; `None` with `backups_known` means there is none.
    pub last_backup_epoch: Option<u64>,
    #[serde(default)]
    pub backups_known: bool,
    pub k3s: Option<String>,
    /// Images the machine is missing (offered as a safe pull).
    #[serde(default)]
    pub missing_images: Vec<String>,
    #[serde(default)]
    pub checks: Vec<RawCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Capacity {
    /// How many more "small" VMs (2 vCPUs, 4 GiB) fit, by CPU and memory.
    pub room_for_vms: Option<u32>,
    pub macos_in_use: Option<u32>,
    /// Only on a Mac.
    pub macos_limit: Option<u32>,
    pub disk_free_gib: Option<u64>,
    pub mem_free_mib: Option<u64>,
    pub mem_total_mib: Option<u64>,
    /// "room for 3 more VMs, 1 of 2 macOS slots free, 212 GiB disk free".
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub current: String,
    pub wanted: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineHealth {
    pub name: String,
    pub platform: Platform,
    pub local: bool,
    pub light: Severity,
    /// "mini-01 is fine" / "mini-01 needs attention: FluxVM is not running".
    pub headline: String,
    pub version: Option<String>,
    pub update: Option<Update>,
    pub capacity: Capacity,
    pub last_backup_age_secs: Option<u64>,
    pub last_backup: String,
    pub checks_passed: u32,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextAction {
    pub item: Option<String>,
    pub machine: Option<String>,
    /// One sentence: what to do next.
    pub text: String,
    pub command: Option<String>,
    pub safe: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterHealth {
    pub schema: u32,
    pub generated_at: u64,
    pub light: Severity,
    /// "All 2 machines are healthy" / "1 of 2 machines needs attention".
    pub headline: String,
    pub machines: Vec<MachineHealth>,
    /// Problems that belong to no single machine.
    pub items: Vec<Item>,
    pub capacity: Capacity,
    pub next_action: NextAction,
}

impl ClusterHealth {
    /// Every item, cluster-level first, in the order a person should read them (worst first).
    pub fn all_items(&self) -> Vec<&Item> {
        let mut v: Vec<&Item> = self
            .items
            .iter()
            .chain(self.machines.iter().flat_map(|m| m.items.iter()))
            .collect();
        v.sort_by_key(|i| std::cmp::Reverse(i.severity));
        v
    }
}

// ---- explain codes -----------------------------------------------------------------------------------------------------

/// What `fluxctl explain CODE` shows for a health code. Same shape as `fluxvm_core::error_codes::ErrorCode`, without an
/// HTTP status because these are not API errors.
pub struct HealthCode {
    pub code: &'static str,
    pub summary: &'static str,
    pub causes: &'static [&'static str],
    pub fixes: &'static [&'static str],
}

macro_rules! hc {
    ($c:expr, $s:expr, [$($cause:expr),*], [$($fix:expr),*]) => {
        HealthCode { code: $c, summary: $s, causes: &[$($cause),*], fixes: &[$($fix),*] }
    };
}

pub const CODES: &[HealthCode] = &[
    hc!("HOST_UNREACHABLE", "FluxVM could not reach the machine over ssh.",
        ["the machine is off, asleep or on another network", "ssh is off on it, or the key or host key is not accepted"],
        ["check the power and the network", "try `ssh USER@HOST` yourself and read its error", "add the host key the first time with --accept-new-hostkey"]),
    hc!("NOT_INSTALLED", "FluxVM is not installed on the machine.",
        ["the machine was added to the inventory but never set up"],
        ["`fluxctl mac-fleet up HOST` installs it over ssh", "on this machine, follow docs/install.md"]),
    hc!("DAEMON_DOWN", "The FluxVM daemon is not answering on the machine.",
        ["the service is stopped or was never loaded", "it started and crashed (see its log)", "another program holds its port"],
        ["macOS: `launchctl kickstart -k gui/$(id -u)/dev.zyvor.fluxvm` (`fluxctl fix` does this)", "Linux: `sudo systemctl start fluxvm`, then `journalctl -u fluxvm -n 50`"]),
    hc!("NO_KVM", "Hardware virtualization (/dev/kvm) is missing or not usable.",
        ["virtualization is off in the firmware, or this is a VM without nested virtualization", "your user is not in the kvm group"],
        ["enable VT-x/AMD-V in the BIOS", "`sudo usermod -aG kvm $USER`, then log out and in"]),
    hc!("CGROUPS", "The kernel control groups FluxVM needs are not all available.",
        ["cgroup v2 is not mounted, or the cpu/memory/io controllers are not delegated"],
        ["boot with systemd.unified_cgroup_hierarchy=1", "see docs/host-hardening.md"]),
    hc!("OPEN_FILES", "The limit on open files is low for a busy host.",
        ["the service runs with the default 1024 limit"],
        ["raise LimitNOFILE in the service unit, then restart FluxVM"]),
    hc!("NET_DEVICES", "Kernel networking pieces (tun, vhost_net) are not loaded.",
        ["the modules are not loaded"],
        ["`sudo modprobe tun vhost_net`; add them to /etc/modules-load.d to keep them"]),
    hc!("IP_FORWARD", "IP forwarding is off, so VMs cannot reach the network through this host.",
        ["net.ipv4.ip_forward is 0"],
        ["`sudo sysctl -w net.ipv4.ip_forward=1`; set it in /etc/sysctl.d to keep it"]),
    hc!("MISSING_BINARIES", "A program FluxVM needs is not installed.",
        ["the package or its dependency was not installed"],
        ["install the missing program with the system package manager, or re-run the FluxVM installer"]),
    hc!("RUNNER_PROBLEM", "The Mac virtualization helper (fluxvm-vz-runner) is missing, unsigned or lacks its entitlement.",
        ["the runner was replaced by an unsigned build", "an old install is in the way"],
        ["reinstall FluxVM (`fluxctl mac-fleet up HOST`, or the installer on this Mac)", "`fluxctl vz doctor` shows the exact reason"]),
    hc!("LOW_DISK", "Free disk space is low.",
        ["VM disks, images and backups fill the disk", "a macOS guest needs about 60 GiB free"],
        ["delete VMs, images or backups you no longer need (`fluxctl mac images`, `fluxctl backup`)", "move the store to a bigger disk with FLUXVM_MAC_STORE"]),
    hc!("LOW_MEMORY", "Free memory is low.",
        ["running VMs use most of the memory"],
        ["stop a VM you are not using", "start fewer VMs at once"]),
    hc!("NO_LOGIN_SESSION", "Nobody is logged in at the Mac, or its screen is locked, so saved VM state cannot be restored.",
        ["the Mac restarted and sits at the login window", "FileVault blocks automatic login"],
        ["log in once over Screen Sharing", "turn on automatic login (System Settings > Users & Groups); FluxVM never changes it for you"]),
    hc!("SECURITY_SETTING", "A Mac security setting (FileVault, automatic login, screen lock) affects unattended use.",
        ["the Mac is set up for a person at the keyboard, not for a server"],
        ["it is your choice; FluxVM only tells you and never changes these", "see docs/macos-host.md#headless"]),
    hc!("AGENT_NOT_LOADED", "A FluxVM background service is installed but not running.",
        ["the Mac restarted before anyone logged in", "it was unloaded by hand"],
        ["`fluxctl fix` loads it; or `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.zyvor.fluxvm.plist`"]),
    hc!("POWER_SETTINGS", "The Mac may go to sleep, which pauses its VMs.",
        ["sleep, disk sleep or restart-after-power-loss are at their laptop defaults"],
        ["`fluxctl mac host tune` shows the recommended settings; `--apply --only NAME` applies them (asks for sudo)", "`fluxctl mac host keepawake install` holds sleep off without sudo"]),
    hc!("STORE_PROBLEM", "The place FluxVM keeps Mac VMs is missing or not suitable.",
        ["the directory does not exist yet", "it is on a disk that is not APFS or that is slow or full"],
        ["`fluxctl fix` creates a missing directory", "set FLUXVM_MAC_STORE to a folder on an internal APFS disk"]),
    hc!("NO_SSH_KEY", "This Mac has no ssh key, so it cannot be reached or clone from private repositories.",
        ["no key pair exists for the user"],
        ["`ssh-keygen -t ed25519`, then add the public key to the machines or services that need it"]),
    hc!("STALE_PATHS", "Some tools still point at an old FluxVM location.",
        ["FluxVM was moved or upgraded and a client config kept the old path"],
        ["`fluxctl doctor --fix` rewrites the stale entries"]),
    hc!("MACOS_SLOTS_FULL", "The Mac is running the most macOS VMs Apple's licence allows (2).",
        ["two macOS guests are running or paused"],
        ["stop one (`fluxctl mac stop NAME`), or run the job on another Mac (`fluxctl mac-fleet run`)"]),
    hc!("NO_BACKUP", "There is no backup of this machine's VMs.",
        ["no backup was ever taken"],
        ["`fluxctl backup VM` takes one; see docs/backup-verify.md"]),
    hc!("STALE_BACKUP", "The newest backup is more than a week old.",
        ["backups are not scheduled"],
        ["take a fresh one with `fluxctl backup VM`, and schedule it"]),
    hc!("K3S_NOT_READY", "The Kubernetes (k3s) part of the machine is not ready.",
        ["the k3s VM is stopped or still starting", "a node has not joined"],
        ["`fluxctl kube doctor` (or `fluxctl kube apple doctor NAME`) names the failing check and its fix"]),
    hc!("MISSING_IMAGE", "An image the machine needs is not downloaded yet.",
        ["it was never pulled on this machine"],
        ["`fluxctl fix` pulls it; or `fluxctl mac pull REFERENCE`"]),
    hc!("UNSUPPORTED_HOST", "This machine cannot run the kind of VM requested.",
        ["macOS guests need a Mac with Apple silicon on macOS 14 or newer", "this is an Intel Mac or an old macOS"],
        ["use a supported Mac for macOS guests; Linux VMs run on Linux hosts with KVM"]),
    hc!("NO_MACHINES", "FluxVM does not know about any machine yet.",
        ["no daemon is running here and no fleet inventory was given"],
        ["start FluxVM here (`fluxctl service install` on a Mac), or pass `--inventory FILE` (docs/mac-fleet-quickstart.md)"]),
    hc!("CHECK_FAILED", "A host check reported a problem that has no dedicated explanation.",
        ["see the check's own message"],
        ["run the command named in the fix line, or `fluxctl doctor` for details"]),
];

pub fn code(name: &str) -> Option<&'static HealthCode> {
    let n = name.trim().to_ascii_uppercase().replace('-', "_");
    CODES.iter().find(|c| c.code == n)
}

// ---- classification --------------------------------------------------------------------------------------------------

struct Rule {
    code: &'static str,
    /// `{m}` is the machine name.
    headline: &'static str,
}

fn rule_for(name: &str) -> Rule {
    let n = name.to_ascii_lowercase();
    let has = |s: &str| n.contains(s);
    let (code, headline) = if has("daemon") {
        ("DAEMON_DOWN", "FluxVM is not running properly on {m}")
    } else if has("installed") {
        ("NOT_INSTALLED", "FluxVM is not installed on {m}")
    } else if has("kvm") {
        ("NO_KVM", "{m} cannot start virtual machines: hardware virtualization is not usable")
    } else if has("cgroup") {
        ("CGROUPS", "{m} is missing kernel features FluxVM needs (control groups)")
    } else if has("open files") {
        ("OPEN_FILES", "{m} allows too few open files for a busy host")
    } else if has("net devices") {
        ("NET_DEVICES", "{m} is missing network drivers (tun, vhost_net)")
    } else if has("ip forwarding") {
        ("IP_FORWARD", "{m} does not forward network traffic, so VMs cannot reach the network")
    } else if has("binaries") {
        ("MISSING_BINARIES", "{m} is missing a program FluxVM needs")
    } else if has("entitlement") || has("signature") || has("vz runner") || has("virtualization.framework") {
        ("RUNNER_PROBLEM", "The Mac virtualization helper on {m} is missing or not signed correctly")
    } else if has("disk") && !has("sleep") && !has("clone") {
        ("LOW_DISK", "{m} is short on disk space")
    } else if has("memory") {
        ("LOW_MEMORY", "{m} is short on memory")
    } else if has("session") || has("console") || has("screen locked") {
        ("NO_LOGIN_SESSION", "Nobody is logged in at {m}, so saved VMs cannot be restored")
    } else if has("filevault") || has("automatic login") || has("autologin") || has("screen lock") {
        ("SECURITY_SETTING", "A security setting on {m} gets in the way of unattended use")
    } else if has("agent-binary") {
        ("MISSING_BINARIES", "The FluxVM node agent is missing on {m}")
    } else if has("launch agent") || has("agent") && !has("keepawake") {
        ("AGENT_NOT_LOADED", "A FluxVM background service on {m} is not running")
    } else if has("sleep") || has("power") || has("keepawake") || has("autorestart") || has("restart after") || has("thermal") {
        ("POWER_SETTINGS", "{m} may go to sleep or lose power state, which pauses its VMs")
    } else if has("store") || has("clone volume") {
        ("STORE_PROBLEM", "The place {m} keeps Mac VMs needs attention")
    } else if has("ssh key") {
        ("NO_SSH_KEY", "{m} has no ssh key")
    } else if has("stale") || has("client entries") {
        ("STALE_PATHS", "Some tools on {m} point at an old FluxVM location")
    } else if has("k3s") || has("kube") {
        ("K3S_NOT_READY", "Kubernetes on {m} is not ready")
    } else if has("platform") || has("apple silicon") || has("macos version") || has("rosetta") {
        ("UNSUPPORTED_HOST", "{m} cannot run everything FluxVM offers")
    } else {
        ("CHECK_FAILED", "A check on {m} needs attention")
    };
    Rule { code, headline }
}

/// The first `` `command` `` in a doctor's fix text.
fn backticked(text: &str) -> Option<String> {
    let start = text.find('`')? + 1;
    let end = start + text[start..].find('`')?;
    let c = text[start..end].trim();
    (!c.is_empty()).then(|| c.to_owned())
}

fn fix_for(code: &str, platform: Platform, raw: &RawCheck) -> Fix {
    let own = raw.fix.as_deref().unwrap_or("");
    let base = || match backticked(own) {
        Some(c) => {
            let mut f = Fix::command(c);
            if !own.is_empty() {
                f.steps = Some(own.to_owned());
            }
            f
        }
        None if own.is_empty() => Fix::steps("see `fluxctl explain ".to_owned() + code + "`"),
        None => Fix::steps(own),
    };
    let mut f = match code {
        "DAEMON_DOWN" | "AGENT_NOT_LOADED" => match platform {
            Platform::Mac => {
                Fix::command("launchctl kickstart -k gui/$(id -u)/dev.zyvor.fluxvm").auto(FixAction::StartService)
            }
            _ => Fix::command("sudo systemctl start fluxvm").sudo(),
        },
        "NO_KVM" => Fix::command("sudo usermod -aG kvm $USER").sudo().login(),
        "NET_DEVICES" => Fix::command("sudo modprobe tun vhost_net").sudo(),
        "IP_FORWARD" => Fix::command("sudo sysctl -w net.ipv4.ip_forward=1").sudo(),
        "OPEN_FILES" => base().restart(),
        "NO_LOGIN_SESSION" => Fix::steps(
            "log in at the Mac once (Screen Sharing works), and turn on automatic login if it must survive reboots",
        )
        .login(),
        "SECURITY_SETTING" => {
            Fix::steps("this is a security choice; FluxVM never changes FileVault, automatic login or the screen lock").security()
        }
        "POWER_SETTINGS" => Fix::command("fluxctl mac host tune").sudo(),
        "STORE_PROBLEM" => match &raw.path {
            Some(p) => Fix::command(format!("mkdir -p {p}")).auto(FixAction::CreateDir { path: p.clone() }),
            None => base(),
        },
        "RUNNER_PROBLEM" | "NOT_INSTALLED" => base(),
        _ => base(),
    };
    if own.contains("restart") && code != "DAEMON_DOWN" && code != "AGENT_NOT_LOADED" {
        f.needs_restart = true;
    }
    if !f.consistent() {
        // Never ship an inconsistent fix: fall back to advice only.
        f = Fix::steps(own.to_owned());
    }
    f
}

fn item_from_raw(machine: &str, platform: Platform, raw: &RawCheck) -> Option<Item> {
    if raw.severity == Severity::Ok {
        return None;
    }
    let rule = rule_for(&raw.name);
    let fix = fix_for(rule.code, platform, raw);
    Some(Item {
        id: String::new(),
        machine: machine.to_owned(),
        source: raw.source.clone(),
        severity: raw.severity,
        // Codes that repeat with different subjects name the subject, so two lines never read the same.
        message: if matches!(rule.code, "SECURITY_SETTING" | "POWER_SETTINGS" | "CHECK_FAILED") {
            format!("{} ({})", rule.headline.replace("{m}", machine), raw.name)
        } else {
            rule.headline.replace("{m}", machine)
        },
        cause: (!raw.detail.is_empty()).then(|| raw.detail.clone()),
        fix: Some(fix),
        explain: Some(rule.code.to_owned()),
    })
}

fn item(
    machine: &str,
    source: &str,
    severity: Severity,
    code: &str,
    message: String,
    cause: Option<String>,
    fix: Option<Fix>,
) -> Item {
    Item {
        id: String::new(),
        machine: machine.to_owned(),
        source: source.to_owned(),
        severity,
        message,
        cause,
        fix,
        explain: Some(code.to_owned()),
    }
}

// ---- assessment ------------------------------------------------------------------------------------------------------

/// "3 days", "5 hours", "12 minutes".
pub fn human_age(secs: u64) -> String {
    let (n, unit) = match secs {
        s if s >= 2 * 86400 => (s / 86400, "day"),
        s if s >= 3600 => (s / 3600, "hour"),
        s if s >= 60 => (s / 60, "minute"),
        s => (s, "second"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// GiB with no decimals for big numbers.
fn gib(mib: u64) -> String {
    let g = mib as f64 / 1024.0;
    if g >= 10.0 { format!("{g:.0} GiB") } else { format!("{g:.1} GiB") }
}

/// How many more small VMs fit, by CPU and memory.
pub fn room_for_vms(cpus_free: Option<u32>, mem_free_mib: Option<u64>) -> Option<u32> {
    let by_cpu = cpus_free.map(|c| c / SMALL_VM_CPUS);
    let by_mem = mem_free_mib.map(|m| u32::try_from(m / SMALL_VM_MEM_MIB).unwrap_or(u32::MAX));
    match (by_cpu, by_mem) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

pub fn capacity_of(f: &NodeFacts) -> Capacity {
    let limit = (f.platform == Platform::Mac).then_some(MACOS_SLOT_LIMIT);
    let mut c = Capacity {
        room_for_vms: room_for_vms(f.cpus_free, f.mem_free_mib),
        macos_in_use: f.macos_in_use,
        macos_limit: limit,
        disk_free_gib: f.disk_free_gib,
        mem_free_mib: f.mem_free_mib,
        mem_total_mib: f.mem_total_mib,
        summary: String::new(),
    };
    let mut parts = vec![];
    if let Some(r) = c.room_for_vms {
        parts.push(match r {
            0 => "no room for another VM".to_owned(),
            n => format!("room for {n} more VM{}", if n == 1 { "" } else { "s" }),
        });
    }
    if let (Some(used), Some(max)) = (c.macos_in_use, c.macos_limit) {
        parts.push(format!("{used} of {max} macOS slots in use"));
    }
    if let Some(d) = c.disk_free_gib {
        parts.push(format!("{d} GiB disk free"));
    }
    if let (Some(free), Some(total)) = (c.mem_free_mib, c.mem_total_mib) {
        parts.push(format!("{} of {} memory free", gib(free), gib(total)));
    }
    c.summary = if parts.is_empty() { "capacity unknown".to_owned() } else { parts.join(", ") };
    c
}

fn assess_node(f: &NodeFacts, now: u64) -> MachineHealth {
    let m = f.name.as_str();
    let mut items: Vec<Item> = vec![];
    let mut passed = 0u32;

    if !f.reachable {
        let why = f.unreachable_reason.clone().unwrap_or_else(|| "no answer".into());
        items.push(item(
            m,
            "fleet",
            Severity::Fail,
            "HOST_UNREACHABLE",
            format!("{m} cannot be reached"),
            Some(why),
            Some(Fix::steps("check that the machine is on and on the network, then try `ssh` to it by hand")),
        ));
        return finish(f, items, passed, now, Capacity::default());
    }
    if f.installed == Some(false) {
        items.push(item(
            m,
            "fleet",
            Severity::Fail,
            "NOT_INSTALLED",
            format!("FluxVM is not installed on {m}"),
            None,
            Some(Fix::command(format!("fluxctl mac-fleet up {m}"))),
        ));
        return finish(f, items, passed, now, Capacity::default());
    }

    let drop_slot = f.macos_in_use.is_some();
    let has_disk = f.checks.iter().any(|c| c.name.to_ascii_lowercase().contains("free disk") || c.name == "disk");
    let has_daemon = f.checks.iter().any(|c| c.name.to_ascii_lowercase().contains("daemon"));
    for raw in &f.checks {
        let n = raw.name.to_ascii_lowercase();
        if drop_slot && (n.contains("slot") || n.contains("guest limit")) {
            continue;
        }
        match item_from_raw(m, f.platform, raw) {
            Some(i) => items.push(i),
            None => passed += 1,
        }
    }
    if f.daemon_up == Some(false) && !has_daemon {
        let r = RawCheck {
            source: "daemon".into(),
            name: "daemon".into(),
            severity: Severity::Fail,
            detail: "the daemon does not answer /healthz".into(),
            ..Default::default()
        };
        items.extend(item_from_raw(m, f.platform, &r));
    } else if f.daemon_up == Some(true) && !has_daemon {
        passed += 1;
    }

    // capacity, in human terms
    if let Some(used) = f.macos_in_use {
        if f.platform == Platform::Mac {
            if used >= MACOS_SLOT_LIMIT {
                items.push(item(
                    m,
                    "capacity",
                    Severity::Warn,
                    "MACOS_SLOTS_FULL",
                    format!("{used} of {MACOS_SLOT_LIMIT} macOS slots are in use on {m}"),
                    Some("Apple's licence allows two macOS VMs per Mac at a time".into()),
                    Some(Fix::steps("stop one macOS VM, or run the job on another Mac")),
                ));
            } else {
                passed += 1;
            }
        }
    }
    if !has_disk {
        match f.disk_free_gib {
            Some(d) if d < CRITICAL_DISK_GIB || (d < LOW_DISK_GIB && f.platform != Platform::Mac) => {
                items.push(item(
                    m, "capacity", if d < CRITICAL_DISK_GIB { Severity::Fail } else { Severity::Warn }, "LOW_DISK",
                    format!("{m} has only {d} GiB of disk left"),
                    Some("new VMs, images and backups need room".into()),
                    Some(Fix::steps("free space by deleting VMs, images or backups you no longer need")),
                ));
            }
            Some(d) if f.platform == Platform::Mac && d < MACOS_DISK_GIB => items.push(item(
                m, "capacity", Severity::Warn, "LOW_DISK",
                format!("{m} has {d} GiB of disk left; a new macOS VM needs about {MACOS_DISK_GIB} GiB"),
                Some("a macOS guest image and its disk are large".into()),
                Some(Fix::steps("free space, or move the store to a bigger disk with FLUXVM_MAC_STORE")),
            )),
            Some(_) => passed += 1,
            None => {}
        }
    }
    if let (Some(free), Some(total)) = (f.mem_free_mib, f.mem_total_mib) {
        if total > 0 && free * 10 < total && !f.checks.iter().any(|c| c.name.to_ascii_lowercase().contains("memory") && c.severity != Severity::Ok) {
            items.push(item(
                m, "capacity", Severity::Warn, "LOW_MEMORY",
                format!("{m} has only {} of {} memory free", gib(free), gib(total)),
                Some("running VMs use most of it".into()),
                Some(Fix::steps("stop a VM you are not using")),
            ));
        } else {
            passed += 1;
        }
    }
    // backups
    if f.backups_known && f.vms_total.unwrap_or(0) > 0 {
        match f.last_backup_epoch {
            None => items.push(item(
                m, "backup", Severity::Warn, "NO_BACKUP",
                format!("{m} has VMs but no backup yet"),
                Some("nothing has been backed up".into()),
                Some(Fix::command("fluxctl backup VM")),
            )),
            Some(t) if now.saturating_sub(t) > STALE_BACKUP_SECS => items.push(item(
                m, "backup", Severity::Warn, "STALE_BACKUP",
                format!("the newest backup on {m} is {} old", human_age(now.saturating_sub(t))),
                Some("backups are not being taken regularly".into()),
                Some(Fix::command("fluxctl backup VM")),
            )),
            Some(_) => passed += 1,
        }
    }
    if let Some(k) = f.k3s.as_deref() {
        let bad = !matches!(k, "" | "-" | "ready" | "ok" | "up" | "none" | "off" | "running");
        if bad {
            items.push(item(
                m, "fleet", Severity::Warn, "K3S_NOT_READY",
                format!("Kubernetes on {m} is {k}"),
                None,
                Some(Fix::command("fluxctl kube doctor")),
            ));
        } else {
            passed += 1;
        }
    }
    for img in &f.missing_images {
        items.push(item(
            m, "fleet", Severity::Warn, "MISSING_IMAGE",
            format!("the image {img} is not on {m} yet"),
            Some("it was never pulled there".into()),
            Some(Fix::command(format!("fluxctl mac pull {img}")).auto(FixAction::PullImage { reference: img.clone() })),
        ));
    }
    let cap = capacity_of(f);
    finish(f, dedup(items), passed, now, cap)
}

/// Two doctors often report the same thing (the login session, the daemon): keep the worse one. Codes that
/// legitimately repeat with different subjects are left alone.
fn dedup(items: Vec<Item>) -> Vec<Item> {
    const REPEATS: [&str; 4] = ["SECURITY_SETTING", "MISSING_IMAGE", "CHECK_FAILED", "POWER_SETTINGS"];
    let mut out: Vec<Item> = vec![];
    for i in items {
        let code = i.explain.clone().unwrap_or_default();
        if REPEATS.contains(&code.as_str()) {
            out.push(i);
        } else if let Some(prev) = out.iter_mut().find(|o| o.explain.as_deref() == Some(code.as_str())) {
            if i.severity > prev.severity {
                *prev = i;
            }
        } else {
            out.push(i);
        }
    }
    out
}

fn finish(f: &NodeFacts, mut items: Vec<Item>, passed: u32, now: u64, capacity: Capacity) -> MachineHealth {
    let m = f.name.as_str();
    // worst first, stable
    items.sort_by_key(|i| std::cmp::Reverse(i.severity));
    for (n, i) in items.iter_mut().enumerate() {
        let c = i.explain.as_deref().unwrap_or("item").to_ascii_lowercase();
        i.id = format!("{m}/{c}/{}", n + 1);
    }
    let light = items.iter().map(|i| i.severity).max().unwrap_or(Severity::Ok);
    let headline = match (light, items.first()) {
        (Severity::Ok, _) => format!("{m} is healthy"),
        (_, Some(i)) => format!(
            "{m} needs attention: {}",
            i.message.strip_suffix(&format!(" on {m}")).unwrap_or(&i.message)
        ),
        _ => format!("{m} needs attention"),
    };
    let update = match (&f.version, &f.wanted_version) {
        (Some(c), Some(w)) if c != w && newer(w, c) => Some(Update { current: c.clone(), wanted: w.clone() }),
        _ => None,
    };
    let age = f.last_backup_epoch.map(|t| now.saturating_sub(t));
    let last_backup = match (f.backups_known, age) {
        (_, Some(a)) => format!("{} ago", human_age(a)),
        (true, None) => "never".to_owned(),
        (false, None) => "not reported".to_owned(),
    };
    MachineHealth {
        name: f.name.clone(),
        platform: f.platform,
        local: f.local,
        light,
        headline,
        version: f.version.clone(),
        update,
        capacity,
        last_backup_age_secs: age,
        last_backup,
        checks_passed: passed,
        items,
    }
}

/// `a` is a newer dotted version than `b` (non-numeric parts compare as 0). Equal or unparsable is not newer.
fn newer(a: &str, b: &str) -> bool {
    let p = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v').split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).map(|x| x.parse().unwrap_or(0)).collect()
    };
    p(a) > p(b)
}

pub fn assess(nodes: &[NodeFacts], now: u64) -> ClusterHealth {
    let machines: Vec<MachineHealth> = nodes.iter().map(|n| assess_node(n, now)).collect();
    let mut items = vec![];
    if machines.is_empty() {
        items.push(item(
            "cluster", "fleet", Severity::Fail, "NO_MACHINES",
            "FluxVM does not know about any machine yet".into(),
            Some("no daemon answered here and no inventory was given".into()),
            Some(Fix::steps("start FluxVM on this machine, or pass --inventory FILE; see docs/mac-fleet-quickstart.md")),
        ));
    }
    for (n, i) in items.iter_mut().enumerate() {
        i.id = format!("cluster/{}/{}", i.explain.as_deref().unwrap_or("item").to_ascii_lowercase(), n + 1);
    }
    let light = machines.iter().map(|m| m.light).chain(items.iter().map(|i| i.severity)).max().unwrap_or(Severity::Ok);
    let bad = machines.iter().filter(|m| m.light != Severity::Ok).count();
    let headline = if machines.is_empty() {
        "No machines found".to_owned()
    } else if bad == 0 {
        match machines.len() {
            1 => format!("{} is healthy", machines[0].name),
            n => format!("All {n} machines are healthy"),
        }
    } else {
        format!(
            "{bad} of {} machine{} need{} attention",
            machines.len(),
            if machines.len() == 1 { "" } else { "s" },
            if bad == 1 { "s" } else { "" }
        )
    };
    let up: Vec<&MachineHealth> = machines.iter().filter(|m| m.capacity.room_for_vms.is_some() || m.capacity.macos_in_use.is_some()).collect();
    let room: Option<u32> = up.iter().filter_map(|m| m.capacity.room_for_vms).fold(None, |a, r| Some(a.unwrap_or(0) + r));
    let mac_used: u32 = up.iter().filter_map(|m| m.capacity.macos_in_use).sum();
    let mac_max: u32 = up.iter().filter_map(|m| m.capacity.macos_limit).sum();
    let mut capacity = Capacity {
        room_for_vms: room,
        macos_in_use: (mac_max > 0).then_some(mac_used),
        macos_limit: (mac_max > 0).then_some(mac_max),
        disk_free_gib: up.iter().filter_map(|m| m.capacity.disk_free_gib).fold(None, |a, d| Some(a.unwrap_or(0) + d)),
        mem_free_mib: up.iter().filter_map(|m| m.capacity.mem_free_mib).fold(None, |a, d| Some(a.unwrap_or(0) + d)),
        mem_total_mib: up.iter().filter_map(|m| m.capacity.mem_total_mib).fold(None, |a, d| Some(a.unwrap_or(0) + d)),
        summary: String::new(),
    };
    capacity.summary = {
        let mut p = vec![];
        if let Some(r) = capacity.room_for_vms {
            p.push(if r == 0 { "no room for another VM".to_owned() } else { format!("room for {} more", plural(r as u64, "VM", "VMs")) });
        }
        if let (Some(u), Some(l)) = (capacity.macos_in_use, capacity.macos_limit) {
            p.push(format!("{u} of {l} macOS slots in use"));
        }
        if let Some(d) = capacity.disk_free_gib {
            p.push(format!("{d} GiB disk free"));
        }
        if p.is_empty() { "capacity unknown".into() } else { p.join(", ") }
    };
    let mut h = ClusterHealth {
        schema: SCHEMA,
        generated_at: now,
        light,
        headline,
        machines,
        items,
        capacity,
        next_action: NextAction { item: None, machine: None, text: String::new(), command: None, safe: false },
    };
    h.next_action = next_action(&h);
    h
}

/// The one thing to do next: the worst problem, a safe automatic fix first among equals.
pub fn next_action(h: &ClusterHealth) -> NextAction {
    let mut all = h.all_items();
    all.sort_by_key(|i| {
        (
            std::cmp::Reverse(i.severity),
            !i.fix.as_ref().is_some_and(|f| f.safe),
        )
    });
    match all.first() {
        None => NextAction {
            item: None,
            machine: None,
            text: match h.capacity.room_for_vms {
                Some(r) if r > 0 => format!("Nothing to do. There is room for {} more.", plural(r as u64, "VM", "VMs")),
                _ => "Nothing to do.".into(),
            },
            command: None,
            safe: false,
        },
        Some(i) => {
            let fix = i.fix.as_ref();
            let safe = fix.is_some_and(|f| f.safe);
            let command = fix.and_then(|f| f.command.clone());
            let how = if safe {
                "run `fluxctl fix` to do it for you".to_owned()
            } else if let Some(c) = &command {
                format!("run `{c}`{}", if i.machine == "cluster" { String::new() } else { format!(" on {}", i.machine) })
            } else if let Some(s) = fix.and_then(|f| f.steps.clone()) {
                s
            } else {
                format!("run `fluxctl explain {}`", i.explain.as_deref().unwrap_or("CHECK_FAILED"))
            };
            NextAction {
                item: Some(i.id.clone()),
                machine: Some(i.machine.clone()),
                text: format!("{}: {how}", i.message),
                command,
                safe,
            }
        }
    }
}

// ---- facts from the daemon's JSON ---------------------------------------------------------------------------------

/// Fills the VM, CPU, memory and macOS-slot numbers of `f` from `GET /v1/vms` (`{"items": [...]}`) and, when given, the
/// host memory (`GET /v1/host/memory`). `host_cpus` is the machine's CPU count. Used by the CLI (over HTTP) and by the
/// daemon (in process) so both read VMs the same way.
pub fn apply_vm_list(f: &mut NodeFacts, vms: &serde_json::Value, host_cpus: Option<u32>) {
    let Some(items) = vms.get("items").and_then(|v| v.as_array()) else { return };
    let (mut total, mut running, mut used_cpu, mut macos) = (0u32, 0u32, 0u32, 0u32);
    for vm in items {
        total += 1;
        let st = vm["status"].as_str().unwrap_or("");
        if st == "running" {
            running += 1;
        }
        let holds = !matches!(st, "stopped" | "failed" | "deleted" | "deleting");
        if holds {
            used_cpu += u32::try_from(vm["request"]["vcpus"].as_u64().unwrap_or(0)).unwrap_or(0);
            if vm["request"]["apple"]["guest_os"].as_str() == Some("macos") {
                macos += 1;
            }
        }
    }
    f.vms_total = Some(total);
    f.vms_running = Some(running);
    if f.platform == Platform::Mac {
        f.macos_in_use = Some(macos);
    }
    if let Some(c) = host_cpus {
        f.cpus_total = Some(c);
        f.cpus_free = Some(c.saturating_sub(used_cpu));
    }
}

/// Newest modification time (unix seconds) of an entry directly inside `dir`; `None` when the folder is missing or
/// empty. Read-only.
pub fn newest_mtime(dir: &std::path::Path) -> Option<u64> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok()?.metadata().ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
}

/// Free space, in whole GiB, on the filesystem holding `path` (or its nearest existing parent). Read-only.
pub fn free_gib(path: &std::path::Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.to_str()?).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a plain-old-data out parameter that statvfs fills.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64) / (1024 * 1024 * 1024))
}

/// This machine's short host name.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is 256 bytes and gethostname writes at most that many, NUL-terminating when it fits.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(0);
    let full = if ok { String::from_utf8_lossy(&buf[..end]).into_owned() } else { String::new() };
    let short = full.split('.').next().unwrap_or("").to_owned();
    if short.is_empty() { "this-machine".into() } else { short }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_800_000_000;

    fn raw(name: &str, sev: Severity, detail: &str, fix: Option<&str>) -> RawCheck {
        RawCheck { source: "doctor".into(), name: name.into(), severity: sev, detail: detail.into(), fix: fix.map(str::to_owned), path: None }
    }

    fn mac(name: &str) -> NodeFacts {
        NodeFacts {
            name: name.into(),
            platform: Platform::Mac,
            reachable: true,
            installed: Some(true),
            daemon_up: Some(true),
            vms_total: Some(1),
            macos_in_use: Some(0),
            cpus_total: Some(10),
            cpus_free: Some(8),
            mem_total_mib: Some(16384),
            mem_free_mib: Some(12288),
            disk_free_gib: Some(300),
            backups_known: true,
            last_backup_epoch: Some(NOW - 3600),
            ..Default::default()
        }
    }

    #[test]
    fn a_healthy_cluster_says_so_and_has_nothing_to_do() {
        let h = assess(&[mac("mini-01"), mac("mini-02")], NOW);
        assert_eq!(h.light, Severity::Ok);
        assert_eq!(h.headline, "All 2 machines are healthy");
        assert!(h.next_action.text.starts_with("Nothing to do"), "{}", h.next_action.text);
        assert_eq!(h.next_action.item, None);
        assert_eq!(h.capacity.macos_limit, Some(4));
        assert_eq!(h.capacity.room_for_vms, Some(6)); // min(4 by cpu, 3 by memory) per Mac
    }

    #[test]
    fn room_is_the_tighter_of_cpu_and_memory() {
        assert_eq!(room_for_vms(Some(8), Some(12288)), Some(3));
        assert_eq!(room_for_vms(Some(2), Some(40960)), Some(1));
        assert_eq!(room_for_vms(Some(1), Some(40960)), Some(0));
        assert_eq!(room_for_vms(None, Some(8192)), Some(2));
        assert_eq!(room_for_vms(None, None), None);
    }

    #[test]
    fn full_macos_slots_are_explained_in_plain_english() {
        let mut f = mac("mini-01");
        f.macos_in_use = Some(2);
        f.checks.push(raw("macOS VM slots", Severity::Warn, "2/2 used", None));
        let h = assess(&[f], NOW);
        let m = &h.machines[0];
        assert_eq!(m.light, Severity::Warn);
        // the doctor's own slot row is replaced by the model's, not shown twice
        assert_eq!(m.items.len(), 1);
        assert_eq!(m.items[0].message, "2 of 2 macOS slots are in use on mini-01");
        assert_eq!(m.items[0].explain.as_deref(), Some("MACOS_SLOTS_FULL"));
        assert!(m.capacity.summary.contains("2 of 2 macOS slots in use"));
        assert_eq!(h.headline, "1 of 1 machine needs attention");
    }

    #[test]
    fn a_stopped_daemon_on_a_mac_is_a_safe_automatic_fix() {
        let mut f = mac("mini-01");
        f.daemon_up = Some(false);
        let h = assess(&[f], NOW);
        let i = &h.machines[0].items[0];
        assert_eq!(i.severity, Severity::Fail);
        assert_eq!(i.explain.as_deref(), Some("DAEMON_DOWN"));
        let fix = i.fix.as_ref().unwrap();
        assert!(fix.safe && !fix.needs_sudo);
        assert_eq!(fix.action, Some(FixAction::StartService));
        assert!(h.next_action.safe);
        assert!(h.next_action.text.contains("fluxctl fix"), "{}", h.next_action.text);
    }

    #[test]
    fn a_stopped_daemon_on_linux_needs_sudo_and_is_never_automatic() {
        let mut f = NodeFacts { name: "box".into(), platform: Platform::Linux, reachable: true, daemon_up: Some(false), ..Default::default() };
        f.checks.push(raw("daemon", Severity::Fail, "connection refused", Some("run `fluxctl serve`")));
        let h = assess(&[f], NOW);
        let fix = h.machines[0].items[0].fix.as_ref().unwrap();
        assert!(!fix.safe && fix.needs_sudo && fix.action.is_none());
        assert_eq!(fix.command.as_deref(), Some("sudo systemctl start fluxvm"));
    }

    #[test]
    fn security_settings_are_never_automatic() {
        let mut f = mac("m");
        f.checks.push(raw("FileVault (host)", Severity::Warn, "on", Some("System Settings > Privacy & Security")));
        f.checks.push(raw("automatic login", Severity::Fail, "off", None));
        let h = assess(&[f], NOW);
        for i in &h.machines[0].items {
            let fx = i.fix.as_ref().unwrap();
            assert!(fx.changes_security && !fx.safe, "{i:?}");
            assert_eq!(i.explain.as_deref(), Some("SECURITY_SETTING"));
        }
    }

    #[test]
    fn a_missing_store_directory_can_be_created_and_nothing_else_can() {
        let mut f = mac("m");
        let mut r = raw("store location", Severity::Fail, "does not exist", None);
        r.path = Some("/Volumes/Fast/fluxvm".into());
        f.checks.push(r);
        f.checks.push(raw("login session", Severity::Fail, "login window", None));
        let h = assess(&[f], NOW);
        let safe: Vec<_> = h.machines[0].items.iter().filter(|i| i.fix.as_ref().unwrap().safe).collect();
        assert_eq!(safe.len(), 1);
        assert_eq!(safe[0].fix.as_ref().unwrap().action, Some(FixAction::CreateDir { path: "/Volumes/Fast/fluxvm".into() }));
        let login = h.machines[0].items.iter().find(|i| i.explain.as_deref() == Some("NO_LOGIN_SESSION")).unwrap();
        assert!(login.fix.as_ref().unwrap().needs_login);
    }

    #[test]
    fn a_missing_image_is_a_safe_pull() {
        let mut f = mac("m");
        f.missing_images = vec!["ghcr.io/acme/mac-ci:1".into()];
        let h = assess(&[f], NOW);
        let fx = h.machines[0].items[0].fix.as_ref().unwrap();
        assert_eq!(fx.action, Some(FixAction::PullImage { reference: "ghcr.io/acme/mac-ci:1".into() }));
    }

    #[test]
    fn unreachable_and_not_installed_stop_further_checks() {
        let down = NodeFacts { name: "mini-03".into(), platform: Platform::Mac, reachable: false, unreachable_reason: Some("Connection timed out".into()), ..Default::default() };
        let fresh = NodeFacts { name: "mini-04".into(), platform: Platform::Mac, reachable: true, installed: Some(false), ..Default::default() };
        let h = assess(&[down, fresh], NOW);
        assert_eq!(h.light, Severity::Fail);
        assert_eq!(h.machines[0].items[0].explain.as_deref(), Some("HOST_UNREACHABLE"));
        assert_eq!(h.machines[0].items[0].cause.as_deref(), Some("Connection timed out"));
        assert_eq!(h.machines[1].items[0].fix.as_ref().unwrap().command.as_deref(), Some("fluxctl mac-fleet up mini-04"));
        assert_eq!(h.headline, "2 of 2 machines need attention");
    }

    #[test]
    fn backups_are_reported_in_age_and_only_when_there_are_vms() {
        let mut f = mac("m");
        f.last_backup_epoch = Some(NOW - 9 * 86400);
        let h = assess(&[f.clone()], NOW);
        assert_eq!(h.machines[0].items[0].explain.as_deref(), Some("STALE_BACKUP"));
        assert_eq!(h.machines[0].last_backup, "9 days ago");
        f.last_backup_epoch = None;
        assert_eq!(assess(&[f.clone()], NOW).machines[0].items[0].explain.as_deref(), Some("NO_BACKUP"));
        assert_eq!(assess(&[f.clone()], NOW).machines[0].last_backup, "never");
        f.vms_total = Some(0);
        assert!(assess(&[f], NOW).machines[0].items.is_empty());
    }

    #[test]
    fn low_disk_is_graded_and_deduplicated_against_the_doctor() {
        let mut f = mac("m");
        f.disk_free_gib = Some(30);
        let h = assess(&[f.clone()], NOW);
        assert!(h.machines[0].items[0].message.contains("a new macOS VM needs about 60 GiB"));
        f.disk_free_gib = Some(3);
        assert_eq!(assess(&[f.clone()], NOW).machines[0].items[0].severity, Severity::Fail);
        f.checks.push(raw("free disk", Severity::Warn, "30 GiB", None));
        let h = assess(&[f], NOW);
        assert_eq!(h.machines[0].items.len(), 1, "one disk item, not two");
    }

    #[test]
    fn update_is_reported_but_does_not_turn_the_light_yellow() {
        let mut f = mac("m");
        f.version = Some("0.4.20".into());
        f.wanted_version = Some("0.4.21".into());
        let h = assess(&[f.clone()], NOW);
        assert_eq!(h.light, Severity::Ok);
        assert_eq!(h.machines[0].update, Some(Update { current: "0.4.20".into(), wanted: "0.4.21".into() }));
        f.wanted_version = Some("0.4.9".into());
        assert_eq!(assess(&[f], NOW).machines[0].update, None, "0.4.9 is older than 0.4.20");
    }

    #[test]
    fn next_action_prefers_the_worst_then_the_safe_one() {
        let mut a = mac("a");
        a.checks.push(raw("sleep", Severity::Warn, "sleep 1", None));
        let mut b = mac("b");
        b.daemon_up = Some(false);
        let h = assess(&[a, b], NOW);
        assert_eq!(h.next_action.machine.as_deref(), Some("b"));
        assert!(h.next_action.safe);
    }

    #[test]
    fn no_machines_is_a_failure_with_a_way_forward() {
        let h = assess(&[], NOW);
        assert_eq!(h.light, Severity::Fail);
        assert_eq!(h.items[0].explain.as_deref(), Some("NO_MACHINES"));
        assert_eq!(h.headline, "No machines found");
    }

    #[test]
    fn every_problem_has_an_explain_code_that_exists_and_a_consistent_fix() {
        let names = [
            "daemon", "installed", "kvm", "cgroups", "open files", "net devices", "ip forwarding", "binaries", "vmnet entitlement",
            "runner signature", "vz runner", "Virtualization.framework", "free disk", "memory", "login session", "console session",
            "FileVault (host)", "automatic login", "screen lock", "launch agents", "keepawake agent", "sleep held off by", "idle sleep",
            "power source", "store location", "clone volume", "ssh key", "client entries", "stale paths", "k3s", "platform",
            "apple silicon", "macOS version", "Rosetta", "something nobody has heard of",
        ];
        let mut seen = std::collections::BTreeSet::new();
        for plat in [Platform::Mac, Platform::Linux, Platform::Unknown] {
            for n in names {
                for sev in [Severity::Warn, Severity::Fail] {
                    for fix in [None, Some("do `a thing` then restart"), Some("plain words")] {
                        let i = item_from_raw("m", plat, &raw(n, sev, "d", fix)).unwrap();
                        let code = i.explain.clone().unwrap();
                        assert!(super::code(&code).is_some(), "{n}: {code} is not in CODES");
                        seen.insert(code);
                        let fx = i.fix.unwrap();
                        assert!(fx.consistent(), "{n}: {fx:?}");
                        assert!(fx.command.is_some() || fx.steps.is_some(), "{n}: a fix says nothing");
                    }
                }
            }
        }
        // every code the model can emit is explained, and every explained code can be emitted by some path
        for extra in ["HOST_UNREACHABLE", "MACOS_SLOTS_FULL", "NO_BACKUP", "STALE_BACKUP", "MISSING_IMAGE", "NO_MACHINES", "LOW_MEMORY"] {
            seen.insert(extra.to_owned());
        }
        for c in CODES {
            assert!(seen.contains(c.code), "{} is never produced", c.code);
            assert!(!c.causes.is_empty() && !c.fixes.is_empty() && !c.summary.is_empty());
        }
    }

    #[test]
    fn the_same_problem_from_two_doctors_is_listed_once() {
        let mut f = mac("m");
        f.checks.push(raw("session", Severity::Warn, "locked", None));
        let mut b = raw("login session", Severity::Fail, "nobody", None);
        b.source = "vz".into();
        f.checks.push(b);
        let h = assess(&[f], NOW);
        assert_eq!(h.machines[0].items.len(), 1);
        assert_eq!(h.machines[0].items[0].severity, Severity::Fail);
    }

    #[test]
    fn json_document_is_versioned_and_round_trips() {
        let mut f = mac("m");
        f.daemon_up = Some(false);
        let h = assess(&[f], NOW);
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["light"], "fail");
        assert_eq!(v["machines"][0]["items"][0]["fix"]["action"]["kind"], "start_service");
        let back: ClusterHealth = serde_json::from_value(v).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn vm_list_fills_counts_cpu_and_macos_slots() {
        let mut f = NodeFacts { name: "m".into(), platform: Platform::Mac, ..Default::default() };
        let vms = json!({"items": [
            {"status": "running", "request": {"vcpus": 4, "apple": {"guest_os": "macos"}}},
            {"status": "paused", "request": {"vcpus": 2, "apple": {"guest_os": "macos"}}},
            {"status": "stopped", "request": {"vcpus": 8, "apple": {"guest_os": "macos"}}},
            {"status": "running", "request": {"vcpus": 2}},
        ]});
        apply_vm_list(&mut f, &vms, Some(10));
        assert_eq!((f.vms_total, f.vms_running, f.macos_in_use), (Some(4), Some(2), Some(2)));
        assert_eq!(f.cpus_free, Some(2));
        let mut l = NodeFacts { platform: Platform::Linux, ..Default::default() };
        apply_vm_list(&mut l, &vms, None);
        assert_eq!(l.macos_in_use, None);
    }

    #[test]
    fn ages_read_naturally() {
        assert_eq!(human_age(1), "1 second");
        assert_eq!(human_age(150), "2 minutes");
        assert_eq!(human_age(3 * 3600), "3 hours");
        assert_eq!(human_age(86400), "24 hours");
        assert_eq!(human_age(3 * 86400), "3 days");
    }

    #[test]
    fn free_space_and_hostname_read_this_machine() {
        let d = tempfile::tempdir().unwrap();
        assert!(free_gib(d.path()).is_some());
        assert!(free_gib(&d.path().join("not/yet/created")).is_some(), "falls back to the nearest parent");
        assert!(!hostname().is_empty() && !hostname().contains('.'));
    }

    #[test]
    fn newest_mtime_reads_a_folder() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(newest_mtime(d.path()), None);
        std::fs::write(d.path().join("a.qcow2"), b"x").unwrap();
        assert!(newest_mtime(d.path()).unwrap() > 1_700_000_000);
        assert_eq!(newest_mtime(&d.path().join("missing")), None);
    }
}
