// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Apple Virtualization.framework backend for FluxVM (`backend: "vz"`).
//!
//! The daemon never links Virtualization.framework. Each VM is run by a small signed helper,
//! `fluxvm-vz-runner` (see `runner/Runner.swift`), which the backend supervises over a unix control
//! socket (one JSON line per request). See `docs/macos.md` for the contract and the capability matrix.

pub mod attest;
pub mod blockd;
mod capability;
mod control;
pub mod display;
mod host_caps;
pub mod host_health;
pub mod host_infer;
pub mod ipsw;
pub mod keepawake;
pub mod macos_install;
mod runner;
pub mod screen;
pub mod ssh;
mod usage;
mod usb_passthrough;
pub mod usernet;
pub mod usernet_policy;
pub use usernet::usernet_forward_addrs;
pub mod vz27;
pub mod vznet;

pub use attest::{Attestation, verify_attestation, verify_attestation_hex};
pub use capability::{
    CAPABILITIES, Capability, CapabilityStatus, RUNNER_MIN_MACOS, capability_report,
    current_host_capabilities, macos_major, set_host_capabilities, validate_disk, validate_request,
    validate_request_with_host, wire_host_gpu, with_egress_forwarder, with_guest_reporting,
    with_host_gpu_forwarder, with_self_control_forwarder, with_shared_folder_mounts,
};
pub use control::{ControlReply, call as control_call, call_with as control_call_with};
pub use display::{
    DISPLAY_RESIZE_NOTE, display_resize, display_resize_reply, display_resize_request,
};
pub use host_caps::{AppleHostCapabilities, host_capabilities, prime_host_capabilities};
pub use host_infer::{HostInferStatus, probe as probe_host_infer};
pub use ipsw::{IpswInfo, latest_ipsw};
pub use runner::{
    EGRESS_PORT, ForwardConfig, HOST_INFER_PORT, MACOS_TEMPLATE_FILES, OneShotVm, RunnerConfig,
    SELF_CONTROL_PORT, SNAPSHOT_FILES, STATE_FILE, ShareConfig, adopt_macos_template,
    clone_counters, clone_file, clone_file_reported, console_port_socket, export_checkpoint,
    find_runner, ip_file, oci_meta_dir, prepare_fork_child, read_guest_ip, release_mac,
    self_control_socket_path, set_self_control_socket, snapshot_dir, write_oci_meta_as,
};
pub use usage::{
    RunnerUsage, VmUsage, attribute_services, is_vz_service_path, mach_ticks_to_seconds,
    process_started_at, runner_usage, vm_usage,
};
pub use usb_passthrough::{physical_usb_attach, physical_usb_list};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use fluxvm_core::{
    backend::{LaunchContext, LaunchResult, VmBackend},
    config::Config,
    model::{AppleGuest, BackendKind, CreateVmRequest, VmRecord},
};
use serde::Deserialize;
use std::time::Duration;

/// Whether Rosetta is installed on this Mac (`softwareupdate --install-rosetta`); `apple.rosetta` fails without it.
pub fn rosetta_installed() -> bool {
    std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppleBalloonStatus {
    pub memory_mib: u64,
    pub target_mib: u64,
    pub actual_mib: u64,
}

pub async fn balloon_control(
    vm: &VmRecord,
    balloon_mib: Option<u64>,
) -> Result<AppleBalloonStatus> {
    let sock = vm
        .control_socket
        .as_deref()
        .context("VM has no runner control socket recorded")?;
    let mut body = serde_json::json!({"cmd": "balloon"});
    if let Some(v) = balloon_mib {
        body["balloon_mib"] = serde_json::json!(v);
    }
    let reply = control_call_with(sock, body)
        .await
        .context("runner `balloon`")?;
    if !reply.ok() {
        bail!(
            "runner refused `balloon`: {}",
            reply.error().unwrap_or("unknown error")
        );
    }
    serde_json::from_value(reply.0).context("decoding Apple balloon response")
}

pub async fn usb_attach(
    vm: &VmRecord,
    path: &std::path::Path,
    read_only: bool,
    bus: Option<u8>,
) -> Result<String> {
    let sock = vm
        .control_socket
        .as_deref()
        .context("VM has no runner control socket recorded")?;
    let reply = control_call_with(
        sock,
        serde_json::json!({"cmd":"usb-attach","path":path,"read_only":read_only,"bus":bus.unwrap_or(0)}),
    )
    .await?;
    if !reply.ok() {
        bail!(
            "runner refused usb-attach: {}",
            reply.error().unwrap_or("unknown error")
        );
    }
    reply
        .0
        .get("uuid")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .context("usb-attach reply had no uuid")
}

pub async fn usb_detach(vm: &VmRecord, uuid: &str) -> Result<()> {
    let sock = vm
        .control_socket
        .as_deref()
        .context("VM has no runner control socket recorded")?;
    let reply =
        control_call_with(sock, serde_json::json!({"cmd":"usb-detach","uuid":uuid})).await?;
    if reply.ok() {
        Ok(())
    } else {
        bail!(
            "runner refused usb-detach: {}",
            reply.error().unwrap_or("unknown error")
        )
    }
}

/// Points the running VM's virtiofs share `tag` at `path`.
pub async fn share_set(
    vm: &VmRecord,
    tag: &str,
    path: &std::path::Path,
    read_only: bool,
) -> Result<()> {
    let sock = vm
        .control_socket
        .as_deref()
        .context("VM has no runner control socket recorded")?;
    let reply = control_call_with(
        sock,
        serde_json::json!({"cmd":"share-set","tag":tag,"path":path,"read_only":read_only}),
    )
    .await?;
    if reply.ok() {
        Ok(())
    } else {
        bail!(
            "runner refused share-set: {}",
            reply.error().unwrap_or("unknown error")
        )
    }
}

/// Hands a waiting warm-pool guest its [`Claim`](fluxvm_oci_init::config::Claim) over the runner's vsock proxy.
pub async fn warm_claim(
    vm: &VmRecord,
    port: u32,
    claim: &fluxvm_oci_init::config::Claim,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    claim.validate()?;
    let sock = vm
        .vsock_socket
        .as_deref()
        .context("VM has no vsock socket recorded")?;
    let talk = async {
        let s = tokio::net::UnixStream::connect(sock)
            .await
            .with_context(|| format!("connecting to {}", sock.display()))?;
        let mut s = BufReader::new(s);
        s.get_mut()
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .await?;
        let mut line = String::new();
        s.read_line(&mut line).await?;
        if !line.starts_with("OK") {
            bail!(
                "the guest is not listening on vsock port {port}: {}",
                line.trim()
            );
        }
        let mut body = serde_json::to_vec(claim)?;
        body.push(b'\n');
        s.get_mut().write_all(&body).await?;
        line.clear();
        s.read_line(&mut line).await?;
        match line.trim() {
            "ok" => Ok(()),
            "" => bail!("the guest closed the claim without answering"),
            other => bail!("the guest refused the claim: {other}"),
        }
    };
    tokio::time::timeout(Duration::from_secs(20), talk)
        .await
        .context("the guest did not answer the claim in 20s")?
}

/// How long `launch` waits for the runner to report a running guest.
const START_TIMEOUT: Duration = Duration::from_secs(120);

pub struct AppleBackend;

#[async_trait]
impl VmBackend for AppleBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Vz
    }

    async fn launch(
        &self,
        cfg: &Config,
        req: &CreateVmRequest,
        ctx: &LaunchContext,
    ) -> Result<LaunchResult> {
        // `loadvm_tag` is how the scheduler asks for a restore; creation still rejects it (see `validate_request`).
        validate_request(&CreateVmRequest {
            loadvm_tag: None,
            ..req.clone()
        })?;
        if let Some(tag) = &req.loadvm_tag {
            restore_files(&ctx.workspace, &ctx.disk, tag)?;
        }
        let guest = req.apple.as_ref().map(|a| a.guest_os).unwrap_or_default();
        let installs_itself = macos_install::is_macos_install(req);
        let install =
            installs_itself && !ctx.workspace.join(macos_install::INSTALLED_MARKER).exists();
        if guest == AppleGuest::Macos
            && !installs_itself
            && !ctx.workspace.join("hardware.bin").exists()
            && !adopt_macos_template(&req.image, &ctx.workspace)?
        {
            bail!(
                "a macOS guest needs a prepared template (set `image` to the disk.raw of an installed guest, with its hardware.bin and auxiliary.bin beside it) or `apple.install: true` with an IPSW"
            );
        }
        if let Some(fb) = req.apple.as_ref().and_then(|a| a.firstboot.as_ref()) {
            macos_install::write_firstboot(&ctx.workspace, fb)?;
        }
        if macos_install::wants_agent_seed(req)
            && let Some(token) = req.agent.as_ref().and_then(|a| a.token.as_deref())
        {
            macos_install::write_agent_token(&ctx.workspace, token)?;
        }
        runner::write_oci_meta(req, &ctx.workspace)?;
        let runner = find_runner()?;
        let mut conf = RunnerConfig::for_launch(req, ctx)?;
        conf.serial_log_max_bytes = Some(cfg.apple.serial_log_max_mib.max(1) << 20);
        // A guest with a host allow-list reaches the network only through the runner's proxy. When the daemon runs its
        // own egress proxy on loopback, relay through it so per-session method/path rules and grants can apply.
        if !conf.egress_allow.is_empty()
            && let Ok(addr) = cfg
                .sandbox
                .egress_proxy_listen
                .parse::<std::net::SocketAddr>()
            && addr.ip() == std::net::Ipv4Addr::LOCALHOST
        {
            let (_, token) = fluxvm_core::peer_alias::register(ctx.id);
            conf.egress_via = Some(format!("127.0.0.1:{}", addr.port()));
            conf.egress_token = Some(token);
        }
        // Signed boot manifest in the `metadata` vsock service (a host claim, not hardware attestation; see `attest`).
        // A failure leaves the metadata without an `attestation` field rather than blocking the boot.
        if let Some(meta) = conf.vsock_metadata.take() {
            conf.vsock_metadata = Some(
                match attest::attach(
                    cfg.state_dir.clone(),
                    meta.clone(),
                    ctx.id.to_string(),
                    req.clone(),
                    ctx.disk.clone(),
                    runner.clone(),
                )
                .await
                {
                    Ok(with) => {
                        // Kept for `GET /v1/vms/{id}/vz/attestation`; failing to keep it does not block the boot.
                        if let Err(e) = attest::persist(&ctx.workspace, &with) {
                            tracing::warn!(vm = %ctx.id, error = %e, "boot attestation not recorded");
                        }
                        with
                    }
                    Err(e) => {
                        attest::clear(&ctx.workspace);
                        tracing::warn!(vm = %ctx.id, error = %e, "boot attestation not produced");
                        meta
                    }
                },
            );
        }
        for net in &mut conf.networks {
            net.socket = vznet::ensure_switch(&net.name).await?;
            net.switch_bin = vznet::find_switch()?;
        }
        // Overlay disks: one supervised `fluxvm-blockd` per VM, attached to the runner as `nbd` disks.
        let blockd = match req.apple.as_ref() {
            Some(a) if !blockd::exports(&a.extra_disks).is_empty() => {
                let b = blockd::Blockd::start(blockd::exports(&a.extra_disks), &ctx.workspace)
                    .await
                    .context("starting the overlay disk server")?;
                conf.extra_disks = blockd::runner_disks(&a.extra_disks, b.port);
                Some(b)
            }
            _ => None,
        };
        // apple.usernet: the NIC socketpair is made here so the runner and the supervised gateway each inherit an end.
        let gateway = match req.apple.as_ref().and_then(|a| a.usernet.as_ref()) {
            Some(u) => Some(usernet::Gateway::prepare(
                u,
                ctx.id,
                &ctx.workspace,
                &conf.mac,
            )?),
            None => None,
        };
        let conf_path = conf.write(&ctx.workspace)?;
        if install {
            macos_install::run_install(&runner, &conf_path, &ctx.workspace).await?;
        }
        // A restarted VM must not report the previous boot's address.
        // (A restore keeps it: the restored guest does not announce its address again.)
        // A disk-only snapshot boots cold, so it announces its address again like any start.
        if !req
            .loadvm_tag
            .as_deref()
            .is_some_and(|tag| snapshot_has_state(&ctx.workspace, tag))
        {
            let _ = std::fs::remove_file(ip_file(&ctx.workspace));
        }
        let log = runner::open_runner_log(&ctx.workspace)?;
        let mut cmd = tokio::process::Command::new(&runner);
        cmd.args(["run", "--config"]).arg(&conf_path);
        cmd.stdin(std::process::Stdio::null())
            .stdout(log.try_clone().context("cloning the runner log")?)
            .stderr(log)
            .process_group(0);
        if let Some(g) = &gateway {
            let nic = g.runner_fd();
            // SAFETY: only async-signal-safe calls (dup2/fcntl) run between fork and exec.
            unsafe {
                cmd.pre_exec(move || usernet::install_fd(nic, usernet::RUNNER_NIC_FD));
            }
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", runner.display()))?;
        let pid = child.id().context("runner exited immediately")?;
        // Dropping `gateway_stop` (a failed launch below, or the runner exiting) stops the gateway.
        let (gateway_stop, gateway_rx) = tokio::sync::watch::channel(false);
        // (the same channel stops the overlay disk server)
        if let Some(g) = gateway {
            g.supervise(pid, gateway_rx.clone());
        }
        if let Some(b) = blockd {
            b.supervise(pid, gateway_rx);
        }

        // Ready = the control socket answers `status` with a running guest. A runner that exits first has failed.
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                bail!(
                    "the Apple runner exited ({status}): {}",
                    runner::tail_runner_log(&ctx.workspace)
                );
            }
            if let Ok(reply) = control_call(&conf.control_socket, "status").await
                && reply.state() == Some("running")
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let _ = child.start_kill();
                bail!(
                    "the Apple runner did not report a running guest within {}s: {}",
                    START_TIMEOUT.as_secs(),
                    runner::tail_runner_log(&ctx.workspace)
                );
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Reap the runner in the background; it outlives this call and is stopped through its control socket or SIGTERM.
        tokio::spawn(async move {
            let _ = child.wait().await;
            drop(gateway_stop);
        });
        Ok(LaunchResult {
            pid,
            control_socket: Some(conf.control_socket.clone()),
            jail_path: None,
            vsock_socket: conf.vsock_socket.clone(),
            virtiofsd_pids: Vec::new(),
            swtpm_pid: None,
        })
    }

    async fn pause(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        send(vm, "pause").await
    }

    async fn resume(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        send(vm, "resume").await
    }

    async fn graceful_shutdown(&self, _cfg: &Config, vm: &VmRecord) -> Result<()> {
        send(vm, "shutdown").await
    }
}

async fn send(vm: &VmRecord, cmd: &str) -> Result<()> {
    let sock = vm
        .control_socket
        .as_deref()
        .context("VM has no runner control socket recorded")?;
    let reply = control_call(sock, cmd)
        .await
        .with_context(|| format!("runner `{cmd}`"))?;
    if reply.ok() {
        Ok(())
    } else {
        bail!(
            "runner refused `{cmd}`: {}",
            reply.error().unwrap_or("unknown error")
        )
    }
}

/// Puts the disk and EFI variables from snapshot `tag` back in place, ahead of resuming the saved state (or of a cold
/// boot, for a disk-only snapshot).
fn restore_files(workspace: &std::path::Path, disk: &std::path::Path, tag: &str) -> Result<()> {
    let dir = snapshot_dir(workspace, tag);
    if !dir.join(STATE_FILE).is_file() && !dir.join("disk.raw").is_file() {
        bail!(
            "snapshot {tag:?} has no saved state or disk at {}",
            dir.display()
        );
    }
    for name in SNAPSHOT_FILES {
        let src = dir.join(name);
        let dst = if *name == "disk.raw" {
            disk.to_path_buf()
        } else {
            workspace.join(name)
        };
        if src.is_file() {
            clone_file(&src, &dst)?;
        }
    }
    Ok(())
}

/// Clones the disk and every other present `SNAPSHOT_FILES` entry from the workspace into snapshot `dir`.
fn save_files(
    workspace: &std::path::Path,
    disk: &std::path::Path,
    dir: &std::path::Path,
) -> Result<()> {
    for name in SNAPSHOT_FILES {
        let src = if *name == "disk.raw" {
            disk.to_path_buf()
        } else {
            workspace.join(name)
        };
        if *name == "disk.raw" || src.is_file() {
            clone_file(&src, &dir.join(name))?;
        }
    }
    Ok(())
}

/// Saves the running guest's memory and device state plus a matching copy of its disk under
/// `<workspace>/snapshots/<tag>/`. The guest is paused while both are taken, then continues if it was running.
pub async fn snapshot_save(vm: &VmRecord, tag: &str) -> Result<()> {
    let sock = vm
        .control_socket
        .as_deref()
        .context("VM has no runner control socket recorded")?;
    let dir = snapshot_dir(&vm.workspace, tag);
    if dir.exists() {
        bail!("snapshot {tag:?} already exists for this VM");
    }
    // Ask the runner whether this device set can be saved at all (`validateSaveRestoreSupport`) before touching the disk,
    // so the reason comes back first and no snapshot dir is made.
    if let Some(reason) = save_unsupported(sock).await {
        bail!("this VM cannot be saved: {reason}");
    }
    std::fs::create_dir_all(&dir)?;
    let reply = control_call_with(
        sock,
        serde_json::json!({"cmd": "save", "path": dir.join(STATE_FILE)}),
    )
    .await
    .context("runner `save`");
    let was_running = reply.as_ref().is_ok_and(|r| r.ok() && r.was_running());
    let outcome: Result<()> = (|| {
        let reply = reply?;
        if !reply.ok() {
            bail!(
                "the runner could not save the VM: {}",
                reply.error().unwrap_or("unknown error")
            );
        }
        // The guest is paused, so these files match the saved memory exactly.
        host_health::record_build(&dir);
        save_files(&vm.workspace, &vm.disk, &dir)
    })();
    if was_running {
        // Even after a failure, the guest must not be left paused.
        let _ = send(vm, "resume").await;
    }
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    outcome
}

/// The runner's `save-support` answer: `Some(reason)` when this VM's devices cannot be saved, `None` when they can or
/// when the runner is too old to say (an unknown answer never blocks a save).
async fn save_unsupported(sock: &std::path::Path) -> Option<String> {
    let reply = control_call(sock, "save-support").await.ok()?;
    save_support_reason(&reply)
}

fn save_support_reason(reply: &ControlReply) -> Option<String> {
    if !reply.ok() || reply.0.get("supported").and_then(|v| v.as_bool()) != Some(false) {
        return None;
    }
    Some(
        reply
            .0
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("Virtualization.framework gave no reason")
            .to_owned(),
    )
}

/// Tags that start with this were taken by FluxVM itself, as the safety net before a risky operation (a restore). They are
/// pruned to the newest [`AUTO_CHECKPOINT_KEEP`]; a checkpoint you name yourself is never pruned.
pub const AUTO_CHECKPOINT_PREFIX: &str = "auto-";
/// How many automatic checkpoints a VM keeps.
pub const AUTO_CHECKPOINT_KEEP: usize = 3;

/// `auto-<what>-<stamp>`, e.g. `auto-before-restore-20261123-101500`. `stamp` is a UTC `YYYYMMDD-HHMMSS`.
pub fn auto_checkpoint_tag(what: &str, stamp: &str) -> String {
    format!("{AUTO_CHECKPOINT_PREFIX}{what}-{stamp}")
}

/// Whether a checkpoint tag was made automatically.
pub fn is_auto_checkpoint(tag: &str) -> bool {
    tag.starts_with(AUTO_CHECKPOINT_PREFIX)
}

/// The tags to delete so that only the newest `keep` automatic checkpoints stay. `autos` is `(tag, modified)`; tags that are not
/// automatic are ignored, so a checkpoint you named yourself can never be chosen.
pub fn auto_checkpoints_to_prune(
    autos: &[(String, std::time::SystemTime)],
    keep: usize,
) -> Vec<String> {
    let mut v: Vec<&(String, std::time::SystemTime)> = autos
        .iter()
        .filter(|(t, _)| is_auto_checkpoint(t))
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    v.into_iter().skip(keep).map(|(t, _)| t.clone()).collect()
}

/// Deletes the oldest automatic checkpoints of this workspace beyond the newest `keep`, except those in `protect` (the one a
/// restore is about to use). Returns the tags it removed. Best effort: a directory that cannot be removed is left.
pub fn prune_auto_checkpoints(
    workspace: &std::path::Path,
    keep: usize,
    protect: &[&str],
) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(workspace.join("snapshots")) else {
        return Vec::new();
    };
    let autos: Vec<(String, std::time::SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_dir().then(|| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    m.modified().ok(),
                )
            })
        })
        .filter_map(|(t, m)| Some((t, m?)))
        .collect();
    let mut removed = Vec::new();
    for tag in auto_checkpoints_to_prune(&autos, keep) {
        if protect.contains(&tag.as_str()) {
            continue;
        }
        if std::fs::remove_dir_all(snapshot_dir(workspace, &tag)).is_ok() {
            removed.push(tag);
        }
    }
    removed
}

/// Whether snapshot `tag` of this workspace carries saved memory and device state (a disk-only snapshot does not).
pub fn snapshot_has_state(workspace: &std::path::Path, tag: &str) -> bool {
    snapshot_dir(workspace, tag).join(STATE_FILE).is_file()
}

/// Takes a disk-only snapshot under `<workspace>/snapshots/<tag>/`: an APFS clone of the disk and firmware files, no
/// memory. A running guest (`running`) is paused while the files are cloned, so the copy is crash-consistent, and then
/// resumed; a stopped or already paused guest is left as it is. Restoring it boots the guest cold from that disk.
pub async fn snapshot_save_disk(vm: &VmRecord, tag: &str, running: bool) -> Result<()> {
    let dir = snapshot_dir(&vm.workspace, tag);
    if dir.exists() {
        bail!("snapshot {tag:?} already exists for this VM");
    }
    std::fs::create_dir_all(&dir)?;
    let mut paused = false;
    if running {
        if let Err(e) = send(vm, "pause").await {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e.context("pausing the guest for a consistent disk copy"));
        }
        paused = true;
    }
    let outcome = save_files(&vm.workspace, &vm.disk, &dir);
    if paused {
        // Even after a failure, the guest must not be left paused.
        let _ = send(vm, "resume").await;
    }
    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    outcome
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn save_support_replies_decode() {
        let r = ControlReply;
        let no = serde_json::json!({"ok": true, "supported": false, "reason": "Unsupported graphics device in configuration"});
        assert_eq!(
            save_support_reason(&r(no)).as_deref(),
            Some("Unsupported graphics device in configuration")
        );
        let nameless = serde_json::json!({"ok": true, "supported": false});
        assert!(
            save_support_reason(&r(nameless))
                .unwrap()
                .contains("no reason")
        );
        assert_eq!(
            save_support_reason(&r(serde_json::json!({"ok": true, "supported": true}))),
            None
        );
        // An old runner answers `unknown command`: never block a save on that.
        assert_eq!(
            save_support_reason(&r(
                serde_json::json!({"ok": false, "error": "unknown command"})
            )),
            None
        );
    }

    #[test]
    fn automatic_checkpoints_are_pruned_oldest_first_and_named_ones_never() {
        use std::time::{Duration, SystemTime};
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let at = |s: u64| t0 + Duration::from_secs(s);
        let autos = vec![
            ("auto-before-restore-1".to_string(), at(1)),
            ("before-upgrade".to_string(), at(0)),
            ("auto-before-restore-4".to_string(), at(4)),
            ("auto-before-restore-2".to_string(), at(2)),
            ("auto-before-restore-3".to_string(), at(3)),
        ];
        assert_eq!(
            auto_checkpoints_to_prune(&autos, 3),
            vec!["auto-before-restore-1".to_string()]
        );
        assert!(auto_checkpoints_to_prune(&autos, 4).is_empty());
        assert_eq!(auto_checkpoints_to_prune(&autos, 0).len(), 4);
        assert_eq!(
            auto_checkpoint_tag("before-restore", "20261123-101500"),
            "auto-before-restore-20261123-101500"
        );
        assert!(is_auto_checkpoint("auto-x") && !is_auto_checkpoint("my-auto"));
    }

    #[test]
    fn pruning_removes_old_automatic_directories_and_spares_protected_and_named_ones() {
        let ws = tempfile::tempdir().unwrap();
        for t in ["auto-a", "auto-b", "auto-c", "auto-d", "mine"] {
            std::fs::create_dir_all(snapshot_dir(ws.path(), t)).unwrap();
            // Distinct modification times: a sleep-free way is to set them explicitly.
            let f = std::fs::File::open(snapshot_dir(ws.path(), t)).unwrap();
            let n = match t {
                "auto-a" => 1,
                "auto-b" => 2,
                "auto-c" => 3,
                "auto-d" => 4,
                _ => 0,
            };
            f.set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + n),
            )
            .unwrap();
        }
        let mut removed = prune_auto_checkpoints(ws.path(), 1, &["auto-b"]);
        // The order the removed tags come back in carries no meaning.
        removed.sort();
        assert_eq!(removed, vec!["auto-a".to_string(), "auto-c".to_string()]);
        assert!(snapshot_dir(ws.path(), "auto-b").is_dir(), "protected");
        assert!(snapshot_dir(ws.path(), "auto-d").is_dir(), "newest");
        assert!(snapshot_dir(ws.path(), "mine").is_dir(), "named");
        assert!(prune_auto_checkpoints(&ws.path().join("none"), 1, &[]).is_empty());
    }

    #[test]
    fn restore_puts_the_snapshot_disk_and_efi_back() {
        let ws = tempfile::tempdir().unwrap();
        let dir = snapshot_dir(ws.path(), "s1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STATE_FILE), b"state").unwrap();
        std::fs::write(dir.join("disk.raw"), b"disk at snapshot").unwrap();
        std::fs::write(dir.join("efi.bin"), b"efi at snapshot").unwrap();
        let disk = ws.path().join("disk.raw");
        std::fs::write(&disk, b"disk now").unwrap();
        std::fs::write(ws.path().join("efi.bin"), b"efi now").unwrap();
        restore_files(ws.path(), &disk, "s1").unwrap();
        assert_eq!(std::fs::read(&disk).unwrap(), b"disk at snapshot");
        assert_eq!(
            std::fs::read(ws.path().join("efi.bin")).unwrap(),
            b"efi at snapshot"
        );
    }

    #[test]
    fn restore_brings_back_the_asif_overlay_too() {
        let ws = tempfile::tempdir().unwrap();
        let dir = snapshot_dir(ws.path(), "s1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(STATE_FILE), b"state").unwrap();
        std::fs::write(dir.join("disk-overlay.asif"), b"overlay at snapshot").unwrap();
        std::fs::write(ws.path().join("disk-overlay.asif"), b"overlay now").unwrap();
        restore_files(ws.path(), &ws.path().join("disk.raw"), "s1").unwrap();
        assert_eq!(
            std::fs::read(ws.path().join("disk-overlay.asif")).unwrap(),
            b"overlay at snapshot"
        );
    }

    #[test]
    fn runner_balloon_reply_decodes() {
        let reply = ControlReply(serde_json::json!({
            "ok": true, "memory_mib": 8192, "target_mib": 2048, "actual_mib": 2048
        }));
        let s: AppleBalloonStatus = serde_json::from_value(reply.0).unwrap();
        assert_eq!(
            (s.memory_mib, s.target_mib, s.actual_mib),
            (8192, 2048, 2048)
        );
    }

    #[test]
    fn a_disk_only_snapshot_restores_its_disk_and_has_no_state() {
        let ws = tempfile::tempdir().unwrap();
        let dir = snapshot_dir(ws.path(), "d1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("disk.raw"), b"disk at snapshot").unwrap();
        let disk = ws.path().join("disk.raw");
        std::fs::write(&disk, b"disk now").unwrap();
        assert!(!snapshot_has_state(ws.path(), "d1"));
        restore_files(ws.path(), &disk, "d1").unwrap();
        assert_eq!(std::fs::read(&disk).unwrap(), b"disk at snapshot");
    }

    #[test]
    fn restore_refuses_a_tag_without_saved_state() {
        let ws = tempfile::tempdir().unwrap();
        let err = restore_files(ws.path(), &ws.path().join("disk.raw"), "nope").unwrap_err();
        assert!(err.to_string().contains("no saved state"), "{err}");
    }
}
