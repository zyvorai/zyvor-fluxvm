// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The Mac-cloud half of `fluxvm-agent central`: tenants, host leases, scrub, drain, metering, the ledger
//! reconciler and an exec proxy. Rules live in [`crate::tenancy`]; this module is HTTP and node I/O.
//!
//! Routes (operator-only unless marked *tenant*):
//!
//! | Route | What |
//! |---|---|
//! | `GET/POST /fleet/tenants`, `GET/PATCH/DELETE /fleet/tenants/{id}`, `POST …/{id}/rotate-token` | tenant admin |
//! | `GET /fleet/tenants/me` (*tenant*) | own record, quota and current use |
//! | `GET/POST /fleet/leases`, `GET /fleet/leases/{id}`, `POST …/{id}/release` (*tenant*) | Mac leases |
//! | `POST /fleet/nodes/{name}/scrub` | clear a released node for reuse |
//! | `POST/GET /fleet/nodes/{name}/drain` | cordon, then evict or reschedule |
//! | `POST /fleet/vms/{node}/{id}/exec` (*tenant*) | guest exec through the node's agent |
//! | `GET /fleet/usage` (*tenant*: own only) | metering, JSON or CSV |
//! | `POST /fleet/reconcile`, `GET/PUT /fleet/policy` | ledger reconcile, licence policy |

use super::*;
use crate::tenancy::{Lease, LeaseState, Policy, Quota};
use axum::extract::Query;
use chrono::Duration as ChronoDuration;

fn require_admin(caller: &Caller) -> Result<(), AppError> {
    match caller {
        Caller::Admin => Ok(()),
        Caller::Tenant(t) => Err(AppError(
            StatusCode::FORBIDDEN,
            format!("tenant '{t}' may not call this route"),
        )),
    }
}

fn not_found(what: String) -> AppError {
    AppError(StatusCode::NOT_FOUND, what)
}

async fn node_by_name(fleet: &Fleet, name: &str) -> Result<NodeInfo, AppError> {
    fleet
        .nodes
        .lock()
        .await
        .get(name)
        .cloned()
        .ok_or_else(|| not_found(format!("no registered node named '{name}'")))
}

/// `GET {node}/v1/vms` items.
async fn fetch_node_vms(fleet: &Fleet, node: &NodeInfo) -> Result<Vec<Value>, AppError> {
    let resp = fleet
        .http
        .get(format!("{}/v1/vms", node.fluxvm_url))
        .send()
        .await
        .map_err(|e| {
            AppError(
                StatusCode::BAD_GATEWAY,
                format!("node '{}' unreachable: {e}", node.name),
            )
        })?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| {
        AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{}' sent an unreadable VM list: {e}", node.name),
        )
    })?;
    if !status.is_success() {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{}' rejected list ({status}): {body}", node.name),
        ));
    }
    Ok(body
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn vm_id(vm: &Value) -> String {
    vm.get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Deletes `tenant`'s active ledger VMs on `node` (lease release or expiry). Returns one result per VM.
async fn evict_tenant_vms(fleet: &Fleet, tenant: &str, node: &str) -> Vec<Value> {
    let entries = fleet.tenancy.lock().await.active_vms_of_on(tenant, node);
    let mut out = Vec::new();
    for e in entries {
        match delete_vm_on_node(fleet, node, &e.vm_id).await {
            Ok(_) => {
                let mut t = fleet.tenancy.lock().await;
                t.end_vm(node, &e.vm_id, Utc::now());
                fleet.persist_tenancy(&t);
                out.push(json!({"vm": e.vm_id, "name": e.name, "deleted": true}));
            }
            Err(err) => {
                out.push(json!({"vm": e.vm_id, "name": e.name, "deleted": false, "error": err.1}));
            }
        }
    }
    out
}

// ---- reconciler ---------------------------------------------------------------------------------------------------

pub(super) fn spawn_reconciler(fleet: Fleet, every: std::time::Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let report = reconcile_once(&fleet).await;
            tracing::debug!(%report, "fleet reconcile");
        }
    });
}

/// Ends expired leases (evicting their tenants' VMs) and closes ledger entries for VMs a node no longer lists.
pub(super) async fn reconcile_once(fleet: &Fleet) -> Value {
    let ended = {
        let mut t = fleet.tenancy.lock().await;
        let ended = t.expire_leases(Utc::now());
        if !ended.is_empty() {
            fleet.persist_tenancy(&t);
        }
        ended
    };
    let compacted = {
        let mut t = fleet.tenancy.lock().await;
        let days = t.policy.ledger_retention_days;
        let n = if days > 0 {
            t.compact(Utc::now() - ChronoDuration::days(days))
        } else {
            0
        };
        if n > 0 {
            fleet.persist_tenancy(&t);
        }
        n
    };
    let mut evicted = Vec::new();
    for l in &ended {
        tracing::info!(lease = %l.id, tenant = %l.tenant, node = %l.node, "lease expired");
        evicted.extend(evict_tenant_vms(fleet, &l.tenant, &l.node).await);
    }
    let targets: Vec<NodeInfo> = fleet
        .nodes
        .lock()
        .await
        .values()
        .filter(|n| n.healthy())
        .cloned()
        .collect();
    let mut closed = 0usize;
    let mut errors = Vec::new();
    for n in targets {
        match fetch_node_vms(fleet, &n).await {
            Ok(items) => {
                let ids: HashSet<String> = items.iter().map(vm_id).collect();
                let mut t = fleet.tenancy.lock().await;
                let c = t.reconcile_node(&n.name, &ids, Utc::now());
                if c > 0 {
                    fleet.persist_tenancy(&t);
                }
                closed += c;
            }
            Err(e) => errors.push(json!({"node": n.name, "error": e.1})),
        }
    }
    json!({
        "expired_leases": ended.iter().map(|l| l.id.clone()).collect::<Vec<_>>(),
        "evicted": evicted,
        "closed_ledger_entries": closed,
        "compacted_records": compacted,
        "errors": errors,
    })
}

/// `POST /fleet/reconcile`: one reconcile pass now.
pub(super) async fn reconcile(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    Ok(Json(reconcile_once(&fleet).await))
}

// ---- policy -------------------------------------------------------------------------------------------------------

pub(super) async fn get_policy(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Policy>, AppError> {
    require_admin(&caller)?;
    Ok(Json(fleet.tenancy.lock().await.policy.clone()))
}

pub(super) async fn put_policy(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(policy): Json<Policy>,
) -> Result<Json<Policy>, AppError> {
    require_admin(&caller)?;
    let mut t = fleet.tenancy.lock().await;
    t.policy = policy;
    if t.policy.min_lease_hours < crate::tenancy::MIN_LEASE_HOURS {
        t.policy.min_lease_hours = crate::tenancy::MIN_LEASE_HOURS;
    }
    fleet.persist_tenancy(&t);
    Ok(Json(t.policy.clone()))
}

// ---- tenants ------------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct CreateTenantBody {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    quota: Quota,
}

pub(super) async fn create_tenant(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(b): Json<CreateTenantBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    require_admin(&caller)?;
    let mut t = fleet.tenancy.lock().await;
    let (tenant, token) = t
        .create_tenant(&b.id, b.display_name, b.quota, Utc::now())
        .map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    tracing::info!(tenant = %tenant.id, "tenant created");
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "tenant": tenant.public_json(),
            "token": token,
            "note": "store this token now; central keeps only its hash and cannot show it again",
        })),
    ))
}

pub(super) async fn list_tenants(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let t = fleet.tenancy.lock().await;
    let items: Vec<Value> = t.tenants.values().map(|x| x.public_json()).collect();
    Ok(Json(json!({"items": items})))
}

/// A tenant's record plus what it is using right now.
fn tenant_view(t: &TenancyState, id: &str) -> Option<Value> {
    let tenant = t.tenants.get(id)?;
    let (mut vms, mut vcpus, mut mem) = (0u32, 0u64, 0u64);
    for v in t
        .vms
        .iter()
        .filter(|v| v.active() && v.tenant.as_deref() == Some(id))
    {
        vms += 1;
        vcpus += u64::from(v.vcpus);
        mem += v.memory_mib;
    }
    let leases: Vec<&Lease> = t
        .leases
        .iter()
        .filter(|l| l.tenant == id && l.state == LeaseState::Active)
        .collect();
    let mut out = tenant.public_json();
    out["in_use"] = json!({
        "vms": vms,
        "vcpus": vcpus,
        "memory_mib": mem,
        "leased_hosts": leases.len(),
    });
    out["active_leases"] = json!(leases);
    Some(out)
}

pub(super) async fn get_tenant(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let t = fleet.tenancy.lock().await;
    tenant_view(&t, &id)
        .map(Json)
        .ok_or_else(|| not_found(format!("no tenant '{id}'")))
}

/// `GET /fleet/tenants/me`: the calling tenant's own record and use.
pub(super) async fn tenant_me(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Value>, AppError> {
    let Caller::Tenant(id) = &caller else {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "the operator token is not a tenant; use GET /fleet/tenants".into(),
        ));
    };
    let t = fleet.tenancy.lock().await;
    tenant_view(&t, id)
        .map(Json)
        .ok_or_else(|| not_found(format!("no tenant '{id}'")))
}

#[derive(Debug, Deserialize)]
pub(super) struct PatchTenantBody {
    #[serde(default)]
    quota: Option<Quota>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    display_name: Option<String>,
}

pub(super) async fn update_tenant(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(b): Json<PatchTenantBody>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let mut t = fleet.tenancy.lock().await;
    let tenant = t
        .update_tenant(&id, b.quota, b.disabled, b.display_name)
        .map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    Ok(Json(tenant.public_json()))
}

pub(super) async fn delete_tenant(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    require_admin(&caller)?;
    let mut t = fleet.tenancy.lock().await;
    t.delete_tenant(&id).map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn rotate_tenant_token(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let mut t = fleet.tenancy.lock().await;
    let token = t.rotate_token(&id).map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    Ok(Json(json!({"tenant": id, "token": token})))
}

// ---- leases -------------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(super) struct CreateLeaseBody {
    /// Operator only: the tenant to lease for (a tenant always leases for itself).
    #[serde(default)]
    tenant: Option<String>,
    /// A specific node; otherwise the smallest eligible Mac is picked.
    #[serde(default)]
    node: Option<String>,
    #[serde(default, rename = "nodeSelector")]
    node_selector: HashMap<String, String>,
    /// Lease length; omitted means until released. At least the policy minimum (24 h).
    #[serde(default)]
    hours: Option<i64>,
    #[serde(default)]
    min_vcpus: u32,
    #[serde(default)]
    min_memory_mib: u64,
    #[serde(default)]
    nested_virtualization: bool,
    /// Operator only: allow a node without Apple capabilities (a Linux host).
    #[serde(default)]
    any_node: bool,
    /// Operator only: lease a node that still runs VMs. Off by default, because a lease is sole use.
    #[serde(default)]
    allow_busy: bool,
}

/// Why `n` cannot be leased for `b`, or `None` when it can.
fn lease_ineligibility(
    n: &NodeInfo,
    t: &TenancyState,
    b: &CreateLeaseBody,
    admin: bool,
) -> Option<String> {
    if !n.healthy() {
        return Some("unhealthy (stale heartbeat)".into());
    }
    if n.cordoned || t.draining.contains(&n.name) {
        return Some("cordoned or draining".into());
    }
    if let Err(e) = t.leasable(&n.name) {
        return Some(e.to_string());
    }
    if !n.matches_selector(&b.node_selector) {
        return Some(format!(
            "labels do not match nodeSelector {}",
            format_selector(&b.node_selector)
        ));
    }
    let apple = n.apple.as_ref();
    if apple.is_none() && !(admin && b.any_node) {
        return Some("not a Mac (no Apple capabilities in its heartbeat)".into());
    }
    if b.nested_virtualization && !apple.is_some_and(|a| a.nested_virtualization) {
        return Some("no nested virtualization (needs M3 or later)".into());
    }
    if n.vcpus_total < b.min_vcpus || n.memory_mib_total < b.min_memory_mib {
        return Some(format!(
            "too small ({} vCPUs, {} MiB)",
            n.vcpus_total, n.memory_mib_total
        ));
    }
    if n.vm_count > 0 && !(admin && b.allow_busy) {
        return Some(format!(
            "still runs {} VM(s); a lease starts on an empty Mac (sole use)",
            n.vm_count
        ));
    }
    None
}

/// The node a lease should take: the named one if eligible, else the smallest eligible node (by memory, then
/// vCPUs, then name), keeping big Macs free for big asks.
fn pick_lease_node(
    nodes: &HashMap<String, NodeInfo>,
    t: &TenancyState,
    b: &CreateLeaseBody,
    admin: bool,
) -> Result<NodeInfo, AppError> {
    if let Some(name) = &b.node {
        let n = nodes
            .get(name)
            .ok_or_else(|| not_found(format!("no registered node named '{name}'")))?;
        return match lease_ineligibility(n, t, b, admin) {
            None => Ok(n.clone()),
            Some(why) => Err(AppError(
                StatusCode::CONFLICT,
                format!("node '{name}' cannot be leased: {why}"),
            )),
        };
    }
    nodes
        .values()
        .filter(|n| lease_ineligibility(n, t, b, admin).is_none())
        .min_by(|a, b| {
            a.memory_mib_total
                .cmp(&b.memory_mib_total)
                .then(a.vcpus_total.cmp(&b.vcpus_total))
                .then(a.name.cmp(&b.name))
        })
        .cloned()
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "no free Mac can be leased right now (healthy, uncordoned, not leased, scrubbed, empty and large enough)"
                    .into(),
            )
        })
}

/// `POST /fleet/leases` (*tenant*): lease a whole Mac for at least 24 hours.
pub(super) async fn create_lease(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(b): Json<CreateLeaseBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let admin = caller == Caller::Admin;
    let tenant = match &caller {
        Caller::Tenant(t) => t.clone(),
        Caller::Admin => b.tenant.clone().ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                "an operator lease needs \"tenant\"".into(),
            )
        })?,
    };
    // End leases past their `ends_at` first so their Macs show up as released, and evict those tenants' VMs exactly as
    // the reconciler would (the reconciler only evicts for leases it ends itself).
    let ended = {
        let mut t = fleet.tenancy.lock().await;
        let ended = t.expire_leases(Utc::now());
        if !ended.is_empty() {
            fleet.persist_tenancy(&t);
        }
        ended
    };
    for l in &ended {
        tracing::info!(lease = %l.id, tenant = %l.tenant, node = %l.node, "lease expired");
        evict_tenant_vms(&fleet, &l.tenant, &l.node).await;
    }
    let mut t = fleet.tenancy.lock().await;
    let node = {
        let nodes = fleet.nodes.lock().await;
        pick_lease_node(&nodes, &t, &b, admin)?
    };
    let lease = t
        .create_lease(&tenant, &node.name, b.hours, Utc::now())
        .map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    tracing::info!(lease = %lease.id, tenant = %tenant, node = %node.name, "lease created");
    Ok((
        StatusCode::CREATED,
        Json(json!({"lease": lease, "node": node_json(&node)})),
    ))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct LeaseQuery {
    #[serde(default)]
    tenant: Option<String>,
    /// `active` or `released`.
    #[serde(default)]
    state: Option<String>,
}

pub(super) async fn list_leases(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<LeaseQuery>,
) -> Json<Value> {
    let t = fleet.tenancy.lock().await;
    let scope = caller.tenant().map(str::to_owned).or(q.tenant);
    let items: Vec<&Lease> = t
        .leases
        .iter()
        .filter(|l| scope.as_deref().is_none_or(|s| l.tenant == s))
        .filter(|l| match q.state.as_deref() {
            Some("active") => l.state == LeaseState::Active,
            Some("released") => l.state == LeaseState::Released,
            _ => true,
        })
        .collect();
    Json(json!({"items": items}))
}

pub(super) async fn get_lease(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<Json<Lease>, AppError> {
    let t = fleet.tenancy.lock().await;
    t.lease(&id)
        .filter(|l| caller.tenant().is_none_or(|c| l.tenant == c))
        .cloned()
        .map(Json)
        .ok_or_else(|| not_found(format!("no lease '{id}'")))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ForceQuery {
    #[serde(default)]
    force: bool,
}

/// `POST /fleet/leases/{id}/release[?force=true]` (*tenant*): end a lease (not before its 24-hour minimum unless an
/// operator forces it), delete the tenant's VMs on that Mac, and mark the Mac for scrubbing.
pub(super) async fn release_lease(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<ForceQuery>,
) -> Result<Json<Value>, AppError> {
    let lease = {
        let mut t = fleet.tenancy.lock().await;
        let lease = t
            .release_lease(&id, caller.tenant(), q.force, Utc::now())
            .map_err(tenancy_error)?;
        fleet.persist_tenancy(&t);
        lease
    };
    tracing::info!(lease = %lease.id, tenant = %lease.tenant, node = %lease.node, "lease released");
    let evicted = evict_tenant_vms(&fleet, &lease.tenant, &lease.node).await;
    Ok(Json(json!({
        "lease": lease,
        "evicted": evicted,
        "scrub_required": true,
    })))
}

// ---- scrub --------------------------------------------------------------------------------------------------------

/// `POST /fleet/nodes/{name}/scrub[?force=true]`: make a node released from a lease usable again. Refuses while the
/// node still lists any VM, unless `force` deletes them all first.
///
/// What central can guarantee is "no VM and no VM workspace left": a node deletes a VM's workspace (disks, EFI and
/// auxiliary storage, sockets) with it. Host-level wipes beyond that (login Keychain items, caches, a reinstall) are the
/// operator's runbook; do them before calling this.
pub(super) async fn scrub_node(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(q): Query<ForceQuery>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let node = node_by_name(&fleet, &name).await?;
    if let Some(l) = fleet.tenancy.lock().await.active_lease_for_node(&name) {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!(
                "node '{name}' is leased to '{}' (lease {}); release the lease first",
                l.tenant, l.id
            ),
        ));
    }
    let vms = fetch_node_vms(&fleet, &node).await?;
    if !vms.is_empty() && !q.force {
        let ids: Vec<String> = vms.iter().map(vm_id).collect();
        return Err(AppError(
            StatusCode::CONFLICT,
            format!(
                "node '{name}' still runs {} VM(s) ({}); delete them or pass ?force=true",
                ids.len(),
                ids.join(", ")
            ),
        ));
    }
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for vm in &vms {
        let id = vm_id(vm);
        match delete_vm_on_node(&fleet, &name, &id).await {
            Ok(_) => deleted.push(id),
            Err(e) => failed.push(json!({"vm": id, "error": e.1})),
        }
    }
    let mut t = fleet.tenancy.lock().await;
    for id in &deleted {
        t.end_vm(&name, id, Utc::now());
    }
    if !failed.is_empty() {
        fleet.persist_tenancy(&t);
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!(
                "node '{name}' not scrubbed; some deletes failed: {}",
                Value::Array(failed)
            ),
        ));
    }
    let was_pending = t.mark_scrubbed(&name);
    fleet.persist_tenancy(&t);
    Ok(Json(json!({
        "node": name,
        "deleted": deleted,
        "was_pending": was_pending,
        "scrubbed": true,
    })))
}

// ---- drain --------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainMode {
    /// Cordon and report what is left; touch nothing.
    Cordon,
    /// Also delete ephemeral VMs (a TTL, or the `fluxvm.dev/ephemeral=true` label).
    EvictEphemeral,
    /// Also recreate reschedulable VMs elsewhere (then delete the old one), and evict ephemeral ones.
    Reschedule,
}

impl DrainMode {
    fn parse(s: Option<&str>) -> Result<Self, AppError> {
        match s.unwrap_or("cordon") {
            "cordon" => Ok(DrainMode::Cordon),
            "evict-ephemeral" => Ok(DrainMode::EvictEphemeral),
            "reschedule" => Ok(DrainMode::Reschedule),
            other => Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("unknown drain mode '{other}' (cordon, evict-ephemeral, reschedule)"),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            DrainMode::Cordon => "cordon",
            DrainMode::EvictEphemeral => "evict-ephemeral",
            DrainMode::Reschedule => "reschedule",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct DrainQuery {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    force: bool,
}

/// A VM that can simply be deleted on drain: it has a TTL, or carries `fluxvm.dev/ephemeral=true`.
fn is_ephemeral(vm: &Value) -> bool {
    let req = vm.get("request");
    let ttl = req
        .and_then(|r| r.get("ttl_seconds"))
        .is_some_and(|v| v.as_u64().is_some());
    let label = |v: Option<&Value>| {
        v.and_then(|l| l.get("fluxvm.dev/ephemeral"))
            .and_then(Value::as_str)
            == Some("true")
    };
    ttl || label(vm.get("labels")) || label(req.and_then(|r| r.get("labels")))
}

/// `POST /fleet/nodes/{name}/drain?mode=cordon|evict-ephemeral|reschedule[&force=true]`.
///
/// Cordons the node and marks it draining, then handles each VM on it by `mode`. `Virtualization.framework` has no
/// live migration, so "reschedule" is recreate-elsewhere: a VM created with `"reschedulable": true` is created again
/// from its stored spec on another node (disk state is not carried) and the old one deleted. Anything else is left
/// running and listed as `kept` for the operator. A leased node needs `force`, because draining it breaks the
/// tenant's sole use.
pub(super) async fn drain_node(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Query(q): Query<DrainQuery>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let mode = DrainMode::parse(q.mode.as_deref())?;
    let node = node_by_name(&fleet, &name).await?;
    {
        let mut t = fleet.tenancy.lock().await;
        if let Some(l) = t.active_lease_for_node(&name)
            && !q.force
        {
            return Err(AppError(
                StatusCode::CONFLICT,
                format!(
                    "node '{name}' is leased to '{}' until at least {}; pass force=true to drain anyway",
                    l.tenant, l.min_end
                ),
            ));
        }
        t.draining.insert(name.clone());
        fleet.persist_tenancy(&t);
    }
    {
        let mut nodes = fleet.nodes.lock().await;
        set_cordoned(&mut nodes, &name, true);
        if let (Some(path), Some(lock)) = (&fleet.persist_path, &fleet.lock_path) {
            persist_nodes(path, lock, &nodes);
        }
    }
    let vms = fetch_node_vms(&fleet, &node).await?;
    let mut actions = Vec::new();
    let mut remaining = 0usize;
    for vm in &vms {
        let id = vm_id(vm);
        let entry = fleet.tenancy.lock().await.vm(&name, &id).cloned();
        if mode == DrainMode::Reschedule
            && let Some(spec) = entry
                .as_ref()
                .filter(|e| e.active())
                .and_then(|e| e.spec.clone())
        {
            let owner = match entry.as_ref().and_then(|e| e.tenant.clone()) {
                Some(t) => Caller::Tenant(t),
                None => Caller::Admin,
            };
            let mut body = spec;
            body["reschedulable"] = json!(true);
            match create_vm_inner(&fleet, &owner, body, true).await {
                Ok(Json(created)) => {
                    let del = delete_vm_on_node(&fleet, &name, &id).await;
                    if del.is_ok() {
                        let mut t = fleet.tenancy.lock().await;
                        t.end_vm(&name, &id, Utc::now());
                        fleet.persist_tenancy(&t);
                    } else {
                        remaining += 1;
                    }
                    actions.push(json!({
                        "vm": id,
                        "action": "rescheduled",
                        "to_node": created.get("node"),
                        "new_vm": created.get("vm").and_then(|v| v.get("id")),
                        "old_deleted": del.is_ok(),
                    }));
                }
                Err(e) => {
                    remaining += 1;
                    actions.push(json!({"vm": id, "action": "failed", "error": e.1}));
                }
            }
            continue;
        }
        if mode != DrainMode::Cordon && is_ephemeral(vm) {
            match delete_vm_on_node(&fleet, &name, &id).await {
                Ok(_) => {
                    let mut t = fleet.tenancy.lock().await;
                    t.end_vm(&name, &id, Utc::now());
                    fleet.persist_tenancy(&t);
                    actions.push(json!({"vm": id, "action": "evicted"}));
                }
                Err(e) => {
                    remaining += 1;
                    actions.push(json!({"vm": id, "action": "failed", "error": e.1}));
                }
            }
            continue;
        }
        remaining += 1;
        actions.push(json!({"vm": id, "action": "kept"}));
    }
    tracing::info!(node = %name, mode = mode.as_str(), remaining, "node drained");
    Ok(Json(json!({
        "node": name,
        "mode": mode.as_str(),
        "cordoned": true,
        "draining": true,
        "actions": actions,
        "remaining": remaining,
        "done": remaining == 0,
    })))
}

/// `GET /fleet/nodes/{name}/drain`: drain state from the registry (no node call).
pub(super) async fn drain_status(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let t = fleet.tenancy.lock().await;
    let node = node_by_name(&fleet, &name).await?;
    Ok(Json(json!({
        "node": name,
        "draining": t.draining.contains(&name),
        "cordoned": node.cordoned,
        "vm_count": node.vm_count,
        "done": node.vm_count == 0,
        "lease": t.active_lease_for_node(&name),
        "scrub_pending": t.scrub_pending.contains(&name),
    })))
}

// ---- exec ---------------------------------------------------------------------------------------------------------

/// `POST /fleet/vms/{node}/{id}/exec` (*tenant*): forwards `{"command": "...", "timeout_seconds": N}` to the node's
/// guest agent (`POST /v1/vms/{id}/agent`). A tenant can exec only in its own VMs.
pub(super) async fn exec_vm(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    if !fleet.tenancy.lock().await.owns(&caller, &node, &id) {
        return Err(not_found(format!("no VM '{id}' on node '{node}'")));
    }
    let target = node_by_name(&fleet, &node).await?;
    let resp = fleet
        .http
        .post(format!("{}/v1/vms/{id}/agent", target.fluxvm_url))
        .json(&body)
        .send()
        .await?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let out: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        Ok((status, Json(out)))
    } else {
        Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{node}' rejected exec ({status}): {out}"),
        ))
    }
}

// ---- usage --------------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(super) struct UsageQuery {
    /// RFC 3339; default 30 days before `to`.
    #[serde(default)]
    from: Option<String>,
    /// RFC 3339; default now.
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    tenant: Option<String>,
    /// `json` (default) or `csv`.
    #[serde(default)]
    format: Option<String>,
}

fn parse_time(s: &str, what: &str) -> Result<DateTime<Utc>, AppError> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, format!("bad {what} '{s}': {e}")))
}

/// `GET /fleet/usage?from=&to=&tenant=&format=json|csv` (*tenant*: always its own). VM-, vCPU- and memory-seconds
/// from the ledger and lease time (actual and billable, with the 24-hour minimum) per tenant.
pub(super) async fn usage(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<UsageQuery>,
) -> Result<Response, AppError> {
    let now = Utc::now();
    let to = match &q.to {
        Some(s) => parse_time(s, "to")?,
        None => now,
    };
    let from = match &q.from {
        Some(s) => parse_time(s, "from")?,
        None => to - ChronoDuration::days(30),
    };
    if from >= to {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "from must be before to".into(),
        ));
    }
    let only = caller.tenant().map(str::to_owned).or(q.tenant);
    let report = fleet
        .tenancy
        .lock()
        .await
        .usage(from, to, only.as_deref(), now);
    Ok(match q.format.as_deref() {
        Some("csv") => (
            [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
            report.to_csv(),
        )
            .into_response(),
        None | Some("json") => Json(report).into_response(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("unknown format '{other}' (json, csv)"),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::TenancyError;
    use axum::body::Body;
    use tower::ServiceExt;

    fn mac(name: &str, mem: u64, vms: usize) -> NodeInfo {
        NodeInfo {
            name: name.into(),
            fluxvm_url: "http://127.0.0.1:1".into(),
            vcpus_total: 10,
            memory_mib_total: mem,
            vm_count: vms,
            last_seen: Utc::now(),
            cordoned: false,
            labels: HashMap::new(),
            security: Default::default(),
            density: None,
            apple: Some(AppleHostCaps {
                free_cpu: 10,
                free_memory_mib: mem,
                nested_virtualization: true,
                ..Default::default()
            }),
        }
    }

    fn fleet_of(nodes: Vec<NodeInfo>, tenancy: TenancyState) -> Fleet {
        Fleet {
            nodes: Arc::new(Mutex::new(
                nodes.into_iter().map(|n| (n.name.clone(), n)).collect(),
            )),
            http: reqwest::Client::new(),
            persist_path: None,
            lock_path: None,
            token: Some("admin-token".into()),
            tenancy: Arc::new(Mutex::new(tenancy)),
            tenancy_path: None,
        }
    }

    fn app(fleet: Fleet) -> Router {
        Router::new()
            .route("/fleet/tenants", post(create_tenant))
            .route("/fleet/tenants/me", get(tenant_me))
            .route("/fleet/leases", get(list_leases).post(create_lease))
            .route("/fleet/leases/{id}/release", post(release_lease))
            .route("/fleet/nodes/{name}/drain", get(drain_status))
            .route("/fleet/usage", get(usage))
            .layer(middleware::from_fn_with_state(
                fleet.clone(),
                auth_middleware,
            ))
            .with_state(fleet)
    }

    async fn call(app: &Router, method: &str, uri: &str, token: &str, body: Value) -> (u16, Value) {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    #[test]
    fn lease_picks_the_smallest_empty_mac() {
        let mut nodes = HashMap::new();
        for n in [
            mac("big", 128_000, 0),
            mac("small", 32_000, 0),
            mac("busy", 16_000, 1),
        ] {
            nodes.insert(n.name.clone(), n);
        }
        let t = TenancyState::default();
        let b = CreateLeaseBody::default();
        assert_eq!(
            pick_lease_node(&nodes, &t, &b, false).unwrap().name,
            "small"
        );
        let b = CreateLeaseBody {
            min_memory_mib: 64_000,
            ..Default::default()
        };
        assert_eq!(pick_lease_node(&nodes, &t, &b, false).unwrap().name, "big");
        let b = CreateLeaseBody {
            node: Some("busy".into()),
            ..Default::default()
        };
        let err = pick_lease_node(&nodes, &t, &b, false).unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        assert!(err.1.contains("sole use"));
        let b = CreateLeaseBody {
            node: Some("busy".into()),
            allow_busy: true,
            ..Default::default()
        };
        // Only an operator may lease a busy Mac.
        assert!(pick_lease_node(&nodes, &t, &b, false).is_err());
        assert!(pick_lease_node(&nodes, &t, &b, true).is_ok());
    }

    #[test]
    fn linux_nodes_are_not_leased_unless_the_operator_asks() {
        let mut linux = mac("linux", 64_000, 0);
        linux.apple = None;
        let nodes: HashMap<String, NodeInfo> = [("linux".to_string(), linux)].into();
        let t = TenancyState::default();
        assert!(pick_lease_node(&nodes, &t, &CreateLeaseBody::default(), false).is_err());
        let b = CreateLeaseBody {
            any_node: true,
            ..Default::default()
        };
        assert!(pick_lease_node(&nodes, &t, &b, false).is_err());
        assert!(pick_lease_node(&nodes, &t, &b, true).is_ok());
    }

    #[test]
    fn ephemeral_vms_are_recognised() {
        assert!(is_ephemeral(&json!({"request": {"ttl_seconds": 600}})));
        assert!(is_ephemeral(
            &json!({"labels": {"fluxvm.dev/ephemeral": "true"}})
        ));
        assert!(!is_ephemeral(&json!({"request": {"ttl_seconds": null}})));
        assert!(!is_ephemeral(
            &json!({"labels": {"fluxvm.dev/ephemeral": "false"}})
        ));
    }

    #[test]
    fn tenant_routes_are_limited() {
        use axum::http::Method;
        assert!(tenant_route_allowed(&Method::POST, "/fleet/vms"));
        assert!(tenant_route_allowed(&Method::DELETE, "/fleet/vms/mac1/abc"));
        assert!(tenant_route_allowed(
            &Method::POST,
            "/fleet/leases/l1/release"
        ));
        assert!(tenant_route_allowed(&Method::GET, "/fleet/tenants/me"));
        assert!(!tenant_route_allowed(&Method::POST, "/fleet/register"));
        assert!(!tenant_route_allowed(
            &Method::POST,
            "/fleet/nodes/mac1/cordon"
        ));
        assert!(!tenant_route_allowed(&Method::GET, "/fleet/tenants"));
        assert!(!tenant_route_allowed(&Method::GET, "/fleet/nodes"));
        assert!(!tenant_route_allowed(&Method::POST, "/fleet/reconcile"));
    }

    #[tokio::test]
    async fn tenant_lifecycle_over_http() {
        let fleet = fleet_of(
            vec![mac("mac-a", 32_000, 0), mac("mac-b", 64_000, 0)],
            TenancyState::default(),
        );
        let app = app(fleet.clone());

        // No token, a bad token: 401.
        let (s, _) = call(&app, "GET", "/fleet/leases", "wrong", json!({})).await;
        assert_eq!(s, 401);

        // Operator creates a tenant; the token comes back once.
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/tenants",
            "admin-token",
            json!({"id": "acme", "quota": {"max_leased_hosts": 1}}),
        )
        .await;
        assert_eq!(s, 201, "{v}");
        let token = v["token"].as_str().unwrap().to_owned();

        // The tenant cannot do operator things.
        let (s, _) = call(&app, "POST", "/fleet/tenants", &token, json!({"id": "x"})).await;
        assert_eq!(s, 403);

        // A lease shorter than 24 h is refused; a default one lands on the smallest Mac.
        let (s, v) = call(&app, "POST", "/fleet/leases", &token, json!({"hours": 4})).await;
        assert_eq!(s, 400, "{v}");
        let (s, v) = call(&app, "POST", "/fleet/leases", &token, json!({})).await;
        assert_eq!(s, 201, "{v}");
        assert_eq!(v["lease"]["node"], "mac-a");
        assert_eq!(v["lease"]["tenant"], "acme");
        let lease_id = v["lease"]["id"].as_str().unwrap().to_owned();

        // Quota: one leased host.
        let (s, v) = call(&app, "POST", "/fleet/leases", &token, json!({})).await;
        assert_eq!(s, 429, "{v}");

        // Early release is refused for the tenant.
        let (s, v) = call(
            &app,
            "POST",
            &format!("/fleet/leases/{lease_id}/release"),
            &token,
            json!({}),
        )
        .await;
        assert_eq!(s, 409, "{v}");

        // `me` shows the lease.
        let (s, v) = call(&app, "GET", "/fleet/tenants/me", &token, json!({})).await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["in_use"]["leased_hosts"], 1);

        // Operator force-releases; the Mac now needs a scrub.
        let (s, v) = call(
            &app,
            "POST",
            &format!("/fleet/leases/{lease_id}/release?force=true"),
            "admin-token",
            json!({}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["scrub_required"], true);
        let (s, v) = call(
            &app,
            "GET",
            "/fleet/nodes/mac-a/drain",
            "admin-token",
            json!({}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["scrub_pending"], true);

        // Usage: the tenant sees only itself, billed to the 24-hour minimum.
        let to = (Utc::now() + ChronoDuration::days(2))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let (s, v) = call(
            &app,
            "GET",
            &format!("/fleet/usage?to={to}"),
            &token,
            json!({}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let tenants = v["tenants"].as_array().unwrap();
        assert_eq!(tenants.len(), 1);
        assert_eq!(tenants[0]["tenant"], "acme");
        assert!(tenants[0]["billable_lease_seconds"].as_i64().unwrap() >= 24 * 3600 - 5);

        // Disabled tenants are refused at the door.
        fleet
            .tenancy
            .lock()
            .await
            .update_tenant("acme", None, Some(true), None)
            .unwrap();
        let (s, _) = call(&app, "GET", "/fleet/tenants/me", &token, json!({})).await;
        assert_eq!(s, 403);
    }

    #[tokio::test]
    async fn tenant_macos_vms_stay_on_their_lease() {
        // Two Macs, both fake nodes that accept creates.
        let (url, hits) = super::super::tests::spawn_fake_create_node_pub(
            StatusCode::OK,
            json!({"id": "vm-1", "name": "ci"}),
        )
        .await;
        let mut a = mac("mac-a", 32_000, 0);
        a.fluxvm_url = url.clone();
        let mut b = mac("mac-b", 64_000, 0);
        b.fluxvm_url = url;
        let mut tenancy = TenancyState::default();
        tenancy
            .create_tenant("acme", None, Quota::default(), Utc::now())
            .unwrap();
        tenancy
            .create_tenant("other", None, Quota::default(), Utc::now())
            .unwrap();
        tenancy
            .create_lease("other", "mac-b", None, Utc::now())
            .unwrap();
        let fleet = fleet_of(vec![a, b], tenancy);
        let acme = Caller::Tenant("acme".into());
        let macos = json!({"backend": "vz", "name": "ci", "apple": {"guest_os": "macos"}});

        // No lease: refused with a hint, and no node was called.
        let err = create_vm_inner(&fleet, &acme, macos.clone(), false)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.contains("POST /fleet/leases"), "{}", err.1);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Lease mac-a: the VM lands there (never on other's mac-b), stamped with the tenant, in the ledger.
        fleet
            .tenancy
            .lock()
            .await
            .create_lease("acme", "mac-a", None, Utc::now())
            .unwrap();
        let Json(v) = create_vm_inner(&fleet, &acme, macos.clone(), false)
            .await
            .unwrap();
        assert_eq!(v["node"], "mac-a");
        let t = fleet.tenancy.lock().await;
        let e = t.vm("mac-a", "vm-1").unwrap();
        assert_eq!(e.tenant.as_deref(), Some("acme"));
        assert!(e.macos_guest);
        assert!(e.spec.is_none());
        drop(t);

        // Naming other's Mac is refused for a tenant.
        let mut pinned = macos.clone();
        pinned["node"] = json!("mac-b");
        let err = create_vm_inner(&fleet, &acme, pinned, false)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn quotas_apply_to_fleet_creates() {
        let (url, _) = super::super::tests::spawn_fake_create_node_pub(
            StatusCode::OK,
            json!({"id": "vm-1", "name": "x"}),
        )
        .await;
        let mut n = mac("linux-1", 64_000, 0);
        n.apple = None;
        n.fluxvm_url = url;
        let mut tenancy = TenancyState::default();
        tenancy
            .create_tenant(
                "acme",
                None,
                Quota {
                    max_vms: Some(1),
                    ..Default::default()
                },
                Utc::now(),
            )
            .unwrap();
        let fleet = fleet_of(vec![n], tenancy);
        let acme = Caller::Tenant("acme".into());
        let body = json!({"backend": "qemu", "name": "x", "reschedulable": true});
        let Json(v) = create_vm_inner(&fleet, &acme, body.clone(), false)
            .await
            .unwrap();
        assert_eq!(v["node"], "linux-1");
        // `reschedulable` kept the spec (without the routing field itself).
        let spec = fleet
            .tenancy
            .lock()
            .await
            .vm("linux-1", "vm-1")
            .unwrap()
            .spec
            .clone()
            .unwrap();
        assert_eq!(spec["tenant"], "acme");
        assert!(spec.get("reschedulable").is_none());
        let err = create_vm_inner(&fleet, &acme, body, false)
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn tenancy_errors_map_to_http() {
        assert_eq!(
            tenancy_error(TenancyError::Quota("x".into())).0,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            tenancy_error(TenancyError::UnknownLease("x".into())).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            tenancy_error(TenancyError::EarlyRelease {
                earliest: Utc::now()
            })
            .0,
            StatusCode::CONFLICT
        );
    }
}
