// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::{
    rule_pack::{PackInfo, PatchSet},
    vm_key,
};

pub const DEFAULT_GUARD_PIN_ROOT: &str = "/sys/fs/bpf/fluxvm/guard";
pub const DEFAULT_GUARD_STATE_ROOT: &str = "/run/fluxvm/guard";

const FLAG_AUDIT: u32 = 1 << 0;
const FLAG_DENY_EXEC: u32 = 1 << 1;
const FLAG_DENY_WX: u32 = 1 << 2;
const FLAG_RESTRICT_DEVICES: u32 = 1 << 3;
const FLAG_RESTRICT_WRITES: u32 = 1 << 4;
const FLAG_RESTRICT_EXEC: u32 = 1 << 5;
const FLAG_PATCHES: u32 = 1 << 6;
const DEVICE_CHAR: u32 = 1;
const DEVICE_BLOCK: u32 = 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GuardMode {
    Audit,
    Enforce,
}

impl GuardMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "audit" => Ok(Self::Audit),
            "enforce" | "enforced" => Ok(Self::Enforce),
            _ => bail!("invalid guard mode {value:?}; use audit or enforce"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub path: PathBuf,
    pub rdev: u32,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileIdentity {
    pub path: PathBuf,
    pub filesystem_dev: u32,
    pub inode: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardPolicyState {
    pub schema_version: u32,
    pub vm_id: Uuid,
    pub vm_key: u64,
    pub pid: u32,
    pub cgroup_path: PathBuf,
    pub cgroup_id: u64,
    pub mode: GuardMode,
    pub generation: u32,
    pub deny_exec: bool,
    pub deny_wx: bool,
    pub restrict_devices: bool,
    pub restrict_writes: bool,
    pub allowed_devices: Vec<DeviceIdentity>,
    pub allowed_files: Vec<FileIdentity>,
    /// Positive exec allow-list (`FLAG_RESTRICT_EXEC`). Absent in state files
    /// written before it existed.
    #[serde(default)]
    pub restrict_exec: bool,
    #[serde(default)]
    pub allowed_execs: Vec<FileIdentity>,
    /// Signed virtual-patch rule packs applied with this policy generation.
    #[serde(default)]
    pub patches: Vec<PackInfo>,
    #[serde(default)]
    pub patch_files: Vec<PatchFileState>,
    #[serde(default)]
    pub patch_devices: Vec<DeviceIdentity>,
}

/// A file deny rule resolved to kernel identity; `bits` are `rule_pack::BIT_*`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PatchFileState {
    #[serde(flatten)]
    pub file: FileIdentity,
    pub bits: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardStatus {
    pub configured: bool,
    pub kernel_policy_present: bool,
    /// Ids of applied rule packs whose `valid_until` has passed. They stay
    /// loaded until the policy is re-applied; re-apply without them.
    #[serde(default)]
    pub expired_packs: Vec<String>,
    pub state: Option<GuardPolicyState>,
}

pub fn cgroup_for_pid(pid: u32) -> Result<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("reading cgroup for pid {pid}"))?;
    for line in text.lines() {
        if let Some(relative) = line.strip_prefix("0::") {
            let rel = relative.trim_start_matches('/');
            return Ok(Path::new("/sys/fs/cgroup").join(rel));
        }
    }
    bail!("pid {pid} is not in a cgroup-v2 hierarchy")
}

fn cgroup_id(path: &Path) -> Result<u64> {
    Ok(fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .ino())
}

fn device_identity(path: &Path) -> Result<DeviceIdentity> {
    let meta = fs::metadata(path).with_context(|| format!("stat device {}", path.display()))?;
    let ft = meta.file_type();
    let (kind, _) = if ft.is_char_device() {
        ("char", DEVICE_CHAR)
    } else if ft.is_block_device() {
        ("block", DEVICE_BLOCK)
    } else {
        bail!("{} is not a character/block device", path.display())
    };
    let rdev =
        kernel_dev_t(meta.rdev()).with_context(|| format!("encode rdev for {}", path.display()))?;
    Ok(DeviceIdentity {
        path: path.to_path_buf(),
        rdev,
        kind: kind.into(),
    })
}

fn file_identity(path: &Path) -> Result<FileIdentity> {
    let meta =
        fs::metadata(path).with_context(|| format!("stat writable file {}", path.display()))?;
    if !meta.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let dev = kernel_dev_t(meta.dev())
        .with_context(|| format!("encode filesystem dev_t for {}", path.display()))?;
    Ok(FileIdentity {
        path: path.to_path_buf(),
        filesystem_dev: dev,
        inode: meta.ino(),
    })
}

/// Like [`file_identity`], but also requires an executable regular file so a
/// typo in an allow-list cannot silently list a data file.
fn exec_identity(path: &Path) -> Result<FileIdentity> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        fs::metadata(path).with_context(|| format!("stat exec allow-list {}", path.display()))?;
    if !meta.file_type().is_file() || meta.permissions().mode() & 0o111 == 0 {
        bail!("{} is not an executable regular file", path.display());
    }
    file_identity(path)
}

fn kernel_dev_t(raw: u64) -> Result<u32> {
    // Userspace dev_t (glibc, 64-bit) -> the kernel's internal dev_t. Inside
    // the kernel, `inode->i_rdev` and `super_block->s_dev` are
    // MKDEV(major, minor) = (major << 20) | minor. That is NOT the compact
    // `new_encode_dev()` layout, which is only the userspace-facing ABI; using
    // it here makes every allow-list key miss the BPF CO-RE read (verified on
    // a live kernel: root fs 8:2 is 0x800002 in-kernel, 0x802 encoded).
    let major = ((raw >> 8) & 0xfff) | ((raw >> 32) & 0xffff_f000);
    let minor = (raw & 0xff) | ((raw >> 12) & 0xffff_ff00);
    if major >= (1 << 12) || minor >= (1 << 20) {
        bail!("device number major={major} minor={minor} does not fit the kernel's 12:20 dev_t");
    }
    Ok(((major << 20) | minor) as u32)
}

fn default_vmm_devices() -> Vec<PathBuf> {
    [
        "/dev/kvm",
        "/dev/net/tun",
        "/dev/vhost-net",
        "/dev/vhost-vsock",
        "/dev/vhost-vdpa",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|p| p.exists())
    .collect()
}

fn generation() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mixed = (nanos as u64) ^ ((std::process::id() as u64) << 32);
    let v = (mixed ^ (mixed >> 32)) as u32;
    if v == 0 { 1 } else { v }
}

#[allow(clippy::too_many_arguments)]
pub fn apply_policy(
    vm_id: Uuid,
    pid: u32,
    mode: GuardMode,
    deny_exec: bool,
    deny_wx: bool,
    restrict_devices: bool,
    restrict_writes: bool,
    allow_devices: &[PathBuf],
    allow_files: &[PathBuf],
    allow_execs: Option<&[PathBuf]>,
    patches: Option<&PatchSet>,
    pin_root: &Path,
    state_root: &Path,
) -> Result<GuardPolicyState> {
    let cgroup_path = cgroup_for_pid(pid)?;
    let cgroup_id = cgroup_id(&cgroup_path)?;
    let key = vm_key(vm_id);
    let generation = generation();
    require_guard_maps(pin_root)?;
    if allow_execs.is_some() {
        require_exec_map(pin_root)?;
    }
    let patches = patches.filter(|p| !p.is_empty());
    if patches.is_some() {
        require_patch_maps(pin_root)?;
    }
    let previous = if state_path(state_root, vm_id).exists() {
        Some(read_state(state_root, vm_id)?)
    } else {
        None
    };

    let mut device_paths = allow_devices.to_vec();
    if restrict_devices {
        device_paths.extend(default_vmm_devices());
    }
    device_paths.sort();
    device_paths.dedup();
    let mut devices = Vec::new();
    for path in device_paths {
        devices.push(device_identity(&path)?);
    }
    let mut files = Vec::new();
    for path in allow_files {
        files.push(file_identity(path)?);
    }
    let restrict_exec = allow_execs.is_some();
    let mut execs = Vec::new();
    for path in allow_execs.unwrap_or_default() {
        execs.push(exec_identity(path)?);
    }
    execs.sort_by_key(|f| (f.filesystem_dev, f.inode));
    execs.dedup_by_key(|f| (f.filesystem_dev, f.inode));
    let mut patch_files: Vec<PatchFileState> = Vec::new();
    let mut patch_devices: Vec<DeviceIdentity> = Vec::new();
    if let Some(set) = patches {
        for (path, bits) in &set.files {
            let id = file_identity(path)?;
            // Two rule paths can name the same inode (symlink, hard link).
            match patch_files
                .iter_mut()
                .find(|p| p.file.filesystem_dev == id.filesystem_dev && p.file.inode == id.inode)
            {
                Some(existing) => existing.bits |= bits,
                None => patch_files.push(PatchFileState {
                    file: id,
                    bits: *bits,
                }),
            }
        }
        for path in &set.devices {
            patch_devices.push(device_identity(path)?);
        }
    }

    // Populate the new generation first, then publish the policy atomically by
    // replacing one cgroup-keyed map value. Old generations become unreachable
    // and can never silently broaden a reconfigured policy after state loss.
    for d in &devices {
        update_device(pin_root, key, generation, d)?;
    }
    for f in &files {
        update_file(pin_root, key, generation, f)?;
    }
    for f in &execs {
        update_exec(pin_root, key, generation, f)?;
    }
    for f in &patch_files {
        update_key_value(
            &pin_root.join("maps/guard_patch_files"),
            &file_key(key, generation, &f.file),
            &[f.bits],
        )?;
    }
    for d in &patch_devices {
        update_key_value(
            &pin_root.join("maps/guard_patch_devices"),
            &device_key(key, generation, d),
            &[1],
        )?;
    }

    let mut flags = 0u32;
    if mode == GuardMode::Audit {
        flags |= FLAG_AUDIT;
    }
    if deny_exec {
        flags |= FLAG_DENY_EXEC;
    }
    if deny_wx {
        flags |= FLAG_DENY_WX;
    }
    if restrict_devices {
        flags |= FLAG_RESTRICT_DEVICES;
    }
    if restrict_writes {
        flags |= FLAG_RESTRICT_WRITES;
    }
    if restrict_exec {
        flags |= FLAG_RESTRICT_EXEC;
    }
    if !patch_files.is_empty() || !patch_devices.is_empty() {
        flags |= FLAG_PATCHES;
    }
    update_policy(pin_root, cgroup_id, key, flags, generation)?;

    // Once the new generation is live, retire the previous cgroup binding and
    // its reachable allow-list entries. Failure to remove stale allow entries
    // cannot broaden the new policy because every lookup carries generation.
    if let Some(old) = &previous {
        if old.cgroup_id != cgroup_id {
            let _ = delete_key(
                &pin_root.join("maps/guard_policies"),
                &old.cgroup_id.to_le_bytes(),
            );
        }
        for d in &old.allowed_devices {
            let _ = delete_key(
                &pin_root.join("maps/guard_devices"),
                &device_key(old.vm_key, old.generation, d),
            );
        }
        for f in &old.allowed_files {
            let _ = delete_key(
                &pin_root.join("maps/guard_files"),
                &file_key(old.vm_key, old.generation, f),
            );
        }
        for f in &old.allowed_execs {
            let _ = delete_key(
                &pin_root.join("maps/guard_exec"),
                &file_key(old.vm_key, old.generation, f),
            );
        }
        delete_patches(pin_root, old);
    }

    let state = GuardPolicyState {
        schema_version: 1,
        vm_id,
        vm_key: key,
        pid,
        cgroup_path,
        cgroup_id,
        mode,
        generation,
        deny_exec,
        deny_wx,
        restrict_devices,
        restrict_writes,
        allowed_devices: devices,
        allowed_files: files,
        restrict_exec,
        allowed_execs: execs,
        patches: patches.map(|p| p.packs.clone()).unwrap_or_default(),
        patch_files,
        patch_devices,
    };
    write_state(state_root, &state)?;
    Ok(state)
}

pub fn remove_policy(vm_id: Uuid, pin_root: &Path, state_root: &Path) -> Result<()> {
    let state = if state_path(state_root, vm_id).exists() {
        Some(read_state(state_root, vm_id)?)
    } else {
        None
    };
    if let Some(state) = &state {
        let _ = delete_key(
            &pin_root.join("maps/guard_policies"),
            &state.cgroup_id.to_le_bytes(),
        );
        // Best effort cleanup of the exact active generation. Stale generations
        // are harmless because generation is part of every allow-list key.
        for d in &state.allowed_devices {
            let _ = delete_key(
                &pin_root.join("maps/guard_devices"),
                &device_key(state.vm_key, state.generation, d),
            );
        }
        for f in &state.allowed_files {
            let _ = delete_key(
                &pin_root.join("maps/guard_files"),
                &file_key(state.vm_key, state.generation, f),
            );
        }
        for f in &state.allowed_execs {
            let _ = delete_key(
                &pin_root.join("maps/guard_exec"),
                &file_key(state.vm_key, state.generation, f),
            );
        }
        delete_patches(pin_root, state);
    }
    let path = state_path(state_root, vm_id);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn status(vm_id: Uuid, pin_root: &Path, state_root: &Path) -> Result<GuardStatus> {
    let state = if state_path(state_root, vm_id).exists() {
        Some(read_state(state_root, vm_id)?)
    } else {
        None
    };
    let present = state
        .as_ref()
        .map(|s| {
            lookup_exists(
                &pin_root.join("maps/guard_policies"),
                &s.cgroup_id.to_le_bytes(),
            )
        })
        .unwrap_or(false);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let expired_packs = state
        .as_ref()
        .map(|s| expired_pack_ids(&s.patches, now))
        .unwrap_or_default();
    Ok(GuardStatus {
        expired_packs,
        configured: state.is_some(),
        kernel_policy_present: present,
        state,
    })
}

pub fn stream_events(vm_id: Uuid, pin_root: &Path, seconds: u64, limit: usize) -> Result<()> {
    let helper = std::env::var("FLUXVM_GUARD_EVENT_HELPER")
        .unwrap_or_else(|_| "/usr/libexec/fluxvm/fluxvm-guard-events".into());
    let helper = if Path::new(&helper).exists() {
        helper
    } else {
        "fluxvm-guard-events".into()
    };
    let status = Command::new(helper)
        .arg(pin_root.join("maps/guard_events"))
        .arg(vm_key(vm_id).to_string())
        .arg(seconds.to_string())
        .arg(limit.max(1).to_string())
        .status()
        .context("running fluxvm-guard-events")?;
    if !status.success() {
        bail!("guard event reader exited with {status}");
    }
    Ok(())
}

fn require_guard_maps(root: &Path) -> Result<()> {
    for name in [
        "guard_policies",
        "guard_devices",
        "guard_files",
        "guard_stats",
        "guard_events",
    ] {
        let p = root.join("maps").join(name);
        if !p.exists() {
            bail!(
                "VMM Guard map missing: {}; load fluxvm_guard.bpf.o first",
                p.display()
            );
        }
    }
    Ok(())
}

/// The exec allow-list map is newer than the other guard maps, so it is only
/// required when an exec policy is applied; older loaded objects keep working.
fn require_exec_map(root: &Path) -> Result<()> {
    let p = root.join("maps/guard_exec");
    if !p.exists() {
        bail!(
            "VMM Guard map missing: {}; reload a fluxvm_guard.bpf.o built with exec allow-list support",
            p.display()
        );
    }
    Ok(())
}

/// The patch maps are newer than the other guard maps; required only when a
/// rule pack is applied.
fn require_patch_maps(root: &Path) -> Result<()> {
    for name in ["guard_patch_files", "guard_patch_devices"] {
        let p = root.join("maps").join(name);
        if !p.exists() {
            bail!(
                "VMM Guard map missing: {}; reload a fluxvm_guard.bpf.o built with rule-pack support",
                p.display()
            );
        }
    }
    Ok(())
}

fn delete_patches(pin_root: &Path, state: &GuardPolicyState) {
    for f in &state.patch_files {
        let _ = delete_key(
            &pin_root.join("maps/guard_patch_files"),
            &file_key(state.vm_key, state.generation, &f.file),
        );
    }
    for d in &state.patch_devices {
        let _ = delete_key(
            &pin_root.join("maps/guard_patch_devices"),
            &device_key(state.vm_key, state.generation, d),
        );
    }
}

fn expired_pack_ids(packs: &[PackInfo], now: i64) -> Vec<String> {
    packs
        .iter()
        .filter(|p| p.valid_until <= now)
        .map(|p| p.id.clone())
        .collect()
}

fn update_policy(root: &Path, cgroup: u64, vm: u64, flags: u32, generation: u32) -> Result<()> {
    let mut value = Vec::with_capacity(16);
    value.extend(vm.to_le_bytes());
    value.extend(flags.to_le_bytes());
    value.extend(generation.to_le_bytes());
    update_key_value(
        &root.join("maps/guard_policies"),
        &cgroup.to_le_bytes(),
        &value,
    )
}
fn device_kind(d: &DeviceIdentity) -> u32 {
    if d.kind == "block" {
        DEVICE_BLOCK
    } else {
        DEVICE_CHAR
    }
}
fn device_key(vm: u64, generation: u32, d: &DeviceIdentity) -> Vec<u8> {
    let mut k = Vec::with_capacity(24);
    k.extend(vm.to_le_bytes());
    k.extend(generation.to_le_bytes());
    k.extend(d.rdev.to_le_bytes());
    k.extend(device_kind(d).to_le_bytes());
    k.extend(0u32.to_le_bytes());
    k
}
fn file_key(vm: u64, generation: u32, f: &FileIdentity) -> Vec<u8> {
    let mut k = Vec::with_capacity(24);
    k.extend(vm.to_le_bytes());
    k.extend(generation.to_le_bytes());
    k.extend(f.filesystem_dev.to_le_bytes());
    k.extend(f.inode.to_le_bytes());
    k
}
fn update_device(root: &Path, vm: u64, generation: u32, d: &DeviceIdentity) -> Result<()> {
    update_key_value(
        &root.join("maps/guard_devices"),
        &device_key(vm, generation, d),
        &[1],
    )
}
fn update_exec(root: &Path, vm: u64, generation: u32, f: &FileIdentity) -> Result<()> {
    update_key_value(
        &root.join("maps/guard_exec"),
        &file_key(vm, generation, f),
        &[1],
    )
}
fn update_file(root: &Path, vm: u64, generation: u32, f: &FileIdentity) -> Result<()> {
    update_key_value(
        &root.join("maps/guard_files"),
        &file_key(vm, generation, f),
        &[1],
    )
}

fn hex(bytes: &[u8]) -> Vec<String> {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn update_key_value(map: &Path, key: &[u8], value: &[u8]) -> Result<()> {
    let mut args = vec![
        "map".into(),
        "update".into(),
        "pinned".into(),
        map.display().to_string(),
        "key".into(),
        "hex".into(),
    ];
    args.extend(hex(key));
    args.push("value".into());
    args.push("hex".into());
    args.extend(hex(value));
    run_bpftool(&args)
}
fn delete_key(map: &Path, key: &[u8]) -> Result<()> {
    let mut args = vec![
        "map".into(),
        "delete".into(),
        "pinned".into(),
        map.display().to_string(),
        "key".into(),
        "hex".into(),
    ];
    args.extend(hex(key));
    run_bpftool(&args)
}
fn lookup_exists(map: &Path, key: &[u8]) -> bool {
    let mut args = vec![
        "map".into(),
        "lookup".into(),
        "pinned".into(),
        map.display().to_string(),
        "key".into(),
        "hex".into(),
    ];
    args.extend(hex(key));
    Command::new("bpftool")
        .args(&args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
fn run_bpftool(args: &[String]) -> Result<()> {
    let out = Command::new("bpftool")
        .args(args)
        .output()
        .context("running bpftool")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "bpftool failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}
fn state_path(root: &Path, id: Uuid) -> PathBuf {
    root.join(format!("{id}.json"))
}
fn write_state(root: &Path, state: &GuardPolicyState) -> Result<()> {
    fs::create_dir_all(root)?;
    let path = state_path(root, state.vm_id);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}
fn read_state(root: &Path, id: Uuid) -> Result<GuardPolicyState> {
    Ok(serde_json::from_slice(&fs::read(state_path(root, id))?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_key_is_generation_scoped() {
        let d = DeviceIdentity {
            path: "/dev/kvm".into(),
            rdev: 10,
            kind: "char".into(),
        };
        assert_ne!(device_key(7, 1, &d), device_key(7, 2, &d));
        assert_eq!(device_key(7, 1, &d).len(), 24);
    }
    #[test]
    fn file_key_is_generation_scoped() {
        let f = FileIdentity {
            path: "x".into(),
            filesystem_dev: 3,
            inode: 9,
        };
        assert_ne!(file_key(7, 1, &f), file_key(7, 2, &f));
        assert_eq!(file_key(7, 1, &f).len(), 24);
    }
    #[test]
    fn exec_allow_uses_the_file_key_layout() {
        let f = FileIdentity {
            path: "/bin/true".into(),
            filesystem_dev: 3,
            inode: 9,
        };
        assert_eq!(file_key(7, 1, &f).len(), 24);
        assert_eq!(FLAG_RESTRICT_EXEC, 1 << 5);
    }
    #[test]
    fn exec_identity_rejects_non_executables_and_missing_paths() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::write(&data, "x").unwrap();
        fs::set_permissions(&data, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(exec_identity(&data).is_err());
        assert!(exec_identity(&dir.path().join("missing")).is_err());
        let bin = dir.path().join("bin");
        fs::write(&bin, "x").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let id = exec_identity(&bin).unwrap();
        assert_eq!(id.inode, fs::metadata(&bin).unwrap().ino());
    }
    #[test]
    fn old_state_files_without_exec_fields_still_load() {
        let json = r#"{"schema_version":1,"vm_id":"00000000-0000-0000-0000-000000000001","vm_key":1,"pid":2,"cgroup_path":"/x","cgroup_id":3,"mode":"audit","generation":4,"deny_exec":true,"deny_wx":true,"restrict_devices":true,"restrict_writes":false,"allowed_devices":[],"allowed_files":[]}"#;
        let s: GuardPolicyState = serde_json::from_str(json).unwrap();
        assert!(!s.restrict_exec && s.allowed_execs.is_empty());
    }
    #[test]
    fn old_state_files_load_without_patch_fields_and_expired_packs_are_reported() {
        let json = r#"{"schema_version":1,"vm_id":"00000000-0000-0000-0000-000000000001","vm_key":1,"pid":2,"cgroup_path":"/x","cgroup_id":3,"mode":"audit","generation":4,"deny_exec":true,"deny_wx":true,"restrict_devices":true,"restrict_writes":false,"allowed_devices":[],"allowed_files":[]}"#;
        let s: GuardPolicyState = serde_json::from_str(json).unwrap();
        assert!(s.patches.is_empty() && s.patch_files.is_empty() && s.patch_devices.is_empty());
        let packs = vec![
            PackInfo {
                id: "old".into(),
                cve: vec![],
                valid_until: 100,
                signer: "ci".into(),
            },
            PackInfo {
                id: "new".into(),
                cve: vec![],
                valid_until: 200,
                signer: "ci".into(),
            },
        ];
        assert_eq!(expired_pack_ids(&packs, 150), vec!["old".to_string()]);
        assert_eq!(expired_pack_ids(&packs, 200).len(), 2);
    }
    #[test]
    fn patch_file_state_round_trips_with_flattened_identity() {
        let p = PatchFileState {
            file: FileIdentity {
                path: "/usr/lib/v".into(),
                filesystem_dev: 3,
                inode: 9,
            },
            bits: 3,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"inode\":9") && json.contains("\"bits\":3"));
        assert_eq!(serde_json::from_str::<PatchFileState>(&json).unwrap(), p);
        assert_eq!(FLAG_PATCHES, 1 << 6);
    }
    #[test]
    fn kernel_dev_encoding_matches_common_values() {
        // makedev(10, 232) on Linux/glibc is 0x0ae8; the kernel's MKDEV is (10 << 20) | 232.
        assert_eq!(kernel_dev_t(0x0ae8).unwrap(), 0x00a0_00e8);
        // The root disk seen on the test host: makedev(8, 2) = 0x802 -> 8:2 in-kernel.
        assert_eq!(kernel_dev_t(0x0802).unwrap(), 0x0080_0002);
        // A minor above 255 spreads across glibc's split fields: makedev(259, 300).
        let raw = (300u64 & 0xff) | ((259u64 & 0xfff) << 8) | ((300u64 & !0xff) << 12);
        assert_eq!(kernel_dev_t(raw).unwrap(), (259 << 20) | 300);
    }
}
