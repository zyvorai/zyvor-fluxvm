//! Data disks: qcow2 files under `<workspace>/disks/<name>.qcow2`, attached
//! as `scsi-hd` on the boot-time `scsi0` controller. The directory is the
//! source of truth, so a disk attached live is re-attached on every boot.
//!
//! An existing image or block device (a PVC's volume, an RBD map) is attached
//! as a symlink `<name>.qcow2` / `<name>.raw` pointing at it. Detach removes
//! the link only; the source belongs to whoever created it.
//!
//! A disk attached with a backing image is a qcow2 overlay in `disks/`, so
//! several VMs can start from one shared image (an imported OVA's data
//! disks) without writing to it.
//!
//! A disk attached with I/O settings ([`DiskIo`]) has a `<name>.io.json`
//! sidecar next to it. Its bus is `scsi` (default), `virtio-blk` (its own
//! PCIe device, optionally with an iothread and several queues) or
//! `vhost-user-blk` (served by a per-disk `qemu-storage-daemon` QEMU talks to
//! over `vub-<name>.sock`). A virtio disk hot-added live takes one of the
//! `hotplug-disk-N` root ports, recorded in the sidecar so it comes back on
//! the same port at the next boot and the device layout stays stable.

use crate::{QMP_TIMEOUT, qmp};
use anyhow::{Context, Result, bail};
use fluxvm_core::backend::path_arg;
use fluxvm_core::config::Config;
use fluxvm_core::model::{DiskBus, DiskIo, DiskThrottle, VmDiskInfo, VmRecord, VmStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const ROOT_DISK: &str = "root";
/// `-drive if=virtio` without an id gets the BlockBackend name `virtio0`.
const ROOT_BLOCK_DEVICE: &str = "virtio0";
/// Node name of a local qcow2 root's format node. Persistent dirty bitmaps
/// live on it, so it has to be the same on every boot. VMs started before
/// the name was set have an auto-generated one until their next boot.
pub const ROOT_NODE: &str = "root";

/// The `-drive` value for the root disk. A local qcow2 root names its
/// format node [`ROOT_NODE`]; the device and BlockBackend (`virtio0`) are
/// what they always were, so the guest sees no change. Raw and NBD roots
/// can't hold persistent bitmaps and keep an auto-generated node name.
pub fn root_drive(disk: &Path, disk_format: &str, nbd_export: Option<&Path>) -> String {
    match nbd_export {
        Some(socket) => format!("file=nbd:unix:{},if=virtio,format=raw", socket.display()),
        // writeback (not cache=none) so QMP `savevm` / internal snapshots work on
        // CoW overlays — O_DIRECT hangs human-monitor-command savevm indefinitely.
        None if disk_format == "qcow2" => format!(
            "file={},if=virtio,format=qcow2,cache=writeback,node-name={ROOT_NODE}",
            path_arg(disk)
        ),
        None => format!(
            "file={},if=virtio,format={disk_format},cache=writeback",
            path_arg(disk)
        ),
    }
}

/// The root disk's QEMU arguments. Without `io` this is the plain
/// [`root_drive`] (`-drive if=virtio`) every VM has always had. With it the
/// drive is split into `-drive if=none,id=virtio0` and an explicit
/// `virtio-blk-pci`, so the BlockBackend keeps its `virtio0` name (resize,
/// backups) while the device gets an iothread and queues.
pub fn root_args(
    disk: &Path,
    disk_format: &str,
    nbd_export: Option<&Path>,
    io: Option<&DiskIo>,
) -> Result<Vec<String>> {
    if let Some(io) = io {
        io.validate(true).map_err(anyhow::Error::msg)?;
    }
    let Some(io) = io.filter(|io| io.has_device_settings()) else {
        return Ok(vec![
            "-drive".into(),
            root_drive(disk, disk_format, nbd_export),
        ]);
    };
    let mut a = Vec::new();
    let mut drive = match nbd_export {
        Some(socket) => {
            if io.aio.is_some() {
                bail!("root_disk_io.aio does not apply to an NBD root");
            }
            format!(
                "file=nbd:unix:{},if=none,id={ROOT_BLOCK_DEVICE},format=raw",
                socket.display()
            )
        }
        None => format!(
            "file={},if=none,id={ROOT_BLOCK_DEVICE},format={disk_format}",
            path_arg(disk)
        ),
    };
    drive.push_str(&format!(",cache={}", io.cache().as_str()));
    if let Some(aio) = io.aio {
        drive.push_str(&format!(",aio={}", aio.as_str()));
    }
    if nbd_export.is_none() && disk_format == "qcow2" {
        drive.push_str(&format!(",node-name={ROOT_NODE}"));
    }
    let mut dev = format!("virtio-blk-pci,drive={ROOT_BLOCK_DEVICE},id=disk-root,bus=pcie.0");
    if io.iothread {
        a.extend(["-object".into(), "iothread,id=io-root".into()]);
        dev.push_str(",iothread=io-root");
    }
    if let Some(q) = io.queues {
        dev.push_str(&format!(",num-queues={q}"));
    }
    a.extend(["-drive".into(), drive, "-device".into(), dev]);
    Ok(a)
}

/// True when [`root_args`] gives the root an explicit `disk-root` device.
pub fn root_is_split(io: Option<&DiskIo>) -> bool {
    io.is_some_and(DiskIo::has_device_settings)
}

/// `block_set_io_throttle` arguments for `target` (`{"id": ..}` or
/// `{"device": ..}`); `None` lifts every limit.
fn throttle_args(target: serde_json::Value, t: Option<&DiskThrottle>) -> serde_json::Value {
    let t = t.cloned().unwrap_or_default();
    let v = |x: Option<u64>| x.unwrap_or(0);
    let mut a = json!({
        "iops": v(t.iops_total),
        "iops_rd": v(t.iops_read),
        "iops_wr": v(t.iops_write),
        "bps": v(t.bps_total),
        "bps_rd": v(t.bps_read),
        "bps_wr": v(t.bps_write),
        "iops_max": v(t.iops_total_max),
        "bps_max": v(t.bps_total_max),
    });
    if let Some(s) = t.burst_seconds {
        if t.iops_total_max.is_some() {
            a["iops_max_length"] = json!(s);
        }
        if t.bps_total_max.is_some() {
            a["bps_max_length"] = json!(s);
        }
    }
    if let (Some(m), Some(o)) = (a.as_object_mut(), target.as_object()) {
        m.extend(o.clone());
    }
    a
}

/// The `block_set_io_throttle` target of the root: its `disk-root` device
/// when split, else the `virtio0` BlockBackend.
fn root_throttle_target(split: bool) -> serde_json::Value {
    if split {
        json!({"id": "disk-root"})
    } else {
        json!({"device": ROOT_BLOCK_DEVICE})
    }
}

/// `(target, throttle)` for every disk of a just-launched VM that has limits.
fn boot_throttles(
    workspace: &Path,
    root_io: Option<&DiskIo>,
) -> Vec<(serde_json::Value, DiskThrottle)> {
    let mut out = Vec::new();
    if let Some(t) = root_io.and_then(DiskIo::throttle) {
        out.push((root_throttle_target(root_is_split(root_io)), t.clone()));
    }
    for (name, _) in data_disks(workspace) {
        let io = disk_io(workspace, &name);
        if io.bus() == DiskBus::VhostUserBlk {
            continue;
        }
        if let Some(t) = io.throttle() {
            out.push((json!({"id": device_id(&name)}), t.clone()));
        }
    }
    out
}

/// Applies every configured disk throttle to a QEMU that was just launched.
/// QEMU keeps throttles out of the migration stream, so this also runs on a
/// migration target.
pub async fn apply_boot_throttles(
    workspace: &Path,
    root_io: Option<&DiskIo>,
    timeout: std::time::Duration,
) -> Result<()> {
    let sock = workspace.join("qmp.sock");
    for (target, t) in boot_throttles(workspace, root_io) {
        let what = target.to_string();
        qmp::execute(
            &sock,
            "block_set_io_throttle",
            Some(throttle_args(target, Some(&t))),
            timeout,
        )
        .await
        .with_context(|| format!("block_set_io_throttle {what}"))?;
    }
    Ok(())
}

/// What this host's kernel and QEMU install support for disk I/O.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostIoSupport {
    pub io_uring: bool,
    pub vhost_user_blk: bool,
    pub nvme_multipath: bool,
}

pub fn host_io_support(cfg: &Config) -> HostIoSupport {
    if !cfg!(target_os = "linux") {
        return HostIoSupport::default();
    }
    let read = |p: &str| std::fs::read_to_string(p).map(|s| s.trim().to_string());
    HostIoSupport {
        // 0 allows everyone, 1 only CAP_SYS_ADMIN (the daemon), 2 nobody.
        io_uring: read("/proc/sys/kernel/io_uring_disabled").map_or(true, |v| v != "2"),
        vhost_user_blk: binary_on_path(&cfg.qemu_storage_daemon_binary),
        nvme_multipath: read("/sys/module/nvme_core/parameters/multipath").is_ok_and(|v| v == "Y"),
    }
}

fn binary_on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

pub fn disks_dir(workspace: &Path) -> PathBuf {
    workspace.join("disks")
}

/// Root port id prefix for hot-added virtio data disks (`hotplug-disk-0..N`).
pub const DISK_PORT_PREFIX: &str = "hotplug-disk-";
/// Root ports a new QEMU VM reserves for virtio data disk hotplug.
pub const DEFAULT_DISK_HOTPLUG_PORTS: u8 = 4;
/// Most root ports a VM can reserve for virtio data disk hotplug.
pub const MAX_DISK_HOTPLUG_PORTS: u8 = 16;

/// What `disks/<name>.io.json` holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Sidecar {
    #[serde(default)]
    io: DiskIo,
    /// `hotplug-disk-N` the disk was hot-added on; `None` for a disk present
    /// at boot, which sits on `pcie.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    port: Option<u8>,
}

fn sidecar_path(workspace: &Path, name: &str) -> PathBuf {
    disks_dir(workspace).join(format!("{name}.io.json"))
}

fn read_sidecar(workspace: &Path, name: &str) -> Sidecar {
    std::fs::read(sidecar_path(workspace, name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_sidecar(workspace: &Path, name: &str, sc: &Sidecar) -> Result<()> {
    let path = sidecar_path(workspace, name);
    if *sc == Sidecar::default() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(sc)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

/// The I/O settings data disk `name` was attached with (defaults if none).
pub fn disk_io(workspace: &Path, name: &str) -> DiskIo {
    read_sidecar(workspace, name).io
}

/// Record `io` for data disk `name` before its first boot.
pub fn set_disk_io(workspace: &Path, name: &str, io: &DiskIo) -> Result<()> {
    io.validate(false).map_err(anyhow::Error::msg)?;
    write_sidecar(
        workspace,
        name,
        &Sidecar {
            io: io.clone(),
            port: None,
        },
    )
}

/// Copy `name`'s I/O settings to another workspace (a clone). The port is
/// not copied: the clone boots the disk on `pcie.0`.
pub fn copy_disk_io(src: &Path, dst: &Path, name: &str) -> Result<()> {
    let io = disk_io(src, name);
    write_sidecar(dst, name, &Sidecar { io, port: None })
}

fn bus_name(bus: DiskBus) -> &'static str {
    match bus {
        DiskBus::Scsi => "scsi",
        DiskBus::VirtioBlk => "virtio-blk",
        DiskBus::VhostUserBlk => "vhost-user-blk",
    }
}

/// True when data disk `name` is served by a `qemu-storage-daemon`.
pub fn is_vhost_user(workspace: &Path, name: &str) -> bool {
    disk_io(workspace, name).bus() == DiskBus::VhostUserBlk
}

/// True when any data disk is vhost-user-blk, which needs shareable guest
/// memory.
pub fn has_vhost_user(workspace: &Path) -> bool {
    data_disks(workspace)
        .iter()
        .any(|(n, _)| is_vhost_user(workspace, n))
}

fn vhost_user_socket(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(format!("vub-{name}.sock"))
}

fn vhost_user_pidfile(workspace: &Path, name: &str) -> PathBuf {
    workspace.join(format!("vub-{name}.pid"))
}

/// The `qemu-storage-daemon` pid serving data disk `name`, if one is still
/// running. A pidfile whose pid now belongs to another process (the daemon
/// exited and the pid was reused) yields `None`.
pub fn vhost_user_pid(workspace: &Path, name: &str) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(vhost_user_pidfile(workspace, name))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let socket = vhost_user_socket(workspace, name);
    let needle = socket.to_string_lossy();
    String::from_utf8_lossy(&cmdline)
        .contains(needle.as_ref())
        .then_some(pid)
}

fn port_id(port: u8) -> String {
    format!("{DISK_PORT_PREFIX}{port}")
}

/// Lowest `hotplug-disk-N` (below `ports`) no data disk holds.
fn free_port(workspace: &Path, ports: u8) -> Option<u8> {
    let used: Vec<u8> = data_disks(workspace)
        .iter()
        .filter_map(|(n, _)| read_sidecar(workspace, n).port)
        .collect();
    (0..ports.min(MAX_DISK_HOTPLUG_PORTS)).find(|p| !used.contains(p))
}

/// Boot-time root ports for hot-added virtio data disks. `first_chassis` and
/// `first_slot` follow the ports the VM already has.
pub fn disk_port_args(ports: u8, first_chassis: u8, first_slot: u8) -> Vec<String> {
    (0..ports.min(MAX_DISK_HOTPLUG_PORTS))
        .flat_map(|i| {
            [
                "-device".to_string(),
                format!(
                    "pcie-root-port,id={},bus=pcie.0,chassis={},slot={}",
                    port_id(i),
                    first_chassis + i,
                    first_slot + i
                ),
            ]
        })
        .collect()
}

pub fn validate_disk_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 32
        || name == ROOT_DISK
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || name.starts_with('-')
    {
        bail!("invalid disk name {name:?}: use 1-32 of [a-z0-9-], not 'root'");
    }
    Ok(())
}

/// Sorted `(name, path)` of every data disk in `workspace`.
pub fn data_disks(workspace: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(disks_dir(workspace)) else {
        return vec![];
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "qcow2" || x == "raw"))
        .filter_map(|p| {
            let name = p.file_stem()?.to_str()?.to_string();
            validate_disk_name(&name).ok()?;
            Some((name, p))
        })
        .collect();
    out.sort();
    out
}

/// The data disk named `name`, whichever format it was attached as.
fn find_disk(workspace: &Path, name: &str) -> Option<PathBuf> {
    data_disks(workspace)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, p)| p)
}

/// `qcow2` or `raw`, from the file extension.
fn disk_format(path: &Path) -> &'static str {
    if path.extension().is_some_and(|x| x == "raw") {
        "raw"
    } else {
        "qcow2"
    }
}

/// True for a disk attached from an existing image (a symlink in `disks/`).
pub fn is_external(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

fn is_block_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path).is_ok_and(|m| m.file_type().is_block_device())
}

/// The `file` protocol driver for `path`: `host_device` for a block device.
fn file_driver(path: &Path) -> &'static str {
    if is_block_device(path) {
        "host_device"
    } else {
        "file"
    }
}

/// The QEMU block node of data disk `name`.
pub fn node_name(name: &str) -> String {
    format!("data-{name}")
}

fn device_id(name: &str) -> String {
    format!("disk-{name}")
}

/// Boot-time `-blockdev`/`-device` pairs for install media: `ide-cd` on the
/// q35 AHCI ports `ide.0..`, read-only raw. No `bootindex`, so firmware tries
/// the (blank) root disk, falls through to the CD, and later honors the boot
/// entry the installed OS wrote. An ejected entry keeps an empty drive.
pub fn cdrom_args(cdroms: &[fluxvm_core::model::CdromSpec]) -> Vec<String> {
    let mut a = Vec::new();
    for (i, c) in cdroms.iter().enumerate() {
        if c.is_ejected() {
            a.extend([
                "-device".into(),
                format!("ide-cd,bus=ide.{i},id=cdrom-{}", c.name),
            ]);
            continue;
        }
        a.extend([
            "-blockdev".into(),
            format!(
                "driver=raw,node-name=cd-{},read-only=on,file.driver=file,file.filename={},file.read-only=on",
                c.name,
                path_arg(&c.path)
            ),
            "-device".into(),
            format!("ide-cd,bus=ide.{i},drive=cd-{},id=cdrom-{}", c.name, c.name),
        ]);
    }
    a
}

/// Eject a live VM's install medium: open the tray (`force`, so a guest
/// lock can't block it), drop the medium, close the tray, then free the
/// ISO's node. The empty `ide-cd` device stays.
pub async fn eject_cdrom(vm: &VmRecord, name: &str) -> Result<()> {
    let sock = vm.workspace.join("qmp.sock");
    let id = format!("cdrom-{name}");
    qmp::execute(
        &sock,
        "blockdev-open-tray",
        Some(json!({"id": id, "force": true})),
        QMP_TIMEOUT,
    )
    .await
    .context("blockdev-open-tray")?;
    qmp::execute(
        &sock,
        "blockdev-remove-medium",
        Some(json!({"id": id})),
        QMP_TIMEOUT,
    )
    .await
    .context("blockdev-remove-medium")?;
    qmp::execute(
        &sock,
        "blockdev-close-tray",
        Some(json!({"id": id})),
        QMP_TIMEOUT,
    )
    .await
    .context("blockdev-close-tray")?;
    // A node left behind only keeps the ISO open until the next restart.
    let _ = qmp::execute(
        &sock,
        "blockdev-del",
        Some(json!({"node-name": format!("cd-{name}")})),
        QMP_TIMEOUT,
    )
    .await;
    Ok(())
}

pub fn is_live_vm(vm: &VmRecord) -> bool {
    is_live(vm)
}

fn iothread_id(name: &str) -> String {
    format!("io-{name}")
}

fn chardev_id(name: &str) -> String {
    format!("vub-{name}")
}

/// `-blockdev` value for a data disk opened by QEMU itself.
fn blockdev_arg(name: &str, path: &Path, io: &DiskIo) -> String {
    let cache = io.cache();
    let mut s = format!(
        "driver={},node-name={},cache.direct={}",
        disk_format(path),
        node_name(name),
        if cache.direct() { "on" } else { "off" }
    );
    if cache == fluxvm_core::model::DiskCache::Unsafe {
        s.push_str(",cache.no-flush=on");
    }
    s.push_str(&format!(
        ",file.driver={},file.filename={}",
        file_driver(path),
        path_arg(path)
    ));
    if let Some(aio) = io.aio {
        s.push_str(&format!(",file.aio={}", aio.as_str()));
    }
    s
}

/// `,write-cache=` for a disk whose cache mode was chosen explicitly.
fn write_cache_prop(io: &DiskIo) -> String {
    match io.cache {
        Some(c) => format!(
            ",write-cache={}",
            if c.guest_write_cache() { "on" } else { "off" }
        ),
        None => String::new(),
    }
}

fn virtio_device(name: &str, io: &DiskIo, port: Option<u8>) -> String {
    let bus = port.map(port_id).unwrap_or_else(|| "pcie.0".into());
    let mut d = match io.bus() {
        DiskBus::VhostUserBlk => format!(
            "vhost-user-blk-pci,chardev={},id={},bus={bus}",
            chardev_id(name),
            device_id(name)
        ),
        _ => format!(
            "virtio-blk-pci,drive={},id={},serial={name},bus={bus}{}",
            node_name(name),
            device_id(name),
            write_cache_prop(io)
        ),
    };
    if io.iothread && io.bus() == DiskBus::VirtioBlk {
        d.push_str(&format!(",iothread={}", iothread_id(name)));
    }
    if let Some(q) = io.queues {
        d.push_str(&format!(",num-queues={q}"));
    }
    d
}

/// Boot-time arguments for every data disk. vhost-user-blk disks expect
/// their `qemu-storage-daemon` to be listening already
/// ([`spawn_vhost_user_all`]).
pub fn boot_args(workspace: &Path) -> Vec<String> {
    let mut a = Vec::new();
    for (name, path) in data_disks(workspace) {
        let sc = read_sidecar(workspace, &name);
        let io = &sc.io;
        match io.bus() {
            DiskBus::Scsi => a.extend([
                "-blockdev".into(),
                blockdev_arg(&name, &path, io),
                "-device".into(),
                format!(
                    "scsi-hd,bus=scsi0.0,drive={},id={},serial={name}{}",
                    node_name(&name),
                    device_id(&name),
                    write_cache_prop(io)
                ),
            ]),
            DiskBus::VirtioBlk => {
                if io.iothread {
                    a.extend([
                        "-object".into(),
                        format!("iothread,id={}", iothread_id(&name)),
                    ]);
                }
                a.extend([
                    "-blockdev".into(),
                    blockdev_arg(&name, &path, io),
                    "-device".into(),
                    virtio_device(&name, io, sc.port),
                ]);
            }
            DiskBus::VhostUserBlk => a.extend([
                "-chardev".into(),
                format!(
                    "socket,id={},path={}",
                    chardev_id(&name),
                    path_arg(&vhost_user_socket(workspace, &name))
                ),
                "-device".into(),
                virtio_device(&name, io, sc.port),
            ]),
        }
    }
    a
}

/// `qemu-storage-daemon` arguments exporting `path` as vhost-user-blk on
/// `socket`.
fn vhost_user_args(name: &str, path: &Path, io: &DiskIo, socket: &Path) -> Vec<String> {
    let cache = io.cache();
    let mut file = format!(
        "driver={},node-name=file-{name},filename={},cache.direct={}",
        file_driver(path),
        path_arg(path),
        if cache.direct() { "on" } else { "off" }
    );
    if let Some(aio) = io.aio {
        file.push_str(&format!(",aio={}", aio.as_str()));
    }
    let mut fmt = format!(
        "driver={},node-name={},file=file-{name}",
        disk_format(path),
        node_name(name)
    );
    if cache == fluxvm_core::model::DiskCache::Unsafe {
        fmt.push_str(",cache.no-flush=on");
    }
    let mut a = vec!["--blockdev".into(), file, "--blockdev".into(), fmt];
    let mut export = format!(
        "type=vhost-user-blk,id=exp-{name},node-name={},addr.type=unix,addr.path={},writable=on",
        node_name(name),
        path_arg(socket)
    );
    if !cache.guest_write_cache() {
        export.push_str(",writethrough=on");
    }
    if let Some(q) = io.queues {
        export.push_str(&format!(",num-queues={q}"));
    }
    if io.iothread {
        a.extend([
            "--object".into(),
            format!("iothread,id={}", iothread_id(name)),
        ]);
        export.push_str(&format!(",iothread={}", iothread_id(name)));
    }
    a.extend(["--export".into(), export]);
    a
}

/// Start the `qemu-storage-daemon` serving data disk `name` and wait for its
/// socket. Its pid is written to `vub-<name>.pid`.
pub async fn spawn_vhost_user(cfg: &Config, workspace: &Path, name: &str) -> Result<u32> {
    let Some(path) = find_disk(workspace, name) else {
        bail!("disk {name:?} not found");
    };
    let io = disk_io(workspace, name);
    let socket = vhost_user_socket(workspace, name);
    // A daemon left by an earlier boot whose QEMU died holds the disk open;
    // one daemon per disk, ever.
    stop_vhost_user(workspace, name).await;
    let log = workspace.join(format!("vub-{name}.log"));
    let args = vhost_user_args(name, &path, &io, &socket);
    let child = fluxvm_core::process::spawn_logged(&cfg.qemu_storage_daemon_binary, &args, &log)
        .await
        .with_context(|| format!("starting qemu-storage-daemon for disk {name:?}"))?;
    let Some(pid) = child.id() else {
        bail!("qemu-storage-daemon for disk {name:?} exited before its pid was known");
    };
    std::fs::write(vhost_user_pidfile(workspace, name), pid.to_string())
        .context("writing the qemu-storage-daemon pidfile")?;
    if let Err(e) = fluxvm_core::process::wait_for_socket_ready(
        pid,
        &socket,
        std::time::Duration::from_secs(10),
        &format!("qemu-storage-daemon for disk {name:?}"),
        &log,
    )
    .await
    {
        stop_vhost_user(workspace, name).await;
        return Err(e);
    }
    Ok(pid)
}

/// Start a `qemu-storage-daemon` for every vhost-user-blk data disk. On
/// failure the ones already started are stopped.
pub async fn spawn_vhost_user_all(cfg: &Config, workspace: &Path) -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    for (name, _) in data_disks(workspace) {
        if !is_vhost_user(workspace, &name) {
            continue;
        }
        match spawn_vhost_user(cfg, workspace, &name).await {
            Ok(pid) => pids.push(pid),
            Err(e) => {
                for (n, _) in data_disks(workspace) {
                    if is_vhost_user(workspace, &n) {
                        stop_vhost_user(workspace, &n).await;
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(pids)
}

/// Stop the `qemu-storage-daemon` for data disk `name` (SIGTERM, which
/// flushes, then SIGKILL) and remove its socket and pidfile.
pub async fn stop_vhost_user(workspace: &Path, name: &str) {
    if let Some(pid) = vhost_user_pid(workspace, name) {
        let _ = fluxvm_core::process::terminate_pid(pid).await;
    }
    let _ = std::fs::remove_file(vhost_user_pidfile(workspace, name));
    let _ = std::fs::remove_file(vhost_user_socket(workspace, name));
}

async fn qemu_img(cfg: &Config, args: &[&str], path: &Path) -> Result<Vec<u8>> {
    let out = tokio::process::Command::new(&cfg.qemu_img_binary)
        .args(args)
        .arg(path)
        .output()
        .await
        .with_context(|| format!("running {}", cfg.qemu_img_binary))?;
    if !out.status.success() {
        bail!(
            "qemu-img {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

async fn virtual_size(cfg: &Config, path: &Path) -> Result<(u64, String)> {
    let out = qemu_img(cfg, &["info", "-U", "--output=json"], path).await?;
    let v: serde_json::Value = serde_json::from_slice(&out).context("parsing qemu-img info")?;
    Ok((
        v.get("virtual-size").and_then(|s| s.as_u64()).unwrap_or(0),
        v.get("format")
            .and_then(|s| s.as_str())
            .unwrap_or("raw")
            .to_string(),
    ))
}

async fn disk_info(
    cfg: &Config,
    name: &str,
    path: &Path,
    bus: &str,
    throttle: Option<&DiskThrottle>,
) -> Result<VmDiskInfo> {
    let (size_bytes, format) = virtual_size(cfg, path).await?;
    let allocated_bytes = std::fs::metadata(path)
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.blocks() * 512
        })
        .unwrap_or(0);
    Ok(VmDiskInfo {
        name: name.to_string(),
        path: path.to_path_buf(),
        bus: bus.to_string(),
        format,
        size_bytes,
        allocated_bytes,
        throttle: throttle.cloned(),
    })
}

fn root_throttle(vm: &VmRecord) -> Option<&DiskThrottle> {
    vm.request.root_disk_io.as_ref().and_then(DiskIo::throttle)
}

pub async fn list(cfg: &Config, vm: &VmRecord) -> Result<Vec<VmDiskInfo>> {
    let mut out = vec![disk_info(cfg, ROOT_DISK, &vm.disk, "virtio", root_throttle(vm)).await?];
    for (name, path) in data_disks(&vm.workspace) {
        let io = disk_io(&vm.workspace, &name);
        out.push(disk_info(cfg, &name, &path, bus_name(io.bus()), io.throttle()).await?);
    }
    Ok(out)
}

/// Changes the limits of `root` or a data disk. The new setting is returned
/// for the caller to store: a data disk's in its sidecar here, the root's in
/// the VM record (`request.root_disk_io`) by the caller. A live VM gets it
/// at once; otherwise it applies at the next start.
pub async fn set_throttle(
    vm: &VmRecord,
    name: &str,
    throttle: Option<DiskThrottle>,
) -> Result<Option<DiskThrottle>> {
    let throttle = throttle.filter(|t| !t.is_empty());
    if let Some(t) = &throttle {
        t.validate().map_err(anyhow::Error::msg)?;
    }
    let target = if name == ROOT_DISK {
        root_throttle_target(root_is_split(vm.request.root_disk_io.as_ref()))
    } else {
        validate_disk_name(name)?;
        if find_disk(&vm.workspace, name).is_none() {
            bail!("disk {name:?} not found");
        }
        let mut sc = read_sidecar(&vm.workspace, name);
        if sc.io.bus() == DiskBus::VhostUserBlk {
            bail!(
                "disk {name:?} is vhost-user-blk, which bypasses QEMU's block layer; it cannot be throttled"
            );
        }
        sc.io.throttle = throttle.clone();
        sc.io.validate(false).map_err(anyhow::Error::msg)?;
        if is_live(vm) {
            apply_throttle(vm, json!({"id": device_id(name)}), throttle.as_ref()).await?;
        }
        write_sidecar(&vm.workspace, name, &sc)?;
        return Ok(throttle);
    };
    if is_live(vm) {
        apply_throttle(vm, target, throttle.as_ref()).await?;
    }
    Ok(throttle)
}

async fn apply_throttle(
    vm: &VmRecord,
    target: serde_json::Value,
    t: Option<&DiskThrottle>,
) -> Result<()> {
    qmp::execute(
        &vm.workspace.join("qmp.sock"),
        "block_set_io_throttle",
        Some(throttle_args(target, t)),
        QMP_TIMEOUT,
    )
    .await
    .context("block_set_io_throttle")?;
    Ok(())
}

fn is_live(vm: &VmRecord) -> bool {
    matches!(vm.status, VmStatus::Running | VmStatus::Paused)
}

/// Create a new qcow2 data disk and, when the VM is live, hot-add it.
pub async fn attach(
    cfg: &Config,
    vm: &VmRecord,
    name: &str,
    size_gib: u64,
    io: &DiskIo,
) -> Result<VmDiskInfo> {
    validate_disk_name(name)?;
    io.validate(false).map_err(anyhow::Error::msg)?;
    if size_gib == 0 || size_gib > 16 * 1024 {
        bail!("disk size must be 1..=16384 GiB");
    }
    let dir = disks_dir(&vm.workspace);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    if find_disk(&vm.workspace, name).is_some() {
        bail!("disk {name:?} already exists");
    }
    let path = dir.join(format!("{name}.qcow2"));
    let out = tokio::process::Command::new(&cfg.qemu_img_binary)
        .args(["create", "-q", "-f", "qcow2"])
        .arg(&path)
        .arg(format!("{size_gib}G"))
        .output()
        .await
        .with_context(|| format!("running {}", cfg.qemu_img_binary))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        bail!(
            "qemu-img create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    if let Err(e) = add(cfg, vm, name, &path, io).await {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    disk_info(cfg, name, &path, bus_name(io.bus()), io.throttle()).await
}

/// Attach an existing qcow2/raw image or block device as data disk `name`
/// (hot-adding it when the VM is live). The source is never modified by
/// detach; QEMU's image locking refuses a source another VM has open.
pub async fn attach_existing(
    cfg: &Config,
    vm: &VmRecord,
    name: &str,
    source: &Path,
    io: &DiskIo,
) -> Result<VmDiskInfo> {
    validate_disk_name(name)?;
    io.validate(false).map_err(anyhow::Error::msg)?;
    if !source.is_absolute() {
        bail!("disk source {} must be an absolute path", source.display());
    }
    let meta =
        std::fs::metadata(source).with_context(|| format!("disk source {}", source.display()))?;
    let format = if is_block_device(source) {
        "raw".to_string()
    } else if meta.is_file() {
        virtual_size(cfg, source).await?.1
    } else {
        bail!(
            "disk source {} is neither a file nor a block device",
            source.display()
        );
    };
    if format != "raw" && format != "qcow2" {
        bail!("disk source format {format:?} is not supported (raw or qcow2)");
    }
    let real = std::fs::canonicalize(source)?;
    if real.starts_with(std::fs::canonicalize(&vm.workspace)?) {
        bail!("disk source must live outside the VM workspace");
    }
    if find_disk(&vm.workspace, name).is_some() {
        bail!("disk {name:?} already exists");
    }
    let dir = disks_dir(&vm.workspace);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let link = dir.join(format!("{name}.{format}"));
    std::os::unix::fs::symlink(&real, &link)
        .with_context(|| format!("linking {} -> {}", link.display(), real.display()))?;
    if let Err(e) = add(cfg, vm, name, &link, io).await {
        let _ = std::fs::remove_file(&link);
        return Err(e);
    }
    disk_info(cfg, name, &link, bus_name(io.bus()), io.throttle()).await
}

/// Create data disk `name` as a qcow2 overlay on top of `backing` (a qcow2
/// or raw file or block device, never written to) and, when the VM is
/// live, hot-add it. Detach deletes the overlay only.
pub async fn attach_overlay(
    cfg: &Config,
    vm: &VmRecord,
    name: &str,
    backing: &Path,
    io: &DiskIo,
) -> Result<VmDiskInfo> {
    io.validate(false).map_err(anyhow::Error::msg)?;
    let path = create_overlay(cfg, &vm.workspace, name, backing).await?;
    if let Err(e) = add(cfg, vm, name, &path, io).await {
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    disk_info(cfg, name, &path, bus_name(io.bus()), io.throttle()).await
}

/// Create `disks/<name>.qcow2` in `workspace` as an overlay on `backing`.
/// Picked up as a data disk at the next boot.
pub async fn create_overlay(
    cfg: &Config,
    workspace: &Path,
    name: &str,
    backing: &Path,
) -> Result<PathBuf> {
    validate_disk_name(name)?;
    if !backing.is_absolute() {
        bail!(
            "backing image {} must be an absolute path",
            backing.display()
        );
    }
    let format = if is_block_device(backing) {
        "raw".to_string()
    } else if std::fs::metadata(backing)
        .with_context(|| format!("backing image {}", backing.display()))?
        .is_file()
    {
        virtual_size(cfg, backing).await?.1
    } else {
        bail!(
            "backing image {} is neither a file nor a block device",
            backing.display()
        );
    };
    if format != "raw" && format != "qcow2" {
        bail!("backing image format {format:?} is not supported (raw or qcow2)");
    }
    if find_disk(workspace, name).is_some() {
        bail!("disk {name:?} already exists");
    }
    let dir = disks_dir(workspace);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{name}.qcow2"));
    let out = tokio::process::Command::new(&cfg.qemu_img_binary)
        .args(["create", "-q", "-f", "qcow2", "-F", &format, "-b"])
        .arg(backing)
        .arg(&path)
        .output()
        .await
        .with_context(|| format!("running {}", cfg.qemu_img_binary))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        bail!(
            "qemu-img create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(path)
}

/// Record `io` for a newly placed disk file and, when the VM is live,
/// hot-add it. On error the sidecar is gone again; the caller removes the
/// file.
async fn add(cfg: &Config, vm: &VmRecord, name: &str, path: &Path, io: &DiskIo) -> Result<()> {
    let mut sc = Sidecar {
        io: io.clone(),
        port: None,
    };
    if is_live(vm) && io.bus() != DiskBus::Scsi {
        let ports = vm.request.disk_hotplug_ports.unwrap_or(0);
        let Some(port) = free_port(&vm.workspace, ports) else {
            bail!(
                "no free hotplug-disk root port ({ports} reserved at create); attach {} disks while the VM is stopped",
                bus_name(io.bus())
            );
        };
        sc.port = Some(port);
    }
    write_sidecar(&vm.workspace, name, &sc)?;
    if is_live(vm)
        && let Err(e) = hot_add(cfg, vm, name, path, &sc).await
    {
        let _ = std::fs::remove_file(sidecar_path(&vm.workspace, name));
        return Err(e);
    }
    Ok(())
}

fn blockdev_add_args(name: &str, path: &Path, io: &DiskIo) -> serde_json::Value {
    let cache = io.cache();
    let mut file = json!({"driver": file_driver(path), "filename": path.to_string_lossy()});
    if let Some(aio) = io.aio {
        file["aio"] = json!(aio.as_str());
    }
    json!({
        "driver": disk_format(path),
        "node-name": node_name(name),
        "cache": {
            "direct": cache.direct(),
            "no-flush": cache == fluxvm_core::model::DiskCache::Unsafe,
        },
        "file": file,
    })
}

/// True when the running QEMU's guest RAM is shareable (memfd or file with
/// `share=on`), which vhost-user devices need.
async fn has_shared_memory(sock: &Path) -> bool {
    qmp::execute(sock, "query-memdev", None, QMP_TIMEOUT)
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .is_some_and(|m| {
            m.iter()
                .any(|d| d.get("share").and_then(|s| s.as_bool()) == Some(true))
        })
}

async fn hot_add(cfg: &Config, vm: &VmRecord, name: &str, path: &Path, sc: &Sidecar) -> Result<()> {
    let sock = vm.workspace.join("qmp.sock");
    let io = &sc.io;
    let bus = sc.port.map(port_id);
    if io.bus() == DiskBus::VhostUserBlk {
        if !has_shared_memory(&sock).await {
            bail!(
                "the VM booted without shareable memory, which vhost-user-blk needs; attach the disk while it is stopped, or create it with shared_memory"
            );
        }
        spawn_vhost_user(cfg, &vm.workspace, name).await?;
        let socket = vhost_user_socket(&vm.workspace, name);
        let added = qmp::execute(
            &sock,
            "chardev-add",
            Some(json!({
                "id": chardev_id(name),
                "backend": {"type": "socket", "data": {
                    "addr": {"type": "unix", "data": {"path": socket.to_string_lossy()}},
                    "server": false,
                }},
            })),
            QMP_TIMEOUT,
        )
        .await
        .context("chardev-add");
        if let Err(e) = added {
            stop_vhost_user(&vm.workspace, name).await;
            return Err(e);
        }
        let mut dev = json!({
            "driver": "vhost-user-blk-pci",
            "chardev": chardev_id(name),
            "id": device_id(name),
            "bus": bus,
        });
        if let Some(q) = io.queues {
            dev["num-queues"] = json!(q);
        }
        if let Err(e) = qmp::execute(&sock, "device_add", Some(dev), QMP_TIMEOUT).await {
            let _ = qmp::execute(
                &sock,
                "chardev-remove",
                Some(json!({"id": chardev_id(name)})),
                QMP_TIMEOUT,
            )
            .await;
            stop_vhost_user(&vm.workspace, name).await;
            return Err(e).context("device_add vhost-user-blk-pci");
        }
        return Ok(());
    }
    if io.iothread {
        qmp::execute(
            &sock,
            "object-add",
            Some(json!({"qom-type": "iothread", "id": iothread_id(name)})),
            QMP_TIMEOUT,
        )
        .await
        .context("object-add iothread")?;
    }
    if let Err(e) = qmp::execute(
        &sock,
        "blockdev-add",
        Some(blockdev_add_args(name, path, io)),
        QMP_TIMEOUT,
    )
    .await
    {
        del_iothread(&sock, name, io).await;
        return Err(e).context("blockdev-add");
    }
    let mut dev = match io.bus() {
        DiskBus::Scsi => json!({
            "driver": "scsi-hd",
            "bus": "scsi0.0",
            "drive": node_name(name),
            "id": device_id(name),
            "serial": name,
        }),
        _ => {
            let mut d = json!({
                "driver": "virtio-blk-pci",
                "drive": node_name(name),
                "id": device_id(name),
                "serial": name,
                "bus": bus,
            });
            if io.iothread {
                d["iothread"] = json!(iothread_id(name));
            }
            if let Some(q) = io.queues {
                d["num-queues"] = json!(q);
            }
            d
        }
    };
    if let Some(c) = io.cache {
        dev["write-cache"] = json!(if c.guest_write_cache() { "on" } else { "off" });
    }
    if let Err(e) = qmp::execute(&sock, "device_add", Some(dev), QMP_TIMEOUT).await {
        let _ = qmp::execute(
            &sock,
            "blockdev-del",
            Some(json!({"node-name": node_name(name)})),
            QMP_TIMEOUT,
        )
        .await;
        del_iothread(&sock, name, io).await;
        return Err(e).context("device_add");
    }
    if let Some(t) = io.throttle()
        && let Err(e) = apply_throttle(vm, json!({"id": device_id(name)}), Some(t)).await
    {
        let _ = detach(vm, name).await;
        return Err(e);
    }
    Ok(())
}

async fn del_iothread(sock: &Path, name: &str, io: &DiskIo) {
    if io.iothread {
        let _ = qmp::execute(
            sock,
            "object-del",
            Some(json!({"id": iothread_id(name)})),
            QMP_TIMEOUT,
        )
        .await;
    }
}

/// Retry `command` until it succeeds: unplug finishes asynchronously, and
/// the backend stays "in use" until the guest lets the device go. PCIe
/// unplug waits for the guest's hotplug driver (Linux pciehp waits ~5s).
async fn retry_until_unplugged(
    sock: &Path,
    command: &str,
    args: serde_json::Value,
    attempts: u32,
) -> Result<()> {
    let mut last = None;
    for _ in 0..attempts {
        match qmp::execute(sock, command, Some(args.clone()), QMP_TIMEOUT).await {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    match last {
        Some(e) => Err(e).context(format!("{command} after device_del")),
        None => Ok(()),
    }
}

/// Detach (hot-unplug when live) and delete a data disk's file, or only the
/// link for a disk attached from an existing image.
pub async fn detach(vm: &VmRecord, name: &str) -> Result<()> {
    validate_disk_name(name)?;
    let Some(path) = find_disk(&vm.workspace, name) else {
        bail!("disk {name:?} not found");
    };
    let io = disk_io(&vm.workspace, name);
    if is_live(vm) {
        let sock = vm.workspace.join("qmp.sock");
        qmp::execute(
            &sock,
            "device_del",
            Some(json!({"id": device_id(name)})),
            QMP_TIMEOUT,
        )
        .await
        .context("device_del")?;
        match io.bus() {
            DiskBus::Scsi => {
                retry_until_unplugged(
                    &sock,
                    "blockdev-del",
                    json!({"node-name": node_name(name)}),
                    50,
                )
                .await?
            }
            DiskBus::VirtioBlk => {
                retry_until_unplugged(
                    &sock,
                    "blockdev-del",
                    json!({"node-name": node_name(name)}),
                    300,
                )
                .await?;
                del_iothread(&sock, name, &io).await;
            }
            DiskBus::VhostUserBlk => {
                retry_until_unplugged(
                    &sock,
                    "chardev-remove",
                    json!({"id": chardev_id(name)}),
                    300,
                )
                .await?;
            }
        }
    }
    if io.bus() == DiskBus::VhostUserBlk {
        stop_vhost_user(&vm.workspace, name).await;
    }
    let _ = std::fs::remove_file(sidecar_path(&vm.workspace, name));
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

/// `block_resize` for the root: by [`ROOT_NODE`] when the root has it, else
/// by its `virtio0` device (raw/NBD roots, or VMs booted before the node was
/// named).
async fn resize_live_root(qmp_sock: &Path, new_bytes: u64) -> Result<()> {
    let by_node = qmp::execute(
        qmp_sock,
        "block_resize",
        Some(json!({"node-name": ROOT_NODE, "size": new_bytes})),
        QMP_TIMEOUT,
    )
    .await;
    if by_node.is_ok() {
        return Ok(());
    }
    qmp::execute(
        qmp_sock,
        "block_resize",
        Some(json!({"device": ROOT_BLOCK_DEVICE, "size": new_bytes})),
        QMP_TIMEOUT,
    )
    .await
    .context("block_resize")?;
    Ok(())
}

/// Grow a disk to `size_gib`. Shrinking is refused.
pub async fn resize(cfg: &Config, vm: &VmRecord, name: &str, size_gib: u64) -> Result<VmDiskInfo> {
    let (path, bus, throttle) = if name == ROOT_DISK {
        (vm.disk.clone(), "virtio", root_throttle(vm).cloned())
    } else {
        validate_disk_name(name)?;
        let Some(path) = find_disk(&vm.workspace, name) else {
            bail!("disk {name:?} not found");
        };
        if is_external(&path) {
            bail!("disk {name:?} is attached from an existing image; resize its source");
        }
        let io = disk_io(&vm.workspace, name);
        let bus = io.bus();
        if bus == DiskBus::VhostUserBlk && is_live(vm) {
            bail!("disk {name:?} is vhost-user-blk; resize it while the VM is stopped");
        }
        (path, bus_name(bus), io.throttle().cloned())
    };
    if !path.is_file() {
        bail!("disk {name:?} not found");
    }
    let new_bytes = size_gib
        .checked_mul(1 << 30)
        .context("disk size overflow")?;
    let (cur, _) = virtual_size(cfg, &path).await?;
    if new_bytes <= cur {
        bail!("disk {name:?} is already {cur} bytes; resize only grows");
    }
    if is_live(vm) {
        let qmp_sock = vm.workspace.join("qmp.sock");
        if name == ROOT_DISK {
            resize_live_root(&qmp_sock, new_bytes).await?;
        } else {
            qmp::execute(
                &qmp_sock,
                "block_resize",
                Some(json!({"node-name": node_name(name), "size": new_bytes})),
                QMP_TIMEOUT,
            )
            .await
            .context("block_resize")?;
        }
    } else {
        let out = tokio::process::Command::new(&cfg.qemu_img_binary)
            .arg("resize")
            .arg(&path)
            .arg(new_bytes.to_string())
            .output()
            .await
            .with_context(|| format!("running {}", cfg.qemu_img_binary))?;
        if !out.status.success() {
            bail!(
                "qemu-img resize failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    disk_info(cfg, name, &path, bus, throttle.as_ref()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_names() {
        assert!(validate_disk_name("data-1").is_ok());
        for bad in ["", "root", "-x", "Upper", "a/b", "a.b", &"x".repeat(33)] {
            assert!(validate_disk_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn qcow2_root_names_its_format_node_and_keeps_the_virtio_drive() {
        let d = root_drive(Path::new("/w/root.qcow2"), "qcow2", None);
        assert_eq!(
            d,
            "file=/w/root.qcow2,if=virtio,format=qcow2,cache=writeback,node-name=root"
        );
        let raw = root_drive(Path::new("/srv/shared/root.raw"), "raw", None);
        assert!(!raw.contains("node-name"), "{raw}");
        assert!(raw.contains("if=virtio,format=raw,cache=writeback"));
        let nbd = root_drive(
            Path::new("/w/root.qcow2"),
            "qcow2",
            Some(Path::new("/w/nbd.sock")),
        );
        assert_eq!(nbd, "file=nbd:unix:/w/nbd.sock,if=virtio,format=raw");
    }

    #[test]
    fn cdrom_args_use_ahci_ports_read_only_without_bootindex() {
        use fluxvm_core::model::CdromSpec;
        assert!(cdrom_args(&[]).is_empty());
        let args = cdrom_args(&[
            CdromSpec {
                name: "install".into(),
                path: "/iso/win.iso".into(),
            },
            CdromSpec {
                name: "virtio".into(),
                path: "/iso/virtio-win.iso".into(),
            },
        ])
        .join(" ");
        assert!(args.contains("node-name=cd-install,read-only=on"));
        assert!(args.contains("file.filename=/iso/win.iso,file.read-only=on"));
        assert!(args.contains("ide-cd,bus=ide.0,drive=cd-install,id=cdrom-install"));
        assert!(args.contains("ide-cd,bus=ide.1,drive=cd-virtio,id=cdrom-virtio"));
        let ejected = cdrom_args(&[
            CdromSpec {
                name: "install".into(),
                path: std::path::PathBuf::new(),
            },
            CdromSpec {
                name: "virtio".into(),
                path: "/srv/iso/virtio-win.iso".into(),
            },
        ])
        .join(" ");
        assert!(ejected.starts_with("-device ide-cd,bus=ide.0,id=cdrom-install -blockdev"));
        assert!(!ejected.contains("node-name=cd-install"));
        assert!(ejected.contains("ide-cd,bus=ide.1,drive=cd-virtio,id=cdrom-virtio"));
        assert!(!args.contains("bootindex"));
    }

    #[test]
    fn boot_args_follow_the_disks_dir() {
        let tmp = std::env::temp_dir().join(format!("fluxvm-disks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(disks_dir(&tmp)).unwrap();
        assert!(boot_args(&tmp).is_empty());
        for n in ["b", "a", "bad.name"] {
            std::fs::write(disks_dir(&tmp).join(format!("{n}.qcow2")), b"").unwrap();
        }
        std::fs::write(disks_dir(&tmp).join("c.img"), b"").unwrap();
        let args = boot_args(&tmp).join(" ");
        assert!(args.find("node-name=data-a").unwrap() < args.find("node-name=data-b").unwrap());
        assert!(args.contains("scsi-hd,bus=scsi0.0,drive=data-a,id=disk-a,serial=a"));
        assert!(!args.contains("bad") && !args.contains("data-c"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn sidecars_pick_the_bus_and_io_options() {
        use fluxvm_core::model::{DiskAio, DiskCache};
        let tmp = std::env::temp_dir().join(format!("fluxvm-disks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(disks_dir(&tmp)).unwrap();
        for n in ["fast", "plain", "vub", "hot"] {
            std::fs::write(disks_dir(&tmp).join(format!("{n}.qcow2")), b"").unwrap();
        }
        set_disk_io(
            &tmp,
            "fast",
            &DiskIo {
                bus: Some(DiskBus::VirtioBlk),
                aio: Some(DiskAio::Native),
                cache: Some(DiskCache::None),
                iothread: true,
                queues: Some(4),
                throttle: None,
            },
        )
        .unwrap();
        set_disk_io(
            &tmp,
            "vub",
            &DiskIo {
                bus: Some(DiskBus::VhostUserBlk),
                queues: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        write_sidecar(
            &tmp,
            "hot",
            &Sidecar {
                io: DiskIo {
                    bus: Some(DiskBus::VirtioBlk),
                    ..Default::default()
                },
                port: Some(1),
            },
        )
        .unwrap();
        set_disk_io(&tmp, "plain", &DiskIo::default()).unwrap();
        assert!(!sidecar_path(&tmp, "plain").exists());

        let args = boot_args(&tmp).join(" ");
        assert!(args.contains("-object iothread,id=io-fast"));
        assert!(args.contains(
            "driver=qcow2,node-name=data-fast,cache.direct=on,file.driver=file,file.filename="
        ));
        assert!(args.contains(",file.aio=native"));
        assert!(args.contains(
            "virtio-blk-pci,drive=data-fast,id=disk-fast,serial=fast,bus=pcie.0,write-cache=on,iothread=io-fast,num-queues=4"
        ));
        assert!(
            args.contains(
                "virtio-blk-pci,drive=data-hot,id=disk-hot,serial=hot,bus=hotplug-disk-1"
            )
        );
        assert!(args.contains("scsi-hd,bus=scsi0.0,drive=data-plain,id=disk-plain,serial=plain -"));
        assert!(args.contains(&format!(
            "-chardev socket,id=vub-vub,path={}",
            tmp.join("vub-vub.sock").display()
        )));
        assert!(
            args.contains("vhost-user-blk-pci,chardev=vub-vub,id=disk-vub,bus=pcie.0,num-queues=2")
        );
        assert!(!args.contains("node-name=data-vub"));
        assert!(has_vhost_user(&tmp));
        assert!(is_vhost_user(&tmp, "vub") && !is_vhost_user(&tmp, "fast"));
        assert_eq!(free_port(&tmp, 4), Some(0));
        assert_eq!(free_port(&tmp, 1), Some(0));
        write_sidecar(
            &tmp,
            "fast",
            &Sidecar {
                io: disk_io(&tmp, "fast"),
                port: Some(0),
            },
        )
        .unwrap();
        assert_eq!(free_port(&tmp, 4), Some(2));
        assert_eq!(free_port(&tmp, 2), None);
        assert_eq!(free_port(&tmp, 0), None);

        let dst = tmp.join("clone");
        std::fs::create_dir_all(disks_dir(&dst)).unwrap();
        copy_disk_io(&tmp, &dst, "fast").unwrap();
        assert_eq!(read_sidecar(&dst, "fast").port, None);
        assert_eq!(disk_io(&dst, "fast").queues, Some(4));
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn vhost_user_daemon_args() {
        use fluxvm_core::model::{DiskAio, DiskCache};
        let io = DiskIo {
            bus: Some(DiskBus::VhostUserBlk),
            aio: Some(DiskAio::IoUring),
            cache: Some(DiskCache::Writethrough),
            iothread: true,
            queues: Some(4),
            throttle: None,
        };
        let a = vhost_user_args(
            "d",
            Path::new("/w/disks/d.qcow2"),
            &io,
            Path::new("/w/vub-d.sock"),
        )
        .join(" ");
        assert!(a.contains(
            "--blockdev driver=file,node-name=file-d,filename=/w/disks/d.qcow2,cache.direct=off,aio=io_uring"
        ));
        assert!(a.contains("--blockdev driver=qcow2,node-name=data-d,file=file-d"));
        assert!(a.contains("--object iothread,id=io-d"));
        assert!(a.contains(
            "--export type=vhost-user-blk,id=exp-d,node-name=data-d,addr.type=unix,addr.path=/w/vub-d.sock,writable=on,writethrough=on,num-queues=4,iothread=io-d"
        ));
    }

    #[test]
    fn disk_io_validation() {
        use fluxvm_core::model::{DiskAio, DiskCache};
        let ok = DiskIo {
            bus: Some(DiskBus::VirtioBlk),
            aio: Some(DiskAio::Native),
            cache: Some(DiskCache::Directsync),
            iothread: true,
            queues: Some(64),
            throttle: None,
        };
        assert!(ok.validate(false).is_ok());
        let mut bad = ok.clone();
        bad.cache = None;
        assert!(bad.validate(false).is_err(), "native without O_DIRECT");
        let mut bad = ok.clone();
        bad.queues = Some(0);
        assert!(bad.validate(false).is_err());
        let mut bad = ok.clone();
        bad.bus = None;
        assert!(bad.validate(false).is_err(), "iothread on scsi");
        assert!(ok.validate(true).is_ok(), "virtio-blk is the root's bus");
        let mut root = ok;
        root.bus = Some(DiskBus::VhostUserBlk);
        assert!(root.validate(true).is_err(), "vhost-user-blk root");
    }

    #[test]
    fn linked_disks_boot_with_their_format_and_stay_external() {
        let tmp = std::env::temp_dir().join(format!("fluxvm-disks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(disks_dir(&tmp)).unwrap();
        let src = tmp.join("pvc.img");
        std::fs::write(&src, b"").unwrap();
        let link = disks_dir(&tmp).join("pvc.raw");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        std::fs::write(disks_dir(&tmp).join("own.qcow2"), b"").unwrap();
        let args = boot_args(&tmp).join(" ");
        assert!(args.contains("driver=raw,node-name=data-pvc,cache.direct=off,file.driver=file"));
        assert!(args.contains("driver=qcow2,node-name=data-own"));
        assert!(is_external(&link));
        assert!(!is_external(&disks_dir(&tmp).join("own.qcow2")));
        assert_eq!(find_disk(&tmp, "pvc"), Some(link));
        assert_eq!(find_disk(&tmp, "nope"), None);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn throttle_args_fill_every_limit_and_the_burst_length() {
        let t = DiskThrottle {
            iops_total: Some(500),
            bps_write: Some(1 << 20),
            iops_total_max: Some(1000),
            burst_seconds: Some(5),
            ..Default::default()
        };
        let a = throttle_args(json!({"id": "disk-d"}), Some(&t));
        assert_eq!(a["id"], "disk-d");
        assert_eq!(a["iops"], 500);
        assert_eq!(a["bps_wr"], 1 << 20);
        assert_eq!(a["bps"], 0);
        assert_eq!(a["iops_max"], 1000);
        assert_eq!(a["iops_max_length"], 5);
        assert!(a.get("bps_max_length").is_none());
        let cleared = throttle_args(json!({"device": "virtio0"}), None);
        assert_eq!(cleared["device"], "virtio0");
        for k in ["iops", "iops_rd", "iops_wr", "bps", "bps_rd", "bps_wr"] {
            assert_eq!(cleared[k], 0, "{k}");
        }
    }

    #[test]
    fn a_throttle_alone_keeps_the_root_drive_whole() {
        let io = DiskIo {
            throttle: Some(DiskThrottle {
                iops_total: Some(100),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!root_is_split(Some(&io)));
        let a = root_args(Path::new("/w/root.qcow2"), "qcow2", None, Some(&io)).unwrap();
        assert_eq!(a.len(), 2);
        assert!(a[1].contains("if=virtio"));
        assert_eq!(root_throttle_target(false), json!({"device": "virtio0"}));
        let split = DiskIo {
            iothread: true,
            ..io
        };
        assert!(root_is_split(Some(&split)));
        assert_eq!(root_throttle_target(true), json!({"id": "disk-root"}));
    }

    #[test]
    fn boot_throttles_cover_the_root_and_throttled_data_disks() {
        let tmp = std::env::temp_dir().join(format!("fluxvm-disks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(disks_dir(&tmp)).unwrap();
        for n in ["a", "b", "v"] {
            std::fs::write(disks_dir(&tmp).join(format!("{n}.qcow2")), b"").unwrap();
        }
        let t = DiskThrottle {
            bps_total: Some(10 << 20),
            ..Default::default()
        };
        set_disk_io(
            &tmp,
            "a",
            &DiskIo {
                throttle: Some(t.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let vub = DiskIo {
            bus: Some(DiskBus::VhostUserBlk),
            throttle: Some(t.clone()),
            ..Default::default()
        };
        assert!(set_disk_io(&tmp, "v", &vub).is_err(), "vhost-user-blk");
        let root = DiskIo {
            throttle: Some(t.clone()),
            ..Default::default()
        };
        let got = boot_throttles(&tmp, Some(&root));
        assert_eq!(
            got,
            vec![
                (json!({"device": "virtio0"}), t.clone()),
                (json!({"id": "disk-a"}), t.clone()),
            ]
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn throttle_validation() {
        let ok = DiskThrottle {
            iops_read: Some(100),
            iops_write: Some(50),
            bps_total: Some(1 << 20),
            bps_total_max: Some(2 << 20),
            burst_seconds: Some(10),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
        let mut bad = ok.clone();
        bad.iops_total = Some(10);
        assert!(bad.validate().is_err(), "total with read/write");
        let mut bad = ok.clone();
        bad.bps_total_max = Some(1);
        assert!(bad.validate().is_err(), "max below base");
        let mut bad = ok.clone();
        bad.iops_total_max = Some(10);
        assert!(bad.validate().is_err(), "max without base");
        let mut bad = ok.clone();
        bad.bps_total_max = None;
        assert!(bad.validate().is_err(), "burst_seconds without a max");
        let mut bad = ok;
        bad.iops_read = Some(0);
        assert!(bad.validate().is_err(), "zero");
    }
}
