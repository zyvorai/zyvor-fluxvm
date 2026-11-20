// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Instancetypes and preferences, after KubeVirt's `VirtualMachineInstancetype` and
//! `VirtualMachinePreference`: a named, versioned size class (vCPUs, memory, hugepages, CPU model,
//! optional limits) and a named bundle of device defaults (disk bus, NIC model, firmware).
//!
//! A create body may carry `"instancetype": "name[:version]"` and `"preference": "name"`. Resolution
//! works on the JSON body, before it becomes a `CreateVmRequest`, because only the raw body shows
//! whether the caller set a field:
//!
//! * An instancetype *owns* the fields it defines. A body that also sets one of them (`vcpus`,
//!   `memory_mib`, ...) is a validation error naming every clash, as in KubeVirt.
//! * A preference only supplies defaults. A field the body sets itself always wins.
//!
//! The values are written into the body, so the VM's own spec is the snapshot: editing or deleting a
//! class later changes nothing for existing VMs. [`ResolvedSnapshot`] records which class and version
//! produced them, and what was applied or left out.
//!
//! Classes live in `<state_dir>/instancetypes.json` and `<state_dir>/preferences.json`, like
//! `vm-templates.json`. A few built-in classes ship in the binary; a stored class of the same name
//! and version replaces a built-in one, and a new version of a built-in name becomes the latest.

use crate::config_lint::{Finding, Severity};
use crate::model::DiskBus;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Body keys that select a class. They are consumed by resolution and never reach `CreateVmRequest`.
pub const INSTANCETYPE_KEY: &str = "instancetype";
pub const PREFERENCE_KEY: &str = "preference";
/// File written into a VM's workspace with its [`ResolvedSnapshot`].
pub const SNAPSHOT_FILE: &str = "instancetype.json";

/// Rate and cap limits an instancetype may impose; they map onto the VM spec's existing limit fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstancetypeLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_mbit_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_pps_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blk_mbit_limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blk_ops_limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstancetypeSpec {
    pub vcpus: u8,
    pub memory_mib: u64,
    /// CPU hotplug ceiling (`max_vcpus`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u8>,
    /// Memory hotplug ceiling (`max_memory_mib`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hugepages: Option<bool>,
    /// CPU model. Applied to the VM's `cpu_template` on Firecracker only; other backends have no
    /// per-VM CPU model setting yet, so it is recorded in the snapshot and reported as not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<InstancetypeLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Instancetype {
    pub name: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Shipped in the binary rather than stored; never persisted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    pub spec: InstancetypeSpec,
}

/// Device and firmware defaults. Every field is optional and loses to the same setting in the spec.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreferenceSpec {
    /// Root disk bus. The root disk is always virtio-blk, so only `virtio-blk` can be applied to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bus: Option<DiskBus>,
    /// NIC model (`virtio`, `e1000e`, ...). The VM spec has no NIC model setting yet: recorded, not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nic_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure_boot: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preference {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    pub spec: PreferenceSpec,
}

/// What one create resolved to, kept with the VM (`<workspace>/instancetype.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedSnapshot {
    pub resolved_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instancetype: Option<Instancetype>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preference: Option<Preference>,
    /// Spec keys the resolution wrote.
    pub applied: Vec<String>,
    /// Class settings not written, with the reason (set explicitly, or not supported).
    pub skipped: Vec<SkippedField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedField {
    pub field: String,
    pub reason: String,
}

/// `name` or `name:version` as written in a create body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassRef {
    pub name: String,
    pub version: Option<u32>,
}

impl ClassRef {
    pub fn parse(s: &str) -> Result<Self> {
        let (name, version) = match s.rsplit_once(':') {
            Some((n, v)) => (
                n,
                Some(v.parse::<u32>().with_context(|| {
                    format!("invalid instancetype {s:?}: the version after ':' must be a number")
                })?),
            ),
            None => (s, None),
        };
        validate_name("instancetype", name)?;
        if version == Some(0) {
            bail!("invalid instancetype {s:?}: versions start at 1");
        }
        Ok(Self {
            name: name.to_string(),
            version,
        })
    }
}

pub fn validate_name(what: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 63
        || name.starts_with(['-', '.'])
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("invalid {what} name {name:?}: use 1-63 chars of [A-Za-z0-9._-]");
    }
    Ok(())
}

impl InstancetypeSpec {
    pub fn validate(&self) -> Result<()> {
        if self.vcpus == 0 {
            bail!("instancetype vcpus must be at least 1");
        }
        if self.memory_mib < 128 {
            bail!("instancetype memory_mib must be at least 128");
        }
        if self.max_vcpus.is_some_and(|m| m < self.vcpus) {
            bail!("instancetype max_vcpus must be >= vcpus");
        }
        if self.max_memory_mib.is_some_and(|m| m < self.memory_mib) {
            bail!("instancetype max_memory_mib must be >= memory_mib");
        }
        if self.cpu_model.as_deref().is_some_and(str::is_empty) {
            bail!("instancetype cpu_model must not be empty");
        }
        Ok(())
    }
}

fn builtin_class(name: &str, desc: &str, vcpus: u8, memory_mib: u64) -> Instancetype {
    Instancetype {
        name: name.into(),
        version: 1,
        description: Some(desc.into()),
        builtin: true,
        created_at: None,
        spec: InstancetypeSpec {
            vcpus,
            memory_mib,
            max_vcpus: None,
            max_memory_mib: None,
            hugepages: None,
            cpu_model: None,
            limits: None,
        },
    }
}

/// The classes shipped with FluxVM.
pub fn builtin_instancetypes() -> Vec<Instancetype> {
    vec![
        builtin_class("small", "1 vCPU, 2 GiB", 1, 2048),
        builtin_class("medium", "2 vCPUs, 4 GiB", 2, 4096),
        builtin_class("large", "4 vCPUs, 8 GiB", 4, 8192),
        builtin_class(
            "memory-optimised",
            "4 vCPUs, 32 GiB (8 GiB per vCPU)",
            4,
            32768,
        ),
    ]
}

/// The preferences shipped with FluxVM.
pub fn builtin_preferences() -> Vec<Preference> {
    let p = |name: &str, desc: &str, spec: PreferenceSpec| Preference {
        name: name.into(),
        description: Some(desc.into()),
        builtin: true,
        created_at: None,
        spec,
    };
    vec![
        p(
            "linux",
            "virtio disk and NIC",
            PreferenceSpec {
                disk_bus: Some(DiskBus::VirtioBlk),
                nic_model: Some("virtio".into()),
                ..Default::default()
            },
        ),
        p(
            "windows",
            "virtio disk, e1000e NIC, Secure Boot and a TPM",
            PreferenceSpec {
                disk_bus: Some(DiskBus::VirtioBlk),
                nic_model: Some("e1000e".into()),
                secure_boot: Some(true),
                tpm: Some(true),
                ..Default::default()
            },
        ),
    ]
}

/// Every class a create can name: built-ins overlaid with the stored ones.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub instancetypes: BTreeMap<String, BTreeMap<u32, Instancetype>>,
    pub preferences: BTreeMap<String, Preference>,
}

impl Catalog {
    /// Built-ins only.
    pub fn builtin() -> Self {
        let mut c = Self::default();
        for t in builtin_instancetypes() {
            c.instancetypes
                .entry(t.name.clone())
                .or_default()
                .insert(t.version, t);
        }
        for p in builtin_preferences() {
            c.preferences.insert(p.name.clone(), p);
        }
        c
    }

    /// Built-ins overlaid with the classes stored under `state_dir`.
    pub fn load(state_dir: &Path) -> Result<Self> {
        let mut c = Self::builtin();
        for (name, versions) in load_instancetypes(state_dir)? {
            c.instancetypes.entry(name).or_default().extend(versions);
        }
        c.preferences.extend(load_preferences(state_dir)?);
        Ok(c)
    }

    pub fn find_instancetype(&self, r: &ClassRef) -> Result<&Instancetype> {
        let versions = self
            .instancetypes
            .get(&r.name)
            .with_context(|| format!("instancetype {:?} not found", r.name))?;
        match r.version {
            Some(v) => versions
                .get(&v)
                .with_context(|| format!("instancetype {}:{v} not found", r.name)),
            None => versions
                .values()
                .next_back()
                .with_context(|| format!("instancetype {:?} has no versions", r.name)),
        }
    }

    pub fn find_preference(&self, name: &str) -> Result<&Preference> {
        self.preferences
            .get(name)
            .with_context(|| format!("preference {name:?} not found"))
    }

    /// The version a new class of `name` gets when the caller does not pick one.
    pub fn next_version(&self, name: &str) -> u32 {
        self.instancetypes
            .get(name)
            .and_then(|v| v.keys().next_back())
            .map_or(1, |v| v + 1)
    }
}

// ---- storage -------------------------------------------------------------------------------------

static STORE_LOCK: Mutex<()> = Mutex::new(());

pub fn instancetypes_path(state_dir: &Path) -> PathBuf {
    state_dir.join("instancetypes.json")
}

pub fn preferences_path(state_dir: &Path) -> PathBuf {
    state_dir.join("preferences.json")
}

fn read_json<T: Default + for<'de> Deserialize<'de>>(p: &Path) -> Result<T> {
    match std::fs::read_to_string(p) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

fn write_json<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, p).with_context(|| format!("replacing {}", p.display()))
}

/// The stored (non-built-in) instancetypes, by name then version.
pub fn load_instancetypes(
    state_dir: &Path,
) -> Result<BTreeMap<String, BTreeMap<u32, Instancetype>>> {
    read_json(&instancetypes_path(state_dir))
}

pub fn load_preferences(state_dir: &Path) -> Result<BTreeMap<String, Preference>> {
    read_json(&preferences_path(state_dir))
}

/// Stores an instancetype. Without `version` it becomes the next version of `name`; with one, it
/// replaces a stored class of that version (or shadows the built-in one) only when `replace` is set.
pub fn save_instancetype(
    state_dir: &Path,
    name: &str,
    version: Option<u32>,
    description: Option<String>,
    spec: InstancetypeSpec,
    replace: bool,
) -> Result<Instancetype> {
    validate_name("instancetype", name)?;
    spec.validate()?;
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let catalog = Catalog::load(state_dir)?;
    let version = version.unwrap_or_else(|| catalog.next_version(name));
    if version == 0 {
        bail!("instancetype versions start at 1");
    }
    let exists = catalog
        .instancetypes
        .get(name)
        .is_some_and(|v| v.contains_key(&version));
    if exists && !replace {
        bail!("instancetype {name}:{version} already exists (use replace to overwrite it)");
    }
    let t = Instancetype {
        name: name.into(),
        version,
        description,
        builtin: false,
        created_at: Some(Utc::now()),
        spec,
    };
    let mut stored = load_instancetypes(state_dir)?;
    stored
        .entry(name.into())
        .or_default()
        .insert(version, t.clone());
    write_json(&instancetypes_path(state_dir), &stored)?;
    Ok(t)
}

/// Removes one stored version, or every stored version when `version` is `None`. Built-ins cannot be
/// removed (store a class of the same name to override them).
pub fn delete_instancetype(state_dir: &Path, name: &str, version: Option<u32>) -> Result<()> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut stored = load_instancetypes(state_dir)?;
    let found = match (stored.get_mut(name), version) {
        (Some(_), None) => stored.remove(name).is_some(),
        (Some(v), Some(ver)) => v.remove(&ver).is_some(),
        (None, _) => false,
    };
    if !found {
        let builtin = builtin_instancetypes().iter().any(|t| t.name == name);
        if builtin {
            bail!(
                "instancetype {name:?} is built in and cannot be deleted; store a class of the same name to override it"
            );
        }
        bail!("instancetype {name:?} not found");
    }
    if stored.get(name).is_some_and(|v| v.is_empty()) {
        stored.remove(name);
    }
    write_json(&instancetypes_path(state_dir), &stored)
}

pub fn save_preference(
    state_dir: &Path,
    name: &str,
    description: Option<String>,
    spec: PreferenceSpec,
    replace: bool,
) -> Result<Preference> {
    validate_name("preference", name)?;
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut stored = load_preferences(state_dir)?;
    if !replace
        && (stored.contains_key(name) || builtin_preferences().iter().any(|p| p.name == name))
    {
        bail!("preference {name:?} already exists (use replace to overwrite it)");
    }
    let p = Preference {
        name: name.into(),
        description,
        builtin: false,
        created_at: Some(Utc::now()),
        spec,
    };
    stored.insert(name.into(), p.clone());
    write_json(&preferences_path(state_dir), &stored)?;
    Ok(p)
}

pub fn delete_preference(state_dir: &Path, name: &str) -> Result<()> {
    let _g = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut stored = load_preferences(state_dir)?;
    if stored.remove(name).is_none() {
        if builtin_preferences().iter().any(|p| p.name == name) {
            bail!(
                "preference {name:?} is built in and cannot be deleted; store one of the same name to override it"
            );
        }
        bail!("preference {name:?} not found");
    }
    write_json(&preferences_path(state_dir), &stored)
}

// ---- resolution ----------------------------------------------------------------------------------

fn put_default(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    v: Value,
    applied: &mut Vec<String>,
    skipped: &mut Vec<SkippedField>,
) {
    if is_set(obj, key) {
        skipped.push(SkippedField {
            field: key.into(),
            reason: "set explicitly in the spec, which wins over the preference".into(),
        });
    } else {
        obj.insert(key.into(), v);
        applied.push(key.into());
    }
}

fn is_set(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.get(key).is_some_and(|v| !v.is_null())
}

/// Resolves `instancetype`/`preference` in a create body in place and returns what was recorded.
/// `None` when the body names neither. On success the two selector keys are gone from `body`.
pub fn resolve_body(body: &mut Value, catalog: &Catalog) -> Result<Option<ResolvedSnapshot>> {
    let Some(obj) = body.as_object_mut() else {
        return Ok(None);
    };
    let take_name =
        |obj: &mut serde_json::Map<String, Value>, key: &str| -> Result<Option<String>> {
            match obj.remove(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s)),
                Some(_) => bail!("`{key}` must be a string (`name[:version]` for instancetype)"),
            }
        };
    let it_name = take_name(obj, INSTANCETYPE_KEY)?;
    let pref_name = take_name(obj, PREFERENCE_KEY)?;
    if it_name.is_none() && pref_name.is_none() {
        return Ok(None);
    }
    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    let backend = obj
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut it_snapshot = None;
    if let Some(s) = it_name {
        let class = catalog.find_instancetype(&ClassRef::parse(&s)?)?.clone();
        let sp = &class.spec;
        // (spec key, value to write, whether it applies on this backend)
        let mut fields: Vec<(&str, Value)> = vec![
            ("vcpus", json!(sp.vcpus)),
            ("memory_mib", json!(sp.memory_mib)),
        ];
        if let Some(v) = sp.max_vcpus {
            fields.push(("max_vcpus", json!(v)));
        }
        if let Some(v) = sp.max_memory_mib {
            fields.push(("max_memory_mib", json!(v)));
        }
        if let Some(v) = sp.hugepages {
            fields.push(("hugepages", json!(v)));
        }
        if let Some(l) = &sp.limits {
            for (k, v) in [
                ("net_mbit_limit", l.net_mbit_limit.map(|v| json!(v))),
                ("net_pps_limit", l.net_pps_limit.map(|v| json!(v))),
                ("blk_mbit_limit", l.blk_mbit_limit.map(|v| json!(v))),
                ("blk_ops_limit", l.blk_ops_limit.map(|v| json!(v))),
            ] {
                if let Some(v) = v {
                    fields.push((k, v));
                }
            }
        }
        if let Some(m) = &sp.cpu_model {
            if backend == "firecracker" {
                fields.push(("cpu_template", json!(m)));
            } else {
                skipped.push(SkippedField {
                    field: "cpu_model".into(),
                    reason: format!(
                        "the {} backend has no per-VM CPU model setting; recorded only",
                        if backend.is_empty() {
                            "chosen"
                        } else {
                            &backend
                        }
                    ),
                });
            }
        }
        let clashes: Vec<&str> = fields
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| is_set(obj, k))
            .collect();
        if !clashes.is_empty() {
            let list = clashes
                .iter()
                .map(|k| format!("`{k}`"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "spec sets {list} but instancetype {}:{} defines {}; remove {} from the spec, or drop `instancetype`",
                class.name,
                class.version,
                if clashes.len() == 1 { "it" } else { "them" },
                if clashes.len() == 1 { "it" } else { "them" },
            );
        }
        for (k, v) in fields {
            obj.insert(k.into(), v);
            applied.push(k.to_string());
        }
        it_snapshot = Some(class);
    }

    let mut pref_snapshot = None;
    if let Some(name) = pref_name {
        let pref = catalog.find_preference(&name)?.clone();
        let sp = &pref.spec;
        match sp.disk_bus {
            Some(DiskBus::VirtioBlk) => {
                let io = obj
                    .entry("root_disk_io")
                    .or_insert_with(|| json!({}))
                    .as_object_mut();
                match io {
                    Some(io) if is_set(io, "bus") => skipped.push(SkippedField {
                        field: "root_disk_io.bus".into(),
                        reason: "set explicitly in the spec, which wins over the preference".into(),
                    }),
                    Some(io) => {
                        io.insert("bus".into(), json!("virtio-blk"));
                        applied.push("root_disk_io.bus".into());
                    }
                    None => bail!("`root_disk_io` must be an object"),
                }
            }
            Some(other) => skipped.push(SkippedField {
                field: "disk_bus".into(),
                reason: format!(
                    "the root disk is always virtio-blk; {other:?} applies to data disks only"
                ),
            }),
            None => {}
        }
        if let Some(m) = &sp.nic_model {
            skipped.push(SkippedField {
                field: "nic_model".into(),
                reason: format!("the VM spec has no NIC model setting yet; {m:?} recorded only"),
            });
        }
        if let Some(v) = sp.secure_boot {
            put_default(obj, "secure_boot", json!(v), &mut applied, &mut skipped);
        }
        if let Some(v) = sp.tpm {
            put_default(obj, "tpm", json!(v), &mut applied, &mut skipped);
        }
        if let Some(v) = &sp.firmware {
            put_default(obj, "firmware", json!(v), &mut applied, &mut skipped);
        }
        pref_snapshot = Some(pref);
    }

    Ok(Some(ResolvedSnapshot {
        resolved_at: Utc::now(),
        instancetype: it_snapshot,
        preference: pref_snapshot,
        applied,
        skipped,
    }))
}

impl ResolvedSnapshot {
    /// `name:version` of the instancetype, for the VM's labels.
    pub fn instancetype_label(&self) -> Option<String> {
        self.instancetype
            .as_ref()
            .map(|t| format!("{}:{}", t.name, t.version))
    }
}

// ---- spec lint ------------------------------------------------------------------------------------

/// Spec-lint support: resolves the selector keys of a spec file against the built-in catalog and
/// returns the body to lint plus findings. A class that is not built in cannot be checked offline,
/// so the clashes are checked against what is known and the rest is reported as `Info`.
pub fn lint_prepare(mut value: Value) -> (Value, Vec<Finding>) {
    let mut out = Vec::new();
    let finding = |severity, key: &str, message: String, fix: Option<String>| Finding {
        severity,
        key: key.into(),
        message,
        fix,
    };
    let Some(obj) = value.as_object() else {
        return (value, out);
    };
    if !obj.contains_key(INSTANCETYPE_KEY) && !obj.contains_key(PREFERENCE_KEY) {
        return (value, out);
    }
    let catalog = Catalog::builtin();
    // Unknown names are only an error for built-ins we can see; a stored class may exist on the host.
    for (key, kind) in [
        (INSTANCETYPE_KEY, "instancetype"),
        (PREFERENCE_KEY, "preference"),
    ] {
        let Some(v) = obj.get(key) else { continue };
        let Some(s) = v.as_str() else {
            out.push(finding(
                Severity::Error,
                key,
                format!("`{key}` must be a string"),
                None,
            ));
            continue;
        };
        let known = if key == INSTANCETYPE_KEY {
            ClassRef::parse(s).map(|r| catalog.find_instancetype(&r).is_ok())
        } else {
            validate_name(kind, s).map(|()| catalog.find_preference(s).is_ok())
        };
        match known {
            Err(e) => out.push(finding(Severity::Error, key, format!("{e:#}"), None)),
            Ok(false) => out.push(finding(
                Severity::Info,
                key,
                format!("{kind} {s:?} is not built in; it is checked against the host's stored classes at create time"),
                None,
            )),
            Ok(true) => {}
        }
    }
    let mut probe = value.clone();
    match resolve_body(&mut probe, &catalog) {
        Ok(_) => value = probe,
        Err(e) => {
            let msg = format!("{e:#}");
            // Unknown names were reported above; only a clash is new information here.
            if msg.starts_with("spec sets") {
                out.push(finding(
                    Severity::Error,
                    INSTANCETYPE_KEY,
                    msg,
                    Some("remove the clashing keys, or drop `instancetype`".into()),
                ));
            }
            if let Some(o) = value.as_object_mut() {
                o.remove(INSTANCETYPE_KEY);
                o.remove(PREFERENCE_KEY);
            }
        }
    }
    (value, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(v: u8, m: u64) -> InstancetypeSpec {
        InstancetypeSpec {
            vcpus: v,
            memory_mib: m,
            max_vcpus: None,
            max_memory_mib: None,
            hugepages: None,
            cpu_model: None,
            limits: None,
        }
    }

    #[test]
    fn parses_refs() {
        assert_eq!(
            ClassRef::parse("small:2").unwrap(),
            ClassRef {
                name: "small".into(),
                version: Some(2)
            }
        );
        assert_eq!(ClassRef::parse("small").unwrap().version, None);
        for bad in ["", "a b", "small:x", "small:0", ":1", "../x"] {
            assert!(ClassRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn builtins_resolve_and_the_latest_version_wins() {
        let c = Catalog::builtin();
        for n in ["small", "medium", "large", "memory-optimised"] {
            assert_eq!(
                c.find_instancetype(&ClassRef::parse(n).unwrap())
                    .unwrap()
                    .version,
                1
            );
        }
        assert!(c.find_preference("linux").is_ok());
        assert!(
            c.find_instancetype(&ClassRef::parse("small:9").unwrap())
                .is_err()
        );
        assert!(
            c.find_instancetype(&ClassRef::parse("nope").unwrap())
                .is_err()
        );
    }

    #[test]
    fn instancetype_fills_the_spec_and_clashes_are_errors() {
        let c = Catalog::builtin();
        let mut body =
            json!({"backend": "qemu", "name": "a", "image": "/i", "instancetype": "large"});
        let snap = resolve_body(&mut body, &c).unwrap().unwrap();
        assert_eq!(body["vcpus"], 4);
        assert_eq!(body["memory_mib"], 8192);
        assert!(body.get("instancetype").is_none());
        assert_eq!(snap.instancetype_label().as_deref(), Some("large:1"));
        assert_eq!(snap.applied, ["vcpus", "memory_mib"]);

        let mut body =
            json!({"name": "a", "instancetype": "large", "vcpus": 2, "memory_mib": 1024});
        let e = resolve_body(&mut body, &c).unwrap_err().to_string();
        assert!(
            e.contains("`vcpus`") && e.contains("`memory_mib`") && e.contains("large:1"),
            "{e}"
        );
    }

    #[test]
    fn explicit_spec_wins_over_a_preference() {
        let c = Catalog::builtin();
        let mut body = json!({"backend": "qemu", "tpm": false, "preference": "windows"});
        let snap = resolve_body(&mut body, &c).unwrap().unwrap();
        assert_eq!(body["tpm"], false);
        assert_eq!(body["secure_boot"], true);
        assert_eq!(body["root_disk_io"]["bus"], "virtio-blk");
        assert!(snap.skipped.iter().any(|s| s.field == "tpm"));
        assert!(snap.skipped.iter().any(|s| s.field == "nic_model"));
        // An explicit bus wins as well.
        let mut body =
            json!({"root_disk_io": {"bus": "virtio-blk", "iothread": true}, "preference": "linux"});
        resolve_body(&mut body, &c).unwrap();
        assert_eq!(body["root_disk_io"]["iothread"], true);
    }

    #[test]
    fn cpu_model_only_applies_on_firecracker() {
        let mut c = Catalog::builtin();
        let mut s = spec(2, 1024);
        s.cpu_model = Some("T2".into());
        c.instancetypes.entry("fc".into()).or_default().insert(
            1,
            Instancetype {
                name: "fc".into(),
                version: 1,
                description: None,
                builtin: false,
                created_at: None,
                spec: s,
            },
        );
        let mut qemu = json!({"backend": "qemu", "instancetype": "fc"});
        let snap = resolve_body(&mut qemu, &c).unwrap().unwrap();
        assert!(qemu.get("cpu_template").is_none());
        assert!(snap.skipped.iter().any(|s| s.field == "cpu_model"));
        let mut fc = json!({"backend": "firecracker", "instancetype": "fc"});
        resolve_body(&mut fc, &c).unwrap();
        assert_eq!(fc["cpu_template"], "T2");
    }

    #[test]
    fn stored_classes_are_versioned_and_override_built_ins() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // A new version of a built-in name becomes the latest.
        let t = save_instancetype(d, "small", None, None, spec(2, 2048), false).unwrap();
        assert_eq!(t.version, 2);
        let c = Catalog::load(d).unwrap();
        assert_eq!(
            c.find_instancetype(&ClassRef::parse("small").unwrap())
                .unwrap()
                .spec
                .vcpus,
            2
        );
        assert_eq!(
            c.find_instancetype(&ClassRef::parse("small:1").unwrap())
                .unwrap()
                .spec
                .vcpus,
            1
        );
        // Re-using a version needs replace; with it the built-in is shadowed.
        assert!(save_instancetype(d, "small", Some(1), None, spec(3, 2048), false).is_err());
        save_instancetype(d, "small", Some(1), None, spec(3, 2048), true).unwrap();
        let c = Catalog::load(d).unwrap();
        assert_eq!(
            c.find_instancetype(&ClassRef::parse("small:1").unwrap())
                .unwrap()
                .spec
                .vcpus,
            3
        );
        assert!(save_instancetype(d, "bad", None, None, spec(0, 1024), false).is_err());
        delete_instancetype(d, "small", Some(2)).unwrap();
        delete_instancetype(d, "small", None).unwrap();
        assert!(delete_instancetype(d, "small", None).is_err());
        assert!(delete_instancetype(d, "nope", None).is_err());
    }

    #[test]
    fn later_edits_do_not_change_an_existing_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        save_instancetype(d, "web", None, None, spec(2, 2048), false).unwrap();
        let mut body = json!({"instancetype": "web:1"});
        resolve_body(&mut body, &Catalog::load(d).unwrap()).unwrap();
        save_instancetype(d, "web", Some(1), None, spec(16, 65536), true).unwrap();
        assert_eq!(body["vcpus"], 2);
        assert_eq!(body["memory_mib"], 2048);
    }

    #[test]
    fn preferences_store_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let p = PreferenceSpec {
            tpm: Some(true),
            ..Default::default()
        };
        save_preference(d, "mine", None, p.clone(), false).unwrap();
        assert!(save_preference(d, "mine", None, p.clone(), false).is_err());
        assert!(save_preference(d, "linux", None, p.clone(), false).is_err());
        save_preference(d, "linux", None, p, true).unwrap();
        assert_eq!(
            Catalog::load(d)
                .unwrap()
                .find_preference("linux")
                .unwrap()
                .spec
                .tpm,
            Some(true)
        );
        delete_preference(d, "mine").unwrap();
        assert!(delete_preference(d, "mine").is_err());
    }

    #[test]
    fn lint_reports_clashes_and_lints_the_resolved_spec() {
        let (v, f) = lint_prepare(json!({"instancetype": "small", "vcpus": 4}));
        assert!(
            f.iter()
                .any(|x| x.severity == Severity::Error && x.message.contains("`vcpus`")),
            "{f:?}"
        );
        assert!(v.get("instancetype").is_none());
        let (v, f) = lint_prepare(json!({"instancetype": "small"}));
        assert!(f.is_empty(), "{f:?}");
        assert_eq!(v["vcpus"], 1);
        let (_, f) = lint_prepare(json!({"instancetype": "custom:3"}));
        assert!(f.iter().any(|x| x.severity == Severity::Info));
        let (_, f) = lint_prepare(json!({"instancetype": "a b"}));
        assert!(f.iter().any(|x| x.severity == Severity::Error));
    }
}
