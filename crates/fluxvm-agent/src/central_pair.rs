// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Pairing by short code (docs/cluster-pairing.md): a machine asks to join, a person approves, the machine receives its
//! node credential. The protocol and the state machine are `fluxvm_core::pairing`; this file is the HTTP glue and the
//! hook into the *existing* enrollment (`EnrollState::issue_paired`), so no new credential type exists.
//!
//! Off unless central runs with `--pairing`: then `/fleet/pair/request` and `/fleet/pair/claim` are the only two
//! unauthenticated pairing routes. Listing, approving and denying need the operator token like any other `/fleet/*`.

use super::{AppError, Fleet};
use axum::{
    Json,
    extract::{ConnectInfo, Path, State},
    http::StatusCode,
};
use chrono::Utc;
use fluxvm_core::pairing::{Claim, PairError, hex};
use serde::Deserialize;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turns pairing on for this process (`--pairing`).
pub fn set_pairing(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn off() -> AppError {
    AppError(
        StatusCode::NOT_FOUND,
        "pairing is off; start central with --pairing".into(),
    )
}

fn app(e: PairError) -> AppError {
    match e {
        PairError::Invalid(m) => AppError(StatusCode::BAD_REQUEST, m.into()),
        PairError::Busy(s) => AppError(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many pairing requests; retry in {s}s"),
        ),
        PairError::NotFound => AppError(
            StatusCode::NOT_FOUND,
            "no such pairing request (it may have expired)".into(),
        ),
        PairError::Expired => AppError(StatusCode::GONE, "the pairing request expired".into()),
        PairError::NotPending => AppError(
            StatusCode::CONFLICT,
            "that request is not waiting any more".into(),
        ),
        PairError::BadProof => AppError(StatusCode::FORBIDDEN, "bad proof".into()),
        PairError::Issue(m) => AppError(StatusCode::CONFLICT, m),
    }
}

#[derive(Deserialize)]
pub(super) struct PairRequest {
    name: String,
    ed_pub: String,
    x_pub: String,
}

/// `POST /fleet/pair/request` (public): grants nothing, only puts the machine in front of the operator.
pub(super) async fn request(
    State(fleet): State<Fleet>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<PairRequest>,
) -> Result<Json<Value>, AppError> {
    if !enabled() {
        return Err(off());
    }
    let mut book = fleet.registry.enroll.pair.lock().await;
    let offer = book
        .request(
            Utc::now().timestamp(),
            &peer.ip().to_string(),
            &req.name,
            &req.ed_pub,
            &req.x_pub,
        )
        .map_err(app)?;
    tracing::info!(node = %req.name, from = %peer.ip(), "a machine asks to join; waiting for approval");
    Ok(Json(json!(offer)))
}

#[derive(Deserialize)]
pub(super) struct ClaimRequest {
    id: String,
    sig: String,
}

/// `POST /fleet/pair/claim` (public, but signed by the machine's key): the state of the request and, once, the sealed credential.
pub(super) async fn claim(
    State(fleet): State<Fleet>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(req): Json<ClaimRequest>,
) -> Result<Json<Value>, AppError> {
    if !enabled() {
        return Err(off());
    }
    let mut book = fleet.registry.enroll.pair.lock().await;
    let r = book
        .claim(
            Utc::now().timestamp(),
            &peer.ip().to_string(),
            &req.id,
            &req.sig,
        )
        .map_err(app)?;
    Ok(Json(match r {
        Claim::Pending => json!({"state": "pending"}),
        Claim::Denied => json!({"state": "denied"}),
        Claim::Approved(sealed) => json!({"state": "approved", "sealed": hex(&sealed)}),
    }))
}

/// `GET /fleet/pair/pending` (operator): machines asking to join.
pub(super) async fn pending(State(fleet): State<Fleet>) -> Json<Value> {
    let mut book = fleet.registry.enroll.pair.lock().await;
    Json(json!({"enabled": enabled(), "items": book.pending(Utc::now().timestamp())}))
}

/// `POST /fleet/pair/{key}/approve|deny` (operator); `key` is the request id or the code on the machine's screen.
pub(super) async fn decide(
    State(fleet): State<Fleet>,
    Path((key, action)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let now = Utc::now().timestamp();
    let enroll = &fleet.registry.enroll;
    match action.as_str() {
        "approve" => {
            if super::enroll::node_auth() == super::enroll::NodeAuth::Legacy {
                return Err(AppError(
                    StatusCode::CONFLICT,
                    "enrollment is off (--node-auth legacy); pairing cannot issue credentials"
                        .into(),
                ));
            }
            // Lock order: the book, then the enrollment state.
            let mut book = enroll.pair.lock().await;
            let mut st = enroll.state.lock().await;
            let v = book
                .approve(now, &key, |name, node_id| {
                    let c = st
                        .issue_paired(name, node_id, now)
                        .map_err(|e| e.into_app().1);
                    if c.is_ok() {
                        enroll.save_state(&st);
                    }
                    c
                })
                .map_err(app)?;
            tracing::info!(node = %v.name, "pairing approved");
            Ok(Json(json!({"approved": v.name, "code": v.code})))
        }
        "deny" => {
            let mut book = enroll.pair.lock().await;
            book.deny(now, &key).map_err(app)?;
            Ok(Json(json!({"denied": key})))
        }
        _ => Err(AppError(
            StatusCode::NOT_FOUND,
            "unknown pairing action".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::central::{CentralConfig, router};
    use axum::body::Body;
    use axum::http::Request;
    use fluxvm_core::pairing::{NodeKeys, unhex};
    use tower::ServiceExt;

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let mut req = b.body(Body::from(body.to_string())).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo("10.1.2.3:5000".parse::<SocketAddr>().unwrap()));
        let resp = app.clone().oneshot(req).await.unwrap();
        let s = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (s, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn pairing_over_http_issues_the_existing_node_credential() {
        set_pairing(true);
        let dir = tempfile::tempdir().unwrap();
        let app = router(CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some("admin".into()),
            reconcile_interval: None,
            ha: None,
        });
        let keys = NodeKeys::new(None);
        let (s, offer) = call(
            &app,
            "POST",
            "/fleet/pair/request",
            None,
            json!({"name": "mini-2", "ed_pub": hex(&keys.ed_pub()), "x_pub": hex(&keys.x_pub())}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{offer}");
        let id = offer["id"].as_str().unwrap().to_owned();
        let sp: [u8; 32] = unhex(offer["server_pub"].as_str().unwrap()).unwrap();
        let sig = keys.claim_signature(&id, &sp);

        // Nothing is granted yet.
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/pair/claim",
            None,
            json!({"id": id, "sig": sig}),
        )
        .await;
        assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("pending")));

        // Listing and approving need the operator.
        for (m, p) in [
            ("GET", "/fleet/pair/pending".to_owned()),
            ("POST", format!("/fleet/pair/{id}/approve")),
            ("POST", format!("/fleet/pair/{id}/deny")),
        ] {
            let (s, _) = call(&app, m, &p, None, json!({})).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {p}");
        }
        let (s, v) = call(&app, "GET", "/fleet/pair/pending", Some("admin"), json!({})).await;
        assert_eq!(s, StatusCode::OK);
        let code = v["items"][0]["code"].as_str().unwrap().to_owned();
        assert_eq!(
            code,
            keys.code(&id, &sp),
            "the screen and the page show the same code"
        );

        // Approve by the code a person reads off the machine.
        let (s, v) = call(
            &app,
            "POST",
            &format!("/fleet/pair/{}/approve", code.to_lowercase()),
            Some("admin"),
            json!({}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");

        // Claims are rate limited to one a second per request.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/pair/claim",
            None,
            json!({"id": id, "sig": sig}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let sealed = unhex_vec(v["sealed"].as_str().unwrap());
        let cred = keys.open_credential(&id, &sp, &sealed).unwrap();
        assert!(cred.starts_with("fvnc_"));

        // The credential is the ordinary one: it registers the node as itself.
        let beat = json!({"name": "mini-2", "fluxvm_url": "http://10.1.2.3:7788", "vcpus_total": 4, "memory_mib_total": 4096, "vm_count": 0});
        let (s, v) = call(&app, "POST", "/fleet/register", Some(&cred), beat).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        // ... and nothing more (the operator list is not for nodes).
        let (s, _) = call(&app, "GET", "/fleet/pair/pending", Some(&cred), json!({})).await;
        assert_ne!(s, StatusCode::OK);
        // A second collection (replay) yields nothing.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let (s, _) = call(
            &app,
            "POST",
            "/fleet/pair/claim",
            None,
            json!({"id": id, "sig": sig}),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        // No join token is left usable.
        let (_, v) = call(
            &app,
            "GET",
            "/fleet/enroll/tokens",
            Some("admin"),
            json!({}),
        )
        .await;
        assert!(
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["revoked"] == true)
        );
    }

    fn unhex_vec(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }
}
