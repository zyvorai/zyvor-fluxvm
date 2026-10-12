// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `/fleet/volumes`: persistent volumes, block disks that outlive VMs. Rules live in [`crate::volumes`]; this module is
//! HTTP and node I/O. A volume is a file on one Mac, made by that Mac's `fluxctl serve` (`/v1/volumes`).
//!
//! | Route | What |
//! |---|---|
//! | `GET /fleet/volumes` (*tenant*: own) | list; the operator can filter with `?tenant=` |
//! | `POST /fleet/volumes` (*tenant*) | `{"name", "size_gib", "node"?}` (operator: `"tenant"?`): create |
//! | `GET /fleet/volumes/{name}` (*tenant*) | one volume, by name or id |
//! | `DELETE /fleet/volumes/{name}` (*tenant*) | delete; 409 while attached |
//! | `POST /fleet/volumes/{name}/resize` (*tenant*) | `{"size_gib"}`: grow a detached volume |
//!
//! A launch attaches volumes with `"volumes": ["name", …]` on `POST /fleet/vms` (see [`claim_for_launch`]); deleting the
//! VM detaches them.

use super::*;
use crate::volumes::{MAX_VOLUMES_PER_VM, Volume, VolumeError};
use axum::extract::Query;

fn vol_error(e: VolumeError) -> AppError {
    AppError(
        StatusCode::from_u16(e.0).unwrap_or(StatusCode::CONFLICT),
        e.1,
    )
}

fn volume_json(v: &Volume, caller: &Caller) -> Value {
    let mut out = v.public_json();
    if *caller == Caller::Admin {
        out["path"] = json!(v.path);
    }
    out
}

/// A node's answer turned into an error for the caller: unreachable, too old for volumes, or what it said.
async fn node_failure(node: &str, resp: reqwest::Response) -> AppError {
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let said = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("no reason given")
        .to_owned();
    match status {
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED if !said.contains("volume") => {
            AppError(
                StatusCode::NOT_IMPLEMENTED,
                format!(
                    "node '{node}' does not support volumes: its fluxctl serve predates them; update it"
                ),
            )
        }
        StatusCode::NOT_FOUND => AppError(StatusCode::NOT_FOUND, format!("node '{node}': {said}")),
        StatusCode::CONFLICT => AppError(StatusCode::CONFLICT, format!("node '{node}': {said}")),
        StatusCode::BAD_REQUEST => {
            AppError(StatusCode::BAD_REQUEST, format!("node '{node}': {said}"))
        }
        _ => AppError(
            StatusCode::BAD_GATEWAY,
            format!("node '{node}' refused ({status}): {said}"),
        ),
    }
}

fn unreachable(node: &str, e: reqwest::Error) -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        format!("node '{node}' unreachable: {e}"),
    )
}

/// Marks a volume busy (a delete or resize is talking to its Mac) so no launch claims it meanwhile.
fn hold(t: &mut TenancyState, id: &str) {
    if let Some(v) = t.volumes.iter_mut().find(|v| v.id == id) {
        v.claimed = true;
    }
}

// ---- list and show ----------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub(super) struct VolumeQuery {
    /// Operator only: one tenant's volumes.
    #[serde(default)]
    tenant: Option<String>,
}

pub(super) async fn list_volumes(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<VolumeQuery>,
) -> Json<Value> {
    let t = fleet.tenancy.lock().await;
    let mut list = t.volumes_for(&caller);
    if caller == Caller::Admin
        && let Some(want) = q.tenant.as_deref().filter(|s| !s.is_empty())
    {
        list.retain(|v| v.tenant.as_deref() == Some(want));
    }
    list.sort_by(|a, b| (&a.tenant, &a.name).cmp(&(&b.tenant, &b.name)));
    let items: Vec<Value> = list.iter().map(|v| volume_json(v, &caller)).collect();
    Json(json!({"items": items}))
}

pub(super) async fn get_volume(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
) -> Result<Json<Value>, AppError> {
    let t = fleet.tenancy.lock().await;
    let v = t.find_volume(&caller, &key).map_err(vol_error)?;
    Ok(Json(volume_json(v, &caller)))
}

// ---- create -----------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct CreateVolumeBody {
    name: String,
    size_gib: u64,
    /// The Mac to put it on; default: one the tenant leases, else the healthy Mac with the most free memory.
    #[serde(default)]
    node: Option<String>,
    /// Operator only: the tenant that owns it (a tenant's volumes are always its own).
    #[serde(default)]
    tenant: Option<String>,
}

/// Where a new volume of `owner` goes when the request names no Mac: a healthy, uncordoned, undrained Mac the owner
/// may place VMs on, preferring the owner's leased Macs, then the most free memory.
fn pick_volume_node(
    t: &TenancyState,
    nodes: &HashMap<String, NodeInfo>,
    owner: &Caller,
) -> Result<String, AppError> {
    let leased_to_owner = |n: &str| {
        owner
            .tenant()
            .is_some_and(|o| t.active_lease_for_node(n).is_some_and(|l| l.tenant == o))
    };
    nodes
        .values()
        .filter(|n| n.healthy() && !n.cordoned && n.apple.is_some())
        .filter(|n| !t.draining.contains(&n.name))
        .filter(|n| t.admit(&n.name, owner, false, false).is_ok())
        .max_by(|a, b| {
            leased_to_owner(&a.name)
                .cmp(&leased_to_owner(&b.name))
                .then(a.free_memory_mib().cmp(&b.free_memory_mib()))
                .then(b.name.cmp(&a.name))
        })
        .map(|n| n.name.clone())
        .ok_or_else(|| {
            AppError(
                StatusCode::SERVICE_UNAVAILABLE,
                "no Mac can hold a volume right now: none is healthy, uncordoned and open to you"
                    .into(),
            )
        })
}

/// Checks a Mac a create names: registered and answering, and for a tenant also open to it (not cordoned, draining,
/// or leased to someone else).
fn check_named_node(
    t: &TenancyState,
    nodes: &HashMap<String, NodeInfo>,
    caller: &Caller,
    owner: &Caller,
    name: &str,
) -> Result<String, AppError> {
    let Some(n) = nodes.get(name) else {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("no registered node named '{name}'"),
        ));
    };
    if !n.healthy() {
        return Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("node '{name}' is not responding"),
        ));
    }
    if *caller != Caller::Admin {
        if n.cordoned || t.draining.contains(name) {
            return Err(AppError(
                StatusCode::CONFLICT,
                format!("node '{name}' is cordoned or draining"),
            ));
        }
        t.admit(name, owner, false, true).map_err(tenancy_error)?;
    }
    Ok(name.to_owned())
}

/// `POST /fleet/volumes`.
pub(super) async fn create_volume(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Json(b): Json<CreateVolumeBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let owner_id = match &caller {
        Caller::Tenant(t) => Some(t.clone()),
        Caller::Admin => b.tenant.clone().filter(|t| !t.is_empty()),
    };
    let owner = owner_id.clone().map_or(Caller::Admin, Caller::Tenant);
    let (volume, target) = {
        let mut t = fleet.tenancy.lock().await;
        let nodes = fleet.nodes.lock().await;
        let node = match &b.node {
            Some(n) => check_named_node(&t, &nodes, &caller, &owner, n)?,
            None => pick_volume_node(&t, &nodes, &owner)?,
        };
        let v = t
            .begin_volume(owner_id.as_deref(), &b.name, b.size_gib, &node, Utc::now())
            .map_err(vol_error)?;
        fleet.persist_tenancy(&t);
        let target = nodes.get(&node).cloned().expect("checked above");
        (v, target)
    };
    // In its own task, so a client that hangs up mid-request does not leave the record half made.
    let f = fleet.clone();
    let task = tokio::spawn(async move {
        let sent = f
            .http
            .post(format!("{}/v1/volumes", target.fluxvm_url))
            .json(&json!({"name": volume.id, "size_gib": volume.size_gib}))
            .send()
            .await;
        let made: Result<String, AppError> = match sent {
            Err(e) => Err(unreachable(&target.name, e)),
            Ok(resp) if resp.status().is_success() => {
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                body.get("path")
                    .and_then(Value::as_str)
                    .filter(|p| p.starts_with('/'))
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        AppError(
                            StatusCode::BAD_GATEWAY,
                            format!(
                                "node '{}' made the volume but did not say where",
                                target.name
                            ),
                        )
                    })
            }
            Ok(resp) => Err(node_failure(&target.name, resp).await),
        };
        let mut t = f.tenancy.lock().await;
        let out = match made {
            Ok(path) => t
                .finish_volume(&volume.id, path)
                .ok_or_else(|| AppError(StatusCode::CONFLICT, "the volume vanished".into())),
            Err(e) => {
                t.abort_volume(&volume.id);
                Err(e)
            }
        };
        f.persist_tenancy(&t);
        out
    });
    let v = task
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    tracing::info!(volume = %v.id, name = %v.name, node = %v.node, size_gib = v.size_gib, "volume created");
    Ok((StatusCode::CREATED, Json(volume_json(&v, &caller))))
}

// ---- delete and resize ------------------------------------------------------------------------------------------

/// `DELETE /fleet/volumes/{name}`: refused while attached. The file on the Mac goes with it.
pub(super) async fn delete_volume(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
) -> Result<StatusCode, AppError> {
    let (v, target) = {
        let mut t = fleet.tenancy.lock().await;
        let v = t.volume_to_delete(&caller, &key).map_err(vol_error)?;
        let target = fleet.nodes.lock().await.get(&v.node).cloned();
        hold(&mut t, &v.id);
        (v, target)
    };
    let f = fleet.clone();
    let task = tokio::spawn(async move {
        let done: Result<(), AppError> = match target {
            // A Mac that was deregistered took the file with it.
            None => Ok(()),
            Some(n) => match f
                .http
                .delete(format!("{}/v1/volumes/{}", n.fluxvm_url, v.id))
                .send()
                .await
            {
                Err(e) => Err(unreachable(&n.name, e)),
                Ok(resp) if resp.status().is_success() => Ok(()),
                // Already gone there (or a record left by a create that never finished).
                Ok(resp) if resp.status() == StatusCode::NOT_FOUND && v.creating => Ok(()),
                Ok(resp) => match node_failure(&n.name, resp).await {
                    AppError(StatusCode::NOT_FOUND, _) => Ok(()),
                    e => Err(e),
                },
            },
        };
        let mut t = f.tenancy.lock().await;
        match done {
            Ok(()) => {
                t.mark_volume_deleted(&v.id, Utc::now());
                f.persist_tenancy(&t);
                Ok(v)
            }
            Err(e) => {
                t.settle_volumes(std::slice::from_ref(&v.id), None);
                Err(e)
            }
        }
    });
    let v = task
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    tracing::info!(volume = %v.id, name = %v.name, node = %v.node, "volume deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub(super) struct ResizeVolumeBody {
    size_gib: u64,
}

/// `POST /fleet/volumes/{name}/resize`: grow a detached volume. The guest's file system is not grown: do that inside
/// the guest (`resize2fs`, `xfs_growfs`) after attaching it again.
pub(super) async fn resize_volume(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
    Json(b): Json<ResizeVolumeBody>,
) -> Result<Json<Value>, AppError> {
    let (v, target) = {
        let mut t = fleet.tenancy.lock().await;
        let v = t
            .volume_to_resize(&caller, &key, b.size_gib)
            .map_err(vol_error)?;
        let target = fleet
            .nodes
            .lock()
            .await
            .get(&v.node)
            .cloned()
            .ok_or_else(|| {
                AppError(
                    StatusCode::CONFLICT,
                    format!(
                        "volume '{}' is on '{}', which is no longer registered",
                        v.name, v.node
                    ),
                )
            })?;
        hold(&mut t, &v.id);
        (v, target)
    };
    let size = b.size_gib;
    let f = fleet.clone();
    let task = tokio::spawn(async move {
        let done: Result<(), AppError> = match f
            .http
            .post(format!("{}/v1/volumes/{}/resize", target.fluxvm_url, v.id))
            .json(&json!({"size_gib": size}))
            .send()
            .await
        {
            Err(e) => Err(unreachable(&target.name, e)),
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(node_failure(&target.name, resp).await),
        };
        let mut t = f.tenancy.lock().await;
        t.settle_volumes(std::slice::from_ref(&v.id), None);
        let out = match done {
            Ok(()) => t
                .set_volume_size(&v.id, size)
                .ok_or_else(|| AppError(StatusCode::CONFLICT, "the volume vanished".into())),
            Err(e) => Err(e),
        };
        f.persist_tenancy(&t);
        out
    });
    let v = task
        .await
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    tracing::info!(volume = %v.id, size_gib = v.size_gib, "volume resized");
    Ok(Json(volume_json(&v, &caller)))
}

// ---- attach at launch -------------------------------------------------------------------------------------------

/// A tenant's launch may not name `apple.extra_disks` itself: each entry is a file, device or NBD server as seen from
/// the Mac, so it could be any file there (another tenant's volume or VM disk, the host's own files) or reach into the
/// Mac's network. Tenants get block storage as volumes (`"volumes"`), which central turns into extra disks after this
/// check. The operator may still name extra disks; the node's `policy.allowed_image_dirs` applies to those.
pub(super) fn refuse_tenant_extra_disks(caller: &Caller, body: &Value) -> Result<(), AppError> {
    if *caller == Caller::Admin {
        return Ok(());
    }
    let named = body
        .pointer("/apple/extra_disks")
        .is_some_and(|d| !d.is_null() && d.as_array().is_none_or(|a| !a.is_empty()));
    if named {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "apple.extra_disks names files on the Mac, which tenants may not do; attach storage with \"volumes\": [\"<name>\"] (POST /fleet/volumes makes one)".into(),
        ));
    }
    Ok(())
}

/// Takes `"volumes"` (names or ids) out of a `POST /fleet/vms` body: it is for central, never forwarded to a node.
pub(super) fn take_volumes(body: &mut Value) -> Result<Vec<String>, AppError> {
    let raw = body.as_object_mut().and_then(|o| o.remove("volumes"));
    let bad = || {
        AppError(
            StatusCode::BAD_REQUEST,
            "\"volumes\" must be a list of volume names".into(),
        )
    };
    match raw {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => {
            if a.len() > MAX_VOLUMES_PER_VM {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("a VM can have at most {MAX_VOLUMES_PER_VM} volumes"),
                ));
            }
            a.iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(bad))
                .collect()
        }
        Some(_) => Err(bad()),
    }
}

/// The volumes a launch claimed. [`LaunchVolumes::settle`] attaches them to the new VM or frees them; dropped
/// unsettled (the request was cancelled), it frees them.
pub(super) struct LaunchVolumes<'a> {
    fleet: &'a Fleet,
    ids: Vec<String>,
}

impl LaunchVolumes<'_> {
    /// Attaches the claimed volumes to the VM the launch created, or frees them when it failed.
    pub(super) async fn settle(mut self, placed: &Result<Json<Value>, AppError>) {
        let ids = std::mem::take(&mut self.ids);
        if ids.is_empty() {
            return;
        }
        let vm = placed
            .as_ref()
            .ok()
            .and_then(|j| j.0.get("vm"))
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str);
        let mut t = self.fleet.tenancy.lock().await;
        t.settle_volumes(&ids, vm);
        self.fleet.persist_tenancy(&t);
    }
}

impl Drop for LaunchVolumes<'_> {
    fn drop(&mut self) {
        let ids = std::mem::take(&mut self.ids);
        if ids.is_empty() {
            return;
        }
        if let Ok(mut t) = self.fleet.tenancy.try_lock() {
            t.settle_volumes(&ids, None);
            return;
        }
        let tenancy = self.fleet.tenancy.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                tenancy.lock().await.settle_volumes(&ids, None);
            });
        }
    }
}

/// Prepares a launch's volumes: claims them for `effective` (the tenant the VM is for, or the operator), pins the
/// launch to their Mac (refusing a launch that names another one), and adds each as an `apple.extra_disks` entry.
/// Also refuses a body that names a volume's file directly in `apple.extra_disks`, which would skip all of that.
pub(super) async fn claim_for_launch<'a>(
    fleet: &'a Fleet,
    effective: &Caller,
    keys: &[String],
    body: &mut Value,
    requested_node: &mut Option<String>,
    reschedulable: bool,
) -> Result<LaunchVolumes<'a>, AppError> {
    let none = LaunchVolumes {
        fleet,
        ids: Vec::new(),
    };
    let named_paths: Vec<String> = body
        .pointer("/apple/extra_disks")
        .and_then(Value::as_array)
        .map(|disks| {
            disks
                .iter()
                .flat_map(|d| [d.get("path"), d.get("overlay")])
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if keys.is_empty() && named_paths.is_empty() {
        return Ok(none);
    }
    let mut t = fleet.tenancy.lock().await;
    if named_paths.iter().any(|p| t.is_volume_path(p)) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "apple.extra_disks names a volume's file; attach volumes with \"volumes\": [\"<name>\"] instead".into(),
        ));
    }
    if keys.is_empty() {
        return Ok(none);
    }
    let refuse = |msg: &str| Err(AppError(StatusCode::BAD_REQUEST, msg.to_owned()));
    if body.get("backend").and_then(Value::as_str) != Some("vz") {
        return refuse("volumes attach to vz VMs only: set \"backend\": \"vz\"");
    }
    if body.pointer("/apple/guest_os").and_then(Value::as_str) == Some("macos") {
        return refuse(
            "volumes attach to Linux guests only: Virtualization.framework gives macOS guests no extra disks here",
        );
    }
    if body
        .get("apple")
        .is_some_and(|a| !a.is_object() && !a.is_null())
    {
        return refuse("\"apple\" must be an object");
    }
    if reschedulable {
        return refuse(
            "a VM with volumes cannot be reschedulable: its volumes stay on their Mac, so a drain could not move it",
        );
    }
    let vols = t.claim_volumes(effective, keys).map_err(vol_error)?;
    let claim = LaunchVolumes {
        fleet,
        ids: vols.iter().map(|v| v.id.clone()).collect(),
    };
    drop(t);
    let node = vols[0].node.clone();
    if let Some(asked) = requested_node.as_deref()
        && asked != node
    {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!(
                "volume '{}' is on '{node}', but the launch asks for '{asked}'; a VM runs on the Mac its volumes are on",
                vols[0].name
            ),
        ));
    }
    *requested_node = Some(node);
    if !body.get("apple").is_some_and(Value::is_object) {
        body["apple"] = json!({});
    }
    let apple = &mut body["apple"];
    if !apple.get("extra_disks").is_some_and(Value::is_array) {
        apple["extra_disks"] = json!([]);
    }
    if let Some(disks) = apple["extra_disks"].as_array_mut() {
        disks.extend(vols.iter().filter_map(Volume::extra_disk));
    }
    Ok(claim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_volumes_are_a_list_of_names() {
        let mut b = json!({"name": "db", "volumes": ["data", "logs"]});
        assert_eq!(take_volumes(&mut b).unwrap(), ["data", "logs"]);
        assert!(b.get("volumes").is_none(), "never forwarded to the node");
        assert!(take_volumes(&mut json!({})).unwrap().is_empty());
        assert!(take_volumes(&mut json!({"volumes": "data"})).is_err());
        assert!(take_volumes(&mut json!({"volumes": [1]})).is_err());
        let many: Vec<String> = (0..=MAX_VOLUMES_PER_VM).map(|i| format!("v{i}")).collect();
        assert!(take_volumes(&mut json!({ "volumes": many })).is_err());
    }

    #[test]
    fn tenants_never_name_extra_disks() {
        let acme = Caller::Tenant("acme".into());
        for body in [
            json!({"apple": {"extra_disks": [{"path": "/etc/master.passwd"}]}}),
            json!({"apple": {"extra_disks": [{"kind": "block", "path": "/dev/disk0"}]}}),
            json!({"apple": {"extra_disks": [{"kind": "nbd", "url": "nbd://10.0.0.1:10809/x"}]}}),
            json!({"apple": {"extra_disks": "x"}}),
        ] {
            assert_eq!(
                refuse_tenant_extra_disks(&acme, &body).unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "{body}"
            );
            assert!(refuse_tenant_extra_disks(&Caller::Admin, &body).is_ok());
        }
        for body in [
            json!({}),
            json!({"apple": {"guest_os": "linux"}}),
            json!({"apple": {"extra_disks": []}}),
            json!({"apple": {"extra_disks": null}}),
        ] {
            assert!(refuse_tenant_extra_disks(&acme, &body).is_ok(), "{body}");
        }
    }
}
