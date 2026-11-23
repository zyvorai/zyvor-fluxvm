// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The Apps page: the Docker-style apps `fluxctl kube run` makes, over the cluster's own API server (the same client certificate
//! and address fallback as the rest of `/v1/kube`; no kubectl).
//!
//! * `GET  /v1/kube/{cluster}/apps` lists the apps (Deployments labelled `app.kubernetes.io/managed-by=fluxctl-app`), each with its
//!   state in one sentence, its pods and, for a problem, the explain code and the command to run next. Not reaching the cluster is an
//!   `error` field in the answer, not an HTTP failure.
//! * `POST /v1/kube/{cluster}/apps` runs an image (`{"image": "nginx", "name"?, "port"?, "replicas"?, "namespace"?, "env"?}`), admin only.
//! * `POST /v1/kube/{cluster}/apps/{ns}/{app}/delete` removes an app and its Service, admin only, and only if fluxctl made it.
//!
//! Scaling and restarting reuse the workload endpoints; logs and a shell reuse the pod ones. The objects and the sentences come from
//! `fluxvm_core::kube_app`, which the CLI shares.

use crate::kube::{self, API_NAME, Target, err};
use crate::{ApiError, ApiResult, Role, TokenTenant, require_admin};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use fluxvm_core::kube_app::{self, Spec};
use fluxvm_scheduler::VmManager;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const SEL: &str = "labelSelector=app.kubernetes.io%2Fmanaged-by%3Dfluxctl-app";

#[derive(Debug, Deserialize)]
pub(crate) struct EnvVar {
    name: String,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RunBody {
    image: String,
    name: Option<String>,
    port: Option<u16>,
    replicas: Option<u32>,
    namespace: Option<String>,
    #[serde(default)]
    env: Vec<EnvVar>,
}

/// The [`Spec`] a request body asks for, checked.
pub(crate) fn spec_of(b: RunBody) -> Result<Spec, ApiError> {
    let image = b.image.trim().to_owned();
    let spec = Spec {
        name: b
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| kube_app::default_name(&image)),
        namespace: b
            .namespace
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| "default".to_owned()),
        port: b.port.unwrap_or_else(|| kube_app::default_port(&image)),
        replicas: b.replicas.unwrap_or(1),
        env: b.env.into_iter().map(|e| (e.name, e.value)).collect(),
        image,
    };
    kube_app::validate(&spec).map_err(|m| err(StatusCode::BAD_REQUEST, m))?;
    Ok(spec)
}

fn is_404(e: &ApiError) -> bool {
    e.message.contains("answered 404")
}

/// The answer when the cluster could not be read: a sentence and the command to run next.
fn failure(cluster: &str, message: &str) -> Value {
    let (sentence, next) = if message.contains("is stopped")
        || message.contains("is paused")
        || message.contains("is failed")
        || message.contains("is creating")
    {
        (
            format!("The cluster's server machine is not running ({message})."),
            format!("fluxctl kube up {cluster}"),
        )
    } else {
        (
            format!("Could not read the cluster's apps: {message}."),
            "fluxctl cluster status".to_owned(),
        )
    };
    json!({"schema": 1, "apps": [], "error": {"message": sentence, "next": next}})
}

async fn send(
    target: &Target,
    method: Method,
    path: &str,
    body: Option<&Value>,
    content_type: &str,
) -> Result<(), ApiError> {
    let mut tried = vec![];
    for &addr in &target.addrs {
        let client = kube::http_client(&target.creds, addr)?;
        let url = format!("https://{API_NAME}:{}{path}", addr.port());
        let mut call = client.request(method.clone(), &url);
        if let Some(body) = body {
            call = call
                .header("content-type", content_type)
                .body(body.to_string());
        }
        let resp = match call.send().await {
            Ok(r) => r,
            Err(e) => {
                tried.push(format!("{addr}: {}", kube::chain(&e)));
                continue;
            }
        };
        let status = resp.status();
        let text = resp.bytes().await.unwrap_or_default();
        return if status.is_success() {
            Ok(())
        } else {
            Err(crate::kube_actions::refusal(status.as_u16(), &text))
        };
    }
    Err(err(StatusCode::BAD_GATEWAY, tried.join("; ")))
}

/// `GET /v1/kube/{cluster}/apps`
pub(crate) async fn get_apps(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target = match kube::resolve(&m, tenant.as_deref(), &name).await {
        Ok(t) => t,
        Err(e) if matches!(e.status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND) => {
            return Err(e);
        }
        Err(e) => return Ok(Json(failure(&name, &e.message))),
    };
    let deps_path = format!("/apis/apps/v1/deployments?{SEL}");
    let svcs_path = format!("/api/v1/services?{SEL}");
    let pods_path = format!("/api/v1/pods?{SEL}");
    let (deps, svcs, pods) = tokio::join!(
        kube::get_json(&target, &deps_path),
        kube::get_json(&target, &svcs_path),
        kube::get_json(&target, &pods_path),
    );
    match (deps, svcs, pods) {
        (Ok(d), Ok(s), Ok(p)) => Ok(Json(json!({
            "schema": 1,
            "cluster": name,
            "apps": kube_app::summarize(&d, &s, &p),
        }))),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => Ok(Json(failure(&name, &e.message))),
    }
}

/// `POST /v1/kube/{cluster}/apps`
pub(crate) async fn run_app(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(cluster): Path<String>,
    Json(body): Json<RunBody>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let spec = spec_of(body)?;
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target = kube::resolve(&m, tenant.as_deref(), &cluster).await?;
    let (ns, name) = (spec.namespace.as_str(), spec.name.as_str());
    let taken = |kind: &str| {
        err(
            StatusCode::CONFLICT,
            format!(
                "A {kind} called {name} already exists in {ns} and was not made by fluxctl, so it is left alone. Choose another name (APP_NAME_TAKEN)"
            ),
        )
    };
    let dep_path = format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}");
    let svc_path = format!("/api/v1/namespaces/{ns}/services/{name}");
    let dep_exists = match kube::get_json(&target, &dep_path).await {
        Ok(d) if kube_app::is_managed(&d) => true,
        Ok(_) => return Err(taken("Deployment")),
        Err(e) if is_404(&e) => false,
        Err(e) => return Err(e),
    };
    let svc_exists = match kube::get_json(&target, &svc_path).await {
        Ok(s) if kube_app::is_managed(&s) => true,
        Ok(_) => return Err(taken("Service")),
        Err(e) if is_404(&e) => false,
        Err(e) => return Err(e),
    };
    if ns != "default"
        && let Err(e) = kube::get_json(&target, &format!("/api/v1/namespaces/{ns}")).await
    {
        if !is_404(&e) {
            return Err(e);
        }
        send(
            &target,
            Method::POST,
            "/api/v1/namespaces",
            Some(&json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": ns}})),
            "application/json",
        )
        .await?;
    }
    let objs = kube_app::manifests(&spec);
    if dep_exists {
        send(
            &target,
            Method::PATCH,
            &dep_path,
            Some(&objs[0]),
            "application/merge-patch+json",
        )
        .await?;
    } else {
        send(
            &target,
            Method::POST,
            &format!("/apis/apps/v1/namespaces/{ns}/deployments"),
            Some(&objs[0]),
            "application/json",
        )
        .await?;
    }
    if !svc_exists {
        send(
            &target,
            Method::POST,
            &format!("/api/v1/namespaces/{ns}/services"),
            Some(&objs[1]),
            "application/json",
        )
        .await?;
    }
    Ok(Json(json!({
        "cluster": cluster,
        "name": spec.name,
        "namespace": spec.namespace,
        "result": if dep_exists { "updated" } else { "created" },
    })))
}

/// `POST /v1/kube/{cluster}/apps/{ns}/{app}/delete`
pub(crate) async fn delete_app(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((cluster, ns, app)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    if !kube_app::name_ok(&ns) || !kube_app::name_ok(&app) {
        return Err(err(StatusCode::BAD_REQUEST, "not a namespace or app name"));
    }
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target = kube::resolve(&m, tenant.as_deref(), &cluster).await?;
    let dep_path = format!("/apis/apps/v1/namespaces/{ns}/deployments/{app}");
    match kube::get_json(&target, &dep_path).await {
        Ok(d) if kube_app::is_managed(&d) => {}
        Ok(_) => {
            return Err(err(
                StatusCode::CONFLICT,
                format!("{app} was not made by `fluxctl kube run`, so fluxctl leaves it alone. Use kubectl for it."),
            ));
        }
        Err(e) if is_404(&e) => {
            return Err(err(
                StatusCode::NOT_FOUND,
                format!("There is no app called {app} in {ns} (it may already be removed). List the apps with `fluxctl kube apps` (APP_NOT_FOUND)"),
            ));
        }
        Err(e) => return Err(e),
    }
    send(&target, Method::DELETE, &dep_path, None, "application/json").await?;
    // The Service is ours only if it carries the label; a missing one is fine.
    let svc_path = format!("/api/v1/namespaces/{ns}/services/{app}");
    if let Ok(s) = kube::get_json(&target, &svc_path).await
        && kube_app::is_managed(&s)
    {
        send(&target, Method::DELETE, &svc_path, None, "application/json").await?;
    }
    Ok(Json(json!({"cluster": cluster, "name": app, "namespace": ns, "result": "deleted"})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Value) -> RunBody {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_request_fills_in_the_name_and_port_from_the_image() {
        let s = spec_of(body(json!({"image": "ghcr.io/acme/Shop:1.0", "env": [{"name": "A", "value": "b"}]}))).unwrap();
        assert_eq!((s.name.as_str(), s.port, s.replicas, s.namespace.as_str()), ("shop", 80, 1, "default"));
        assert_eq!(s.env, vec![("A".to_owned(), "b".to_owned())]);
        let s = spec_of(body(json!({"image": "grafana/grafana", "name": " ", "port": 3001, "replicas": 2}))).unwrap();
        assert_eq!((s.name.as_str(), s.port, s.replicas), ("grafana", 3001, 2));
    }

    #[test]
    fn bad_requests_are_400_with_a_sentence() {
        for b in [
            json!({"image": "--privileged"}),
            json!({"image": "nginx", "name": "Bad Name"}),
            json!({"image": "nginx", "replicas": 500}),
            json!({"image": "nginx", "env": [{"name": "a-b", "value": ""}]}),
            json!({"image": "nginx", "namespace": "../x"}),
        ] {
            let e = spec_of(body(b)).unwrap_err();
            assert_eq!(e.status, StatusCode::BAD_REQUEST);
            assert!(e.message.len() > 20, "{}", e.message);
        }
    }

    #[test]
    fn an_unreachable_cluster_is_a_sentence_and_a_command() {
        let v = failure("dev", "the server VM dev is stopped");
        assert_eq!(v["error"]["next"], "fluxctl kube up dev");
        assert!(v["apps"].as_array().unwrap().is_empty());
        let v = failure("dev", "connection refused");
        assert_eq!(v["error"]["next"], "fluxctl cluster status");
    }
}
