// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The Secrets and certificates of a cluster for the Kubernetes page: `GET /v1/kube/{cluster}/secrets[?namespace=]` and
//! `GET /v1/kube/{cluster}/certs`. Read-only. Secrets are reduced to name, type and key names on the way in (see
//! `fluxvm_core::kube_view`): a value is never put in an answer, a log line or an error, and a TLS Secret contributes its public
//! certificate's expiry only. Each answer carries the `fluxctl` command that shows the same thing, for Copy-as-fluxctl. Not being able to
//! reach the cluster is an `error` field (a sentence and the next command), not an HTTP failure, like the Gateways answer.

use crate::kube::{self, err};
use crate::{ApiResult, TokenTenant};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use fluxvm_core::kube_view;
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const INGRESS_PATH: &str = "/apis/networking.k8s.io/v1/ingresses";
const GATEWAY_PATH: &str = "/apis/gateway.networking.k8s.io/v1/gateways";
const TLS_SECRETS_PATH: &str = "/api/v1/secrets?fieldSelector=type%3Dkubernetes.io%2Ftls";

#[derive(Deserialize)]
pub(crate) struct SecretsQuery {
    namespace: Option<String>,
}

/// The answer when the cluster could not be read: a plain sentence and the command to run next.
pub(crate) fn failure(cluster: &str, what: &str, message: &str, fluxctl: &str) -> Value {
    let stopped = ["is stopped", "is paused", "is failed", "is creating"]
        .iter()
        .any(|s| message.contains(s));
    let (sentence, next) = if stopped {
        (
            format!("The cluster's server machine is not running ({message})."),
            format!("fluxctl kube up {cluster}"),
        )
    } else {
        (
            format!("Could not read the cluster's {what}: {message}."),
            "fluxctl cluster status".to_owned(),
        )
    };
    json!({"schema": 1, "cluster": cluster, "fluxctl": fluxctl, "error": {"message": sentence, "next": next}})
}

/// The `secrets` answer from a Secret list: names, types and key names. Pure, so tests can check no value gets through.
pub(crate) fn secrets_answer(cluster: &str, namespace: Option<&str>, list: &Value) -> Value {
    let rows = kube_view::secrets(list);
    let fluxctl = match namespace {
        Some(ns) => format!("fluxctl kube secrets ls --cluster {cluster} -n {ns}"),
        None => format!("fluxctl kube secrets ls --cluster {cluster}"),
    };
    json!({
        "schema": 1,
        "cluster": cluster,
        "secrets": rows,
        "fluxctl": fluxctl,
        "set": format!("fluxctl kube secrets set --cluster {cluster} NAME KEY"),
        "truncated": list["metadata"]["continue"].as_str().is_some_and(|c| !c.is_empty()),
    })
}

/// The `certs` answer from the three lists.
pub(crate) fn certs_answer(
    cluster: &str,
    ingresses: &Value,
    gateways: &Value,
    secrets: &Value,
    now: i64,
) -> Value {
    let rows = kube_view::certs(
        ingresses,
        gateways,
        &kube_view::tls_secrets(secrets),
        now,
        kube_view::WARN_DAYS,
    );
    let problems = rows.iter().filter(|r| r.state != "ok").count();
    json!({
        "schema": 1,
        "cluster": cluster,
        "certs": rows,
        "problems": problems,
        "warn_days": kube_view::WARN_DAYS,
        "fluxctl": format!("fluxctl kube certs --cluster {cluster}"),
    })
}

/// `GET /v1/kube/{cluster}/secrets[?namespace=]`
pub(crate) async fn get_secrets(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(name): Path<String>,
    Query(q): Query<SecretsQuery>,
) -> ApiResult<Json<Value>> {
    if let Some(ns) = &q.namespace
        && !crate::kube_pod::k8s_name_ok(ns)
    {
        return Err(err(StatusCode::BAD_REQUEST, "not a namespace name"));
    }
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let fluxctl = format!("fluxctl kube secrets ls --cluster {name}");
    let target = match kube::resolve(&m, tenant.as_deref(), &name).await {
        Ok(t) => t,
        Err(e) if matches!(e.status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND) => {
            return Err(e);
        }
        Err(e) => return Ok(Json(failure(&name, "Secrets", &e.message, &fluxctl))),
    };
    let path = match &q.namespace {
        Some(ns) => format!("/api/v1/namespaces/{ns}/secrets?limit=500"),
        None => "/api/v1/secrets?limit=500".to_owned(),
    };
    match kube::get_json(&target, &path).await {
        Ok(list) => Ok(Json(secrets_answer(&name, q.namespace.as_deref(), &list))),
        Err(e) => Ok(Json(failure(&name, "Secrets", &e.message, &fluxctl))),
    }
}

/// `GET /v1/kube/{cluster}/certs`
pub(crate) async fn get_certs(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let fluxctl = format!("fluxctl kube certs --cluster {name}");
    let target = match kube::resolve(&m, tenant.as_deref(), &name).await {
        Ok(t) => t,
        Err(e) if matches!(e.status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND) => {
            return Err(e);
        }
        Err(e) => return Ok(Json(failure(&name, "certificates", &e.message, &fluxctl))),
    };
    let (ingresses, gateways, secrets) = tokio::join!(
        kube::get_json(&target, INGRESS_PATH),
        kube::get_json(&target, GATEWAY_PATH),
        kube::get_json(&target, TLS_SECRETS_PATH),
    );
    let (ingresses, secrets) = match (ingresses, secrets) {
        (Ok(i), Ok(s)) => (i, s),
        (Err(e), _) | (_, Err(e)) => {
            return Ok(Json(failure(&name, "certificates", &e.message, &fluxctl)));
        }
    };
    // No Gateway API installed answers 404: that only means there are no Gateway certificates.
    let gateways = gateways.unwrap_or_else(|_| json!({}));
    let now = chrono::Utc::now().timestamp();
    Ok(Json(certs_answer(
        &name, &ingresses, &gateways, &secrets, now,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn word(n: usize) -> String {
        // built at run time: no secret-looking literal in the source
        ["val", "ue", &"7".repeat(n)].concat()
    }

    #[test]
    fn a_secret_value_never_reaches_the_answer() {
        let v = word(6);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&v);
        let list = json!({"items": [{"metadata": {"namespace": "shop", "name": "db"},
            "type": "Opaque", "data": {"password": b64},
            "stringData": {"password": v}}]});
        let a = secrets_answer("dev", None, &list);
        assert_eq!(a["secrets"][0]["keys"], json!(["password"]));
        let text = a.to_string();
        assert!(!text.contains(&v) && !text.contains(&b64), "{text}");
        assert_eq!(a["fluxctl"], "fluxctl kube secrets ls --cluster dev");
        let a = secrets_answer("dev", Some("shop"), &list);
        assert!(a["fluxctl"].as_str().unwrap().ends_with("-n shop"));
    }

    #[test]
    fn certs_answer_counts_problems_and_leaves_out_private_keys() {
        let ing = json!({"items": [{"metadata": {"namespace": "shop", "name": "web"},
            "spec": {"tls": [{"hosts": ["a.test"], "secretName": "ghost"}]}}]});
        let key = word(5);
        let secrets = json!({"items": [{"metadata": {"namespace": "shop", "name": "other"},
            "type": "kubernetes.io/tls", "data": {"tls.crt": "Q1JU", "tls.key": key}}]});
        let a = certs_answer("dev", &ing, &json!({}), &secrets, 1_800_000_000);
        assert_eq!(a["problems"], 1);
        assert_eq!(a["certs"][0]["state"], "missing");
        assert!(!a.to_string().contains(&key));
        assert_eq!(a["fluxctl"], "fluxctl kube certs --cluster dev");
    }

    #[test]
    fn failure_is_a_sentence_and_a_command() {
        let v = failure(
            "dev",
            "Secrets",
            "the server VM dev is stopped",
            "fluxctl kube secrets ls --cluster dev",
        );
        assert_eq!(v["error"]["next"], "fluxctl kube up dev");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not running")
        );
        assert_eq!(
            failure("dev", "Secrets", "timeout", "x")["error"]["next"],
            "fluxctl cluster status"
        );
        assert_eq!(v["fluxctl"], "fluxctl kube secrets ls --cluster dev");
    }
}
