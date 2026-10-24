// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! What runs in a cluster and what has happened to it, for the Kubernetes page: `GET /v1/kube/{cluster}/workloads` (Deployments,
//! StatefulSets, DaemonSets and Services) and `GET /v1/kube/{cluster}/events`. Read-only, through the same client certificate and
//! address fallback as the pod list.

use crate::kube::{self, err};
use crate::{ApiResult, TokenTenant};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const EVENTS_LIMIT: usize = 500;
const EVENTS_SHOWN: usize = 300;

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
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

fn images(w: &Value) -> Vec<&str> {
    w["spec"]["template"]["spec"]["containers"]
        .as_array()
        .map(|c| c.iter().filter_map(|c| c["image"].as_str()).collect())
        .unwrap_or_default()
}

/// A Deployment or StatefulSet: replicas wanted, ready, up to date and available.
pub(crate) fn simplify_replicated(w: &Value) -> Value {
    let n = |k: &str| w["status"][k].as_u64().unwrap_or(0);
    json!({
        "namespace": str_at(w, &["metadata", "namespace"]),
        "name": str_at(w, &["metadata", "name"]),
        "desired": w["spec"]["replicas"].as_u64().unwrap_or(1),
        "ready": n("readyReplicas"),
        "updated": n("updatedReplicas"),
        "available": n("availableReplicas"),
        "images": images(w),
        "created": str_at(w, &["metadata", "creationTimestamp"]),
    })
}

/// A DaemonSet: one pod per node that matches.
pub(crate) fn simplify_daemonset(w: &Value) -> Value {
    let n = |k: &str| w["status"][k].as_u64().unwrap_or(0);
    json!({
        "namespace": str_at(w, &["metadata", "namespace"]),
        "name": str_at(w, &["metadata", "name"]),
        "desired": n("desiredNumberScheduled"),
        "ready": n("numberReady"),
        "updated": n("updatedNumberScheduled"),
        "available": n("numberAvailable"),
        "images": images(w),
        "created": str_at(w, &["metadata", "creationTimestamp"]),
    })
}

/// A Service: its type, address, where it is reachable from outside, and its ports.
pub(crate) fn simplify_service(s: &Value) -> Value {
    let ports: Vec<Value> = s["spec"]["ports"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| {
                    json!({
                        "name": p["name"],
                        "protocol": p["protocol"].as_str().unwrap_or("TCP"),
                        "port": p["port"],
                        "target_port": p["targetPort"],
                        "node_port": p["nodePort"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut external: Vec<String> = s["spec"]["externalIPs"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    for i in s["status"]["loadBalancer"]["ingress"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(a) = i["ip"].as_str().or_else(|| i["hostname"].as_str()) {
            external.push(a.to_owned());
        }
    }
    let selector: Vec<String> = s["spec"]["selector"]
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| Some(format!("{k}={}", v.as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    let cluster_ip = str_at(s, &["spec", "clusterIP"]).filter(|ip| *ip != "None" && !ip.is_empty());
    json!({
        "namespace": str_at(s, &["metadata", "namespace"]),
        "name": str_at(s, &["metadata", "name"]),
        "type": str_at(s, &["spec", "type"]).unwrap_or("ClusterIP"),
        "cluster_ip": cluster_ip,
        "external": external,
        "ports": ports,
        "selector": selector,
        "created": str_at(s, &["metadata", "creationTimestamp"]),
    })
}

fn sorted(mut v: Vec<Value>) -> Vec<Value> {
    v.sort_by(|a, b| {
        (a["namespace"].as_str(), a["name"].as_str())
            .cmp(&(b["namespace"].as_str(), b["name"].as_str()))
    });
    v
}

/// `GET /v1/kube/{cluster}/workloads`
pub(crate) async fn get_workloads(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target = kube::resolve(&m, tenant.as_deref(), &name).await?;
    let (deployments, statefulsets, daemonsets, services) = tokio::join!(
        kube::get_json(&target, "/apis/apps/v1/deployments"),
        kube::get_json(&target, "/apis/apps/v1/statefulsets"),
        kube::get_json(&target, "/apis/apps/v1/daemonsets"),
        kube::get_json(&target, "/api/v1/services"),
    );
    let (deployments, statefulsets, daemonsets, services) =
        (deployments?, statefulsets?, daemonsets?, services?);
    Ok(Json(json!({
        "cluster": name,
        "deployments": sorted(items(&deployments).into_iter().map(simplify_replicated).collect()),
        "statefulsets": sorted(items(&statefulsets).into_iter().map(simplify_replicated).collect()),
        "daemonsets": sorted(items(&daemonsets).into_iter().map(simplify_daemonset).collect()),
        "services": sorted(items(&services).into_iter().map(simplify_service).collect()),
    })))
}

/// One event the way the UI shows it. Old-style events carry `lastTimestamp` and `source`, newer ones `eventTime`, `series` and
/// `reportingComponent`.
pub(crate) fn simplify_event(e: &Value) -> Value {
    let obj = &e["involvedObject"];
    let first = str_at(e, &["firstTimestamp"])
        .or_else(|| str_at(e, &["eventTime"]))
        .or_else(|| str_at(e, &["metadata", "creationTimestamp"]));
    let last = str_at(e, &["lastTimestamp"])
        .or_else(|| str_at(e, &["series", "lastObservedTime"]))
        .or_else(|| str_at(e, &["eventTime"]))
        .or(first);
    let kind = str_at(obj, &["kind"]).unwrap_or("");
    let name = str_at(obj, &["name"]).unwrap_or("");
    json!({
        "namespace": str_at(e, &["metadata", "namespace"]).or_else(|| str_at(obj, &["namespace"])),
        "type": str_at(e, &["type"]).unwrap_or("Normal"),
        "reason": str_at(e, &["reason"]),
        "message": str_at(e, &["message"]).or_else(|| str_at(e, &["note"])),
        "object": if kind.is_empty() { name.to_owned() } else { format!("{kind}/{name}") },
        "kind": kind,
        "name": name,
        "count": e["count"].as_u64().or_else(|| e["series"]["count"].as_u64()).unwrap_or(1),
        "first": first,
        "last": last,
        "component": str_at(e, &["source", "component"]).or_else(|| str_at(e, &["reportingComponent"])),
    })
}

/// The newest `shown` events, newest first, and how many of all of them are warnings.
pub(crate) fn simplify_events(list: &Value, shown: usize) -> (Vec<Value>, usize) {
    let mut all: Vec<Value> = items(list).into_iter().map(simplify_event).collect();
    let warnings = all.iter().filter(|e| e["type"] == "Warning").count();
    all.sort_by(|a, b| b["last"].as_str().cmp(&a["last"].as_str()));
    all.truncate(shown);
    (all, warnings)
}

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    /// Namespace to read, or all when absent.
    namespace: Option<String>,
}

/// `GET /v1/kube/{cluster}/events[?namespace=]`
pub(crate) async fn get_events(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(name): Path<String>,
    Query(q): Query<EventsQuery>,
) -> ApiResult<Json<Value>> {
    if let Some(ns) = &q.namespace
        && !crate::kube_pod::k8s_name_ok(ns)
    {
        return Err(err(StatusCode::BAD_REQUEST, "not a namespace name"));
    }
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target = kube::resolve(&m, tenant.as_deref(), &name).await?;
    let path = match &q.namespace {
        Some(ns) => format!("/api/v1/namespaces/{ns}/events?limit={EVENTS_LIMIT}"),
        None => format!("/api/v1/events?limit={EVENTS_LIMIT}"),
    };
    let list = kube::get_json(&target, &path).await?;
    let (events, warnings) = simplify_events(&list, EVENTS_SHOWN);
    Ok(Json(json!({
        "cluster": name,
        "events": events,
        "warnings": warnings,
        "truncated": list["metadata"]["continue"].as_str().is_some_and(|c| !c.is_empty()),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployments_and_statefulsets_show_replicas_and_images() {
        let d = json!({"metadata": {"namespace": "default", "name": "web", "creationTimestamp": "t"},
            "spec": {"replicas": 3, "template": {"spec": {"containers": [{"image": "nginx:1.29"}, {"image": "side:1"}]}}},
            "status": {"readyReplicas": 2, "updatedReplicas": 3, "availableReplicas": 2}});
        let s = simplify_replicated(&d);
        assert_eq!(
            (
                s["desired"].as_u64(),
                s["ready"].as_u64(),
                s["updated"].as_u64(),
                s["available"].as_u64()
            ),
            (Some(3), Some(2), Some(3), Some(2))
        );
        assert_eq!(s["images"], json!(["nginx:1.29", "side:1"]));
        // a scaled-to-nothing or brand-new object: the API omits zero fields
        let bare = simplify_replicated(
            &json!({"metadata": {"namespace": "d", "name": "x"}, "spec": {}, "status": {}}),
        );
        assert_eq!(
            (bare["desired"].as_u64(), bare["ready"].as_u64()),
            (Some(1), Some(0))
        );
        let zero =
            simplify_replicated(&json!({"metadata": {"name": "x"}, "spec": {"replicas": 0}}));
        assert_eq!(zero["desired"], 0);
    }

    #[test]
    fn daemonsets_count_nodes() {
        let ds = json!({"metadata": {"namespace": "kube-system", "name": "cilium"},
            "spec": {"template": {"spec": {"containers": [{"image": "quay.io/cilium/cilium:v1"}]}}},
            "status": {"desiredNumberScheduled": 2, "numberReady": 1, "updatedNumberScheduled": 2, "numberAvailable": 1}});
        let s = simplify_daemonset(&ds);
        assert_eq!(
            (
                s["desired"].as_u64(),
                s["ready"].as_u64(),
                s["available"].as_u64()
            ),
            (Some(2), Some(1), Some(1))
        );
    }

    #[test]
    fn services_show_type_ports_and_how_to_reach_them() {
        let svc = json!({"metadata": {"namespace": "default", "name": "web"},
            "spec": {"type": "NodePort", "clusterIP": "10.43.1.2", "selector": {"app": "web"}, "externalIPs": ["1.2.3.4"],
                "ports": [{"name": "http", "protocol": "TCP", "port": 80, "targetPort": 8080, "nodePort": 30080}, {"port": 53, "protocol": "UDP", "targetPort": "dns"}]},
            "status": {"loadBalancer": {"ingress": [{"ip": "10.0.0.9"}, {"hostname": "lb.example"}]}}});
        let s = simplify_service(&svc);
        assert_eq!(
            (s["type"].as_str(), s["cluster_ip"].as_str()),
            (Some("NodePort"), Some("10.43.1.2"))
        );
        assert_eq!(s["external"], json!(["1.2.3.4", "10.0.0.9", "lb.example"]));
        assert_eq!(s["selector"], json!(["app=web"]));
        assert_eq!(
            s["ports"][0],
            json!({"name": "http", "protocol": "TCP", "port": 80, "target_port": 8080, "node_port": 30080})
        );
        assert_eq!(s["ports"][1]["target_port"], "dns");
        assert!(s["ports"][1]["node_port"].is_null());
        let headless =
            simplify_service(&json!({"metadata": {"name": "h"}, "spec": {"clusterIP": "None"}}));
        assert!(headless["cluster_ip"].is_null());
        assert_eq!(headless["type"], "ClusterIP");
    }

    #[test]
    fn events_read_old_and_new_styles_and_sort_newest_first() {
        let old = json!({"metadata": {"namespace": "default", "creationTimestamp": "2026-10-24T10:00:00Z"}, "type": "Warning", "reason": "BackOff",
            "message": "Back-off restarting failed container", "count": 7, "firstTimestamp": "2026-10-24T10:00:00Z", "lastTimestamp": "2026-10-24T10:30:00Z",
            "involvedObject": {"kind": "Pod", "name": "api-1", "namespace": "default"}, "source": {"component": "kubelet"}});
        let new = json!({"metadata": {"namespace": "kube-system", "creationTimestamp": "2026-10-24T10:40:00Z"}, "type": "Normal", "reason": "Scheduled",
            "note": "Successfully assigned", "eventTime": "2026-10-24T10:40:00.123456Z", "series": {"count": 2, "lastObservedTime": "2026-10-24T10:45:00.000000Z"},
            "involvedObject": {"kind": "Pod", "name": "dns-1"}, "reportingComponent": "default-scheduler"});
        let bare = json!({"metadata": {"creationTimestamp": "2026-10-24T09:00:00Z"}, "involvedObject": {"name": "node-a"}});
        let (events, warnings) = simplify_events(&json!({"items": [old, new, bare]}), 10);
        assert_eq!(warnings, 1);
        assert_eq!(
            events
                .iter()
                .map(|e| e["object"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["Pod/dns-1", "Pod/api-1", "node-a"]
        );
        let e = &events[1];
        assert_eq!(
            (
                e["type"].as_str(),
                e["reason"].as_str(),
                e["count"].as_u64(),
                e["component"].as_str()
            ),
            (Some("Warning"), Some("BackOff"), Some(7), Some("kubelet"))
        );
        assert_eq!(events[0]["message"], "Successfully assigned");
        assert_eq!(
            (events[0]["count"].as_u64(), events[0]["component"].as_str()),
            (Some(2), Some("default-scheduler"))
        );
        assert_eq!(
            (events[2]["type"].as_str(), events[2]["count"].as_u64()),
            (Some("Normal"), Some(1))
        );
    }

    #[test]
    fn only_the_newest_events_are_kept_but_all_warnings_are_counted() {
        let many: Vec<Value> = (0..20)
            .map(|i| json!({"metadata": {"creationTimestamp": format!("2026-10-24T10:{i:02}:00Z")}, "type": if i % 2 == 0 { "Warning" } else { "Normal" },
                "involvedObject": {"kind": "Pod", "name": format!("p{i}")}}))
            .collect();
        let (events, warnings) = simplify_events(&json!({"items": many}), 5);
        assert_eq!((events.len(), warnings), (5, 10));
        assert_eq!(events[0]["name"], "p19");
        assert_eq!(simplify_events(&json!({}), 5), (vec![], 0));
    }
}
