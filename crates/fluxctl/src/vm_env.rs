// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! VM environments: the `fluxctl templates` entries that are a ready-made virtual machine rather than a Compose file or
//! k3s manifests. Each is a small TOML file in `crates/fluxctl/vm-templates/` (or one the user wrote): an image, a size,
//! packages, first-boot commands and the readiness checks that say the machine is usable. This file holds the format and
//! turns a definition into a VM request; `vm_env_plan` judges what would happen and `vm_env_up` does it.

use crate::vm_net::Backend;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Written into the guest and kept in the VM's request: how a later run knows the VM is this environment's. It is part
/// of the create request, so it exists from the moment the VM record does (labels are only set afterwards).
pub const MARKER_PATH: &str = "/etc/fluxvm-template";
pub const L_TEMPLATE: &str = "fluxvm.template";
pub const L_HASH: &str = "fluxvm.template-hash";

const BUILTIN: &[(&str, &str)] = &[
    ("debian", include_str!("../vm-templates/debian.toml")),
    ("ubuntu", include_str!("../vm-templates/ubuntu.toml")),
    ("build", include_str!("../vm-templates/build.toml")),
    ("amd64", include_str!("../vm-templates/amd64.toml")),
    ("kvm", include_str!("../vm-templates/kvm.toml")),
];

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    /// Run in the guest with `sh`; the check passes when it exits 0.
    pub command: String,
}

/// What differs between guest CPU architectures; appended to the common lists.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ArchPart {
    #[serde(default)]
    pub packages: Vec<String>,
    #[serde(default, rename = "check")]
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvTemplate {
    pub name: String,
    pub title: String,
    pub summary: String,
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u64,
    pub disk_gib: u64,
    #[serde(default)]
    pub packages: Vec<String>,
    /// Commands cloud-init runs as root on first boot, after the packages. `{user}` is the guest login.
    #[serde(default)]
    pub run: Vec<String>,
    /// Run x86-64 programs through Apple's Rosetta for Linux (`vz` only).
    #[serde(default)]
    pub rosetta: bool,
    #[serde(default)]
    pub nested_virtualization: bool,
    #[serde(default, rename = "check")]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub arch: BTreeMap<String, ArchPart>,
}

impl EnvTemplate {
    pub fn packages_for(&self, arch: &str) -> Vec<String> {
        let mut p = self.packages.clone();
        if let Some(a) = self.arch.get(arch) {
            p.extend(a.packages.iter().cloned());
        }
        p
    }

    pub fn checks_for(&self, arch: &str, user: &str) -> Vec<Check> {
        self.checks
            .iter()
            .chain(self.arch.get(arch).into_iter().flat_map(|a| &a.checks))
            .map(|c| Check {
                name: c.name.clone(),
                command: c.command.replace("{user}", user),
            })
            .collect()
    }

    /// Why this environment cannot exist on `backend`, in words a user can act on.
    pub fn unsupported_on(&self, backend: Backend, guest_arch: &str) -> Option<String> {
        if self.rosetta && !backend.has_rosetta() {
            return Some(format!(
                "it uses Apple's Rosetta for Linux, which only exists in the macOS (vz) backend; this host uses {}.{}",
                backend.name(),
                if guest_arch == "x86_64" {
                    " Guests here are already x86-64: use `build`"
                } else {
                    ""
                }
            ));
        }
        None
    }
}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn check_vm_name(s: &str) -> Result<()> {
    if !valid_name(s) {
        bail!(
            "VM name {s:?} must be 1-63 characters of a-z, 0-9 and '-', not starting or ending with '-'"
        );
    }
    Ok(())
}

pub fn parse(text: &str) -> Result<EnvTemplate> {
    let t: EnvTemplate = toml::from_str(text).context("reading the VM template")?;
    if !valid_name(&t.name) {
        bail!(
            "template name {:?} must be 1-63 characters of a-z, 0-9 and '-'",
            t.name
        );
    }
    if t.cpus == 0 || t.memory_mib < 256 || t.disk_gib < 5 {
        bail!(
            "template {}: needs cpus >= 1, memory_mib >= 256 and disk_gib >= 5",
            t.name
        );
    }
    if t.image.trim().is_empty() {
        bail!("template {}: image is empty", t.name);
    }
    for arch in t.arch.keys() {
        if arch != "x86_64" && arch != "aarch64" {
            bail!(
                "template {}: [arch.{arch}] must be x86_64 or aarch64",
                t.name
            );
        }
    }
    let all = t
        .checks
        .iter()
        .chain(t.arch.values().flat_map(|a| &a.checks));
    for c in all {
        if c.name.trim().is_empty() || c.command.trim().is_empty() {
            bail!(
                "template {}: every check needs a name and a command",
                t.name
            );
        }
    }
    Ok(t)
}

pub fn names() -> Vec<&'static str> {
    BUILTIN.iter().map(|(n, _)| *n).collect()
}

/// The built-in definition's file, as `templates show` prints it.
pub fn body(name: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    BUILTIN.iter().find(|(b, _)| *b == n).map(|(_, body)| *body)
}

pub fn builtin() -> Vec<EnvTemplate> {
    BUILTIN
        .iter()
        .map(|(_, body)| parse(body).expect("built-in VM templates parse (tested)"))
        .collect()
}

/// A name that means "a template file of mine" rather than a built-in.
pub fn is_file_reference(name: &str) -> bool {
    name.ends_with(".toml")
}

/// A built-in by name, or a `.toml` file by path. `None` when `reference` is neither (a Compose or k3s template, or a typo).
pub fn load(reference: &str) -> Result<Option<EnvTemplate>> {
    if let Some(body) = body(reference) {
        return parse(body).map(Some);
    }
    if !is_file_reference(reference) {
        return Ok(None);
    }
    let text = std::fs::read_to_string(reference)
        .with_context(|| format!("reading the VM template {reference}"))?;
    parse(&text)
        .with_context(|| format!("in {reference}"))
        .map(Some)
}

/// What the command line adds to a template.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub cpus: Option<u8>,
    pub memory_mib: Option<u64>,
    pub disk_gib: Option<u64>,
    /// Parsed forwards (`run::parse_port`).
    pub forwards: Vec<Value>,
    /// Parsed shared folders (`run::parse_volume`).
    pub shares: Vec<Value>,
    /// Private networks to join (`vz` only).
    pub networks: Vec<String>,
}

/// The VM request an environment asks for, without what is only decided at create time (the SSH forward on QEMU and the
/// marker file), so the same run always computes the same definition. `hash` identifies it.
pub fn definition(
    t: &EnvTemplate,
    o: &Overrides,
    vm: &str,
    backend: Backend,
    guest_arch: &str,
    user: &str,
    ssh_key: &str,
) -> Value {
    let mut run: Vec<String> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    if t.rosetta {
        // The same mount-and-register unit the k3s guests use.
        files = crate::kube_host::rosetta_files();
        run = crate::kube_host::rosetta_commands();
    }
    run.extend(t.run.iter().map(|c| c.replace("{user}", user)));
    let mut forwards = o.forwards.clone();
    if backend == Backend::Qemu {
        // QEMU would listen on every interface; a development VM stays on this machine unless told otherwise.
        for f in &mut forwards {
            if f.get("host_addr").is_none() {
                f["host_addr"] = json!("127.0.0.1");
            }
        }
    }
    let mut spec = json!({
        "name": vm,
        "backend": match backend { Backend::Vz => "vz", Backend::Qemu => "qemu" },
        "image": t.image,
        "vcpus": o.cpus.unwrap_or(t.cpus),
        "memory_mib": o.memory_mib.unwrap_or(t.memory_mib),
        "disk_size_gib": o.disk_gib.unwrap_or(t.disk_gib),
        "network": {"mode": "user", "forwards": forwards},
        "shared_folders": o.shares,
        "cloud_init": {
            "hostname": vm,
            "user": user,
            "ssh_authorized_keys": [ssh_key],
            "packages": t.packages_for(guest_arch),
            "runcmd": run,
            "write_files": files,
        },
    });
    let mut apple = serde_json::Map::new();
    if backend == Backend::Vz {
        if t.rosetta {
            apple.insert("rosetta".into(), json!(true));
        }
        if t.nested_virtualization {
            apple.insert("nested_virtualization".into(), json!(true));
        }
        if !o.networks.is_empty() {
            let n: Vec<Value> = o.networks.iter().map(|n| json!({"name": n})).collect();
            apple.insert("networks".into(), json!(n));
        }
    }
    if !apple.is_empty() {
        spec["apple"] = Value::Object(apple);
    }
    spec
}

/// The request actually sent: [`definition`] plus the marker and, on QEMU, a loopback SSH forward on a free port.
pub fn create_request(definition: &Value, template: &str, backend: Backend) -> Value {
    let mut spec = definition.clone();
    let marker = json!({
        "path": MARKER_PATH,
        "content": format!("{}\n", json!({"template": template, "hash": crate::stack::spec_hash(definition)})),
        "permissions": "0644",
    });
    if let Some(files) = spec["cloud_init"]["write_files"].as_array_mut() {
        files.push(marker);
    }
    if backend == Backend::Qemu {
        let name = spec["name"].as_str().unwrap_or_default().to_owned();
        let forwards = spec["network"]["forwards"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let port = crate::vm_net::pick_ssh_port(&name, &crate::vm_net::loopback_port_free)
            .unwrap_or_else(|| crate::stack::ssh_forward_port(&name));
        spec["network"] = crate::vm_net::network_spec(backend, forwards, port);
    }
    spec
}

/// `(template, definition hash)` of a VM record that an environment created. The marker in the request wins; labels
/// are the fallback for a daemon that does not return the request's files.
pub fn owner(record: &Value) -> Option<(String, String)> {
    let marker = record["request"]["cloud_init"]["write_files"]
        .as_array()
        .and_then(|f| f.iter().find(|f| f["path"] == MARKER_PATH))
        .and_then(|f| f["content"].as_str())
        .and_then(|c| serde_json::from_str::<Value>(c.trim()).ok());
    let (t, h) = match &marker {
        Some(m) => (m["template"].as_str(), m["hash"].as_str()),
        None => (
            record["labels"][L_TEMPLATE].as_str(),
            record["labels"][L_HASH].as_str(),
        ),
    };
    Some((t?.to_owned(), h.unwrap_or_default().to_owned()))
}

fn size(t: &EnvTemplate) -> String {
    let mem = if t.memory_mib.is_multiple_of(1024) {
        format!("{} GiB", t.memory_mib / 1024)
    } else {
        format!("{} MiB", t.memory_mib)
    };
    format!("{} CPUs, {mem}, {} GiB disk", t.cpus, t.disk_gib)
}

/// The "VM environments" part of `templates ls`.
pub fn ls_text(backend: Backend, guest_arch: &str) -> String {
    let all = builtin();
    let w = all.iter().map(|t| t.name.len()).max().unwrap_or(0);
    let mut s = String::from("\nVM environments (a ready virtual machine):\n");
    for t in &all {
        let here = if t.unsupported_on(backend, guest_arch).is_some() {
            "  [not on this host: needs macOS]"
        } else {
            ""
        };
        s.push_str(&format!(
            "{:<w$}  {:<30}  {}{here}\n",
            t.name,
            size(t),
            t.summary
        ));
    }
    s.push_str(
        "\nSee what it would do: fluxctl templates run NAME --dry-run      Start it: fluxctl templates run NAME\n\
         A Docker engine is `fluxctl docker up`; Kubernetes is `fluxctl kube up`; macOS guests are `fluxctl mac run`.\n",
    );
    s
}

/// The `environments` member of `templates ls --json`.
pub fn gallery_json(backend: Backend, guest_arch: &str) -> Value {
    let items: Vec<Value> = builtin()
        .iter()
        .map(|t| {
            json!({
                "name": t.name, "title": t.title, "summary": t.summary, "image": t.image,
                "cpus": t.cpus, "memory_mib": t.memory_mib, "disk_gib": t.disk_gib,
                "supported": t.unsupported_on(backend, guest_arch).is_none(),
                "unsupported_reason": t.unsupported_on(backend, guest_arch),
                "run": format!("fluxctl templates run {}", t.name),
                "plan": format!("fluxctl templates run {} --dry-run", t.name),
            })
        })
        .collect();
    json!(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(name: &str, o: &Overrides, backend: Backend, arch: &str) -> Value {
        let t = load(name).unwrap().unwrap();
        definition(&t, o, name, backend, arch, "dev", "ssh-ed25519 AAA")
    }

    #[test]
    fn every_builtin_parses_has_checks_and_makes_a_valid_request_on_both_backends() {
        assert_eq!(builtin().len(), BUILTIN.len());
        for ((key, _), t) in BUILTIN.iter().zip(builtin()) {
            assert_eq!(*key, t.name);
            for (backend, arch) in [
                (Backend::Vz, "aarch64"),
                (Backend::Qemu, "x86_64"),
                (Backend::Qemu, "aarch64"),
            ] {
                assert!(!t.checks_for(arch, "dev").is_empty(), "{}", t.name);
                if t.unsupported_on(backend, arch).is_some() {
                    continue;
                }
                let d = definition(&t, &Overrides::default(), "x", backend, arch, "dev", "k");
                let req = create_request(&d, &t.name, backend);
                serde_json::from_value::<fluxvm_core::model::CreateVmRequest>(req)
                    .unwrap_or_else(|e| panic!("{} on {backend:?}: {e}", t.name));
            }
        }
    }

    #[test]
    fn builtin_names_do_not_collide_with_the_stack_templates() {
        for n in names() {
            assert!(fluxvm_core::stack_templates::find(n).is_none(), "{n}");
        }
    }

    #[test]
    fn architecture_parts_pick_the_right_qemu() {
        let t = load("kvm").unwrap().unwrap();
        assert!(t.packages_for("x86_64").contains(&"qemu-system-x86".into()));
        assert!(!t.packages_for("x86_64").contains(&"qemu-system-arm".into()));
        assert!(
            t.packages_for("aarch64")
                .contains(&"qemu-system-arm".into())
        );
        assert!(
            t.checks_for("x86_64", "u")
                .iter()
                .any(|c| c.command.contains("qemu-system-x86_64"))
        );
    }

    #[test]
    fn the_user_placeholder_is_filled_in() {
        let d = def("kvm", &Overrides::default(), Backend::Vz, "aarch64");
        assert_eq!(d["cloud_init"]["runcmd"][0], "usermod -aG kvm dev");
        assert_eq!(d["apple"]["nested_virtualization"], true);
        // QEMU has no apple section: the host CPU is passed through.
        let d = def("kvm", &Overrides::default(), Backend::Qemu, "x86_64");
        assert!(d.get("apple").is_none());
    }

    #[test]
    fn rosetta_mounts_before_the_template_commands_and_is_refused_on_qemu() {
        let d = def("amd64", &Overrides::default(), Backend::Vz, "aarch64");
        assert_eq!(d["apple"]["rosetta"], true);
        let run: Vec<&str> = d["cloud_init"]["runcmd"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let at = |needle: &str| run.iter().position(|c| c.contains(needle)).unwrap();
        assert!(at("fluxvm-rosetta") < at("add-architecture"), "{run:?}");
        let t = load("amd64").unwrap().unwrap();
        let why = t.unsupported_on(Backend::Qemu, "x86_64").unwrap();
        assert!(why.contains("Rosetta") && why.contains("`build`"), "{why}");
        assert!(t.unsupported_on(Backend::Vz, "aarch64").is_none());
    }

    #[test]
    fn overrides_apply_and_change_the_hash() {
        let base = def("debian", &Overrides::default(), Backend::Vz, "aarch64");
        let o = Overrides {
            memory_mib: Some(1024),
            networks: vec!["lab".into()],
            ..Default::default()
        };
        let d = def("debian", &o, Backend::Vz, "aarch64");
        assert_eq!(d["memory_mib"], 1024);
        assert_eq!(d["apple"]["networks"][0]["name"], "lab");
        assert_ne!(crate::stack::spec_hash(&base), crate::stack::spec_hash(&d));
    }

    #[test]
    fn qemu_forwards_stay_on_loopback_and_the_ssh_port_is_not_part_of_the_hash() {
        let o = Overrides {
            forwards: vec![json!({"host_port": 8080, "guest_port": 80})],
            ..Default::default()
        };
        let d = def("debian", &o, Backend::Qemu, "x86_64");
        assert_eq!(d["network"]["forwards"][0]["host_addr"], "127.0.0.1");
        assert_eq!(d["network"]["forwards"].as_array().unwrap().len(), 1);
        let req = create_request(&d, "debian", Backend::Qemu);
        assert!(crate::run::loopback_ssh_endpoint(&req).is_some());
        let record = json!({"request": req});
        assert_eq!(
            owner(&record),
            Some(("debian".into(), crate::stack::spec_hash(&d)))
        );
        let vz = def("debian", &o, Backend::Vz, "aarch64");
        assert!(vz["network"]["forwards"][0].get("host_addr").is_none());
    }

    #[test]
    fn ownership_comes_from_the_marker_then_the_labels_and_is_absent_otherwise() {
        let d = def("debian", &Overrides::default(), Backend::Vz, "aarch64");
        let req = create_request(&d, "debian", Backend::Vz);
        let marked = json!({"request": req, "labels": {L_TEMPLATE: "build"}});
        assert_eq!(owner(&marked).unwrap().0, "debian");
        let labelled = json!({"labels": {L_TEMPLATE: "build", L_HASH: "abc"}});
        assert_eq!(owner(&labelled), Some(("build".into(), "abc".into())));
        assert_eq!(owner(&json!({"name": "mine", "labels": {}})), None);
    }

    #[test]
    fn a_users_file_loads_and_mistakes_are_named() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("mine.toml");
        std::fs::write(&p, body("debian").unwrap()).unwrap();
        assert_eq!(load(p.to_str().unwrap()).unwrap().unwrap().name, "debian");
        assert!(load("nope").unwrap().is_none());
        assert!(load("/does/not/exist.toml").is_err());
        let e = parse("name='x'\ntitle='t'\nsummary='s'\nimage='i'\ncpus=1\nmemory_mib=512\ndisk_gib=10\nshares=['/']")
            .unwrap_err();
        assert!(format!("{e:#}").contains("shares"), "{e:#}");
        assert!(
            parse(
                "name='x'\ntitle='t'\nsummary='s'\nimage='i'\ncpus=0\nmemory_mib=512\ndisk_gib=10"
            )
            .is_err()
        );
        assert!(check_vm_name("flux-agent-templates").is_ok());
        assert!(check_vm_name("../x").is_err());
    }

    #[test]
    fn the_listing_marks_what_this_host_cannot_run() {
        let linux = ls_text(Backend::Qemu, "x86_64");
        let amd = linux.lines().find(|l| l.starts_with("amd64")).unwrap();
        assert!(amd.contains("not on this host"), "{amd}");
        assert!(!ls_text(Backend::Vz, "aarch64").contains("not on this host"));
        for n in names() {
            assert!(linux.contains(n));
        }
    }
}
