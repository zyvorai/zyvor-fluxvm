// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Migration-compatibility check between two host descriptions, for `fluxctl host-compat check`.
//!
//! A live migration or a snapshot restore on another host fails late: after the memory copy has started, or when
//! the guest first executes an instruction the new CPU lacks. Everything this module looks at can be known up
//! front from two small JSON descriptions of the hosts (and, optionally, the VM spec), so it is reported before
//! anything is started. Nothing here talks to a hypervisor, a daemon or `/dev/kvm`: it compares two documents.
//!
//! The rules are deliberately conservative and each finding names the document key it concerns. They are drawn
//! from the vendors' own statements (see `docs/RESEARCH.md`): the same CPU vendor and a superset of CPU features
//! on the destination (libvirt, Proxmox), Cloud Hypervisor accepting the previous two releases only, no backward
//! migration, Firecracker snapshots restored on the same format version and micro-architecture unless a CPU
//! template masks the difference, and VFIO passthrough needing the same device on the destination.
//! A `Compatible` verdict means "none of these known blockers", not a guarantee that the migration will succeed.
//!
//! Hostile input: the CLI caps the file size, and this module caps the number of CPU flags, backends and devices
//! and the length of every string it keeps or prints.

use crate::config_lint::{Finding, Severity};
use crate::model::{BackendKind, CreateVmRequest};
use crate::topology::HostTopology;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Largest document the CLI reads (the caller enforces it; exported so the limit is in one place).
pub const MAX_DOC_BYTES: u64 = 2 * 1024 * 1024;
/// More CPU flags than this is not a real CPU.
pub const MAX_FLAGS: usize = 2048;
pub const MAX_FLAG_LEN: usize = 48;
pub const MAX_BACKENDS: usize = 16;
pub const MAX_STR: usize = 128;
const MAX_LISTED: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostDescription {
    #[serde(default)]
    pub name: String,
    /// `x86_64` or `aarch64`.
    #[serde(default)]
    pub arch: String,
    /// Kernel release, e.g. `6.1.0-18-amd64`.
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub cpu: CpuDescription,
    #[serde(default)]
    pub memory_mib: u64,
    #[serde(default)]
    pub free_memory_mib: Option<u64>,
    #[serde(default)]
    pub smt: Option<bool>,
    /// Hypervisor name (`qemu`, `cloud-hypervisor`, `firecracker`) to version string.
    #[serde(default)]
    pub backends: BTreeMap<String, String>,
    /// The host graph of `fluxvm_core::topology` (NUMA nodes, hugepage pools, PCI devices), when known.
    #[serde(default)]
    pub topology: Option<HostTopology>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CpuDescription {
    /// `GenuineIntel`, `AuthenticAMD`, or an ARM implementer name.
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub family: Option<u32>,
    #[serde(default)]
    pub model: Option<u32>,
    #[serde(default)]
    pub stepping: Option<u32>,
    #[serde(default)]
    pub microcode: Option<String>,
    /// `flags` of `/proc/cpuinfo` (x86) or `Features` (arm).
    #[serde(default)]
    pub flags: Vec<String>,
}

/// What the check needs to know about the VM that would move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmNeeds {
    pub backend: BackendKind,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub cpu_template: Option<String>,
    pub hugepages: bool,
    pub numa_node: Option<u32>,
    /// Host NUMA nodes named by the `numa` list.
    pub numa_host_nodes: Vec<u32>,
    pub vfio_devices: Vec<String>,
    pub cpuset: Option<String>,
}

impl VmNeeds {
    pub fn from_request(r: &CreateVmRequest) -> Self {
        VmNeeds {
            backend: r.backend,
            vcpus: u32::from(r.vcpus),
            memory_mib: r.memory_mib,
            cpu_template: r.cpu_template.clone().filter(|t| !t.trim().is_empty()),
            hugepages: r.hugepages.unwrap_or(false),
            numa_node: r.numa_node.map(u32::from),
            numa_host_nodes: r.numa.iter().map(|n| n.host_node).collect(),
            vfio_devices: r
                .vfio_devices
                .iter()
                .take(64)
                .map(|d| d.trim().to_ascii_lowercase())
                .collect(),
            cpuset: r.cpuset.clone(),
        }
    }

    /// Parses the JSON of `fluxctl create --spec`, resolving `instancetype` keys first like `vm-spec check`.
    pub fn from_spec_str(raw: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("the VM spec is not valid JSON: {e}"))?;
        let (value, _) = crate::instancetype::lint_prepare(value);
        let req: CreateVmRequest =
            serde_json::from_value(value).map_err(|e| format!("not a VM spec: {e}"))?;
        Ok(Self::from_request(&req))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// No known blocker.
    Compatible,
    /// Nothing that must fail, but something to check by hand first.
    CompatibleWithWarnings,
    /// At least one rule says the migration or restore will fail or the guest may crash.
    Incompatible,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompatReport {
    pub verdict: Verdict,
    pub source: String,
    pub destination: String,
    pub findings: Vec<Finding>,
}

/// Parses and sanity-checks a host description. Errors are one-line strings for the CLI.
pub fn parse_host(raw: &str) -> Result<HostDescription, String> {
    // serde accepts a JSON array for a struct (fields by position); a host description is an object.
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("not a host description: {e}"))?;
    if !v.is_object() {
        return Err("not a host description: the file must hold one JSON object".into());
    }
    serde_json::from_value(v).map_err(|e| format!("not a host description: {e}"))
}

fn finding(sev: Severity, key: &str, message: String, fix: Option<&str>) -> Finding {
    Finding {
        severity: sev,
        key: key.to_string(),
        message,
        fix: fix.map(str::to_string),
    }
}

fn clip(s: &str) -> String {
    let t: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_STR)
        .collect();
    if t.is_empty() { "(unset)".into() } else { t }
}

fn label(h: &HostDescription) -> String {
    if h.name.trim().is_empty() {
        "(unnamed host)".into()
    } else {
        clip(&h.name)
    }
}

/// Dotted numeric prefix of a version string (`v54.0` -> [54, 0], `9.1.0-rc1` -> [9, 1, 0]).
pub fn parse_version(v: &str) -> Option<Vec<u64>> {
    let t = v.trim().trim_start_matches(['v', 'V']);
    let mut out = Vec::new();
    for part in t.split('.').take(4) {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || digits.len() > 9 {
            break;
        }
        out.push(digits.parse().ok()?);
        if digits.len() != part.len() {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn backend_key(b: BackendKind) -> Option<&'static str> {
    match b {
        BackendKind::Qemu => Some("qemu"),
        BackendKind::CloudHypervisor => Some("cloud-hypervisor"),
        BackendKind::Firecracker => Some("firecracker"),
        _ => None,
    }
}

fn known_backends() -> [(&'static str, BackendKind); 3] {
    [
        ("qemu", BackendKind::Qemu),
        ("cloud-hypervisor", BackendKind::CloudHypervisor),
        ("firecracker", BackendKind::Firecracker),
    ]
}

fn sanitized_flags(h: &HostDescription, which: &str, out: &mut Vec<Finding>) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if h.cpu.flags.len() > MAX_FLAGS {
        out.push(finding(
            Severity::Warning,
            &format!("{which}.cpu.flags"),
            format!(
                "{} CPU flags listed; only the first {MAX_FLAGS} are compared",
                h.cpu.flags.len()
            ),
            None,
        ));
    }
    for f in h.cpu.flags.iter().take(MAX_FLAGS) {
        let ok = !f.is_empty()
            && f.len() <= MAX_FLAG_LEN
            && f.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
        if ok {
            set.insert(f.to_ascii_lowercase());
        }
    }
    set
}

fn list(items: &BTreeSet<String>) -> String {
    let shown: Vec<&str> = items.iter().take(MAX_LISTED).map(String::as_str).collect();
    let more = items.len().saturating_sub(MAX_LISTED);
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// Compares `src` (where the VM runs) with `dst` (where it would go). `vm` narrows the check to one VM.
pub fn check(src: &HostDescription, dst: &HostDescription, vm: Option<&VmNeeds>) -> CompatReport {
    let mut f: Vec<Finding> = Vec::new();

    cpu_rules(src, dst, vm, &mut f);
    backend_rules(src, dst, vm, &mut f);
    if let Some(vm) = vm {
        resource_rules(dst, vm, &mut f);
    }
    if let (Some(a), Some(b)) = (src.smt, dst.smt) {
        if a != b {
            f.push(finding(
                Severity::Info,
                "smt",
                format!(
                    "SMT is {} on the source and {} on the destination; guest CPU topology and cost per vCPU differ",
                    on_off(a),
                    on_off(b)
                ),
                None,
            ));
        }
    }
    f.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.key.cmp(&b.key)));
    let verdict = if f.iter().any(|x| x.severity == Severity::Error) {
        Verdict::Incompatible
    } else if f.iter().any(|x| x.severity == Severity::Warning) {
        Verdict::CompatibleWithWarnings
    } else {
        Verdict::Compatible
    };
    CompatReport {
        verdict,
        source: label(src),
        destination: label(dst),
        findings: f,
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn cpu_rules(
    src: &HostDescription,
    dst: &HostDescription,
    vm: Option<&VmNeeds>,
    f: &mut Vec<Finding>,
) {
    if !src.arch.is_empty() && !dst.arch.is_empty() && src.arch != dst.arch {
        f.push(finding(
            Severity::Error,
            "arch",
            format!(
                "architecture differs: {} to {}; a guest cannot move between architectures",
                clip(&src.arch),
                clip(&dst.arch)
            ),
            Some("pick a destination with the same architecture"),
        ));
        return;
    }
    if src.arch.is_empty() || dst.arch.is_empty() {
        f.push(finding(
            Severity::Info,
            "arch",
            "architecture is not given for both hosts; assuming they are the same".into(),
            None,
        ));
    }
    let (sv, dv) = (src.cpu.vendor.trim(), dst.cpu.vendor.trim());
    if sv.is_empty() || dv.is_empty() {
        f.push(finding(
            Severity::Info,
            "cpu.vendor",
            "CPU vendor is not given for both hosts, so vendor compatibility was not checked"
                .into(),
            None,
        ));
    } else if sv != dv {
        f.push(finding(
            Severity::Error,
            "cpu.vendor",
            format!(
                "CPU vendor differs: {} to {}; live migration between vendors has no guarantee to work",
                clip(sv),
                clip(dv)
            ),
            Some("migrate within one vendor, or restart the VM on the destination"),
        ));
    }

    let template = vm.and_then(|v| v.cpu_template.as_deref());
    let mut diag = Vec::new();
    let sflags = sanitized_flags(src, "source", &mut diag);
    let dflags = sanitized_flags(dst, "destination", &mut diag);
    f.extend(diag);
    if sflags.is_empty() || dflags.is_empty() {
        f.push(finding(
            Severity::Info,
            "cpu.flags",
            "CPU flags are not given for both hosts, so feature compatibility was not checked"
                .into(),
            None,
        ));
    } else {
        let missing: BTreeSet<String> = sflags.difference(&dflags).cloned().collect();
        if !missing.is_empty() {
            let (sev, tail) = match (vm, template) {
                (Some(_), Some(t)) => (
                    Severity::Warning,
                    format!(
                        "the CPU template `{}` may hide them from the guest; confirm it masks every one",
                        clip(t)
                    ),
                ),
                (Some(_), None) => (
                    Severity::Error,
                    "the guest sees the host CPU, so it may already use them and fault after the move".into(),
                ),
                (None, _) => (
                    Severity::Warning,
                    "a VM that sees the host CPU may use them; pass --spec to check one VM".into(),
                ),
            };
            f.push(finding(
                sev,
                "cpu.flags",
                format!(
                    "the destination lacks {} CPU flag(s) the source has: {}; {tail}",
                    missing.len(),
                    list(&missing)
                ),
                Some("use a CPU template or a lower-common-denominator model, or choose a newer destination"),
            ));
        }
        let extra: BTreeSet<String> = dflags.difference(&sflags).cloned().collect();
        if !extra.is_empty() {
            f.push(finding(
                Severity::Info,
                "cpu.flags",
                format!(
                    "the destination has {} flag(s) the source lacks ({}); a guest that restarts there may see new features",
                    extra.len(),
                    list(&extra)
                ),
                None,
            ));
        }
    }
    let id = |h: &HostDescription| (h.cpu.family, h.cpu.model, h.cpu.stepping);
    if id(src) != id(dst) && id(src) != (None, None, None) && id(dst) != (None, None, None) {
        f.push(finding(
            Severity::Warning,
            "cpu.model",
            format!(
                "CPU family/model/stepping differ: {} to {} ({} to {}); micro-architecture differences are invisible to a flag comparison",
                fmt_id(id(src)),
                fmt_id(id(dst)),
                clip(&src.cpu.model_name),
                clip(&dst.cpu.model_name)
            ),
            None,
        ));
    }
    if let (Some(a), Some(b)) = (&src.cpu.microcode, &dst.cpu.microcode) {
        if a != b {
            f.push(finding(
                Severity::Info,
                "cpu.microcode",
                format!(
                    "microcode differs ({} to {}); host-passthrough guests can see it",
                    clip(a),
                    clip(b)
                ),
                None,
            ));
        }
    }
}

fn fmt_id((f, m, s): (Option<u32>, Option<u32>, Option<u32>)) -> String {
    let p = |v: Option<u32>| v.map_or("?".to_string(), |x| x.to_string());
    format!("{}/{}/{}", p(f), p(m), p(s))
}

fn backend_rules(
    src: &HostDescription,
    dst: &HostDescription,
    vm: Option<&VmNeeds>,
    f: &mut Vec<Finding>,
) {
    if src.backends.len() > MAX_BACKENDS || dst.backends.len() > MAX_BACKENDS {
        f.push(finding(
            Severity::Warning,
            "backends",
            format!("more than {MAX_BACKENDS} backends listed; only the known ones are compared"),
            None,
        ));
    }
    let wanted: Vec<(&str, BackendKind)> = match vm {
        Some(v) => match backend_key(v.backend) {
            Some(k) => vec![(k, v.backend)],
            None => {
                let sev = if v.backend == BackendKind::Vz {
                    Severity::Error
                } else {
                    Severity::Info
                };
                f.push(finding(
                    sev,
                    "backend",
                    format!(
                        "the {:?} backend has no live-migration rules in this check{}",
                        v.backend,
                        if v.backend == BackendKind::Vz {
                            "; Apple Virtualization.framework guests cannot be migrated between hosts"
                        } else {
                            ""
                        }
                    ),
                    None,
                ));
                return;
            }
        },
        None => known_backends()
            .into_iter()
            .filter(|(k, _)| src.backends.contains_key(*k))
            .collect(),
    };
    for (key, kind) in wanted {
        let fkey = format!("backends.{key}");
        let Some(dv_raw) = dst.backends.get(key) else {
            f.push(finding(
                Severity::Error,
                &fkey,
                format!("{key} is not installed on the destination"),
                Some("install it there, or choose another destination"),
            ));
            continue;
        };
        let Some(sv_raw) = src.backends.get(key) else {
            f.push(finding(
                Severity::Info,
                &fkey,
                format!(
                    "the source does not report a {key} version, so versions were not compared"
                ),
                None,
            ));
            continue;
        };
        let (Some(sv), Some(dv)) = (parse_version(sv_raw), parse_version(dv_raw)) else {
            f.push(finding(
                Severity::Warning,
                &fkey,
                format!(
                    "cannot read a version from `{}` or `{}`",
                    clip(sv_raw),
                    clip(dv_raw)
                ),
                None,
            ));
            continue;
        };
        version_rule(kind, key, &fkey, (&sv, sv_raw), (&dv, dv_raw), f);
        if kind == BackendKind::Firecracker {
            firecracker_rules(src, dst, vm, f);
        }
    }
}

fn version_rule(
    kind: BackendKind,
    key: &str,
    fkey: &str,
    (sv, sraw): (&[u64], &str),
    (dv, draw): (&[u64], &str),
    f: &mut Vec<Finding>,
) {
    let (s, d) = (clip(sraw), clip(draw));
    match kind {
        BackendKind::CloudHypervisor => {
            let (sm, dm) = (sv[0], dv[0]);
            if dm < sm {
                f.push(finding(
                    Severity::Error,
                    fkey,
                    format!("{key} {d} is older than {s}; backward migration is not supported"),
                    Some("upgrade the destination first"),
                ));
            } else if dm - sm > 2 {
                f.push(finding(
                    Severity::Error,
                    fkey,
                    format!(
                        "{key} {d} accepts migrations from the previous two releases only; {s} is {} behind",
                        dm - sm
                    ),
                    Some("upgrade the source one release at a time, or stop and start the VM"),
                ));
            } else if dm < 54 {
                f.push(finding(
                    Severity::Warning,
                    fkey,
                    format!("the n-2 migration guarantee starts at release 54; {d} is older"),
                    None,
                ));
            }
        }
        BackendKind::Qemu => {
            let (sm, dm) = (&sv[..sv.len().min(2)], &dv[..dv.len().min(2)]);
            if dm < sm {
                f.push(finding(
                    Severity::Warning,
                    fkey,
                    format!(
                        "{key} {d} is older than {s}; QEMU maintains forward compatibility, a backward migration may be refused by the destination (it also needs the saved machine type)"
                    ),
                    Some("prefer a destination with the same or a newer QEMU"),
                ));
            }
        }
        BackendKind::Firecracker => {
            let (sm, dm) = (&sv[..sv.len().min(2)], &dv[..dv.len().min(2)]);
            if dm < sm {
                f.push(finding(
                    Severity::Error,
                    fkey,
                    format!("{key} {d} is older than {s}; it cannot load a snapshot written by a newer format"),
                    Some("upgrade the destination first"),
                ));
            } else if dm > sm {
                f.push(finding(
                    Severity::Warning,
                    fkey,
                    format!(
                        "{key} {d} differs from {s}; each release accepts a fixed snapshot format version, check its compatibility table"
                    ),
                    None,
                ));
            }
        }
        _ => {}
    }
}

fn firecracker_rules(
    src: &HostDescription,
    dst: &HostDescription,
    vm: Option<&VmNeeds>,
    f: &mut Vec<Finding>,
) {
    let template = vm.and_then(|v| v.cpu_template.as_deref());
    let id = |h: &HostDescription| (h.cpu.family, h.cpu.model);
    if id(src) != id(dst) && id(src) != (None, None) && id(dst) != (None, None) {
        let (sev, tail) = match template {
            Some(t) => (
                Severity::Warning,
                format!("the CPU template `{}` is the only thing that can bridge them; confirm it covers both models", clip(t)),
            ),
            None => (
                Severity::Error,
                "snapshots must be restored on the same CPU micro-architecture unless a CPU template masks the difference".into(),
            ),
        };
        f.push(finding(
            sev,
            "backends.firecracker",
            format!("Firecracker snapshot across different CPU models: {tail}"),
            Some("set cpu_template (T2S is documented for Skylake and Cascade Lake) or restore on the same model"),
        ));
    }
    if let (Some(a), Some(b)) = (parse_version(&src.kernel), parse_version(&dst.kernel)) {
        if b < a {
            f.push(finding(
                Severity::Warning,
                "kernel",
                format!(
                    "host kernel {} is older than {}; Firecracker documents restoring a snapshot from a newer host kernel on an older one as unsupported",
                    clip(&dst.kernel),
                    clip(&src.kernel)
                ),
                None,
            ));
        }
    }
}

fn resource_rules(dst: &HostDescription, vm: &VmNeeds, f: &mut Vec<Finding>) {
    if let Some(free) = dst.free_memory_mib {
        if free < vm.memory_mib {
            f.push(finding(
                Severity::Error,
                "freeMemoryMib",
                format!(
                    "the destination has {free} MiB free but the VM needs {} MiB",
                    vm.memory_mib
                ),
                Some("free memory there or choose another destination"),
            ));
        }
    }
    let topo = dst.topology.as_ref();
    if vm.hugepages {
        match topo {
            Some(t) => {
                let free_mib: u64 = t
                    .numa_nodes
                    .iter()
                    .flat_map(|n| n.hugepages.iter())
                    .map(|p| p.free.saturating_mul(p.size_kib) / 1024)
                    .fold(0u64, u64::saturating_add);
                if free_mib < vm.memory_mib {
                    f.push(finding(
                        Severity::Error,
                        "topology.hugepages",
                        format!(
                            "the VM is backed by huge pages and needs {} MiB; the destination has {free_mib} MiB of free huge pages",
                            vm.memory_mib
                        ),
                        Some("reserve more huge pages on the destination (ideally at boot) first"),
                    ));
                }
            }
            None => f.push(finding(
                Severity::Info,
                "topology.hugepages",
                "the VM needs huge pages but the destination has no topology section, so they were not checked".into(),
                None,
            )),
        }
    }
    let mut nodes: Vec<u32> = vm
        .numa_node
        .into_iter()
        .chain(vm.numa_host_nodes.iter().copied())
        .collect();
    nodes.sort_unstable();
    nodes.dedup();
    if !nodes.is_empty() {
        match topo {
            Some(t) => {
                let have: BTreeSet<u32> = t.numa_nodes.iter().map(|n| n.id).collect();
                for n in nodes.into_iter().filter(|n| !have.contains(n)) {
                    f.push(finding(
                        Severity::Error,
                        "topology.numaNodes",
                        format!("the VM is bound to host NUMA node {n}, which the destination does not have"),
                        Some("rebind the VM to a node the destination has (`fluxctl place advise`)"),
                    ));
                }
            }
            None => f.push(finding(
                Severity::Info,
                "topology.numaNodes",
                "the VM is bound to host NUMA nodes but the destination has no topology section, so they were not checked".into(),
                None,
            )),
        }
    }
    if !vm.vfio_devices.is_empty() {
        if vm.backend == BackendKind::Firecracker {
            f.push(finding(
                Severity::Error,
                "vfio_devices",
                "Firecracker has no device passthrough; the spec cannot be right for this backend"
                    .into(),
                None,
            ));
        } else {
            f.push(finding(
                Severity::Warning,
                "vfio_devices",
                format!(
                    "{} passthrough device(s): only devices whose driver supports migration (for example some NIC virtual functions) can move live, and the destination needs the same device",
                    vm.vfio_devices.len()
                ),
                Some("detach the device before migrating, or use a virtual function with VFIO migration support"),
            ));
        }
        if let Some(t) = topo {
            for d in &vm.vfio_devices {
                match t.devices.iter().find(|x| x.bdf.eq_ignore_ascii_case(d)) {
                    None => f.push(finding(
                        Severity::Warning,
                        "vfio_devices",
                        format!("`{}` is not a device on the destination; the spec needs another address there", clip(d)),
                        None,
                    )),
                    Some(dev) if dev.allocated => f.push(finding(
                        Severity::Error,
                        "vfio_devices",
                        format!("`{}` on the destination is already assigned to a VM", clip(d)),
                        None,
                    )),
                    Some(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, vendor: &str, flags: &[&str]) -> HostDescription {
        HostDescription {
            name: name.into(),
            arch: "x86_64".into(),
            kernel: "6.1.0".into(),
            cpu: CpuDescription {
                vendor: vendor.into(),
                flags: flags.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            backends: BTreeMap::from([("qemu".to_string(), "9.1.0".to_string())]),
            ..Default::default()
        }
    }
    fn vm(backend: BackendKind) -> VmNeeds {
        VmNeeds {
            backend,
            vcpus: 2,
            memory_mib: 1024,
            cpu_template: None,
            hugepages: false,
            numa_node: None,
            numa_host_nodes: vec![],
            vfio_devices: vec![],
            cpuset: None,
        }
    }
    fn keys(r: &CompatReport, sev: Severity) -> Vec<String> {
        r.findings
            .iter()
            .filter(|x| x.severity == sev)
            .map(|x| x.key.clone())
            .collect()
    }

    #[test]
    fn identical_hosts_are_compatible() {
        let a = host("a", "GenuineIntel", &["avx2", "sse4_2"]);
        let r = check(&a, &a.clone(), None);
        assert_eq!(r.verdict, Verdict::Compatible);
        assert!(r.findings.is_empty());
    }

    #[test]
    fn vendor_and_arch_mismatch_are_errors() {
        let a = host("a", "GenuineIntel", &["avx2"]);
        let b = host("b", "AuthenticAMD", &["avx2"]);
        let r = check(&a, &b, None);
        assert_eq!(r.verdict, Verdict::Incompatible);
        assert_eq!(keys(&r, Severity::Error), vec!["cpu.vendor"]);
        let mut c = b.clone();
        c.arch = "aarch64".into();
        let r = check(&a, &c, None);
        assert_eq!(keys(&r, Severity::Error), vec!["arch"]);
    }

    #[test]
    fn missing_flags_are_error_for_a_vm_and_warning_without() {
        let a = host("a", "GenuineIntel", &["avx2", "avx512f"]);
        let b = host("b", "GenuineIntel", &["avx2"]);
        let r = check(&a, &b, None);
        assert_eq!(r.verdict, Verdict::CompatibleWithWarnings);
        let r = check(&a, &b, Some(&vm(BackendKind::Qemu)));
        assert_eq!(r.verdict, Verdict::Incompatible);
        assert!(r.findings[0].message.contains("avx512f"));
        let mut v = vm(BackendKind::Qemu);
        v.cpu_template = Some("T2".into());
        assert_eq!(
            check(&a, &b, Some(&v)).verdict,
            Verdict::CompatibleWithWarnings
        );
    }

    #[test]
    fn extra_destination_flags_are_only_a_note() {
        let a = host("a", "GenuineIntel", &["avx2"]);
        let b = host("b", "GenuineIntel", &["avx2", "avx512f"]);
        assert_eq!(
            check(&a, &b, Some(&vm(BackendKind::Qemu))).verdict,
            Verdict::Compatible
        );
    }

    #[test]
    fn cloud_hypervisor_follows_the_n_minus_2_rule() {
        let mut a = host("a", "GenuineIntel", &["avx2"]);
        let mut b = a.clone();
        a.backends.insert("cloud-hypervisor".into(), "v52.0".into());
        let mut go = |dst: &str| {
            b.backends.insert("cloud-hypervisor".into(), dst.into());
            check(&a, &b, Some(&vm(BackendKind::CloudHypervisor))).verdict
        };
        assert_eq!(go("v54.0"), Verdict::Compatible);
        assert_eq!(go("v55.1"), Verdict::Incompatible);
        assert_eq!(go("v51.0"), Verdict::Incompatible);
    }

    #[test]
    fn missing_backend_on_destination_is_an_error() {
        let mut a = host("a", "GenuineIntel", &[]);
        a.backends.insert("qemu".into(), "9.1.0".into());
        let mut b = host("b", "GenuineIntel", &[]);
        b.backends.clear();
        let r = check(&a, &b, None);
        assert_eq!(keys(&r, Severity::Error), vec!["backends.qemu"]);
    }

    #[test]
    fn qemu_backward_is_a_warning() {
        let mut a = host("a", "GenuineIntel", &[]);
        let mut b = a.clone();
        a.backends.insert("qemu".into(), "9.1.0".into());
        b.backends.insert("qemu".into(), "8.2.2".into());
        let r = check(&a, &b, Some(&vm(BackendKind::Qemu)));
        assert_eq!(r.verdict, Verdict::CompatibleWithWarnings);
    }

    #[test]
    fn firecracker_needs_same_microarchitecture_or_template() {
        let mut a = host("a", "GenuineIntel", &[]);
        a.cpu.family = Some(6);
        a.cpu.model = Some(85);
        let mut b = a.clone();
        b.cpu.model = Some(106);
        for h in [&mut a, &mut b] {
            h.backends.insert("firecracker".into(), "1.10.1".into());
        }
        let r = check(&a, &b, Some(&vm(BackendKind::Firecracker)));
        assert_eq!(r.verdict, Verdict::Incompatible);
        let mut v = vm(BackendKind::Firecracker);
        v.cpu_template = Some("T2S".into());
        let r = check(&a, &b, Some(&v));
        assert!(!keys(&r, Severity::Error).contains(&"backends.firecracker".to_string()));
    }

    #[test]
    fn vz_is_never_migratable() {
        let a = host("a", "Apple", &[]);
        let r = check(&a, &a.clone(), Some(&vm(BackendKind::Vz)));
        assert_eq!(r.verdict, Verdict::Incompatible);
    }

    #[test]
    fn destination_memory_and_hugepages_are_checked() {
        use crate::topology::{HugepagePool, NumaNodeInfo};
        let a = host("a", "GenuineIntel", &[]);
        let mut b = a.clone();
        b.free_memory_mib = Some(512);
        b.topology = Some(HostTopology {
            numa_nodes: vec![NumaNodeInfo {
                id: 0,
                hugepages: vec![HugepagePool {
                    size_kib: 2048,
                    total: 100,
                    free: 100,
                }],
                ..Default::default()
            }],
            devices: vec![],
        });
        let mut v = vm(BackendKind::Qemu);
        v.hugepages = true;
        v.numa_node = Some(1);
        let r = check(&a, &b, Some(&v));
        let e = keys(&r, Severity::Error);
        assert!(e.contains(&"freeMemoryMib".to_string()));
        assert!(e.contains(&"topology.hugepages".to_string()));
        assert!(e.contains(&"topology.numaNodes".to_string()));
    }

    #[test]
    fn passthrough_warns_and_firecracker_rejects() {
        let a = host("a", "GenuineIntel", &[]);
        let mut v = vm(BackendKind::Qemu);
        v.vfio_devices = vec!["0000:01:00.0".into()];
        assert_eq!(
            check(&a, &a.clone(), Some(&v)).verdict,
            Verdict::CompatibleWithWarnings
        );
        v.backend = BackendKind::Firecracker;
        assert_eq!(
            check(&a, &a.clone(), Some(&v)).verdict,
            Verdict::Incompatible
        );
    }

    #[test]
    fn hostile_input_is_bounded() {
        let mut a = host("a\u{1b}[31m", "GenuineIntel", &[]);
        a.cpu.flags = (0..MAX_FLAGS * 3).map(|i| format!("f{i}")).collect();
        a.cpu.flags.push("x".repeat(10_000));
        a.cpu.flags.push("bad flag\n".into());
        let b = host(&"n".repeat(100_000), "GenuineIntel", &["z"]);
        let r = check(&a, &b, None);
        assert!(r.destination.chars().count() <= MAX_STR);
        assert!(!r.source.contains('\u{1b}'));
        for x in &r.findings {
            assert!(x.message.chars().count() < 2000, "{}", x.message.len());
        }
    }

    #[test]
    fn parse_version_forms() {
        assert_eq!(parse_version("v54.0"), Some(vec![54, 0]));
        assert_eq!(parse_version("9.1.0-rc1"), Some(vec![9, 1, 0]));
        assert_eq!(parse_version("QEMU 9"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version(&"9".repeat(40)), None);
    }

    #[test]
    fn parse_host_and_spec() {
        assert!(parse_host("[]").is_err());
        assert!(parse_host("{\"name\":\"x\",\"cpu\":{\"flags\":[\"a\"]}}").is_ok());
        let v = VmNeeds::from_spec_str(
            r#"{"name":"t","backend":"qemu","image":"/a.qcow2","vcpus":4,"memory_mib":2048,"hugepages":true,"numa_node":1}"#,
        )
        .unwrap();
        assert_eq!(
            (v.vcpus, v.memory_mib, v.numa_node, v.hugepages),
            (4, 2048, Some(1), true)
        );
        assert!(VmNeeds::from_spec_str("{").is_err());
        assert!(VmNeeds::from_spec_str("[]").is_err());
    }
}
