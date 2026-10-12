// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! VM snapshots and clones through central.
//!
//! | Route | What |
//! |---|---|
//! | `GET /fleet/vms/{node}/{id}/snapshots` (*tenant*) | the VM's snapshots, newest first, with the per-VM limit |
//! | `POST /fleet/vms/{node}/{id}/snapshots` (*tenant*) | take one: `{"name": "...", "include_memory": false}` |
//! | `DELETE /fleet/vms/{node}/{id}/snapshots/{name}` (*tenant*) | delete one |
//! | `POST /fleet/vms/{node}/{id}/snapshots/{name}/restore` (*tenant*) | put the VM back to it (stop, swap, start) |
//! | `POST /fleet/vms/{node}/{id}/clone` (*tenant*) | a new VM on the same Mac: `{"name": "...", "snapshot": "..."}` |
//!
//! A snapshot is taken by the VM's node: an APFS clone of the disk and firmware files, plus, with `include_memory`,
//! the guest's saved memory and device state (Virtualization.framework `saveMachineStateTo`, macOS 14+). A disk-only
//! snapshot works in any state (a running guest is paused for the copy); one with memory needs a running or paused
//! VM. Restoring stops the VM, puts the snapshot's files back and starts it, resuming the saved memory when there is
//! some. A clone is a new VM made from a snapshot's disk, or from a stopped VM's own disk; it lands on the same Mac,
//! belongs to the source VM's tenant and counts against that tenant's quota.
//!
//! Central keeps a ledger entry per snapshot ([`crate::tenancy::SnapshotEntry`]) for the per-VM limit
//! ([`crate::tenancy::Policy::max_snapshots_per_vm`], overridable per tenant) and for storage billing; listing a VM's
//! snapshots brings the ledger in line with what the node has.

use super::*;
use crate::tenancy::{SnapshotEntry, valid_snapshot_name};
use std::collections::BTreeMap;
use std::time::Duration;

/// Saving memory, restoring and cloning can take a while on a big guest.
const SLOW: Duration = Duration::from_secs(600);
const QUICK: Duration = Duration::from_secs(30);

fn not_found(what: String) -> AppError {
    AppError(StatusCode::NOT_FOUND, what)
}

fn bad_request(what: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_REQUEST, what.into())
}

/// After the ownership check: the VM's tenant per the ledger (`None` for an operator's VM, or one central did not
/// create). Someone else's VM is a 404, so a tenant learns nothing about it.
async fn owned(
    fleet: &Fleet,
    caller: &Caller,
    node: &str,
    id: &str,
) -> Result<Option<String>, AppError> {
    let t = fleet.tenancy.lock().await;
    if !t.owns(caller, node, id) {
        return Err(not_found(format!("no VM '{id}' on node '{node}'")));
    }
    Ok(t.vm(node, id).and_then(|v| v.tenant.clone()))
}

async fn node_info(fleet: &Fleet, name: &str) -> Result<NodeInfo, AppError> {
    fleet
        .nodes
        .lock()
        .await
        .get(name)
        .cloned()
        .ok_or_else(|| not_found(format!("no registered node named '{name}'")))
}

/// One escaped URL path segment (VM ids are UUIDs, snapshot names are checked; anything else is escaped).
fn seg(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Calls the node's own API. Unreachable is a 502; otherwise the node's status and JSON body (or `null`).
async fn node_call(
    fleet: &Fleet,
    node: &NodeInfo,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
    timeout: Duration,
) -> Result<(reqwest::StatusCode, Value), AppError> {
    let mut req = fleet
        .http
        .request(method, format!("{}{path}", node.fluxvm_url))
        .timeout(timeout);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.map_err(|e| {
        AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{}' did not answer: {e}", node.name),
        )
    })?;
    let status = resp.status();
    let out: Value = resp.json().await.unwrap_or(Value::Null);
    Ok((status, out))
}

/// The node's refusal as central's error: its 4xx is the caller's problem (409, or 404), anything else is a 502.
fn refused(what: &str, status: reqwest::StatusCode, body: &Value) -> AppError {
    let msg = body
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("the node answered {status}"));
    let code = if status == reqwest::StatusCode::NOT_FOUND || msg.contains("not found") {
        StatusCode::NOT_FOUND
    } else if status.is_client_error() || status == reqwest::StatusCode::INTERNAL_SERVER_ERROR {
        // The node's API answers 500 for most refusals (wrong state, unsupported guest); they are not central's fault.
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_GATEWAY
    };
    AppError(code, format!("{what}: {msg}"))
}

fn check_name(name: &str) -> Result<(), AppError> {
    if valid_snapshot_name(name) {
        Ok(())
    } else {
        Err(bad_request(format!(
            "invalid snapshot name '{name}': use 1-64 letters, digits, '.', '_' and '-', starting with a letter or digit"
        )))
    }
}

/// A VM name for a clone: 1-63 of `[A-Za-z0-9._-]`, starting with a letter or digit (the node checks it again).
fn valid_vm_name(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

/// The snapshot name in a `…/snapshots/{name}[/restore]` path, for the activity log's target.
pub(super) fn path_target(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["fleet", "vms", _, _, "snapshots", name]
        | ["fleet", "vms", _, _, "snapshots", name, "restore"] => {
            Some(name.chars().take(64).filter(|c| !c.is_control()).collect())
        }
        _ => None,
    }
}

/// Title and link of the notification for a snapshot, restore or clone the activity log just recorded.
pub(super) async fn notice(
    fleet: &Fleet,
    seg: &[&str],
    target: &str,
) -> Option<(&'static str, String, Option<String>)> {
    let name_of = |node: &str, id: &str| {
        let fleet = fleet.clone();
        let (node, id) = (node.to_owned(), id.to_owned());
        async move {
            fleet
                .tenancy
                .lock()
                .await
                .vm(&node, &id)
                .map(|v| v.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("A VM on {node}"))
        }
    };
    let target = if target.is_empty() {
        String::new()
    } else {
        format!(" {target}")
    };
    match seg {
        ["fleet", "vms", node, id, "snapshots"] => Some((
            "vm.snapshot",
            format!("{}: snapshot{target} taken", name_of(*node, *id).await),
            Some(format!("#/vms/{node}/{id}")),
        )),
        ["fleet", "vms", node, id, "snapshots", snap, "restore"] => Some((
            "vm.restored",
            format!(
                "{} was restored to snapshot {snap}",
                name_of(*node, *id).await
            ),
            Some(format!("#/vms/{node}/{id}")),
        )),
        ["fleet", "vms", node, id, "clone"] => Some((
            "vm.cloned",
            format!("VM{target} cloned from {}", name_of(*node, *id).await),
            Some("#/vms".to_owned()),
        )),
        _ => None,
    }
}

fn entry_json(e: &SnapshotEntry) -> Value {
    json!({
        "name": e.name,
        "created_at": e.created_at,
        "size_bytes": e.size_bytes,
        "memory": e.memory,
    })
}

/// The node's snapshot list of one VM, as `name -> item`.
async fn node_snapshots(
    fleet: &Fleet,
    target: &NodeInfo,
    id: &str,
) -> Result<BTreeMap<String, Value>, AppError> {
    let (status, body) = node_call(
        fleet,
        target,
        reqwest::Method::GET,
        &format!("/v1/vms/{}/snapshots", seg(id)),
        None,
        QUICK,
    )
    .await?;
    if !status.is_success() {
        return Err(refused("could not list the snapshots", status, &body));
    }
    Ok(body
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|it| {
            let tag = it.get("tag").and_then(Value::as_str)?;
            Some((tag.to_owned(), it.clone()))
        })
        .collect())
}

/// Brings the ledger in line with the node's list; returns the VM's live entries by name.
async fn sync(
    fleet: &Fleet,
    node: &str,
    id: &str,
    listed: &BTreeMap<String, Value>,
) -> BTreeMap<String, SnapshotEntry> {
    let present: BTreeMap<String, u64> = listed
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
            )
        })
        .collect();
    let mut t = fleet.tenancy.lock().await;
    if t.sync_snapshots(node, id, &present, Utc::now()) {
        fleet.persist_tenancy(&t);
    }
    t.snapshots_of(node, id)
        .into_iter()
        .map(|e| (e.name.clone(), e.clone()))
        .collect()
}

/// `GET /fleet/vms/{node}/{id}/snapshots`: newest first. Snapshots taken on the Mac directly are listed too.
pub(super) async fn list(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let tenant = owned(&fleet, &caller, &node, &id).await?;
    let target = node_info(&fleet, &node).await?;
    let listed = node_snapshots(&fleet, &target, &id).await?;
    let ledger = sync(&fleet, &node, &id, &listed).await;
    let limit = fleet.tenancy.lock().await.snapshot_limit(tenant.as_deref());
    let mut items: Vec<Value> = listed
        .iter()
        .map(|(name, it)| {
            let e = ledger.get(name);
            json!({
                "name": name,
                "created_at": e.map(|e| json!(e.created_at)).unwrap_or_else(|| it.get("created_at").cloned().unwrap_or(Value::Null)),
                "size_bytes": it.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
                "memory": it.get("has_memory").and_then(Value::as_bool).or(e.map(|e| e.memory)).unwrap_or(false),
            })
        })
        .collect();
    items.sort_by(|a, b| {
        b["created_at"]
            .as_str()
            .unwrap_or("")
            .cmp(a["created_at"].as_str().unwrap_or(""))
    });
    Ok(Json(json!({
        "node": node,
        "vm": id,
        "items": items,
        "limit": limit,
    })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SnapshotBody {
    #[serde(default)]
    name: Option<String>,
    /// Save the guest's memory and device state too (needs a running or paused VM).
    #[serde(default)]
    include_memory: bool,
}

/// `POST /fleet/vms/{node}/{id}/snapshots` `{"name": "...", "include_memory": false}` (name defaults to
/// `snap-<UTC time>`). 201 with the snapshot.
pub(super) async fn create(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
    raw: axum::body::Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    // An empty body takes every default.
    let body: SnapshotBody = if raw.iter().all(u8::is_ascii_whitespace) {
        SnapshotBody::default()
    } else {
        serde_json::from_slice(&raw)
            .map_err(|e| bad_request(format!("bad snapshot request: {e}")))?
    };
    let tenant = owned(&fleet, &caller, &node, &id).await?;
    let name = body
        .name
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("snap-{}", Utc::now().format("%Y%m%d-%H%M%S")));
    check_name(&name)?;
    let target = node_info(&fleet, &node).await?;
    let memory = body.include_memory;
    fleet
        .tenancy
        .lock()
        .await
        .begin_snapshot(&node, &id, &name, tenant, memory, Utc::now())
        .map_err(tenancy_error)?;
    // Until the node answers, the entry holds the VM's slot; it is dropped if the node refuses (or this request is
    // cancelled before then).
    let mut hold = PendingSnapshot {
        fleet: &fleet,
        key: Some((node.clone(), id.clone(), name.clone())),
    };
    let (status, out) = node_call(
        &fleet,
        &target,
        reqwest::Method::POST,
        &format!("/v1/vms/{}/snapshot", seg(&id)),
        Some(json!({"tag": name, "memory": memory})),
        SLOW,
    )
    .await?;
    if !status.is_success() {
        return Err(refused("could not take the snapshot", status, &out));
    }
    hold.key = None;
    // The node knows the size once the files are there.
    let size = node_snapshots(&fleet, &target, &id)
        .await
        .ok()
        .and_then(|l| {
            l.get(&name)
                .and_then(|v| v.get("size_bytes"))
                .and_then(Value::as_u64)
        })
        .unwrap_or(0);
    let mut t = fleet.tenancy.lock().await;
    let mut made = None;
    if let Some(e) = t
        .snapshots
        .iter_mut()
        .rev()
        .find(|e| e.node == node && e.vm_id == id && e.name == name && e.deleted_at.is_none())
    {
        e.size_bytes = size;
        made = Some(entry_json(e));
    }
    fleet.persist_tenancy(&t);
    drop(t);
    let snap = made.unwrap_or_else(|| json!({"name": name, "memory": memory, "size_bytes": size}));
    Ok((
        StatusCode::CREATED,
        Json(json!({"node": node, "vm": id, "snapshot": snap})),
    ))
}

/// The ledger entry of a snapshot being taken; dropped unless the node took it.
struct PendingSnapshot<'a> {
    fleet: &'a Fleet,
    key: Option<(String, String, String)>,
}

impl Drop for PendingSnapshot<'_> {
    fn drop(&mut self) {
        let Some((node, id, name)) = self.key.take() else {
            return;
        };
        if let Ok(mut t) = self.fleet.tenancy.try_lock() {
            t.forget_snapshot(&node, &id, &name);
            return;
        }
        let tenancy = self.fleet.tenancy.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                tenancy.lock().await.forget_snapshot(&node, &id, &name);
            });
        }
    }
}

/// `DELETE /fleet/vms/{node}/{id}/snapshots/{name}`: 204.
pub(super) async fn delete(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id, name)): Path<(String, String, String)>,
) -> Result<StatusCode, AppError> {
    owned(&fleet, &caller, &node, &id).await?;
    check_name(&name)?;
    let target = node_info(&fleet, &node).await?;
    let (status, out) = node_call(
        &fleet,
        &target,
        reqwest::Method::DELETE,
        &format!("/v1/vms/{}/snapshots/{}", seg(&id), seg(&name)),
        None,
        QUICK,
    )
    .await?;
    let mut t = fleet.tenancy.lock().await;
    let had = t.end_snapshot(&node, &id, &name, Utc::now());
    if had {
        fleet.persist_tenancy(&t);
    }
    drop(t);
    if status.is_success() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let err = refused("could not delete the snapshot", status, &out);
    // Already gone from the Mac: done, as far as the ledger is concerned.
    if err.0 == StatusCode::NOT_FOUND && had {
        return Ok(StatusCode::NO_CONTENT);
    }
    Err(err)
}

/// `POST /fleet/vms/{node}/{id}/snapshots/{name}/restore`: stops the VM, puts the snapshot back and starts it.
pub(super) async fn restore(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id, name)): Path<(String, String, String)>,
) -> Result<Json<Value>, AppError> {
    owned(&fleet, &caller, &node, &id).await?;
    check_name(&name)?;
    let target = node_info(&fleet, &node).await?;
    let (status, out) = node_call(
        &fleet,
        &target,
        reqwest::Method::POST,
        &format!("/v1/vms/{}/restore", seg(&id)),
        Some(json!({"tag": name})),
        SLOW,
    )
    .await?;
    if !status.is_success() {
        return Err(refused("could not restore the snapshot", status, &out));
    }
    Ok(Json(json!({"node": node, "vm": out, "snapshot": name})))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CloneBody {
    name: String,
    /// Clone this snapshot's disk; without it the VM must be stopped.
    #[serde(default)]
    snapshot: Option<String>,
}

/// `POST /fleet/vms/{node}/{id}/clone` `{"name": "...", "snapshot": "..."}`: a new VM on the same Mac, owned by the
/// source VM's tenant and counted against its quota. 201 `{"node", "vm"}` like a create.
pub(super) async fn clone_vm(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path((node, id)): Path<(String, String)>,
    Json(body): Json<CloneBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let tenant = owned(&fleet, &caller, &node, &id).await?;
    let name = body.name.trim().to_owned();
    if !valid_vm_name(&name) {
        return Err(bad_request(format!(
            "invalid VM name '{name}': use 1-63 letters, digits, '.', '_' and '-', starting with a letter or digit"
        )));
    }
    if let Some(s) = &body.snapshot {
        check_name(s)?;
    }
    let target = node_info(&fleet, &node).await?;
    // The clone is the source's size; the ledger knows it for VMs central made, the node for the rest.
    let known = fleet
        .tenancy
        .lock()
        .await
        .vm(&node, &id)
        .map(|v| (v.vcpus, v.memory_mib, v.macos_guest));
    let (vcpus, memory_mib, macos_guest) = match known {
        Some(k) => k,
        None => {
            let (status, rec) = node_call(
                &fleet,
                &target,
                reqwest::Method::GET,
                &format!("/v1/vms/{}", seg(&id)),
                None,
                QUICK,
            )
            .await?;
            if !status.is_success() {
                return Err(refused("could not read the VM", status, &rec));
            }
            let req = rec.get("request").cloned().unwrap_or(Value::Null);
            let (c, m) = request_sizes(&req);
            let macos = req
                .get("apple")
                .and_then(|a| a.get("guest_os"))
                .and_then(Value::as_str)
                == Some("macos");
            (c, m, macos)
        }
    };
    // Quota and placement rules apply as for a create on that node: a tenant may not clone onto a cordoned or
    // draining Mac, nor onto one leased to someone else.
    let reservation = {
        let mut t = fleet.tenancy.lock().await;
        if caller != Caller::Admin {
            let cordoned = fleet
                .nodes
                .lock()
                .await
                .get(&node)
                .is_some_and(|n| n.cordoned);
            if cordoned || t.draining.contains(&node) {
                return Err(AppError(
                    StatusCode::CONFLICT,
                    format!("node '{node}' is cordoned or draining: it takes no new VMs"),
                ));
            }
        }
        match &tenant {
            Some(tn) if t.enabled() && t.tenants.contains_key(tn) => {
                t.admit(&node, &Caller::Tenant(tn.clone()), macos_guest, true)
                    .map_err(tenancy_error)?;
                Some(t.reserve(tn, vcpus, memory_mib).map_err(tenancy_error)?)
            }
            _ => None,
        }
    };
    let mut hold = ReservationHold {
        fleet: &fleet,
        id: reservation,
    };
    let (status, record) = node_call(
        &fleet,
        &target,
        reqwest::Method::POST,
        &format!("/v1/vms/{}/clone", seg(&id)),
        Some(json!({"name": name, "snapshot": body.snapshot})),
        SLOW,
    )
    .await?;
    if !status.is_success() {
        return Err(refused("could not clone the VM", status, &record));
    }
    record_created_vm(
        &fleet,
        &node,
        &record,
        tenant,
        vcpus,
        memory_mib,
        macos_guest,
        None,
    )
    .await;
    if let Some(r) = hold.id.take() {
        fleet.tenancy.lock().await.release_reservation(r);
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"node": node, "vm": record})),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert!(valid_vm_name("build-2.copy_1"));
        assert!(!valid_vm_name("-x"));
        assert!(!valid_vm_name(&"x".repeat(64)));
        assert!(!valid_vm_name("a/b"));
        assert_eq!(seg("a b/c"), "a%20b%2Fc");
        assert_eq!(
            path_target("/fleet/vms/mac/abc/snapshots/before-upgrade/restore").as_deref(),
            Some("before-upgrade")
        );
        assert_eq!(
            path_target("/fleet/vms/mac/abc/snapshots/s1").as_deref(),
            Some("s1")
        );
        assert_eq!(path_target("/fleet/vms/mac/abc/snapshots"), None);
    }

    #[test]
    fn tenants_reach_the_snapshot_routes_with_the_right_methods() {
        use axum::http::Method;
        let vm = "/fleet/vms/mac/0b9c";
        for (m, p) in [
            (Method::GET, format!("{vm}/snapshots")),
            (Method::POST, format!("{vm}/snapshots")),
            (Method::DELETE, format!("{vm}/snapshots/s1")),
            (Method::POST, format!("{vm}/snapshots/s1/restore")),
            (Method::POST, format!("{vm}/clone")),
        ] {
            assert!(tenant_route_allowed(&m, &p), "{m} {p}");
        }
        for (m, p) in [
            (Method::DELETE, format!("{vm}/snapshots")),
            (Method::GET, format!("{vm}/snapshots/s1/restore")),
            (Method::PUT, format!("{vm}/snapshots/s1")),
            (Method::GET, format!("{vm}/clone")),
        ] {
            assert!(!tenant_route_allowed(&m, &p), "{m} {p}");
        }
    }

    #[test]
    fn node_refusals_map_to_caller_errors() {
        let e = refused(
            "x",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": "snapshot requires a running or paused VM"}),
        );
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert!(e.1.contains("running or paused"), "{}", e.1);
        let e = refused(
            "x",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"error": "snapshot \"s\" not found for VM 1"}),
        );
        assert_eq!(e.0, StatusCode::NOT_FOUND);
        let e = refused("x", reqwest::StatusCode::BAD_GATEWAY, &Value::Null);
        assert_eq!(e.0, StatusCode::BAD_GATEWAY);
    }
}
