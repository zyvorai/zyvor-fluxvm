// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet`: the scheduler core. Pure functions and small state types, no ssh: what a Mac reports (the template
//! `name:tag@digest` it holds, free CPU/memory/disk, power, running macOS guests), how a job is matched to a Mac, how
//! short leases keep two jobs from taking the same slot between two probes, and the drain list that keeps new jobs off
//! a Mac. `mac_fleet_run` does the I/O around it.
//!
//! The placement score is `fluxvm_scheduler::apple_placement::score` (tightest fit, Apple's two-macOS-guest limit)
//! plus a bonus for holding the exact template. Everything the score cannot express (labels, disk, AC power, drain, a
//! template that is missing or a different digest) is a hard filter that returns a reason, so "no Mac" always says why.

use crate::mac_fleet_remote::{Facts, valid_label};
use crate::mac_fleet_ssh::Target;
use crate::mac_store;
use anyhow::{Context, Result, bail, ensure};
use fluxvm_scheduler::apple_placement::{
    AppleHostCaps, ApplePlacementRequest, MAX_MACOS_GUESTS, allocatable_mib, score,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Added when a Mac holds the exact template: larger than any fit difference and than the built-in-image bonus.
pub const TEMPLATE_PRESENT_BONUS: i64 = 10_000_000;
/// How long a placement holds its slot, CPU and memory before the next probe is expected to show the guest (Mactra uses
/// 90 s leases; a macOS guest takes about this long to appear in `/v1/vms`).
pub const LEASE_TTL_SECS: u64 = 120;

// ---- templates -------------------------------------------------------------------------------------------------------

/// `name:tag` with an optional pinned `@sha256:HEX`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TemplateRef {
    /// The store key: `registry/repo:tag` (`:latest` added when no tag is given).
    pub name: String,
    pub digest: Option<String>,
}

impl TemplateRef {
    pub fn parse(s: &str) -> Result<Self> {
        let (name, digest) = match s.split_once('@') {
            Some((n, d)) => {
                let hex = d.strip_prefix("sha256:").unwrap_or("");
                ensure!(
                    hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit()),
                    "{s:?}: the digest must be `@sha256:` and 64 hex digits"
                );
                (n, Some(format!("sha256:{}", hex.to_ascii_lowercase())))
            }
            None => (s, None),
        };
        ensure!(
            !name.is_empty() && name.bytes().all(|c| c.is_ascii_graphic()) && !name.contains('\''),
            "{s:?} is not a template reference"
        );
        Ok(Self {
            name: mac_store::normalize(name)?,
            digest,
        })
    }

    pub fn require_digest(&self) -> Result<&str> {
        self.digest
            .as_deref()
            .context("pin the digest: `name:tag@sha256:HEX` (a tag can move, and a digest is what gets verified)")
    }
}

impl std::fmt::Display for TemplateRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.digest {
            Some(d) => write!(f, "{}@{d}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

/// One entry of `fluxctl mac images --json` on a Mac.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldTemplate {
    pub reference: String,
    /// `sha256:...`; empty for a template imported locally (no registry digest to compare).
    #[serde(default)]
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holds {
    /// Same name and the pinned digest (or no digest was pinned and the name is there).
    Yes,
    /// The name is there with another digest: running it would run a different template than the one pinned.
    Stale(String),
    No,
}

pub fn holds(held: &[HeldTemplate], want: &TemplateRef) -> Holds {
    match held.iter().find(|h| h.reference == want.name) {
        None => Holds::No,
        Some(h) => match &want.digest {
            None => Holds::Yes,
            Some(d) if &h.digest == d => Holds::Yes,
            Some(_) => Holds::Stale(if h.digest.is_empty() {
                "local import".into()
            } else {
                h.digest.clone()
            }),
        },
    }
}

pub fn parse_images_json(text: &str) -> Vec<HeldTemplate> {
    serde_json::from_str::<Vec<HeldTemplate>>(text.trim()).unwrap_or_default()
}

// ---- what a Mac reports ----------------------------------------------------------------------------------------------

/// The extra read-only lines `sched_probe_script` prints on top of `probe_script`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchedFacts {
    pub ncpu: Option<u32>,
    pub memsize_mib: Option<u64>,
    /// `ac`, `battery`, or empty when unknown (a Mac mini has no battery and reports `ac`).
    pub power: String,
    pub templates: Vec<HeldTemplate>,
    pub templates_known: bool,
}

/// Appended to `probe_script()`; one ssh call gets both.
pub fn sched_probe_script() -> &'static str {
    r#"echo "ncpu=$(sysctl -n hw.ncpu 2>/dev/null)"
echo "memsize=$(sysctl -n hw.memsize 2>/dev/null)"
echo "power=$(pmset -g batt 2>/dev/null | awk -F"'" 'NR==1{if($2=="AC Power")print "ac"; else if($2=="Battery Power")print "battery"}')"
echo "images_json=$([ -n "$FLUXCTL" ] && "$FLUXCTL" mac images --json 2>/dev/null | tr -d '\n')"
"#
}

pub fn parse_sched_facts(out: &str) -> SchedFacts {
    let mut f = SchedFacts::default();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k {
            "ncpu" => f.ncpu = v.parse().ok(),
            "memsize" => f.memsize_mib = v.parse::<u64>().ok().map(|b| b / (1024 * 1024)),
            "power" => f.power = v.into(),
            "images_json" if !v.is_empty() => {
                f.templates_known = serde_json::from_str::<serde_json::Value>(v).is_ok();
                f.templates = parse_images_json(v);
            }
            _ => {}
        }
    }
    f
}

/// One Mac as the scheduler sees it.
#[derive(Debug, Clone, Default)]
pub struct HostView {
    pub name: String,
    pub label: String,
    /// Why the Mac cannot be used at all (ssh failed, daemon down), or `None`.
    pub unavailable: Option<String>,
    pub labels: Vec<String>,
    pub caps: AppleHostCaps,
    pub disk_free_gib: Option<u64>,
    /// `Some(false)` on battery.
    pub on_ac: Option<bool>,
    pub templates: Vec<HeldTemplate>,
    pub templates_known: bool,
}

/// Status values that hold a macOS slot: everything the daemon counts, i.e. not stopped and not failed.
fn holds_slot(status: &str) -> bool {
    !matches!(status, "stopped" | "failed" | "deleted" | "deleting")
}

impl HostView {
    pub fn unavailable(target: &Target, why: impl Into<String>) -> Self {
        Self {
            name: target.name.clone(),
            label: target.label(),
            labels: target.labels.clone(),
            unavailable: Some(why.into()),
            ..Default::default()
        }
    }

    pub fn from_facts(target: &Target, facts: &Facts, sf: &SchedFacts) -> Self {
        let mut v = Self {
            name: target.name.clone(),
            label: target.label(),
            labels: target.labels.clone(),
            disk_free_gib: facts.disk_free_gib,
            on_ac: match sf.power.as_str() {
                "ac" => Some(true),
                "battery" => Some(false),
                _ => None,
            },
            templates: sf.templates.clone(),
            templates_known: sf.templates_known,
            ..Default::default()
        };
        if !facts.known {
            v.unavailable = Some("the probe returned nothing".into());
        } else if facts.fluxctl_version.is_none() {
            v.unavailable = Some("FluxVM is not installed (fluxctl mac-fleet up)".into());
        } else if !facts.daemon_healthy {
            v.unavailable = Some("the FluxVM daemon does not answer".into());
        } else if facts.vms_json.is_none() {
            v.unavailable = Some("the daemon did not list its VMs".into());
        }
        let (mut used_cpu, mut used_mem, mut macos) = (0u64, 0u64, 0u32);
        if let Some(items) = facts
            .vms_json
            .as_ref()
            .and_then(|j| j.get("items")?.as_array())
        {
            for vm in items {
                if !holds_slot(vm["status"].as_str().unwrap_or("")) {
                    continue;
                }
                used_cpu += vm["request"]["vcpus"].as_u64().unwrap_or(0);
                used_mem += vm["request"]["memory_mib"].as_u64().unwrap_or(0);
                if vm["request"]["apple"]["guest_os"].as_str() == Some("macos") {
                    macos += 1;
                }
            }
        }
        let total_cpu = u64::from(sf.ncpu.unwrap_or(0));
        let alloc_mem = sf.memsize_mib.map(allocatable_mib).unwrap_or(0);
        v.caps = AppleHostCaps {
            free_cpu: u32::try_from(total_cpu.saturating_sub(used_cpu)).unwrap_or(u32::MAX),
            free_memory_mib: alloc_mem.saturating_sub(used_mem),
            macos_guests: macos,
            ..Default::default()
        };
        v
    }
}

// ---- jobs and placement ----------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct JobSpec {
    pub template: TemplateRef,
    /// 0 = not stated (the template's own default applies and only the slot is checked).
    pub cpus: u32,
    pub memory_mib: u64,
    pub disk_gib: u64,
    /// Every one must be a label of the Mac (inventory `labels`).
    pub labels: Vec<String>,
    pub ac_only: bool,
    /// Pin to one Mac by name.
    pub host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Clears by itself: a slot frees, a job ends.
    Transient,
    /// Needs an operator: a template to distribute, a label that no Mac has.
    Permanent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub kind: Kind,
    pub text: String,
}

fn why(kind: Kind, text: impl Into<String>) -> Reason {
    Reason {
        kind,
        text: text.into(),
    }
}

/// Slots, CPU and memory already promised by leases that the Mac's own report may not show yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub macos: u32,
    pub cpus: u32,
    pub memory_mib: u64,
}

/// `Ok(score)` (higher is better) or every reason the Mac cannot take the job.
pub fn evaluate(
    h: &HostView,
    job: &JobSpec,
    leased: Usage,
    drained: bool,
) -> std::result::Result<i64, Vec<Reason>> {
    if let Some(p) = &job.host
        && p != &h.name
    {
        return Err(vec![why(
            Kind::Permanent,
            format!("not the requested host {p}"),
        )]);
    }
    if let Some(u) = &h.unavailable {
        return Err(vec![why(Kind::Transient, u.clone())]);
    }
    let mut r = vec![];
    if drained {
        r.push(why(
            Kind::Permanent,
            "drained (mac-fleet resume to allow new jobs)",
        ));
    }
    let missing: Vec<&String> = job
        .labels
        .iter()
        .filter(|l| !h.labels.contains(l))
        .collect();
    if !missing.is_empty() {
        r.push(why(
            Kind::Permanent,
            format!(
                "lacks label {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    match holds(&h.templates, &job.template) {
        Holds::Yes => {}
        Holds::No => r.push(why(
            Kind::Permanent,
            format!(
                "does not hold {} (fluxctl mac-fleet templates distribute)",
                job.template.name
            ),
        )),
        Holds::Stale(d) => r.push(why(
            Kind::Permanent,
            format!(
                "holds {} at {d}, not the pinned digest (fluxctl mac-fleet templates distribute)",
                job.template.name
            ),
        )),
    }
    if let (Some(free), true) = (h.disk_free_gib, job.disk_gib > 0)
        && free < job.disk_gib
    {
        r.push(why(
            Kind::Transient,
            format!("{free} GiB free, {} needed", job.disk_gib),
        ));
    }
    if job.ac_only && h.on_ac == Some(false) {
        r.push(why(Kind::Transient, "on battery power"));
    }
    let mut caps = h.caps.clone();
    caps.macos_guests += leased.macos;
    caps.free_cpu = caps.free_cpu.saturating_sub(leased.cpus);
    caps.free_memory_mib = caps.free_memory_mib.saturating_sub(leased.memory_mib);
    let req = ApplePlacementRequest {
        vcpus: job.cpus,
        memory_mib: job.memory_mib,
        macos_guest: true,
        ..Default::default()
    };
    let fit = score(&caps, &req);
    if fit.is_none() {
        if caps.macos_guests >= MAX_MACOS_GUESTS {
            r.push(why(
                Kind::Transient,
                format!(
                    "both macOS slots are taken ({} of {MAX_MACOS_GUESTS}, Apple's limit)",
                    caps.macos_guests
                ),
            ));
        } else {
            r.push(why(
                Kind::Transient,
                format!(
                    "{} CPUs and {} MiB free, {} and {} needed",
                    caps.free_cpu, caps.free_memory_mib, job.cpus, job.memory_mib
                ),
            ));
        }
    }
    match (r.is_empty(), fit) {
        (true, Some(s)) => Ok(s + if holds(&h.templates, &job.template) == Holds::Yes {
            TEMPLATE_PRESENT_BONUS
        } else {
            0
        }),
        _ => Err(r),
    }
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub index: usize,
    pub score: i64,
}

#[derive(Debug, Clone)]
pub enum Decision {
    Place(Candidate),
    /// No Mac now, but one would fit when a running job ends: wait.
    Wait(Vec<(String, Vec<Reason>)>),
    /// No Mac will fit without an operator: say what each needs.
    Never(Vec<(String, Vec<Reason>)>),
}

/// Pick the best Mac; ties go to the earlier one. Never when every Mac has a permanent reason (or all are unreachable and
/// there is nothing to wait on), Wait when at least one Mac is blocked only by something that clears.
pub fn decide(
    views: &[HostView],
    job: &JobSpec,
    leases: &Leases,
    drain: &FleetState,
    now: u64,
) -> Decision {
    let mut best: Option<Candidate> = None;
    let mut blocked = vec![];
    let mut waitable = false;
    for (i, h) in views.iter().enumerate() {
        match evaluate(
            h,
            job,
            leases.usage(&h.name, now),
            drain.is_drained(&h.name),
        ) {
            Ok(s) => {
                if best.as_ref().is_none_or(|b| s > b.score) {
                    best = Some(Candidate { index: i, score: s });
                }
            }
            Err(rs) => {
                let pinned_elsewhere = job.host.as_ref().is_some_and(|p| p != &h.name);
                if !pinned_elsewhere && rs.iter().all(|r| r.kind == Kind::Transient) {
                    waitable = true;
                }
                blocked.push((h.name.clone(), rs));
            }
        }
    }
    match best {
        Some(c) => Decision::Place(c),
        None if waitable => Decision::Wait(blocked),
        None => Decision::Never(blocked),
    }
}

pub fn explain(blocked: &[(String, Vec<Reason>)]) -> String {
    blocked
        .iter()
        .filter(|(_, rs)| !rs.is_empty())
        .map(|(n, rs)| {
            format!(
                "{n}: {}",
                rs.iter()
                    .map(|r| r.text.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n  ")
}

// ---- leases ----------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Lease {
    id: String,
    host: String,
    cpus: u32,
    memory_mib: u64,
    expires: u64,
}

/// In-process reservations. A placement takes one before the guest exists, so a second placement made before the next probe
/// cannot take the same macOS slot; it is released when the job ends and expires after [`LEASE_TTL_SECS`] if the process
/// dies. (Mactra's leases also survive a restart because its queue is a database; these do not.)
#[derive(Debug, Default)]
pub struct Leases {
    items: Vec<Lease>,
}

impl Leases {
    pub fn acquire(&mut self, id: &str, host: &str, job: &JobSpec, now: u64) {
        self.items.retain(|l| l.expires > now && l.id != id);
        self.items.push(Lease {
            id: id.into(),
            host: host.into(),
            cpus: job.cpus,
            memory_mib: job.memory_mib,
            expires: now + LEASE_TTL_SECS,
        });
    }

    pub fn release(&mut self, id: &str) {
        self.items.retain(|l| l.id != id);
    }

    pub fn usage(&self, host: &str, now: u64) -> Usage {
        self.items
            .iter()
            .filter(|l| l.host == host && l.expires > now)
            .fold(Usage::default(), |u, l| Usage {
                macos: u.macos + 1,
                cpus: u.cpus + l.cpus,
                memory_mib: u.memory_mib + l.memory_mib,
            })
    }
}

// ---- drain list and applied inventory --------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Drained {
    pub since_unix: u64,
}

/// A Mac as `apply` last saw it in an inventory, enough to reach it again after it left the file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedHost {
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<PathBuf>,
}

impl AppliedHost {
    pub fn from_target(t: &Target) -> Self {
        Self {
            name: t.name.clone(),
            host: t.host.clone(),
            user: t.user.clone(),
            port: t.port,
            identity: t.identity.clone(),
        }
    }

    pub fn to_target(&self) -> Target {
        Target {
            name: self.name.clone(),
            host: self.host.clone(),
            user: self.user.clone(),
            port: self.port,
            identity: self.identity.clone(),
            ..Default::default()
        }
    }
}

/// What `fluxctl` remembers between runs, in one small file (`~/.config/fluxvm/mac-fleet-state.json`, or
/// `$FLUXVM_MAC_FLEET_STATE`). Holds no secret.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FleetState {
    #[serde(default)]
    pub drained: BTreeMap<String, Drained>,
    /// Inventory file (canonical path) to the hosts its last `apply` converged.
    #[serde(default)]
    pub applied: BTreeMap<String, Vec<AppliedHost>>,
}

pub fn state_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_FLEET_STATE") {
        return Ok(p.into());
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/fluxvm/mac-fleet-state.json"))
}

impl FleetState {
    pub fn load(p: &Path) -> Result<Self> {
        match std::fs::read_to_string(p) {
            Ok(t) => serde_json::from_str(&t).with_context(|| format!("reading {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
        }
    }

    /// Atomic: write a sibling file, then rename.
    pub fn save(&self, p: &Path) -> Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, p)?;
        Ok(())
    }

    pub fn is_drained(&self, name: &str) -> bool {
        self.drained.contains_key(name)
    }

    pub fn drain(&mut self, name: &str, now: u64) -> bool {
        self.drained
            .insert(name.into(), Drained { since_unix: now })
            .is_none()
    }

    pub fn resume(&mut self, name: &str) -> bool {
        self.drained.remove(name).is_some()
    }

    /// Hosts the last `apply` of `inventory` configured that the inventory no longer lists.
    pub fn left_inventory(&self, inventory: &str, now_hosts: &[Target]) -> Vec<AppliedHost> {
        self.applied
            .get(inventory)
            .map(|prev| {
                prev.iter()
                    .filter(|p| {
                        !now_hosts
                            .iter()
                            .any(|t| t.name == p.name || (t.host == p.host && t.port == p.port))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A `--host-label`/`--label` value for placement: `key=value` or a bare word, no shell characters.
pub fn check_label(l: &str) -> Result<()> {
    if !(valid_label(l)
        || (!l.is_empty()
            && l.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.=/".contains(&c))))
    {
        bail!("label {l:?} must be a word or key=value without spaces or shell characters");
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const D1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    pub const D2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

    pub fn view(name: &str, templates: &[(&str, &str)]) -> HostView {
        HostView {
            name: name.into(),
            label: format!("admin@{name}"),
            caps: AppleHostCaps {
                free_cpu: 8,
                free_memory_mib: 24_000,
                ..Default::default()
            },
            disk_free_gib: Some(200),
            on_ac: Some(true),
            templates: templates
                .iter()
                .map(|(r, d)| HeldTemplate {
                    reference: (*r).into(),
                    digest: (*d).into(),
                })
                .collect(),
            templates_known: true,
            ..Default::default()
        }
    }

    pub fn job(t: &str) -> JobSpec {
        JobSpec {
            template: TemplateRef::parse(t).unwrap(),
            cpus: 4,
            memory_mib: 8192,
            disk_gib: 0,
            labels: vec![],
            ac_only: false,
            host: None,
        }
    }

    fn pinned() -> String {
        format!("reg.example/xcode:15@{D1}")
    }

    #[test]
    fn template_refs_normalise_and_validate_the_digest() {
        let t = TemplateRef::parse(&pinned()).unwrap();
        assert_eq!(t.name, "reg.example/xcode:15");
        assert_eq!(t.digest.as_deref(), Some(D1));
        assert_eq!(TemplateRef::parse("xcode").unwrap().name, "xcode:latest");
        assert!(TemplateRef::parse("x:1@sha256:abc").is_err());
        assert!(TemplateRef::parse("x:1@md5:abc").is_err());
        assert!(TemplateRef::parse("x'y").is_err());
        assert!(TemplateRef::parse("x:1").unwrap().require_digest().is_err());
        assert_eq!(TemplateRef::parse(&pinned()).unwrap().to_string(), pinned());
    }

    #[test]
    fn holds_tells_exact_from_stale_from_missing() {
        let want = TemplateRef::parse(&pinned()).unwrap();
        let h = |d: &str| {
            vec![HeldTemplate {
                reference: "reg.example/xcode:15".into(),
                digest: d.into(),
            }]
        };
        assert_eq!(holds(&h(D1), &want), Holds::Yes);
        assert_eq!(holds(&h(D2), &want), Holds::Stale(D2.into()));
        assert_eq!(holds(&h(""), &want), Holds::Stale("local import".into()));
        assert_eq!(holds(&[], &want), Holds::No);
        let unpinned = TemplateRef::parse("reg.example/xcode:15").unwrap();
        assert_eq!(holds(&h(D2), &unpinned), Holds::Yes);
    }

    #[test]
    fn placement_prefers_the_mac_that_already_holds_the_template() {
        let t = pinned();
        let views = vec![
            view("a", &[("reg.example/xcode:15", D1)]),
            view("b", &[("reg.example/xcode:15", D1)]),
        ];
        // b is the tighter fit (less free), a holds it too: equal bonus, fit decides.
        let mut v = views.clone();
        v[1].caps.free_cpu = 4;
        v[1].caps.free_memory_mib = 9000;
        let d = decide(&v, &job(&t), &Leases::default(), &FleetState::default(), 0);
        assert!(matches!(d, Decision::Place(c) if c.index == 1));
        // a Mac without the template never wins, even when it fits tighter.
        let mut v = views;
        v[0].templates.clear();
        v[1].caps.free_cpu = 8;
        let d = decide(&v, &job(&t), &Leases::default(), &FleetState::default(), 0);
        assert!(matches!(d, Decision::Place(c) if c.index == 1));
    }

    #[test]
    fn a_missing_or_stale_template_is_a_permanent_reason_that_names_the_fix() {
        let t = pinned();
        let v = vec![view("a", &[]), view("b", &[("reg.example/xcode:15", D2)])];
        match decide(&v, &job(&t), &Leases::default(), &FleetState::default(), 0) {
            Decision::Never(b) => {
                let text = explain(&b);
                assert!(text.contains("a: does not hold"), "{text}");
                assert!(
                    text.contains("b: holds reg.example/xcode:15 at sha256:2222"),
                    "{text}"
                );
                assert!(text.contains("templates distribute"), "{text}");
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn two_macos_slots_means_wait_not_fail() {
        let t = pinned();
        let mut a = view("a", &[("reg.example/xcode:15", D1)]);
        a.caps.macos_guests = 2;
        match decide(
            &[a],
            &job(&t),
            &Leases::default(),
            &FleetState::default(),
            0,
        ) {
            Decision::Wait(b) => assert!(explain(&b).contains("both macOS slots"), "{b:?}"),
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn leases_hold_a_slot_until_released_or_expired() {
        let t = pinned();
        let mut a = view("a", &[("reg.example/xcode:15", D1)]);
        a.caps.macos_guests = 1;
        let j = job(&t);
        let mut l = Leases::default();
        l.acquire("job-1", "a", &j, 100);
        assert!(matches!(
            decide(&[a.clone()], &j, &l, &FleetState::default(), 101),
            Decision::Wait(_)
        ));
        // expired: the probe is trusted again
        assert!(matches!(
            decide(
                &[a.clone()],
                &j,
                &l,
                &FleetState::default(),
                100 + LEASE_TTL_SECS
            ),
            Decision::Place(_)
        ));
        l.release("job-1");
        assert!(matches!(
            decide(&[a], &j, &l, &FleetState::default(), 101),
            Decision::Place(_)
        ));
    }

    #[test]
    fn drain_labels_disk_power_and_pinning_filter_hosts() {
        let t = pinned();
        let mut a = view("a", &[("reg.example/xcode:15", D1)]);
        a.labels = vec!["zone=lab".into()];
        let mut j = job(&t);
        let mut st = FleetState::default();
        st.drain("a", 1);
        assert!(evaluate(&a, &j, Usage::default(), st.is_drained("a")).is_err());
        j.labels = vec!["zone=lab".into(), "gpu".into()];
        let e = evaluate(&a, &j, Usage::default(), false).unwrap_err();
        assert!(e[0].text.contains("lacks label gpu"), "{e:?}");
        j.labels = vec!["zone=lab".into()];
        assert!(evaluate(&a, &j, Usage::default(), false).is_ok());
        j.disk_gib = 500;
        assert!(evaluate(&a, &j, Usage::default(), false).is_err());
        j.disk_gib = 0;
        a.on_ac = Some(false);
        j.ac_only = true;
        assert!(evaluate(&a, &j, Usage::default(), false).is_err());
        a.on_ac = Some(true);
        j.host = Some("b".into());
        assert!(evaluate(&a, &j, Usage::default(), false).is_err());
    }

    #[test]
    fn an_unreachable_mac_is_a_transient_reason() {
        let target = Target::parse("admin@down.local").unwrap();
        let v = HostView::unavailable(&target, "ssh: Connection refused");
        let e = evaluate(&v, &job(&pinned()), Usage::default(), false).unwrap_err();
        assert_eq!(e[0].kind, Kind::Transient);
    }

    #[test]
    fn facts_become_free_capacity_and_count_every_slot_holding_macos_guest() {
        let target = Target::parse("admin@m.local").unwrap();
        let vms = serde_json::json!({"items": [
            {"status": "running", "request": {"vcpus": 4, "memory_mib": 8192, "apple": {"guest_os": "macos"}}},
            {"status": "stopped", "request": {"vcpus": 4, "memory_mib": 8192, "apple": {"guest_os": "macos"}}},
            {"status": "running", "request": {"vcpus": 2, "memory_mib": 2048, "apple": {"guest_os": "linux"}}},
        ]});
        let facts = Facts {
            known: true,
            fluxctl_version: Some("0.4.21".into()),
            daemon_healthy: true,
            vms_json: Some(vms),
            disk_free_gib: Some(300),
            ..Default::default()
        };
        let sf = parse_sched_facts(&format!(
            "ncpu=10\nmemsize={}\npower=ac\nimages_json=[{{\"reference\":\"x:1\",\"digest\":\"{D1}\"}}]\n",
            32u64 * 1024 * 1024 * 1024
        ));
        let v = HostView::from_facts(&target, &facts, &sf);
        assert_eq!(v.unavailable, None);
        assert_eq!(v.caps.macos_guests, 1);
        assert_eq!(v.caps.free_cpu, 4);
        // 32 GiB total, 8 GiB headroom, 10 GiB used by guests
        assert_eq!(v.caps.free_memory_mib, 24_576 - 10_240);
        assert_eq!(v.on_ac, Some(true));
        assert_eq!(v.templates[0].reference, "x:1");
        let down = Facts {
            known: true,
            fluxctl_version: Some("0.4.21".into()),
            ..Default::default()
        };
        assert!(
            HostView::from_facts(&target, &down, &sf)
                .unavailable
                .is_some()
        );
    }

    #[test]
    fn state_round_trips_and_finds_hosts_that_left_the_inventory() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/state.json");
        assert_eq!(FleetState::load(&p).unwrap(), FleetState::default());
        let mut s = FleetState::default();
        assert!(s.drain("mini-1", 5));
        assert!(!s.drain("mini-1", 6));
        let a = Target::parse("admin@mini-1.local").unwrap();
        let b = Target::parse("admin@mini-2.local").unwrap();
        s.applied.insert(
            "/inv.yaml".into(),
            vec![AppliedHost::from_target(&a), AppliedHost::from_target(&b)],
        );
        s.save(&p).unwrap();
        let mut back = FleetState::load(&p).unwrap();
        assert_eq!(back, s);
        let left = back.left_inventory("/inv.yaml", std::slice::from_ref(&a));
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].host, "mini-2.local");
        assert!(back.left_inventory("/other.yaml", &[]).is_empty());
        assert!(back.resume("mini-1"));
        assert!(!back.resume("mini-1"));
    }
}
