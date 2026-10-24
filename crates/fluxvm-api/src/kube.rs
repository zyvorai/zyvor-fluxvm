// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A read-only view of the Kubernetes clusters `fluxctl kube` makes, for the Studio UI.
//!
//! * `GET /v1/kube` lists clusters: the VMs labelled `fluxvm.kube=<cluster>`, grouped, with their status. No cluster
//!   call is made.
//! * `GET /v1/kube/{cluster}` asks the cluster's API server for its nodes and pods and returns them in a small, flat
//!   shape. It reads the credentials from the kubeconfig `fluxctl kube up` wrote (`~/.kube/fluxvm-<cluster>.yaml` of the
//!   user the daemon runs as). It reaches the API through the loopback port the kubeconfig names (the SSH forward
//!   `fluxctl kube up` keeps running) and, failing that, at the server VM's own address: with a private network k3s listens
//!   only on that network and loopback, so the VM address works for a single node but not always for a cluster with
//!   workers. Nothing is written to the cluster.

use crate::{ApiError, ApiResult, TokenTenant};
use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use base64::Engine;
use fluxvm_core::model::{VmRecord, VmStatus};
use fluxvm_scheduler::VmManager;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

/// `fluxctl kube` puts the cluster name here on every node (older clusters used `1` on the server).
const LABEL: &str = "fluxvm.kube";
pub(crate) const API_PORT: u16 = 6443;
/// A name every k3s certificate has, so the connection verifies without the address being in the certificate.
pub(crate) const API_NAME: &str = "kubernetes.default.svc";
const PODS_LIMIT: usize = 500;

/// A cluster name as `fluxctl kube` accepts it: also what keeps it out of file paths.
pub(crate) fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// VMs by cluster, the server (named like the cluster) first and workers after it by name.
pub(crate) fn group(vms: &[VmRecord]) -> BTreeMap<String, Vec<&VmRecord>> {
    let mut out: BTreeMap<String, Vec<&VmRecord>> = BTreeMap::new();
    for vm in vms {
        let Some(value) = vm.labels.get(LABEL) else {
            continue;
        };
        let cluster = if value == "1" { &vm.name } else { value };
        if name_ok(cluster) {
            out.entry(cluster.clone()).or_default().push(vm);
        }
    }
    for (cluster, nodes) in out.iter_mut() {
        nodes.sort_by(|a, b| (a.name != *cluster, &a.name).cmp(&(b.name != *cluster, &b.name)));
    }
    out
}

pub(crate) fn status_name(s: &VmStatus) -> &'static str {
    match s {
        VmStatus::Creating => "creating",
        VmStatus::Running => "running",
        VmStatus::Paused => "paused",
        VmStatus::Stopped => "stopped",
        VmStatus::Failed => "failed",
    }
}

/// What a client certificate login needs, decoded from a kubeconfig.
#[derive(Debug, PartialEq)]
pub(crate) struct Credentials {
    pub ca: Vec<u8>,
    /// Client certificate followed by its private key, PEM.
    pub identity: Vec<u8>,
    /// The loopback address the kubeconfig's `server:` names, where `fluxctl kube` forwards the API. Any other host is
    /// ignored: this file is read from disk, but nothing but the local forward should ever be dialled from it.
    pub server: Option<SocketAddr>,
}

/// The three `*-data` fields of the kubeconfig `fluxctl kube` writes (one base64 line each).
pub(crate) fn parse_kubeconfig(text: &str) -> Option<Credentials> {
    let field = |key: &str| -> Option<Vec<u8>> {
        let line = text
            .lines()
            .map(str::trim)
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?;
        base64::engine::general_purpose::STANDARD
            .decode(line.trim())
            .ok()
    };
    let ca = field("certificate-authority-data")?;
    let mut identity = field("client-certificate-data")?;
    identity.push(b'\n');
    identity.extend(field("client-key-data")?);
    let server = text
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("server:"))
        .and_then(|u| u.trim().strip_prefix("https://"))
        .and_then(|hp| hp.parse::<SocketAddr>().ok())
        .filter(|a| a.ip().is_loopback());
    Some(Credentials {
        ca,
        identity,
        server,
    })
}

fn str_of<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(*p)?;
    }
    cur.as_str()
}

/// One pod in the shape the UI shows: the status is what `kubectl get pods` would print.
pub(crate) fn simplify_pod(p: &Value) -> Value {
    let statuses = p["status"]["containerStatuses"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let total = p["spec"]["containers"].as_array().map_or(0, Vec::len);
    let ready = statuses.iter().filter(|c| c["ready"] == true).count();
    let restarts: u64 = statuses
        .iter()
        .filter_map(|c| c["restartCount"].as_u64())
        .sum();
    let waiting = statuses
        .iter()
        .find_map(|c| str_of(c, &["state", "waiting", "reason"]));
    let failed = statuses.iter().find_map(|c| {
        let t = c.get("state")?.get("terminated")?;
        (t["exitCode"].as_i64()? != 0).then(|| t["reason"].as_str().unwrap_or("Error"))
    });
    let status = if p["metadata"].get("deletionTimestamp").is_some() {
        "Terminating"
    } else if let Some(w) = waiting {
        w
    } else if let Some(f) = failed {
        f
    } else if let Some(r) = str_of(p, &["status", "reason"]) {
        r
    } else {
        str_of(p, &["status", "phase"]).unwrap_or("Unknown")
    };
    let names = |key: &str| -> Vec<&str> {
        p["spec"][key]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["name"].as_str()).collect())
            .unwrap_or_default()
    };
    json!({
        "namespace": str_of(p, &["metadata", "namespace"]),
        "name": str_of(p, &["metadata", "name"]),
        "containers": names("containers"),
        "init_containers": names("initContainers"),
        "status": status,
        "phase": str_of(p, &["status", "phase"]),
        "ready": format!("{ready}/{total}"),
        "restarts": restarts,
        "node": str_of(p, &["spec", "nodeName"]),
        "pod_ip": str_of(p, &["status", "podIP"]),
        "created": str_of(p, &["metadata", "creationTimestamp"]),
    })
}

/// `4005272Ki` -> MiB. Other units are left out rather than guessed.
fn memory_mib(q: &str) -> Option<u64> {
    q.strip_suffix("Ki")?.parse::<u64>().ok().map(|k| k / 1024)
}

pub(crate) fn simplify_node(n: &Value) -> Value {
    let ready = n["status"]["conditions"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["type"] == "Ready"))
        .is_some_and(|c| c["status"] == "True");
    let mut roles: Vec<String> = n["metadata"]["labels"]
        .as_object()
        .map(|l| {
            l.keys()
                .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if roles.is_empty() {
        roles.push("worker".into());
    }
    roles.sort();
    let ip = n["status"]["addresses"].as_array().and_then(|a| {
        a.iter()
            .find(|x| x["type"] == "InternalIP")
            .and_then(|x| x["address"].as_str())
    });
    json!({
        "name": str_of(n, &["metadata", "name"]),
        "ready": ready,
        "roles": roles,
        "version": str_of(n, &["status", "nodeInfo", "kubeletVersion"]),
        "internal_ip": ip,
        "os": str_of(n, &["status", "nodeInfo", "osImage"]),
        "cpu": str_of(n, &["status", "capacity", "cpu"]),
        "memory_mib": str_of(n, &["status", "capacity", "memory"]).and_then(memory_mib),
        "created": str_of(n, &["metadata", "creationTimestamp"]),
    })
}

fn visible(vms: &[VmRecord], tenant: Option<&str>) -> Vec<VmRecord> {
    vms.iter()
        .filter(|vm| tenant.is_none_or(|t| vm.request.tenant.as_deref() == Some(t)))
        .cloned()
        .collect()
}

pub(crate) fn err(status: StatusCode, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        message: message.into(),
    }
}

/// `GET /v1/kube`: the clusters on this host.
pub(crate) async fn list_clusters(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let all = visible(&m.list().await, tenant.as_deref());
    let items: Vec<Value> = group(&all)
        .into_iter()
        .map(|(name, nodes)| {
            json!({
                "name": name,
                "nodes": nodes.iter().map(|vm| json!({
                    "name": vm.name,
                    "status": status_name(&vm.status),
                    "role": if vm.name == name { "server" } else { "worker" },
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({"items": items})))
}

fn kubeconfig_path(name: &str) -> Result<std::path::PathBuf, ApiError> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "HOME is not set for the daemon",
        )
    })?;
    Ok(std::path::PathBuf::from(home)
        .join(".kube")
        .join(format!("fluxvm-{name}.yaml")))
}

/// An error and its causes, `a: b: c`: reqwest's own message stops at "error sending request".
pub(crate) fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

async fn fetch(client: &reqwest::Client, port: u16, path: &str) -> Result<Value, ApiError> {
    let url = format!("https://{API_NAME}:{port}{path}");
    let resp = client.get(&url).send().await.map_err(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            format!("could not reach the cluster's API server: {}", chain(&e)),
        )
    })?;
    if !resp.status().is_success() {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!(
                "the cluster's API server answered {} for {path}",
                resp.status()
            ),
        ));
    }
    resp.json::<Value>().await.map_err(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            format!("the cluster's answer for {path} was not JSON: {e}"),
        )
    })
}

/// An HTTPS client for the API server at `addr`: the cluster's own CA is the only trust root, the kubeconfig's client
/// certificate authenticates, and `API_NAME` (a name every k3s certificate carries) is what the connection is checked
/// against.
pub(crate) fn http_client(
    creds: &Credentials,
    addr: SocketAddr,
) -> Result<reqwest::Client, ApiError> {
    reqwest::Client::builder()
        .no_proxy()
        .tls_certs_only([reqwest::Certificate::from_pem(&creds.ca).map_err(|e| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("bad CA in the kubeconfig: {e}"),
            )
        })?])
        .identity(reqwest::Identity::from_pem(&creds.identity).map_err(|e| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("bad client certificate in the kubeconfig: {e}"),
            )
        })?)
        .resolve(API_NAME, addr)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("building the HTTPS client: {e}"),
            )
        })
}

/// Connects to the API server at `addr` and returns its nodes and pods.
async fn read(creds: &Credentials, addr: SocketAddr) -> Result<(Value, Value), ApiError> {
    let client = http_client(creds, addr)?;
    let pods_path = format!("/api/v1/pods?limit={PODS_LIMIT}");
    let port = addr.port();
    let (nodes, pods) = tokio::join!(
        fetch(&client, port, "/api/v1/nodes"),
        fetch(&client, port, &pods_path),
    );
    Ok((nodes?, pods?))
}

/// Everything needed to talk to one cluster's API server.
pub(crate) struct Target {
    pub creds: Credentials,
    /// Where to try, in order: the kubeconfig's loopback forward, then the server VM's own address.
    pub addrs: Vec<SocketAddr>,
    pub vms: Vec<Value>,
}

/// Finds the cluster among the VMs `tenant` may see, checks its server is running and reads its kubeconfig.
pub(crate) async fn resolve(
    m: &Arc<VmManager>,
    tenant: Option<&str>,
    name: &str,
) -> Result<Target, ApiError> {
    if !name_ok(name) {
        return Err(err(StatusCode::BAD_REQUEST, "not a cluster name"));
    }
    let all = visible(&m.list().await, tenant);
    let groups = group(&all);
    let nodes = groups.get(name).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            format!("no Kubernetes cluster named {name}"),
        )
    })?;
    let server = nodes
        .iter()
        .find(|vm| vm.name == name)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("{name} has no server VM")))?;
    if server.status != VmStatus::Running {
        return Err(err(
            StatusCode::CONFLICT,
            format!("the server VM {name} is {}", status_name(&server.status)),
        ));
    }
    let ip: IpAddr = m
        .get(server.id)
        .await?
        .guest_ip
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| err(StatusCode::CONFLICT, "the server VM has no address yet"))?;
    let path = kubeconfig_path(name)?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        err(
            StatusCode::NOT_FOUND,
            format!(
                "no kubeconfig at {} for the daemon's user; `fluxctl kube up` writes it",
                path.display()
            ),
        )
    })?;
    let creds = parse_kubeconfig(&text).ok_or_else(|| {
        err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the kubeconfig has no client certificate",
        )
    })?;
    let addrs = creds
        .server
        .into_iter()
        .chain([SocketAddr::new(ip, API_PORT)])
        .collect();
    let vms = nodes
        .iter()
        .map(|vm| json!({"name": vm.name, "status": status_name(&vm.status)}))
        .collect();
    Ok(Target { creds, addrs, vms })
}

/// `GET /v1/kube/{cluster}`: its nodes and pods.
pub(crate) async fn get_cluster(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target = resolve(&m, tenant.as_deref(), &name).await?;
    let mut tried = vec![];
    let mut found = None;
    for &addr in &target.addrs {
        match read(&target.creds, addr).await {
            Ok(both) => {
                found = Some((addr, both));
                break;
            }
            Err(e) => tried.push(format!("{addr}: {}", e.message)),
        }
    }
    let Some((addr, (node_list, pod_list))) = found else {
        return Err(err(StatusCode::BAD_GATEWAY, tried.join("; ")));
    };
    let items = |v: &Value| v["items"].as_array().cloned().unwrap_or_default();
    Ok(Json(json!({
        "cluster": name,
        "api": format!("https://{addr}"),
        "vms": target.vms,
        "nodes": items(&node_list).iter().map(simplify_node).collect::<Vec<_>>(),
        "pods": items(&pod_list).iter().map(simplify_pod).collect::<Vec<_>>(),
        "truncated": pod_list["metadata"].get("continue").is_some_and(|c| c.as_str().is_some_and(|s| !s.is_empty())),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_names_are_plain() {
        for ok in ["fluxvm-kube", "a", "k8s-1"] {
            assert!(name_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            "A",
            "a_b",
            "../x",
            "a/b",
            "a.b",
            &"x".repeat(41),
        ] {
            assert!(!name_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn kubeconfig_credentials_are_decoded() {
        let b = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let text = format!(
            "apiVersion: v1\nclusters:\n- cluster:\n    certificate-authority-data: {}\n    server: https://127.0.0.1:6443\n  name: dev\nusers:\n- name: dev\n  user:\n    client-certificate-data: {}\n    client-key-data: {}\n",
            b("CA-PEM"),
            b("CERT-PEM"),
            b("KEY-PEM")
        );
        let c = parse_kubeconfig(&text).unwrap();
        assert_eq!(c.ca, b"CA-PEM");
        assert_eq!(c.server, Some("127.0.0.1:6443".parse().unwrap()));
        // Only a loopback server is ever dialled from the file.
        let remote = text.replace("https://127.0.0.1:6443", "https://10.0.0.9:6443");
        assert_eq!(parse_kubeconfig(&remote).unwrap().server, None);
        let named = text.replace("https://127.0.0.1:6443", "https://example.com:6443");
        assert_eq!(parse_kubeconfig(&named).unwrap().server, None);
        assert_eq!(c.identity, b"CERT-PEM\nKEY-PEM");
        assert!(parse_kubeconfig("apiVersion: v1\n").is_none());
        assert!(parse_kubeconfig(&text.replace("client-key-data", "other")).is_none());
        assert!(parse_kubeconfig(&text.replace(&b("CA-PEM"), "!!!not base64")).is_none());
    }

    #[test]
    fn pods_read_like_kubectl() {
        let pod = |status: Value, deleting: bool| {
            let mut md = json!({"namespace": "default", "name": "web", "creationTimestamp": "2026-10-24T12:00:00Z"});
            if deleting {
                md["deletionTimestamp"] = json!("2026-10-24T12:05:00Z");
            }
            json!({"metadata": md, "spec": {"nodeName": "n1", "containers": [{}, {}]}, "status": status})
        };
        let running = simplify_pod(&pod(
            json!({"phase": "Running", "podIP": "10.42.0.5", "containerStatuses": [
                {"ready": true, "restartCount": 1, "state": {"running": {}}},
                {"ready": false, "restartCount": 2, "state": {"running": {}}}]}),
            false,
        ));
        assert_eq!(running["status"], "Running");
        assert_eq!(running["ready"], "1/2");
        assert_eq!(running["restarts"], 3);
        assert_eq!(running["containers"], json!([]));
        assert_eq!(
            (running["node"].as_str(), running["pod_ip"].as_str()),
            (Some("n1"), Some("10.42.0.5"))
        );
        let crash = simplify_pod(&pod(
            json!({"phase": "Running", "containerStatuses": [
                {"ready": false, "restartCount": 7, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}),
            false,
        ));
        assert_eq!(crash["status"], "CrashLoopBackOff");
        let oom = simplify_pod(&pod(
            json!({"phase": "Failed", "containerStatuses": [
                {"ready": false, "state": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}}]}),
            false,
        ));
        assert_eq!(oom["status"], "OOMKilled");
        let done = simplify_pod(&pod(
            json!({"phase": "Succeeded", "containerStatuses": [
                {"ready": false, "state": {"terminated": {"exitCode": 0, "reason": "Completed"}}}]}),
            false,
        ));
        assert_eq!(done["status"], "Succeeded");
        assert_eq!(
            simplify_pod(&pod(json!({"phase": "Running"}), true))["status"],
            "Terminating"
        );
        assert_eq!(
            simplify_pod(&pod(json!({"phase": "Failed", "reason": "Evicted"}), false))["status"],
            "Evicted"
        );
        assert_eq!(
            simplify_pod(&pod(json!({"phase": "Pending"}), false))["status"],
            "Pending"
        );
        assert_eq!(simplify_pod(&json!({}))["status"], "Unknown");
    }

    #[test]
    fn nodes_show_roles_readiness_and_size() {
        let node = json!({
            "metadata": {"name": "dev", "creationTimestamp": "t", "labels": {"node-role.kubernetes.io/control-plane": "true", "node-role.kubernetes.io/master": "true", "x": "y"}},
            "status": {
                "conditions": [{"type": "MemoryPressure", "status": "False"}, {"type": "Ready", "status": "True"}],
                "addresses": [{"type": "Hostname", "address": "dev"}, {"type": "InternalIP", "address": "10.89.0.2"}],
                "nodeInfo": {"kubeletVersion": "v1.36.5+k3s1", "osImage": "Debian GNU/Linux 13"},
                "capacity": {"cpu": "2", "memory": "4005272Ki"}
            }
        });
        let n = simplify_node(&node);
        assert_eq!(n["ready"], true);
        assert_eq!(n["roles"], json!(["control-plane", "master"]));
        assert_eq!(n["internal_ip"], "10.89.0.2");
        assert_eq!(n["version"], "v1.36.5+k3s1");
        assert_eq!(
            (n["cpu"].as_str(), n["memory_mib"].as_u64()),
            (Some("2"), Some(3911))
        );
        let worker = simplify_node(
            &json!({"metadata": {"name": "w"}, "status": {"conditions": [{"type": "Ready", "status": "False"}]}}),
        );
        assert_eq!(worker["ready"], false);
        assert_eq!(worker["roles"], json!(["worker"]));
        assert_eq!(memory_mib("1Gi"), None);
    }

    /// Against a real cluster: `FLUXVM_KUBE_LIVE_KUBECONFIG=file FLUXVM_KUBE_LIVE_ADDR=127.0.0.1:26443 cargo test -p
    /// fluxvm-api --lib kube::tests::live -- --ignored --nocapture`. The address may be an SSH forward to the server VM.
    #[tokio::test]
    #[ignore = "needs a running cluster"]
    async fn live_cluster_is_read_over_mutual_tls() {
        let file =
            std::env::var("FLUXVM_KUBE_LIVE_KUBECONFIG").expect("FLUXVM_KUBE_LIVE_KUBECONFIG");
        let addr: SocketAddr = std::env::var("FLUXVM_KUBE_LIVE_ADDR")
            .expect("FLUXVM_KUBE_LIVE_ADDR")
            .parse()
            .unwrap();
        let creds = parse_kubeconfig(&std::fs::read_to_string(file).unwrap())
            .expect("kubeconfig credentials");
        let (nodes, pods) = read(&creds, addr)
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
        let nodes: Vec<Value> = nodes["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(simplify_node)
            .collect();
        let pods: Vec<Value> = pods["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(simplify_pod)
            .collect();
        println!("nodes: {}", serde_json::to_string_pretty(&nodes).unwrap());
        println!("pods: {}", serde_json::to_string_pretty(&pods).unwrap());
        assert!(!nodes.is_empty() && nodes.iter().all(|n| n["name"].is_string()));
        assert!(pods.iter().any(|p| p["namespace"] == "kube-system"));
    }
}
