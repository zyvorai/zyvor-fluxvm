// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Create-path admission helpers: image containment, signed catalog names,
//! and O(1) quota checks against a precomputed ledger. Pause, resume, and
//! delete do not call these.

use crate::config::Policy;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageTotals {
    pub vms: u64,
    pub vcpus: u64,
    pub memory_mib: u64,
}

impl UsageTotals {
    pub fn add(&mut self, vcpus: u8, memory_mib: u64) {
        self.vms = self.vms.saturating_add(1);
        self.vcpus = self.vcpus.saturating_add(u64::from(vcpus));
        self.memory_mib = self.memory_mib.saturating_add(memory_mib);
    }

    pub fn sub(&mut self, vcpus: u8, memory_mib: u64) {
        self.vms = self.vms.saturating_sub(1);
        self.vcpus = self.vcpus.saturating_sub(u64::from(vcpus));
        self.memory_mib = self.memory_mib.saturating_sub(memory_mib);
    }
}

/// Running totals maintained beside the VM store. Admission reads this
/// instead of scanning every VM. `host.vms` is the consistency check: if it
/// does not match the store length, the caller rebuilds.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuotaLedger {
    pub host: UsageTotals,
    pub tenants: BTreeMap<String, UsageTotals>,
    pub tokens: BTreeMap<String, UsageTotals>,
    /// vCPU and memory held by processes that are not VM records, such as
    /// a migration receiver. `vms` on this total stays 0 so the warm check
    /// (`host.vms` against the store length) still applies.
    #[serde(default)]
    pub untracked_host: UsageTotals,
    /// Batches admitted but not yet recorded (a fork or branch start in progress): tenant and token totals they will
    /// add. Held apart from `tenants` and `tokens` so a rebuild from the VM records keeps them (see
    /// [`ledger_with_reservations`]) and the records' own totals stay exact.
    #[serde(default)]
    pub reserved_tenants: BTreeMap<String, UsageTotals>,
    #[serde(default)]
    pub reserved_tokens: BTreeMap<String, UsageTotals>,
}

/// The ledger admission decides against: the records' totals plus every batch that has been admitted but not yet
/// recorded, per tenant and token (host vCPUs and memory are in `untracked_host`, see [`ledger_for_host_admission`]).
pub fn ledger_with_reservations(ledger: &QuotaLedger) -> QuotaLedger {
    let mut view = ledger_for_host_admission(ledger);
    for (k, r) in &ledger.reserved_tenants {
        let e = view.tenants.entry(k.clone()).or_default();
        e.vms = e.vms.saturating_add(r.vms);
        e.vcpus = e.vcpus.saturating_add(r.vcpus);
        e.memory_mib = e.memory_mib.saturating_add(r.memory_mib);
    }
    for (k, r) in &ledger.reserved_tokens {
        let e = view.tokens.entry(k.clone()).or_default();
        e.vms = e.vms.saturating_add(r.vms);
        e.vcpus = e.vcpus.saturating_add(r.vcpus);
        e.memory_mib = e.memory_mib.saturating_add(r.memory_mib);
    }
    view
}

/// A batch of identical VMs (a fork's children) to admit as one unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchReservation {
    pub count: u64,
    pub vcpus: u8,
    pub memory_mib: u64,
    pub tenant: Option<String>,
    /// The API token's name; `None` for no token or the anonymous admin (no token quota applies).
    pub token: Option<String>,
}

impl QuotaLedger {
    /// Admits `b` against the host, tenant and token limits **as a whole** and records it as reserved, or refuses with
    /// the first limit it would break and changes nothing. Called under the store lock, so two batches racing each other
    /// cannot both be admitted into room that only one fits.
    pub fn reserve_batch(
        &mut self,
        b: &BatchReservation,
        policy: &Policy,
        max_vms_per_token: Option<usize>,
        max_memory_mib_per_token: Option<u64>,
    ) -> Result<()> {
        let mut view = ledger_with_reservations(self);
        // `enforce_*` price one more VM; add the other `count - 1` to the view first.
        let extra = b.count.saturating_sub(1);
        if extra > 0 {
            let (v, m) = (u64::from(b.vcpus) * extra, b.memory_mib * extra);
            view.host.vcpus = view.host.vcpus.saturating_add(v);
            view.host.memory_mib = view.host.memory_mib.saturating_add(m);
            if let Some(t) = b.tenant.as_deref() {
                let e = view.tenants.entry(t.to_string()).or_default();
                e.vms = e.vms.saturating_add(extra);
                e.vcpus = e.vcpus.saturating_add(v);
                e.memory_mib = e.memory_mib.saturating_add(m);
            }
            if let Some(k) = b.token.as_deref() {
                let e = view.tokens.entry(k.to_string()).or_default();
                e.vms = e.vms.saturating_add(extra);
                e.vcpus = e.vcpus.saturating_add(v);
                e.memory_mib = e.memory_mib.saturating_add(m);
            }
        }
        if let Some(t) = b.tenant.as_deref()
            && !policy.tenants.is_empty()
        {
            enforce_tenant_totals(t, b.vcpus, b.memory_mib, policy, &view)?;
        }
        enforce_host_totals(b.vcpus, b.memory_mib, policy, &view)?;
        if let Some(k) = b.token.as_deref() {
            enforce_token_totals(
                k,
                b.vcpus,
                b.memory_mib,
                max_vms_per_token,
                max_memory_mib_per_token,
                &view,
            )?;
        }
        let (v, m) = (u64::from(b.vcpus) * b.count, b.memory_mib * b.count);
        self.untracked_host.vcpus = self.untracked_host.vcpus.saturating_add(v);
        self.untracked_host.memory_mib = self.untracked_host.memory_mib.saturating_add(m);
        let bump = |e: &mut UsageTotals| {
            e.vms = e.vms.saturating_add(b.count);
            e.vcpus = e.vcpus.saturating_add(v);
            e.memory_mib = e.memory_mib.saturating_add(m);
        };
        if let Some(t) = b.tenant.as_deref() {
            bump(self.reserved_tenants.entry(t.to_string()).or_default());
        }
        if let Some(k) = b.token.as_deref() {
            bump(self.reserved_tokens.entry(k.to_string()).or_default());
        }
        Ok(())
    }

    /// Returns what [`QuotaLedger::reserve_batch`] reserved. Never underflows.
    pub fn release_batch(&mut self, b: &BatchReservation) {
        let (v, m) = (u64::from(b.vcpus) * b.count, b.memory_mib * b.count);
        self.untracked_host.vcpus = self.untracked_host.vcpus.saturating_sub(v);
        self.untracked_host.memory_mib = self.untracked_host.memory_mib.saturating_sub(m);
        let drop_from = |map: &mut BTreeMap<String, UsageTotals>, key: &str| {
            if let Some(e) = map.get_mut(key) {
                e.vms = e.vms.saturating_sub(b.count);
                e.vcpus = e.vcpus.saturating_sub(v);
                e.memory_mib = e.memory_mib.saturating_sub(m);
                if e.vms == 0 && e.vcpus == 0 && e.memory_mib == 0 {
                    map.remove(key);
                }
            }
        };
        if let Some(t) = b.tenant.as_deref() {
            drop_from(&mut self.reserved_tenants, t);
        }
        if let Some(k) = b.token.as_deref() {
            drop_from(&mut self.reserved_tokens, k);
        }
    }
}

/// Host admission view: VM totals plus reservations that are not VM records.
pub fn ledger_for_host_admission(ledger: &QuotaLedger) -> QuotaLedger {
    let mut view = ledger.clone();
    view.host.vcpus = view.host.vcpus.saturating_add(ledger.untracked_host.vcpus);
    view.host.memory_mib = view
        .host
        .memory_mib
        .saturating_add(ledger.untracked_host.memory_mib);
    view
}

impl QuotaLedger {
    pub fn ingest(
        &mut self,
        tenant: Option<&str>,
        token: Option<&str>,
        vcpus: u8,
        memory_mib: u64,
    ) {
        self.host.add(vcpus, memory_mib);
        if let Some(tenant) = tenant.map(str::trim).filter(|s| !s.is_empty()) {
            self.tenants
                .entry(tenant.to_string())
                .or_default()
                .add(vcpus, memory_mib);
        }
        if let Some(token) = token.map(str::trim).filter(|s| !s.is_empty()) {
            self.tokens
                .entry(token.to_string())
                .or_default()
                .add(vcpus, memory_mib);
        }
    }

    pub fn release(
        &mut self,
        tenant: Option<&str>,
        token: Option<&str>,
        vcpus: u8,
        memory_mib: u64,
    ) {
        self.host.sub(vcpus, memory_mib);
        if let Some(tenant) = tenant.map(str::trim).filter(|s| !s.is_empty()) {
            let empty = self.tenants.get_mut(tenant).is_some_and(|slot| {
                slot.sub(vcpus, memory_mib);
                slot.vms == 0
            });
            if empty {
                self.tenants.remove(tenant);
            }
        }
        if let Some(token) = token.map(str::trim).filter(|s| !s.is_empty()) {
            let empty = self.tokens.get_mut(token).is_some_and(|slot| {
                slot.sub(vcpus, memory_mib);
                slot.vms == 0
            });
            if empty {
                self.tokens.remove(token);
            }
        }
    }
}

/// Walk `path`, resolving each existing component so a symlink cannot escape
/// `dir`. Components that do not exist yet stay lexical, which is what a
/// not-yet-created image path needs.
pub fn resolve_existing(path: &Path) -> PathBuf {
    let mut acc = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => acc.push(prefix.as_os_str()),
            Component::RootDir => acc.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                acc.pop();
            }
            Component::Normal(seg) => {
                acc.push(seg);
                if acc.exists()
                    && let Ok(canon) = std::fs::canonicalize(&acc)
                {
                    acc = canon;
                }
            }
        }
    }
    acc
}

pub fn path_within(path: &Path, dir: &Path) -> bool {
    let path = resolve_existing(path);
    let dir = resolve_existing(dir);
    path.starts_with(&dir)
}

/// True when `path` is under `dir` after collapsing `.` and `..`, before
/// symlink resolution. A symlink that escapes still claims the allowlist so
/// the caller can fail closed instead of copying the target.
pub fn lexical_within(path: &Path, dir: &Path) -> bool {
    let mut acc = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => acc.push(prefix.as_os_str()),
            Component::RootDir => acc.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                acc.pop();
            }
            Component::Normal(seg) => acc.push(seg),
        }
    }
    acc.starts_with(dir)
}

pub fn image_within_allowed(image: &Path, dirs: &[PathBuf]) -> bool {
    dirs.iter().any(|dir| path_within(image, dir))
}

/// Guest path for an allowlisted hostPath bind. `None` when `source` is
/// outside `allow` after symlink resolution.
pub fn hostpath_guest(source: &Path, allow: &Path) -> Option<PathBuf> {
    let source_r = resolve_existing(source);
    let allow_r = resolve_existing(allow);
    let rel = source_r.strip_prefix(&allow_r).ok()?;
    Some(Path::new("/run/fluxvm/hostpath-allow").join(rel))
}

/// `require_catalog_names` accepts only a catalog entry that verified
/// against `[[catalog.trusted_signers]]`.
pub fn require_signed_catalog(
    require: bool,
    from_catalog: bool,
    signed_by: Option<&str>,
) -> Result<()> {
    if !require {
        return Ok(());
    }
    if from_catalog && signed_by.is_some_and(|s| !s.is_empty()) {
        return Ok(());
    }
    bail!(
        "policy.require_catalog_names requires a catalog name signed by [[catalog.trusted_signers]] (literal paths and unsigned entries are rejected)"
    );
}

pub fn unsigned_catalog_insert_allowed(trusted_signer_count: usize) -> bool {
    trusted_signer_count == 0
}

pub fn enforce_tenant_totals(
    tenant: &str,
    vcpus: u8,
    memory_mib: u64,
    policy: &Policy,
    ledger: &QuotaLedger,
) -> Result<()> {
    let Some(tp) = policy.tenants.iter().find(|t| t.tenant == tenant) else {
        return Ok(());
    };
    let used = ledger.tenants.get(tenant).cloned().unwrap_or_default();
    if let Some(max) = tp.max_vms_total
        && used.vms >= max as u64
    {
        bail!("tenant '{tenant}' is already at max_vms_total ({max})");
    }
    if let Some(max) = tp.max_vcpus_total {
        let max = u64::from(max);
        let next = used.vcpus.saturating_add(u64::from(vcpus));
        if next > max {
            bail!(
                "tenant '{tenant}' would exceed max_vcpus_total ({max}): {} already used + {vcpus} requested",
                used.vcpus
            );
        }
    }
    if let Some(max) = tp.max_memory_mib_total {
        let next = used.memory_mib.saturating_add(memory_mib);
        if next > max {
            bail!(
                "tenant '{tenant}' would exceed max_memory_mib_total ({max}): {} already used + {memory_mib} requested",
                used.memory_mib
            );
        }
    }
    Ok(())
}

pub fn enforce_token_totals(
    actor: &str,
    vcpus: u8,
    memory_mib: u64,
    max_vms: Option<usize>,
    max_memory_mib: Option<u64>,
    ledger: &QuotaLedger,
) -> Result<()> {
    let _ = vcpus;
    let used = ledger.tokens.get(actor).cloned().unwrap_or_default();
    if let Some(max) = max_vms
        && used.vms >= max as u64
    {
        bail!("token '{actor}' at max_vms_per_token ({max})");
    }
    if let Some(max_mem) = max_memory_mib {
        let next = used.memory_mib.saturating_add(memory_mib);
        if next > max_mem {
            bail!("token '{actor}' would exceed max_memory_mib_per_token ({max_mem})");
        }
    }
    Ok(())
}

pub fn enforce_host_totals(
    vcpus: u8,
    memory_mib: u64,
    policy: &Policy,
    ledger: &QuotaLedger,
) -> Result<()> {
    if let Some(max) = policy.max_vcpus_host {
        let next = ledger.host.vcpus.saturating_add(u64::from(vcpus));
        if next > u64::from(max) {
            bail!(
                "host would exceed policy.max_vcpus_host ({max}): {} already used + {vcpus} requested",
                ledger.host.vcpus
            );
        }
    }
    if let Some(max) = policy.max_memory_mib_host {
        let next = ledger.host.memory_mib.saturating_add(memory_mib);
        if next > max {
            bail!(
                "host would exceed policy.max_memory_mib_host ({max}): {} already used + {memory_mib} requested",
                ledger.host.memory_mib
            );
        }
    }
    Ok(())
}

/// True when a persisted ledger matches the store length and can be trusted
/// without rebuilding from every VM record.
pub fn ledger_is_warm(ledger_vms: u64, store_len: u64) -> bool {
    ledger_vms == store_len
}

pub fn format_audit_record(event: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = format!("event={event}");
    for (k, v) in pairs {
        out.push(' ');
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// How a VMM child filter is installed. Default is log, never a notify broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmmSeccompMode {
    Off,
    Log,
    Kill,
}

pub fn vmm_seccomp_mode_from(value: Option<&str>) -> VmmSeccompMode {
    match value.unwrap_or("log") {
        "off" | "0" | "false" => VmmSeccompMode::Off,
        "kill" => VmmSeccompMode::Kill,
        _ => VmmSeccompMode::Log,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TenantPolicy;
    use std::fs;

    #[test]
    fn lexical_dotdot_does_not_escape_allowed_dir() {
        let dir = Path::new("/var/lib/fluxvm/images");
        assert!(path_within(
            Path::new("/var/lib/fluxvm/images/base.qcow2"),
            dir
        ));
        assert!(!path_within(Path::new("/tmp/evil.qcow2"), dir));
        assert!(!path_within(
            Path::new("/var/lib/fluxvm/images/../../../../etc/passwd"),
            dir
        ));
        assert!(lexical_within(
            Path::new("/var/lib/fluxvm/images/escaped-link"),
            dir
        ));
        assert!(!lexical_within(
            Path::new("/var/lib/fluxvm/images/../../../../etc/passwd"),
            dir
        ));
    }

    #[test]
    fn symlink_outside_allowlist_is_rejected() {
        let root = std::env::temp_dir().join(format!("fluxvm-policy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let allow = root.join("allow");
        let outside = root.join("outside");
        fs::create_dir_all(&allow).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret");
        fs::write(&secret, b"no").unwrap();
        let link = allow.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        #[cfg(unix)]
        assert!(hostpath_guest(&link, &allow).is_none());
        assert!(hostpath_guest(&allow.join("missing.img"), &allow).is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn signed_catalog_names_fail_closed() {
        assert!(require_signed_catalog(false, false, None).is_ok());
        assert!(require_signed_catalog(true, true, Some("build")).is_ok());
        assert!(require_signed_catalog(true, true, None).is_err());
        assert!(require_signed_catalog(true, false, Some("build")).is_err());
        assert!(!unsigned_catalog_insert_allowed(1));
        assert!(unsigned_catalog_insert_allowed(0));
    }

    #[test]
    fn tenant_token_and_host_caps_match_the_old_messages() {
        let mut policy = Policy::default();
        policy.tenants.push(TenantPolicy {
            tenant: "acme".into(),
            max_vcpus_total: Some(2),
            max_memory_mib_total: Some(1024),
            max_vms_total: Some(1),
        });
        policy.max_vcpus_host = Some(4);
        policy.max_memory_mib_host = Some(2048);
        let mut ledger = QuotaLedger::default();
        ledger.ingest(Some("acme"), Some("tok"), 2, 512);
        let err = enforce_tenant_totals("acme", 1, 128, &policy, &ledger)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_vms_total"));
        ledger.tenants.get_mut("acme").unwrap().vms = 0;
        let err = enforce_tenant_totals("acme", 1, 128, &policy, &ledger)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_vcpus_total"));
        let err = enforce_token_totals("tok", 1, 800, Some(1), Some(1000), &ledger)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_vms_per_token"));
        let err = enforce_host_totals(4, 64, &policy, &ledger)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_vcpus_host"));
        assert!(ledger_is_warm(ledger.host.vms, 1));
        assert!(!ledger_is_warm(ledger.host.vms, 2));
    }

    #[test]
    fn audit_record_carries_event_and_digest() {
        let line = format_audit_record(
            "vm.create",
            &[
                ("tenant", "acme"),
                ("image_sha256", "abc"),
                ("signed_by", "build"),
            ],
        );
        assert!(line.contains("event=vm.create"));
        assert!(line.contains("image_sha256=abc"));
        assert!(line.contains("signed_by=build"));
    }

    #[test]
    fn vmm_seccomp_defaults_to_log() {
        assert_eq!(vmm_seccomp_mode_from(None), VmmSeccompMode::Log);
        assert_eq!(vmm_seccomp_mode_from(Some("kill")), VmmSeccompMode::Kill);
        assert_eq!(vmm_seccomp_mode_from(Some("off")), VmmSeccompMode::Off);
    }

    fn batch(count: u64, tenant: Option<&str>, token: Option<&str>) -> BatchReservation {
        BatchReservation {
            count,
            vcpus: 2,
            memory_mib: 1024,
            tenant: tenant.map(str::to_string),
            token: token.map(str::to_string),
        }
    }

    #[test]
    fn a_reserved_batch_is_seen_by_the_next_one_so_two_cannot_share_room_for_one() {
        let policy = Policy {
            max_memory_mib_host: Some(4096),
            ..Default::default()
        };
        let mut l = QuotaLedger::default();
        l.reserve_batch(&batch(3, None, None), &policy, None, None)
            .unwrap();
        // 3 GiB are held; another 2 GiB batch does not fit although nothing has been recorded yet.
        let err = l
            .reserve_batch(&batch(2, None, None), &policy, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_memory_mib_host"), "{err}");
        assert_eq!(
            l.untracked_host.memory_mib,
            3 * 1024,
            "a refusal changes nothing"
        );
        // One more does fit; releasing the first makes room for the 2-batch again.
        l.reserve_batch(&batch(1, None, None), &policy, None, None)
            .unwrap();
        l.release_batch(&batch(3, None, None));
        l.reserve_batch(&batch(2, None, None), &policy, None, None)
            .unwrap();
        assert_eq!(l.untracked_host.memory_mib, 3 * 1024);
    }

    #[test]
    fn a_batch_is_priced_whole_against_the_tenant() {
        let policy = Policy {
            tenants: vec![TenantPolicy {
                tenant: "acme".into(),
                max_vms_total: Some(4),
                max_vcpus_total: None,
                max_memory_mib_total: None,
            }],
            ..Default::default()
        };
        let mut l = QuotaLedger::default();
        l.ingest(Some("acme"), None, 2, 1024);
        l.ingest(Some("acme"), None, 2, 1024);
        // Two VMs exist, so a batch of 3 would make 5: refused as a whole, not child by child.
        let err = l
            .reserve_batch(&batch(3, Some("acme"), None), &policy, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_vms_total"), "{err}");
        assert!(l.reserved_tenants.is_empty());
        l.reserve_batch(&batch(2, Some("acme"), None), &policy, None, None)
            .unwrap();
        // The two reserved count: one more VM is now over the limit of 4.
        assert!(
            l.reserve_batch(&batch(1, Some("acme"), None), &policy, None, None)
                .is_err()
        );
        // Other tenants are unaffected.
        l.reserve_batch(&batch(5, Some("other"), None), &policy, None, None)
            .unwrap();
        l.release_batch(&batch(2, Some("acme"), None));
        assert!(
            !l.reserved_tenants.contains_key("acme"),
            "an empty entry is dropped"
        );
    }

    #[test]
    fn a_batch_is_priced_whole_against_the_token() {
        let policy = Policy::default();
        let mut l = QuotaLedger::default();
        l.ingest(None, Some("ci"), 2, 1024);
        // max_vms_per_token 3 with one VM already: a batch of 3 makes 4. The old per-child loop priced one VM.
        assert!(
            l.reserve_batch(&batch(3, None, Some("ci")), &policy, Some(3), None)
                .is_err()
        );
        l.reserve_batch(&batch(2, None, Some("ci")), &policy, Some(3), None)
            .unwrap();
        // Memory: 1 existing + 2 reserved = 3 GiB; the cap of 3 GiB leaves nothing.
        assert!(
            l.reserve_batch(&batch(1, None, Some("ci")), &policy, None, Some(3 * 1024))
                .is_err()
        );
        // No token (anonymous admin) is never limited by token quotas.
        l.reserve_batch(&batch(9, None, None), &policy, Some(1), Some(1))
            .unwrap();
    }

    #[test]
    fn reservations_survive_a_json_round_trip_and_an_old_ledger_has_none() {
        let mut l = QuotaLedger::default();
        l.reserve_batch(
            &batch(2, Some("t"), Some("k")),
            &Policy::default(),
            None,
            None,
        )
        .unwrap();
        let back: QuotaLedger = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        let old: QuotaLedger = serde_json::from_str(
            r#"{"host":{"vms":0,"vcpus":0,"memory_mib":0},"tenants":{},"tokens":{}}"#,
        )
        .unwrap();
        assert!(old.reserved_tenants.is_empty() && old.reserved_tokens.is_empty());
    }
}
