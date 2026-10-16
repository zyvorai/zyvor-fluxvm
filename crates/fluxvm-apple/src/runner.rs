// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use fluxvm_core::{
    backend::LaunchContext,
    model::{AppleGuest, CreateVmRequest, NetworkSpec},
};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

/// JSON handed to `fluxvm-vz-runner --config`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RunnerConfig {
    pub id: String,
    pub workspace: PathBuf,
    pub cpus: u8,
    pub memory_mib: u64,
    pub guest_os: &'static str,
    pub disk: PathBuf,
    pub seed: Option<PathBuf>,
    pub media: Option<PathBuf>,
    pub mac: String,
    pub control_socket: PathBuf,
    pub serial_log: PathBuf,
    pub vsock_socket: Option<PathBuf>,
    /// The daemon's self-control socket; the runner relays guest connections on vsock `SELF_CONTROL_PORT` to it.
    pub self_socket: Option<PathBuf>,
    pub ip_file: PathBuf,
    pub window: bool,
    pub display_count: u8,
    pub clipboard: bool,
    pub bridge_interface: Option<String>,
    pub asif_overlay: bool,
    pub provision_full_name: Option<String>,
    pub provision_username: Option<String>,
    pub provision_password_file: Option<PathBuf>,
    pub provision_auto_login: bool,
    pub provision_remote_login: bool,
    pub display_width: u32,
    pub display_height: u32,
    pub display_ppi: u32,
    pub audio_output: bool,
    pub microphone: bool,
    pub rosetta: bool,
    pub nested_virtualization: bool,
    pub usb_controller: bool,
    /// XHCI controllers to create (1-4) when `usb_controller` is set.
    pub usb_controllers: u8,
    pub vmnet: Option<fluxvm_core::model::AppleVmnetSpec>,
    pub custom_virtio: bool,
    /// macOS guests only: attach a virtio-socket device (experimental; verified once on one M4 and macOS 27.0.1).
    pub macos_vsock: bool,
    /// virtiofs shares, tagged `fs0`, `fs1`, … in request order (the tags the guest-side mount uses).
    pub shares: Vec<ShareConfig>,
    /// Host `127.0.0.1:host_port` listeners that relay TCP to the guest's NAT address.
    pub forwards: Vec<ForwardConfig>,
    /// Attach no network device (`network.mode = "none"`).
    pub network_none: bool,
    /// Hosts the guest may reach through the vsock proxy on `EGRESS_PORT` (empty: no proxy).
    pub egress_allow: Vec<String>,
    /// `127.0.0.1:port` of the daemon's egress proxy: allowed requests are relayed there so the session's
    /// method/path rules and grants apply. Needs `egress_allow`.
    pub egress_via: Option<String>,
    /// The secret the daemon's proxy maps to this VM (`fluxvm_core::peer_alias`); never reaches the guest.
    pub egress_token: Option<String>,
    /// Extra guest→host vsock services (see `AppleSpec::vsock_services`), with socket names resolved to host paths.
    pub vsock_services: Vec<VsockServiceConfig>,
    /// The JSON the `metadata` built-in serves (id, name, sizing).
    pub vsock_metadata: Option<String>,
    /// Where the `telemetry` built-in appends guest lines.
    pub telemetry_log: Option<PathBuf>,
    /// A state file written by a snapshot; the runner resumes from it instead of cold-booting.
    pub restore_state: Option<PathBuf>,
    /// Direct boot (`VZLinuxBootLoader`): an uncompressed arm64 `Image`. None boots EFI from the disk.
    pub kernel: Option<PathBuf>,
    pub initrd: Option<PathBuf>,
    pub cmdline: Option<String>,
    pub root_read_only: bool,
    pub root_caching: fluxvm_core::model::AppleDiskCaching,
    pub root_sync: fluxvm_core::model::AppleDiskSync,
    pub extra_disks: Vec<fluxvm_core::model::AppleDisk>,
    /// Private networks: one more network card each, connected to the network's switch.
    pub networks: Vec<NetworkConfig>,
    /// `apple.usernet`: the inherited descriptor number of the guest NIC's datagram socket (see `crate::usernet`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usernet_fd: Option<i32>,
    /// `apple.usernet.mtu` when it is above 1500: the runner raises the file-handle attachment's
    /// `maximumTransmissionUnit` to match, otherwise Virtualization drops frames the gateway advertised room for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usernet_mtu: Option<u32>,
    pub console_ports: Vec<ConsolePortConfig>,
    /// The console log is rotated to `<log>.1` past this size (the runner defaults to 16 MiB).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial_log_max_bytes: Option<u64>,
    /// The VM's name in system services (macOS 27+); the runner trims it to 64 characters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// EFI Secure Boot (macOS 27+, Linux EFI guests). None leaves the variable store as it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure_boot: Option<bool>,
    pub secure_boot_reset: bool,
    pub secure_boot_default_signatures: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure_boot_platform_key: Option<PathBuf>,
    pub secure_boot_kek: Vec<PathBuf>,
    pub secure_boot_db: Vec<PathBuf>,
    pub secure_boot_dbx: Vec<PathBuf>,
    /// macOS guests: start up from macOS Recovery.
    pub recovery: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rosetta_cache: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsolePortConfig {
    pub name: String,
    pub socket: PathBuf,
}

/// The host end of VM `id`'s console port `name`.
pub fn console_port_socket(id: uuid::Uuid, name: &str) -> Result<PathBuf> {
    Ok(socket_dir()?.join(format!("{}.port-{name}", id.simple())))
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkConfig {
    pub name: String,
    pub mac: String,
    pub socket: PathBuf,
    /// The switch binary, so the runner can restart a switch that died.
    pub switch_bin: PathBuf,
    /// The network's MTU when it is above 1500: the runner raises the card's `maximumTransmissionUnit` to it.
    pub mtu: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShareConfig {
    pub tag: String,
    pub host_path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct VsockServiceConfig {
    pub port: u32,
    /// Host Unix socket the guest's connections are relayed to.
    pub socket: Option<PathBuf>,
    /// `metadata` or `telemetry`, answered by the runner.
    pub builtin: Option<String>,
    /// Loopback TCP port on this Mac that the guest's connections are relayed to (always 127.0.0.1; the runner has
    /// no host field to misuse).
    pub tcp_port: Option<u16>,
}

/// The host end of VM `id`'s allow-listed vsock service `name` (`apple.vsock_services[].socket`). The host-side service
/// listens here; only names that pass validation resolve, so a spec cannot point the guest at an arbitrary host socket.
pub fn vsock_service_socket(id: uuid::Uuid, name: &str) -> Result<PathBuf> {
    Ok(socket_dir()?.join(format!("{}.svc-{name}", id.simple())))
}

#[derive(Debug, Clone, Serialize)]
pub struct ForwardConfig {
    pub host_port: u16,
    pub guest_port: u16,
    /// Listen on the NAT gateway so other guests can connect (see `PortForward::guests`).
    pub guests: bool,
}

impl RunnerConfig {
    pub fn for_launch(req: &CreateVmRequest, ctx: &LaunchContext) -> Result<Self> {
        let apple = req.apple.clone().unwrap_or_default();
        let sb = apple.efi_secure_boot.as_ref();
        let id8: String = ctx.id.simple().to_string().chars().take(8).collect();
        Ok(Self {
            id: ctx.id.to_string(),
            workspace: ctx.workspace.clone(),
            cpus: req.vcpus,
            memory_mib: req.memory_mib,
            guest_os: match apple.guest_os {
                AppleGuest::Linux => "linux",
                AppleGuest::Macos => "macos",
            },
            disk: ctx.disk.clone(),
            seed: ctx.seed_disk.clone(),
            media: if crate::macos_install::is_macos_install(req) {
                Some(crate::macos_install::install_media(req))
            } else {
                apple.media.clone()
            },
            mac: stable_mac(&ctx.workspace)?,
            control_socket: control_socket_path(&id8)?,
            serial_log: ctx.log_path.clone(),
            // Always present: the daemon reaches an offline guest's sshd through it (the guest listens on vsock port 22).
            vsock_socket: Some(short_socket(
                &id8,
                "vsock",
                ctx.vsock_socket
                    .clone()
                    .unwrap_or_else(|| ctx.workspace.join("vsock.sock")),
            )),
            self_socket: if apple.self_control {
                SELF_CONTROL_SOCKET.get().cloned()
            } else {
                None
            },
            ip_file: ip_file(&ctx.workspace),
            window: apple.window,
            display_count: apple.display_count.clamp(1, 8),
            clipboard: apple.clipboard,
            bridge_interface: apple.bridge_interface.clone(),
            asif_overlay: apple.asif_overlay,
            provision_full_name: apple.provision_full_name.clone(),
            provision_username: apple.provision_username.clone(),
            provision_password_file: apple.provision_password_file.clone(),
            provision_auto_login: apple.provision_auto_login,
            provision_remote_login: apple.provision_remote_login,
            display_width: apple.display_width.clamp(800, 5120),
            display_height: apple.display_height.clamp(600, 2880),
            display_ppi: apple.display_ppi.clamp(72, 300),
            audio_output: apple.audio_output,
            microphone: apple.microphone,
            rosetta: apple.rosetta,
            nested_virtualization: apple.nested_virtualization,
            usb_controller: apple.usb_controller || apple.usb_controllers > 0,
            usb_controllers: apple.usb_controllers.max(1),
            vmnet: apple.vmnet.clone(),
            custom_virtio: apple.custom_virtio,
            macos_vsock: apple.macos_vsock,
            restore_state: req
                .loadvm_tag
                .as_deref()
                .map(|tag| snapshot_dir(&ctx.workspace, tag).join(STATE_FILE))
                // A disk-only snapshot has no saved state: its disk is put back and the guest boots cold.
                .filter(|state| state.is_file()),
            shares: req
                .shared_folders
                .iter()
                .enumerate()
                .map(|(i, f)| ShareConfig {
                    tag: format!("fs{i}"),
                    host_path: f.host_path.clone(),
                    read_only: f.read_only,
                })
                .chain(apple.tagged_shares.iter().map(|s| ShareConfig {
                    tag: s.tag.clone(),
                    host_path: s.host_path.clone(),
                    read_only: s.read_only,
                }))
                .chain(apple.init_config.is_some().then(|| ShareConfig {
                    tag: fluxvm_oci_init::config::META_TAG.into(),
                    host_path: oci_meta_dir(&ctx.workspace),
                    read_only: true,
                }))
                .chain(
                    (apple.guest_os == AppleGuest::Macos
                        && (apple.firstboot.is_some()
                            || crate::macos_install::wants_agent_seed(req)))
                    .then(|| ShareConfig {
                        tag: crate::macos_install::FIRSTBOOT_DIR.into(),
                        host_path: ctx.workspace.join(crate::macos_install::FIRSTBOOT_DIR),
                        read_only: true,
                    }),
                )
                .collect(),
            kernel: req.kernel.clone(),
            initrd: req.initrd.clone(),
            cmdline: req.kernel_args.clone(),
            root_read_only: apple.root_read_only,
            root_caching: apple.root_caching,
            root_sync: apple.root_sync,
            extra_disks: apple.extra_disks.clone(),
            networks: apple
                .networks
                .iter()
                .map(|n| {
                    Ok(NetworkConfig {
                        name: n.name.clone(),
                        mac: n
                            .mac
                            .clone()
                            .with_context(|| format!("network {} has no MAC assigned", n.name))?,
                        socket: crate::vznet::socket_path(&n.name)?,
                        switch_bin: PathBuf::new(),
                        mtu: n.mtu.filter(|m| *m > 1500),
                    })
                })
                .collect::<Result<_>>()?,
            usernet_fd: apple
                .usernet
                .as_ref()
                .map(|_| crate::usernet::RUNNER_NIC_FD),
            usernet_mtu: apple
                .usernet
                .as_ref()
                .and_then(|u| u.mtu)
                .filter(|m| *m > 1500)
                .map(u32::from),
            console_ports: apple
                .console_ports
                .iter()
                .map(|name| {
                    Ok(ConsolePortConfig {
                        name: name.clone(),
                        socket: console_port_socket(ctx.id, name)?,
                    })
                })
                .collect::<Result<_>>()?,
            serial_log_max_bytes: None,
            label: Some(req.name.clone()).filter(|n| !n.trim().is_empty()),
            secure_boot: req.secure_boot,
            secure_boot_reset: sb.is_some_and(|s| s.reset),
            secure_boot_default_signatures: sb.is_none_or(|s| s.default_signatures),
            secure_boot_platform_key: sb.and_then(|s| s.platform_key.clone()),
            secure_boot_kek: sb.map(|s| s.kek.clone()).unwrap_or_default(),
            secure_boot_db: sb.map(|s| s.db.clone()).unwrap_or_default(),
            secure_boot_dbx: sb.map(|s| s.dbx.clone()).unwrap_or_default(),
            recovery: apple.recovery,
            rosetta_cache: apple.rosetta_cache.clone(),
            network_none: matches!(req.network, NetworkSpec::None),
            egress_allow: apple.egress_allow.clone(),
            egress_via: None,
            egress_token: None,
            vsock_services: apple
                .vsock_services
                .iter()
                .map(|s| {
                    Ok(VsockServiceConfig {
                        port: s.port,
                        socket: match &s.socket {
                            Some(n) => Some(vsock_service_socket(ctx.id, n)?),
                            None => None,
                        },
                        builtin: s.builtin.clone(),
                        tcp_port: match &s.tcp {
                            Some(t) => Some(crate::capability::loopback_tcp_port(t)?),
                            None => None,
                        },
                    })
                })
                .collect::<Result<_>>()?,
            vsock_metadata: apple
                .vsock_services
                .iter()
                .any(|s| s.builtin.as_deref() == Some("metadata"))
                .then(|| {
                    serde_json::json!({
                        "id": ctx.id,
                        "name": req.name,
                        "vcpus": req.vcpus,
                        "memory_mib": req.memory_mib,
                    })
                    .to_string()
                }),
            telemetry_log: apple
                .vsock_services
                .iter()
                .any(|s| s.builtin.as_deref() == Some("telemetry"))
                .then(|| ctx.workspace.join("telemetry.log")),
            forwards: match &req.network {
                NetworkSpec::User { forwards } => forwards
                    .iter()
                    .map(|f| ForwardConfig {
                        host_port: f.host_port,
                        guest_port: f.guest_port,
                        guests: f.guests,
                    })
                    .collect(),
                _ => Vec::new(),
            },
        })
    }

    pub fn write(&self, workspace: &Path) -> Result<PathBuf> {
        let p = workspace.join("vz-config.json");
        fs::write(&p, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", p.display()))?;
        Ok(p)
    }
}

/// Where an OCI sandbox's `config.json` and agent token live (shared into the guest as `fluxvm-meta`).
pub fn oci_meta_dir(workspace: &Path) -> PathBuf {
    workspace.join("meta")
}

/// Writes `apple.init_config` and the agent token into [`oci_meta_dir`], replacing what a previous boot left.
pub fn write_oci_meta(req: &CreateVmRequest, workspace: &Path) -> Result<()> {
    write_oci_meta_as(req, workspace, fluxvm_oci_init::config::CONFIG_FILE)
}

/// [`write_oci_meta`] with the init config under `config_name` (a warm-pool claim writes a fresh name).
pub fn write_oci_meta_as(req: &CreateVmRequest, workspace: &Path, config_name: &str) -> Result<()> {
    use fluxvm_oci_init::config::{SECRETS_FILE, TOKEN_FILE};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let Some(apple) = req.apple.as_ref() else {
        return Ok(());
    };
    let Some(init) = apple.init_config.as_ref() else {
        return Ok(());
    };
    let dir = oci_meta_dir(workspace);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let mut init = init.clone();
    if !apple.networks.is_empty() && init["mode"] == "boot" {
        init["networks"] = serde_json::to_value(
            apple
                .networks
                .iter()
                .filter_map(|n| {
                    Some(serde_json::json!({"name": n.name, "mac": n.mac.as_ref()?, "address": n.address.as_ref()?}))
                })
                .collect::<Vec<_>>(),
        )?;
    }
    let tmp = dir.join(format!("{config_name}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(&init)?)?;
    fs::rename(&tmp, dir.join(config_name))?;
    // Only the creating launch carries secrets (they are never persisted); later boots keep the file it wrote.
    if !apple.secret_env.is_empty() {
        let values: std::collections::BTreeMap<&str, &str> = apple
            .secret_env
            .iter()
            .map(|(k, v)| (k.as_str(), v.expose()))
            .collect();
        let path = dir.join(SECRETS_FILE);
        let tmp = dir.join(format!("{SECRETS_FILE}.tmp"));
        let _ = fs::remove_file(&tmp);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        std::io::Write::write_all(&mut f, &serde_json::to_vec(&values)?)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
    }
    let token_path = dir.join(TOKEN_FILE);
    match req
        .agent
        .as_ref()
        .filter(|a| a.enabled)
        .and_then(|a| a.token.as_deref())
    {
        Some(token) => {
            fs::write(&token_path, token)?;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600))?;
        }
        None => {
            let _ = fs::remove_file(&token_path);
        }
    }
    Ok(())
}

/// The vsock port the runner's egress proxy listens on (guest to host); the guest forwards 127.0.0.1:3128 to it.
pub const EGRESS_PORT: u32 = 3128;
/// Guest TCP and vsock port for `apple.host_gpu`: clients use `http://127.0.0.1:8080/v1`.
pub const HOST_INFER_PORT: u32 = 8080;

/// The vsock port (and guest-local TCP port) of a `self_control` VM's MCP server.
pub const SELF_CONTROL_PORT: u32 = 7790;

static SELF_CONTROL_SOCKET: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where this daemon serves self-control, unique per state directory so several daemons of one user do not collide.
pub fn self_control_socket_path(state_dir: &Path) -> Result<PathBuf> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in state_dir.as_os_str().as_encoded_bytes() {
        h = (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3);
    }
    Ok(socket_dir()?.join(format!("self-{:08x}.sock", h as u32)))
}

/// Called once the daemon listens on `path`; VMs launched afterwards with `self_control` get the relay.
pub fn set_self_control_socket(path: PathBuf) {
    let _ = SELF_CONTROL_SOCKET.set(path);
}

pub const STATE_FILE: &str = "state.vzvmsave";
/// Files cloned alongside the saved state, so a restore gets the disk exactly as it was when the state was saved.
/// `auxiliary.bin` is a macOS guest's NVRAM.
pub const SNAPSHOT_FILES: &[&str] = &["disk.raw", "disk-overlay.asif", "efi.bin", "auxiliary.bin"];

pub fn snapshot_dir(workspace: &Path, tag: &str) -> PathBuf {
    workspace.join("snapshots").join(tag)
}

/// Copies snapshot `tag` of `parent` out of the VM's workspace into `dest`, laid out as a workspace of its own (the saved
/// state, disk and firmware files under `snapshots/<dest_tag>`, and the machine identifier, MAC and seed beside them), so
/// the checkpoint outlives the VM and [`prepare_fork_child`] can restore children from it unchanged. All clones are
/// copy-on-write on APFS. This is what a branch is made of.
pub fn export_checkpoint(parent: &Path, tag: &str, dest: &Path, dest_tag: &str) -> Result<()> {
    let src = snapshot_dir(parent, tag);
    if !src.join(STATE_FILE).is_file() {
        bail!("snapshot {tag:?} has no saved state at {}", src.display());
    }
    let dst = snapshot_dir(dest, dest_tag);
    fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;
    for name in std::iter::once(&STATE_FILE).chain(SNAPSHOT_FILES.iter()) {
        let from = src.join(name);
        if from.is_file() {
            clone_file(&from, &dst.join(name))?;
        }
    }
    for name in ["generic-id.bin", MAC_FILE, "seed.img"] {
        let from = parent.join(name);
        if from.is_file() {
            clone_file(&from, &dest.join(name))?;
        }
    }
    Ok(())
}

/// Populates a fork child's workspace `child` from snapshot `tag` of the parent workspace `parent`: the saved state and
/// its disk and firmware files (so the child can be started from the same tag), a clone of the snapshot disk as its
/// `root.raw`, and the per-VM files the saved device state is bound to: the machine identifier (`generic-id.bin`), the
/// MAC (`vz-mac`, marked shared so it is never recycled) and the seed image. All clones are copy-on-write on APFS, so
/// N children cost no disk until they write. Returns the child's disk path.
pub fn prepare_fork_child(parent: &Path, tag: &str, child: &Path) -> Result<PathBuf> {
    let src = snapshot_dir(parent, tag);
    if !src.join(STATE_FILE).is_file() {
        bail!("snapshot {tag:?} has no saved state at {}", src.display());
    }
    let dst = snapshot_dir(child, tag);
    fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;
    for name in std::iter::once(&STATE_FILE).chain(SNAPSHOT_FILES.iter()) {
        let from = src.join(name);
        if from.is_file() {
            clone_file(&from, &dst.join(name))?;
        }
    }
    let disk = child.join("root.raw");
    clone_file(&src.join("disk.raw"), &disk)?;
    for name in ["generic-id.bin", MAC_FILE, "seed.img"] {
        let from = parent.join(name);
        if from.is_file() {
            clone_file(&from, &child.join(name))?;
        }
    }
    fs::write(
        child.join(MAC_SHARED_MARKER),
        b"shared with the fork parent\n",
    )?;
    Ok(disk)
}

static CLONED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static COPIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Files cloned copy-on-write, and files that had to be fully copied because the volume cannot clone
/// (not APFS, or `from` and `to` on different volumes). A fork or checkpoint that copied is slow and
/// uses real disk, so callers compare the counters around an operation and say so.
pub fn clone_counters() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (CLONED.load(Relaxed), COPIED.load(Relaxed))
}

/// Copy-on-write clone (`clonefile(2)`; APFS), a plain copy where the volume cannot clone. Replaces `to`
/// if it exists. `Ok(true)` when the file was cloned, `Ok(false)` when it was copied.
pub fn clone_file_reported(from: &Path, to: &Path) -> Result<bool> {
    use std::sync::atomic::Ordering::Relaxed;
    let _ = fs::remove_file(to);
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let f = std::ffi::CString::new(from.as_os_str().as_bytes());
        let t = std::ffi::CString::new(to.as_os_str().as_bytes());
        if let (Ok(f), Ok(t)) = (f, t) {
            // SAFETY: both are valid NUL-terminated paths that outlive the call.
            if unsafe { libc::clonefile(f.as_ptr(), t.as_ptr(), 0) } == 0 {
                CLONED.fetch_add(1, Relaxed);
                return Ok(true);
            }
        }
    }
    fs::copy(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    COPIED.fetch_add(1, Relaxed);
    tracing::warn!(from = %from.display(), to = %to.display(), "could not clone copy-on-write; copied the file");
    Ok(false)
}

/// Copy-on-write clone on APFS, a plain copy elsewhere (counted; see [`clone_counters`]).
pub fn clone_file(from: &Path, to: &Path) -> Result<()> {
    clone_file_reported(from, to).map(|_| ())
}

/// Files that make a macOS guest the machine it is, kept beside its disk by the installer. A clone of a prepared guest takes
/// the hardware model and auxiliary storage (NVRAM) from the template; its machine identifier is deliberately not copied, so the
/// runner gives every clone its own and two clones are two machines.
pub const MACOS_TEMPLATE_FILES: &[&str] = &["hardware.bin", "auxiliary.bin"];

/// Clones a prepared macOS guest's files into `workspace`. `image` is the template's disk (`<dir>/disk.raw`); the files sit
/// beside it. `Ok(false)` means `image` is not a prepared guest.
pub fn adopt_macos_template(image: &Path, workspace: &Path) -> Result<bool> {
    let Some(dir) = image.parent() else {
        return Ok(false);
    };
    if MACOS_TEMPLATE_FILES.iter().any(|f| !dir.join(f).is_file()) {
        return Ok(false);
    }
    for f in MACOS_TEMPLATE_FILES {
        clone_file(&dir.join(f), &workspace.join(f))?;
    }
    Ok(true)
}

/// unix socket paths are limited to ~104 bytes on macOS, so sockets live under a short per-user directory.
pub(crate) fn socket_dir() -> Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/fluxvm-{uid}"));
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

fn control_socket_path(id8: &str) -> Result<PathBuf> {
    Ok(socket_dir()?.join(format!("{id8}.ctl")))
}

fn short_socket(id8: &str, kind: &str, requested: PathBuf) -> PathBuf {
    // Keep the requested path if it already fits; otherwise relocate under the short directory.
    if requested.as_os_str().len() < 100 {
        return requested;
    }
    socket_dir()
        .map(|d| d.join(format!("{id8}.{kind}")))
        .unwrap_or(requested)
}

const MAC_FILE: &str = "vz-mac";
/// Present in a fork child's workspace: its `vz-mac` is shared with the parent and is not returned to the pool.
const MAC_SHARED_MARKER: &str = "vz-mac.shared";

/// MACs of deleted VMs, shared by every daemon of this user. The Mac's DHCP server keeps a lease per MAC for a day, so a
/// fresh MAC per VM runs its /24 dry after about 250 creates; a recycled MAC gets its old address back.
fn mac_pool() -> Option<PathBuf> {
    socket_dir().ok().map(|d| d.join("vz-mac-pool"))
}

fn valid_mac(m: &str) -> bool {
    m.len() == 17 && m.starts_with("02:")
}

/// Hands a deleted VM's MAC back for reuse. Call before removing its workspace.
pub fn release_mac(workspace: &Path) {
    if let Some(pool) = mac_pool() {
        release_mac_to(workspace, &pool);
    }
}

fn release_mac_to(workspace: &Path, pool: &Path) {
    // A fork child's MAC is its parent's (the saved device state carries it), so it must never be recycled to a new VM
    // while the parent may still be running.
    if workspace.join(MAC_SHARED_MARKER).exists() {
        let _ = fs::remove_file(workspace.join(MAC_FILE));
        return;
    }
    let Ok(m) = fs::read_to_string(workspace.join(MAC_FILE)) else {
        return;
    };
    let m = m.trim();
    if valid_mac(m) && fs::create_dir_all(pool).is_ok() {
        let _ = fs::rename(workspace.join(MAC_FILE), pool.join(m.replace(':', "-")));
    }
}

/// Moves a pooled MAC into `f`; the rename is atomic, so two new VMs never get the same one.
fn reuse_mac(pool: &Path, f: &Path) -> Option<String> {
    for e in fs::read_dir(pool).ok()?.flatten() {
        if fs::rename(e.path(), f).is_ok()
            && let Ok(m) = fs::read_to_string(f)
            && valid_mac(m.trim())
        {
            return Some(m.trim().to_owned());
        }
    }
    None
}

/// A locally-administered MAC kept in the workspace so a VM keeps its identity (and DHCP lease) across restarts.
fn stable_mac(workspace: &Path) -> Result<String> {
    stable_mac_from(workspace, mac_pool().as_deref())
}

fn stable_mac_from(workspace: &Path, pool: Option<&Path>) -> Result<String> {
    let f = workspace.join(MAC_FILE);
    if let Ok(m) = fs::read_to_string(&f) {
        let m = m.trim().to_owned();
        if m.len() == 17 {
            return Ok(m);
        }
    }
    if let Some(m) = pool.and_then(|p| reuse_mac(p, &f)) {
        return Ok(m);
    }
    let u = uuid::Uuid::new_v4();
    let b = u.as_bytes();
    let mac = format!(
        "02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        b[0], b[1], b[2], b[3], b[4]
    );
    fs::write(&f, &mac)?;
    Ok(mac)
}

/// Where the runner writes the guest's address once the guest reports it on the serial console.
pub fn ip_file(workspace: &Path) -> PathBuf {
    workspace.join("vz-ip")
}

pub fn read_guest_ip(workspace: &Path) -> Option<String> {
    let s = fs::read_to_string(ip_file(workspace)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

pub(crate) fn open_runner_log(workspace: &Path) -> Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workspace.join("vz-runner.log"))
        .context("opening the runner log")
}

pub(crate) fn tail_runner_log(workspace: &Path) -> String {
    let s = fs::read_to_string(workspace.join("vz-runner.log")).unwrap_or_default();
    let lines: Vec<&str> = s.lines().rev().take(6).collect();
    let mut v = lines;
    v.reverse();
    if v.is_empty() {
        "(no runner output)".into()
    } else {
        v.join(" | ")
    }
}

/// The runner inside the Developer ID signed bundle (`FluxVMRunner.app`) that a release package ships when it has a
/// provisioning profile for `com.apple.vm.networking`. Restricted entitlements are only honoured for an executable
/// launched from its bundle, so this path, not a copy of the binary, is what must be run.
pub const RUNNER_BUNDLE_EXE: &str = "FluxVMRunner.app/Contents/MacOS/fluxvm-vz-runner";

/// Runner candidates in `dir`, best first: the networking bundle, then the plain runner.
pub fn runner_candidates(dir: &Path) -> [PathBuf; 2] {
    [dir.join(RUNNER_BUNDLE_EXE), dir.join("fluxvm-vz-runner")]
}

/// Locates the signed runner: `FLUXVM_VZ_RUNNER`, then next to the daemon binary (the `FluxVMRunner.app` bundle before
/// the plain binary), then the copy built by this crate.
pub fn find_runner() -> Result<PathBuf> {
    let mut tried = Vec::new();
    if let Some(p) = std::env::var_os("FLUXVM_VZ_RUNNER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        tried.push(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for p in runner_candidates(dir) {
            if p.is_file() {
                return Ok(p);
            }
            tried.push(p);
        }
    }
    if let Some(p) = option_env!("FLUXVM_VZ_RUNNER_BUILT") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        tried.push(p);
    }
    bail!(
        "the Apple runner `fluxvm-vz-runner` was not found (looked at: {}). Build it with `cargo build -p fluxvm-apple` on macOS or set FLUXVM_VZ_RUNNER.",
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A headless direct-boot Linux VM with no network card that runs until its guest powers off (the OCI rootfs builder).
#[derive(Debug, Clone)]
pub struct OneShotVm {
    pub workspace: PathBuf,
    pub disk: PathBuf,
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
    pub cpus: u8,
    pub memory_mib: u64,
    pub shares: Vec<ShareConfig>,
}

impl OneShotVm {
    pub fn runner_config(&self) -> Result<RunnerConfig> {
        let id = uuid::Uuid::new_v4();
        let id8: String = id.simple().to_string().chars().take(8).collect();
        Ok(RunnerConfig {
            id: id.to_string(),
            workspace: self.workspace.clone(),
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            guest_os: "linux",
            disk: self.disk.clone(),
            mac: stable_mac(&self.workspace)?,
            control_socket: control_socket_path(&id8)?,
            serial_log: self.serial_log(),
            ip_file: ip_file(&self.workspace),
            display_count: 1,
            display_width: 1280,
            display_height: 800,
            display_ppi: 220,
            shares: self.shares.clone(),
            network_none: true,
            kernel: Some(self.kernel.clone()),
            initrd: Some(self.initrd.clone()),
            cmdline: Some(self.cmdline.clone()),
            ..RunnerConfig::default()
        })
    }

    pub fn serial_log(&self) -> PathBuf {
        self.workspace.join("serial.log")
    }

    /// Boots the VM and waits for the runner to exit (the guest powered off), killing it after `timeout`.
    /// Returns the guest's serial console output.
    pub async fn run(&self, timeout: std::time::Duration) -> Result<String> {
        let runner = find_runner()?;
        let conf = self.runner_config()?;
        let conf_path = conf.write(&self.workspace)?;
        let log = open_runner_log(&self.workspace)?;
        let mut child = tokio::process::Command::new(&runner)
            .args(["run", "--config"])
            .arg(&conf_path)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone().context("cloning the runner log")?)
            .stderr(log)
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", runner.display()))?;
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(s) => s?,
            Err(_) => {
                let _ = child.kill().await;
                bail!(
                    "the VM did not power off within {}s: {}",
                    timeout.as_secs(),
                    tail_runner_log(&self.workspace)
                );
            }
        };
        let serial = fs::read_to_string(self.serial_log()).unwrap_or_default();
        if !status.success() {
            bail!(
                "the Apple runner exited ({status}): {}",
                tail_runner_log(&self.workspace)
            );
        }
        Ok(serial)
    }
}

#[cfg(test)]
mod one_shot_tests {
    use super::*;

    #[test]
    fn a_clone_is_copy_on_write_where_the_volume_allows_and_replaces_the_target() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::write(&a, b"one").unwrap();
        fs::write(&b, b"stale").unwrap();
        let before = clone_counters();
        let cloned = clone_file_reported(&a, &b).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"one");
        let after = clone_counters();
        // Counters are process-wide and other tests clone too: only check that this call moved one of them.
        if cloned {
            assert!(after.0 > before.0);
        } else {
            assert!(after.1 > before.1);
        }
        // Writing the clone leaves the source alone.
        fs::write(&b, b"two").unwrap();
        assert_eq!(fs::read(&a).unwrap(), b"one");
    }

    #[test]
    fn one_shot_vm_is_headless_offline_direct_boot() {
        let dir = tempfile::tempdir().unwrap();
        let vm = OneShotVm {
            workspace: dir.path().into(),
            disk: dir.path().join("disk.raw"),
            kernel: "/k/oci-kernel".into(),
            initrd: "/k/oci-initrd".into(),
            cmdline: "console=hvc0".into(),
            cpus: 2,
            memory_mib: 1024,
            shares: vec![ShareConfig {
                tag: "fluxvm-meta".into(),
                host_path: dir.path().join("meta"),
                read_only: true,
            }],
        };
        let c = vm.runner_config().unwrap();
        assert_eq!(c.guest_os, "linux");
        assert!(
            c.network_none && !c.window && c.vsock_socket.is_none() && c.egress_allow.is_empty()
        );
        assert_eq!(c.kernel.as_deref(), Some(Path::new("/k/oci-kernel")));
        assert_eq!(c.shares[0].tag, "fluxvm-meta");
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["cmdline"], "console=hvc0");
        assert_eq!(
            json["serial_log"],
            dir.path().join("serial.log").display().to_string()
        );
    }

    #[test]
    fn secrets_go_only_to_a_private_meta_file_that_later_boots_keep() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut req: CreateVmRequest = serde_json::from_value(serde_json::json!({
            "name": "s", "backend": "vz", "image": "/r.ext4", "kernel": "/k",
            "apple": {"init_config": {"mode": "boot", "hostname": "s"}},
        }))
        .unwrap();
        req.apple.as_mut().unwrap().secret_env.insert(
            "DB_PASSWORD".into(),
            fluxvm_core::grants::Secret::new("hunter2"),
        );
        assert!(!serde_json::to_string(&req).unwrap().contains("hunter2"));
        assert!(!format!("{req:?}").contains("hunter2"));
        write_oci_meta(&req, dir.path()).unwrap();
        let meta = oci_meta_dir(dir.path());
        let secrets = meta.join("secrets.json");
        assert_eq!(
            fs::read_to_string(&secrets).unwrap(),
            r#"{"DB_PASSWORD":"hunter2"}"#
        );
        assert_eq!(
            fs::metadata(&secrets).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            !fs::read_to_string(meta.join("config.json"))
                .unwrap()
                .contains("hunter2")
        );

        let reloaded: CreateVmRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        write_oci_meta(&reloaded, dir.path()).unwrap();
        assert!(fs::read_to_string(&secrets).unwrap().contains("hunter2"));
    }

    #[test]
    fn oci_sandbox_meta_holds_config_and_token() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let req: CreateVmRequest = serde_json::from_value(serde_json::json!({
            "name": "s", "backend": "vz", "image": "/r.ext4", "kernel": "/k",
            "agent": {"enabled": true, "port": 17777, "token": "t0k"},
            "apple": {"init_config": {"mode": "boot", "hostname": "s"}},
        }))
        .unwrap();
        write_oci_meta(&req, dir.path()).unwrap();
        let meta = oci_meta_dir(dir.path());
        let cfg: serde_json::Value =
            serde_json::from_slice(&fs::read(meta.join("config.json")).unwrap()).unwrap();
        assert_eq!(cfg["hostname"], "s");
        assert_eq!(fs::read_to_string(meta.join("agent.token")).unwrap(), "t0k");
        let mode = fs::metadata(meta.join("agent.token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        let ctx = fluxvm_core::backend::LaunchContext {
            id: uuid::Uuid::new_v4(),
            workspace: dir.path().into(),
            disk: dir.path().join("root.raw"),
            seed_disk: None,
            log_path: dir.path().join("console.log"),
            network: fluxvm_core::backend::PreparedNetwork {
                spec: NetworkSpec::None,
                tap_name: None,
                tap_fd: None,
                netns: None,
                dhcp_leasefile: None,
                guest_ip: None,
                guest_cidr: None,
                gateway: None,
                extra_tap_fds: vec![],
            },
            guest_cid: Some(3),
            vsock_socket: None,
            disk_format: "raw".into(),
            nbd_export: None,
            machine_type: None,
        };
        let conf = RunnerConfig::for_launch(&req, &ctx).unwrap();
        let share = conf.shares.iter().find(|s| s.tag == "fluxvm-meta").unwrap();
        assert!(share.read_only && share.host_path == meta);
    }

    #[test]
    fn a_macos_guest_with_the_host_agent_gets_the_firstboot_share_for_its_token() {
        let share_tags = |apple: serde_json::Value, agent: serde_json::Value| -> Vec<String> {
            let dir = tempfile::tempdir().unwrap();
            let req: CreateVmRequest = serde_json::from_value(serde_json::json!({
                "name": "m", "backend": "vz", "image": "/r.raw", "apple": apple, "agent": agent,
            }))
            .unwrap();
            let ctx = fluxvm_core::backend::LaunchContext {
                id: uuid::Uuid::new_v4(),
                workspace: dir.path().into(),
                disk: dir.path().join("root.raw"),
                seed_disk: None,
                log_path: dir.path().join("console.log"),
                network: fluxvm_core::backend::PreparedNetwork {
                    spec: NetworkSpec::None,
                    tap_name: None,
                    tap_fd: None,
                    netns: None,
                    dhcp_leasefile: None,
                    guest_ip: None,
                    guest_cidr: None,
                    gateway: None,
                    extra_tap_fds: vec![],
                },
                guest_cid: None,
                vsock_socket: None,
                disk_format: "raw".into(),
                nbd_export: None,
                machine_type: None,
            };
            RunnerConfig::for_launch(&req, &ctx)
                .unwrap()
                .shares
                .into_iter()
                .map(|s| s.tag)
                .collect()
        };
        let on = serde_json::json!({"enabled": true, "token": "0123456789abcdef-token"});
        let mac = |extra: serde_json::Value| {
            let mut a = serde_json::json!({"guest_os": "macos", "macos_vsock": true});
            a.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            a
        };
        assert!(
            share_tags(mac(serde_json::json!({})), on.clone()).contains(&"firstboot".to_string())
        );
        // No host agent, or no vsock, or a Linux guest: no share for it.
        assert!(
            !share_tags(
                mac(serde_json::json!({})),
                serde_json::json!({"enabled": false})
            )
            .contains(&"firstboot".to_string())
        );
        assert!(
            !share_tags(serde_json::json!({"guest_os": "macos"}), on.clone())
                .contains(&"firstboot".to_string())
        );
        assert!(
            !share_tags(serde_json::json!({"guest_os": "linux"}), on)
                .contains(&"firstboot".to_string())
        );
    }

    #[test]
    fn a_jumbo_usernet_mtu_reaches_the_runner_and_the_default_does_not() {
        let launch = |usernet: serde_json::Value| {
            let dir = tempfile::tempdir().unwrap();
            let req: CreateVmRequest = serde_json::from_value(serde_json::json!({
                "name": "m", "backend": "vz", "image": "/r.raw",
                "apple": {"usernet": usernet},
            }))
            .unwrap();
            let ctx = fluxvm_core::backend::LaunchContext {
                id: uuid::Uuid::new_v4(),
                workspace: dir.path().into(),
                disk: dir.path().join("root.raw"),
                seed_disk: None,
                log_path: dir.path().join("console.log"),
                network: fluxvm_core::backend::PreparedNetwork {
                    spec: NetworkSpec::None,
                    tap_name: None,
                    tap_fd: None,
                    netns: None,
                    dhcp_leasefile: None,
                    guest_ip: None,
                    guest_cidr: None,
                    gateway: None,
                    extra_tap_fds: vec![],
                },
                guest_cid: None,
                vsock_socket: None,
                disk_format: "raw".into(),
                nbd_export: None,
                machine_type: None,
            };
            RunnerConfig::for_launch(&req, &ctx).unwrap().usernet_mtu
        };
        assert_eq!(launch(serde_json::json!({"mtu": 9000})), Some(9000));
        assert_eq!(launch(serde_json::json!({"mtu": 1500})), None);
        assert_eq!(launch(serde_json::json!({"mtu": 1400})), None);
        assert_eq!(launch(serde_json::json!({})), None);
    }

    #[test]
    fn a_fork_childs_mac_is_never_recycled_while_the_parent_may_run() {
        let dir = tempfile::tempdir().unwrap();
        let (parent, child) = (dir.path().join("p"), dir.path().join("c"));
        for w in [&parent, &child] {
            fs::create_dir_all(w).unwrap();
        }
        let tag = "fork-1";
        let snap = snapshot_dir(&parent, tag);
        fs::create_dir_all(&snap).unwrap();
        fs::write(snap.join(STATE_FILE), b"state").unwrap();
        fs::write(snap.join("disk.raw"), b"disk").unwrap();
        fs::write(snap.join("efi.bin"), b"efi").unwrap();
        fs::write(parent.join("generic-id.bin"), b"machine-id").unwrap();
        fs::write(parent.join(MAC_FILE), "02:aa:bb:cc:dd:ee").unwrap();
        fs::write(parent.join("seed.img"), b"seed").unwrap();

        let disk = prepare_fork_child(&parent, tag, &child).unwrap();
        assert_eq!(disk, child.join("root.raw"));
        assert_eq!(fs::read(&disk).unwrap(), b"disk");
        let csnap = snapshot_dir(&child, tag);
        assert_eq!(fs::read(csnap.join(STATE_FILE)).unwrap(), b"state");
        assert_eq!(fs::read(csnap.join("efi.bin")).unwrap(), b"efi");
        assert_eq!(
            fs::read(child.join("generic-id.bin")).unwrap(),
            b"machine-id"
        );
        assert_eq!(
            fs::read_to_string(child.join(MAC_FILE)).unwrap(),
            "02:aa:bb:cc:dd:ee"
        );
        assert_eq!(fs::read(child.join("seed.img")).unwrap(), b"seed");
        // The parent keeps its own files untouched.
        assert_eq!(fs::read(snap.join("disk.raw")).unwrap(), b"disk");

        let pool = dir.path().join("pool");
        release_mac_to(&child, &pool);
        assert!(!child.join(MAC_FILE).exists(), "the child's MAC file goes");
        assert!(
            !pool.exists() || fs::read_dir(&pool).unwrap().next().is_none(),
            "a shared MAC must not enter the pool"
        );
        // The parent's own MAC is still recycled as before.
        release_mac_to(&parent, &pool);
        assert_eq!(fs::read_dir(&pool).unwrap().count(), 1);
    }

    #[test]
    fn a_checkpoint_outlives_its_vm_and_restores_children() {
        let dir = tempfile::tempdir().unwrap();
        let (vm, branch, child) = (
            dir.path().join("vm"),
            dir.path().join("branch"),
            dir.path().join("child"),
        );
        for w in [&vm, &child] {
            fs::create_dir_all(w).unwrap();
        }
        let snap = snapshot_dir(&vm, "s");
        fs::create_dir_all(&snap).unwrap();
        fs::write(snap.join(STATE_FILE), b"state").unwrap();
        fs::write(snap.join("disk.raw"), b"disk").unwrap();
        fs::write(snap.join("efi.bin"), b"efi").unwrap();
        fs::write(vm.join("generic-id.bin"), b"mid").unwrap();
        fs::write(vm.join(MAC_FILE), "02:11:22:33:44:55").unwrap();
        fs::write(vm.join("seed.img"), b"seed").unwrap();

        export_checkpoint(&vm, "s", &branch, "ckpt").unwrap();
        // The VM and its snapshot go away; the checkpoint is self-contained.
        fs::remove_dir_all(&vm).unwrap();
        let disk = prepare_fork_child(&branch, "ckpt", &child).unwrap();
        assert_eq!(fs::read(&disk).unwrap(), b"disk");
        assert_eq!(
            fs::read(snapshot_dir(&child, "ckpt").join(STATE_FILE)).unwrap(),
            b"state"
        );
        assert_eq!(fs::read(child.join("generic-id.bin")).unwrap(), b"mid");
        assert_eq!(
            fs::read_to_string(child.join(MAC_FILE)).unwrap(),
            "02:11:22:33:44:55"
        );
        assert!(child.join(MAC_SHARED_MARKER).exists());
        // Several children from one checkpoint do not disturb it.
        let child2 = dir.path().join("child2");
        fs::create_dir_all(&child2).unwrap();
        prepare_fork_child(&branch, "ckpt", &child2).unwrap();
        assert_eq!(
            fs::read(snapshot_dir(&branch, "ckpt").join("disk.raw")).unwrap(),
            b"disk"
        );
    }

    #[test]
    fn fork_child_needs_a_saved_state() {
        let dir = tempfile::tempdir().unwrap();
        let (parent, child) = (dir.path().join("p"), dir.path().join("c"));
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&child).unwrap();
        let err = prepare_fork_child(&parent, "nope", &child).unwrap_err();
        assert!(format!("{err:#}").contains("no saved state"));
    }

    #[test]
    fn a_deleted_vms_mac_goes_to_the_next_new_vm() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a"),
            dir.path().join("b"),
            dir.path().join("c"),
        );
        for w in [&a, &b, &c] {
            fs::create_dir_all(w).unwrap();
        }
        let pool = dir.path().join("pool");
        let mac = stable_mac_from(&a, Some(&pool)).unwrap();
        assert_eq!(
            stable_mac_from(&a, Some(&pool)).unwrap(),
            mac,
            "stable across launches"
        );
        release_mac_to(&a, &pool);
        assert!(!a.join(MAC_FILE).exists());
        assert_eq!(
            stable_mac_from(&b, Some(&pool)).unwrap(),
            mac,
            "reused by the next VM"
        );
        assert_ne!(
            stable_mac_from(&c, Some(&pool)).unwrap(),
            mac,
            "the pool is empty again"
        );
    }
}

#[cfg(test)]
mod template_tests {
    use super::*;

    #[test]
    fn the_networking_bundle_is_preferred_over_the_plain_runner() {
        let d = tempfile::tempdir().unwrap();
        let [bundle, plain] = runner_candidates(d.path());
        assert!(bundle.ends_with("FluxVMRunner.app/Contents/MacOS/fluxvm-vz-runner"));
        assert!(plain.ends_with("fluxvm-vz-runner"));
        assert!(!plain.to_string_lossy().contains(".app"));
    }

    #[test]
    fn a_prepared_guest_is_cloned_without_its_machine_identifier() {
        let d = tempfile::tempdir().unwrap();
        let (tmpl, ws) = (d.path().join("tmpl"), d.path().join("ws"));
        fs::create_dir_all(&tmpl).unwrap();
        fs::create_dir_all(&ws).unwrap();
        for f in ["disk.raw", "hardware.bin", "auxiliary.bin", "identity.bin"] {
            fs::write(tmpl.join(f), f).unwrap();
        }
        assert!(adopt_macos_template(&tmpl.join("disk.raw"), &ws).unwrap());
        assert_eq!(
            fs::read_to_string(ws.join("hardware.bin")).unwrap(),
            "hardware.bin"
        );
        assert_eq!(
            fs::read_to_string(ws.join("auxiliary.bin")).unwrap(),
            "auxiliary.bin"
        );
        assert!(
            !ws.join("identity.bin").exists(),
            "each clone gets its own identity"
        );
    }

    #[test]
    fn a_plain_disk_is_not_a_prepared_guest() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("disk.raw"), "x").unwrap();
        assert!(!adopt_macos_template(&d.path().join("disk.raw"), d.path()).unwrap());
    }
}
