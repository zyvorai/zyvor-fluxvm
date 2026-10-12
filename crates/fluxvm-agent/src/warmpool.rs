// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Warm pools: VMs booted ahead of time so a launch (a macOS CI job, say) starts in seconds instead of minutes.
//!
//! A pool names a catalog image and a size and keeps `min_ready` VMs of it booted and idle. A `POST /fleet/vms` whose
//! image and size match a ready warm VM of a pool the caller may use (or that names the pool with `"pool"`) *claims*
//! that VM: central renames it, hands it to the caller and answers at once with `"warm": true`; the reconciler then
//! boots a replacement.
//!
//! Two kinds of pool, because of Apple's macOS licence (two macOS guests per Mac) and exclusive leases:
//!
//! - **Tenant pools** belong to one tenant, run only on Macs that tenant leases, count against its quota and are billed
//!   to it like any of its VMs. Tenants manage them themselves.
//! - **Shared pools** belong to the operator and run only on Macs nobody leases. Their warm VMs are no tenant's (not in
//!   any quota or bill) until claimed; a claimed VM is billed from the moment of the claim. A tenant can claim from a
//!   shared pool only where policy lets its VMs run on a shared host: never for macOS guests while
//!   `require_lease_for_macos` is on, and never while `isolate_tenant_networks` is on (a warm VM was booted on no
//!   tenant's network).
//!
//! This module holds the rules (validation, what to boot or trim, matching a launch to a pool, statistics); the HTTP
//! routes and the reconciler task are in `central_warmpool.rs`.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::tenancy::{CatalogImage, VmEntry};

/// Node label on a warm VM that has not been claimed yet; its value is the pool's name.
pub const WARM_LABEL: &str = "fluxvm.dev/warm-pool";
/// Node label on a VM that was claimed from a pool; its value is the pool's name.
pub const CLAIMED_LABEL: &str = "fluxvm.dev/warm-claimed";
/// Warm VMs are disposable, so a drain may simply delete them (`evict-ephemeral`).
pub const EPHEMERAL_LABEL: &str = "fluxvm.dev/ephemeral";
/// Field a create made by the reconciler carries, so it is never itself answered from a pool. Central strips it.
pub const FILL_MARKER: &str = "warm_pool_fill";
/// Most pools in a fleet.
pub const MAX_POOLS: usize = 64;
/// Most pools one tenant may own.
pub const MAX_POOLS_PER_TENANT: usize = 8;
/// Largest `min_ready`.
pub const MAX_MIN_READY: u32 = 32;
/// Largest `max_total`.
pub const MAX_TOTAL: u32 = 100;
/// Claims remembered per pool for its statistics (and never older than [`CLAIM_HISTORY_DAYS`]).
pub const MAX_CLAIM_HISTORY: usize = 1000;
pub const CLAIM_HISTORY_DAYS: i64 = 7;
/// A warm VM still booting after this long is replaced.
pub const BOOT_TIMEOUT_SECS: i64 = 900;
/// A warm VM a node does not list yet is left alone this long (the node may not have caught up).
pub const MISSING_GRACE_SECS: i64 = 60;
/// Most VMs one pool boots in one reconcile pass, so a big `min_ready` fills in steps instead of all at once.
pub const MAX_CREATES_PER_PASS: u32 = 4;
/// Longest wait between attempts after failures.
pub const MAX_BACKOFF_SECS: i64 = 600;
const MAX_SELECTOR_ENTRIES: usize = 16;
const MAX_NODES: usize = 64;

/// The size of each warm VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSize {
    pub vcpus: u32,
    pub memory_mib: u64,
}

/// One claim, for the pool's statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimRecord {
    pub at: DateTime<Utc>,
    /// How long central took to answer the launch, in milliseconds.
    pub ms: u64,
    /// How long the VM had been warm.
    pub warm_secs: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
}

/// What the reconciler knows about a pool between passes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PoolRuntime {
    /// Warm VMs (`node/id`) the last pass saw running; only these are handed out.
    #[serde(default)]
    pub ready: BTreeSet<String>,
    #[serde(default)]
    pub claims: VecDeque<ClaimRecord>,
    /// Failed fill attempts in a row.
    #[serde(default)]
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fill_at: Option<DateTime<Utc>>,
}

/// A warm pool (`/fleet/pools`), persisted in `fleet-tenancy.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pool {
    pub name: String,
    /// The owning tenant; `None` is a shared pool owned by the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// A catalog image name (`/fleet/images`).
    pub image: String,
    pub size: PoolSize,
    pub min_ready: u32,
    /// Most VMs from this pool alive at once: warm ones plus claimed ones still running. Bounds what a burst costs.
    pub max_total: u32,
    /// Only nodes whose labels include all of these.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    /// Only these nodes (empty: any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub runtime: PoolRuntime,
}

/// `POST /fleet/pools` body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolSpec {
    pub name: String,
    pub image: String,
    /// Operator only: create the pool for this tenant. A tenant's own pools are always its own.
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub vcpus: Option<u32>,
    #[serde(default)]
    pub memory_mib: Option<u64>,
    #[serde(default)]
    pub size: Option<PoolSize>,
    #[serde(default)]
    pub min_ready: Option<u32>,
    #[serde(default)]
    pub max_total: Option<u32>,
    #[serde(default)]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default)]
    pub nodes: Vec<String>,
}

/// `PATCH /fleet/pools/{name}` body. `image` and `size` may be sent back unchanged (a client echoing the pool) but not
/// changed: the warm VMs are already booted from them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolPatch {
    #[serde(default)]
    pub min_ready: Option<u32>,
    #[serde(default)]
    pub max_total: Option<u32>,
    #[serde(default)]
    pub node_selector: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub nodes: Option<Vec<String>>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub size: Option<PoolSize>,
    #[serde(default)]
    pub vcpus: Option<u32>,
    #[serde(default)]
    pub memory_mib: Option<u64>,
}

/// 1-40 of `a-z 0-9 -`, starting and ending with a letter or digit (warm VMs are named `warm-<pool>-<8 hex>`).
pub fn valid_pool_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// A VM name the node accepts (and so can rename a warm VM to): 1-63 of `A-Z a-z 0-9 . _ -`.
pub fn valid_vm_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn valid_label_part(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

/// The default `max_total` for a `min_ready`: room for a burst of claims to keep running while the pool refills.
pub fn default_max_total(min_ready: u32) -> u32 {
    (min_ready.saturating_mul(4))
        .max(min_ready.saturating_add(4))
        .min(MAX_TOTAL)
}

impl Pool {
    /// `operator`, or the owning tenant's id.
    pub fn owner(&self) -> &str {
        self.tenant.as_deref().unwrap_or("operator")
    }

    /// Builds a pool from a create request. `tenant` is who will own it (already decided by the caller's role).
    pub fn from_spec(
        spec: PoolSpec,
        tenant: Option<String>,
        image: Option<&CatalogImage>,
        now: DateTime<Utc>,
    ) -> Result<Pool, String> {
        let img = image.ok_or_else(|| {
            format!(
                "no image '{}' in the catalog (GET /fleet/images)",
                spec.image
            )
        })?;
        let size = spec.size.unwrap_or(PoolSize {
            vcpus: spec.vcpus.unwrap_or(u32::from(img.vcpus)),
            memory_mib: spec.memory_mib.unwrap_or(img.memory_mib),
        });
        let min_ready = spec.min_ready.unwrap_or(1);
        let pool = Pool {
            name: spec.name,
            tenant,
            image: spec.image,
            size,
            min_ready,
            max_total: spec
                .max_total
                .unwrap_or_else(|| default_max_total(min_ready)),
            node_selector: spec.node_selector,
            nodes: spec.nodes,
            created_at: now,
            runtime: PoolRuntime::default(),
        };
        pool.validate()?;
        Ok(pool)
    }

    /// Applies a `PATCH`, validating the result.
    pub fn apply(&mut self, patch: PoolPatch) -> Result<(), String> {
        if patch.image.as_ref().is_some_and(|i| *i != self.image) {
            return Err(
                "a pool's image cannot change (its warm VMs are booted from it): create a new pool"
                    .into(),
            );
        }
        let size = patch.size.unwrap_or(PoolSize {
            vcpus: patch.vcpus.unwrap_or(self.size.vcpus),
            memory_mib: patch.memory_mib.unwrap_or(self.size.memory_mib),
        });
        if size != self.size {
            return Err(
                "a pool's size cannot change (its warm VMs are booted at it): create a new pool"
                    .into(),
            );
        }
        let mut next = self.clone();
        if let Some(m) = patch.min_ready {
            next.min_ready = m;
            // Raising min_ready above max_total raises max_total with it, unless max_total is set in the same call.
            if patch.max_total.is_none() && next.max_total < m {
                next.max_total = m;
            }
        }
        if let Some(m) = patch.max_total {
            next.max_total = m;
        }
        if let Some(s) = patch.node_selector {
            next.node_selector = s;
        }
        if let Some(n) = patch.nodes {
            next.nodes = n;
        }
        next.validate()?;
        // A new target is worth trying at once, whatever the last attempts did.
        next.runtime.failures = 0;
        next.runtime.backoff_until = None;
        *self = next;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if !valid_pool_name(&self.name) {
            return Err(format!(
                "pool name '{}': use 1-40 lowercase letters, digits and '-', starting and ending with a letter or digit",
                self.name
            ));
        }
        if self.size.vcpus == 0 || self.size.vcpus > 64 {
            return Err("vcpus must be 1-64".into());
        }
        if self.size.memory_mib < 512 || self.size.memory_mib > 1024 * 1024 {
            return Err("memory_mib must be 512-1048576".into());
        }
        if self.min_ready > MAX_MIN_READY {
            return Err(format!("min_ready must be 0-{MAX_MIN_READY}"));
        }
        if self.max_total == 0 || self.max_total > MAX_TOTAL {
            return Err(format!("max_total must be 1-{MAX_TOTAL}"));
        }
        if self.max_total < self.min_ready {
            return Err(format!(
                "max_total ({}) must be at least min_ready ({})",
                self.max_total, self.min_ready
            ));
        }
        if self.node_selector.len() > MAX_SELECTOR_ENTRIES {
            return Err(format!(
                "node_selector: at most {MAX_SELECTOR_ENTRIES} labels"
            ));
        }
        for (k, v) in &self.node_selector {
            if !valid_label_part(k, 63) || !valid_label_part(v, 63) {
                return Err(format!(
                    "node_selector {k}={v}: use 1-63 of A-Z a-z 0-9 . _ - / for each"
                ));
            }
        }
        if self.nodes.len() > MAX_NODES {
            return Err(format!("nodes: at most {MAX_NODES}"));
        }
        if let Some(n) = self.nodes.iter().find(|n| !valid_label_part(n, 253)) {
            return Err(format!("nodes: '{n}' is not a node name"));
        }
        Ok(())
    }

    /// Whether a node with these labels may hold this pool's warm VMs (leases and capacity are checked elsewhere).
    pub fn allows_node(
        &self,
        name: &str,
        labels: &std::collections::HashMap<String, String>,
    ) -> bool {
        (self.nodes.is_empty() || self.nodes.iter().any(|n| n == name))
            && self
                .node_selector
                .iter()
                .all(|(k, v)| labels.get(k) == Some(v))
    }

    /// The create body for one warm VM of this pool.
    pub fn fill_body(&self, img: &CatalogImage, name: &str) -> Value {
        let mut b = json!({
            "name": name,
            "backend": "vz",
            "image": img.image,
            "vcpus": self.size.vcpus,
            "memory_mib": self.size.memory_mib,
            "apple": {"guest_os": img.guest_os},
        });
        b[FILL_MARKER] = json!(self.name);
        if let Some(d) = img.disk_size_gib {
            b["disk_size_gib"] = json!(d);
        }
        if let Some(t) = &self.tenant {
            b["tenant"] = json!(t);
        }
        b
    }

    /// Notes a claim for the statistics, keeping the history bounded.
    pub fn record_claim(&mut self, c: ClaimRecord) {
        let cutoff = c.at - Duration::days(CLAIM_HISTORY_DAYS);
        let claims = &mut self.runtime.claims;
        claims.push_back(c);
        while claims.len() > MAX_CLAIM_HISTORY || claims.front().is_some_and(|x| x.at < cutoff) {
            claims.pop_front();
        }
    }

    /// Notes a failed fill: waits longer after each failure in a row.
    pub fn record_failure(&mut self, msg: String, now: DateTime<Utc>) {
        self.runtime.failures = self.runtime.failures.saturating_add(1);
        self.runtime.backoff_until = Some(now + backoff(self.runtime.failures));
        self.runtime.last_error = Some(msg);
    }

    pub fn record_success(&mut self, now: DateTime<Utc>) {
        self.runtime.failures = 0;
        self.runtime.backoff_until = None;
        self.runtime.last_error = None;
        self.runtime.last_fill_at = Some(now);
    }

    pub fn backing_off(&self, now: DateTime<Utc>) -> bool {
        self.runtime.backoff_until.is_some_and(|t| t > now)
    }
}

/// The wait after `failures` failed attempts in a row: 5 s, 10 s, 20 s, … up to [`MAX_BACKOFF_SECS`].
pub fn backoff(failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(10);
    Duration::seconds((5i64 << exp).min(MAX_BACKOFF_SECS))
}

/// `node/id`, the key of a warm VM in [`PoolRuntime::ready`].
pub fn key(node: &str, vm_id: &str) -> String {
    format!("{node}/{vm_id}")
}

/// What a node says about a warm VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmState {
    Ready,
    Booting,
    /// Stopped, failed or otherwise not going to become ready by itself.
    Failed,
    /// The node does not list it.
    Missing,
}

/// Reads a node's VM record (`status` is a string, or an object keyed by the state on some daemons).
pub fn classify(vm: Option<&Value>) -> WarmState {
    let Some(vm) = vm else {
        return WarmState::Missing;
    };
    let st = vm
        .get("status")
        .and_then(|s| {
            s.as_str()
                .map(str::to_owned)
                .or_else(|| s.as_object().and_then(|o| o.keys().next().cloned()))
        })
        .or_else(|| vm.get("state").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
        .to_ascii_lowercase();
    match st.as_str() {
        "running" => WarmState::Ready,
        "failed" | "error" | "stopped" | "crashed" | "paused" | "hibernated" | "deleted" => {
            WarmState::Failed
        }
        _ => WarmState::Booting,
    }
}

/// A pool's warm (unclaimed) VM, as the reconciler and the views see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmVm {
    pub key: String,
    pub node: String,
    pub vm_id: String,
    pub name: String,
    pub ready: bool,
    pub since: DateTime<Utc>,
}

/// What one reconcile pass should do for a pool.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub create: u32,
    /// Warm VMs to delete (keys), because the pool holds more than `min_ready`.
    pub trim: Vec<String>,
}

/// Boot up to `min_ready` warm VMs while the pool's live VMs (warm plus `claimed_alive`) stay within `max_total`; trim
/// warm VMs above `min_ready`, booting ones first, then the newest.
pub fn plan(pool: &Pool, warm: &[WarmVm], claimed_alive: u32) -> Plan {
    let n = warm.len() as u32;
    if n > pool.min_ready {
        let mut order: Vec<&WarmVm> = warm.iter().collect();
        order.sort_by(|a, b| a.ready.cmp(&b.ready).then(b.since.cmp(&a.since)));
        return Plan {
            create: 0,
            trim: order
                .into_iter()
                .take((n - pool.min_ready) as usize)
                .map(|w| w.key.clone())
                .collect(),
        };
    }
    let want = pool.min_ready - n;
    let room = pool
        .max_total
        .saturating_sub(n.saturating_add(claimed_alive));
    Plan {
        create: want.min(room).min(MAX_CREATES_PER_PASS),
        trim: Vec::new(),
    }
}

/// The pool's warm VMs in the ledger (active, not claimed), with readiness from its runtime.
pub fn warm_vms(pool: &Pool, vms: &[VmEntry]) -> Vec<WarmVm> {
    vms.iter()
        .filter(|v| {
            v.active() && v.claimed_at.is_none() && v.warm_pool.as_deref() == Some(&pool.name)
        })
        .map(|v| {
            let k = key(&v.node, &v.vm_id);
            WarmVm {
                ready: pool.runtime.ready.contains(&k),
                key: k,
                node: v.node.clone(),
                vm_id: v.vm_id.clone(),
                name: v.name.clone(),
                since: v.created_at,
            }
        })
        .collect()
}

/// VMs claimed from the pool that are still running.
pub fn claimed_alive(pool: &str, vms: &[VmEntry]) -> u32 {
    vms.iter()
        .filter(|v| v.active() && v.claimed_at.is_some() && v.warm_pool.as_deref() == Some(pool))
        .count() as u32
}

/// Fields a launch may carry and still be answered from a pool: anything else (cloud-init, networks, disks, …) changes
/// what boots, and a warm VM has already booted.
const CLAIMABLE_KEYS: [&str; 13] = [
    "name",
    "backend",
    "image",
    "vcpus",
    "memory_mib",
    "apple",
    "disk_size_gib",
    "ttl_seconds",
    "tenant",
    "node",
    "nodeSelector",
    "security_profile",
    "pool",
];

/// Why this launch cannot take a warm VM, or `None` when it can.
pub fn unclaimable_reason(body: &Value) -> Option<String> {
    let obj = body.as_object()?;
    // Persistent volumes are attached when a VM is created; a warm VM was created without them.
    match obj.get("volumes") {
        None | Some(Value::Null) => {}
        Some(Value::Array(a)) if a.is_empty() => {}
        Some(_) => {
            return Some(
                "'volumes' are attached when a VM is created, and a warm VM already exists".into(),
            );
        }
    }
    if let Some(k) = obj
        .keys()
        .find(|k| k.as_str() != "volumes" && !CLAIMABLE_KEYS.contains(&k.as_str()))
    {
        return Some(format!(
            "'{k}' changes how the VM boots, and a warm VM has already booted"
        ));
    }
    // What the node would refuse at create is refused here too, so a bad launch never uses up a warm VM (the rename
    // would fail and the warm VM would be thrown away) nor skips its TTL.
    if let Some(n) = obj.get("name").filter(|n| !n.is_null())
        && !n.as_str().is_some_and(valid_vm_name)
    {
        return Some("'name' must be 1-63 of A-Z a-z 0-9 . _ -".into());
    }
    if obj
        .get("ttl_seconds")
        .is_some_and(|t| !t.is_null() && !t.is_u64())
    {
        return Some("'ttl_seconds' must be a whole number of seconds".into());
    }
    if body
        .get("backend")
        .is_some_and(|b| !b.is_null() && b.as_str() != Some("vz"))
    {
        return Some("warm pools hold vz VMs".into());
    }
    match body.get("apple") {
        None | Some(Value::Null) => {}
        Some(Value::Object(a)) => {
            if let Some(k) = a.keys().find(|k| k.as_str() != "guest_os") {
                return Some(format!(
                    "'apple.{k}' changes how the VM boots, and a warm VM has already booted"
                ));
            }
        }
        Some(_) => return Some("'apple' must be an object".into()),
    }
    if body
        .get("security_profile")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty() && p != "standard")
    {
        return Some("warm VMs run the standard security profile".into());
    }
    None
}

/// Whether a launch body asks for what this pool holds: its image (catalog path or name) and, where the body says, its
/// size, guest and disk.
pub fn matches(pool: &Pool, img: &CatalogImage, body: &Value) -> bool {
    let image_ok = match body.get("image").and_then(Value::as_str) {
        Some(i) => i == img.image || i == pool.image,
        None => false,
    };
    let num = |k: &str| body.get(k).filter(|v| !v.is_null());
    image_ok
        && num("vcpus").is_none_or(|v| v.as_u64() == Some(u64::from(pool.size.vcpus)))
        && num("memory_mib").is_none_or(|v| v.as_u64() == Some(pool.size.memory_mib))
        && num("disk_size_gib").is_none_or(|v| v.as_u64() == img.disk_size_gib)
        && body
            .get("apple")
            .and_then(|a| a.get("guest_os"))
            .and_then(Value::as_str)
            .is_none_or(|g| g == img.guest_os)
}

/// Claims in the last 24 hours, their average answer time (ms), and the newest claim.
pub fn claim_stats(pool: &Pool, now: DateTime<Utc>) -> (u32, Option<u64>, Option<DateTime<Utc>>) {
    let since = now - Duration::hours(24);
    let recent: Vec<&ClaimRecord> = pool
        .runtime
        .claims
        .iter()
        .filter(|c| c.at >= since)
        .collect();
    let avg = (!recent.is_empty())
        .then(|| recent.iter().map(|c| c.ms).sum::<u64>() / recent.len() as u64);
    (
        recent.len() as u32,
        avg,
        pool.runtime.claims.back().map(|c| c.at),
    )
}

/// The pool as the API shows it. `detail` adds where its warm VMs run (not for a tenant looking at a shared pool).
pub fn view(
    pool: &Pool,
    img: Option<&CatalogImage>,
    vms: &[VmEntry],
    now: DateTime<Utc>,
    detail: bool,
) -> Value {
    let warm = warm_vms(pool, vms);
    let ready = warm.iter().filter(|w| w.ready).count();
    let booting = warm.len() - ready;
    let (claimed_24h, avg_ms, last_claim) = claim_stats(pool, now);
    let state = if pool.backing_off(now) {
        "backing_off"
    } else if pool.min_ready == 0 {
        "paused"
    } else if ready as u32 >= pool.min_ready {
        "ready"
    } else {
        "filling"
    };
    let mut v = json!({
        "name": pool.name,
        "owner": pool.owner(),
        "tenant": pool.tenant,
        "image": pool.image,
        "image_display_name": img.and_then(|i| i.display_name.clone()),
        "guest_os": img.map(|i| i.guest_os.clone()),
        "size": pool.size,
        "min_ready": pool.min_ready,
        "max_total": pool.max_total,
        "created_at": pool.created_at,
        "status": {
            "state": state,
            "ready": ready,
            "booting": booting,
            "claimed_running": claimed_alive(&pool.name, vms),
            "claimed_24h": claimed_24h,
            "avg_claim_ms": avg_ms,
            "last_claim_at": last_claim,
            "last_fill_at": pool.runtime.last_fill_at,
            "last_error": pool.runtime.last_error,
            "backoff_until": pool.runtime.backoff_until.filter(|t| *t > now),
        },
    });
    if img.is_none() {
        v["status"]["last_error"] = json!(format!(
            "image '{}' is no longer in the catalog; nothing new is booted",
            pool.image
        ));
    }
    if detail {
        v["node_selector"] = json!(pool.node_selector);
        v["nodes"] = json!(pool.nodes);
        v["warm"] = json!(
            warm.iter()
                .map(|w| json!({
                    "node": w.node,
                    "id": w.vm_id,
                    "name": w.name,
                    "state": if w.ready { "ready" } else { "booting" },
                    "since": w.since,
                }))
                .collect::<Vec<_>>()
        );
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    }

    fn image() -> CatalogImage {
        serde_json::from_value(json!({
            "name": "macos-27-xcode",
            "guest_os": "macos",
            "image": "/vms/xcode/disk.raw",
            "vcpus": 6,
            "memory_mib": 16384,
        }))
        .unwrap()
    }

    fn spec(v: Value) -> PoolSpec {
        serde_json::from_value(v).unwrap()
    }

    fn pool(min_ready: u32, max_total: u32) -> Pool {
        let mut p = Pool::from_spec(
            spec(json!({"name": "ci", "image": "macos-27-xcode"})),
            None,
            Some(&image()),
            t0(),
        )
        .unwrap();
        p.min_ready = min_ready;
        p.max_total = max_total;
        p
    }

    fn warm(id: &str, ready: bool, age_mins: i64) -> WarmVm {
        WarmVm {
            key: key("mac", id),
            node: "mac".into(),
            vm_id: id.into(),
            name: format!("warm-ci-{id}"),
            ready,
            since: t0() - Duration::minutes(age_mins),
        }
    }

    #[test]
    fn specs_take_the_image_defaults_and_are_validated() {
        let p = pool(1, 5);
        assert_eq!(
            p.size,
            PoolSize {
                vcpus: 6,
                memory_mib: 16384
            }
        );
        assert_eq!(
            Pool::from_spec(
                spec(json!({"name": "ci", "image": "macos-27-xcode", "min_ready": 2})),
                None,
                Some(&image()),
                t0()
            )
            .unwrap()
            .max_total,
            8
        );
        for bad in [
            json!({"name": "CI", "image": "macos-27-xcode"}),
            json!({"name": "-ci", "image": "macos-27-xcode"}),
            json!({"name": "ci", "image": "macos-27-xcode", "vcpus": 0}),
            json!({"name": "ci", "image": "macos-27-xcode", "memory_mib": 100}),
            json!({"name": "ci", "image": "macos-27-xcode", "min_ready": 5, "max_total": 2}),
            json!({"name": "ci", "image": "macos-27-xcode", "min_ready": 500}),
            json!({"name": "ci", "image": "macos-27-xcode", "node_selector": {"zone": "a b"}}),
        ] {
            assert!(
                Pool::from_spec(spec(bad.clone()), None, Some(&image()), t0()).is_err(),
                "{bad}"
            );
        }
        assert!(
            Pool::from_spec(
                spec(json!({"name": "ci", "image": "nope"})),
                None,
                None,
                t0()
            )
            .unwrap_err()
            .contains("no image")
        );
        assert!(
            serde_json::from_value::<PoolSpec>(json!({"name": "a", "image": "b", "x": 1})).is_err()
        );
    }

    #[test]
    fn patches_change_counts_but_never_image_or_size() {
        let mut p = pool(1, 4);
        p.runtime.failures = 3;
        p.apply(PoolPatch {
            min_ready: Some(6),
            ..Default::default()
        })
        .unwrap();
        assert_eq!((p.min_ready, p.max_total, p.runtime.failures), (6, 6, 0));
        assert!(
            p.apply(PoolPatch {
                min_ready: Some(2),
                max_total: Some(1),
                ..Default::default()
            })
            .is_err()
        );
        assert_eq!(p.min_ready, 6, "a refused patch changes nothing");
        assert!(
            p.apply(PoolPatch {
                image: Some("other".into()),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            p.apply(PoolPatch {
                vcpus: Some(2),
                ..Default::default()
            })
            .is_err()
        );
        p.apply(PoolPatch {
            image: Some("macos-27-xcode".into()),
            size: Some(p.size),
            min_ready: Some(0),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(p.min_ready, 0);
    }

    #[test]
    fn plans_fill_to_min_ready_within_max_total_and_trim_booting_first() {
        let p = pool(3, 5);
        assert_eq!(plan(&p, &[], 0).create, 3);
        assert_eq!(plan(&p, &[warm("a", true, 5)], 0).create, 2);
        // Two claimed VMs still running: room for only one more of the two missing.
        assert_eq!(
            plan(&p, &[warm("a", true, 5), warm("b", false, 1)], 2).create,
            1
        );
        assert_eq!(plan(&p, &[warm("a", true, 5)], 4).create, 0);
        // A big pool fills in steps.
        assert_eq!(plan(&pool(20, 40), &[], 0).create, MAX_CREATES_PER_PASS);
        // min_ready lowered: the booting VM goes first, then the newest ready one.
        let mut p1 = pool(1, 5);
        let ws = [
            warm("old", true, 30),
            warm("new", true, 2),
            warm("boot", false, 1),
        ];
        let pl = plan(&p1, &ws, 0);
        assert_eq!(pl.create, 0);
        assert_eq!(pl.trim, vec![key("mac", "boot"), key("mac", "new")]);
        p1.min_ready = 3;
        assert_eq!(plan(&p1, &ws, 0), Plan::default());
    }

    #[test]
    fn backoff_doubles_up_to_a_cap() {
        assert_eq!(backoff(1).num_seconds(), 5);
        assert_eq!(backoff(2).num_seconds(), 10);
        assert_eq!(backoff(4).num_seconds(), 40);
        assert_eq!(backoff(30).num_seconds(), MAX_BACKOFF_SECS);
        let mut p = pool(1, 2);
        p.record_failure("no Mac".into(), t0());
        assert!(p.backing_off(t0() + Duration::seconds(4)));
        assert!(!p.backing_off(t0() + Duration::seconds(6)));
        p.record_success(t0());
        assert_eq!(p.runtime.failures, 0);
        assert!(p.runtime.last_error.is_none());
    }

    #[test]
    fn node_states_are_read_from_either_shape() {
        assert_eq!(classify(None), WarmState::Missing);
        assert_eq!(
            classify(Some(&json!({"status": "running"}))),
            WarmState::Ready
        );
        assert_eq!(
            classify(Some(&json!({"status": "Running"}))),
            WarmState::Ready
        );
        assert_eq!(
            classify(Some(&json!({"state": "creating"}))),
            WarmState::Booting
        );
        assert_eq!(
            classify(Some(&json!({"status": {"failed": "boot loader"}}))),
            WarmState::Failed
        );
        assert_eq!(
            classify(Some(&json!({"status": "stopped"}))),
            WarmState::Failed
        );
    }

    #[test]
    fn only_plain_launches_of_the_same_image_and_size_match() {
        let p = pool(1, 4);
        let img = image();
        let plain = json!({"name": "job-1", "backend": "vz", "image": "/vms/xcode/disk.raw",
                           "vcpus": 6, "memory_mib": 16384, "apple": {"guest_os": "macos"}, "ttl_seconds": 3600});
        assert_eq!(unclaimable_reason(&plain), None);
        let mut no_volumes = plain.clone();
        no_volumes["volumes"] = json!([]);
        assert_eq!(unclaimable_reason(&no_volumes), None);
        assert!(matches(&p, &img, &plain));
        assert!(matches(
            &p,
            &img,
            &json!({"name": "x", "image": "macos-27-xcode"})
        ));
        assert!(!matches(
            &p,
            &img,
            &json!({"name": "x", "image": "/vms/other.raw"})
        ));
        assert!(!matches(
            &p,
            &img,
            &json!({"image": "/vms/xcode/disk.raw", "vcpus": 4})
        ));
        assert!(!matches(
            &p,
            &img,
            &json!({"image": "/vms/xcode/disk.raw", "apple": {"guest_os": "linux"}})
        ));
        assert!(!matches(&p, &img, &json!({"vcpus": 6})));
        for (body, what) in [
            (
                json!({"image": "/x", "cloud_init": {"runcmd": []}}),
                "cloud_init",
            ),
            (
                json!({"image": "/x", "apple": {"guest_os": "macos", "networks": [{"name": "n"}]}}),
                "apple.networks",
            ),
            (json!({"image": "/x", "backend": "qemu"}), "vz"),
            (
                json!({"image": "/x", "security_profile": "confidential-snp"}),
                "standard",
            ),
            (
                json!({"image": "/x", "reschedulable": true}),
                "reschedulable",
            ),
            (json!({"image": "/x", "volumes": ["pg-data"]}), "volumes"),
            (json!({"image": "/x", "name": "no spaces"}), "name"),
            (json!({"image": "/x", "ttl_seconds": -5}), "ttl_seconds"),
        ] {
            assert!(
                unclaimable_reason(&body).is_some_and(|r| r.contains(what)),
                "{body}"
            );
        }
    }

    #[test]
    fn claim_statistics_cover_the_last_day_and_history_is_bounded() {
        let mut p = pool(1, 4);
        let at = |h: i64, ms: u64| ClaimRecord {
            at: t0() - Duration::hours(h),
            ms,
            warm_secs: 60,
            tenant: None,
        };
        p.record_claim(at(30, 9000));
        p.record_claim(at(2, 100));
        p.record_claim(at(1, 300));
        assert_eq!(
            claim_stats(&p, t0()),
            (2, Some(200), Some(t0() - Duration::hours(1)))
        );
        for _ in 0..(MAX_CLAIM_HISTORY + 10) {
            p.record_claim(at(0, 1));
        }
        assert_eq!(p.runtime.claims.len(), MAX_CLAIM_HISTORY);
        // Older than the history window: dropped.
        let mut q = pool(1, 4);
        q.record_claim(at(24 * 10, 5));
        q.record_claim(at(0, 5));
        assert_eq!(q.runtime.claims.len(), 1);
    }

    #[test]
    fn fill_bodies_carry_the_marker_owner_and_size() {
        let mut p = pool(1, 4);
        p.tenant = Some("acme".into());
        let b = p.fill_body(&image(), "warm-ci-1234abcd");
        assert_eq!(b[FILL_MARKER], "ci");
        assert_eq!(b["tenant"], "acme");
        assert_eq!(b["vcpus"], 6);
        assert_eq!(b["apple"]["guest_os"], "macos");
        assert!(valid_pool_name(&"a".repeat(40)));
        assert!(!valid_pool_name(&"a".repeat(41)));
        let mut labels = std::collections::HashMap::new();
        labels.insert("zone".to_string(), "lab".to_string());
        p.node_selector.insert("zone".into(), "lab".into());
        assert!(p.allows_node("mac-1", &labels));
        p.nodes = vec!["mac-2".into()];
        assert!(!p.allows_node("mac-1", &labels));
    }
}
