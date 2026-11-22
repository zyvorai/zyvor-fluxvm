// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The Gateway API objects of a cluster for the Kubernetes page: `GET /v1/kube/{cluster}/gateways`. Read-only (four GETs through the
//! same client certificate and address fallback as the workloads), objects only and never Secrets. Not being able to reach the
//! cluster is an `error` field in the answer (a sentence and the next command), not an HTTP failure. The parsing is a pure function
//! over the raw lists so it can be tested with fixtures.

use crate::kube;
use crate::{ApiResult, TokenTenant};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use fluxvm_scheduler::VmManager;
use serde_json::{Value, json};
use std::sync::Arc;

const GW_PATH: &str = "/apis/gateway.networking.k8s.io/v1/gateways";
const ROUTE_PATH: &str = "/apis/gateway.networking.k8s.io/v1/httproutes";
const SLICE_PATH: &str = "/apis/discovery.k8s.io/v1/endpointslices";
const ENVOY_PATH: &str = "/apis/apps/v1/namespaces/envoy-gateway-system/deployments/envoy-gateway";

fn s<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str()
}

fn items(list: &Value) -> Vec<&Value> {
    list["items"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// One status condition: its status, reason and message, and whether the controller has seen the object's latest edit
/// (`observedGeneration` equals `metadata.generation`).
fn condition(obj: &Value, conds: &Value, kind: &str) -> Value {
    let c = conds
        .as_array()
        .and_then(|a| a.iter().find(|c| c["type"] == kind));
    let Some(c) = c else {
        return Value::Null;
    };
    let generation = obj["metadata"]["generation"].as_i64();
    let observed = c["observedGeneration"].as_i64();
    json!({
        "status": c["status"].as_str().unwrap_or("Unknown"),
        "reason": c["reason"],
        "message": c["message"],
        "current": observed.is_some() && observed == generation,
    })
}

fn is_true(c: &Value) -> bool {
    c["status"] == "True"
}

fn is_current(c: &Value) -> bool {
    c["current"] == true
}

/// `ready`, `waiting` or `problem`, with a plain sentence. `checks` are (label, condition) in the order they matter.
fn judge(checks: &[(&str, &Value)]) -> (&'static str, String) {
    for (label, c) in checks {
        if c.is_null() || !is_current(c) {
            return ("waiting", "Waiting for the controller".into());
        }
        if !is_true(c) {
            let reason = c["reason"].as_str().unwrap_or("no reason given");
            let what = match *label {
                "Accepted" => "Not accepted",
                "ResolvedRefs" => "Backend problem",
                _ => "Not ready",
            };
            return ("problem", format!("{what}: {reason}"));
        }
    }
    ("ready", "Ready".into())
}

fn match_text(m: &Value) -> String {
    let mut parts = vec![];
    if let Some(method) = m["method"].as_str() {
        parts.push(method.to_owned());
    }
    if let Some(v) = m["path"]["value"].as_str() {
        parts.push(format!("{} {v}", m["path"]["type"].as_str().unwrap_or("PathPrefix")));
    }
    let h = m["headers"].as_array().map_or(0, Vec::len);
    if h > 0 {
        parts.push(format!("{h} header{}", if h == 1 { "" } else { "s" }));
    }
    if parts.is_empty() {
        "everything".into()
    } else {
        parts.join(" ")
    }
}

/// Ready endpoints behind a Service: endpoints of its EndpointSlices whose `ready` condition is not false.
fn ready_endpoints(slices: &[&Value], ns: &str, service: &str) -> u64 {
    slices
        .iter()
        .filter(|sl| {
            s(sl, &["metadata", "namespace"]) == Some(ns)
                && s(sl, &["metadata", "labels", "kubernetes.io/service-name"]) == Some(service)
        })
        .flat_map(|sl| sl["endpoints"].as_array().into_iter().flatten())
        .filter(|e| e["conditions"]["ready"] != false)
        .count() as u64
}

fn route_for(route: &Value, gw_ns: &str, gw: &str, slices: &[&Value]) -> Value {
    let ns = s(route, &["metadata", "namespace"]).unwrap_or("default");
    // The controller's verdict for this Gateway: the status parent that names it.
    let parent = route["status"]["parents"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| {
            s(p, &["parentRef", "name"]) == Some(gw)
                && s(p, &["parentRef", "namespace"]).unwrap_or(ns) == gw_ns
        });
    let conds = parent.map_or(&Value::Null, |p| &p["conditions"]);
    let accepted = condition(route, conds, "Accepted");
    let resolved = condition(route, conds, "ResolvedRefs");
    let (state, summary) = judge(&[("Accepted", &accepted), ("ResolvedRefs", &resolved)]);
    let mut matches = vec![];
    let mut backends = vec![];
    for rule in route["spec"]["rules"].as_array().into_iter().flatten() {
        let ms = rule["matches"].as_array().cloned().unwrap_or_default();
        if ms.is_empty() {
            matches.push("everything".to_owned());
        }
        matches.extend(ms.iter().map(match_text));
        for b in rule["backendRefs"].as_array().into_iter().flatten() {
            let name = b["name"].as_str().unwrap_or("");
            let bns = b["namespace"].as_str().unwrap_or(ns);
            backends.push(json!({
                "namespace": bns,
                "name": name,
                "port": b["port"],
                "ready_endpoints": ready_endpoints(slices, bns, name),
            }));
        }
    }
    matches.dedup();
    json!({
        "namespace": ns,
        "name": s(route, &["metadata", "name"]),
        "hostnames": route["spec"]["hostnames"].as_array().cloned().unwrap_or_default(),
        "matches": matches,
        "accepted": accepted,
        "resolved_refs": resolved,
        "state": state,
        "summary": summary,
        "backends": backends,
        "inspect": format!("fluxctl gateway inspect {} -n {ns}", s(route, &["metadata", "name"]).unwrap_or("")),
    })
}

fn attached(route: &Value, gw_ns: &str, gw: &str) -> bool {
    let rns = s(route, &["metadata", "namespace"]).unwrap_or("default");
    route["spec"]["parentRefs"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|p| p["name"] == gw && p["namespace"].as_str().unwrap_or(rns) == gw_ns)
}

/// The answer for lists read from the cluster. `envoy` is the `envoy-gateway` Deployment when it exists.
pub(crate) fn parse(gateways: &Value, routes: &Value, slices: &Value, envoy: Option<&Value>) -> Value {
    let slices = items(slices);
    let routes = items(routes);
    let mut out: Vec<Value> = items(gateways)
        .into_iter()
        .map(|g| {
            let ns = s(g, &["metadata", "namespace"]).unwrap_or("default");
            let name = s(g, &["metadata", "name"]).unwrap_or("");
            let conds = &g["status"]["conditions"];
            let accepted = condition(g, conds, "Accepted");
            let programmed = condition(g, conds, "Programmed");
            let (state, summary) = judge(&[("Accepted", &accepted), ("Programmed", &programmed)]);
            let listeners: Vec<Value> = g["spec"]["listeners"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| json!({"name": l["name"], "port": l["port"], "protocol": l["protocol"], "hostname": l["hostname"]}))
                .collect();
            let mut rs: Vec<Value> = routes
                .iter()
                .filter(|r| attached(r, ns, name))
                .map(|r| route_for(r, ns, name, &slices))
                .collect();
            rs.sort_by(|a, b| (a["namespace"].as_str(), a["name"].as_str()).cmp(&(b["namespace"].as_str(), b["name"].as_str())));
            json!({
                "namespace": ns,
                "name": name,
                "class": s(g, &["spec", "gatewayClassName"]),
                "listeners": listeners,
                "accepted": accepted,
                "programmed": programmed,
                "state": state,
                "summary": summary,
                "routes": rs,
            })
        })
        .collect();
    out.sort_by(|a, b| (a["namespace"].as_str(), a["name"].as_str()).cmp(&(b["namespace"].as_str(), b["name"].as_str())));
    json!({
        "schema": 1,
        "gateway_api_installed": true,
        "envoy_gateway": {
            "installed": envoy.is_some(),
            "ready": envoy.is_some_and(|d| d["status"]["readyReplicas"].as_u64().unwrap_or(0) > 0),
        },
        "gateways": out,
        "error": null,
    })
}

/// The answer when the cluster could not be read: a plain sentence and the command to run next.
pub(crate) fn failure(cluster: &str, message: &str) -> Value {
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
            format!("Could not read the cluster's Gateways: {message}."),
            "fluxctl doctor".to_owned(),
        )
    };
    json!({
        "schema": 1,
        "gateway_api_installed": null,
        "envoy_gateway": {"installed": false, "ready": false},
        "gateways": [],
        "error": {"message": sentence, "next": next},
    })
}

/// The API server answers 404 for a resource group that is not installed.
fn not_installed(e: &crate::ApiError) -> bool {
    e.message.contains("404") && !e.message.contains("could not reach")
}

/// `GET /v1/kube/{cluster}/gateways`
pub(crate) async fn get_gateways(
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
    let (gws, routes, slices, envoy) = tokio::join!(
        kube::get_json(&target, GW_PATH),
        kube::get_json(&target, ROUTE_PATH),
        kube::get_json(&target, SLICE_PATH),
        kube::get_json(&target, ENVOY_PATH),
    );
    let envoy = envoy.ok();
    let (gws, routes) = match (gws, routes) {
        (Ok(g), Ok(r)) => (g, r),
        (Err(e), _) | (_, Err(e)) if not_installed(&e) => {
            let mut v = parse(&json!({}), &json!({}), &json!({}), envoy.as_ref());
            v["gateway_api_installed"] = json!(false);
            v["cluster"] = json!(name);
            return Ok(Json(v));
        }
        (Err(e), _) | (_, Err(e)) => return Ok(Json(failure(&name, &e.message))),
    };
    let mut v = parse(&gws, &routes, &slices.unwrap_or_else(|_| json!({})), envoy.as_ref());
    v["cluster"] = json!(name);
    Ok(Json(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(t: &str, status: &str, reason: &str, gen_: i64) -> Value {
        json!({"type": t, "status": status, "reason": reason, "observedGeneration": gen_})
    }

    fn fixtures() -> (Value, Value, Value) {
        let gws = json!({"items": [{
            "metadata": {"namespace": "default", "name": "eg", "generation": 2},
            "spec": {"gatewayClassName": "eg", "listeners": [{"name": "http", "port": 80, "protocol": "HTTP", "hostname": "app.example.com"}]},
            "status": {"conditions": [cond("Accepted", "True", "Accepted", 2), cond("Programmed", "True", "Programmed", 2)]}
        }]});
        let routes = json!({"items": [
            {"metadata": {"namespace": "shop", "name": "web", "generation": 1},
             "spec": {"hostnames": ["app.example.com"],
                      "parentRefs": [{"name": "eg", "namespace": "default"}],
                      "rules": [{"matches": [{"path": {"type": "PathPrefix", "value": "/api"}, "method": "GET"}],
                                 "backendRefs": [{"name": "api", "port": 8080}]}]},
             "status": {"parents": [{"parentRef": {"name": "eg", "namespace": "default"},
                        "conditions": [cond("Accepted", "True", "Accepted", 1), cond("ResolvedRefs", "True", "ResolvedRefs", 1)]}]}},
            {"metadata": {"namespace": "shop", "name": "bad", "generation": 3},
             "spec": {"parentRefs": [{"name": "eg", "namespace": "default"}], "rules": [{"backendRefs": [{"name": "gone", "port": 80}]}]},
             "status": {"parents": [{"parentRef": {"name": "eg", "namespace": "default"},
                        "conditions": [cond("Accepted", "True", "Accepted", 3), cond("ResolvedRefs", "False", "BackendNotFound", 3)]}]}},
            {"metadata": {"namespace": "shop", "name": "elsewhere"}, "spec": {"parentRefs": [{"name": "other"}]}}
        ]});
        let slices = json!({"items": [
            {"metadata": {"namespace": "shop", "labels": {"kubernetes.io/service-name": "api"}},
             "endpoints": [{"conditions": {"ready": true}}, {"conditions": {"ready": false}}, {"conditions": {}}]},
            {"metadata": {"namespace": "other", "labels": {"kubernetes.io/service-name": "api"}}, "endpoints": [{"conditions": {"ready": true}}]}
        ]});
        (gws, routes, slices)
    }

    #[test]
    fn a_healthy_gateway_and_route_are_ready_with_endpoint_counts() {
        let (g, r, sl) = fixtures();
        let v = parse(&g, &r, &sl, Some(&json!({"status": {"readyReplicas": 1}})));
        assert_eq!(v["schema"], 1);
        assert_eq!(v["envoy_gateway"], json!({"installed": true, "ready": true}));
        let gw = &v["gateways"][0];
        assert_eq!(gw["state"], "ready");
        assert_eq!(gw["listeners"][0]["port"], 80);
        assert_eq!(gw["programmed"]["current"], true);
        assert_eq!(gw["routes"].as_array().unwrap().len(), 2, "the route for another gateway is not attached");
        let web = &gw["routes"][1];
        assert_eq!(web["name"], "web");
        assert_eq!(web["state"], "ready");
        assert_eq!(web["matches"][0], "GET PathPrefix /api");
        assert_eq!(web["backends"][0]["ready_endpoints"], 2, "a not-ready endpoint and another namespace's slice do not count");
        assert_eq!(web["inspect"], "fluxctl gateway inspect web -n shop");
    }

    #[test]
    fn an_unresolved_backend_is_a_problem_with_the_reason() {
        let (g, r, sl) = fixtures();
        let v = parse(&g, &r, &sl, None);
        let bad = &v["gateways"][0]["routes"][0];
        assert_eq!(bad["name"], "bad");
        assert_eq!(bad["state"], "problem");
        assert_eq!(bad["summary"], "Backend problem: BackendNotFound");
        assert_eq!(bad["backends"][0]["ready_endpoints"], 0);
        assert_eq!(v["envoy_gateway"]["installed"], false);
    }

    #[test]
    fn a_stale_or_missing_condition_means_waiting_for_the_controller() {
        let (_, r, sl) = fixtures();
        let g = json!({"items": [{"metadata": {"namespace": "default", "name": "eg", "generation": 5},
            "spec": {"listeners": []},
            "status": {"conditions": [cond("Accepted", "True", "Accepted", 4)]}}]});
        let v = parse(&g, &r, &sl, None);
        let gw = &v["gateways"][0];
        assert_eq!(gw["accepted"]["current"], false);
        assert_eq!(gw["state"], "waiting");
        assert_eq!(gw["summary"], "Waiting for the controller");
        let none = json!({"items": [{"metadata": {"namespace": "default", "name": "eg"}, "spec": {}}]});
        assert_eq!(parse(&none, &json!({}), &json!({}), None)["gateways"][0]["state"], "waiting");
    }

    #[test]
    fn a_refused_gateway_says_not_accepted_with_the_reason() {
        let g = json!({"items": [{"metadata": {"namespace": "d", "name": "eg", "generation": 1},
            "status": {"conditions": [cond("Accepted", "False", "InvalidParameters", 1)]}}]});
        let v = parse(&g, &json!({}), &json!({}), None);
        assert_eq!(v["gateways"][0]["summary"], "Not accepted: InvalidParameters");
    }

    #[test]
    fn failure_is_a_sentence_and_a_command() {
        let v = failure("dev", "the server VM dev is stopped");
        assert_eq!(v["schema"], 1);
        assert_eq!(v["error"]["next"], "fluxctl kube up dev");
        assert!(v["error"]["message"].as_str().unwrap().contains("not running"));
        assert_eq!(failure("dev", "timeout")["error"]["next"], "fluxctl doctor");
        assert!(v["gateways"].as_array().unwrap().is_empty());
    }
}
