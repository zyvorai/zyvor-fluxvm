// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Static checks of a VM spec (the JSON body of `fluxctl create --spec` and `POST /v1/vms`), for
//! `fluxctl vm-spec check`.
//!
//! `CreateVmRequest` is parsed leniently: unknown keys are ignored, most fields have defaults, and many settings
//! only apply to one backend and are silently dropped on the others. The daemon enforces some rules at create time
//! (balloon, NUMA, VFIO, security profile) and others only once the VMM fails to start. This module reports what it
//! can know from the file alone, each finding with the key it concerns and a fix. It never changes the spec and
//! never starts anything. The create-time rules are the ones in `model.rs` (`validate_balloon`, `validate_numa`,
//! `validate_vfio_devices`), called here, so they cannot drift.
//!
//! Severity is the one of `fluxvm_core::config_lint`: `Error` = the daemon refuses it or the VM cannot boot;
//! `Warning` = legal but probably not what was meant (a setting that the chosen backend ignores); `Info` = a note.
//! Checks that look at the host (does the image exist) go through [`SpecEnv::exists`] and are skipped when it is
//! `None` (`--offline`, for a spec written on another machine).

use crate::config_lint::{Finding, Severity, edit_distance};
use crate::model::{BackendKind, CreateVmRequest, NetworkSpec};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// The top-level keys of `CreateVmRequest`. A test reads `model.rs` and fails when this list and the struct differ.
pub const KNOWN_KEYS: &[&str] = &[
    "agent",
    "apple",
    "backend",
    "balloon",
    "blk_mbit_limit",
    "blk_ops_limit",
    "cdroms",
    "cloud_init",
    "cpu_template",
    "cpuset",
    "created_by_token",
    "data_disks",
    "disk_hotplug_ports",
    "disk_size_gib",
    "egress",
    "extra_args",
    "firmware",
    "hugepages",
    "hyperv",
    "image",
    "initrd",
    "kernel",
    "kernel_args",
    "loadvm_tag",
    "max_memory_mib",
    "max_vcpus",
    "measurement_policy",
    "memory_mib",
    "name",
    "net_mbit_limit",
    "net_pps_limit",
    "network",
    "numa",
    "numa_node",
    "pod_uid",
    "qga",
    "root_disk_io",
    "secure_boot",
    "security_profile",
    "shared_folders",
    "shared_memory",
    "storage",
    "tenant",
    "tpm",
    "ttl_seconds",
    "vcpus",
    "vfio_devices",
];

/// What the checks may ask about the machine they run on.
pub struct SpecEnv<'a> {
    /// `None` skips every "does this path exist" check.
    pub exists: Option<&'a dyn Fn(&Path) -> bool>,
}

impl SpecEnv<'_> {
    /// No host checks.
    pub const OFFLINE: SpecEnv<'static> = SpecEnv { exists: None };
}

/// Lints the text of a spec file. A file that is not JSON, or not a VM spec at all, is one `(file)` error.
pub fn lint_str(raw: &str, env: &SpecEnv<'_>) -> Vec<Finding> {
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => return vec![err("(file)", format!("not valid JSON: {e}"), None)],
    };
    let spec: CreateVmRequest = match serde_json::from_value(value.clone()) {
        Ok(s) => s,
        Err(e) => {
            let mut msg = format!("not a VM spec: {e}");
            let fix = if value.is_object() {
                None
            } else {
                msg = "not a VM spec: the file must hold one JSON object".into();
                Some("start from examples/fluxvm.json or `fluxctl create --help`".into())
            };
            return vec![err("(file)", msg, fix)];
        }
    };
    let mut out = unknown_keys(&value);
    out.extend(lint_spec(&spec, env));
    out
}

fn err(key: &str, message: impl Into<String>, fix: Option<String>) -> Finding {
    finding(Severity::Error, key, message, fix)
}
fn warn(key: &str, message: impl Into<String>, fix: Option<String>) -> Finding {
    finding(Severity::Warning, key, message, fix)
}
fn info(key: &str, message: impl Into<String>) -> Finding {
    finding(Severity::Info, key, message, None)
}
fn finding(
    severity: Severity,
    key: &str,
    message: impl Into<String>,
    fix: Option<String>,
) -> Finding {
    Finding {
        severity,
        key: key.to_string(),
        message: message.into(),
        fix,
    }
}

/// Top-level keys the daemon would ignore, with the closest real key when one is near.
pub fn unknown_keys(value: &Value) -> Vec<Finding> {
    let Some(map) = value.as_object() else {
        return Vec::new();
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    keys.into_iter()
        .filter(|k| !KNOWN_KEYS.contains(&k.as_str()))
        .map(|k| {
            let near = KNOWN_KEYS
                .iter()
                .map(|c| (edit_distance(k, c), *c))
                .filter(|(d, c)| *d <= 2.max(c.len() / 4))
                .min_by_key(|(d, _)| *d);
            match near {
                Some((_, c)) => warn(
                    k,
                    format!("unknown key, ignored by the daemon (did you mean `{c}`?)"),
                    Some(format!("rename it to `{c}`")),
                ),
                None => warn(
                    k,
                    "unknown key, ignored by the daemon",
                    Some("remove it, or check the spelling against `docs/api.md`".into()),
                ),
            }
        })
        .collect()
}

/// The checks that need a parsed spec.
pub fn lint_spec(s: &CreateVmRequest, env: &SpecEnv<'_>) -> Vec<Finding> {
    let mut out = Vec::new();
    let qemu = matches!(s.backend, BackendKind::Qemu);
    let firecracker = matches!(s.backend, BackendKind::Firecracker);
    let ch = matches!(s.backend, BackendKind::CloudHypervisor);
    let backend = backend_name(s.backend);

    if s.name.trim().is_empty() {
        out.push(err(
            "name",
            "the name is empty",
            Some("give the VM a name".into()),
        ));
    }
    if matches!(s.backend, BackendKind::Auto) {
        out.push(info(
            "backend",
            "`auto` is resolved by the daemon when the VM is created; backend-specific checks below assume a Linux QEMU-like feature set, so name the backend to get them",
        ));
    }

    // CPU and memory.
    if s.vcpus == 0 {
        out.push(err(
            "vcpus",
            "0 vCPUs",
            Some("set vcpus to at least 1".into()),
        ));
    }
    if s.memory_mib < 128 {
        out.push(warn(
            "memory_mib",
            format!("{} MiB is too little for most guests to boot", s.memory_mib),
            Some("set memory_mib to 512 or more".into()),
        ));
    }
    if let Some(m) = s.max_vcpus
        && m < s.vcpus
    {
        out.push(err(
            "max_vcpus",
            format!("max_vcpus ({m}) is below vcpus ({})", s.vcpus),
            Some(format!("set max_vcpus to {} or more", s.vcpus)),
        ));
    }
    if let Some(m) = s.max_memory_mib
        && m < s.memory_mib
    {
        out.push(err(
            "max_memory_mib",
            format!(
                "max_memory_mib ({m}) is below memory_mib ({})",
                s.memory_mib
            ),
            Some(format!("set max_memory_mib to {} or more", s.memory_mib)),
        ));
    }
    if s.ttl_seconds == Some(0) {
        out.push(warn(
            "ttl_seconds",
            "ttl_seconds 0 means the VM expires immediately (or is treated as unset, depending on the route)",
            Some("remove it for a VM that lives until deleted, or set the lifetime in seconds".into()),
        ));
    }

    // The create-time rules of the model, with their own messages.
    if let Err(e) = s.validate_balloon() {
        out.push(err("balloon", format!("{e:#}"), None));
    }
    if let Err(e) = s.validate_vfio_devices() {
        out.push(err("vfio_devices", format!("{e:#}"), None));
    }
    if let Err(e) = s.validate_numa() {
        out.push(err("numa", format!("{e:#}"), None));
    }

    // Backend-specific settings that the chosen backend drops or refuses.
    let qemu_only = |key: &str, set: bool, out: &mut Vec<Finding>| {
        if set && !qemu && !matches!(s.backend, BackendKind::Auto) {
            out.push(warn(
                key,
                format!("only the qemu backend uses `{key}`; the {backend} backend ignores it"),
                Some("remove it, or set backend to qemu".into()),
            ));
        }
    };
    qemu_only("numa_node", s.numa_node.is_some(), &mut out);
    qemu_only("cpuset", s.cpuset.is_some(), &mut out);
    qemu_only("hugepages", s.hugepages == Some(true), &mut out);
    qemu_only("vfio_devices", !s.vfio_devices.is_empty(), &mut out);
    qemu_only("numa", !s.numa.is_empty(), &mut out);
    qemu_only("balloon", s.balloon.is_some(), &mut out);
    qemu_only("data_disks", !s.data_disks.is_empty(), &mut out);
    qemu_only("cdroms", !s.cdroms.is_empty(), &mut out);
    qemu_only("root_disk_io", s.root_disk_io.is_some(), &mut out);
    qemu_only(
        "disk_hotplug_ports",
        s.disk_hotplug_ports.is_some(),
        &mut out,
    );
    qemu_only("secure_boot", s.secure_boot == Some(true), &mut out);
    qemu_only("tpm", s.tpm == Some(true), &mut out);

    if !firecracker && !matches!(s.backend, BackendKind::Auto) {
        for (key, set) in [
            ("net_mbit_limit", s.net_mbit_limit.is_some()),
            ("net_pps_limit", s.net_pps_limit.is_some()),
            ("blk_mbit_limit", s.blk_mbit_limit.is_some()),
            ("blk_ops_limit", s.blk_ops_limit.is_some()),
        ] {
            if set {
                out.push(warn(
                    key,
                    format!("a Firecracker rate limit; the {backend} backend ignores it"),
                    Some("remove it, or set backend to firecracker".into()),
                ));
            }
        }
        if s.cpu_template.is_some() {
            out.push(err(
                "cpu_template",
                format!(
                    "cpu_template is a Firecracker setting and the {backend} backend rejects it"
                ),
                Some("remove it, or set backend to firecracker".into()),
            ));
        }
    }
    if s.hyperv && !ch && !matches!(s.backend, BackendKind::Auto) {
        out.push(warn(
            "hyperv",
            format!("`hyperv` is a Cloud Hypervisor setting; the {backend} backend ignores it"),
            Some("remove it, or set backend to cloud-hypervisor".into()),
        ));
    }
    if s.secure_boot == Some(true) && s.firmware.is_none() {
        out.push(err(
            "secure_boot",
            "secure_boot needs `firmware` (an OVMF image) as well",
            Some("set firmware to the OVMF code image".into()),
        ));
    }
    if s.security_profile != crate::security::SecurityProfile::Standard
        && !qemu
        && !matches!(s.backend, BackendKind::Auto)
    {
        out.push(err(
            "security_profile",
            format!(
                "security_profile {} requires the qemu backend; {backend} cannot provide the measured/confidential launch path",
                s.security_profile.as_str()
            ),
            Some("set backend to qemu, or security_profile to standard".into()),
        ));
    }
    if s.security_profile.is_confidential() {
        if !s.extra_args.is_empty() {
            out.push(err(
                "extra_args",
                format!(
                    "extra_args is refused under security_profile {}",
                    s.security_profile.as_str()
                ),
                Some("remove extra_args".into()),
            ));
        }
        if s.shared_memory {
            out.push(err(
                "shared_memory",
                format!(
                    "shared_memory is refused under security_profile {}",
                    s.security_profile.as_str()
                ),
                Some("remove shared_memory".into()),
            ));
        }
        if s.hugepages == Some(true) {
            out.push(err(
                "hugepages",
                format!(
                    "hugepages is refused under security_profile {}",
                    s.security_profile.as_str()
                ),
                Some("remove hugepages".into()),
            ));
        }
    } else if !s.extra_args.is_empty() {
        out.push(info(
            "extra_args",
            "extra_args go to the VMM unchecked; a typo here fails only when the VM starts",
        ));
    }
    if s.measurement_policy.is_some()
        && s.security_profile == crate::security::SecurityProfile::Standard
    {
        out.push(warn(
            "measurement_policy",
            "a measurement policy has no effect under security_profile standard",
            Some("set security_profile to measured or a confidential profile".into()),
        ));
    }
    if s.kernel.is_some() && s.firmware.is_some() {
        out.push(warn(
            "firmware",
            "both `kernel` (direct kernel boot) and `firmware` are set; direct boot does not run firmware",
            Some("remove one of them".into()),
        ));
    }
    if s.kernel_args.is_some() && s.kernel.is_none() && firecracker {
        out.push(err(
            "kernel_args",
            "Firecracker boots a kernel directly; kernel_args without `kernel` has nothing to apply to",
            Some("set kernel to a vmlinux".into()),
        ));
    }
    if firecracker && s.kernel.is_none() {
        out.push(err(
            "kernel",
            "the firecracker backend needs `kernel` (a vmlinux), it has no firmware boot",
            Some("set kernel, see examples/firecracker.json".into()),
        ));
    }

    // Networking.
    match &s.network {
        NetworkSpec::None => out.push(info(
            "network",
            "mode none: the guest has no network device",
        )),
        NetworkSpec::User { forwards } => {
            let mut seen = BTreeSet::new();
            for (i, f) in forwards.iter().enumerate() {
                let key = format!("network.forwards[{i}]");
                if f.host_port == 0 || f.guest_port == 0 {
                    out.push(err(
                        &key,
                        "port 0 is not a forwardable port",
                        Some("use a port from 1 to 65535".into()),
                    ));
                }
                let proto = f.protocol.to_ascii_lowercase();
                if proto != "tcp" && proto != "udp" {
                    out.push(err(
                        &key,
                        format!("protocol {:?} is not tcp or udp", f.protocol),
                        Some("use \"tcp\" or \"udp\"".into()),
                    ));
                }
                if !seen.insert((f.host_port, proto)) {
                    out.push(err(
                        &key,
                        format!(
                            "host port {} is forwarded twice with the same protocol",
                            f.host_port
                        ),
                        Some("give each forward its own host_port".into()),
                    ));
                }
                if f.host_port != 0 && f.host_port < 1024 {
                    out.push(warn(
                        &key,
                        format!("host port {} is privileged; the daemon needs root or CAP_NET_BIND_SERVICE to bind it", f.host_port),
                        Some("use a host port of 1024 or more".into()),
                    ));
                }
                if qemu && f.host_addr.is_none() && f.host_port != 0 {
                    out.push(info(
                        &key,
                        "no host_addr: the forward listens on 0.0.0.0, reachable from outside the host",
                    ));
                }
            }
        }
        NetworkSpec::Tap { .. } | NetworkSpec::Macvtap { .. } => {}
    }

    // Shared folders and the data the VM needs from the host.
    let mut tags = BTreeSet::new();
    for (i, f) in s.shared_folders.iter().enumerate() {
        let key = format!("shared_folders[{i}]");
        if !f.guest_path.starts_with('/') {
            out.push(err(
                &key,
                format!("guest_path {:?} is not absolute", f.guest_path),
                Some("start it with /".into()),
            ));
        }
        if !tags.insert(f.guest_path.clone()) {
            out.push(err(
                &key,
                format!(
                    "guest_path {:?} is used by two shared folders",
                    f.guest_path
                ),
                None,
            ));
        }
        if let Some(exists) = env.exists
            && !exists(&f.host_path)
        {
            out.push(warn(
                &key,
                format!(
                    "host_path {} does not exist on this machine",
                    f.host_path.display()
                ),
                None,
            ));
        }
    }
    if let Some(exists) = env.exists {
        for (key, path) in [
            ("image", Some(&s.image)),
            ("kernel", s.kernel.as_ref()),
            ("initrd", s.initrd.as_ref()),
            ("firmware", s.firmware.as_ref()),
        ] {
            if let Some(p) = path
                && !exists(p)
            {
                out.push(warn(
                    key,
                    format!("{} does not exist on this machine", p.display()),
                    Some(
                        "check the path, or run with --offline when the spec is for another host"
                            .into(),
                    ),
                ));
            }
        }
    } else if s.image.is_relative() {
        out.push(info(
            "image",
            "a relative path is resolved against the daemon's working directory, not yours",
        ));
    }
    if s.image.as_os_str().is_empty() {
        out.push(err(
            "image",
            "the image path is empty",
            Some("set image to a disk image path".into()),
        ));
    }
    if s.created_by_token.is_some() {
        out.push(info(
            "created_by_token",
            "the server overwrites this with the caller's identity; remove it",
        ));
    }

    out.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.key.cmp(&b.key)));
    out
}

fn backend_name(b: BackendKind) -> &'static str {
    match b {
        BackendKind::Qemu => "qemu",
        BackendKind::CloudHypervisor => "cloud-hypervisor",
        BackendKind::Firecracker => "firecracker",
        BackendKind::FluxVm => "fluxvm",
        BackendKind::Vz => "vz",
        BackendKind::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OFFLINE: SpecEnv<'static> = SpecEnv::OFFLINE;

    fn lint(v: Value) -> Vec<Finding> {
        lint_str(&v.to_string(), &OFFLINE)
    }
    fn has(f: &[Finding], sev: Severity, key: &str) -> bool {
        f.iter().any(|x| x.severity == sev && x.key == key)
    }
    fn base() -> Value {
        json!({"name": "t", "backend": "qemu", "image": "/var/lib/fluxvm/images/a.qcow2"})
    }
    fn with(mut v: Value, k: &str, x: Value) -> Value {
        v[k] = x;
        v
    }

    #[test]
    fn a_plain_spec_has_no_errors_or_warnings() {
        let f = lint(base());
        assert!(f.iter().all(|x| x.severity == Severity::Info), "{f:?}");
    }

    #[test]
    fn bad_json_and_non_specs_are_one_file_error() {
        let f = lint_str("{nope", &OFFLINE);
        assert_eq!(f.len(), 1);
        assert!(f[0].key == "(file)" && f[0].message.contains("not valid JSON"));
        let f = lint(json!({"name": "x"}));
        assert!(f[0].message.contains("not a VM spec"), "{f:?}");
        let f = lint(json!([1]));
        assert!(f[0].fix.is_some());
    }

    #[test]
    fn unknown_keys_get_a_suggestion_and_sorted_order() {
        let mut v = base();
        v["memory_mb"] = json!(1024);
        v["vcpu"] = json!(2);
        v["zzz_made_up_setting"] = json!(1);
        let f = unknown_keys(&v);
        let keys: Vec<&str> = f.iter().map(|x| x.key.as_str()).collect();
        assert_eq!(keys, ["memory_mb", "vcpu", "zzz_made_up_setting"]);
        assert!(f[0].message.contains("did you mean `memory_mib`"));
        assert!(f[1].message.contains("did you mean `vcpus`"));
        assert!(!f[2].message.contains("did you mean"));
    }

    #[test]
    fn cpu_and_memory_bounds() {
        let f = lint(with(with(base(), "vcpus", json!(4)), "max_vcpus", json!(2)));
        assert!(has(&f, Severity::Error, "max_vcpus"));
        let f = lint(with(
            with(base(), "memory_mib", json!(4096)),
            "max_memory_mib",
            json!(1024),
        ));
        assert!(has(&f, Severity::Error, "max_memory_mib"));
        assert!(has(
            &lint(with(base(), "vcpus", json!(0))),
            Severity::Error,
            "vcpus"
        ));
        assert!(has(
            &lint(with(base(), "memory_mib", json!(64))),
            Severity::Warning,
            "memory_mib"
        ));
    }

    #[test]
    fn model_rules_are_reported_under_their_key() {
        let f = lint(with(
            with(base(), "balloon", json!({})),
            "hugepages",
            json!(true),
        ));
        assert!(has(&f, Severity::Error, "balloon"), "{f:?}");
        let f = lint(with(base(), "vfio_devices", json!(["not-a-device"])));
        assert!(has(&f, Severity::Error, "vfio_devices"), "{f:?}");
        let f = lint(with(
            base(),
            "numa",
            json!([{"host_node": 0, "memory_mib": 1024, "vcpus": [0]}]),
        ));
        assert!(has(&f, Severity::Error, "numa"), "{f:?}");
    }

    #[test]
    fn settings_of_another_backend_are_flagged() {
        let mut v = base();
        v["backend"] = json!("firecracker");
        v["kernel"] = json!("/k/vmlinux");
        v["numa_node"] = json!(0);
        v["tpm"] = json!(true);
        let f = lint(v);
        assert!(has(&f, Severity::Warning, "numa_node"));
        assert!(has(&f, Severity::Warning, "tpm"));
        let f = lint(with(
            with(base(), "net_mbit_limit", json!(100)),
            "cpu_template",
            json!("T2"),
        ));
        assert!(has(&f, Severity::Warning, "net_mbit_limit"));
        assert!(has(&f, Severity::Error, "cpu_template"));
        // `auto` is resolved later: nothing is claimed about it.
        let f = lint(with(
            with(base(), "backend", json!("auto")),
            "numa_node",
            json!(0),
        ));
        assert!(!has(&f, Severity::Warning, "numa_node"));
    }

    #[test]
    fn firecracker_needs_a_kernel() {
        let f = lint(with(base(), "backend", json!("firecracker")));
        assert!(has(&f, Severity::Error, "kernel"));
    }

    #[test]
    fn secure_boot_and_security_profile() {
        assert!(has(
            &lint(with(base(), "secure_boot", json!(true))),
            Severity::Error,
            "secure_boot"
        ));
        let mut v = with(base(), "backend", json!("cloud-hypervisor"));
        v["security_profile"] = json!("measured");
        assert!(has(&lint(v), Severity::Error, "security_profile"));
        let mut v = base();
        v["security_profile"] = json!("confidential-snp");
        v["extra_args"] = json!(["-s"]);
        v["hugepages"] = json!(true);
        let f = lint(v);
        assert!(has(&f, Severity::Error, "extra_args"));
        assert!(has(&f, Severity::Error, "hugepages"));
    }

    #[test]
    fn port_forwards() {
        let v = with(
            base(),
            "network",
            json!({"mode": "user", "forwards": [
                {"host_port": 8080, "guest_port": 80},
                {"host_port": 8080, "guest_port": 81},
                {"host_port": 8080, "guest_port": 82, "protocol": "udp"},
                {"host_port": 22, "guest_port": 22},
                {"host_port": 9, "guest_port": 0, "protocol": "sctp"}
            ]}),
        );
        let f = lint(v);
        let at = |k: &str, sev| f.iter().filter(|x| x.key == k && x.severity == sev).count();
        assert_eq!(
            at("network.forwards[1]", Severity::Error),
            1,
            "duplicate tcp"
        );
        assert_eq!(
            at("network.forwards[2]", Severity::Error),
            0,
            "udp is a different socket"
        );
        assert_eq!(
            at("network.forwards[3]", Severity::Warning),
            1,
            "privileged"
        );
        assert_eq!(
            at("network.forwards[4]", Severity::Error),
            2,
            "port 0 and protocol"
        );
    }

    #[test]
    fn host_paths_are_checked_only_when_asked() {
        let mut v = base();
        v["shared_folders"] = json!([{"host_path": "/nope/src", "guest_path": "mnt"}]);
        let f = lint(v.clone());
        assert!(has(&f, Severity::Error, "shared_folders[0]"));
        assert!(!f.iter().any(|x| x.message.contains("does not exist")));
        let none = |_: &Path| false;
        let f = lint_str(
            &v.to_string(),
            &SpecEnv {
                exists: Some(&none),
            },
        );
        assert!(has(&f, Severity::Warning, "image"));
        assert!(
            f.iter()
                .filter(|x| x.message.contains("does not exist"))
                .count()
                >= 2
        );
        let all = |_: &Path| true;
        let f = lint_str(&base().to_string(), &SpecEnv { exists: Some(&all) });
        assert!(f.iter().all(|x| x.severity == Severity::Info));
    }

    #[test]
    fn findings_are_sorted_worst_first() {
        let f = lint(with(
            with(base(), "vcpus", json!(0)),
            "memory_mib",
            json!(64),
        ));
        assert_eq!(f[0].severity, Severity::Error);
        assert!(f.windows(2).all(|w| w[0].severity >= w[1].severity));
    }

    /// `KNOWN_KEYS` is the field list of `CreateVmRequest`, read from the source so a new field cannot be missed.
    #[test]
    fn known_keys_match_the_struct() {
        let src = include_str!("model.rs");
        let start = src.find("pub struct CreateVmRequest {").unwrap();
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let mut fields: Vec<&str> = body
            .lines()
            .filter_map(|l| l.strip_prefix("    pub "))
            .filter_map(|l| l.split(':').next())
            .collect();
        fields.sort_unstable();
        assert_eq!(fields, KNOWN_KEYS, "update spec_lint::KNOWN_KEYS");
    }

    #[test]
    fn shipped_examples_that_are_specs_have_no_errors() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if serde_json::from_value::<CreateVmRequest>(v).is_err() {
                continue;
            }
            checked += 1;
            let f = lint_str(&text, &OFFLINE);
            let errors: Vec<_> = f.iter().filter(|x| x.severity == Severity::Error).collect();
            assert!(errors.is_empty(), "{}: {errors:?}", p.display());
        }
        assert!(checked >= 3, "only {checked} example specs found");
    }
}
