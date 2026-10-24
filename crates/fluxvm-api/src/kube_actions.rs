// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The few things the Kubernetes page changes in a cluster, all admin only and all small:
//!
//! * `POST /v1/kube/{cluster}/pods/{ns}/{pod}/delete` deletes a pod (its controller makes a new one),
//! * `POST /v1/kube/{cluster}/workloads/{ns}/{kind}/{name}/scale` with `{"replicas": N}` (Deployments and StatefulSets),
//! * `POST /v1/kube/{cluster}/workloads/{ns}/{kind}/{name}/restart` rolls a Deployment, StatefulSet or DaemonSet the way
//!   `kubectl rollout restart` does,
//! * `POST /v1/kube/{cluster}/nodes/{node}/{cordon|uncordon}` marks a node (un)schedulable.
//!
//! `kind` is the plural, `deployments`, `statefulsets` or `daemonsets`. Each request is built by [`plan`], which checks every name
//! before it can reach a URL, then sent with the same client certificate and address fallback as the reads.

use crate::kube::{self, API_NAME, Target, err};
use crate::{ApiError, ApiResult, Role, TokenTenant, require_admin};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use fluxvm_scheduler::VmManager;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_REPLICAS: u64 = 100;
const MERGE_PATCH: &str = "application/merge-patch+json";

/// What to ask the API server.
#[derive(Debug, PartialEq)]
pub(crate) struct Request {
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
    /// What to tell the user happened.
    pub verb: &'static str,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Action<'a> {
    DeletePod {
        ns: &'a str,
        pod: &'a str,
    },
    Scale {
        ns: &'a str,
        kind: &'a str,
        name: &'a str,
        replicas: u64,
    },
    Restart {
        ns: &'a str,
        kind: &'a str,
        name: &'a str,
    },
    Cordon {
        node: &'a str,
        on: bool,
    },
}

fn bad(message: &str) -> ApiError {
    err(StatusCode::BAD_REQUEST, message)
}

/// Kinds the page may touch, and whether they scale.
fn kind_ok(kind: &str) -> Option<bool> {
    match kind {
        "deployments" | "statefulsets" => Some(true),
        "daemonsets" => Some(false),
        _ => None,
    }
}

/// Checks the action's names and numbers and turns it into the call to make. `now` is the time to stamp a restart with.
pub(crate) fn plan(action: &Action, now: &str) -> Result<Request, ApiError> {
    let names = |list: &[&str]| {
        if list.iter().all(|n| crate::kube_pod::k8s_name_ok(n)) {
            Ok(())
        } else {
            Err(bad("not a namespace, pod, workload or node name"))
        }
    };
    match *action {
        Action::DeletePod { ns, pod } => {
            names(&[ns, pod])?;
            Ok(Request {
                method: Method::DELETE,
                path: format!("/api/v1/namespaces/{ns}/pods/{pod}"),
                body: None,
                verb: "deleted",
            })
        }
        Action::Scale {
            ns,
            kind,
            name,
            replicas,
        } => {
            names(&[ns, name])?;
            if kind_ok(kind) != Some(true) {
                return Err(bad("only deployments and statefulsets can be scaled"));
            }
            if replicas > MAX_REPLICAS {
                return Err(bad(&format!("at most {MAX_REPLICAS} replicas")));
            }
            Ok(Request {
                method: Method::PATCH,
                path: format!("/apis/apps/v1/namespaces/{ns}/{kind}/{name}/scale"),
                body: Some(json!({"spec": {"replicas": replicas}})),
                verb: "scaled",
            })
        }
        Action::Restart { ns, kind, name } => {
            names(&[ns, name])?;
            if kind_ok(kind).is_none() {
                return Err(bad(
                    "only deployments, statefulsets and daemonsets can be restarted",
                ));
            }
            Ok(Request {
                method: Method::PATCH,
                path: format!("/apis/apps/v1/namespaces/{ns}/{kind}/{name}"),
                body: Some(
                    json!({"spec": {"template": {"metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": now}}}}}),
                ),
                verb: "restarted",
            })
        }
        Action::Cordon { node, on } => {
            names(&[node])?;
            Ok(Request {
                method: Method::PATCH,
                path: format!("/api/v1/nodes/{node}"),
                body: Some(json!({"spec": {"unschedulable": on}})),
                verb: if on { "cordoned" } else { "uncordoned" },
            })
        }
    }
}

/// The message in a Kubernetes `Status` answer, if that is what the body is.
fn status_message(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("message")?
        .as_str()
        .map(str::to_owned)
}

/// What the user is told when the API server answered `status` with `body`.
pub(crate) fn refusal(status: u16, body: &[u8]) -> ApiError {
    let message = status_message(body).unwrap_or_else(|| format!("the cluster answered {status}"));
    let code = match status {
        400 | 422 => StatusCode::BAD_REQUEST,
        403 => StatusCode::FORBIDDEN,
        404 => StatusCode::NOT_FOUND,
        409 => StatusCode::CONFLICT,
        _ => StatusCode::BAD_GATEWAY,
    };
    err(code, message)
}

async fn send(target: &Target, req: &Request) -> Result<(), ApiError> {
    let mut tried = vec![];
    for &addr in &target.addrs {
        let client = kube::http_client(&target.creds, addr)?;
        let url = format!("https://{API_NAME}:{}{}", addr.port(), req.path);
        let mut call = client.request(req.method.clone(), &url);
        if let Some(body) = &req.body {
            call = call
                .header("content-type", MERGE_PATCH)
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
        let body = resp.bytes().await.unwrap_or_default();
        return if status.is_success() {
            Ok(())
        } else {
            Err(refusal(status.as_u16(), &body))
        };
    }
    Err(err(StatusCode::BAD_GATEWAY, tried.join("; ")))
}

async fn run(
    m: &Arc<VmManager>,
    role: Role,
    tenant: Option<String>,
    cluster: &str,
    action: Action<'_>,
) -> ApiResult<Json<Value>> {
    require_admin(role)?;
    let req = plan(&action, &chrono::Utc::now().to_rfc3339())?;
    let target = kube::resolve(m, tenant.as_deref(), cluster).await?;
    send(&target, &req).await?;
    Ok(Json(
        json!({"cluster": cluster, "path": req.path, "result": req.verb}),
    ))
}

/// `POST /v1/kube/{cluster}/pods/{ns}/{pod}/delete`
pub(crate) async fn delete_pod(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((cluster, ns, pod)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    run(
        &m,
        role,
        tenant,
        &cluster,
        Action::DeletePod { ns: &ns, pod: &pod },
    )
    .await
}

#[derive(Debug, Deserialize)]
pub(crate) struct ScaleBody {
    replicas: u64,
}

/// `POST /v1/kube/{cluster}/workloads/{ns}/{kind}/{name}/scale`
pub(crate) async fn scale(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((cluster, ns, kind, name)): Path<(String, String, String, String)>,
    Json(body): Json<ScaleBody>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let action = Action::Scale {
        ns: &ns,
        kind: &kind,
        name: &name,
        replicas: body.replicas,
    };
    run(&m, role, tenant, &cluster, action).await
}

/// `POST /v1/kube/{cluster}/workloads/{ns}/{kind}/{name}/restart`
pub(crate) async fn restart(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((cluster, ns, kind, name)): Path<(String, String, String, String)>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    run(
        &m,
        role,
        tenant,
        &cluster,
        Action::Restart {
            ns: &ns,
            kind: &kind,
            name: &name,
        },
    )
    .await
}

/// `POST /v1/kube/{cluster}/nodes/{node}/{cordon|uncordon}`
pub(crate) async fn cordon(
    State(m): State<Arc<VmManager>>,
    Extension(role): Extension<Role>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path((cluster, node, what)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let on = match what.as_str() {
        "cordon" => true,
        "uncordon" => false,
        _ => return Err(bad("the action is cordon or uncordon")),
    };
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    run(
        &m,
        role,
        tenant,
        &cluster,
        Action::Cordon { node: &node, on },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-10-25T10:00:00+00:00";

    #[test]
    fn a_pod_is_deleted_by_its_names_only() {
        let r = plan(
            &Action::DeletePod {
                ns: "default",
                pod: "web-7d9c",
            },
            NOW,
        )
        .unwrap();
        assert_eq!(
            (r.method, r.path.as_str(), r.body),
            (
                Method::DELETE,
                "/api/v1/namespaces/default/pods/web-7d9c",
                None
            )
        );
        for (ns, pod) in [
            ("a/b", "x"),
            ("default", "../x"),
            ("default", ""),
            ("Default", "x"),
            ("d", "x?y"),
        ] {
            assert_eq!(
                plan(&Action::DeletePod { ns, pod }, NOW)
                    .unwrap_err()
                    .status,
                StatusCode::BAD_REQUEST,
                "{ns}/{pod}"
            );
        }
    }

    #[test]
    fn scaling_is_bounded_and_only_for_kinds_that_scale() {
        let ok = plan(
            &Action::Scale {
                ns: "default",
                kind: "deployments",
                name: "web",
                replicas: 3,
            },
            NOW,
        )
        .unwrap();
        assert_eq!(
            ok.path,
            "/apis/apps/v1/namespaces/default/deployments/web/scale"
        );
        assert_eq!(ok.body, Some(json!({"spec": {"replicas": 3}})));
        assert!(
            plan(
                &Action::Scale {
                    ns: "d",
                    kind: "statefulsets",
                    name: "db",
                    replicas: 0
                },
                NOW
            )
            .is_ok()
        );
        for (kind, n) in [("daemonsets", 1), ("pods", 1), ("deployments", 101)] {
            let a = Action::Scale {
                ns: "d",
                kind,
                name: "x",
                replicas: n,
            };
            assert_eq!(
                plan(&a, NOW).unwrap_err().status,
                StatusCode::BAD_REQUEST,
                "{kind} {n}"
            );
        }
    }

    #[test]
    fn a_restart_stamps_the_pod_template_like_kubectl() {
        let r = plan(
            &Action::Restart {
                ns: "kube-system",
                kind: "daemonsets",
                name: "cilium",
            },
            NOW,
        )
        .unwrap();
        assert_eq!(
            r.path,
            "/apis/apps/v1/namespaces/kube-system/daemonsets/cilium"
        );
        assert_eq!(
            r.body.unwrap()["spec"]["template"]["metadata"]["annotations"]["kubectl.kubernetes.io/restartedAt"],
            NOW
        );
        assert!(
            plan(
                &Action::Restart {
                    ns: "d",
                    kind: "jobs",
                    name: "x"
                },
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn a_node_is_cordoned_and_released() {
        let on = plan(
            &Action::Cordon {
                node: "fluxvm-kube",
                on: true,
            },
            NOW,
        )
        .unwrap();
        assert_eq!(
            (on.path.as_str(), on.body.clone(), on.verb),
            (
                "/api/v1/nodes/fluxvm-kube",
                Some(json!({"spec": {"unschedulable": true}})),
                "cordoned"
            )
        );
        let off = plan(
            &Action::Cordon {
                node: "fluxvm-kube",
                on: false,
            },
            NOW,
        )
        .unwrap();
        assert_eq!(
            (off.body, off.verb),
            (
                Some(json!({"spec": {"unschedulable": false}})),
                "uncordoned"
            )
        );
    }

    #[test]
    fn the_clusters_refusals_keep_their_words() {
        let body =
            br#"{"kind":"Status","message":"deployments.apps \"web\" not found","code":404}"#;
        let e = refusal(404, body);
        assert_eq!(
            (e.status, e.message.as_str()),
            (StatusCode::NOT_FOUND, "deployments.apps \"web\" not found")
        );
        assert_eq!(refusal(409, b"x").status, StatusCode::CONFLICT);
        assert_eq!(refusal(500, b"").message, "the cluster answered 500");
    }

    /// Against a real cluster: `FLUXVM_KUBE_LIVE_KUBECONFIG=~/.kube/fluxvm-NAME.yaml cargo test -p fluxvm-api --lib kube_actions::tests::
    /// kube_live -- --ignored --nocapture`, with the kubeconfig's loopback port reachable. Scales, restarts and deletes in `kube-system`
    /// (CoreDNS) and cordons the first node, then puts everything back.
    #[tokio::test]
    #[ignore = "needs a cluster"]
    async fn kube_live_the_actions_change_a_real_cluster() {
        let path =
            std::env::var("FLUXVM_KUBE_LIVE_KUBECONFIG").expect("set FLUXVM_KUBE_LIVE_KUBECONFIG");
        let creds = kube::parse_kubeconfig(&std::fs::read_to_string(path).unwrap()).unwrap();
        let addr = creds.server.expect("a loopback server in the kubeconfig");
        let target = Target {
            creds,
            addrs: vec![addr],
            vms: vec![],
        };
        let go = |a: Action<'static>| {
            let target = &target;
            async move { send(target, &plan(&a, &chrono::Utc::now().to_rfc3339()).unwrap()).await }
        };
        let node = kube::get_json(&target, "/api/v1/nodes").await.unwrap()["items"][0]["metadata"]
            ["name"]
            .as_str()
            .unwrap()
            .to_owned();
        let node: &'static str = Box::leak(node.into_boxed_str());
        go(Action::Scale {
            ns: "kube-system",
            kind: "deployments",
            name: "coredns",
            replicas: 2,
        })
        .await
        .unwrap();
        let d = kube::get_json(
            &target,
            "/apis/apps/v1/namespaces/kube-system/deployments/coredns",
        )
        .await
        .unwrap();
        assert_eq!(d["spec"]["replicas"], 2);
        go(Action::Scale {
            ns: "kube-system",
            kind: "deployments",
            name: "coredns",
            replicas: 1,
        })
        .await
        .unwrap();
        go(Action::Restart {
            ns: "kube-system",
            kind: "deployments",
            name: "coredns",
        })
        .await
        .unwrap();
        let d = kube::get_json(
            &target,
            "/apis/apps/v1/namespaces/kube-system/deployments/coredns",
        )
        .await
        .unwrap();
        assert!(
            d["spec"]["template"]["metadata"]["annotations"]["kubectl.kubernetes.io/restartedAt"]
                .is_string()
        );
        go(Action::Cordon { node, on: true }).await.unwrap();
        let n = kube::get_json(&target, &format!("/api/v1/nodes/{node}"))
            .await
            .unwrap();
        assert_eq!(kube::simplify_node(&n)["unschedulable"], true);
        go(Action::Cordon { node, on: false }).await.unwrap();
        let n = kube::get_json(&target, &format!("/api/v1/nodes/{node}"))
            .await
            .unwrap();
        assert_eq!(kube::simplify_node(&n)["unschedulable"], false);
        let pods = kube::get_json(
            &target,
            "/api/v1/namespaces/kube-system/pods?labelSelector=k8s-app%3Dkube-dns",
        )
        .await
        .unwrap();
        let pod: &'static str = Box::leak(
            pods["items"][0]["metadata"]["name"]
                .as_str()
                .unwrap()
                .to_owned()
                .into_boxed_str(),
        );
        go(Action::DeletePod {
            ns: "kube-system",
            pod,
        })
        .await
        .unwrap();
        let gone = go(Action::DeletePod {
            ns: "kube-system",
            pod: "no-such-pod",
        })
        .await
        .unwrap_err();
        assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.message);
        println!(
            "the cluster accepted scale, restart, cordon, uncordon and delete; a missing pod gave: {}",
            gone.message
        );
    }
}
