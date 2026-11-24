// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

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
            self.action.is_none()
                && !(self.changes_security && self.command.is_none() && self.steps.is_none())
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
    hc!(
        "HOST_UNREACHABLE",
        "FluxVM could not reach the machine over ssh.",
        [
            "the machine is off, asleep or on another network",
            "ssh is off on it, or the key or host key is not accepted"
        ],
        [
            "check the power and the network",
            "try `ssh USER@HOST` yourself and read its error",
            "add the host key the first time with --accept-new-hostkey"
        ]
    ),
    hc!(
        "NOT_INSTALLED",
        "FluxVM is not installed on the machine.",
        ["the machine was added to the inventory but never set up"],
        [
            "`fluxctl mac-fleet up HOST` installs it over ssh",
            "on this machine, follow docs/install.md"
        ]
    ),
    hc!(
        "DAEMON_DOWN",
        "The FluxVM daemon is not answering on the machine.",
        [
            "the service is stopped or was never loaded",
            "it started and crashed (see its log)",
            "another program holds its port"
        ],
        [
            "macOS: `launchctl kickstart -k gui/$(id -u)/dev.zyvor.fluxvm` (`fluxctl fix` does this)",
            "Linux: `sudo systemctl start fluxvm`, then `journalctl -u fluxvm -n 50`"
        ]
    ),
    hc!(
        "NO_KVM",
        "Hardware virtualization (/dev/kvm) is missing or not usable.",
        [
            "virtualization is off in the firmware, or this is a VM without nested virtualization",
            "your user is not in the kvm group"
        ],
        [
            "enable VT-x/AMD-V in the BIOS",
            "`sudo usermod -aG kvm $USER`, then log out and in"
        ]
    ),
    hc!(
        "CGROUPS",
        "The kernel control groups FluxVM needs are not all available.",
        ["cgroup v2 is not mounted, or the cpu/memory/io controllers are not delegated"],
        [
            "boot with systemd.unified_cgroup_hierarchy=1",
            "see docs/host-hardening.md"
        ]
    ),
    hc!(
        "OPEN_FILES",
        "The limit on open files is low for a busy host.",
        ["the service runs with the default 1024 limit"],
        ["raise LimitNOFILE in the service unit, then restart FluxVM"]
    ),
    hc!(
        "NET_DEVICES",
        "Kernel networking pieces (tun, vhost_net) are not loaded.",
        ["the modules are not loaded"],
        ["`sudo modprobe tun vhost_net`; add them to /etc/modules-load.d to keep them"]
    ),
    hc!(
        "IP_FORWARD",
        "IP forwarding is off, so VMs cannot reach the network through this host.",
        ["net.ipv4.ip_forward is 0"],
        ["`sudo sysctl -w net.ipv4.ip_forward=1`; set it in /etc/sysctl.d to keep it"]
    ),
    hc!(
        "MISSING_BINARIES",
        "A program FluxVM needs is not installed.",
        ["the package or its dependency was not installed"],
        [
            "install the missing program with the system package manager, or re-run the FluxVM installer"
        ]
    ),
    hc!(
        "RUNNER_PROBLEM",
        "The Mac virtualization helper (fluxvm-vz-runner) is missing, unsigned or lacks its entitlement.",
        [
            "the runner was replaced by an unsigned build",
            "an old install is in the way"
        ],
        [
            "reinstall FluxVM (`fluxctl mac-fleet up HOST`, or the installer on this Mac)",
            "`fluxctl vz doctor` shows the exact reason"
        ]
    ),
    hc!(
        "LOW_DISK",
        "Free disk space is low.",
        [
            "VM disks, images and backups fill the disk",
            "a macOS guest needs about 60 GiB free"
        ],
        [
            "delete VMs, images or backups you no longer need (`fluxctl mac images`, `fluxctl backup`)",
            "move the store to a bigger disk with FLUXVM_MAC_STORE"
        ]
    ),
    hc!(
        "LOW_MEMORY",
        "Free memory is low.",
        ["running VMs use most of the memory"],
        ["stop a VM you are not using", "start fewer VMs at once"]
    ),
    hc!(
        "NO_LOGIN_SESSION",
        "Nobody is logged in at the Mac, or its screen is locked, so saved VM state cannot be restored.",
        [
            "the Mac restarted and sits at the login window",
            "FileVault blocks automatic login"
        ],
        [
            "log in once over Screen Sharing",
            "turn on automatic login (System Settings > Users & Groups); FluxVM never changes it for you"
        ]
    ),
    hc!(
        "SECURITY_SETTING",
        "A Mac security setting (FileVault, automatic login, screen lock) affects unattended use.",
        ["the Mac is set up for a person at the keyboard, not for a server"],
        [
            "it is your choice; FluxVM only tells you and never changes these",
            "see docs/macos-host.md#headless"
        ]
    ),
    hc!(
        "AGENT_NOT_LOADED",
        "A FluxVM background service is installed but not running.",
        [
            "the Mac restarted before anyone logged in",
            "it was unloaded by hand"
        ],
        [
            "`fluxctl fix` loads it; or `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.zyvor.fluxvm.plist`"
        ]
    ),
    hc!(
        "POWER_SETTINGS",
        "The Mac may go to sleep, which pauses its VMs.",
        ["sleep, disk sleep or restart-after-power-loss are at their laptop defaults"],
        [
            "`fluxctl mac host tune` shows the recommended settings; `--apply --only NAME` applies them (asks for sudo)",
            "`fluxctl mac host keepawake install` holds sleep off without sudo"
        ]
    ),
    hc!(
        "STORE_PROBLEM",
        "The place FluxVM keeps Mac VMs is missing or not suitable.",
        [
            "the directory does not exist yet",
            "it is on a disk that is not APFS or that is slow or full"
        ],
        [
            "`fluxctl fix` creates a missing directory",
            "set FLUXVM_MAC_STORE to a folder on an internal APFS disk"
        ]
    ),
    hc!(
        "NO_SSH_KEY",
        "This Mac has no ssh key, so it cannot be reached or clone from private repositories.",
        ["no key pair exists for the user"],
        [
            "`ssh-keygen -t ed25519`, then add the public key to the machines or services that need it"
        ]
    ),
    hc!(
        "STALE_PATHS",
        "Some tools still point at an old FluxVM location.",
        ["FluxVM was moved or upgraded and a client config kept the old path"],
        ["`fluxctl doctor --fix` rewrites the stale entries"]
    ),
    hc!(
        "MACOS_SLOTS_FULL",
        "The Mac is running the most macOS VMs Apple's licence allows (2).",
        ["two macOS guests are running or paused"],
        [
            "stop one (`fluxctl mac stop NAME`), or run the job on another Mac (`fluxctl mac-fleet run`)"
        ]
    ),
    hc!(
        "NO_BACKUP",
        "There is no backup of this machine's VMs.",
        ["no backup was ever taken"],
        ["`fluxctl backup VM` takes one; see docs/backup-verify.md"]
    ),
    hc!(
        "STALE_BACKUP",
        "The newest backup is more than a week old.",
        ["backups are not scheduled"],
        ["take a fresh one with `fluxctl backup VM`, and schedule it"]
    ),
    hc!(
        "K3S_NOT_READY",
        "The Kubernetes (k3s) part of the machine is not ready.",
        [
            "the k3s VM is stopped or still starting",
            "a node has not joined"
        ],
        [
            "`fluxctl kube doctor` (or `fluxctl kube apple doctor NAME`) names the failing check and its fix"
        ]
    ),
    hc!(
        "MISSING_IMAGE",
        "An image the machine needs is not downloaded yet.",
        ["it was never pulled on this machine"],
        ["`fluxctl fix` pulls it; or `fluxctl mac pull REFERENCE`"]
    ),
    hc!(
        "UNSUPPORTED_HOST",
        "This machine cannot run the kind of VM requested.",
        [
            "macOS guests need a Mac with Apple silicon on macOS 14 or newer",
            "this is an Intel Mac or an old macOS"
        ],
        ["use a supported Mac for macOS guests; Linux VMs run on Linux hosts with KVM"]
    ),
    hc!(
        "NO_MACHINES",
        "FluxVM does not know about any machine yet.",
        ["no daemon is running here and no fleet inventory was given"],
        [
            "start FluxVM here (`fluxctl service install` on a Mac), or pass `--inventory FILE` (docs/mac-fleet-quickstart.md)"
        ]
    ),
    hc!(
        "ENVOY_GATEWAY_NOT_INSTALLED",
        "You have Gateways or routes that use Envoy Gateway, but Envoy Gateway is not running in the cluster.",
        [
            "Envoy Gateway was never installed on this cluster, or was removed",
            "its controller (deployment envoy-gateway in envoy-gateway-system) is not running"
        ],
        [
            "install it with this one command (NAME is the cluster shown by `fluxctl cluster status`): `fluxctl kube cluster platform envoy-gateway NAME`",
            "then `fluxctl cluster status` again; the Gateways become Programmed within a minute"
        ]
    ),
    hc!(
        "GATEWAY_NOT_PROGRAMMED",
        "A Gateway has not been set up by its controller, so it is not serving traffic.",
        [
            "the controller rejected the Gateway (a bad listener, a port clash, a missing certificate Secret)",
            "the proxy pods behind it are not running or have no address yet"
        ],
        [
            "`kubectl describe gateway NAME -n NS` shows the controller's reason",
            "`fluxctl gateway inspect ROUTE -n NS` for a route on it",
            "fix the listener, then wait a minute; a Gateway that was just changed shows as still being set up, not as failed"
        ]
    ),
    hc!(
        "ROUTE_NOT_ACCEPTED",
        "A route was not accepted by the Gateway it points at, so it does nothing.",
        [
            "the route names a Gateway or listener that does not exist",
            "the listener does not allow routes from this namespace, or the hostname or protocol does not match"
        ],
        [
            "`fluxctl gateway inspect ROUTE -n NS` shows the reason the Gateway gave",
            "check the route's parentRefs (Gateway name, namespace, sectionName) and the listener's allowedRoutes"
        ]
    ),
    hc!(
        "ROUTE_BACKEND_UNRESOLVED",
        "A route points at a Service it cannot use, so requests to it fail.",
        [
            "the Service does not exist, or is in another namespace without a ReferenceGrant",
            "the port number is not one of the Service's ports"
        ],
        [
            "`fluxctl gateway inspect ROUTE -n NS` lists each backend and whether its port was found",
            "create the Service or fix the name and port in the route; re-run fluxctl gateway expose to write a correct route"
        ]
    ),
    hc!(
        "BACKEND_NO_READY_ENDPOINTS",
        "A route's Service has no ready pods, so the Gateway has nowhere to send requests.",
        [
            "the pods are still starting, crashing, or failing their readiness check",
            "the Service selector matches no pods"
        ],
        [
            "`kubectl get pods -n NS` and `kubectl describe service NAME -n NS`",
            "`fluxctl gateway inspect ROUTE -n NS` shows the ready and not-ready endpoint counts"
        ]
    ),
    hc!(
        "SUPPORT_BUNDLE_SECRET_LEFT",
        "The support bundle was not written because its final check still found something that looks like a secret.",
        [
            "a log or setting held a credential in a form the redaction does not know",
            "nothing was written or sent: the check fails closed on purpose"
        ],
        [
            "send the output of `fluxctl cluster status --json` and `fluxctl doctor` by hand instead",
            "report the pattern name from the message so the redaction can learn it"
        ]
    ),
    hc!(
        "SUPPORT_BUNDLE_WRITE_FAILED",
        "The support bundle file could not be written.",
        [
            "the folder does not exist or is not writable",
            "the disk is full",
            "the output file already exists"
        ],
        [
            "choose another place: `fluxctl support-bundle --out ~/bundle.tar.gz`",
            "free disk space (`fluxctl cluster status` shows how much is left)"
        ]
    ),
    hc!(
        "BACKUP_DISK_LOW",
        "There is not enough free disk space to take a cluster backup safely.",
        [
            "less than the size of the last backup plus 2 GiB is free where the backups live",
            "old backups, VM disks or images fill the disk"
        ],
        [
            "free space: `fluxctl kube backups` shows what the backups use; old scheduled ones are removed automatically when room is needed",
            "send backups to a bigger disk with `--to DIR`"
        ]
    ),
    hc!(
        "BACKUP_CHECKSUM_MISMATCH",
        "A cluster backup does not match its recorded checksum, so it is not trusted.",
        [
            "the file was edited, truncated or corrupted after it was made",
            "the .sha256 file belongs to a different backup"
        ],
        [
            "restore from an earlier backup: `fluxctl kube backups`",
            "check each with `fluxctl kube backup verify BACKUP`"
        ]
    ),
    hc!(
        "BACKUP_NOT_RESTORABLE",
        "A cluster backup is missing something a restore needs, or is not a complete archive.",
        [
            "the copy was cut short (disk full, interrupted transfer)",
            "it holds the wrong datastore for this cluster, or no cluster token",
            "it has no .sha256 file next to it"
        ],
        [
            "take a new one: `fluxctl kube backup`",
            "see which check failed: `fluxctl kube backup verify BACKUP`"
        ]
    ),
    hc!(
        "BACKUP_NOT_FOUND",
        "No cluster backup with that name exists.",
        [
            "the name is mistyped, or retention removed the backup",
            "the backups folder was moved"
        ],
        ["list them: `fluxctl kube backups`"]
    ),
    hc!(
        "RESTORE_NEEDS_CONFIRM",
        "A restore replaces the whole cluster state, so it asks for confirmation first.",
        [
            "--yes was not given (CLI), or the backup name was not repeated to confirm (Studio and API)"
        ],
        [
            "check the backup first: `fluxctl kube backup verify BACKUP`",
            "then `fluxctl kube restore BACKUP --yes`; a backup of the current state is taken first"
        ]
    ),
    hc!(
        "CHECK_FAILED",
        "A host check reported a problem that has no dedicated explanation.",
        ["see the check's own message"],
        ["run the command named in the fix line, or `fluxctl doctor` for details"]
    ),
    hc!(
        "APP_IMAGE_PULL",
        "The cluster cannot download the image an app runs.",
        [
            "the image name or tag is misspelled, or the tag does not exist",
            "the image is private and the cluster has no credentials for it",
            "the image only exists on this machine, so the cluster has nothing to pull",
            "the cluster has no internet (VPN, DNS or a captive network)"
        ],
        [
            "check the name works: `docker pull IMAGE`",
            "for an image built on this machine: `fluxctl kube load IMAGE`, then run the app again",
            "see the exact error: `kubectl describe pod -l app.kubernetes.io/name=APP`",
            "remove the app: `fluxctl kube rm APP`"
        ]
    ),
    hc!(
        "APP_CRASHING",
        "An app starts and then stops, over and over (CrashLoopBackOff), or cannot start because its settings are wrong.",
        [
            "the program exits with an error right after it starts",
            "it needs an environment variable, file or secret that is not there",
            "it listens on a different port than the one given with --port"
        ],
        [
            "read why it stopped: `fluxctl kube logs APP --previous`",
            "pass settings with `--env NAME=value` when you run it",
            "start again with the right port: `fluxctl kube run IMAGE --port N`"
        ]
    ),
    hc!(
        "APP_OUT_OF_MEMORY",
        "An app used more memory than the cluster could give it and was stopped.",
        [
            "the app needs more memory than the cluster's machine has",
            "several apps share a small cluster"
        ],
        [
            "give the cluster more memory: `fluxctl kube up --memory-mib 8192`",
            "run fewer copies: `fluxctl kube scale APP 1`"
        ]
    ),
    hc!(
        "APP_NOT_READY",
        "An app did not become ready in time, or no machine in the cluster has room for it.",
        [
            "the image is still downloading (large images take minutes the first time)",
            "the cluster is out of CPU or memory",
            "the app's health check is failing"
        ],
        [
            "watch it: `fluxctl kube apps`",
            "wait longer with `--wait 600` next time, or start without waiting with `--detach`",
            "give the cluster more room: `fluxctl kube up --memory-mib 8192`"
        ]
    ),
    hc!(
        "APP_NO_CLUSTER",
        "There is no Kubernetes cluster to run apps on.",
        [
            "no cluster has been created on this machine",
            "the cluster's kubeconfig is missing or kubectl is not installed"
        ],
        [
            "create one: `fluxctl kube up` (or `fluxctl up` for everything in one step)",
            "see what exists: `fluxctl cluster status`",
            "install kubectl if it is missing (macOS: `brew install kubectl`)"
        ]
    ),
    hc!(
        "APP_CLUSTER_AMBIGUOUS",
        "There is more than one cluster, so fluxctl cannot guess which one you mean.",
        ["several clusters made with `fluxctl kube up` exist on this machine"],
        [
            "name one: add `--cluster NAME` to the command",
            "list them: `ls ~/.kube/fluxvm-*.yaml`"
        ]
    ),
    hc!(
        "APP_PORT_TAKEN",
        "The port on this machine is already in use by another program.",
        ["another app, a forward or a local server already listens on that port"],
        [
            "leave out --local-port and fluxctl picks a free one",
            "choose another: `--local-port 8081`",
            "see what holds it: `lsof -nP -iTCP:PORT -sTCP:LISTEN`"
        ]
    ),
    hc!(
        "APP_NAME_TAKEN",
        "Something in the cluster already uses that name and was not made by `fluxctl kube run`, so fluxctl will not touch it.",
        [
            "a Deployment or Service with the same name exists in this namespace",
            "it was created with kubectl or Helm"
        ],
        [
            "pick another name: `fluxctl kube run IMAGE --name other`",
            "or use another namespace: `-n NAMESPACE`"
        ]
    ),
    hc!(
        "APP_NOT_FOUND",
        "There is no app with that name in this cluster and namespace.",
        [
            "the name is misspelled, or the app is in another namespace",
            "it was already removed"
        ],
        [
            "list the apps: `fluxctl kube apps`",
            "add `-n NAMESPACE` if it is not in `default`",
            "start one: `fluxctl kube run IMAGE`"
        ]
    ),
    hc!(
        "KUBE_VALUE_MISSING",
        "No value was given for a Secret or ConfigMap key, or it was offered in a way that would leak it.",
        [
            "stdin was empty and no `--from-file` was given",
            "a secret value was written on the command line, where shell history and `ps` would keep it"
        ],
        [
            "type it at the hidden prompt: `fluxctl kube secrets set NAME KEY`",
            "pipe it in: `printf %s \"$VALUE\" | fluxctl kube secrets set NAME KEY`",
            "or read it from a file: `fluxctl kube secrets set NAME KEY --from-file ./value`"
        ]
    ),
    hc!(
        "KUBE_NEEDS_YES",
        "This removes or changes something that cannot be undone from here, so it only runs when you add --yes.",
        ["the command was run without `--yes`, so it showed what it would do instead"],
        ["read the plan it printed, then repeat the same command with `--yes`"]
    ),
    hc!(
        "KUBE_METRICS_UNAVAILABLE",
        "The cluster has no CPU and memory numbers: its metrics-server is missing or has not collected anything yet.",
        [
            "the cluster started less than a minute ago (the first numbers take about 60 seconds)",
            "k3s was started with `--disable metrics-server`, or this is not a k3s cluster",
            "the metrics-server pod is crashing"
        ],
        [
            "wait a minute and run `fluxctl kube top` again",
            "see the pod: `kubectl -n kube-system get pods -l k8s-app=metrics-server`",
            "install it where k3s did not: `kubectl apply -f https://github.com/kubernetes-sigs/metrics-server/releases/latest/download/components.yaml`"
        ]
    ),
    hc!(
        "KUBE_CERT_PROBLEM",
        "A certificate an Ingress or Gateway serves is expired, about to end, missing or unreadable.",
        [
            "nothing renews it (no cert-manager, or it cannot reach the issuer)",
            "the Secret the Ingress or Gateway names does not exist in that namespace",
            "the Secret does not hold a valid certificate in `tls.crt`"
        ],
        [
            "see which and the one-line fix for each: `fluxctl kube certs`",
            "renew with cert-manager: `cmctl renew -n NAMESPACE CERTIFICATE`",
            "or replace the Secret with a new certificate and key: `kubectl -n NAMESPACE create secret tls NAME --cert=CERT.pem --key=KEY.pem --dry-run=client -o yaml | kubectl apply -f -`"
        ]
    ),
    hc!(
        "KUBE_UPGRADE_REFUSED",
        "The k3s upgrade was not started because it would be unsafe or is not possible from here.",
        [
            "the target is older than what runs now (k3s does not support downgrades)",
            "the target skips a Kubernetes minor version",
            "a node is not Ready, or the cluster is owned by another Mac",
            "the pre-upgrade backup could not be taken"
        ],
        [
            "see the plan and the reason: `fluxctl kube upgrade`",
            "go one minor version at a time, for example `fluxctl kube upgrade --channel v1.31`",
            "fix an unhealthy node first: `fluxctl cluster status`"
        ]
    ),
    hc!(
        "KUBE_NODE_UNSUPPORTED",
        "Nodes cannot be added or removed this way on this cluster.",
        [
            "the cluster has no VM backend on this machine (an existing cluster reached with --kube-context)",
            "a server node cannot be removed from here",
            "removing a node's VM is not offered yet: only adding is automatic"
        ],
        [
            "add a node: `fluxctl kube node add`",
            "drain it and remove it from the cluster: `kubectl drain NODE --ignore-daemonsets --delete-emptydir-data` then `kubectl delete node NODE`",
            "see the VMs: `fluxctl cluster status`"
        ]
    ),
    hc!(
        "KUBE_OP_BUSY",
        "The cluster is already busy with an upgrade or a node change, so a second one was not started.",
        [
            "an upgrade or node add started from Studio or a terminal is still running",
            "a previous one was interrupted and its k3s processes are still working"
        ],
        [
            "wait for it to finish: Studio shows its progress, or `fluxctl cluster status`",
            "then repeat the action"
        ]
    ),
    hc!(
        "DOCKER_SOCKET_DOWN",
        "The Docker-compatible socket does not answer.",
        [
            "the daemon is not running, so the socket was never created",
            "a stale socket file is left from a crashed daemon",
            "the socket is switched off in the config"
        ],
        [
            "start the daemon: `fluxctl fix` (macOS) or `sudo systemctl start fluxvm`",
            "remove the stale file and restart the daemon",
            "run `fluxctl self-test` to check again"
        ]
    ),
    hc!(
        "SCHEDULER_NOT_ANSWERING",
        "The daemon is running but its VM list (the scheduler API) failed.",
        [
            "the daemon is still starting or is stuck",
            "the request was refused because no or a wrong --server-token was given"
        ],
        [
            "wait a few seconds and run `fluxctl self-test` again",
            "restart the daemon",
            "collect the log with `fluxctl support-bundle`"
        ]
    ),
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
        (
            "NO_KVM",
            "{m} cannot start virtual machines: hardware virtualization is not usable",
        )
    } else if has("cgroup") {
        (
            "CGROUPS",
            "{m} is missing kernel features FluxVM needs (control groups)",
        )
    } else if has("open files") {
        (
            "OPEN_FILES",
            "{m} allows too few open files for a busy host",
        )
    } else if has("net devices") {
        (
            "NET_DEVICES",
            "{m} is missing network drivers (tun, vhost_net)",
        )
    } else if has("ip forwarding") {
        (
            "IP_FORWARD",
            "{m} does not forward network traffic, so VMs cannot reach the network",
        )
    } else if has("binaries") {
        ("MISSING_BINARIES", "{m} is missing a program FluxVM needs")
    } else if has("entitlement")
        || has("signature")
        || has("vz runner")
        || has("virtualization.framework")
    {
        (
            "RUNNER_PROBLEM",
            "The Mac virtualization helper on {m} is missing or not signed correctly",
        )
    } else if has("disk") && !has("sleep") && !has("clone") {
        ("LOW_DISK", "{m} is short on disk space")
    } else if has("memory") {
        ("LOW_MEMORY", "{m} is short on memory")
    } else if has("session") || has("console") || has("screen locked") {
        (
            "NO_LOGIN_SESSION",
            "Nobody is logged in at {m}, so saved VMs cannot be restored",
        )
    } else if has("filevault") || has("automatic login") || has("autologin") || has("screen lock") {
        (
            "SECURITY_SETTING",
            "A security setting on {m} gets in the way of unattended use",
        )
    } else if has("agent-binary") {
        (
            "MISSING_BINARIES",
            "The FluxVM node agent is missing on {m}",
        )
    } else if has("launch agent") || has("agent") && !has("keepawake") {
        (
            "AGENT_NOT_LOADED",
            "A FluxVM background service on {m} is not running",
        )
    } else if has("sleep")
        || has("power")
        || has("keepawake")
        || has("autorestart")
        || has("restart after")
        || has("thermal")
    {
        (
            "POWER_SETTINGS",
            "{m} may go to sleep or lose power state, which pauses its VMs",
        )
    } else if has("store") || has("clone volume") {
        (
            "STORE_PROBLEM",
            "The place {m} keeps Mac VMs needs attention",
        )
    } else if has("ssh key") {
        ("NO_SSH_KEY", "{m} has no ssh key")
    } else if has("stale") || has("client entries") {
        (
            "STALE_PATHS",
            "Some tools on {m} point at an old FluxVM location",
        )
    } else if has("k3s") || has("kube") {
        ("K3S_NOT_READY", "Kubernetes on {m} is not ready")
    } else if has("platform") || has("apple silicon") || has("macos version") || has("rosetta") {
        (
            "UNSUPPORTED_HOST",
            "{m} cannot run everything FluxVM offers",
        )
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
        message: if matches!(
            rule.code,
            "SECURITY_SETTING" | "POWER_SETTINGS" | "CHECK_FAILED"
        ) {
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
    if g >= 10.0 {
        format!("{g:.0} GiB")
    } else {
        format!("{g:.1} GiB")
    }
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
    c.summary = if parts.is_empty() {
        "capacity unknown".to_owned()
    } else {
        parts.join(", ")
    };
    c
}

fn assess_node(f: &NodeFacts, now: u64) -> MachineHealth {
    let m = f.name.as_str();
    let mut items: Vec<Item> = vec![];
    let mut passed = 0u32;

    if !f.reachable {
        let why = f
            .unreachable_reason
            .clone()
            .unwrap_or_else(|| "no answer".into());
        items.push(item(
            m,
            "fleet",
            Severity::Fail,
            "HOST_UNREACHABLE",
            format!("{m} cannot be reached"),
            Some(why),
            Some(Fix::steps(
                "check that the machine is on and on the network, then try `ssh` to it by hand",
            )),
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
    let has_disk = f
        .checks
        .iter()
        .any(|c| c.name.to_ascii_lowercase().contains("free disk") || c.name == "disk");
    let has_daemon = f
        .checks
        .iter()
        .any(|c| c.name.to_ascii_lowercase().contains("daemon"));
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
    if let Some(used) = f.macos_in_use
        && f.platform == Platform::Mac
    {
        if used >= MACOS_SLOT_LIMIT {
            items.push(item(
                m,
                "capacity",
                Severity::Warn,
                "MACOS_SLOTS_FULL",
                format!("{used} of {MACOS_SLOT_LIMIT} macOS slots are in use on {m}"),
                Some("Apple's licence allows two macOS VMs per Mac at a time".into()),
                Some(Fix::steps(
                    "stop one macOS VM, or run the job on another Mac",
                )),
            ));
        } else {
            passed += 1;
        }
    }
    if !has_disk {
        match f.disk_free_gib {
            Some(d)
                if d < CRITICAL_DISK_GIB || (d < LOW_DISK_GIB && f.platform != Platform::Mac) =>
            {
                items.push(item(
                    m,
                    "capacity",
                    if d < CRITICAL_DISK_GIB {
                        Severity::Fail
                    } else {
                        Severity::Warn
                    },
                    "LOW_DISK",
                    format!("{m} has only {d} GiB of disk left"),
                    Some("new VMs, images and backups need room".into()),
                    Some(Fix::steps(
                        "free space by deleting VMs, images or backups you no longer need",
                    )),
                ));
            }
            Some(d) if f.platform == Platform::Mac && d < MACOS_DISK_GIB => items.push(item(
                m,
                "capacity",
                Severity::Warn,
                "LOW_DISK",
                format!(
                    "{m} has {d} GiB of disk left; a new macOS VM needs about {MACOS_DISK_GIB} GiB"
                ),
                Some("a macOS guest image and its disk are large".into()),
                Some(Fix::steps(
                    "free space, or move the store to a bigger disk with FLUXVM_MAC_STORE",
                )),
            )),
            Some(_) => passed += 1,
            None => {}
        }
    }
    if let (Some(free), Some(total)) = (f.mem_free_mib, f.mem_total_mib) {
        if total > 0
            && free * 10 < total
            && !f.checks.iter().any(|c| {
                c.name.to_ascii_lowercase().contains("memory") && c.severity != Severity::Ok
            })
        {
            items.push(item(
                m,
                "capacity",
                Severity::Warn,
                "LOW_MEMORY",
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
                m,
                "backup",
                Severity::Warn,
                "NO_BACKUP",
                format!("{m} has VMs but no backup yet"),
                Some("nothing has been backed up".into()),
                Some(Fix::command("fluxctl backup VM")),
            )),
            Some(t) if now.saturating_sub(t) > STALE_BACKUP_SECS => items.push(item(
                m,
                "backup",
                Severity::Warn,
                "STALE_BACKUP",
                format!(
                    "the newest backup on {m} is {} old",
                    human_age(now.saturating_sub(t))
                ),
                Some("backups are not being taken regularly".into()),
                Some(Fix::command("fluxctl backup VM")),
            )),
            Some(_) => passed += 1,
        }
    }
    if let Some(k) = f.k3s.as_deref() {
        let bad = !matches!(
            k,
            "" | "-" | "ready" | "ok" | "up" | "none" | "off" | "running"
        );
        if bad {
            items.push(item(
                m,
                "fleet",
                Severity::Warn,
                "K3S_NOT_READY",
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
            m,
            "fleet",
            Severity::Warn,
            "MISSING_IMAGE",
            format!("the image {img} is not on {m} yet"),
            Some("it was never pulled there".into()),
            Some(
                Fix::command(format!("fluxctl mac pull {img}")).auto(FixAction::PullImage {
                    reference: img.clone(),
                }),
            ),
        ));
    }
    let cap = capacity_of(f);
    finish(f, dedup(items), passed, now, cap)
}

/// Two doctors often report the same thing (the login session, the daemon): keep the worse one. Codes that
/// legitimately repeat with different subjects are left alone.
fn dedup(items: Vec<Item>) -> Vec<Item> {
    const REPEATS: [&str; 4] = [
        "SECURITY_SETTING",
        "MISSING_IMAGE",
        "CHECK_FAILED",
        "POWER_SETTINGS",
    ];
    let mut out: Vec<Item> = vec![];
    for i in items {
        let code = i.explain.clone().unwrap_or_default();
        if REPEATS.contains(&code.as_str()) {
            out.push(i);
        } else if let Some(prev) = out
            .iter_mut()
            .find(|o| o.explain.as_deref() == Some(code.as_str()))
        {
            if i.severity > prev.severity {
                *prev = i;
            }
        } else {
            out.push(i);
        }
    }
    out
}

fn finish(
    f: &NodeFacts,
    mut items: Vec<Item>,
    passed: u32,
    now: u64,
    capacity: Capacity,
) -> MachineHealth {
    let m = f.name.as_str();
    // worst first, stable
    items.sort_by_key(|i| std::cmp::Reverse(i.severity));
    for (n, i) in items.iter_mut().enumerate() {
        let c = i.explain.as_deref().unwrap_or("item").to_ascii_lowercase();
        i.id = format!("{m}/{c}/{}", n + 1);
    }
    let light = items
        .iter()
        .map(|i| i.severity)
        .max()
        .unwrap_or(Severity::Ok);
    let headline = match (light, items.first()) {
        (Severity::Ok, _) => format!("{m} is healthy"),
        (_, Some(i)) => format!(
            "{m} needs attention: {}",
            i.message
                .strip_suffix(&format!(" on {m}"))
                .unwrap_or(&i.message)
        ),
        _ => format!("{m} needs attention"),
    };
    let update = match (&f.version, &f.wanted_version) {
        (Some(c), Some(w)) if c != w && newer(w, c) => Some(Update {
            current: c.clone(),
            wanted: w.clone(),
        }),
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
        s.trim_start_matches('v')
            .split(|c: char| !c.is_ascii_digit())
            .filter(|x| !x.is_empty())
            .map(|x| x.parse().unwrap_or(0))
            .collect()
    };
    p(a) > p(b)
}

pub fn assess(nodes: &[NodeFacts], now: u64) -> ClusterHealth {
    assess_with(nodes, vec![], now)
}

/// [`assess`] plus problems that belong to the cluster rather than one machine (for example [`gateway_items`]).
pub fn assess_with(nodes: &[NodeFacts], extra: Vec<Item>, now: u64) -> ClusterHealth {
    let machines: Vec<MachineHealth> = nodes.iter().map(|n| assess_node(n, now)).collect();
    let mut items = extra;
    if machines.is_empty() {
        items.push(item(
            "cluster", "fleet", Severity::Fail, "NO_MACHINES",
            "FluxVM does not know about any machine yet".into(),
            Some("no daemon answered here and no inventory was given".into()),
            Some(Fix::steps("start FluxVM on this machine, or pass --inventory FILE; see docs/mac-fleet-quickstart.md")),
        ));
    }
    for (n, i) in items.iter_mut().enumerate() {
        i.id = format!(
            "cluster/{}/{}",
            i.explain.as_deref().unwrap_or("item").to_ascii_lowercase(),
            n + 1
        );
    }
    let light = machines
        .iter()
        .map(|m| m.light)
        .chain(items.iter().map(|i| i.severity))
        .max()
        .unwrap_or(Severity::Ok);
    let bad = machines.iter().filter(|m| m.light != Severity::Ok).count();
    let headline = if machines.is_empty() {
        "No machines found".to_owned()
    } else if bad == 0 && !items.is_empty() {
        let base = match machines.len() {
            1 => format!("{} is healthy", machines[0].name),
            n => format!("All {n} machines are healthy"),
        };
        format!(
            "{base}, but {} in the cluster",
            plural(
                items.len() as u64,
                "problem needs attention",
                "problems need attention"
            )
        )
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
    let up: Vec<&MachineHealth> = machines
        .iter()
        .filter(|m| m.capacity.room_for_vms.is_some() || m.capacity.macos_in_use.is_some())
        .collect();
    let room: Option<u32> = up
        .iter()
        .filter_map(|m| m.capacity.room_for_vms)
        .fold(None, |a, r| Some(a.unwrap_or(0) + r));
    let mac_used: u32 = up.iter().filter_map(|m| m.capacity.macos_in_use).sum();
    let mac_max: u32 = up.iter().filter_map(|m| m.capacity.macos_limit).sum();
    let mut capacity = Capacity {
        room_for_vms: room,
        macos_in_use: (mac_max > 0).then_some(mac_used),
        macos_limit: (mac_max > 0).then_some(mac_max),
        disk_free_gib: up
            .iter()
            .filter_map(|m| m.capacity.disk_free_gib)
            .fold(None, |a, d| Some(a.unwrap_or(0) + d)),
        mem_free_mib: up
            .iter()
            .filter_map(|m| m.capacity.mem_free_mib)
            .fold(None, |a, d| Some(a.unwrap_or(0) + d)),
        mem_total_mib: up
            .iter()
            .filter_map(|m| m.capacity.mem_total_mib)
            .fold(None, |a, d| Some(a.unwrap_or(0) + d)),
        summary: String::new(),
    };
    capacity.summary = {
        let mut p = vec![];
        if let Some(r) = capacity.room_for_vms {
            p.push(if r == 0 {
                "no room for another VM".to_owned()
            } else {
                format!("room for {} more", plural(r as u64, "VM", "VMs"))
            });
        }
        if let (Some(u), Some(l)) = (capacity.macos_in_use, capacity.macos_limit) {
            p.push(format!("{u} of {l} macOS slots in use"));
        }
        if let Some(d) = capacity.disk_free_gib {
            p.push(format!("{d} GiB disk free"));
        }
        if p.is_empty() {
            "capacity unknown".into()
        } else {
            p.join(", ")
        }
    };
    let mut h = ClusterHealth {
        schema: SCHEMA,
        generated_at: now,
        light,
        headline,
        machines,
        items,
        capacity,
        next_action: NextAction {
            item: None,
            machine: None,
            text: String::new(),
            command: None,
            safe: false,
        },
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
                Some(r) if r > 0 => format!(
                    "Nothing to do. There is room for {} more.",
                    plural(r as u64, "VM", "VMs")
                ),
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
                format!(
                    "run `{c}`{}",
                    if i.machine == "cluster" {
                        String::new()
                    } else {
                        format!(" on {}", i.machine)
                    }
                )
            } else if let Some(s) = fix.and_then(|f| f.steps.clone()) {
                s
            } else {
                format!(
                    "run `fluxctl explain {}`",
                    i.explain.as_deref().unwrap_or("CHECK_FAILED")
                )
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

// ---- Gateway API ------------------------------------------------------------------------------------------------------

/// One Gateway API status condition as the API server reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GwCondition {
    /// `True`, `False` or `Unknown`.
    pub status: String,
    /// The generation of the object the controller last looked at; `None` when it did not say.
    pub observed_generation: Option<i64>,
    pub message: Option<String>,
}

impl GwCondition {
    /// The controller has seen the current version of the object. A condition with an older (or no) observed generation
    /// describes a previous spec: the controller has not caught up yet, which is not a failure.
    fn current(&self, generation: Option<i64>) -> bool {
        match (self.observed_generation, generation) {
            (Some(o), Some(g)) => o == g,
            _ => false,
        }
    }
    /// A current condition that says `False`.
    fn failed(&self, generation: Option<i64>) -> bool {
        self.current(generation) && self.status.eq_ignore_ascii_case("false")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GatewayFact {
    pub namespace: String,
    pub name: String,
    /// Its class belongs to the Envoy Gateway controller.
    #[serde(default)]
    pub envoy: bool,
    pub generation: Option<i64>,
    /// `None` when the controller has not written the condition yet.
    pub programmed: Option<GwCondition>,
}

/// One Gateway a route is attached to, with the route's conditions for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteParentFact {
    pub gateway: String,
    pub accepted: Option<GwCondition>,
    pub resolved_refs: Option<GwCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BackendFact {
    pub namespace: String,
    pub service: String,
    /// Ready endpoints; `None` when they could not be read (the Service is missing, or the read failed).
    pub ready_endpoints: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RouteFact {
    pub namespace: String,
    pub name: String,
    /// It is attached to a Gateway of the Envoy class.
    #[serde(default)]
    pub envoy: bool,
    pub generation: Option<i64>,
    #[serde(default)]
    pub parents: Vec<RouteParentFact>,
    #[serde(default)]
    pub backends: Vec<BackendFact>,
}

/// What is known about the Gateway API objects of one cluster. Read-only evidence; nothing is guessed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GatewayFacts {
    /// The FluxVM cluster name (for the `--cluster` of the fix commands); empty for the kubectl default.
    pub cluster: String,
    /// The Envoy Gateway controller is running; `None` when that could not be read.
    pub controller_present: Option<bool>,
    #[serde(default)]
    pub gateways: Vec<GatewayFact>,
    #[serde(default)]
    pub routes: Vec<RouteFact>,
}

/// The plain-English problems with a cluster's Gateways, routes and backends. A cluster with no Gateways, and a healthy
/// one, give nothing: Envoy Gateway being absent is only a problem when something needs it.
pub fn gateway_items(f: &GatewayFacts) -> Vec<Item> {
    let flag = if f.cluster.is_empty() {
        String::new()
    } else {
        format!(" --cluster {}", f.cluster)
    };
    let on = if f.cluster.is_empty() {
        String::new()
    } else {
        format!(" on {}", f.cluster)
    };
    let mut out = vec![];
    let mk = |sev: Severity, code: &str, msg: String, cause: Option<String>, fix: Fix| Item {
        id: String::new(),
        machine: "cluster".into(),
        source: "gateway".into(),
        severity: sev,
        message: msg,
        cause,
        fix: Some(fix),
        explain: Some(code.to_owned()),
    };
    let missing = f.controller_present == Some(false);
    let needs_envoy = f.gateways.iter().any(|g| g.envoy) || f.routes.iter().any(|r| r.envoy);
    if missing && needs_envoy {
        let n = f.gateways.iter().filter(|g| g.envoy).count();
        let r = f.routes.iter().filter(|r| r.envoy).count();
        let what = match (n, r) {
            (0, r) => plural(r as u64, "route", "routes"),
            (n, 0) => plural(n as u64, "Gateway", "Gateways"),
            (n, r) => format!(
                "{} and {}",
                plural(n as u64, "Gateway", "Gateways"),
                plural(r as u64, "route", "routes")
            ),
        };
        let name = if f.cluster.is_empty() {
            "NAME"
        } else {
            &f.cluster
        };
        out.push(mk(
            Severity::Fail,
            "ENVOY_GATEWAY_NOT_INSTALLED",
            format!(
                "{what} {} Envoy Gateway, which is not running{on}",
                if n + r == 1 { "needs" } else { "need" }
            ),
            Some(
                "the envoy-gateway controller is missing from the envoy-gateway-system namespace"
                    .into(),
            ),
            Fix::command(format!(
                "fluxctl kube cluster platform envoy-gateway {name}"
            )),
        ));
    }
    // Without the controller every Envoy Gateway looks broken for the same reason; say it once.
    let skip = |envoy: bool| missing && envoy;
    for g in f.gateways.iter().filter(|g| !skip(g.envoy)) {
        if let Some(c) = &g.programmed
            && c.failed(g.generation)
        {
            out.push(mk(
                Severity::Fail,
                "GATEWAY_NOT_PROGRAMMED",
                format!(
                    "Gateway {}/{} is not set up, so it serves no traffic",
                    g.namespace, g.name
                ),
                c.message.clone(),
                Fix::command(format!(
                    "kubectl describe gateway {} -n {}",
                    g.name, g.namespace
                )),
            ));
        }
    }
    let mut seen_backends = std::collections::BTreeSet::new();
    for r in f.routes.iter().filter(|r| !skip(r.envoy)) {
        let inspect = format!(
            "fluxctl gateway inspect {} -n {}{flag}",
            r.name, r.namespace
        );
        let id = format!("{}/{}", r.namespace, r.name);
        let mut accepted_ok = true;
        for p in &r.parents {
            if let Some(c) = &p.accepted
                && c.failed(r.generation)
            {
                accepted_ok = false;
                out.push(mk(
                    Severity::Fail,
                    "ROUTE_NOT_ACCEPTED",
                    format!(
                        "Route {id} was not accepted by Gateway {}, so it does nothing",
                        p.gateway
                    ),
                    c.message.clone(),
                    Fix::command(inspect.clone()),
                ));
            }
        }
        for p in &r.parents {
            if let Some(c) = &p.resolved_refs
                && c.failed(r.generation)
            {
                out.push(mk(
                    Severity::Fail,
                    "ROUTE_BACKEND_UNRESOLVED",
                    format!(
                        "Route {id} points at a Service that cannot be used, so requests to it fail"
                    ),
                    c.message.clone(),
                    Fix::command(inspect.clone()),
                ));
                break;
            }
        }
        if !accepted_ok {
            continue;
        }
        for b in &r.backends {
            if b.ready_endpoints == Some(0)
                && seen_backends.insert((b.namespace.clone(), b.service.clone()))
            {
                out.push(mk(
                    Severity::Warn,
                    "BACKEND_NO_READY_ENDPOINTS",
                    format!("Service {}/{} (used by route {id}) has no ready pods, so the Gateway has nowhere to send requests", b.namespace, b.service),
                    Some("its pods are starting, failing their readiness check, or the Service selects none".into()),
                    Fix::command(inspect.clone()),
                ));
            }
        }
    }
    out
}

// ---- facts from the daemon's JSON ---------------------------------------------------------------------------------

/// Fills the VM, CPU, memory and macOS-slot numbers of `f` from `GET /v1/vms` (`{"items": [...]}`) and, when given, the
/// host memory (`GET /v1/host/memory`). `host_cpus` is the machine's CPU count. Used by the CLI (over HTTP) and by the
/// daemon (in process) so both read VMs the same way.
pub fn apply_vm_list(f: &mut NodeFacts, vms: &serde_json::Value, host_cpus: Option<u32>) {
    let Some(items) = vms.get("items").and_then(|v| v.as_array()) else {
        return;
    };
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
    let full = if ok {
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    let short = full.split('.').next().unwrap_or("").to_owned();
    if short.is_empty() {
        "this-machine".into()
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_800_000_000;

    fn raw(name: &str, sev: Severity, detail: &str, fix: Option<&str>) -> RawCheck {
        RawCheck {
            source: "doctor".into(),
            name: name.into(),
            severity: sev,
            detail: detail.into(),
            fix: fix.map(str::to_owned),
            path: None,
        }
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
        assert!(
            h.next_action.text.starts_with("Nothing to do"),
            "{}",
            h.next_action.text
        );
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
        f.checks
            .push(raw("macOS VM slots", Severity::Warn, "2/2 used", None));
        let h = assess(&[f], NOW);
        let m = &h.machines[0];
        assert_eq!(m.light, Severity::Warn);
        // the doctor's own slot row is replaced by the model's, not shown twice
        assert_eq!(m.items.len(), 1);
        assert_eq!(
            m.items[0].message,
            "2 of 2 macOS slots are in use on mini-01"
        );
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
        assert!(
            h.next_action.text.contains("fluxctl fix"),
            "{}",
            h.next_action.text
        );
    }

    #[test]
    fn a_stopped_daemon_on_linux_needs_sudo_and_is_never_automatic() {
        let mut f = NodeFacts {
            name: "box".into(),
            platform: Platform::Linux,
            reachable: true,
            daemon_up: Some(false),
            ..Default::default()
        };
        f.checks.push(raw(
            "daemon",
            Severity::Fail,
            "connection refused",
            Some("run `fluxctl serve`"),
        ));
        let h = assess(&[f], NOW);
        let fix = h.machines[0].items[0].fix.as_ref().unwrap();
        assert!(!fix.safe && fix.needs_sudo && fix.action.is_none());
        assert_eq!(fix.command.as_deref(), Some("sudo systemctl start fluxvm"));
    }

    #[test]
    fn security_settings_are_never_automatic() {
        let mut f = mac("m");
        f.checks.push(raw(
            "FileVault (host)",
            Severity::Warn,
            "on",
            Some("System Settings > Privacy & Security"),
        ));
        f.checks
            .push(raw("automatic login", Severity::Fail, "off", None));
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
        f.checks
            .push(raw("login session", Severity::Fail, "login window", None));
        let h = assess(&[f], NOW);
        let safe: Vec<_> = h.machines[0]
            .items
            .iter()
            .filter(|i| i.fix.as_ref().unwrap().safe)
            .collect();
        assert_eq!(safe.len(), 1);
        assert_eq!(
            safe[0].fix.as_ref().unwrap().action,
            Some(FixAction::CreateDir {
                path: "/Volumes/Fast/fluxvm".into()
            })
        );
        let login = h.machines[0]
            .items
            .iter()
            .find(|i| i.explain.as_deref() == Some("NO_LOGIN_SESSION"))
            .unwrap();
        assert!(login.fix.as_ref().unwrap().needs_login);
    }

    #[test]
    fn a_missing_image_is_a_safe_pull() {
        let mut f = mac("m");
        f.missing_images = vec!["ghcr.io/acme/mac-ci:1".into()];
        let h = assess(&[f], NOW);
        let fx = h.machines[0].items[0].fix.as_ref().unwrap();
        assert_eq!(
            fx.action,
            Some(FixAction::PullImage {
                reference: "ghcr.io/acme/mac-ci:1".into()
            })
        );
    }

    #[test]
    fn unreachable_and_not_installed_stop_further_checks() {
        let down = NodeFacts {
            name: "mini-03".into(),
            platform: Platform::Mac,
            reachable: false,
            unreachable_reason: Some("Connection timed out".into()),
            ..Default::default()
        };
        let fresh = NodeFacts {
            name: "mini-04".into(),
            platform: Platform::Mac,
            reachable: true,
            installed: Some(false),
            ..Default::default()
        };
        let h = assess(&[down, fresh], NOW);
        assert_eq!(h.light, Severity::Fail);
        assert_eq!(
            h.machines[0].items[0].explain.as_deref(),
            Some("HOST_UNREACHABLE")
        );
        assert_eq!(
            h.machines[0].items[0].cause.as_deref(),
            Some("Connection timed out")
        );
        assert_eq!(
            h.machines[1].items[0]
                .fix
                .as_ref()
                .unwrap()
                .command
                .as_deref(),
            Some("fluxctl mac-fleet up mini-04")
        );
        assert_eq!(h.headline, "2 of 2 machines need attention");
    }

    #[test]
    fn backups_are_reported_in_age_and_only_when_there_are_vms() {
        let mut f = mac("m");
        f.last_backup_epoch = Some(NOW - 9 * 86400);
        let h = assess(&[f.clone()], NOW);
        assert_eq!(
            h.machines[0].items[0].explain.as_deref(),
            Some("STALE_BACKUP")
        );
        assert_eq!(h.machines[0].last_backup, "9 days ago");
        f.last_backup_epoch = None;
        assert_eq!(
            assess(&[f.clone()], NOW).machines[0].items[0]
                .explain
                .as_deref(),
            Some("NO_BACKUP")
        );
        assert_eq!(assess(&[f.clone()], NOW).machines[0].last_backup, "never");
        f.vms_total = Some(0);
        assert!(assess(&[f], NOW).machines[0].items.is_empty());
    }

    #[test]
    fn low_disk_is_graded_and_deduplicated_against_the_doctor() {
        let mut f = mac("m");
        f.disk_free_gib = Some(30);
        let h = assess(&[f.clone()], NOW);
        assert!(
            h.machines[0].items[0]
                .message
                .contains("a new macOS VM needs about 60 GiB")
        );
        f.disk_free_gib = Some(3);
        assert_eq!(
            assess(&[f.clone()], NOW).machines[0].items[0].severity,
            Severity::Fail
        );
        f.checks
            .push(raw("free disk", Severity::Warn, "30 GiB", None));
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
        assert_eq!(
            h.machines[0].update,
            Some(Update {
                current: "0.4.20".into(),
                wanted: "0.4.21".into()
            })
        );
        f.wanted_version = Some("0.4.9".into());
        assert_eq!(
            assess(&[f], NOW).machines[0].update,
            None,
            "0.4.9 is older than 0.4.20"
        );
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
            "daemon",
            "installed",
            "kvm",
            "cgroups",
            "open files",
            "net devices",
            "ip forwarding",
            "binaries",
            "vmnet entitlement",
            "runner signature",
            "vz runner",
            "Virtualization.framework",
            "free disk",
            "memory",
            "login session",
            "console session",
            "FileVault (host)",
            "automatic login",
            "screen lock",
            "launch agents",
            "keepawake agent",
            "sleep held off by",
            "idle sleep",
            "power source",
            "store location",
            "clone volume",
            "ssh key",
            "client entries",
            "stale paths",
            "k3s",
            "platform",
            "apple silicon",
            "macOS version",
            "Rosetta",
            "something nobody has heard of",
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
                        assert!(
                            fx.command.is_some() || fx.steps.is_some(),
                            "{n}: a fix says nothing"
                        );
                    }
                }
            }
        }
        // every code the model can emit is explained, and every explained code can be emitted by some path
        for extra in [
            "HOST_UNREACHABLE",
            "MACOS_SLOTS_FULL",
            "NO_BACKUP",
            "STALE_BACKUP",
            "MISSING_IMAGE",
            "NO_MACHINES",
            "LOW_MEMORY",
            "ENVOY_GATEWAY_NOT_INSTALLED",
            "GATEWAY_NOT_PROGRAMMED",
            "ROUTE_NOT_ACCEPTED",
            "ROUTE_BACKEND_UNRESOLVED",
            "BACKEND_NO_READY_ENDPOINTS",
            "SUPPORT_BUNDLE_SECRET_LEFT",
            "SUPPORT_BUNDLE_WRITE_FAILED",
            "BACKUP_DISK_LOW",
            "BACKUP_CHECKSUM_MISMATCH",
            "BACKUP_NOT_RESTORABLE",
            "BACKUP_NOT_FOUND",
            "RESTORE_NEEDS_CONFIRM",
            "APP_IMAGE_PULL",
            "APP_CRASHING",
            "APP_OUT_OF_MEMORY",
            "APP_NOT_READY",
            "APP_NO_CLUSTER",
            "APP_CLUSTER_AMBIGUOUS",
            "APP_PORT_TAKEN",
            "APP_NAME_TAKEN",
            "APP_NOT_FOUND",
            "KUBE_VALUE_MISSING",
            "KUBE_NEEDS_YES",
            "KUBE_METRICS_UNAVAILABLE",
            "KUBE_CERT_PROBLEM",
            "KUBE_UPGRADE_REFUSED",
            "KUBE_NODE_UNSUPPORTED",
            "KUBE_OP_BUSY",
            "DOCKER_SOCKET_DOWN",
            "SCHEDULER_NOT_ANSWERING",
            "K8S_EVENTS_UNREACHABLE",
        ] {
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
        f.checks
            .push(raw("session", Severity::Warn, "locked", None));
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
        assert_eq!(
            v["machines"][0]["items"][0]["fix"]["action"]["kind"],
            "start_service"
        );
        let back: ClusterHealth = serde_json::from_value(v).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn vm_list_fills_counts_cpu_and_macos_slots() {
        let mut f = NodeFacts {
            name: "m".into(),
            platform: Platform::Mac,
            ..Default::default()
        };
        let vms = json!({"items": [
            {"status": "running", "request": {"vcpus": 4, "apple": {"guest_os": "macos"}}},
            {"status": "paused", "request": {"vcpus": 2, "apple": {"guest_os": "macos"}}},
            {"status": "stopped", "request": {"vcpus": 8, "apple": {"guest_os": "macos"}}},
            {"status": "running", "request": {"vcpus": 2}},
        ]});
        apply_vm_list(&mut f, &vms, Some(10));
        assert_eq!(
            (f.vms_total, f.vms_running, f.macos_in_use),
            (Some(4), Some(2), Some(2))
        );
        assert_eq!(f.cpus_free, Some(2));
        let mut l = NodeFacts {
            platform: Platform::Linux,
            ..Default::default()
        };
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
        assert!(
            free_gib(&d.path().join("not/yet/created")).is_some(),
            "falls back to the nearest parent"
        );
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

    // ---- Gateway API ----

    fn cond(status: &str, observed: Option<i64>) -> Option<GwCondition> {
        Some(GwCondition {
            status: status.into(),
            observed_generation: observed,
            message: Some("because".into()),
        })
    }

    fn gw(envoy: bool, programmed: Option<GwCondition>) -> GatewayFact {
        GatewayFact {
            namespace: "web".into(),
            name: "edge".into(),
            envoy,
            generation: Some(2),
            programmed,
        }
    }

    fn route(
        accepted: Option<GwCondition>,
        resolved: Option<GwCondition>,
        ready: Option<u64>,
    ) -> RouteFact {
        RouteFact {
            namespace: "web".into(),
            name: "shop".into(),
            envoy: true,
            generation: Some(2),
            parents: vec![RouteParentFact {
                gateway: "edge".into(),
                accepted,
                resolved_refs: resolved,
            }],
            backends: vec![BackendFact {
                namespace: "web".into(),
                service: "shop".into(),
                ready_endpoints: ready,
            }],
        }
    }

    fn healthy_gateway() -> GatewayFacts {
        GatewayFacts {
            cluster: "dev".into(),
            controller_present: Some(true),
            gateways: vec![gw(true, cond("True", Some(2)))],
            routes: vec![route(cond("True", Some(2)), cond("True", Some(2)), Some(2))],
        }
    }

    fn codes_of(f: &GatewayFacts) -> Vec<String> {
        gateway_items(f)
            .into_iter()
            .filter_map(|i| i.explain)
            .collect()
    }

    #[test]
    fn a_healthy_gateway_setup_and_an_empty_cluster_give_no_items() {
        assert!(gateway_items(&healthy_gateway()).is_empty());
        assert!(gateway_items(&GatewayFacts::default()).is_empty());
        // Envoy Gateway absent is not a problem when nothing uses it, or when it could not be read.
        let none_needed = GatewayFacts {
            controller_present: Some(false),
            ..Default::default()
        };
        assert!(gateway_items(&none_needed).is_empty());
        let other_class = GatewayFacts {
            controller_present: Some(false),
            gateways: vec![gw(false, cond("False", Some(2)))],
            ..Default::default()
        };
        assert_eq!(codes_of(&other_class), ["GATEWAY_NOT_PROGRAMMED"]);
        let unknown = GatewayFacts {
            controller_present: None,
            ..healthy_gateway()
        };
        assert!(gateway_items(&unknown).is_empty());
    }

    #[test]
    fn a_missing_controller_is_said_once_and_hides_its_symptoms() {
        let mut f = healthy_gateway();
        f.controller_present = Some(false);
        f.gateways[0].programmed = cond("False", Some(2));
        f.routes[0].parents[0].accepted = cond("False", Some(2));
        let items = gateway_items(&f);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].explain.as_deref(),
            Some("ENVOY_GATEWAY_NOT_INSTALLED")
        );
        assert_eq!(items[0].severity, Severity::Fail);
        assert_eq!(
            items[0].message,
            "1 Gateway and 1 route need Envoy Gateway, which is not running on dev"
        );
        assert_eq!(
            items[0].fix.as_ref().unwrap().command.as_deref(),
            Some("fluxctl kube cluster platform envoy-gateway dev")
        );
    }

    #[test]
    fn each_failing_condition_has_its_own_code_and_a_command() {
        let mut f = healthy_gateway();
        f.gateways[0].programmed = cond("False", Some(2));
        assert_eq!(codes_of(&f), ["GATEWAY_NOT_PROGRAMMED"]);
        let i = &gateway_items(&f)[0];
        assert_eq!(i.cause.as_deref(), Some("because"));
        assert_eq!(
            i.fix.as_ref().unwrap().command.as_deref(),
            Some("kubectl describe gateway edge -n web")
        );

        let mut f = healthy_gateway();
        f.routes[0].parents[0].accepted = cond("False", Some(2));
        assert_eq!(codes_of(&f), ["ROUTE_NOT_ACCEPTED"]);
        assert_eq!(
            gateway_items(&f)[0]
                .fix
                .as_ref()
                .unwrap()
                .command
                .as_deref(),
            Some("fluxctl gateway inspect shop -n web --cluster dev")
        );

        let mut f = healthy_gateway();
        f.routes[0].parents[0].resolved_refs = cond("False", Some(2));
        assert_eq!(codes_of(&f), ["ROUTE_BACKEND_UNRESOLVED"]);

        let mut f = healthy_gateway();
        f.routes[0].backends[0].ready_endpoints = Some(0);
        let items = gateway_items(&f);
        assert_eq!(codes_of(&f), ["BACKEND_NO_READY_ENDPOINTS"]);
        assert_eq!(items[0].severity, Severity::Warn);
    }

    #[test]
    fn a_stale_condition_is_not_yet_reconciled_not_failed() {
        let mut f = healthy_gateway();
        f.gateways[0].programmed = cond("False", Some(1));
        f.routes[0].parents[0].accepted = cond("False", Some(1));
        f.routes[0].parents[0].resolved_refs = cond("False", None);
        assert!(gateway_items(&f).is_empty());
        // Not written at all, or Unknown, is also "not yet".
        f.gateways[0].programmed = None;
        f.routes[0].parents[0].accepted = cond("Unknown", Some(2));
        assert!(gateway_items(&f).is_empty());
        // An unreadable endpoint count is not "zero".
        f.routes[0].backends[0].ready_endpoints = None;
        assert!(gateway_items(&f).is_empty());
    }

    #[test]
    fn a_rejected_route_does_not_also_complain_about_its_backend_and_backends_are_listed_once() {
        let mut f = healthy_gateway();
        f.routes[0].parents[0].accepted = cond("False", Some(2));
        f.routes[0].backends[0].ready_endpoints = Some(0);
        assert_eq!(codes_of(&f), ["ROUTE_NOT_ACCEPTED"]);
        let mut f = healthy_gateway();
        f.routes[0].backends[0].ready_endpoints = Some(0);
        let mut second = f.routes[0].clone();
        second.name = "shop2".into();
        f.routes.push(second);
        assert_eq!(codes_of(&f), ["BACKEND_NO_READY_ENDPOINTS"]);
    }

    #[test]
    fn gateway_items_are_valid_items_and_join_the_cluster_health() {
        let mut f = healthy_gateway();
        f.gateways[0].programmed = cond("False", Some(2));
        f.routes[0].backends[0].ready_endpoints = Some(0);
        let items = gateway_items(&f);
        for i in &items {
            assert!(super::code(i.explain.as_deref().unwrap()).is_some());
            assert!(i.fix.as_ref().unwrap().consistent());
            assert!(
                !i.fix.as_ref().unwrap().safe,
                "a gateway fix is never applied automatically"
            );
        }
        let h = assess_with(&[mac("m")], items, NOW);
        assert_eq!(h.light, Severity::Fail);
        assert!(
            h.headline
                .starts_with("m is healthy, but 2 problems need attention in the cluster"),
            "{}",
            h.headline
        );
        assert_eq!(h.items.len(), 2);
        assert!(h.items.iter().all(|i| i.id.starts_with("cluster/")));
        assert_eq!(h.next_action.machine.as_deref(), Some("cluster"));
        assert!(
            h.next_action
                .text
                .contains("kubectl describe gateway edge -n web")
        );
        // no gateway problems: identical to the plain assessment
        assert_eq!(
            assess_with(&[mac("m")], vec![], NOW),
            assess(&[mac("m")], NOW)
        );
    }
}
