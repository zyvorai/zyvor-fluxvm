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
            if !fleet.leading() {
                continue;
            }
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
    super::notify::scan(fleet, &ended).await;
    let rotated = {
        let mut t = fleet.tenancy.lock().await;
        let days = t.policy.overlay_key_rotation_days;
        // `try_days`: an absurd policy value must not panic the reconciler (`days` panics out of range).
        let r = match ChronoDuration::try_days(days) {
            Some(max_age) if days > 0 => t.rotate_due_overlays(max_age, Utc::now()),
            _ => Vec::new(),
        };
        if !r.is_empty() {
            tracing::info!(networks = ?r, "overlay keys rotated");
            fleet.persist_tenancy(&t);
        }
        r
    };
    let compacted = {
        let mut t = fleet.tenancy.lock().await;
        let days = t.policy.ledger_retention_days;
        let n = if days > 0 {
            ChronoDuration::try_days(days).map_or(0, |d| t.compact(Utc::now() - d))
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
        "rotated_overlay_keys": rotated,
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

// ---- image catalog ------------------------------------------------------------------------------------------------

/// `GET /fleet/images`: the catalog, for tenants and the operator.
pub(super) async fn list_images(State(fleet): State<Fleet>) -> Json<Value> {
    let t = fleet.tenancy.lock().await;
    Json(json!({ "items": t.images.values().collect::<Vec<_>>() }))
}

/// `PUT /fleet/images/{name}` (operator): add or replace a catalog entry.
pub(super) async fn put_image(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Json(mut image): Json<crate::tenancy::CatalogImage>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    image.name = name;
    image.updated_at = Some(Utc::now());
    image
        .validate()
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e))?;
    let mut t = fleet.tenancy.lock().await;
    t.images.insert(image.name.clone(), image.clone());
    fleet.persist_tenancy(&t);
    Ok(Json(json!(image)))
}

/// `DELETE /fleet/images/{name}` (operator). VMs made from it are not touched.
pub(super) async fn delete_image(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<StatusCode, AppError> {
    require_admin(&caller)?;
    let mut t = fleet.tenancy.lock().await;
    if t.images.remove(&name).is_none() {
        return Err(not_found(format!("no image '{name}'")));
    }
    fleet.persist_tenancy(&t);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /fleet/nodes/{name}` for a tenant: only a Mac it leases, and only its size, free capacity and Apple
/// capabilities (no address, labels or security report).
pub(super) async fn tenant_node_view(
    fleet: &Fleet,
    tenant: &str,
    name: &str,
) -> Result<Value, AppError> {
    let leased = fleet
        .tenancy
        .lock()
        .await
        .active_lease_for_node(name)
        .is_some_and(|l| l.tenant == tenant);
    if !leased {
        return Err(not_found(format!("you do not lease a Mac named '{name}'")));
    }
    let nodes = fleet.nodes.lock().await;
    let n = nodes
        .get(name)
        .ok_or_else(|| not_found(format!("no registered node named '{name}'")))?;
    Ok(json!({
        "name": n.name,
        "healthy": n.healthy(),
        "vcpus_total": n.vcpus_total,
        "memory_mib_total": n.memory_mib_total,
        "free_vcpus": n.free_vcpus(),
        "free_memory_mib": n.free_memory_mib(),
        "vm_count": n.vm_count,
        "last_seen": n.last_seen,
        "apple": n.apple,
    }))
}

// ---- prices and billing -------------------------------------------------------------------------------------------

/// `GET /fleet/prices`: the price list (tenants too, so they can see what things cost).
pub(super) async fn get_prices(State(fleet): State<Fleet>) -> Json<crate::billing::PriceList> {
    Json(fleet.tenancy.lock().await.prices.clone())
}

/// `PUT /fleet/prices` (operator). Running leases keep the price they started with.
pub(super) async fn put_prices(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(prices): Json<crate::billing::PriceList>,
) -> Result<Json<crate::billing::PriceList>, AppError> {
    require_admin(&caller)?;
    prices
        .validate()
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e))?;
    let mut t = fleet.tenancy.lock().await;
    t.prices = prices;
    fleet.persist_tenancy(&t);
    Ok(Json(t.prices.clone()))
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct BillingQuery {
    /// `2026-10`; default the current month.
    #[serde(default)]
    month: Option<String>,
    /// Operator: one tenant (default: every tenant).
    #[serde(default)]
    tenant: Option<String>,
    /// `json` (default) or `csv`.
    #[serde(default)]
    format: Option<String>,
}

/// `GET /fleet/billing[?month=YYYY-MM&tenant=T&format=csv]`: statements, one per tenant. A tenant gets its own.
pub(super) async fn billing(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<BillingQuery>,
) -> Result<Response, AppError> {
    let now = Utc::now();
    let (from, to) = match &q.month {
        Some(m) => crate::billing::parse_month(m).ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                format!("month must look like 2026-10, not '{m}'"),
            )
        })?,
        None => crate::billing::month_of(now),
    };
    let t = fleet.tenancy.lock().await;
    let ids: Vec<String> = match (caller.tenant(), &q.tenant) {
        (Some(me), _) => vec![me.to_owned()],
        (None, Some(one)) => vec![one.clone()],
        (None, None) => t.tenants.keys().cloned().collect(),
    };
    let statements: Vec<crate::billing::Statement> = ids
        .iter()
        .filter(|id| t.tenants.contains_key(id.as_str()))
        .map(|id| crate::billing::statement(&t, id, from, to, now))
        .collect();
    let currency = t.prices.currency.clone();
    let free = t.prices.is_free();
    drop(t);
    Ok(match q.format.as_deref() {
        Some("csv") => {
            // One header for the whole file, even when there is no statement.
            let mut csv = String::from(crate::billing::CSV_HEADER);
            for s in &statements {
                let text = s.to_csv();
                csv.push_str(text.split_once('\n').map_or("", |(_, rest)| rest));
            }
            ([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], csv).into_response()
        }
        None | Some("json") => Json(json!({
            "from": from,
            "to": to,
            "currency": currency,
            "prices_set": !free,
            "statements": statements,
        }))
        .into_response(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("unknown format '{other}' (json, csv)"),
            ));
        }
    })
}

#[derive(Debug, Deserialize)]
pub(super) struct BudgetBody {
    /// Monthly amount in the price list's currency; `null` or 0 removes it.
    #[serde(default)]
    monthly: Option<f64>,
}

/// `PUT /fleet/tenants/{id|me}/budget`: a tenant's monthly budget (warnings only).
pub(super) async fn put_budget(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(b): Json<BudgetBody>,
) -> Result<Json<Value>, AppError> {
    let tenant = keys_owner(&caller, &id)?;
    let monthly = match b.monthly {
        None => None,
        Some(m) if m == 0.0 => None,
        Some(m) if m.is_finite() && m > 0.0 && m <= 1e9 => Some(m),
        Some(_) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "a budget is a positive amount".into(),
            ));
        }
    };
    let mut t = fleet.tenancy.lock().await;
    let rec = t
        .tenants
        .get_mut(&tenant)
        .ok_or_else(|| not_found(format!("no tenant '{tenant}'")))?;
    rec.monthly_budget = monthly;
    let out = rec.public_json();
    fleet.persist_tenancy(&t);
    Ok(Json(out))
}

// ---- API keys -----------------------------------------------------------------------------------------------------

/// The tenant a keys route is about: `me` is the calling tenant; another id needs the operator.
fn keys_owner(caller: &Caller, id: &str) -> Result<String, AppError> {
    match (caller, id) {
        (Caller::Tenant(t), "me") => Ok(t.clone()),
        (Caller::Admin, "me") => Err(AppError(
            StatusCode::BAD_REQUEST,
            "the operator token is not a tenant; use /fleet/tenants/{id}/keys".into(),
        )),
        (_, id) => {
            require_admin(caller)?;
            Ok(id.to_owned())
        }
    }
}

/// Keys are made and revoked with the primary token (or by the operator): a leaked key must not mint more.
fn refuse_named_key(key: &KeyUsed) -> Result<(), AppError> {
    match &key.0 {
        Some(_) => Err(AppError(
            StatusCode::FORBIDDEN,
            "API keys are created and revoked with the tenant's primary token".into(),
        )),
        None => Ok(()),
    }
}

/// `GET /fleet/tenants/{id|me}/keys`.
pub(super) async fn list_keys(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let tenant = keys_owner(&caller, &id)?;
    let t = fleet.tenancy.lock().await;
    let rec = t
        .tenants
        .get(&tenant)
        .ok_or_else(|| not_found(format!("no tenant '{tenant}'")))?;
    let items: Vec<Value> = rec.keys.iter().map(|k| k.public_json()).collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateKeyBody {
    name: String,
    /// Days until it stops working (1 to 3650); omitted never expires.
    #[serde(default)]
    expires_in_days: Option<i64>,
    #[serde(default)]
    read_only: bool,
}

/// `POST /fleet/tenants/{id|me}/keys`: a new named key; its token is in the answer, once.
pub(super) async fn create_key(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Extension(key_used): Extension<KeyUsed>,
    Path(id): Path<String>,
    Json(b): Json<CreateKeyBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    refuse_named_key(&key_used)?;
    let tenant = keys_owner(&caller, &id)?;
    let now = Utc::now();
    let expires_at = match b.expires_in_days {
        None => None,
        Some(d) if (1..=3650).contains(&d) => {
            ChronoDuration::try_days(d).and_then(|d| now.checked_add_signed(d))
        }
        Some(_) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "expires_in_days must be between 1 and 3650".into(),
            ));
        }
    };
    let mut t = fleet.tenancy.lock().await;
    let (key, token) = t
        .create_key(&tenant, &b.name, expires_at, b.read_only, now)
        .map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    tracing::info!(tenant = %tenant, key = %key.name, "API key created");
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "key": key.public_json(),
            "token": token,
            "note": "store this token now; central keeps only its hash and cannot show it again",
        })),
    ))
}

/// `DELETE /fleet/tenants/{id|me}/keys/{key}` (key id or name): the token stops working at once.
pub(super) async fn revoke_key(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Extension(key_used): Extension<KeyUsed>,
    Path((id, key)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    refuse_named_key(&key_used)?;
    let tenant = keys_owner(&caller, &id)?;
    let mut t = fleet.tenancy.lock().await;
    let k = t.revoke_key(&tenant, &key).map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    tracing::info!(tenant = %tenant, key = %k.name, "API key revoked");
    Ok(Json(json!({ "revoked": k.public_json() })))
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
    if fleet.token.is_none() {
        return Err(AppError(
            StatusCode::CONFLICT,
            "central runs without --token: anyone could act as the operator, so tenants are refused; restart it with --token first".into(),
        ));
    }
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
    // With what each is using, so a console can show quota use without one call per tenant.
    let items: Vec<Value> = t
        .tenants
        .keys()
        .filter_map(|id| tenant_view(&t, id))
        .collect();
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
    fleet.notify.keep_deliveries(&t.webhooks);
    fleet.notify.forget_tenant(&id);
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
    // The hourly price is fixed now, from the Mac's size and today's price list.
    let lease = t
        .price_lease(&lease.id, node.vcpus_total, node.memory_mib_total)
        .unwrap_or(lease);
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

// ---- overlays (private networks across Macs) ----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct CreateOverlayBody {
    name: String,
    /// Operator only: the tenant the network belongs to (a tenant's own networks always belong to it).
    #[serde(default)]
    tenant: Option<String>,
    /// Member Macs; a tenant's default is every Mac it leases.
    #[serde(default)]
    nodes: Option<Vec<String>>,
    #[serde(default = "default_hosts_per_node")]
    hosts_per_node: u8,
    #[serde(default = "default_overlay_mtu")]
    mtu: u32,
}

fn default_hosts_per_node() -> u8 {
    30
}

fn default_overlay_mtu() -> u32 {
    crate::tenancy::OVERLAY_DEFAULT_MTU
}

async fn require_registered(fleet: &Fleet, nodes: &[String]) -> Result<(), AppError> {
    let known = fleet.nodes.lock().await;
    match nodes.iter().find(|n| !known.contains_key(*n)) {
        Some(n) => Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("no registered node named '{n}'"),
        )),
        None => Ok(()),
    }
}

/// `POST /fleet/overlays` (*tenant*): a private network spanning Macs. Each member Mac's `fluxvm-agent node` picks it
/// up on its next heartbeat and writes the Mac's `overlay.json`; guests then join it with `apple.networks: [{name}]`.
pub(super) async fn create_overlay(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(b): Json<CreateOverlayBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let tenant = match &caller {
        Caller::Tenant(t) => Some(t.clone()),
        Caller::Admin => b.tenant.clone(),
    };
    let mut t = fleet.tenancy.lock().await;
    let nodes = match (&b.nodes, &tenant) {
        (Some(n), _) => n.clone(),
        (None, Some(tn)) => t
            .leases
            .iter()
            .filter(|l| &l.tenant == tn && l.state == LeaseState::Active)
            .map(|l| l.node.clone())
            .collect(),
        (None, None) => Vec::new(),
    };
    require_registered(&fleet, &nodes).await?;
    let net = t
        .create_overlay(
            &b.name,
            tenant.as_deref(),
            &nodes,
            b.hosts_per_node,
            b.mtu,
            Utc::now(),
        )
        .map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    tracing::info!(network = %net.name, subnet = net.subnet, members = net.members.len(), "overlay created");
    Ok((StatusCode::CREATED, Json(net.public_json())))
}

pub(super) async fn list_overlays(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let t = fleet.tenancy.lock().await;
    let items: Vec<Value> = t
        .overlays
        .values()
        .filter(|o| {
            caller
                .tenant()
                .is_none_or(|c| o.tenant.as_deref() == Some(c))
        })
        .map(|o| o.public_json())
        .collect();
    Json(json!({"items": items}))
}

pub(super) async fn get_overlay(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let t = fleet.tenancy.lock().await;
    let o = t
        .overlay_for(&caller, &name)
        .map_err(|e| not_found(e.to_string()))?;
    Ok(Json(o.public_json()))
}

/// `POST /fleet/overlays/{name}/rotate` (*tenant*): a new network key. Member Macs pick it up within a heartbeat;
/// the previous key keeps working meanwhile.
pub(super) async fn rotate_overlay(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let mut t = fleet.tenancy.lock().await;
    let net = t
        .rotate_overlay(&caller, &name, Utc::now())
        .map_err(|e| match &e {
            crate::tenancy::TenancyError::Overlay(m) if m.starts_with("no network") => {
                not_found(e.to_string())
            }
            _ => tenancy_error(e),
        })?;
    fleet.persist_tenancy(&t);
    tracing::info!(network = %name, epoch = net.epoch, "overlay key rotated");
    Ok(Json(net.public_json()))
}

pub(super) async fn delete_overlay(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut t = fleet.tenancy.lock().await;
    t.delete_overlay(&caller, &name)
        .map_err(|e| not_found(e.to_string()))?;
    fleet.persist_tenancy(&t);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub(super) struct MemberBody {
    node: String,
}

pub(super) async fn add_overlay_member(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    Json(b): Json<MemberBody>,
) -> Result<Json<Value>, AppError> {
    require_registered(&fleet, std::slice::from_ref(&b.node)).await?;
    let mut t = fleet.tenancy.lock().await;
    let net = t
        .add_overlay_member(&caller, &name, &b.node)
        .map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    Ok(Json(net.public_json()))
}

pub(super) async fn remove_overlay_member(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((name, node)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let mut t = fleet.tenancy.lock().await;
    let net = t
        .remove_overlay_member(&caller, &name, &node)
        .map_err(tenancy_error)?;
    fleet.persist_tenancy(&t);
    Ok(Json(net.public_json()))
}

/// The address other Macs reach `n`'s overlay on: the `fluxvm.dev/overlay-ip` label, else the host of its advertised
/// URL.
fn overlay_host(n: &NodeInfo) -> Option<String> {
    if let Some(ip) = n.labels.get("fluxvm.dev/overlay-ip") {
        return Some(ip.clone());
    }
    let rest = n
        .fluxvm_url
        .split_once("://")
        .map_or(n.fluxvm_url.as_str(), |(_, r)| r);
    let authority = rest.split('/').next()?;
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split_once(']').map(|(h, _)| format!("[{h}]"));
    }
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    (!host.is_empty()).then(|| host.to_owned())
}

/// `GET /fleet/nodes/{name}/overlays` (operator token, which node agents use): the node's `overlay.json`, keys
/// included. Peers whose address cannot be worked out are left out (and named in `unresolved`).
pub(super) async fn node_overlays(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    require_admin(&caller)?;
    let t = fleet.tenancy.lock().await;
    let nodes = fleet.nodes.lock().await;
    let mut networks = serde_json::Map::new();
    let mut unresolved = Vec::new();
    for o in t.overlays_of_node(&name) {
        let peers: Vec<String> = o
            .members
            .keys()
            .filter(|m| **m != name)
            .filter_map(|m| match nodes.get(m).and_then(overlay_host) {
                Some(h) => Some(format!("{h}:{}", o.port)),
                None => {
                    unresolved.push(json!({"network": o.name, "node": m}));
                    None
                }
            })
            .collect();
        networks.insert(
            o.name.clone(),
            json!({
                "subnet": o.subnet,
                "hosts": o.members[&name],
                "listen": format!("0.0.0.0:{}", o.port),
                "peers": peers,
                "key": o.key_hex,
                "epoch": o.epoch,
                "previous": o.previous.as_ref().map(|(e, k)| json!({"epoch": e, "key": k})),
                "mtu": o.mtu,
            }),
        );
    }
    Ok(Json(
        json!({"networks": networks, "unresolved": unresolved}),
    ))
}

// ---- exec ---------------------------------------------------------------------------------------------------------

/// `POST /fleet/vms/{node}/{id}/{start|stop|restart|pause|resume}`: power a VM through its node. A tenant can power
/// only its own VMs.
pub(super) async fn vm_power(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id, action)): Path<(String, String, String)>,
) -> Result<Json<Value>, AppError> {
    if !matches!(
        action.as_str(),
        "start" | "stop" | "restart" | "pause" | "resume"
    ) {
        return Err(not_found(format!("no VM action '{action}'")));
    }
    if !fleet.tenancy.lock().await.owns(&caller, &node, &id) {
        return Err(not_found(format!("no VM '{id}' on node '{node}'")));
    }
    let target = node_by_name(&fleet, &node).await?;
    let resp = fleet
        .http
        .post(format!(
            "{}/v1/vms/{}/{action}",
            target.fluxvm_url,
            path_segment(&id)
        ))
        .send()
        .await?;
    let status = resp.status();
    let out: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        Ok(Json(out))
    } else {
        let msg = out
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| out.to_string());
        Err(AppError(
            if status.is_client_error() {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_GATEWAY
            },
            format!("could not {action} the VM: {msg}"),
        ))
    }
}

/// `POST /fleet/vms/{node}/{id}/input`: keyboard and mouse input for the VM's display (`{"action": ...}` or
/// `{"actions": [...]}`, as the node's `POST /v1/vms/{id}/input`). Not written to the activity log one event at a time.
pub(super) async fn vm_input(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, AppError> {
    if !fleet.tenancy.lock().await.owns(&caller, &node, &id) {
        return Err(not_found(format!("no VM '{id}' on node '{node}'")));
    }
    let n = body
        .get("actions")
        .and_then(Value::as_array)
        .map_or(1, Vec::len);
    if n == 0 || n > 200 {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "send 1 to 200 input actions at a time".into(),
        ));
    }
    let target = node_by_name(&fleet, &node).await?;
    let resp = fleet
        .http
        .post(format!(
            "{}/v1/vms/{}/input",
            target.fluxvm_url,
            path_segment(&id)
        ))
        .json(&body)
        .send()
        .await?;
    let status = resp.status();
    let out: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        Ok(Json(out))
    } else {
        Err(AppError(
            if status.is_client_error() {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            },
            format!(
                "the VM did not take the input: {}",
                out.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
            ),
        ))
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct LogsQuery {
    #[serde(default)]
    lines: Option<usize>,
}

/// `GET /fleet/vms/{node}/{id}/logs[?lines=N]`: the end of the VM's console log, as text.
pub(super) async fn vm_logs(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
    Query(q): Query<LogsQuery>,
) -> Result<Response, AppError> {
    if !fleet.tenancy.lock().await.owns(&caller, &node, &id) {
        return Err(not_found(format!("no VM '{id}' on node '{node}'")));
    }
    let lines = q.lines.unwrap_or(200).clamp(1, 5000);
    let target = node_by_name(&fleet, &node).await?;
    let resp = fleet
        .http
        .get(format!(
            "{}/v1/vms/{}/logs?lines={lines}",
            target.fluxvm_url,
            path_segment(&id)
        ))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?;
    let status = resp.status();
    // The node streams; without `follow` (it defaults to off) the stream ends after the last line. Only the last 4 MiB
    // is kept in memory however much the node sends; the timeout bounds how long it may send.
    const KEEP: usize = 4 << 20;
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        buf.extend_from_slice(&chunk);
        if buf.len() > 2 * KEEP {
            buf.drain(..buf.len() - KEEP);
        }
    }
    let start = buf.len().saturating_sub(KEEP);
    let text = String::from_utf8_lossy(&buf[start..]).into_owned();
    if !status.is_success() {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{node}' did not return the log: {}", text.trim()),
        ));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        text,
    )
        .into_response())
}

/// A VM id as one URL path segment (ids are UUIDs; anything else is escaped rather than trusted).
fn path_segment(id: &str) -> String {
    id.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

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
        .post(format!(
            "{}/v1/vms/{}/agent",
            target.fluxvm_url,
            path_segment(&id)
        ))
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
            ha: None,
            audit: Arc::new(super::super::audit::AuditLog::memory()),
            notify: Arc::default(),
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
    async fn lab_mode_refuses_tenants_and_unknown_tokens_once_tenants_exist() {
        let mut fleet = fleet_of(vec![mac("mac-a", 32_000, 0)], TenancyState::default());
        fleet.token = None;
        let app = app(fleet.clone());
        // No fleet token: creating a tenant is refused.
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/tenants",
            "anything",
            json!({"id": "acme"}),
        )
        .await;
        assert_eq!(s, 409, "{v}");
        // Without tenants a stray token is ignored (operator).
        let (s, _) = call(&app, "GET", "/fleet/leases", "stray", json!({})).await;
        assert_eq!(s, 200);
        // Once a tenant exists (e.g. created while a token was set), a stray token is refused.
        fleet
            .tenancy
            .lock()
            .await
            .create_tenant("acme", None, Quota::default(), Utc::now())
            .unwrap();
        let (s, _) = call(&app, "GET", "/fleet/leases", "stray", json!({})).await;
        assert_eq!(s, 401);
    }

    #[tokio::test]
    async fn parallel_creates_cannot_overshoot_the_quota() {
        // A slow node: every create takes 300 ms, so all requests are in flight together.
        let app = Router::new().route(
            "/v1/vms",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                Json(json!({"id": uuid::Uuid::new_v4().to_string(), "name": "x"}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut n = mac("linux-1", 64_000, 0);
        n.apple = None;
        n.vcpus_total = 64;
        n.fluxvm_url = url;
        let mut tenancy = TenancyState::default();
        tenancy
            .create_tenant(
                "acme",
                None,
                Quota {
                    max_vms: Some(3),
                    ..Default::default()
                },
                Utc::now(),
            )
            .unwrap();
        let fleet = fleet_of(vec![n], tenancy);
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let fleet = fleet.clone();
            set.spawn(async move {
                create_vm_inner(
                    &fleet,
                    &Caller::Tenant("acme".into()),
                    json!({"backend": "qemu", "name": "x", "vcpus": 1, "memory_mib": 512}),
                    false,
                )
                .await
                .map(|_| ())
                .map_err(|e| e.0)
            });
        }
        let (mut ok, mut quota) = (0, 0);
        while let Some(r) = set.join_next().await {
            match r.unwrap() {
                Ok(()) => ok += 1,
                Err(code) if code == StatusCode::TOO_MANY_REQUESTS => quota += 1,
                Err(other) => panic!("unexpected {other}"),
            }
        }
        assert_eq!((ok, quota), (3, 5));
        assert_eq!(
            fleet
                .tenancy
                .lock()
                .await
                .vms
                .iter()
                .filter(|v| v.active())
                .count(),
            3
        );
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
    fn only_vz_vms_without_their_own_network_join_the_tenant_network() {
        assert!(wants_tenant_vmnet(&json!({"backend": "vz"})));
        assert!(wants_tenant_vmnet(
            &json!({"backend": "vz", "apple": {"vmnet": null}})
        ));
        assert!(!wants_tenant_vmnet(&json!({"backend": "qemu"})));
        assert!(!wants_tenant_vmnet(
            &json!({"backend": "vz", "network": {"mode": "none"}})
        ));
        assert!(!wants_tenant_vmnet(
            &json!({"backend": "vz", "apple": {"usernet": {}}})
        ));
        assert!(!wants_tenant_vmnet(
            &json!({"backend": "vz", "apple": {"bridge_interface": "en0"}})
        ));
        // A malformed `apple` is left alone (indexing into it would panic).
        assert!(!wants_tenant_vmnet(
            &json!({"backend": "vz", "apple": "oops"})
        ));
        let mut body = json!({"backend": "vz"});
        body["apple"]["vmnet"] = json!({"name": "x"});
        assert_eq!(body["apple"]["vmnet"]["name"], "x");
    }

    #[test]
    fn overlay_hosts_come_from_labels_or_the_advertised_url() {
        let mut n = mac("m", 1, 0);
        n.fluxvm_url = "http://studio-1.local:7788".into();
        assert_eq!(overlay_host(&n).as_deref(), Some("studio-1.local"));
        n.fluxvm_url = "http://[fd00::5]:7788/".into();
        assert_eq!(overlay_host(&n).as_deref(), Some("[fd00::5]"));
        n.fluxvm_url = "http://10.0.0.5".into();
        assert_eq!(overlay_host(&n).as_deref(), Some("10.0.0.5"));
        n.labels
            .insert("fluxvm.dev/overlay-ip".into(), "192.168.50.5".into());
        assert_eq!(overlay_host(&n).as_deref(), Some("192.168.50.5"));
    }

    #[tokio::test]
    async fn tenant_overlays_reach_their_member_macs_with_keys() {
        let mut a = mac("mac-a", 32_000, 0);
        a.fluxvm_url = "http://10.0.0.1:7788".into();
        let mut b = mac("mac-b", 32_000, 0);
        b.fluxvm_url = "http://10.0.0.2:7788".into();
        let fleet = fleet_of(vec![a, b], TenancyState::default());
        let app = Router::new()
            .route("/fleet/tenants", post(create_tenant))
            .route("/fleet/leases", post(create_lease))
            .route("/fleet/overlays", get(list_overlays).post(create_overlay))
            .route("/fleet/nodes/{name}/overlays", get(node_overlays))
            .layer(middleware::from_fn_with_state(
                fleet.clone(),
                auth_middleware,
            ))
            .with_state(fleet.clone());
        let (_, v) = call(
            &app,
            "POST",
            "/fleet/tenants",
            "admin-token",
            json!({"id": "acme"}),
        )
        .await;
        let token = v["token"].as_str().unwrap().to_owned();
        for node in ["mac-a", "mac-b"] {
            let (s, v) = call(&app, "POST", "/fleet/leases", &token, json!({"node": node})).await;
            assert_eq!(s, 201, "{v}");
        }
        // Defaults to every Mac the tenant leases; the key is never in the tenant's view.
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/overlays",
            &token,
            json!({"name": "acme-net"}),
        )
        .await;
        assert_eq!(s, 201, "{v}");
        assert_eq!(v["members"].as_object().unwrap().len(), 2);
        assert!(v.get("key").is_none() && v.get("key_hex").is_none());
        // The node agent's view (operator token): its slice, the other Mac as peer, and the key.
        let (s, v) = call(
            &app,
            "GET",
            "/fleet/nodes/mac-a/overlays",
            "admin-token",
            json!({}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let net = &v["networks"]["acme-net"];
        assert_eq!(
            net["peers"],
            json!([format!("10.0.0.2:{}", crate::tenancy::OVERLAY_PORT_BASE)])
        );
        assert_eq!(net["hosts"], json!([2, 31]));
        assert_eq!(net["key"].as_str().unwrap().len(), 64);
        assert_eq!(net["epoch"], 1);
        assert!(net["previous"].is_null());
        // A tenant cannot read node configs.
        let (s, _) = call(
            &app,
            "GET",
            "/fleet/nodes/mac-a/overlays",
            &token,
            json!({}),
        )
        .await;
        assert_eq!(s, 403);
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

    #[tokio::test]
    async fn image_catalog_and_a_tenants_view_of_its_macs() {
        let fleet = fleet_of(
            vec![mac("mac-a", 32_000, 0), mac("mac-b", 64_000, 0)],
            TenancyState::default(),
        );
        let app = Router::new()
            .route("/fleet/tenants", post(create_tenant))
            .route("/fleet/leases", post(create_lease))
            .route("/fleet/images", get(list_images))
            .route(
                "/fleet/images/{name}",
                axum::routing::put(put_image).delete(delete_image),
            )
            .route("/fleet/nodes/{name}", get(super::get_node))
            .layer(middleware::from_fn_with_state(
                fleet.clone(),
                auth_middleware,
            ))
            .with_state(fleet);

        let xcode =
            json!({"guest_os": "macos", "image": "/Users/fluxvm/vms/xcode/disk.raw", "vcpus": 6});
        let (s, v) = call(
            &app,
            "PUT",
            "/fleet/images/xcode-27",
            "admin-token",
            xcode.clone(),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        assert_eq!(v["memory_mib"], 8192);
        let (s, _) = call(
            &app,
            "PUT",
            "/fleet/images/Bad%20Name",
            "admin-token",
            xcode,
        )
        .await;
        assert_eq!(s, 400);
        let bad = json!({"guest_os": "windows", "image": "/x"});
        let (s, _) = call(&app, "PUT", "/fleet/images/win", "admin-token", bad).await;
        assert_eq!(s, 400);

        let (_, v) = call(
            &app,
            "POST",
            "/fleet/tenants",
            "admin-token",
            json!({"id": "acme"}),
        )
        .await;
        let token = v["token"].as_str().unwrap().to_owned();
        // Tenants read the catalog but cannot change it.
        let (s, v) = call(&app, "GET", "/fleet/images", &token, json!({})).await;
        assert_eq!(s, 200);
        assert_eq!(v["items"][0]["name"], "xcode-27");
        let (s, _) = call(&app, "DELETE", "/fleet/images/xcode-27", &token, json!({})).await;
        assert_eq!(s, 403);

        // A tenant sees a Mac only once it leases it, and without its address.
        let (s, _) = call(&app, "GET", "/fleet/nodes/mac-a", &token, json!({})).await;
        assert_eq!(s, 404);
        let (s, v) = call(&app, "POST", "/fleet/leases", &token, json!({})).await;
        assert_eq!(s, 201, "{v}");
        let (s, v) = call(&app, "GET", "/fleet/nodes/mac-a", &token, json!({})).await;
        assert_eq!(s, 200, "{v}");
        assert!(v.get("fluxvm_url").is_none());
        assert_eq!(v["free_vcpus"], 10);
        let (s, _) = call(&app, "GET", "/fleet/nodes/mac-b", &token, json!({})).await;
        assert_eq!(s, 404);
        // The operator still gets the full record.
        let (_, v) = call(&app, "GET", "/fleet/nodes/mac-b", "admin-token", json!({})).await;
        assert!(v.get("fluxvm_url").is_some());

        let (s, _) = call(
            &app,
            "DELETE",
            "/fleet/images/xcode-27",
            "admin-token",
            json!({}),
        )
        .await;
        assert_eq!(s, 204);
    }

    #[tokio::test]
    async fn api_keys_scopes_and_audit() {
        let fleet = fleet_of(vec![mac("mac-a", 32_000, 0)], TenancyState::default());
        let app = Router::new()
            .route("/fleet/tenants", post(create_tenant))
            .route("/fleet/tenants/me", get(tenant_me))
            .route("/fleet/tenants/{id}/keys", get(list_keys).post(create_key))
            .route(
                "/fleet/tenants/{id}/keys/{key}",
                axum::routing::delete(revoke_key),
            )
            .route("/fleet/leases", get(list_leases).post(create_lease))
            .layer(middleware::from_fn_with_state(
                fleet.clone(),
                auth_middleware,
            ))
            .with_state(fleet.clone());
        let (_, v) = call(
            &app,
            "POST",
            "/fleet/tenants",
            "admin-token",
            json!({"id": "acme"}),
        )
        .await;
        let primary = v["token"].as_str().unwrap().to_owned();

        // The primary token makes keys; the token comes back once and the list never shows it.
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/tenants/me/keys",
            &primary,
            json!({"name": "ci", "expires_in_days": 90}),
        )
        .await;
        assert_eq!(s, 201, "{v}");
        let ci = v["token"].as_str().unwrap().to_owned();
        assert!(v["key"]["expires_at"].is_string());
        let (_, v) = call(
            &app,
            "POST",
            "/fleet/tenants/me/keys",
            &primary,
            json!({"name": "grafana", "read_only": true}),
        )
        .await;
        let ro = v["token"].as_str().unwrap().to_owned();
        let (s, v) = call(&app, "GET", "/fleet/tenants/me/keys", &primary, json!({})).await;
        assert_eq!(s, 200);
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
        assert!(!v.to_string().contains("sha256"));

        // A named key acts as the tenant; a read-only key only reads; keys cannot make keys.
        let (s, v) = call(&app, "GET", "/fleet/tenants/me", &ci, json!({})).await;
        assert_eq!((s, v["id"].as_str()), (200, Some("acme")));
        let (s, _) = call(&app, "GET", "/fleet/leases", &ro, json!({})).await;
        assert_eq!(s, 200);
        let (s, v) = call(&app, "POST", "/fleet/leases", &ro, json!({})).await;
        assert_eq!(s, 403, "{v}");
        let (s, _) = call(
            &app,
            "POST",
            "/fleet/tenants/me/keys",
            &ci,
            json!({"name": "sneaky"}),
        )
        .await;
        assert_eq!(s, 403);
        let (s, _) = call(
            &app,
            "DELETE",
            "/fleet/tenants/me/keys/grafana",
            &ci,
            json!({}),
        )
        .await;
        assert_eq!(s, 403);
        let (s, _) = call(&app, "POST", "/fleet/leases", &ci, json!({})).await;
        assert_eq!(s, 201);

        // The activity log names the key.
        let events = fleet.audit.query(Some("acme"), 50);
        assert!(events.iter().any(|e| e.path == "/fleet/leases"
            && e.status == 201
            && e.key.as_deref() == Some("ci")));
        assert!(
            events
                .iter()
                .any(|e| e.status == 403 && e.key.as_deref() == Some("grafana"))
        );

        // Revoked keys stop working; the operator manages any tenant's keys.
        let (s, _) = call(
            &app,
            "DELETE",
            "/fleet/tenants/me/keys/ci",
            &primary,
            json!({}),
        )
        .await;
        assert_eq!(s, 200);
        let (s, _) = call(&app, "GET", "/fleet/tenants/me", &ci, json!({})).await;
        assert_eq!(s, 401);
        let (s, v) = call(
            &app,
            "GET",
            "/fleet/tenants/acme/keys",
            "admin-token",
            json!({}),
        )
        .await;
        assert_eq!((s, v["items"].as_array().unwrap().len()), (200, 1));
        let (s, _) = call(&app, "GET", "/fleet/tenants/acme/keys", &primary, json!({})).await;
        assert_eq!(s, 403);
    }

    #[tokio::test]
    async fn prices_statements_and_budgets() {
        let fleet = fleet_of(vec![mac("mac-a", 32_768, 0)], TenancyState::default());
        let app = Router::new()
            .route("/fleet/tenants", post(create_tenant))
            .route("/fleet/leases", post(create_lease))
            .route("/fleet/prices", get(get_prices).put(put_prices))
            .route("/fleet/billing", get(billing))
            .route("/fleet/tenants/{id}/budget", axum::routing::put(put_budget))
            .layer(middleware::from_fn_with_state(
                fleet.clone(),
                auth_middleware,
            ))
            .with_state(fleet.clone());
        let (_, v) = call(
            &app,
            "POST",
            "/fleet/tenants",
            "admin-token",
            json!({"id": "acme"}),
        )
        .await;
        let token = v["token"].as_str().unwrap().to_owned();

        // Only the operator sets prices; nonsense is refused; tenants can read them.
        let list = json!({"currency": "EUR", "mac_per_hour": 0.5,
            "mac_classes": [{"name": "mini", "min_vcpus": 8, "per_hour": 1.25}]});
        assert_eq!(
            call(&app, "PUT", "/fleet/prices", &token, list.clone())
                .await
                .0,
            403
        );
        assert_eq!(
            call(
                &app,
                "PUT",
                "/fleet/prices",
                "admin-token",
                json!({"currency": "euro"})
            )
            .await
            .0,
            400
        );
        assert_eq!(
            call(&app, "PUT", "/fleet/prices", "admin-token", list)
                .await
                .0,
            200
        );
        let (s, v) = call(&app, "GET", "/fleet/prices", &token, json!({})).await;
        assert_eq!((s, v["currency"].as_str()), (200, Some("EUR")));

        // A lease takes its class's price when it starts.
        let (s, v) = call(&app, "POST", "/fleet/leases", &token, json!({})).await;
        assert_eq!(s, 201, "{v}");
        assert_eq!(v["lease"]["price_class"], "mini");
        assert_eq!(v["lease"]["rate_micros"], 1_250_000);

        // Statements bill whole seconds, so let the lease accrue one before reading it.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // The tenant sees its statement; a budget shows up on it.
        let (s, _) = call(
            &app,
            "PUT",
            "/fleet/tenants/me/budget",
            &token,
            json!({"monthly": 500}),
        )
        .await;
        assert_eq!(s, 200);
        let (s, v) = call(&app, "GET", "/fleet/billing", &token, json!({})).await;
        assert_eq!(s, 200, "{v}");
        let st = &v["statements"][0];
        assert_eq!(st["tenant"], "acme");
        assert_eq!(st["currency"], "EUR");
        assert!(
            st["lines"][0]["description"]
                .as_str()
                .unwrap()
                .contains("mac-a")
        );
        assert_eq!(st["budget"]["monthly"], 500.0);
        let (s, _) = call(
            &app,
            "GET",
            "/fleet/billing?month=2026-13",
            &token,
            json!({}),
        )
        .await;
        assert_eq!(s, 400);
        let (s, v) = call(
            &app,
            "GET",
            "/fleet/billing?format=csv",
            "admin-token",
            json!({}),
        )
        .await;
        assert_eq!(s, 200);
        assert!(
            v.as_str()
                .unwrap()
                .starts_with("tenant,from,to,description"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn webhooks_get_signed_events() {
        // A receiver that keeps what it gets.
        let got: Arc<Mutex<Vec<(String, String, Vec<u8>)>>> = Arc::default();
        let sink = got.clone();
        let receiver = Router::new().route(
            "/hook",
            post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                    let sink = sink.clone();
                    async move {
                        let h = |k: &str| {
                            headers
                                .get(k)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_owned()
                        };
                        sink.lock().await.push((
                            h("x-fluxvm-event"),
                            h("x-fluxvm-signature"),
                            body.to_vec(),
                        ));
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hook_url = format!("http://{}/hook", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, receiver).await.unwrap() });

        let fleet = fleet_of(vec![mac("mac-a", 32_768, 0)], TenancyState::default());
        let app = Router::new()
            .route("/fleet/tenants", post(create_tenant))
            .route("/fleet/leases", post(create_lease))
            .route(
                "/fleet/webhooks",
                get(super::super::notify::list_webhooks).post(super::super::notify::create_webhook),
            )
            .route(
                "/fleet/webhooks/{id}/test",
                post(super::super::notify::test_webhook),
            )
            .route(
                "/fleet/notifications",
                get(super::super::notify::notifications),
            )
            .layer(middleware::from_fn_with_state(
                fleet.clone(),
                auth_middleware,
            ))
            .with_state(fleet.clone());
        let (_, v) = call(
            &app,
            "POST",
            "/fleet/tenants",
            "admin-token",
            json!({"id": "acme"}),
        )
        .await;
        let token = v["token"].as_str().unwrap().to_owned();

        // A tenant may not point a webhook at a private address or plain http.
        for bad in [
            hook_url.as_str(),
            "https://10.0.0.1/x",
            "https://127.0.0.1/x",
        ] {
            let (s, v) = call(&app, "POST", "/fleet/webhooks", &token, json!({"url": bad})).await;
            assert_eq!(s, 400, "{bad}: {v}");
        }
        let (s, _) = call(
            &app,
            "POST",
            "/fleet/webhooks",
            &token,
            json!({"url": "https://example.com/x", "events": ["node.down"]}),
        )
        .await;
        assert_eq!(s, 400, "operator events are not for tenants");

        // The operator's hook (internal URLs allowed) gets the tenant's lease, signed.
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/webhooks",
            "admin-token",
            json!({"url": hook_url, "events": ["lease.created", "test"]}),
        )
        .await;
        assert_eq!(s, 201, "{v}");
        let secret = v["secret"].as_str().unwrap().to_owned();
        let id = v["webhook"]["id"].as_str().unwrap().to_owned();
        let (s, _) = call(&app, "POST", "/fleet/leases", &token, json!({})).await;
        assert_eq!(s, 201);
        let mut delivered = false;
        for _ in 0..100 {
            if !got.lock().await.is_empty() {
                delivered = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(delivered, "no delivery");
        let (kind, sig, body) = got.lock().await[0].clone();
        assert_eq!(kind, "lease.created");
        assert!(crate::gha::verify_signature(secret.as_bytes(), &body, &sig));
        let event: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(event["tenant"], "acme");

        // A test delivery reports its result; the tenant sees the notification but not the operator's hook.
        let (s, v) = call(
            &app,
            "POST",
            &format!("/fleet/webhooks/{id}/test"),
            "admin-token",
            json!({}),
        )
        .await;
        assert_eq!((s, v["delivery"]["ok"].as_bool()), (200, Some(true)), "{v}");
        let (_, v) = call(&app, "GET", "/fleet/webhooks", &token, json!({})).await;
        assert!(v["items"].as_array().unwrap().is_empty());
        let (_, v) = call(&app, "GET", "/fleet/notifications", &token, json!({})).await;
        assert_eq!(v["items"][0]["type"], "lease.created");
    }
}
