// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `/v1/instancetypes` and `/v1/preferences`: named size classes and device-default bundles (see
//! `fluxvm_core::instancetype`), plus `GET /v1/vms/{id}/instancetype`, the snapshot recorded when a VM
//! was created from them. Reads are open to every role; writes are admin-only.
//!
//! * `GET    /v1/instancetypes`: built-in and stored classes, every version
//! * `POST   /v1/instancetypes` `{"name", "version"?, "description"?, "spec", "replace"?}` (201)
//! * `GET    /v1/instancetypes/{name}`: `name` or `name:version`; the latest version when unversioned
//! * `DELETE /v1/instancetypes/{name}`: every stored version, or one with `name:version`; built-ins refuse
//! * `GET|POST /v1/preferences`, `GET|DELETE /v1/preferences/{name}`
//!
//! Errors are 404 for a class that does not exist and 400 for an invalid or conflicting one.

use super::{ApiError, ApiResult, require_admin};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::get,
};
use fluxvm_core::config::Role;
use fluxvm_core::instancetype::{
    Catalog, ClassRef, Instancetype, InstancetypeSpec, Preference, PreferenceSpec,
};
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

pub(crate) fn routes() -> Router<Arc<VmManager>> {
    Router::new()
        .route("/v1/instancetypes", get(list_types).post(save_type))
        .route(
            "/v1/instancetypes/{name}",
            get(get_type).delete(delete_type),
        )
        .route("/v1/preferences", get(list_prefs).post(save_pref))
        .route("/v1/preferences/{name}", get(get_pref).delete(delete_pref))
        .route("/v1/vms/{id}/instancetype", get(vm_snapshot))
}

fn not_found(msg: String) -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        message: msg,
    }
}

fn catalog(m: &VmManager) -> ApiResult<Catalog> {
    Ok(Catalog::load(&m.cfg.state_dir)?)
}

async fn list_types(State(m): State<Arc<VmManager>>) -> ApiResult<Json<Value>> {
    let c = catalog(&m)?;
    let items: Vec<&Instancetype> = c.instancetypes.values().flat_map(|v| v.values()).collect();
    Ok(Json(json!({ "items": items })))
}

async fn get_type(
    State(m): State<Arc<VmManager>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Instancetype>> {
    let r = ClassRef::parse(&name)?;
    let c = catalog(&m)?;
    let t = c
        .find_instancetype(&r)
        .map_err(|e| not_found(e.to_string()))?;
    Ok(Json(t.clone()))
}

#[derive(Deserialize)]
struct SaveType {
    name: String,
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    description: Option<String>,
    spec: InstancetypeSpec,
    #[serde(default)]
    replace: bool,
}

async fn save_type(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Json(req): Json<SaveType>,
) -> ApiResult<(StatusCode, Json<Instancetype>)> {
    require_admin(role)?;
    let t = fluxvm_core::instancetype::save_instancetype(
        &m.cfg.state_dir,
        &req.name,
        req.version,
        req.description,
        req.spec,
        req.replace,
    )?;
    fluxvm_scheduler::audit_event(
        "instancetype.save",
        &[("instancetype", &format!("{}:{}", t.name, t.version))],
    );
    Ok((StatusCode::CREATED, Json(t)))
}

async fn delete_type(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    let r = ClassRef::parse(&name)?;
    fluxvm_core::instancetype::delete_instancetype(&m.cfg.state_dir, &r.name, r.version).map_err(
        |e| {
            if e.to_string().contains("not found") {
                not_found(e.to_string())
            } else {
                e.into()
            }
        },
    )?;
    fluxvm_scheduler::audit_event("instancetype.delete", &[("instancetype", &name)]);
    Ok(StatusCode::NO_CONTENT)
}

async fn list_prefs(State(m): State<Arc<VmManager>>) -> ApiResult<Json<Value>> {
    let c = catalog(&m)?;
    let items: Vec<&Preference> = c.preferences.values().collect();
    Ok(Json(json!({ "items": items })))
}

async fn get_pref(
    State(m): State<Arc<VmManager>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Preference>> {
    let c = catalog(&m)?;
    let p = c
        .find_preference(&name)
        .map_err(|e| not_found(e.to_string()))?;
    Ok(Json(p.clone()))
}

#[derive(Deserialize)]
struct SavePref {
    name: String,
    #[serde(default)]
    description: Option<String>,
    spec: PreferenceSpec,
    #[serde(default)]
    replace: bool,
}

async fn save_pref(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Json(req): Json<SavePref>,
) -> ApiResult<(StatusCode, Json<Preference>)> {
    require_admin(role)?;
    let p = fluxvm_core::instancetype::save_preference(
        &m.cfg.state_dir,
        &req.name,
        req.description,
        req.spec,
        req.replace,
    )?;
    fluxvm_scheduler::audit_event("preference.save", &[("preference", &p.name)]);
    Ok((StatusCode::CREATED, Json(p)))
}

async fn delete_pref(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    require_admin(role)?;
    fluxvm_core::instancetype::delete_preference(&m.cfg.state_dir, &name).map_err(|e| {
        if e.to_string().contains("not found") {
            not_found(e.to_string())
        } else {
            e.into()
        }
    })?;
    fluxvm_scheduler::audit_event("preference.delete", &[("preference", &name)]);
    Ok(StatusCode::NO_CONTENT)
}

async fn vm_snapshot(
    State(m): State<Arc<VmManager>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let vm = m.get(id).await?;
    match fluxvm_scheduler::instancetypes::read_snapshot(&vm.workspace) {
        Some(s) => Ok(Json(json!(s))),
        None => Err(not_found(format!(
            "VM {id} was not created from an instancetype or preference"
        ))),
    }
}
