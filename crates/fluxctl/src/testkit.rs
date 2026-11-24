// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Test-only fake daemon for the CLI-level tests of `protect`, `prune`, `df` and `top`: an in-memory VM list behind the
//! same routes and status codes the real daemon uses (`GET /v1/vms?label=`, `GET|PATCH|DELETE /v1/vms/{id}`,
//! `GET /v1/host/storage`, `GET /v1/stats`), with a log of every call so a test can prove what was (not) sent.

use crate::remote::Remote;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Fake {
    pub vms: Mutex<Vec<Value>>,
    pub calls: Mutex<Vec<String>>,
    pub storage: Mutex<Value>,
    pub stats: Mutex<VecDeque<Value>>,
}

pub fn vm(id: u128, name: &str, status: &str, age_days: i64, labels: &[(&str, &str)]) -> Value {
    json!({
        "id": uuid::Uuid::from_u128(id),
        "name": name,
        "status": status,
        "created_at": chrono::Utc::now() - chrono::Duration::days(age_days),
        "labels": labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
    })
}

type S = State<Arc<Fake>>;

fn labels_of(v: &Value) -> BTreeMap<String, String> {
    serde_json::from_value(v["labels"].clone()).unwrap_or_default()
}

async fn list(State(f): S, Query(q): Query<BTreeMap<String, String>>) -> Json<Value> {
    f.calls.lock().unwrap().push(format!("GET /v1/vms {q:?}"));
    let sel = q
        .get("label")
        .map(|s| fluxvm_core::model::LabelSelector::parse(s).unwrap());
    let items: Vec<Value> = f
        .vms
        .lock()
        .unwrap()
        .iter()
        .filter(|v| sel.as_ref().is_none_or(|s| s.matches(&labels_of(v))))
        .cloned()
        .collect();
    Json(json!({ "items": items }))
}

async fn patch(
    State(f): S,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    f.calls.lock().unwrap().push(format!("PATCH {id} {body}"));
    let mut vms = f.vms.lock().unwrap();
    let Some(v) = vms.iter_mut().find(|v| v["id"] == id.as_str()) else {
        return Err((StatusCode::NOT_FOUND, Json(json!({"error": "no such VM"}))));
    };
    let mut l = labels_of(v);
    for (k, val) in body["labels"].as_object().into_iter().flatten() {
        match val.as_str() {
            Some(s) => l.insert(k.clone(), s.to_string()),
            None => l.remove(k),
        };
    }
    v["labels"] = json!(l);
    Ok(Json(v.clone()))
}

async fn delete(State(f): S, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    f.calls.lock().unwrap().push(format!("DELETE {id}"));
    let mut vms = f.vms.lock().unwrap();
    let Some(pos) = vms.iter().position(|v| v["id"] == id.as_str()) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "no such VM"})));
    };
    // The same refusal the daemon gives (`fluxvm_core::protect`).
    let name = vms[pos]["name"].as_str().unwrap_or("").to_string();
    if let Err(e) = fluxvm_core::protect::guard_delete(&name, &labels_of(&vms[pos])) {
        return (StatusCode::CONFLICT, Json(json!({"error": e.to_string()})));
    }
    vms.remove(pos);
    (StatusCode::NO_CONTENT, Json(Value::Null))
}

async fn storage(State(f): S) -> Json<Value> {
    f.calls.lock().unwrap().push("GET storage".into());
    Json(f.storage.lock().unwrap().clone())
}

async fn stats(State(f): S) -> Result<Json<Value>, StatusCode> {
    f.calls.lock().unwrap().push("GET stats".into());
    f.stats
        .lock()
        .unwrap()
        .pop_front()
        .map(Json)
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

pub async fn spawn(f: Arc<Fake>) -> Remote {
    let app = Router::new()
        .route("/v1/vms", get(list))
        .route("/v1/vms/{id}", axum::routing::patch(patch).delete(delete))
        .route("/v1/host/storage", get(storage))
        .route("/v1/stats", get(stats))
        .with_state(f);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    Remote::new(&format!("http://{addr}"), None)
}
