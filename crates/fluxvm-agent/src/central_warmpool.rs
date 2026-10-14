// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Warm pools in `fluxvm-agent central`: the `/fleet/pools` routes, claiming a warm VM for a launch, and the task that
//! keeps every pool at `min_ready`. The rules are in [`crate::warmpool`].
//!
//! | Route | Who | What |
//! |---|---|---|
//! | `GET /fleet/pools` | operator, tenant | the caller's pools; a tenant also sees shared pools it may claim from |
//! | `POST /fleet/pools` | operator, tenant | create; a tenant's pool is its own, the operator's is shared unless `tenant` is set |
//! | `GET /fleet/pools/{name}`, `GET …/{name}/status` | as above | one pool, or just its status |
//! | `PATCH /fleet/pools/{name}` | owner | `min_ready`, `max_total`, `node_selector`, `nodes` |
//! | `DELETE /fleet/pools/{name}` | owner | remove it and delete its warm (unclaimed) VMs; claimed VMs are not touched |
//!
//! Claiming happens inside `POST /fleet/vms` ([`claim_or_pass`]): the pick and the hand-over are one step under the
//! tenancy lock, so two launches never get the same warm VM.

use super::*;
use crate::tenancy::VmEntry;
use crate::warmpool::{self as wp, Pool, PoolPatch, PoolSpec, WarmState};
use std::collections::BTreeSet;
use std::time::{Duration as StdDuration, Instant};

/// Longest wait for a node during a reconcile pass or a claim.
const NODE_TIMEOUT: StdDuration = StdDuration::from_secs(15);

/// One reconcile pass at a time (the timer, and the passes a claim or a pool change starts).
static PASS: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// How often pools are topped up: `FLUXVM_WARM_POOL_INTERVAL_SECS` (1-3600), default 5 seconds.
pub(super) fn interval() -> StdDuration {
    let secs = std::env::var("FLUXVM_WARM_POOL_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(5, |s| s.clamp(1, 3600));
    StdDuration::from_secs(secs)
}

/// Runs [`reconcile_pools`] every `every` while this central leads.
pub(super) fn spawn_reconciler(fleet: Fleet, every: StdDuration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if !fleet.leading() {
                continue;
            }
            let report = reconcile_pools(&fleet).await;
            if report.as_object().is_some_and(|o| !o.is_empty()) {
                tracing::debug!(%report, "warm pools reconciled");
            }
        }
    });
}

/// Starts a pass now (after a claim or a pool change), without waiting for it.
fn kick(fleet: &Fleet) {
    // Only the leading central changes pools (a follower's passes would fight the leader's).
    if !fleet.leading() || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let fleet = fleet.clone();
    tokio::spawn(async move {
        reconcile_pools(&fleet).await;
    });
}

// ---- access -------------------------------------------------------------------------------------------------------

fn manages(caller: &Caller, pool: &Pool) -> bool {
    match caller {
        Caller::Admin => true,
        Caller::Tenant(t) => pool.tenant.as_deref() == Some(t.as_str()),
    }
}

fn is_macos(t: &TenancyState, pool: &Pool) -> bool {
    t.images
        .get(&pool.image)
        .is_some_and(|i| i.guest_os == "macos")
}

/// Why a tenant may not take VMs from shared pools of this guest, or `None` when it may.
fn shared_refusal(t: &TenancyState, macos: bool) -> Option<&'static str> {
    if !t.enabled() {
        None
    } else if !t.policy.allow_shared_pool_claims {
        Some(
            "shared pools are off in this cloud's policy (their VMs share a network and the image's credentials between tenants): use a pool of your own",
        )
    } else if t.policy.isolate_tenant_networks {
        Some(
            "tenant networks are isolated, and a shared pool's VMs were booted on no tenant's network: use a pool of your own",
        )
    } else if macos && t.policy.require_lease_for_macos {
        Some(
            "macOS VMs run only on Macs you lease, and shared pools run on Macs nobody leases: use a pool of your own",
        )
    } else {
        None
    }
}

/// Whether `caller` sees the pool: its owner, or a tenant that may claim from this shared pool.
fn sees(t: &TenancyState, caller: &Caller, pool: &Pool) -> bool {
    manages(caller, pool)
        || (pool.tenant.is_none() && shared_refusal(t, is_macos(t, pool)).is_none())
}

fn no_pool(name: &str) -> AppError {
    AppError(StatusCode::NOT_FOUND, format!("no warm pool '{name}'"))
}

fn bad(msg: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.into())
}

/// Why `node` may not hold this pool's warm VMs (leases, drain, scrub, the pool's node choice), or `None`.
fn placement_refusal(
    t: &TenancyState,
    pool: &Pool,
    node: &str,
    labels: &HashMap<String, String>,
    macos: bool,
) -> Option<String> {
    if !pool.allows_node(node, labels) {
        return Some(format!(
            "{node} is not one of the pool's nodes or does not match its node selector"
        ));
    }
    if t.draining.contains(node) {
        return Some(format!("{node} is being drained"));
    }
    let lease = t.active_lease_for_node(node);
    match &pool.tenant {
        Some(tenant) => {
            if lease.is_none_or(|l| l.tenant != *tenant) {
                return Some(format!("{tenant} does not lease {node}"));
            }
            t.admit(node, &Caller::Tenant(tenant.clone()), macos, false)
                .err()
                .map(|e| e.to_string())
        }
        None => {
            if let Some(l) = lease {
                return Some(format!(
                    "{node} is leased to {}, and shared pools run only on Macs nobody leases",
                    l.tenant
                ));
            }
            t.admit(node, &Caller::Admin, macos, false)
                .err()
                .map(|e| e.to_string())
        }
    }
}

/// macOS guests on `node` by the ledger (a heartbeat can lag behind the VMs central just created).
fn macos_guests_on(t: &TenancyState, node: &str) -> usize {
    t.vms
        .iter()
        .filter(|v| v.active() && v.node == node && v.macos_guest)
        .count()
}

// ---- routes -------------------------------------------------------------------------------------------------------

fn pool_json(t: &TenancyState, caller: &Caller, pool: &Pool) -> Value {
    wp::view(
        pool,
        t.images.get(&pool.image),
        &t.vms,
        Utc::now(),
        manages(caller, pool),
    )
}

/// `GET /fleet/pools`.
pub(super) async fn list_pools(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let t = fleet.tenancy.lock().await;
    let items: Vec<Value> = t
        .pools
        .values()
        .filter(|p| sees(&t, &caller, p))
        .map(|p| pool_json(&t, &caller, p))
        .collect();
    Json(json!({ "items": items }))
}

/// `POST /fleet/pools`.
pub(super) async fn create_pool(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(spec): Json<PoolSpec>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let owner = match &caller {
        Caller::Tenant(t) => {
            if spec.tenant.as_ref().is_some_and(|x| x != t) {
                return Err(AppError(
                    StatusCode::FORBIDDEN,
                    "a tenant creates pools only for itself".into(),
                ));
            }
            Some(t.clone())
        }
        Caller::Admin => spec.tenant.clone().filter(|s| !s.is_empty()),
    };
    let name = spec.name.clone();
    let out = {
        let mut t = fleet.tenancy.lock().await;
        if let Some(o) = &owner
            && !t.tenants.contains_key(o)
        {
            return Err(bad(format!("no tenant '{o}'")));
        }
        if t.pools.contains_key(&name) {
            return Err(AppError(
                StatusCode::CONFLICT,
                format!("a warm pool named '{name}' already exists"),
            ));
        }
        if t.pools.len() >= wp::MAX_POOLS {
            return Err(AppError(
                StatusCode::CONFLICT,
                format!(
                    "this fleet already has {} warm pools, the most it allows",
                    wp::MAX_POOLS
                ),
            ));
        }
        if let Some(o) = &owner
            && t.pools
                .values()
                .filter(|p| p.tenant.as_deref() == Some(o.as_str()))
                .count()
                >= wp::MAX_POOLS_PER_TENANT
        {
            return Err(AppError(
                StatusCode::CONFLICT,
                format!(
                    "tenant '{o}' already has {} warm pools, the most a tenant may have",
                    wp::MAX_POOLS_PER_TENANT
                ),
            ));
        }
        let img = t.images.get(&spec.image).cloned();
        let pool = Pool::from_spec(spec, owner.clone(), img.as_ref(), Utc::now()).map_err(bad)?;
        if let Caller::Tenant(me) = &caller
            && let Some(n) = pool
                .nodes
                .iter()
                .find(|n| t.active_lease_for_node(n).is_none_or(|l| l.tenant != *me))
        {
            return Err(bad(format!("you do not lease a Mac named '{n}'")));
        }
        let mut out = pool_json(&t, &caller, &pool);
        let macos = img.as_ref().is_some_and(|i| i.guest_os == "macos");
        let note = match &owner {
            None => shared_refusal(&t, macos).map(|why| {
                format!("Tenants cannot claim from this shared pool: {why}. The operator's own launches still can.")
            }),
            Some(o) if !t.leases.iter().any(|l| {
                l.tenant == *o && l.state == crate::tenancy::LeaseState::Active
            }) =>
            {
                Some(format!(
                    "{o} leases no Mac yet; the pool fills once it does (tenant pools run on the tenant's leased Macs)"
                ))
            }
            Some(_) => None,
        };
        if let Some(n) = note {
            out["note"] = json!(n);
        }
        t.pools.insert(name, pool);
        fleet.persist_tenancy(&t);
        out
    };
    kick(&fleet);
    Ok((StatusCode::CREATED, Json(out)))
}

/// `GET /fleet/pools/{name}`.
pub(super) async fn get_pool(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let t = fleet.tenancy.lock().await;
    let p = t
        .pools
        .get(&name)
        .filter(|p| sees(&t, &caller, p))
        .ok_or_else(|| no_pool(&name))?;
    Ok(Json(pool_json(&t, &caller, p)))
}

/// `GET /fleet/pools/{name}/status`: ready and booting VMs, claims in the last 24 hours and their average time.
pub(super) async fn pool_status(
    state: State<Fleet>,
    caller: Extension<Caller>,
    name: Path<String>,
) -> Result<Json<Value>, AppError> {
    let Json(v) = get_pool(state, caller, name).await?;
    let mut status = v.get("status").cloned().unwrap_or_default();
    status["name"] = v["name"].clone();
    if let Some(w) = v.get("warm") {
        status["warm"] = w.clone();
    }
    Ok(Json(status))
}

/// `PATCH /fleet/pools/{name}`.
pub(super) async fn patch_pool(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Json(patch): Json<PoolPatch>,
) -> Result<Json<Value>, AppError> {
    let out = {
        let mut t = fleet.tenancy.lock().await;
        let mut pool = t
            .pools
            .get(&name)
            .filter(|p| manages(&caller, p))
            .cloned()
            .ok_or_else(|| no_pool(&name))?;
        pool.apply(patch).map_err(bad)?;
        if let Caller::Tenant(me) = &caller
            && let Some(n) = pool
                .nodes
                .iter()
                .find(|n| t.active_lease_for_node(n).is_none_or(|l| l.tenant != *me))
        {
            return Err(bad(format!("you do not lease a Mac named '{n}'")));
        }
        let out = pool_json(&t, &caller, &pool);
        t.pools.insert(name, pool);
        fleet.persist_tenancy(&t);
        out
    };
    kick(&fleet);
    Ok(Json(out))
}

/// `DELETE /fleet/pools/{name}`: removes the pool and deletes its warm VMs. VMs already claimed keep running.
pub(super) async fn delete_pool(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let drained: Vec<(String, String, String)> = {
        let mut t = fleet.tenancy.lock().await;
        if !t.pools.get(&name).is_some_and(|p| manages(&caller, p)) {
            return Err(no_pool(&name));
        }
        t.pools.remove(&name);
        let now = Utc::now();
        let mut out = Vec::new();
        for v in t.vms.iter_mut() {
            if v.active() && v.claimed_at.is_none() && v.warm_pool.as_deref() == Some(name.as_str())
            {
                v.ended_at = Some(now);
                out.push((v.node.clone(), v.vm_id.clone(), v.name.clone()));
            }
        }
        fleet.persist_tenancy(&t);
        out
    };
    let mut results = Vec::new();
    for (node, id, vm) in drained {
        match delete_vm_on_node(&fleet, &node, &id).await {
            Ok(_) => results.push(json!({"node": node, "id": id, "name": vm, "deleted": true})),
            Err(e) => {
                tracing::warn!(pool = %name, %node, vm = %id, error = %e.1, "could not delete a warm VM");
                results.push(
                    json!({"node": node, "id": id, "name": vm, "deleted": false, "error": e.1}),
                );
            }
        }
    }
    Ok(Json(json!({"deleted": name, "drained": results})))
}

// ---- claiming -----------------------------------------------------------------------------------------------------

/// What `POST /fleet/vms` does after [`claim_or_pass`].
pub(super) enum Launch {
    /// A warm VM was handed over; this is the response.
    Claimed(Value),
    /// Create a VM the usual way, from this body.
    Create(Value),
}

/// A warm VM picked for a launch.
struct Picked {
    pool: String,
    node: String,
    vm_id: String,
    name: String,
    tenant: Option<String>,
    warm_secs: i64,
    at: DateTime<Utc>,
}

/// Answers a launch from a warm pool when it can. A launch is answered from a pool when it names one (`"pool"`), or
/// when its image and size match a pool the caller may use, it asks for nothing a booted VM cannot have (cloud-init,
/// networks, disks), and a warm VM is ready. A launch that names a pool with nothing ready is created from the pool's
/// image and size the usual way. Anything else passes through unchanged.
pub(super) async fn claim_or_pass(
    fleet: &Fleet,
    caller: &Caller,
    mut body: Value,
) -> Result<Launch, AppError> {
    let started = Instant::now();
    if !body.is_object() {
        return Ok(Launch::Create(body));
    }
    let obj = body.as_object_mut().expect("checked above");
    if obj.remove(wp::FILL_MARKER).is_some() {
        obj.remove("pool");
        return Ok(Launch::Create(body));
    }
    let named = match obj.remove("pool") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        Some(_) => return Err(bad("'pool' must be the name of a warm pool")),
    };
    let tenant: Option<String> = match caller {
        Caller::Tenant(t) => Some(t.clone()),
        Caller::Admin => body
            .get("tenant")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_owned),
    };
    let reason = if body.get("reschedulable").and_then(Value::as_bool) == Some(true) {
        Some(
            "a reschedulable VM is recreated from its spec, which a warm VM does not have"
                .to_owned(),
        )
    } else {
        wp::unclaimable_reason(&body)
    };
    let picked = {
        let mut t = fleet.tenancy.lock().await;
        if named.is_none() && (t.pools.is_empty() || reason.is_some()) {
            return Ok(Launch::Create(body));
        }
        let nodes = fleet.nodes.lock().await;
        if let Some(n) = &named {
            let pool = t
                .pools
                .get(n)
                .filter(|p| sees(&t, caller, p))
                .ok_or_else(|| no_pool(n))?;
            if let Some(owner) = &pool.tenant
                && tenant.as_deref() != Some(owner.as_str())
            {
                return Err(bad(format!(
                    "warm pool '{n}' belongs to tenant '{owner}': launch for it with \"tenant\": \"{owner}\""
                )));
            }
            if pool.tenant.is_none()
                && tenant.is_some()
                && let Some(why) = shared_refusal(&t, is_macos(&t, pool))
            {
                return Err(AppError(
                    StatusCode::CONFLICT,
                    format!("cannot launch from warm pool '{n}': {why}"),
                ));
            }
            if let Some(r) = &reason {
                return Err(bad(format!("cannot launch from warm pool '{n}': {r}")));
            }
            if let Some(img) = t.images.get(&pool.image) {
                let mut probe = body.clone();
                if probe.get("image").is_none_or(Value::is_null) {
                    probe["image"] = json!(img.image);
                }
                if !wp::matches(pool, img, &probe) {
                    return Err(bad(format!(
                        "warm pool '{n}' holds {} with {} vCPUs and {} MiB: leave out image and size, or match them",
                        pool.image, pool.size.vcpus, pool.size.memory_mib
                    )));
                }
            }
        }
        let requested_node = body.get("node").and_then(Value::as_str);
        let selector: HashMap<String, String> = body
            .get("nodeSelector")
            .and_then(Value::as_object)
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        // Candidate pools: the caller's own first, then shared ones.
        let mut pools: Vec<&Pool> = t
            .pools
            .values()
            .filter(|p| named.as_ref().is_none_or(|n| *n == p.name))
            .filter(|p| match &p.tenant {
                Some(owner) => tenant.as_deref() == Some(owner.as_str()),
                None => true,
            })
            .collect();
        pools.sort_by_key(|p| p.tenant.is_none());
        let mut best: Option<(usize, String)> = None;
        for p in pools {
            let Some(img) = t.images.get(&p.image) else {
                continue;
            };
            if named.is_none() && !wp::matches(p, img, &body) {
                continue;
            }
            let macos = img.guest_os == "macos";
            if p.tenant.is_none() && tenant.is_some() && shared_refusal(&t, macos).is_some() {
                continue;
            }
            let found = t
                .vms
                .iter()
                .enumerate()
                .filter(|(_, v)| {
                    v.active()
                        && v.claimed_at.is_none()
                        && v.warm_pool.as_deref() == Some(p.name.as_str())
                        && p.runtime.ready.contains(&wp::key(&v.node, &v.vm_id))
                })
                .filter(|(_, v)| requested_node.is_none_or(|n| n == v.node))
                .filter(|(_, v)| {
                    nodes.get(&v.node).is_some_and(|n| {
                        n.healthy() && !n.cordoned && n.matches_selector(&selector)
                    }) && !t.draining.contains(&v.node)
                })
                .filter(|(_, v)| match &tenant {
                    Some(tid) if t.enabled() => t
                        .admit(&v.node, &Caller::Tenant(tid.clone()), macos, false)
                        .is_ok(),
                    _ => true,
                })
                .min_by_key(|(_, v)| v.created_at)
                .map(|(i, _)| i);
            if let Some(i) = found {
                best = Some((i, p.name.clone()));
                break;
            }
        }
        drop(nodes);
        match best {
            None => None,
            Some((i, pool)) => {
                let now = Utc::now();
                let (vcpus, mem, owner) = {
                    let v = &t.vms[i];
                    (v.vcpus, v.memory_mib, v.tenant.clone())
                };
                // A shared pool's VM becomes the tenant's now, so it must fit the tenant's quota.
                if let Some(tid) = &tenant
                    && t.enabled()
                    && owner.as_deref() != Some(tid.as_str())
                {
                    t.check_quota(tid, vcpus, mem).map_err(tenancy_error)?;
                }
                let ttl = body
                    .get("ttl_seconds")
                    .and_then(Value::as_u64)
                    .and_then(|s| chrono::Duration::try_seconds(s.min(10 * 365 * 86400) as i64));
                let wanted = body.get("name").and_then(Value::as_str).map(str::to_owned);
                let v = &mut t.vms[i];
                let picked = Picked {
                    pool: pool.clone(),
                    node: v.node.clone(),
                    vm_id: v.vm_id.clone(),
                    name: wanted.unwrap_or_else(|| v.name.clone()),
                    tenant: tenant.clone(),
                    warm_secs: (now - v.created_at).num_seconds().max(0),
                    at: now,
                };
                v.claimed_at = Some(now);
                if v.tenant.is_none() {
                    // Billed from the claim: until now it was the operator's.
                    v.billed_from = Some(now);
                    v.tenant = picked.tenant.clone();
                }
                v.expires_at = ttl.map(|d| now + d);
                v.name = picked.name.clone();
                let k = wp::key(&picked.node, &picked.vm_id);
                if let Some(p) = t.pools.get_mut(&pool) {
                    p.runtime.ready.remove(&k);
                }
                fleet.persist_tenancy(&t);
                Some(picked)
            }
        }
    };
    let Some(c) = picked else {
        if let Some(n) = &named {
            cold_from_pool(fleet, n, &mut body).await;
        }
        return Ok(Launch::Create(body));
    };

    // Hand it over: the node renames it and drops the warm label.
    let target = fleet.nodes.lock().await.get(&c.node).cloned();
    let ttl_set = body.get("ttl_seconds").and_then(Value::as_u64).is_some();
    let mut labels = serde_json::Map::new();
    labels.insert(wp::WARM_LABEL.into(), Value::Null);
    labels.insert(wp::CLAIMED_LABEL.into(), json!(c.pool));
    labels.insert(
        wp::EPHEMERAL_LABEL.into(),
        if ttl_set { json!("true") } else { Value::Null },
    );
    let patched = match &target {
        Some(n) => {
            patch_vm(
                fleet,
                n,
                &c.vm_id,
                &json!({"name": c.name, "labels": labels}),
            )
            .await
        }
        None => Err(format!("node '{}' is no longer registered", c.node)),
    };
    let record = match patched {
        Ok(r) => r,
        Err(e) => {
            // The VM could not be handed over: retire it and boot one the usual way.
            tracing::warn!(pool = %c.pool, node = %c.node, vm = %c.vm_id, error = %e, "warm VM hand-over failed");
            {
                let mut t = fleet.tenancy.lock().await;
                if t.end_vm(&c.node, &c.vm_id, Utc::now()) {
                    fleet.persist_tenancy(&t);
                }
            }
            let _ = delete_vm_on_node(fleet, &c.node, &c.vm_id).await;
            pool_event(
                fleet,
                &c.pool,
                c.tenant.clone(),
                "claim",
                Some(c.name.clone()),
                false,
            );
            if let Some(n) = &named {
                cold_from_pool(fleet, n, &mut body).await;
            }
            kick(fleet);
            return Ok(Launch::Create(body));
        }
    };
    let ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    {
        let mut t = fleet.tenancy.lock().await;
        if let Some(p) = t.pools.get_mut(&c.pool) {
            p.record_claim(wp::ClaimRecord {
                at: c.at,
                ms,
                warm_secs: c.warm_secs,
                tenant: c.tenant.clone(),
            });
            fleet.persist_tenancy(&t);
        }
    }
    kick(fleet);
    let mut vm = if record.is_object() {
        record
    } else {
        json!({"id": c.vm_id, "name": c.name})
    };
    if let Some(exp) = body
        .get("ttl_seconds")
        .and_then(Value::as_u64)
        .and_then(|s| chrono::Duration::try_seconds(s.min(10 * 365 * 86400) as i64))
        .map(|d| c.at + d)
    {
        vm["expires_at"] = json!(exp);
    }
    Ok(Launch::Claimed(json!({
        "node": c.node,
        "vm": vm,
        "warm": true,
        "pool": c.pool,
        "claim_ms": ms,
    })))
}

/// Fills a launch that named a pool with nothing ready with the pool's image and size, so it boots the usual way.
async fn cold_from_pool(fleet: &Fleet, name: &str, body: &mut Value) {
    let t = fleet.tenancy.lock().await;
    let (Some(p), Some(obj)) = (t.pools.get(name), body.as_object_mut()) else {
        return;
    };
    let Some(img) = t.images.get(&p.image) else {
        return;
    };
    obj.insert("image".into(), json!(img.image));
    obj.insert("backend".into(), json!("vz"));
    obj.insert("vcpus".into(), json!(p.size.vcpus));
    obj.insert("memory_mib".into(), json!(p.size.memory_mib));
    match obj.get_mut("apple") {
        Some(Value::Object(a)) => {
            a.insert("guest_os".into(), json!(img.guest_os));
        }
        _ => {
            obj.insert("apple".into(), json!({"guest_os": img.guest_os}));
        }
    }
    if let Some(d) = img.disk_size_gib {
        obj.insert("disk_size_gib".into(), json!(d));
    }
    if !p.node_selector.is_empty() && !obj.contains_key("nodeSelector") {
        obj.insert("nodeSelector".into(), json!(p.node_selector));
    }
}

// ---- the VM list --------------------------------------------------------------------------------------------------

/// Marks warm and claimed VMs in a `GET /fleet/vms` answer: `warm_pool` and `warm_state` (`ready`/`booting`) on a warm
/// VM, `claimed_from`/`claimed_at` on a claimed one, and the ledger's tenant and expiry where the node's record has
/// none (a warm VM is booted before anyone owns it).
pub(super) async fn annotate(fleet: &Fleet, all: &mut Value) {
    let t = fleet.tenancy.lock().await;
    let marked: HashMap<String, &VmEntry> = t
        .vms
        .iter()
        .filter(|v| v.active() && (v.warm_pool.is_some() || v.expires_at.is_some()))
        .map(|v| (wp::key(&v.node, &v.vm_id), v))
        .collect();
    if marked.is_empty() {
        return;
    }
    let Some(items) = all.get_mut("items").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items.iter_mut() {
        let k = wp::key(
            item.get("node").and_then(Value::as_str).unwrap_or_default(),
            item.get("id").and_then(Value::as_str).unwrap_or_default(),
        );
        let Some(e) = marked.get(&k).copied() else {
            continue;
        };
        if let Some(pool) = &e.warm_pool {
            if e.claimed_at.is_none() {
                let ready = t
                    .pools
                    .get(pool)
                    .is_some_and(|p| p.runtime.ready.contains(&wp::key(&e.node, &e.vm_id)));
                item["warm_pool"] = json!(pool);
                item["warm_state"] = json!(if ready { "ready" } else { "booting" });
            } else {
                item["claimed_from"] = json!(pool);
                item["claimed_at"] = json!(e.claimed_at);
            }
        }
        if let Some(exp) = e.expires_at
            && item.get("expires_at").is_none_or(Value::is_null)
        {
            item["expires_at"] = json!(exp);
        }
        if let Some(tenant) = &e.tenant
            && let Some(req) = item.get_mut("request").and_then(Value::as_object_mut)
            && req.get("tenant").is_none_or(Value::is_null)
        {
            req.insert("tenant".into(), json!(tenant));
        }
    }
}

// ---- the reconciler -----------------------------------------------------------------------------------------------

/// Records a reconciler action in the activity log (the actor is `warm-pool`; a tenant pool's events are the tenant's).
fn pool_event(
    fleet: &Fleet,
    pool: &str,
    tenant: Option<String>,
    action: &str,
    target: Option<String>,
    ok: bool,
) {
    fleet.audit.record(super::audit::AuditEvent {
        time: Utc::now(),
        actor: "warm-pool".into(),
        tenant,
        method: "POST".into(),
        path: format!("/fleet/pools/{pool}/{action}"),
        target,
        status: if ok { 200 } else { 503 },
        key: None,
        user: None,
    });
}

async fn list_node(fleet: &Fleet, n: &NodeInfo) -> Result<HashMap<String, Value>, String> {
    let resp = fleet
        .http
        .get(format!("{}/v1/vms", n.fluxvm_url))
        .timeout(NODE_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("node '{}' answered {}", n.name, resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(v.get("items")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|vm| Some((vm.get("id")?.as_str()?.to_owned(), vm.clone())))
                .collect()
        })
        .unwrap_or_default())
}

/// `PATCH {node}/v1/vms/{id}` (rename, labels); the node's updated record.
async fn patch_vm(fleet: &Fleet, n: &NodeInfo, id: &str, body: &Value) -> Result<Value, String> {
    let resp = fleet
        .http
        .patch(format!("{}/v1/vms/{id}", n.fluxvm_url))
        .json(body)
        .timeout(NODE_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("node '{}' unreachable: {e}", n.name))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let msg = v
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string());
        return Err(format!("node '{}' refused ({status}): {msg}", n.name));
    }
    Ok(v)
}

/// Ends a warm VM's ledger entry if it is still warm and unclaimed, so no launch can claim it; true when it did. The
/// caller then deletes it on its node.
async fn retire_warm(fleet: &Fleet, node: &str, vm_id: &str) -> bool {
    let mut t = fleet.tenancy.lock().await;
    let Some(v) = t.vms.iter_mut().rev().find(|v| {
        v.node == node
            && v.vm_id == vm_id
            && v.active()
            && v.claimed_at.is_none()
            && v.warm_pool.is_some()
    }) else {
        return false;
    };
    v.ended_at = Some(Utc::now());
    let pool = v.warm_pool.clone().unwrap_or_default();
    if let Some(p) = t.pools.get_mut(&pool) {
        p.runtime.ready.remove(&wp::key(node, vm_id));
    }
    fleet.persist_tenancy(&t);
    true
}

/// A warm VM that has to go, and why.
struct Doomed {
    entry: VmEntry,
    reason: String,
    /// False when the node no longer lists it (nothing to delete there).
    on_node: bool,
}

/// One pass over every pool: deletes claimed VMs whose TTL ran out; checks each warm VM on its node (ready, still
/// booting, failed, gone, or on a node it may no longer use) and replaces the ones that will not become ready; trims
/// pools above `min_ready`; and boots VMs for pools below it, backing off after failures. Returns what it did.
pub(super) async fn reconcile_pools(fleet: &Fleet) -> Value {
    fn push(report: &mut serde_json::Map<String, Value>, k: &str, v: Value) {
        if let Some(list) = report.entry(k).or_insert_with(|| json!([])).as_array_mut() {
            list.push(v);
        }
    }
    let _pass = PASS.lock().await;
    let now = Utc::now();
    let mut report = serde_json::Map::new();

    // 1. What the ledger says, and which warm VMs sit where they may no longer be.
    let nodes: HashMap<String, NodeInfo> = fleet.nodes.lock().await.clone();
    let (expired, warm, misplaced) = {
        let t = fleet.tenancy.lock().await;
        let expired: Vec<VmEntry> = t
            .vms
            .iter()
            .filter(|v| v.active() && v.expires_at.is_some_and(|e| e <= now))
            .cloned()
            .collect();
        let warm: Vec<VmEntry> = t
            .vms
            .iter()
            .filter(|v| v.active() && v.claimed_at.is_none() && v.warm_pool.is_some())
            .cloned()
            .collect();
        let mut misplaced: HashMap<String, String> = HashMap::new();
        for v in &warm {
            let pool_name = v.warm_pool.as_deref().unwrap_or_default();
            let why = match t.pools.get(pool_name) {
                None => Some("its pool was deleted".to_owned()),
                Some(p) => {
                    let labels = nodes
                        .get(&v.node)
                        .map(|n| n.labels.clone())
                        .unwrap_or_default();
                    placement_refusal(&t, p, &v.node, &labels, v.macos_guest)
                }
            };
            if let Some(w) = why {
                misplaced.insert(wp::key(&v.node, &v.vm_id), w);
            }
        }
        (expired, warm, misplaced)
    };

    // 2. Claimed VMs past their TTL.
    for e in expired {
        if e.warm_pool.is_some() && e.claimed_at.is_none() {
            continue;
        }
        match delete_vm_on_node(fleet, &e.node, &e.vm_id).await {
            Ok(_) => {
                let mut t = fleet.tenancy.lock().await;
                if t.end_vm(&e.node, &e.vm_id, Utc::now()) {
                    fleet.persist_tenancy(&t);
                }
                push(
                    &mut report,
                    "expired",
                    json!({"node": e.node, "name": e.name}),
                );
            }
            Err(err) => push(
                &mut report,
                "errors",
                json!({"node": e.node, "vm": e.vm_id, "error": err.1}),
            ),
        }
    }

    // 3. Each warm VM as its node sees it.
    let hosting: BTreeSet<&str> = warm.iter().map(|v| v.node.as_str()).collect();
    let mut listed: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for name in hosting {
        let Some(n) = nodes.get(name).filter(|n| n.healthy()) else {
            continue;
        };
        match list_node(fleet, n).await {
            Ok(m) => {
                listed.insert(name.to_owned(), m);
            }
            Err(e) => push(&mut report, "errors", json!({"node": name, "error": e})),
        }
    }
    let mut ready_now: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut drops: Vec<Doomed> = Vec::new();
    for v in &warm {
        let k = wp::key(&v.node, &v.vm_id);
        let pool = v.warm_pool.clone().unwrap_or_default();
        let state = listed.get(&v.node).map(|m| wp::classify(m.get(&v.vm_id)));
        if let Some(why) = misplaced.get(&k) {
            drops.push(Doomed {
                entry: v.clone(),
                reason: why.clone(),
                on_node: state != Some(WarmState::Missing),
            });
            continue;
        }
        let age = (now - v.created_at).num_seconds();
        match state {
            // Its node did not answer: not handed out, not replaced yet.
            None => {}
            Some(WarmState::Ready) => {
                ready_now.entry(pool).or_default().insert(k);
            }
            Some(WarmState::Booting) if age > wp::BOOT_TIMEOUT_SECS => drops.push(Doomed {
                entry: v.clone(),
                reason: format!("still booting after {} minutes", wp::BOOT_TIMEOUT_SECS / 60),
                on_node: true,
            }),
            Some(WarmState::Booting) => {}
            Some(WarmState::Failed) => drops.push(Doomed {
                entry: v.clone(),
                reason: "stopped or failed".into(),
                on_node: true,
            }),
            Some(WarmState::Missing) if age > wp::MISSING_GRACE_SECS => drops.push(Doomed {
                entry: v.clone(),
                reason: "its node no longer lists it".into(),
                on_node: false,
            }),
            Some(WarmState::Missing) => {}
        }
    }

    // 4. Retire and delete the ones that have to go.
    for d in drops {
        let e = &d.entry;
        if !retire_warm(fleet, &e.node, &e.vm_id).await {
            continue; // claimed meanwhile
        }
        if d.on_node
            && let Err(err) = delete_vm_on_node(fleet, &e.node, &e.vm_id).await
        {
            push(
                &mut report,
                "errors",
                json!({"node": e.node, "vm": e.vm_id, "error": err.1}),
            );
        }
        let pool = e.warm_pool.clone().unwrap_or_default();
        pool_event(
            fleet,
            &pool,
            e.tenant.clone(),
            "replace",
            Some(format!("{}: {}", e.name, d.reason)),
            true,
        );
        push(
            &mut report,
            "replaced",
            json!({"pool": pool, "node": e.node, "name": e.name, "reason": d.reason}),
        );
    }

    // 5. New readiness, and the plan for each pool.
    let plans: Vec<(String, wp::Plan)> = {
        let mut t = fleet.tenancy.lock().await;
        let still_warm: HashSet<String> = t
            .vms
            .iter()
            .filter(|v| v.active() && v.claimed_at.is_none() && v.warm_pool.is_some())
            .map(|v| wp::key(&v.node, &v.vm_id))
            .collect();
        let mut changed = false;
        for p in t.pools.values_mut() {
            let ready: BTreeSet<String> = ready_now
                .remove(&p.name)
                .unwrap_or_default()
                .into_iter()
                .filter(|k| still_warm.contains(k))
                .collect();
            if p.runtime.ready != ready {
                p.runtime.ready = ready;
                changed = true;
            }
        }
        if changed {
            fleet.persist_tenancy(&t);
        }
        t.pools
            .values()
            .filter(|p| {
                p.tenant
                    .as_ref()
                    .is_none_or(|o| t.tenants.get(o).is_some_and(|x| !x.disabled))
            })
            .map(|p| {
                let warm = wp::warm_vms(p, &t.vms);
                let mut plan = wp::plan(p, &warm, wp::claimed_alive(&p.name, &t.vms));
                if p.backing_off(now) || !t.images.contains_key(&p.image) {
                    plan.create = 0;
                }
                (p.name.clone(), plan)
            })
            .collect()
    };

    // 6. Trim, then fill.
    for (pool, plan) in &plans {
        for k in &plan.trim {
            let Some((node, id)) = k.rsplit_once('/') else {
                continue;
            };
            if !retire_warm(fleet, node, id).await {
                continue;
            }
            match delete_vm_on_node(fleet, node, id).await {
                Ok(_) => push(
                    &mut report,
                    "trimmed",
                    json!({"pool": pool, "node": node, "id": id}),
                ),
                Err(err) => push(
                    &mut report,
                    "errors",
                    json!({"node": node, "vm": id, "error": err.1}),
                ),
            }
        }
    }
    for (pool, plan) in plans {
        for _ in 0..plan.create {
            match fill_one(fleet, &pool).await {
                Ok(v) => push(&mut report, "created", v),
                Err(msg) => {
                    let owner = {
                        let mut t = fleet.tenancy.lock().await;
                        let owner = t.pools.get(&pool).and_then(|p| p.tenant.clone());
                        let first = t.pools.get(&pool).is_some_and(|p| p.runtime.failures == 0);
                        if let Some(p) = t.pools.get_mut(&pool) {
                            p.record_failure(msg.clone(), Utc::now());
                            fleet.persist_tenancy(&t);
                        }
                        first.then_some(owner)
                    };
                    // Logged once when a pool starts failing, not on every retry.
                    if let Some(owner) = owner {
                        pool_event(fleet, &pool, owner, "fill", Some(msg.clone()), false);
                    }
                    push(&mut report, "errors", json!({"pool": pool, "error": msg}));
                    break;
                }
            }
        }
    }
    Value::Object(report)
}

/// The message when no node can take a pool's next warm VM.
fn no_room(pool: &Pool, macos: bool) -> String {
    let guest = if macos { "macOS" } else { "Linux" };
    let mut msg = match &pool.tenant {
        Some(t) => format!(
            "no Mac {t} leases has room for another {guest} VM of {} vCPUs and {} MiB",
            pool.size.vcpus, pool.size.memory_mib
        ),
        None => format!(
            "no host nobody leases has room for another {guest} VM of {} vCPUs and {} MiB",
            pool.size.vcpus, pool.size.memory_mib
        ),
    };
    if macos {
        msg.push_str(" (Apple allows two macOS VMs per Mac)");
    }
    if !pool.nodes.is_empty() || !pool.node_selector.is_empty() {
        msg.push_str(" among the nodes the pool may use");
    }
    msg
}

/// Boots one warm VM for `pool` on the best node it may use. Returns what was created.
async fn fill_one(fleet: &Fleet, pool_name: &str) -> Result<Value, String> {
    let vm_name = format!(
        "warm-{pool_name}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let (pool, body, target) = {
        let t = fleet.tenancy.lock().await;
        let nodes = fleet.nodes.lock().await;
        let pool = t
            .pools
            .get(pool_name)
            .cloned()
            .ok_or_else(|| format!("pool '{pool_name}' was deleted"))?;
        let img = t
            .images
            .get(&pool.image)
            .cloned()
            .ok_or_else(|| format!("image '{}' is not in the catalog", pool.image))?;
        let macos = img.guest_os == "macos";
        let body = pool.fill_body(&img, &vm_name);
        let apple = apple_placement_request(&body, pool.size.vcpus, pool.size.memory_mib);
        let admit = |n: &NodeInfo| {
            placement_refusal(&t, &pool, &n.name, &n.labels, macos).is_none()
                && (!macos || macos_guests_on(&t, &n.name) < 2)
        };
        let target = pick_best_capacity_admitted(
            &nodes,
            pool.size.vcpus,
            pool.size.memory_mib,
            &HashSet::new(),
            &HashMap::new(),
            SecurityProfile::Standard,
            apple.as_ref(),
            &admit,
        )
        .ok_or_else(|| no_room(&pool, macos))?;
        (pool, body, target.name)
    };
    let mut body = body;
    body["node"] = json!(target);
    let Json(created) = create_vm_inner(fleet, &Caller::Admin, body, false)
        .await
        .map_err(|e| e.1)?;
    let node = created
        .get("node")
        .and_then(Value::as_str)
        .unwrap_or(&target)
        .to_owned();
    let id = created
        .pointer("/vm/id")
        .and_then(Value::as_str)
        .ok_or("the node's answer had no VM id")?
        .to_owned();
    let ready = wp::classify(created.get("vm")) == WarmState::Ready;
    {
        let mut t = fleet.tenancy.lock().await;
        if let Some(e) = t
            .vms
            .iter_mut()
            .rev()
            .find(|e| e.node == node && e.vm_id == id && e.active())
        {
            e.warm_pool = Some(pool.name.clone());
        }
        if let Some(p) = t.pools.get_mut(&pool.name) {
            if ready {
                p.runtime.ready.insert(wp::key(&node, &id));
            }
            p.record_success(Utc::now());
        }
        fleet.persist_tenancy(&t);
    }
    // The label shows on the node what the VM is for; nothing depends on it, so a node that cannot label is fine.
    let target_node = fleet.nodes.lock().await.get(&node).cloned();
    if let Some(n) = target_node {
        let mut labels = serde_json::Map::new();
        labels.insert(wp::WARM_LABEL.into(), json!(pool.name));
        labels.insert(wp::EPHEMERAL_LABEL.into(), json!("true"));
        if let Err(e) = patch_vm(fleet, &n, &id, &json!({"labels": labels})).await {
            tracing::debug!(%node, vm = %id, error = %e, "could not label a warm VM");
        }
    }
    Ok(json!({"pool": pool.name, "node": node, "id": id, "name": vm_name, "ready": ready}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::Quota;
    use fluxvm_scheduler::apple_placement::AppleHostCaps;

    /// A simulated Mac serving the node API on a loopback port, registered as `name`.
    async fn sim_mac(name: &str) -> NodeInfo {
        let app = crate::sim::SimNode::default().router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        NodeInfo {
            name: name.into(),
            fluxvm_url: format!("http://{addr}"),
            vcpus_total: 64,
            memory_mib_total: 256 * 1024,
            vm_count: 0,
            last_seen: Utc::now() + chrono::Duration::hours(1),
            cordoned: false,
            labels: HashMap::new(),
            security: Default::default(),
            density: None,
            daemon: None,
            allocated: None,
            apple: Some(AppleHostCaps {
                free_cpu: 64,
                free_memory_mib: 256 * 1024,
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
            token: Some("admin".into()),
            tenancy: Arc::new(Mutex::new(tenancy)),
            tenancy_path: None,
            ha: None,
            audit: Arc::new(super::super::audit::AuditLog::memory()),
            notify: Arc::default(),
            metrics: Arc::default(),
            tunnels: Arc::default(),
            sso: Arc::default(),
            signup: Arc::default(),
        }
    }

    fn catalog(t: &mut TenancyState) {
        for (name, guest, path) in [
            ("debian-13", "linux", "/vms/debian/disk.raw"),
            ("macos-27", "macos", "/vms/macos/disk.raw"),
        ] {
            t.images.insert(
                name.into(),
                serde_json::from_value(json!({
                    "name": name, "guest_os": guest, "image": path, "vcpus": 2, "memory_mib": 4096,
                }))
                .unwrap(),
            );
        }
    }

    async fn add_pool(fleet: &Fleet, caller: Caller, body: Value) -> Result<Value, AppError> {
        let spec: PoolSpec = serde_json::from_value(body).unwrap();
        create_pool(State(fleet.clone()), Extension(caller), Json(spec))
            .await
            .map(|(_, Json(v))| v)
    }

    async fn status(fleet: &Fleet, name: &str) -> Value {
        let Json(v) = get_pool(
            State(fleet.clone()),
            Extension(Caller::Admin),
            Path(name.into()),
        )
        .await
        .unwrap();
        v["status"].clone()
    }

    async fn launch(fleet: &Fleet, caller: &Caller, body: Value) -> Result<Value, AppError> {
        create_vm_inner(fleet, caller, body, false)
            .await
            .map(|Json(v)| v)
    }

    #[test]
    fn tenants_reach_the_pool_routes() {
        use axum::http::Method;
        assert!(tenant_route_allowed(&Method::GET, "/fleet/pools"));
        assert!(tenant_route_allowed(&Method::POST, "/fleet/pools"));
        assert!(tenant_route_allowed(&Method::PATCH, "/fleet/pools/ci"));
        assert!(tenant_route_allowed(&Method::DELETE, "/fleet/pools/ci"));
        assert!(tenant_route_allowed(&Method::GET, "/fleet/pools/ci/status"));
        assert!(!tenant_route_allowed(&Method::PUT, "/fleet/pools/ci"));
        assert!(!tenant_route_allowed(
            &Method::POST,
            "/fleet/pools/ci/status"
        ));
    }

    /// Tenants do not see or claim shared pools unless the operator turns that on; the owner of a pool always does.
    #[test]
    fn shared_pools_are_off_for_tenants_until_the_operator_allows_them() {
        let mut t = TenancyState::default();
        t.create_tenant("acme", None, Quota::default(), Utc::now())
            .unwrap();
        let shared = Pool {
            name: "linux".into(),
            tenant: None,
            image: "debian-13".into(),
            size: crate::warmpool::PoolSize {
                vcpus: 2,
                memory_mib: 2048,
            },
            min_ready: 1,
            max_total: 4,
            node_selector: Default::default(),
            nodes: vec![],
            created_at: Utc::now(),
            runtime: Default::default(),
        };
        let own = Pool {
            name: "mine".into(),
            tenant: Some("acme".into()),
            ..shared.clone()
        };
        let acme = Caller::Tenant("acme".into());
        let why = shared_refusal(&t, false).expect("refused by default");
        assert!(why.contains("off in this cloud's policy"), "{why}");
        assert!(!sees(&t, &acme, &shared));
        assert!(sees(&t, &acme, &own), "a tenant always sees its own pool");
        assert!(sees(&t, &Caller::Admin, &shared));
        t.policy.allow_shared_pool_claims = true;
        assert!(shared_refusal(&t, false).is_none());
        assert!(sees(&t, &acme, &shared));
    }

    #[tokio::test]
    async fn a_shared_pool_fills_hands_out_each_warm_vm_once_and_refills() {
        let mac = sim_mac("mac-1").await;
        let mut t = TenancyState::default();
        catalog(&mut t);
        // Off by default: the operator has to turn shared claims on.
        t.policy.allow_shared_pool_claims = true;
        t.create_tenant("acme", None, Quota::default(), Utc::now())
            .unwrap();
        let fleet = fleet_of(vec![mac.clone()], t);

        add_pool(
            &fleet,
            Caller::Admin,
            json!({"name": "linux", "image": "debian-13", "min_ready": 2}),
        )
        .await
        .unwrap();
        let _ = PASS.lock().await; // the pass the create started
        reconcile_pools(&fleet).await;
        let s = status(&fleet, "linux").await;
        assert_eq!(
            (s["ready"].as_u64(), s["booting"].as_u64()),
            (Some(2), Some(0)),
            "{s}"
        );
        {
            let t = fleet.tenancy.lock().await;
            assert!(
                t.vms
                    .iter()
                    .all(|v| v.tenant.is_none() && v.warm_pool.is_some()),
                "warm VMs of a shared pool are nobody's"
            );
        }

        // Five launches at once: two get the warm VMs, three boot normally, nobody shares a VM.
        let tenant = Caller::Tenant("acme".into());
        let mut tasks = Vec::new();
        for i in 0..5 {
            let f = fleet.clone();
            let c = tenant.clone();
            tasks.push(tokio::spawn(async move {
                launch(
                    &f,
                    &c,
                    json!({"name": format!("job-{i}"), "backend": "vz", "image": "/vms/debian/disk.raw",
                           "vcpus": 2, "memory_mib": 4096, "apple": {"guest_os": "linux"}, "ttl_seconds": 600}),
                )
                .await
                .unwrap()
            }));
        }
        let mut ids = HashSet::new();
        let mut warm = 0;
        for task in tasks {
            let v = task.await.unwrap();
            assert!(
                ids.insert(v["vm"]["id"].as_str().unwrap().to_owned()),
                "{v}"
            );
            if v["warm"] == true {
                warm += 1;
                assert_eq!(v["pool"], "linux");
                assert!(
                    v["vm"]["name"].as_str().unwrap().starts_with("job-"),
                    "renamed: {v}"
                );
            }
        }
        // At least the two warm VMs; a refill that finished during the burst may have served one more.
        assert!(warm >= 2, "{warm}");

        {
            let t = fleet.tenancy.lock().await;
            let claimed: Vec<&VmEntry> = t.vms.iter().filter(|v| v.claimed_at.is_some()).collect();
            assert_eq!(claimed.len(), warm, "one claim per warm answer");
            for v in claimed {
                assert_eq!(v.tenant.as_deref(), Some("acme"));
                assert_eq!(v.billed_from, v.claimed_at, "billed from the claim");
                assert!(v.expires_at.is_some(), "the launch's TTL applies");
                assert!(v.name.starts_with("job-"));
            }
            assert_eq!(t.pools["linux"].runtime.claims.len(), warm);
            let used = t.usage(
                Utc::now() - chrono::Duration::hours(1),
                Utc::now(),
                Some("acme"),
                Utc::now(),
            );
            assert_eq!(used.tenants[0].vms_started, 5);
        }

        // The node knows the new name; the list marks the claim.
        let Json(mut all) = list_vms_all(&fleet).await;
        annotate(&fleet, &mut all).await;
        let items = all["items"].as_array().unwrap();
        let claimed: Vec<&Value> = items
            .iter()
            .filter(|v| v.get("claimed_from").is_some())
            .collect();
        assert_eq!(claimed.len(), warm);
        assert!(
            claimed
                .iter()
                .all(|v| v["name"].as_str().unwrap().starts_with("job-")),
            "{all}"
        );
        assert!(
            claimed.iter().all(|v| v["request"]["tenant"] == "acme"),
            "{all}"
        );

        // Refilled.
        reconcile_pools(&fleet).await;
        reconcile_pools(&fleet).await;
        let s = status(&fleet, "linux").await;
        assert_eq!(s["ready"], 2, "{s}");
        assert_eq!(s["claimed_24h"].as_u64(), Some(warm as u64));
        assert!(s["avg_claim_ms"].is_u64());

        // A launch that changes how the VM boots never takes a warm one.
        let v = launch(
            &fleet,
            &tenant,
            json!({"name": "ci", "backend": "vz", "image": "/vms/debian/disk.raw",
                   "apple": {"guest_os": "linux"}, "cloud_init": {"runcmd": ["true"]}}),
        )
        .await
        .unwrap();
        assert!(v.get("warm").is_none(), "{v}");

        // Deleting the pool deletes its warm VMs, not the claimed ones.
        let Json(d) = delete_pool(
            State(fleet.clone()),
            Extension(Caller::Admin),
            Path("linux".into()),
        )
        .await
        .unwrap();
        assert_eq!(d["drained"].as_array().unwrap().len(), 2, "{d}");
        let t = fleet.tenancy.lock().await;
        assert_eq!(
            t.vms
                .iter()
                .filter(|v| v.active() && v.claimed_at.is_some())
                .count(),
            warm
        );
        assert!(
            t.vms
                .iter()
                .all(|v| v.claimed_at.is_some() || !v.active() || v.warm_pool.is_none())
        );
    }

    #[tokio::test]
    async fn tenant_pools_stay_on_their_leased_macs_and_shared_macos_is_refused() {
        let leased = sim_mac("leased").await;
        let shared = sim_mac("shared").await;
        let mut t = TenancyState::default();
        catalog(&mut t);
        for id in ["acme", "globex"] {
            t.create_tenant(id, None, Quota::default(), Utc::now())
                .unwrap();
        }
        t.create_lease("acme", "leased", None, Utc::now()).unwrap();
        let fleet = fleet_of(vec![leased, shared], t);
        let acme = Caller::Tenant("acme".into());
        let globex = Caller::Tenant("globex".into());

        let v = add_pool(
            &fleet,
            acme.clone(),
            json!({"name": "ios", "image": "macos-27", "min_ready": 3}),
        )
        .await
        .unwrap();
        assert_eq!(v["owner"], "acme");
        let _ = PASS.lock().await;
        reconcile_pools(&fleet).await;
        {
            let t = fleet.tenancy.lock().await;
            let warm: Vec<&VmEntry> = t.vms.iter().filter(|v| v.active()).collect();
            // Apple's limit: two macOS VMs on the one leased Mac, none elsewhere.
            assert_eq!(warm.len(), 2);
            assert!(
                warm.iter()
                    .all(|v| v.node == "leased" && v.tenant.as_deref() == Some("acme"))
            );
            assert!(
                t.pools["ios"]
                    .runtime
                    .last_error
                    .as_deref()
                    .unwrap()
                    .contains("two macOS")
            );
            assert_eq!(t.pools["ios"].runtime.failures, 1);
        }
        // Another tenant neither sees nor uses it.
        let Json(l) = list_pools(State(fleet.clone()), Extension(globex.clone())).await;
        assert!(l["items"].as_array().unwrap().is_empty());
        let err = launch(
            &fleet,
            &globex,
            json!({"name": "x", "image": "macos-27", "pool": "ios"}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        // The owner claims by naming the pool, and the warm VM counts against its quota from the start.
        let v = launch(&fleet, &acme, json!({"name": "build-1", "pool": "ios"}))
            .await
            .unwrap();
        assert_eq!(v["warm"], true, "{v}");
        assert_eq!(v["node"], "leased");
        {
            let t = fleet.tenancy.lock().await;
            let e = t.vms.iter().find(|e| e.name == "build-1").unwrap();
            assert_eq!(
                e.billed_from, None,
                "a tenant's own warm VM was billed all along"
            );
        }

        // A shared macOS pool runs only on the unleased Mac, and tenants may not claim from it under the default policy.
        let v = add_pool(
            &fleet,
            Caller::Admin,
            json!({"name": "mac-shared", "image": "macos-27", "min_ready": 1}),
        )
        .await
        .unwrap();
        assert!(
            v["note"].as_str().unwrap().contains("Tenants cannot claim"),
            "{v}"
        );
        let _ = PASS.lock().await;
        reconcile_pools(&fleet).await;
        {
            let t = fleet.tenancy.lock().await;
            let w = t
                .vms
                .iter()
                .find(|v| v.active() && v.warm_pool.as_deref() == Some("mac-shared"))
                .unwrap();
            assert_eq!(w.node, "shared");
        }
        let err = launch(&fleet, &globex, json!({"name": "y", "pool": "mac-shared"}))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND, "{}", err.1);
        // A plain launch of the image is simply not answered from it (and fails as before: no lease).
        let err = launch(
            &fleet,
            &globex,
            json!({"name": "y", "backend": "vz", "image": "/vms/macos/disk.raw", "apple": {"guest_os": "macos"}}),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE, "{}", err.1);
        // The operator's own launches can.
        let v = launch(
            &fleet,
            &Caller::Admin,
            json!({"name": "op-1", "pool": "mac-shared"}),
        )
        .await
        .unwrap();
        assert_eq!(v["warm"], true, "{v}");

        // Patch and access rules.
        let Json(p) = patch_pool(
            State(fleet.clone()),
            Extension(acme.clone()),
            Path("ios".into()),
            Json(PoolPatch {
                min_ready: Some(1),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(p["min_ready"], 1);
        assert!(
            patch_pool(
                State(fleet.clone()),
                Extension(globex.clone()),
                Path("ios".into()),
                Json(PoolPatch::default()),
            )
            .await
            .is_err()
        );
        assert!(
            add_pool(
                &fleet,
                acme.clone(),
                json!({"name": "x", "image": "macos-27", "tenant": "globex"})
            )
            .await
            .is_err()
        );
        assert!(
            add_pool(
                &fleet,
                acme.clone(),
                json!({"name": "y", "image": "macos-27", "nodes": ["shared"]})
            )
            .await
            .is_err(),
            "a tenant names only Macs it leases"
        );
    }

    #[tokio::test]
    async fn claimed_vms_expire_and_failed_warm_vms_are_replaced() {
        let mac = sim_mac("mac-1").await;
        let mut t = TenancyState::default();
        catalog(&mut t);
        let fleet = fleet_of(vec![mac.clone()], t);
        add_pool(
            &fleet,
            Caller::Admin,
            json!({"name": "p", "image": "debian-13", "min_ready": 1}),
        )
        .await
        .unwrap();
        let _ = PASS.lock().await;
        reconcile_pools(&fleet).await;
        let v = launch(
            &fleet,
            &Caller::Admin,
            json!({"name": "short", "image": "debian-13", "ttl_seconds": 60}),
        )
        .await
        .unwrap();
        assert_eq!(v["warm"], true, "{v}");
        let id = v["vm"]["id"].as_str().unwrap().to_owned();
        {
            let mut t = fleet.tenancy.lock().await;
            let e = t.vms.iter_mut().find(|e| e.vm_id == id).unwrap();
            e.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        }
        reconcile_pools(&fleet).await;
        assert!(
            !fleet
                .tenancy
                .lock()
                .await
                .vms
                .iter()
                .any(|e| e.vm_id == id && e.active())
        );
        let r = fleet
            .http
            .get(format!("{}/v1/vms/{id}", mac.fluxvm_url))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "deleted on the node too");

        // A warm VM that stopped is replaced.
        let warm_id = {
            let t = fleet.tenancy.lock().await;
            t.vms
                .iter()
                .find(|e| e.active() && e.warm_pool.is_some() && e.claimed_at.is_none())
                .unwrap()
                .vm_id
                .clone()
        };
        let r = fleet
            .http
            .post(format!("{}/v1/vms/{warm_id}/stop", mac.fluxvm_url))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        reconcile_pools(&fleet).await;
        reconcile_pools(&fleet).await;
        {
            let t = fleet.tenancy.lock().await;
            assert!(
                !t.vms.iter().any(|e| e.vm_id == warm_id && e.active()),
                "replaced"
            );
            assert_eq!(wp::warm_vms(&t.pools["p"], &t.vms).len(), 1, "and refilled");
        }
        assert_eq!(status(&fleet, "p").await["ready"], 1);
        let events = fleet.audit.query(None, 10);
        assert!(
            events
                .iter()
                .any(|e| e.actor == "warm-pool" && e.path.ends_with("/replace"))
        );
    }
}
