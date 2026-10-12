// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Tenants, host leases, quotas and the VM ledger behind `fluxvm-agent central`.
//!
//! Pure state and rules, no HTTP: `central` owns one [`TenancyState`] behind its mutex, calls the methods here, and
//! persists the whole thing to `fleet-tenancy.json` after every change.
//!
//! # Why host leases
//!
//! Apple's macOS licence lets a Mac run at most two extra macOS copies in VMs, and lets a Mac be leased only to one
//! lessee with sole use for at least 24 consecutive hours. A cloud that hands out macOS VMs per minute on a shared Mac
//! does not fit that, so this module models the unit a tenant rents as a whole Mac (a [`Lease`], 24 h minimum), and
//! [`TenancyState::admit`] keeps macOS guests on the tenant's own leased Macs. Linux VMs carry no such restriction and
//! may still land on shared (unleased) nodes. This is an engineering control, not legal advice: the deploying company
//! decides its licence posture, and [`Policy`] lets it turn the macOS rule off.
//!
//! # Ledger
//!
//! Every VM created through central is recorded as a [`VmEntry`] (tenant, node, size, start, end). The ledger answers
//! quota checks without asking any node, and is the source for metering ([`TenancyState::usage`]). VMs a node deletes on
//! its own (TTL, `fluxctl delete` on the node) are closed by [`TenancyState::reconcile_node`], which central runs
//! periodically against each node's `GET /v1/vms`.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Apple's macOS licence: a leased Mac must be leased for at least 24 consecutive hours.
pub const MIN_LEASE_HOURS: i64 = 24;
/// A ledger entry younger than this is never closed by reconciliation: the node may not list a VM it is still
/// creating.
pub const RECONCILE_GRACE_SECS: i64 = 120;
/// Upper bound on a tenant id, so ids stay usable as labels and file names.
const MAX_TENANT_ID_LEN: usize = 63;

/// Who is calling central, resolved from the bearer token by `central`'s auth middleware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The fleet token (operators and node heartbeats), or any caller when central runs without a token (lab mode).
    Admin,
    /// A tenant token.
    Tenant(String),
}

impl Caller {
    pub fn tenant(&self) -> Option<&str> {
        match self {
            Caller::Admin => None,
            Caller::Tenant(t) => Some(t),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_leased_hosts: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tenant {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// SHA-256 (hex) of the tenant's bearer token. The token itself is returned once, at create or rotate time.
    pub token_sha256: String,
    #[serde(default)]
    pub quota: Quota,
    #[serde(default)]
    pub disabled: bool,
    pub created_at: DateTime<Utc>,
    /// The tenant's /24 under `Policy::tenant_subnet_prefix` when network isolation is on (assigned on first use).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_index: Option<u8>,
    /// Named API keys besides the primary token: one per person, CI system or tool, each revocable on its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<ApiKey>,
}

/// A named API key of a tenant. Like the primary token, only its hash is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiKey {
    /// `key_` and 12 hex characters; what revoking names.
    pub id: String,
    pub name: String,
    pub token_sha256: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Only reads (`GET`): for dashboards, monitoring and support.
    #[serde(default)]
    pub read_only: bool,
    /// Updated at most once a minute, and saved with the next change to the tenancy state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
}

impl ApiKey {
    /// The key as the API shows it: never the hash.
    pub fn public_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "read_only": self.read_only,
            "last_used_at": self.last_used_at,
        })
    }
}

/// Whether `s` has the shape of an [`ApiKey::id`].
fn looks_like_key_id(s: &str) -> bool {
    s.strip_prefix("key_")
        .is_some_and(|h| h.len() == 12 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Most named keys one tenant may hold.
pub const MAX_KEYS: usize = 50;

/// Who a presented bearer token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyMatch {
    pub tenant: String,
    pub disabled: bool,
    /// The named key used, or `None` for the tenant's primary token.
    pub key: Option<String>,
    pub read_only: bool,
}

impl Tenant {
    /// The record as the API shows it: never the token hash.
    pub fn public_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "display_name": self.display_name,
            "quota": self.quota,
            "disabled": self.disabled,
            "created_at": self.created_at,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Active,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub tenant: String,
    pub node: String,
    pub started_at: DateTime<Utc>,
    /// `started_at + MIN_LEASE_HOURS`: the lease cannot be released earlier (see [`TenancyState::release_lease`]),
    /// and is billed to at least this point even when an operator force-releases it.
    pub min_end: DateTime<Utc>,
    /// Requested end; `None` runs until released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<DateTime<Utc>>,
    pub state: LeaseState,
}

impl Lease {
    /// The end used for billing: never before `min_end`.
    pub fn billable_end(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let end = self.released_at.unwrap_or(now);
        if self.state == LeaseState::Released {
            end.max(self.min_end)
        } else {
            end
        }
    }
}

/// One VM created through central.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmEntry {
    pub node: String,
    pub vm_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    pub vcpus: u32,
    pub memory_mib: u64,
    #[serde(default)]
    pub macos_guest: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// The create body, kept only when the caller asked for `"reschedulable": true`, so a drain can recreate the VM
    /// elsewhere. Never kept otherwise: create bodies can carry secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
}

impl VmEntry {
    pub fn active(&self) -> bool {
        self.ended_at.is_none()
    }
}

/// A private network that spans Macs (`fluxvm_vz_switch::overlay`): one subnet, one UDP port and one key fleet-wide,
/// and a disjoint slice of host numbers per member Mac.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayNet {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// `N` of `10.89.N.0/24`.
    pub subnet: u8,
    pub port: u16,
    /// The network key, 64 hex characters. Secret: only member nodes are ever given it.
    pub key_hex: String,
    /// Epoch of `key_hex`; bumped by every rotation.
    #[serde(default)]
    pub epoch: u32,
    /// The key before the last rotation, kept so Macs that have not picked up the new key yet still talk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<(u32, String)>,
    /// When the key was last set (creation or rotation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotated_at: Option<DateTime<Utc>>,
    pub mtu: u32,
    pub hosts_per_node: u8,
    /// Member node -> its host slice (inclusive).
    pub members: BTreeMap<String, [u8; 2]>,
    pub created_at: DateTime<Utc>,
}

impl OverlayNet {
    /// The record as the API shows it: never the key.
    pub fn public_json(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "tenant": self.tenant,
            "subnet": format!("10.89.{}.0/24", self.subnet),
            "port": self.port,
            "mtu": self.mtu,
            "members": self.members,
            "key_epoch": self.epoch,
            "key_rotated_at": self.rotated_at.unwrap_or(self.created_at),
            "created_at": self.created_at,
        })
    }

    /// The next free slice of `hosts_per_node` addresses within 2-254.
    fn free_slice(&self) -> Option<[u8; 2]> {
        let size = u16::from(self.hosts_per_node.max(1));
        let mut lo = 2u16;
        while lo + size - 1 <= 254 {
            let hi = lo + size - 1;
            let overlaps = self
                .members
                .values()
                .any(|[a, b]| u16::from(*a) <= hi && lo <= u16::from(*b));
            if !overlaps {
                return Some([lo as u8, hi as u8]);
            }
            lo += size;
        }
        None
    }
}

/// Shortest time between two key rotations of one network.
pub const MIN_ROTATION_MINUTES: i64 = 5;

/// First UDP port handed out to overlays.
pub const OVERLAY_PORT_BASE: u16 = 47100;
/// Encapsulation adds 60 bytes over IPv4; 1400 also leaves room for IPv6 and tunnels in the underlay.
pub const OVERLAY_DEFAULT_MTU: u32 = 1400;

fn random_key_hex() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut b)
        .ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

fn valid_network_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Fleet-wide rules. Defaults follow the macOS licence; each can be turned off in `fleet-tenancy.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    /// A tenant's macOS guest may only run on a Mac that tenant leases.
    #[serde(default = "yes")]
    pub require_lease_for_macos: bool,
    /// A leased Mac only runs its lessee's VMs (sole use).
    #[serde(default = "yes")]
    pub exclusive_leased_hosts: bool,
    /// Minimum lease length in hours; values below [`MIN_LEASE_HOURS`] are raised to it.
    #[serde(default = "min_lease_hours")]
    pub min_lease_hours: i64,
    /// Ended VMs and leases older than this are dropped by the reconciler (0 keeps everything). Export usage first.
    #[serde(default = "ledger_retention_days")]
    pub ledger_retention_days: i64,
    /// Give every tenant's `vz` VMs their own named vmnet network (one /24 per tenant), so two tenants' VMs on one
    /// Mac never share a network. Needs macOS 26+ and the `fluxvm-vmnetd` broker on every Mac. Off by default.
    #[serde(default)]
    pub isolate_tenant_networks: bool,
    /// First two octets of the tenant /24s, e.g. `10.230` gives tenant n `10.230.n.0/24`.
    #[serde(default = "tenant_subnet_prefix")]
    pub tenant_subnet_prefix: String,
    /// Overlay network keys older than this are rotated by the reconciler (0 never rotates automatically).
    #[serde(default = "overlay_key_rotation_days")]
    pub overlay_key_rotation_days: i64,
}

fn overlay_key_rotation_days() -> i64 {
    30
}

fn tenant_subnet_prefix() -> String {
    "10.230".into()
}

fn ledger_retention_days() -> i64 {
    400
}

fn yes() -> bool {
    true
}

fn min_lease_hours() -> i64 {
    MIN_LEASE_HOURS
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            require_lease_for_macos: true,
            exclusive_leased_hosts: true,
            min_lease_hours: MIN_LEASE_HOURS,
            ledger_retention_days: ledger_retention_days(),
            isolate_tenant_networks: false,
            tenant_subnet_prefix: tenant_subnet_prefix(),
            overlay_key_rotation_days: overlay_key_rotation_days(),
        }
    }
}

impl Policy {
    fn lease_hours(&self) -> i64 {
        self.min_lease_hours.max(MIN_LEASE_HOURS)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenancyError {
    UnknownTenant(String),
    TenantDisabled(String),
    TenantExists(String),
    InvalidTenantId(String),
    TenantBusy(String),
    UnknownLease(String),
    UnknownKey(String),
    InvalidKey(String),
    NodeLeased { node: String, tenant: String },
    NodeNeedsScrub(String),
    LeaseTooShort { hours: i64, min: i64 },
    EarlyRelease { earliest: DateTime<Utc> },
    LeaseNotActive(String),
    Quota(String),
    NotYours(String),
    Overlay(String),
}

impl TenancyError {
    /// The HTTP status central answers with.
    pub fn status(&self) -> u16 {
        match self {
            TenancyError::UnknownTenant(_)
            | TenancyError::UnknownLease(_)
            | TenancyError::UnknownKey(_) => 404,
            TenancyError::InvalidKey(_) => 400,
            TenancyError::InvalidTenantId(_) | TenancyError::LeaseTooShort { .. } => 400,
            TenancyError::TenantDisabled(_) | TenancyError::NotYours(_) => 403,
            TenancyError::Quota(_) => 429,
            TenancyError::Overlay(_) => 409,
            _ => 409,
        }
    }
}

impl std::fmt::Display for TenancyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TenancyError::UnknownTenant(t) => write!(f, "no tenant '{t}'"),
            TenancyError::TenantDisabled(t) => write!(f, "tenant '{t}' is disabled"),
            TenancyError::TenantExists(t) => write!(f, "tenant '{t}' already exists"),
            TenancyError::InvalidTenantId(t) => write!(
                f,
                "invalid tenant id '{t}': use 1-{MAX_TENANT_ID_LEN} lowercase letters, digits and '-', starting with a letter or digit"
            ),
            TenancyError::TenantBusy(t) => {
                write!(f, "tenant '{t}' still has active leases or VMs")
            }
            TenancyError::UnknownLease(l) => write!(f, "no lease '{l}'"),
            TenancyError::UnknownKey(k) => write!(f, "no API key '{k}'"),
            TenancyError::InvalidKey(m) => write!(f, "{m}"),
            TenancyError::NodeLeased { node, tenant } => {
                write!(f, "node '{node}' is leased to tenant '{tenant}'")
            }
            TenancyError::NodeNeedsScrub(n) => write!(
                f,
                "node '{n}' was released from a lease and must be scrubbed (POST /fleet/nodes/{n}/scrub) before reuse"
            ),
            TenancyError::LeaseTooShort { hours, min } => write!(
                f,
                "a lease must be at least {min} hours (asked for {hours}); the macOS licence requires a minimum 24 consecutive hours of sole use"
            ),
            TenancyError::EarlyRelease { earliest } => write!(
                f,
                "lease cannot be released before {earliest} (24-hour minimum); an operator can force it, and it is still billed to that time"
            ),
            TenancyError::LeaseNotActive(l) => write!(f, "lease '{l}' is not active"),
            TenancyError::Quota(m) => write!(f, "quota exceeded: {m}"),
            TenancyError::NotYours(m) => write!(f, "{m}"),
            TenancyError::Overlay(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for TenancyError {}

pub fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// A new random bearer token: 64 hex characters (two v4 UUIDs, 244 random bits) with a recognisable prefix.
pub fn new_token() -> String {
    format!(
        "fvt_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn valid_tenant_id(id: &str) -> bool {
    // `me` is the route `GET /fleet/tenants/me`.
    id != "me"
        && !id.is_empty()
        && id.len() <= MAX_TENANT_ID_LEN
        && id
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Overlap of `[start, end)` with `[from, to)`, in seconds (0 when they do not overlap).
fn overlap_secs(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> i64 {
    let s = start.max(from);
    let e = end.min(to);
    (e - s).num_seconds().max(0)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TenantUsage {
    pub tenant: String,
    pub vm_seconds: i64,
    pub vcpu_seconds: i64,
    pub memory_mib_seconds: i64,
    pub macos_vm_seconds: i64,
    pub lease_seconds: i64,
    /// Lease time billed in the window, counting a released lease to at least its 24-hour `min_end`.
    pub billable_lease_seconds: i64,
    pub vms_started: u32,
    pub leases_started: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageReport {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub tenants: Vec<TenantUsage>,
}

impl UsageReport {
    /// One CSV row per tenant, with a header.
    pub fn to_csv(&self) -> String {
        let mut out = String::from(
            "tenant,from,to,vm_hours,vcpu_hours,memory_gib_hours,macos_vm_hours,lease_hours,billable_lease_hours,vms_started,leases_started\n",
        );
        let h = |s: i64| format!("{:.4}", s as f64 / 3600.0);
        for t in &self.tenants {
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{}\n",
                t.tenant,
                self.from.to_rfc3339(),
                self.to.to_rfc3339(),
                h(t.vm_seconds),
                h(t.vcpu_seconds),
                format!("{:.4}", t.memory_mib_seconds as f64 / 1024.0 / 3600.0),
                h(t.macos_vm_seconds),
                h(t.lease_seconds),
                h(t.billable_lease_seconds),
                t.vms_started,
                t.leases_started,
            ));
        }
        out
    }
}

/// One entry of the image catalog (`GET /fleet/images`). Central does not move images: the operator puts the disk at
/// `image` on every Mac that should run it (`fluxctl vm-image pull`), and the console fills a create request from the
/// entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogImage {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `macos` or `linux`.
    pub guest_os: String,
    /// The disk's path on each Mac (the request's `image`).
    pub image: String,
    #[serde(default = "default_image_vcpus")]
    pub vcpus: u8,
    #[serde(default = "default_image_memory")]
    pub memory_mib: u64,
    #[serde(default)]
    pub disk_size_gib: Option<u64>,
    /// Where it came from (an OCI reference), for display.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

fn default_image_vcpus() -> u8 {
    4
}

fn default_image_memory() -> u64 {
    8192
}

impl CatalogImage {
    /// Checks an entry before it is stored.
    pub fn validate(&self) -> Result<(), String> {
        if !valid_image_name(&self.name) {
            return Err("image names are 1-63 characters of a-z, 0-9, '-', '.' and '_'".into());
        }
        if !matches!(self.guest_os.as_str(), "macos" | "linux") {
            return Err("guest_os must be 'macos' or 'linux'".into());
        }
        if self.image.trim().is_empty() {
            return Err("image (the disk's path on the Macs) is required".into());
        }
        if self.vcpus == 0 || self.memory_mib < 512 {
            return Err("an image needs at least 1 vCPU and 512 MiB".into());
        }
        Ok(())
    }
}

fn valid_image_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 63
        && n.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b'_')
        })
}

/// Everything persisted in `fleet-tenancy.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TenancyState {
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub tenants: BTreeMap<String, Tenant>,
    #[serde(default)]
    pub leases: Vec<Lease>,
    #[serde(default)]
    pub vms: Vec<VmEntry>,
    /// Nodes released from a lease that must be scrubbed before anyone else uses them.
    #[serde(default)]
    pub scrub_pending: BTreeSet<String>,
    /// Nodes being drained (`POST /fleet/nodes/{name}/drain`); cleared by uncordon.
    #[serde(default)]
    pub draining: BTreeSet<String>,
    /// Private networks spanning Macs, by name.
    #[serde(default)]
    pub overlays: BTreeMap<String, OverlayNet>,
    /// The image catalog: VM images the operator has put on the Macs, which tenants pick from in the console.
    #[serde(default)]
    pub images: BTreeMap<String, CatalogImage>,
    /// Quota held by creates that are in flight (between the quota check and the node's answer). In memory only: a
    /// restart drops them, and the requests they belonged to are gone with it.
    #[serde(skip)]
    reservations: Vec<Reservation>,
    #[serde(skip)]
    next_reservation: u64,
}

/// Quota held by one in-flight create.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Reservation {
    id: u64,
    tenant: String,
    vcpus: u32,
    memory_mib: u64,
}

impl TenancyState {
    /// True once any tenant exists; until then central behaves exactly as before tenancy existed.
    pub fn enabled(&self) -> bool {
        !self.tenants.is_empty()
    }

    // ---- tenants ----------------------------------------------------------------------------------------------

    /// Creates a tenant and returns its one-time bearer token.
    pub fn create_tenant(
        &mut self,
        id: &str,
        display_name: Option<String>,
        quota: Quota,
        now: DateTime<Utc>,
    ) -> Result<(Tenant, String), TenancyError> {
        if !valid_tenant_id(id) {
            return Err(TenancyError::InvalidTenantId(id.into()));
        }
        if self.tenants.contains_key(id) {
            return Err(TenancyError::TenantExists(id.into()));
        }
        let token = new_token();
        let t = Tenant {
            id: id.into(),
            display_name,
            token_sha256: hash_token(&token),
            quota,
            disabled: false,
            created_at: now,
            network_index: None,
            keys: Vec::new(),
        };
        self.tenants.insert(id.into(), t.clone());
        Ok((t, token))
    }

    pub fn rotate_token(&mut self, id: &str) -> Result<String, TenancyError> {
        let t = self
            .tenants
            .get_mut(id)
            .ok_or_else(|| TenancyError::UnknownTenant(id.into()))?;
        let token = new_token();
        t.token_sha256 = hash_token(&token);
        Ok(token)
    }

    pub fn update_tenant(
        &mut self,
        id: &str,
        quota: Option<Quota>,
        disabled: Option<bool>,
        display_name: Option<String>,
    ) -> Result<Tenant, TenancyError> {
        let t = self
            .tenants
            .get_mut(id)
            .ok_or_else(|| TenancyError::UnknownTenant(id.into()))?;
        if let Some(q) = quota {
            t.quota = q;
        }
        if let Some(d) = disabled {
            t.disabled = d;
        }
        if display_name.is_some() {
            t.display_name = display_name;
        }
        Ok(t.clone())
    }

    /// Removes a tenant with no active lease and no active VM.
    pub fn delete_tenant(&mut self, id: &str) -> Result<(), TenancyError> {
        if !self.tenants.contains_key(id) {
            return Err(TenancyError::UnknownTenant(id.into()));
        }
        let busy = self
            .leases
            .iter()
            .any(|l| l.tenant == id && l.state == LeaseState::Active)
            || self
                .vms
                .iter()
                .any(|v| v.active() && v.tenant.as_deref() == Some(id));
        if busy {
            return Err(TenancyError::TenantBusy(id.into()));
        }
        self.overlays.retain(|_, o| o.tenant.as_deref() != Some(id));
        self.tenants.remove(id);
        Ok(())
    }

    /// Resolves a bearer token to its tenant and key: the primary token, or a named key that has not expired. A key's
    /// `last_used_at` is refreshed at most once a minute.
    pub fn authenticate_key(&mut self, presented: &str, now: DateTime<Utc>) -> Option<KeyMatch> {
        let h = hash_token(presented);
        for t in self.tenants.values_mut() {
            if constant_time_eq(t.token_sha256.as_bytes(), h.as_bytes()) {
                return Some(KeyMatch {
                    tenant: t.id.clone(),
                    disabled: t.disabled,
                    key: None,
                    read_only: false,
                });
            }
            if let Some(k) = t
                .keys
                .iter_mut()
                .find(|k| constant_time_eq(k.token_sha256.as_bytes(), h.as_bytes()))
            {
                if k.expires_at.is_some_and(|e| e <= now) {
                    return None;
                }
                if k.last_used_at
                    .is_none_or(|u| now - u >= chrono::Duration::seconds(60))
                {
                    k.last_used_at = Some(now);
                }
                return Some(KeyMatch {
                    tenant: t.id.clone(),
                    disabled: t.disabled,
                    key: Some(k.name.clone()),
                    read_only: k.read_only,
                });
            }
        }
        None
    }

    /// Adds a named key and returns it with its one-time token.
    pub fn create_key(
        &mut self,
        tenant: &str,
        name: &str,
        expires_at: Option<DateTime<Utc>>,
        read_only: bool,
        now: DateTime<Utc>,
    ) -> Result<(ApiKey, String), TenancyError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(|c| c.is_control()) {
            return Err(TenancyError::InvalidKey(
                "a key needs a name of 1 to 64 characters".into(),
            ));
        }
        // Revoking takes an id or a name, so a name must never be mistaken for another key's id.
        if looks_like_key_id(name) {
            return Err(TenancyError::InvalidKey(
                "a key's name may not look like a key id (key_ and 12 hex characters)".into(),
            ));
        }
        if expires_at.is_some_and(|e| e <= now) {
            return Err(TenancyError::InvalidKey(
                "a key's expiry must be in the future".into(),
            ));
        }
        let t = self
            .tenants
            .get_mut(tenant)
            .ok_or_else(|| TenancyError::UnknownTenant(tenant.into()))?;
        if t.keys.iter().any(|k| k.name == name) {
            return Err(TenancyError::InvalidKey(format!(
                "a key named '{name}' already exists; revoke it first or pick another name"
            )));
        }
        if t.keys.len() >= MAX_KEYS {
            return Err(TenancyError::InvalidKey(format!(
                "a tenant can hold at most {MAX_KEYS} keys; revoke one first"
            )));
        }
        let token = new_token();
        let key = ApiKey {
            id: format!("key_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
            name: name.to_owned(),
            token_sha256: hash_token(&token),
            created_at: now,
            expires_at,
            read_only,
            last_used_at: None,
        };
        t.keys.push(key.clone());
        Ok((key, token))
    }

    /// Revokes a named key by id or name; its token stops working at once.
    pub fn revoke_key(&mut self, tenant: &str, key: &str) -> Result<ApiKey, TenancyError> {
        let t = self
            .tenants
            .get_mut(tenant)
            .ok_or_else(|| TenancyError::UnknownTenant(tenant.into()))?;
        // An id first: it is unique and never changes.
        let i = t
            .keys
            .iter()
            .position(|k| k.id == key)
            .or_else(|| t.keys.iter().position(|k| k.name == key))
            .ok_or_else(|| TenancyError::UnknownKey(key.into()))?;
        Ok(t.keys.remove(i))
    }

    /// The tenant whose primary token this is (named keys aside; see [`TenancyState::authenticate_key`]).
    pub fn authenticate(&self, presented: &str) -> Option<&Tenant> {
        let h = hash_token(presented);
        self.tenants
            .values()
            .find(|t| constant_time_eq(t.token_sha256.as_bytes(), h.as_bytes()))
    }

    fn require_tenant(&self, id: &str) -> Result<&Tenant, TenancyError> {
        let t = self
            .tenants
            .get(id)
            .ok_or_else(|| TenancyError::UnknownTenant(id.into()))?;
        if t.disabled {
            return Err(TenancyError::TenantDisabled(id.into()));
        }
        Ok(t)
    }

    // ---- leases -----------------------------------------------------------------------------------------------

    pub fn active_lease_for_node(&self, node: &str) -> Option<&Lease> {
        self.leases
            .iter()
            .find(|l| l.node == node && l.state == LeaseState::Active)
    }

    pub fn lease(&self, id: &str) -> Option<&Lease> {
        self.leases.iter().find(|l| l.id == id)
    }

    /// Whether `node` can take a new lease right now (tenancy rules only; health and capacity are central's job).
    pub fn leasable(&self, node: &str) -> Result<(), TenancyError> {
        if let Some(l) = self.active_lease_for_node(node) {
            return Err(TenancyError::NodeLeased {
                node: node.into(),
                tenant: l.tenant.clone(),
            });
        }
        if self.scrub_pending.contains(node) {
            return Err(TenancyError::NodeNeedsScrub(node.into()));
        }
        Ok(())
    }

    /// Leases `node` to `tenant` from `now` for `hours` (default: until released).
    pub fn create_lease(
        &mut self,
        tenant: &str,
        node: &str,
        hours: Option<i64>,
        now: DateTime<Utc>,
    ) -> Result<Lease, TenancyError> {
        let t = self.require_tenant(tenant)?;
        let min = self.policy.lease_hours();
        if let Some(h) = hours
            && h < min
        {
            return Err(TenancyError::LeaseTooShort { hours: h, min });
        }
        self.leasable(node)?;
        if let Some(max) = t.quota.max_leased_hosts {
            let held = self
                .leases
                .iter()
                .filter(|l| l.tenant == tenant && l.state == LeaseState::Active)
                .count() as u32;
            if held >= max {
                return Err(TenancyError::Quota(format!(
                    "tenant '{tenant}' already holds {held} of {max} leased hosts"
                )));
            }
        }
        let lease = Lease {
            id: format!("lease-{}", uuid::Uuid::new_v4().simple()),
            tenant: tenant.into(),
            node: node.into(),
            started_at: now,
            min_end: now + Duration::hours(min),
            ends_at: hours.map(|h| now + Duration::hours(h)),
            released_at: None,
            state: LeaseState::Active,
        };
        self.leases.push(lease.clone());
        Ok(lease)
    }

    /// Releases a lease. A tenant (`scope = Some(tenant)`) can release only its own lease and never before
    /// `min_end`; an operator (`scope = None`) can `force` an early release, which is still billed to `min_end`.
    /// The node then needs a scrub before it can be leased or used again.
    pub fn release_lease(
        &mut self,
        id: &str,
        scope: Option<&str>,
        force: bool,
        now: DateTime<Utc>,
    ) -> Result<Lease, TenancyError> {
        let lease = self
            .leases
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or_else(|| TenancyError::UnknownLease(id.into()))?;
        if let Some(t) = scope
            && lease.tenant != t
        {
            // Same answer as a missing lease: a tenant learns nothing about other tenants' leases.
            return Err(TenancyError::UnknownLease(id.into()));
        }
        if lease.state != LeaseState::Active {
            return Err(TenancyError::LeaseNotActive(id.into()));
        }
        let can_force = force && scope.is_none();
        if now < lease.min_end && !can_force {
            return Err(TenancyError::EarlyRelease {
                earliest: lease.min_end,
            });
        }
        lease.state = LeaseState::Released;
        lease.released_at = Some(now);
        let out = lease.clone();
        self.scrub_pending.insert(out.node.clone());
        self.drop_tenant_overlay_member(&out.tenant, &out.node);
        Ok(out)
    }

    /// Ends every active lease whose requested `ends_at` has passed. Returns the leases it ended.
    pub fn expire_leases(&mut self, now: DateTime<Utc>) -> Vec<Lease> {
        let mut ended = Vec::new();
        for l in self.leases.iter_mut() {
            if l.state == LeaseState::Active && l.ends_at.is_some_and(|e| e <= now) {
                l.state = LeaseState::Released;
                l.released_at = l.ends_at;
                ended.push(l.clone());
            }
        }
        for l in &ended {
            self.scrub_pending.insert(l.node.clone());
            self.drop_tenant_overlay_member(&l.tenant, &l.node);
        }
        ended
    }

    pub fn mark_scrubbed(&mut self, node: &str) -> bool {
        self.scrub_pending.remove(node)
    }

    // ---- admission --------------------------------------------------------------------------------------------

    /// Whether `caller` may place a VM on `node`. `explicit` is true when the caller named the node: an operator
    /// naming a node bypasses tenancy (like it bypasses cordon), a tenant never does.
    pub fn admit(
        &self,
        node: &str,
        caller: &Caller,
        macos_guest: bool,
        explicit: bool,
    ) -> Result<(), TenancyError> {
        if !self.enabled() {
            return Ok(());
        }
        if explicit && *caller == Caller::Admin {
            return Ok(());
        }
        if self.scrub_pending.contains(node) {
            return Err(TenancyError::NodeNeedsScrub(node.into()));
        }
        let lease = self.active_lease_for_node(node);
        match (caller, lease) {
            (Caller::Tenant(t), Some(l)) if &l.tenant == t => Ok(()),
            (_, Some(l)) if self.policy.exclusive_leased_hosts => Err(TenancyError::NodeLeased {
                node: node.into(),
                tenant: l.tenant.clone(),
            }),
            (Caller::Tenant(t), _) if macos_guest && self.policy.require_lease_for_macos => {
                Err(TenancyError::NotYours(format!(
                    "macOS guests for tenant '{t}' run only on Macs the tenant leases (POST /fleet/leases); node '{node}' is not one"
                )))
            }
            _ => Ok(()),
        }
    }

    /// Checks a new VM of this size against the tenant's quota.
    pub fn check_quota(
        &self,
        tenant: &str,
        vcpus: u32,
        memory_mib: u64,
    ) -> Result<(), TenancyError> {
        let t = self.require_tenant(tenant)?;
        let q = &t.quota;
        let mine = self
            .vms
            .iter()
            .filter(|v| v.active() && v.tenant.as_deref() == Some(tenant));
        let (mut n, mut cpu, mut mem) = (0u32, 0u64, 0u64);
        for v in mine {
            n += 1;
            cpu += u64::from(v.vcpus);
            mem += v.memory_mib;
        }
        for r in self.reservations.iter().filter(|r| r.tenant == tenant) {
            n += 1;
            cpu += u64::from(r.vcpus);
            mem += r.memory_mib;
        }
        if let Some(max) = q.max_vms
            && n + 1 > max
        {
            return Err(TenancyError::Quota(format!(
                "tenant '{tenant}' runs {n} of {max} VMs"
            )));
        }
        if let Some(max) = q.max_vcpus
            && cpu + u64::from(vcpus) > max
        {
            return Err(TenancyError::Quota(format!(
                "tenant '{tenant}' uses {cpu} of {max} vCPUs; this VM asks for {vcpus}"
            )));
        }
        if let Some(max) = q.max_memory_mib
            && mem + memory_mib > max
        {
            return Err(TenancyError::Quota(format!(
                "tenant '{tenant}' uses {mem} of {max} MiB; this VM asks for {memory_mib}"
            )));
        }
        Ok(())
    }

    /// The vmnet network a tenant's `vz` VMs join when [`Policy::isolate_tenant_networks`] is on: a shared-mode
    /// network named `fluxvm-tenant-<id>` on the tenant's own /24, assigning the tenant the lowest free index on first
    /// use. Returns `None` when isolation is off, the tenant is unknown, the prefix is malformed, or all 254 /24s are
    /// taken.
    pub fn tenant_vmnet(&mut self, tenant: &str) -> Option<Value> {
        if !self.policy.isolate_tenant_networks {
            return None;
        }
        let prefix = self.policy.tenant_subnet_prefix.clone();
        let octets: Vec<&str> = prefix.split('.').collect();
        if octets.len() != 2 || octets.iter().any(|o| o.parse::<u8>().is_err()) {
            return None;
        }
        let index = match self.tenants.get(tenant)?.network_index {
            Some(i) => i,
            None => {
                let used: BTreeSet<u8> = self
                    .tenants
                    .values()
                    .filter_map(|t| t.network_index)
                    .collect();
                let free = (1..=254u8).find(|i| !used.contains(i))?;
                self.tenants.get_mut(tenant)?.network_index = Some(free);
                free
            }
        };
        Some(serde_json::json!({
            "name": format!("fluxvm-tenant-{tenant}"),
            "mode": "shared",
            "subnet": format!("{prefix}.{index}.0"),
            "mask": "255.255.255.0",
        }))
    }

    /// Creates a private network spanning `nodes`: the lowest free subnet and UDP port, a fresh key, and a host
    /// slice per node.
    pub fn create_overlay(
        &mut self,
        name: &str,
        tenant: Option<&str>,
        nodes: &[String],
        hosts_per_node: u8,
        mtu: u32,
        now: DateTime<Utc>,
    ) -> Result<OverlayNet, TenancyError> {
        if !valid_network_name(name) {
            return Err(TenancyError::Overlay(format!(
                "bad network name {name:?}: 1-40 lowercase letters, digits and '-'"
            )));
        }
        if self.overlays.contains_key(name) {
            return Err(TenancyError::Overlay(format!(
                "network {name:?} already exists"
            )));
        }
        if let Some(t) = tenant {
            self.require_tenant(t)?;
        }
        if !(2..=126).contains(&hosts_per_node) {
            return Err(TenancyError::Overlay("hosts_per_node must be 2-126".into()));
        }
        if !(1280..=9000).contains(&mtu) {
            return Err(TenancyError::Overlay("mtu must be 1280-9000".into()));
        }
        let subnets: BTreeSet<u8> = self.overlays.values().map(|o| o.subnet).collect();
        // Leave low subnets to Mac-local networks, which allocate from 0 upwards.
        let subnet = (128..=255u8)
            .find(|n| !subnets.contains(n))
            .ok_or_else(|| TenancyError::Overlay("all overlay subnets are in use".into()))?;
        let ports: BTreeSet<u16> = self.overlays.values().map(|o| o.port).collect();
        let port = (OVERLAY_PORT_BASE..OVERLAY_PORT_BASE + 1000)
            .find(|p| !ports.contains(p))
            .ok_or_else(|| TenancyError::Overlay("no free overlay port".into()))?;
        let key_hex = random_key_hex().ok_or_else(|| {
            TenancyError::Overlay("could not read random bytes for the key".into())
        })?;
        let mut net = OverlayNet {
            name: name.to_owned(),
            tenant: tenant.map(str::to_owned),
            subnet,
            port,
            key_hex,
            epoch: 1,
            previous: None,
            rotated_at: Some(now),
            mtu,
            hosts_per_node,
            members: BTreeMap::new(),
            created_at: now,
        };
        for n in nodes {
            self.check_overlay_member(&net, n)?;
            let slice = net.free_slice().ok_or_else(|| {
                TenancyError::Overlay(format!("network {name:?} has no room for more Macs"))
            })?;
            net.members.insert(n.clone(), slice);
        }
        self.overlays.insert(name.to_owned(), net.clone());
        Ok(net)
    }

    /// A tenant's network may only include Macs that tenant leases.
    fn check_overlay_member(&self, net: &OverlayNet, node: &str) -> Result<(), TenancyError> {
        if let Some(t) = &net.tenant
            && self
                .active_lease_for_node(node)
                .is_none_or(|l| &l.tenant != t)
        {
            return Err(TenancyError::NotYours(format!(
                "node {node:?} is not leased to tenant {t:?}, so it cannot join its network"
            )));
        }
        Ok(())
    }

    /// The overlay `name` as `caller` may see it.
    pub fn overlay_for(&self, caller: &Caller, name: &str) -> Result<&OverlayNet, TenancyError> {
        let o = self
            .overlays
            .get(name)
            .ok_or_else(|| TenancyError::Overlay(format!("no network {name:?}")))?;
        match caller.tenant() {
            Some(t) if o.tenant.as_deref() != Some(t) => {
                Err(TenancyError::Overlay(format!("no network {name:?}")))
            }
            _ => Ok(o),
        }
    }

    pub fn add_overlay_member(
        &mut self,
        caller: &Caller,
        name: &str,
        node: &str,
    ) -> Result<OverlayNet, TenancyError> {
        let net = self.overlay_for(caller, name)?.clone();
        if net.members.contains_key(node) {
            return Ok(net);
        }
        self.check_overlay_member(&net, node)?;
        let slice = net.free_slice().ok_or_else(|| {
            TenancyError::Overlay(format!("network {name:?} has no room for more Macs"))
        })?;
        let entry = self.overlays.get_mut(name).expect("checked above");
        entry.members.insert(node.to_owned(), slice);
        Ok(entry.clone())
    }

    pub fn remove_overlay_member(
        &mut self,
        caller: &Caller,
        name: &str,
        node: &str,
    ) -> Result<OverlayNet, TenancyError> {
        self.overlay_for(caller, name)?;
        let entry = self.overlays.get_mut(name).expect("checked above");
        entry.members.remove(node);
        Ok(entry.clone())
    }

    /// A new key at the next epoch; the old one stays as `previous` until the next rotation, so traffic keeps flowing
    /// while member Macs pick the new key up (within a heartbeat).
    pub fn rotate_overlay(
        &mut self,
        caller: &Caller,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<OverlayNet, TenancyError> {
        let o = self.overlay_for(caller, name)?;
        // Switches keep sealing with the old key for a minute after loading a new one, and reach peers within about
        // 20 s; a second rotation inside that would retire a key some Mac may still be sealing with.
        if let Some(at) = o.rotated_at
            && now - at < Duration::minutes(MIN_ROTATION_MINUTES)
        {
            return Err(TenancyError::Overlay(format!(
                "network {name:?} was rotated less than {MIN_ROTATION_MINUTES} minutes ago"
            )));
        }
        let key = random_key_hex().ok_or_else(|| {
            TenancyError::Overlay("could not read random bytes for the key".into())
        })?;
        let o = self.overlays.get_mut(name).expect("checked above");
        let old = std::mem::replace(&mut o.key_hex, key);
        o.previous = Some((o.epoch, old));
        o.epoch = o.epoch.wrapping_add(1);
        o.rotated_at = Some(now);
        Ok(o.clone())
    }

    /// Rotates every network whose key is older than `max_age`. Returns the names rotated.
    pub fn rotate_due_overlays(
        &mut self,
        max_age: chrono::Duration,
        now: DateTime<Utc>,
    ) -> Vec<String> {
        let due: Vec<String> = self
            .overlays
            .values()
            .filter(|o| now - o.rotated_at.unwrap_or(o.created_at) >= max_age)
            .map(|o| o.name.clone())
            .collect();
        due.into_iter()
            .filter(|n| self.rotate_overlay(&Caller::Admin, n, now).is_ok())
            .collect()
    }

    pub fn delete_overlay(&mut self, caller: &Caller, name: &str) -> Result<(), TenancyError> {
        self.overlay_for(caller, name)?;
        self.overlays.remove(name);
        Ok(())
    }

    /// The networks `node` belongs to.
    pub fn overlays_of_node(&self, node: &str) -> Vec<&OverlayNet> {
        self.overlays
            .values()
            .filter(|o| o.members.contains_key(node))
            .collect()
    }

    /// Takes `node` out of every network of `tenant` (its lease ended).
    fn drop_tenant_overlay_member(&mut self, tenant: &str, node: &str) {
        for o in self.overlays.values_mut() {
            if o.tenant.as_deref() == Some(tenant) {
                o.members.remove(node);
            }
        }
    }

    /// Checks the quota and, if it fits, holds that much for an in-flight create, so parallel creates by one tenant
    /// cannot overshoot. Release it with [`TenancyState::release_reservation`] once the create succeeded (after
    /// [`TenancyState::record_vm`]) or failed.
    pub fn reserve(
        &mut self,
        tenant: &str,
        vcpus: u32,
        memory_mib: u64,
    ) -> Result<u64, TenancyError> {
        self.check_quota(tenant, vcpus, memory_mib)?;
        self.next_reservation += 1;
        let id = self.next_reservation;
        self.reservations.push(Reservation {
            id,
            tenant: tenant.to_owned(),
            vcpus,
            memory_mib,
        });
        Ok(id)
    }

    pub fn release_reservation(&mut self, id: u64) -> bool {
        let before = self.reservations.len();
        self.reservations.retain(|r| r.id != id);
        self.reservations.len() != before
    }

    // ---- ledger -----------------------------------------------------------------------------------------------

    pub fn record_vm(&mut self, entry: VmEntry) {
        self.vms.push(entry);
    }

    pub fn vm(&self, node: &str, vm_id: &str) -> Option<&VmEntry> {
        self.vms
            .iter()
            .rev()
            .find(|v| v.node == node && v.vm_id == vm_id)
    }

    /// Whether `caller` may act on this VM: operators always; tenants only on VMs the ledger says are theirs.
    pub fn owns(&self, caller: &Caller, node: &str, vm_id: &str) -> bool {
        match caller {
            Caller::Admin => true,
            Caller::Tenant(t) => self
                .vm(node, vm_id)
                .is_some_and(|v| v.tenant.as_deref() == Some(t)),
        }
    }

    pub fn end_vm(&mut self, node: &str, vm_id: &str, now: DateTime<Utc>) -> bool {
        match self
            .vms
            .iter_mut()
            .rev()
            .find(|v| v.node == node && v.vm_id == vm_id && v.active())
        {
            Some(v) => {
                v.ended_at = Some(now);
                true
            }
            None => false,
        }
    }

    /// Closes every active entry on `node` whose VM the node no longer lists. Entries younger than
    /// [`RECONCILE_GRACE_SECS`] are left alone. Returns how many it closed.
    pub fn reconcile_node(
        &mut self,
        node: &str,
        present: &HashSet<String>,
        now: DateTime<Utc>,
    ) -> usize {
        let mut closed = 0;
        for v in self.vms.iter_mut() {
            if v.node == node
                && v.active()
                && !present.contains(&v.vm_id)
                && (now - v.created_at).num_seconds() >= RECONCILE_GRACE_SECS
            {
                v.ended_at = Some(now);
                closed += 1;
            }
        }
        closed
    }

    /// Active ledger entries on `node` belonging to `tenant`.
    pub fn active_vms_of_on(&self, tenant: &str, node: &str) -> Vec<VmEntry> {
        self.vms
            .iter()
            .filter(|v| v.active() && v.node == node && v.tenant.as_deref() == Some(tenant))
            .cloned()
            .collect()
    }

    /// Drops ended ledger entries and released leases that ended before `before`, keeping the file bounded.
    /// Usage for windows before `before` is no longer available afterwards: export it first.
    pub fn compact(&mut self, before: DateTime<Utc>) -> usize {
        let n = self.vms.len() + self.leases.len();
        self.vms.retain(|v| v.ended_at.is_none_or(|e| e >= before));
        self.leases.retain(|l| {
            l.state == LeaseState::Active
                || l.released_at.is_none_or(|r| r.max(l.min_end) >= before)
        });
        n - (self.vms.len() + self.leases.len())
    }

    // ---- metering ---------------------------------------------------------------------------------------------

    /// Usage in `[from, to)`, per tenant (VMs created without a tenant are reported under `""`).
    pub fn usage(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        only: Option<&str>,
        now: DateTime<Utc>,
    ) -> UsageReport {
        let mut by: BTreeMap<String, TenantUsage> = BTreeMap::new();
        let keep = |t: &str| only.is_none_or(|o| o == t);
        for v in &self.vms {
            let tenant = v.tenant.clone().unwrap_or_default();
            if !keep(&tenant) {
                continue;
            }
            let secs = overlap_secs(v.created_at, v.ended_at.unwrap_or(now), from, to);
            let started = v.created_at >= from && v.created_at < to;
            if secs == 0 && !started {
                continue;
            }
            let u = by.entry(tenant.clone()).or_insert_with(|| TenantUsage {
                tenant,
                ..Default::default()
            });
            u.vm_seconds += secs;
            u.vcpu_seconds += secs * i64::from(v.vcpus);
            u.memory_mib_seconds += secs * v.memory_mib as i64;
            if v.macos_guest {
                u.macos_vm_seconds += secs;
            }
            if started {
                u.vms_started += 1;
            }
        }
        for l in &self.leases {
            if !keep(&l.tenant) {
                continue;
            }
            let actual = overlap_secs(l.started_at, l.released_at.unwrap_or(now), from, to);
            let billable = overlap_secs(l.started_at, l.billable_end(now), from, to);
            let started = l.started_at >= from && l.started_at < to;
            if actual == 0 && billable == 0 && !started {
                continue;
            }
            let u = by.entry(l.tenant.clone()).or_insert_with(|| TenantUsage {
                tenant: l.tenant.clone(),
                ..Default::default()
            });
            u.lease_seconds += actual;
            u.billable_lease_seconds += billable;
            if started {
                u.leases_started += 1;
            }
        }
        UsageReport {
            from,
            to,
            tenants: by.into_values().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    }

    fn state_with(tenants: &[&str]) -> TenancyState {
        let mut s = TenancyState::default();
        for t in tenants {
            s.create_tenant(t, None, Quota::default(), t0()).unwrap();
        }
        s
    }

    fn vm(node: &str, id: &str, tenant: Option<&str>, at: DateTime<Utc>) -> VmEntry {
        VmEntry {
            node: node.into(),
            vm_id: id.into(),
            name: id.into(),
            tenant: tenant.map(str::to_owned),
            vcpus: 2,
            memory_mib: 4096,
            macos_guest: false,
            created_at: at,
            ended_at: None,
            spec: None,
        }
    }

    #[test]
    fn tenant_ids_are_validated_and_unique() {
        let mut s = TenancyState::default();
        assert!(matches!(
            s.create_tenant("Acme", None, Quota::default(), t0()),
            Err(TenancyError::InvalidTenantId(_))
        ));
        assert!(matches!(
            s.create_tenant("-acme", None, Quota::default(), t0()),
            Err(TenancyError::InvalidTenantId(_))
        ));
        assert!(matches!(
            s.create_tenant("me", None, Quota::default(), t0()),
            Err(TenancyError::InvalidTenantId(_))
        ));
        s.create_tenant("acme", None, Quota::default(), t0())
            .unwrap();
        assert_eq!(
            s.create_tenant("acme", None, Quota::default(), t0())
                .unwrap_err(),
            TenancyError::TenantExists("acme".into())
        );
    }

    #[test]
    fn tokens_authenticate_only_their_tenant_and_rotate() {
        let mut s = TenancyState::default();
        let (_, a) = s.create_tenant("a", None, Quota::default(), t0()).unwrap();
        let (_, b) = s.create_tenant("b", None, Quota::default(), t0()).unwrap();
        assert_ne!(a, b);
        assert!(a.starts_with("fvt_") && a.len() == 68);
        assert_eq!(s.authenticate(&a).unwrap().id, "a");
        assert_eq!(s.authenticate(&b).unwrap().id, "b");
        assert!(s.authenticate("nope").is_none());
        // The token itself is never stored.
        assert!(!serde_json::to_string(&s).unwrap().contains(&a));
        let a2 = s.rotate_token("a").unwrap();
        assert!(s.authenticate(&a).is_none());
        assert_eq!(s.authenticate(&a2).unwrap().id, "a");
    }

    #[test]
    fn leases_are_at_least_24_hours_and_exclusive() {
        let mut s = state_with(&["a", "b"]);
        assert_eq!(
            s.create_lease("a", "mac1", Some(12), t0()).unwrap_err(),
            TenancyError::LeaseTooShort { hours: 12, min: 24 }
        );
        // A policy asking for less than 24 h is raised to 24.
        s.policy.min_lease_hours = 1;
        assert!(s.create_lease("a", "mac1", Some(2), t0()).is_err());
        let l = s.create_lease("a", "mac1", None, t0()).unwrap();
        assert_eq!(l.min_end, t0() + Duration::hours(24));
        assert!(matches!(
            s.create_lease("b", "mac1", None, t0()),
            Err(TenancyError::NodeLeased { .. })
        ));
    }

    #[test]
    fn disabled_or_unknown_tenants_cannot_lease() {
        let mut s = state_with(&["a"]);
        s.update_tenant("a", None, Some(true), None).unwrap();
        assert_eq!(
            s.create_lease("a", "mac1", None, t0()).unwrap_err(),
            TenancyError::TenantDisabled("a".into())
        );
        assert_eq!(
            s.create_lease("zz", "mac1", None, t0()).unwrap_err(),
            TenancyError::UnknownTenant("zz".into())
        );
    }

    #[test]
    fn leased_host_quota_is_enforced() {
        let mut s = state_with(&["a"]);
        s.update_tenant(
            "a",
            Some(Quota {
                max_leased_hosts: Some(1),
                ..Default::default()
            }),
            None,
            None,
        )
        .unwrap();
        s.create_lease("a", "mac1", None, t0()).unwrap();
        assert!(matches!(
            s.create_lease("a", "mac2", None, t0()),
            Err(TenancyError::Quota(_))
        ));
    }

    #[test]
    fn early_release_is_refused_for_tenants_and_forced_by_operators() {
        let mut s = state_with(&["a", "b"]);
        let l = s.create_lease("a", "mac1", None, t0()).unwrap();
        let early = t0() + Duration::hours(3);
        assert!(matches!(
            s.release_lease(&l.id, Some("a"), true, early),
            Err(TenancyError::EarlyRelease { .. })
        ));
        // Another tenant cannot even see it.
        assert_eq!(
            s.release_lease(&l.id, Some("b"), false, early).unwrap_err(),
            TenancyError::UnknownLease(l.id.clone())
        );
        let r = s.release_lease(&l.id, None, true, early).unwrap();
        assert_eq!(r.state, LeaseState::Released);
        // Billed to the 24-hour minimum anyway.
        assert_eq!(r.billable_end(early), t0() + Duration::hours(24));
        assert!(s.scrub_pending.contains("mac1"));
        assert!(matches!(
            s.create_lease("b", "mac1", None, early),
            Err(TenancyError::NodeNeedsScrub(_))
        ));
        assert!(s.mark_scrubbed("mac1"));
        s.create_lease("b", "mac1", None, early).unwrap();
    }

    #[test]
    fn tenant_release_after_minimum_works() {
        let mut s = state_with(&["a"]);
        let l = s.create_lease("a", "mac1", None, t0()).unwrap();
        let later = t0() + Duration::hours(30);
        let r = s.release_lease(&l.id, Some("a"), false, later).unwrap();
        assert_eq!(r.billable_end(later), later);
        assert_eq!(
            s.release_lease(&l.id, Some("a"), false, later).unwrap_err(),
            TenancyError::LeaseNotActive(l.id.clone())
        );
    }

    #[test]
    fn leases_with_an_end_expire() {
        let mut s = state_with(&["a"]);
        s.create_lease("a", "mac1", Some(48), t0()).unwrap();
        assert!(s.expire_leases(t0() + Duration::hours(47)).is_empty());
        let ended = s.expire_leases(t0() + Duration::hours(49));
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].released_at, Some(t0() + Duration::hours(48)));
        assert!(s.active_lease_for_node("mac1").is_none());
        assert!(s.scrub_pending.contains("mac1"));
    }

    #[test]
    fn admission_keeps_macos_on_the_tenants_own_leased_macs() {
        let mut s = state_with(&["a", "b"]);
        s.create_lease("a", "mac1", None, t0()).unwrap();
        let a = Caller::Tenant("a".into());
        let b = Caller::Tenant("b".into());
        // a: macOS on its own lease, Linux anywhere not leased by someone else.
        assert!(s.admit("mac1", &a, true, false).is_ok());
        assert!(s.admit("mac2", &a, true, false).is_err());
        assert!(s.admit("mac2", &a, false, false).is_ok());
        // b: never on a's lease, even Linux, even naming the node.
        assert!(s.admit("mac1", &b, false, false).is_err());
        assert!(s.admit("mac1", &b, false, true).is_err());
        // Operators: automatic placement respects leases, an explicit node bypasses them.
        assert!(s.admit("mac1", &Caller::Admin, false, false).is_err());
        assert!(s.admit("mac1", &Caller::Admin, false, true).is_ok());
        assert!(s.admit("mac2", &Caller::Admin, true, false).is_ok());
    }

    #[test]
    fn admission_is_off_without_tenants_and_follows_policy() {
        let s = TenancyState::default();
        assert!(
            s.admit("mac1", &Caller::Tenant("x".into()), true, false)
                .is_ok()
        );
        let mut s = state_with(&["a", "b"]);
        s.create_lease("a", "mac1", None, t0()).unwrap();
        s.policy.exclusive_leased_hosts = false;
        s.policy.require_lease_for_macos = false;
        let b = Caller::Tenant("b".into());
        assert!(s.admit("mac1", &b, false, false).is_ok());
        assert!(s.admit("mac2", &b, true, false).is_ok());
    }

    #[test]
    fn scrub_pending_nodes_take_no_automatic_placements() {
        let mut s = state_with(&["a"]);
        s.scrub_pending.insert("mac1".into());
        assert!(
            s.admit("mac1", &Caller::Tenant("a".into()), false, false)
                .is_err()
        );
        assert!(s.admit("mac1", &Caller::Admin, false, false).is_err());
        assert!(s.admit("mac1", &Caller::Admin, false, true).is_ok());
    }

    #[test]
    fn quotas_count_only_active_vms_of_the_tenant() {
        let mut s = state_with(&["a", "b"]);
        s.update_tenant(
            "a",
            Some(Quota {
                max_vms: Some(2),
                max_vcpus: Some(5),
                max_memory_mib: Some(10_000),
                max_leased_hosts: None,
            }),
            None,
            None,
        )
        .unwrap();
        s.record_vm(vm("n", "1", Some("a"), t0()));
        s.record_vm(vm("n", "2", Some("b"), t0()));
        assert!(s.check_quota("a", 2, 4096).is_ok());
        assert!(matches!(
            s.check_quota("a", 4, 1),
            Err(TenancyError::Quota(_))
        ));
        assert!(matches!(
            s.check_quota("a", 1, 8000),
            Err(TenancyError::Quota(_))
        ));
        s.record_vm(vm("n", "3", Some("a"), t0()));
        assert!(matches!(
            s.check_quota("a", 1, 1),
            Err(TenancyError::Quota(_))
        ));
        assert!(s.end_vm("n", "3", t0()));
        assert!(s.check_quota("a", 1, 1).is_ok());
    }

    #[test]
    fn reservations_hold_quota_until_released() {
        let mut s = state_with(&["a"]);
        s.update_tenant(
            "a",
            Some(Quota {
                max_vms: Some(2),
                max_memory_mib: Some(8192),
                ..Default::default()
            }),
            None,
            None,
        )
        .unwrap();
        let r1 = s.reserve("a", 2, 4096).unwrap();
        let r2 = s.reserve("a", 2, 4096).unwrap();
        assert_ne!(r1, r2);
        // Two creates in flight fill the quota: a third is refused before any node is asked.
        assert!(matches!(s.reserve("a", 1, 1), Err(TenancyError::Quota(_))));
        // One succeeded (recorded, then released): still full.
        s.record_vm(vm("n", "1", Some("a"), t0()));
        assert!(s.release_reservation(r1));
        assert!(s.reserve("a", 1, 1).is_err());
        // The other failed: its hold goes away.
        assert!(s.release_reservation(r2));
        assert!(!s.release_reservation(r2));
        assert!(s.reserve("a", 1, 1).is_ok());
        // Reservations are never persisted.
        let back: TenancyState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.reservations.is_empty());
    }

    #[test]
    fn tenant_networks_get_stable_distinct_subnets() {
        let mut s = state_with(&["a", "b"]);
        assert!(s.tenant_vmnet("a").is_none(), "off by default");
        s.policy.isolate_tenant_networks = true;
        let a = s.tenant_vmnet("a").unwrap();
        let b = s.tenant_vmnet("b").unwrap();
        assert_eq!(a["name"], "fluxvm-tenant-a");
        assert_eq!(a["subnet"], "10.230.1.0");
        assert_eq!(b["subnet"], "10.230.2.0");
        assert_eq!(a["mode"], "shared");
        // Stable across calls and restarts.
        assert_eq!(s.tenant_vmnet("a").unwrap(), a);
        let back: TenancyState = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.tenants["a"].network_index, Some(1));
        // A freed index is reused.
        s.delete_tenant("a").unwrap();
        s.create_tenant("c", None, Quota::default(), t0()).unwrap();
        assert_eq!(s.tenant_vmnet("c").unwrap()["subnet"], "10.230.1.0");
        assert!(s.tenant_vmnet("nobody").is_none());
        s.policy.tenant_subnet_prefix = "10.300".into();
        assert!(s.tenant_vmnet("b").is_none());
    }

    #[test]
    fn overlays_get_unique_subnets_ports_keys_and_disjoint_slices() {
        let mut s = state_with(&["a"]);
        s.create_lease("a", "mac1", None, t0()).unwrap();
        s.create_lease("a", "mac2", None, t0()).unwrap();
        let nodes = vec!["mac1".to_string(), "mac2".to_string()];
        let n1 = s
            .create_overlay("a-net", Some("a"), &nodes, 30, 1400, t0())
            .unwrap();
        assert_eq!(n1.subnet, 128);
        assert_eq!(n1.port, OVERLAY_PORT_BASE);
        assert_eq!(n1.key_hex.len(), 64);
        assert_eq!(n1.members["mac1"], [2, 31]);
        assert_eq!(n1.members["mac2"], [32, 61]);
        assert!(
            !n1.public_json().to_string().contains(&n1.key_hex),
            "the key never leaves in public JSON"
        );
        let n2 = s.create_overlay("lab", None, &[], 30, 1400, t0()).unwrap();
        assert_ne!(n2.subnet, n1.subnet);
        assert_ne!(n2.port, n1.port);
        assert_ne!(n2.key_hex, n1.key_hex);
        assert!(s.create_overlay("lab", None, &[], 30, 1400, t0()).is_err());
        assert!(
            s.create_overlay("Bad_Name", None, &[], 30, 1400, t0())
                .is_err()
        );
        assert_eq!(s.overlays_of_node("mac1").len(), 1);
    }

    #[test]
    fn tenant_overlays_follow_leases() {
        let mut s = state_with(&["a", "b"]);
        s.create_lease("a", "mac1", None, t0()).unwrap();
        let l2 = s.create_lease("a", "mac2", None, t0()).unwrap();
        s.create_lease("b", "mac3", None, t0()).unwrap();
        // Only leased Macs may join.
        assert!(
            s.create_overlay("a-net", Some("a"), &["mac3".into()], 30, 1400, t0())
                .is_err()
        );
        s.create_overlay("a-net", Some("a"), &["mac1".into()], 30, 1400, t0())
            .unwrap();
        let a = Caller::Tenant("a".into());
        let b = Caller::Tenant("b".into());
        assert_eq!(
            s.add_overlay_member(&a, "a-net", "mac2").unwrap().members["mac2"],
            [32, 61]
        );
        assert!(s.add_overlay_member(&a, "a-net", "mac3").is_err());
        // Another tenant cannot see or change it.
        assert!(s.overlay_for(&b, "a-net").is_err());
        assert!(s.add_overlay_member(&b, "a-net", "mac3").is_err());
        assert!(s.delete_overlay(&b, "a-net").is_err());
        // The lease ends: the Mac leaves the network.
        s.release_lease(&l2.id, None, true, t0()).unwrap();
        assert!(!s.overlays["a-net"].members.contains_key("mac2"));
        // A freed slice is reused.
        s.create_lease("a", "mac4", None, t0()).unwrap();
        assert_eq!(
            s.add_overlay_member(&a, "a-net", "mac4").unwrap().members["mac4"],
            [32, 61]
        );
        s.delete_overlay(&a, "a-net").unwrap();
        assert!(s.overlays.is_empty());
    }

    #[test]
    fn overlay_keys_rotate_and_keep_the_previous_one() {
        let mut s = TenancyState::default();
        let n = s.create_overlay("lab", None, &[], 30, 1400, t0()).unwrap();
        assert_eq!(n.epoch, 1);
        assert!(n.previous.is_none());
        let r = s
            .rotate_overlay(&Caller::Admin, "lab", t0() + Duration::days(1))
            .unwrap();
        assert_eq!(r.epoch, 2);
        assert_eq!(r.previous, Some((1, n.key_hex.clone())));
        assert_ne!(r.key_hex, n.key_hex);
        assert!(!r.public_json().to_string().contains(&r.key_hex));
        // Automatic rotation only for keys older than the limit.
        assert!(
            s.rotate_due_overlays(Duration::days(30), t0() + Duration::days(10))
                .is_empty()
        );
        assert_eq!(
            s.rotate_due_overlays(Duration::days(30), t0() + Duration::days(32)),
            vec!["lab".to_string()]
        );
        assert_eq!(s.overlays["lab"].epoch, 3);
        assert_eq!(s.overlays["lab"].previous.as_ref().unwrap().0, 2);
        // Not twice within a few minutes.
        assert!(
            s.rotate_overlay(
                &Caller::Admin,
                "lab",
                t0() + Duration::days(32) + Duration::minutes(1)
            )
            .is_err()
        );
        // A tenant cannot rotate someone else's network.
        assert!(
            s.rotate_overlay(&Caller::Tenant("x".into()), "lab", t0())
                .is_err()
        );
    }

    #[test]
    fn slices_run_out_cleanly() {
        let mut s = TenancyState::default();
        let nodes: Vec<String> = (0..3).map(|i| format!("m{i}")).collect();
        let n = s
            .create_overlay("big", None, &nodes[..2], 126, 1400, t0())
            .unwrap();
        assert_eq!(n.members["m0"], [2, 127]);
        assert_eq!(n.members["m1"], [128, 253]);
        assert!(s.add_overlay_member(&Caller::Admin, "big", "m2").is_err());
    }

    #[test]
    fn ownership_follows_the_ledger() {
        let mut s = state_with(&["a", "b"]);
        s.record_vm(vm("n", "1", Some("a"), t0()));
        assert!(s.owns(&Caller::Tenant("a".into()), "n", "1"));
        assert!(!s.owns(&Caller::Tenant("b".into()), "n", "1"));
        assert!(!s.owns(&Caller::Tenant("a".into()), "n", "unknown"));
        assert!(s.owns(&Caller::Admin, "n", "unknown"));
    }

    #[test]
    fn reconcile_closes_vanished_vms_after_the_grace_period() {
        let mut s = TenancyState::default();
        s.record_vm(vm("n", "old", None, t0()));
        s.record_vm(vm("n", "kept", None, t0()));
        s.record_vm(vm("n", "new", None, t0() + Duration::seconds(100)));
        s.record_vm(vm("other", "x", None, t0()));
        let present: HashSet<String> = ["kept".to_string()].into();
        let now = t0() + Duration::seconds(150);
        assert_eq!(s.reconcile_node("n", &present, now), 1);
        assert!(!s.vm("n", "old").unwrap().active());
        assert!(s.vm("n", "kept").unwrap().active());
        assert!(s.vm("n", "new").unwrap().active());
        assert!(s.vm("other", "x").unwrap().active());
    }

    #[test]
    fn usage_clips_to_the_window_and_bills_lease_minimums() {
        let mut s = state_with(&["a"]);
        let mut v = vm("n", "1", Some("a"), t0());
        v.ended_at = Some(t0() + Duration::hours(2));
        v.macos_guest = true;
        s.record_vm(v);
        s.record_vm(vm("n", "2", None, t0() + Duration::hours(1)));
        let l = s.create_lease("a", "mac1", None, t0()).unwrap();
        s.release_lease(&l.id, None, true, t0() + Duration::hours(6))
            .unwrap();

        let from = t0() + Duration::hours(1);
        let to = t0() + Duration::hours(25);
        let now = t0() + Duration::hours(26);
        let r = s.usage(from, to, None, now);
        assert_eq!(r.tenants.len(), 2);
        let untenanted = &r.tenants[0];
        assert_eq!(untenanted.tenant, "");
        assert_eq!(untenanted.vm_seconds, 24 * 3600);
        assert_eq!(untenanted.vms_started, 1);
        let a = &r.tenants[1];
        assert_eq!(a.vm_seconds, 3600);
        assert_eq!(a.macos_vm_seconds, 3600);
        assert_eq!(a.vcpu_seconds, 2 * 3600);
        assert_eq!(a.memory_mib_seconds, 4096 * 3600);
        assert_eq!(a.vms_started, 0);
        assert_eq!(a.lease_seconds, 5 * 3600);
        // Billed from the window start to the 24-hour minimum.
        assert_eq!(a.billable_lease_seconds, 23 * 3600);
        assert_eq!(a.leases_started, 0);

        let only_a = s.usage(from, to, Some("a"), now);
        assert_eq!(only_a.tenants.len(), 1);
        let csv = only_a.to_csv();
        assert!(csv.starts_with("tenant,from,to,vm_hours"));
        assert!(csv.contains("\na,"));
        assert!(csv.contains(",1.0000,2.0000,4.0000,1.0000,5.0000,23.0000,0,0\n"));
    }

    #[test]
    fn tenants_with_work_cannot_be_deleted() {
        let mut s = state_with(&["a"]);
        let l = s.create_lease("a", "mac1", None, t0()).unwrap();
        assert_eq!(
            s.delete_tenant("a").unwrap_err(),
            TenancyError::TenantBusy("a".into())
        );
        s.release_lease(&l.id, None, true, t0()).unwrap();
        s.delete_tenant("a").unwrap();
        assert!(!s.enabled());
    }

    #[test]
    fn compaction_keeps_active_and_recent_records() {
        let mut s = state_with(&["a"]);
        let mut old = vm("n", "old", Some("a"), t0());
        old.ended_at = Some(t0() + Duration::hours(1));
        s.record_vm(old);
        s.record_vm(vm("n", "live", Some("a"), t0()));
        let l = s.create_lease("a", "mac1", None, t0()).unwrap();
        s.release_lease(&l.id, None, true, t0() + Duration::hours(1))
            .unwrap();
        // The released lease is billed to its 24-hour min_end, so it survives a cut before that.
        assert_eq!(s.compact(t0() + Duration::hours(10)), 1);
        assert_eq!(s.vms.len(), 1);
        assert_eq!(s.leases.len(), 1);
        assert_eq!(s.compact(t0() + Duration::hours(30)), 1);
        assert!(s.leases.is_empty());
    }

    #[test]
    fn state_round_trips_and_old_files_load() {
        let mut s = state_with(&["a"]);
        s.create_lease("a", "mac1", None, t0()).unwrap();
        let raw = serde_json::to_string(&s).unwrap();
        let back: TenancyState = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.leases.len(), 1);
        let empty: TenancyState = serde_json::from_str("{}").unwrap();
        assert!(empty.policy.require_lease_for_macos);
        assert_eq!(empty.policy.min_lease_hours, 24);
    }

    #[test]
    fn named_keys_authenticate_expire_and_revoke() {
        let now = Utc::now();
        let mut s = TenancyState::default();
        let (_, primary) = s
            .create_tenant("acme", None, Quota::default(), now)
            .unwrap();
        let (ci, ci_token) = s.create_key("acme", "ci-github", None, false, now).unwrap();
        let (_, ro_token) = s.create_key("acme", "grafana", None, true, now).unwrap();
        let (_, old_token) = s
            .create_key(
                "acme",
                "temp",
                Some(now + chrono::Duration::hours(1)),
                false,
                now,
            )
            .unwrap();

        let m = s.authenticate_key(&primary, now).unwrap();
        assert_eq!(
            (m.tenant.as_str(), m.key, m.read_only),
            ("acme", None, false)
        );
        let m = s.authenticate_key(&ci_token, now).unwrap();
        assert_eq!(m.key.as_deref(), Some("ci-github"));
        assert!(!m.read_only);
        assert!(s.authenticate_key(&ro_token, now).unwrap().read_only);
        assert!(s.tenants["acme"].keys[0].last_used_at.is_some());

        // Expired keys stop working; names are unique; bad names and past expiries are refused.
        assert!(
            s.authenticate_key(&old_token, now + chrono::Duration::hours(2))
                .is_none()
        );
        assert!(s.create_key("acme", "ci-github", None, false, now).is_err());
        assert!(s.create_key("acme", " ", None, false, now).is_err());
        assert!(
            s.create_key("acme", "key_0123456789ab", None, false, now)
                .is_err()
        );
        assert!(
            s.create_key(
                "acme",
                "x",
                Some(now - chrono::Duration::hours(1)),
                false,
                now
            )
            .is_err()
        );

        // Revoking by id or name; the token stops working, the others keep working.
        s.revoke_key("acme", &ci.id).unwrap();
        assert!(s.authenticate_key(&ci_token, now).is_none());
        s.revoke_key("acme", "grafana").unwrap();
        assert!(s.authenticate_key(&ro_token, now).is_none());
        assert!(s.authenticate_key(&primary, now).is_some());
        assert!(matches!(
            s.revoke_key("acme", "nope"),
            Err(TenancyError::UnknownKey(_))
        ));

        // The hash never shows.
        let k = s.create_key("acme", "k", None, false, now).unwrap().0;
        assert!(!k.public_json().to_string().contains(&k.token_sha256));
    }
}
