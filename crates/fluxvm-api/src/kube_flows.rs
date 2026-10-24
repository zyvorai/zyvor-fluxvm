// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Who talks to whom in a cluster, for the Studio's traffic graph.
//!
//! `GET /v1/kube/{cluster}/flows` runs `hubble observe` inside each Cilium agent (`fluxctl kube` installs Cilium by
//! default; Hubble is the agent's built-in flow recorder) through the cluster's `exec` API, and folds the flows into
//! nodes (workloads, the internet, the host) and edges (protocol and port, with counts). It is read-only and runs one fixed
//! command; the only input that reaches it is a bounded number of lines. A flannel cluster has no agents and says so.

use crate::kube::{self, Target, err};
use crate::kube_pod::exec_output;
use crate::{ApiError, ApiResult, TokenTenant};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use fluxvm_scheduler::VmManager;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Duration,
};

const DEFAULT_LINES: u32 = 4000;
const MAX_LINES: u32 = 20_000;
const OUTPUT_CAP: usize = 24 * 1024 * 1024;
const RUN_LIMIT: Duration = Duration::from_secs(15);
const MAX_EDGES: usize = 400;

/// An endpoint of a flow, as a graph node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub id: String,
    pub label: String,
    pub namespace: String,
    /// `workload`, `system` (kube-system and Cilium's own), `world` or `host`.
    pub kind: &'static str,
    pub pod: Option<String>,
}

fn labels_of(e: &Value) -> Vec<&str> {
    e["labels"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// A pod becomes its workload (`default/web`), a reserved identity becomes one shared node.
pub(crate) fn endpoint(e: &Value) -> Option<Endpoint> {
    if !e.is_object() {
        return None;
    }
    let pod = e["pod_name"].as_str().filter(|p| !p.is_empty());
    if let Some(pod) = pod {
        let ns = e["namespace"].as_str().unwrap_or("");
        let workload = e["workloads"][0]["name"].as_str().unwrap_or(pod);
        return Some(Endpoint {
            id: format!("{ns}/{workload}"),
            label: workload.to_owned(),
            namespace: ns.to_owned(),
            kind: if ns == "kube-system" {
                "system"
            } else {
                "workload"
            },
            pod: Some(pod.to_owned()),
        });
    }
    let labels = labels_of(e);
    let has = |l: &str| labels.contains(&l);
    let (id, label, kind) = if has("reserved:world") {
        ("world", "Internet", "world")
    } else if has("reserved:kube-apiserver") {
        ("host", "Node and API server", "host")
    } else if has("reserved:host") {
        ("host", "Node and API server", "host")
    } else if has("reserved:remote-node") {
        ("remote-node", "Other nodes", "host")
    } else if has("reserved:health") {
        ("health", "Cilium health", "system")
    } else if has("reserved:ingress") {
        ("ingress", "Ingress", "host")
    } else {
        return e["identity"].as_u64().map(|i| Endpoint {
            id: format!("identity-{i}"),
            label: format!("identity {i}"),
            namespace: String::new(),
            kind: "system",
            pod: None,
        });
    };
    Some(Endpoint {
        id: id.into(),
        label: label.into(),
        namespace: String::new(),
        kind,
        pod: None,
    })
}

type EdgeKey = (String, String, String, Option<u64>);

#[derive(Debug, Default, Clone)]
struct EdgeCount {
    flows: u64,
    connections: u64,
    dropped: u64,
    last: String,
}

/// What the flows add up to.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    nodes: BTreeMap<String, (Endpoint, BTreeSet<String>)>,
    edges: BTreeMap<EdgeKey, EdgeCount>,
    pub seen: u64,
    pub counted: u64,
    first: String,
    last: String,
}

impl Graph {
    /// Folds in the JSON lines of one agent. `pod_nodes` maps (namespace, pod) to the node it runs on.
    ///
    /// Every packet event is a flow, and a cross-node request is seen by the agent on each node, so a flow counts once:
    /// only replies are skipped, and only the sender's node counts a pod's requests (a request from outside the cluster or
    /// from the host counts on the node of the pod it reaches).
    pub(crate) fn add_lines(&mut self, text: &str, pod_nodes: &HashMap<(String, String), String>) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let f = &v["flow"];
            if !f.is_object() {
                continue;
            }
            self.seen += 1;
            let t = f["time"].as_str().unwrap_or("");
            if !t.is_empty() {
                if self.first.is_empty() || t < self.first.as_str() {
                    self.first = t.to_owned();
                }
                if t > self.last.as_str() {
                    self.last = t.to_owned();
                }
            }
            if f["is_reply"].as_bool() == Some(true) {
                continue;
            }
            let (Some(src), Some(dst)) = (endpoint(&f["source"]), endpoint(&f["destination"]))
            else {
                continue;
            };
            let observer = f["node_name"].as_str().unwrap_or("");
            let node_of = |e: &Endpoint| {
                e.pod
                    .as_ref()
                    .and_then(|p| pod_nodes.get(&(e.namespace.clone(), p.clone())))
            };
            let owner = node_of(&src).or_else(|| node_of(&dst));
            if owner.is_some_and(|n| n != observer) {
                continue;
            }
            let l4 = &f["l4"];
            let (proto, port, syn) = if let Some(t) = l4.get("TCP") {
                let fl = &t["flags"];
                (
                    "TCP",
                    t["destination_port"].as_u64(),
                    fl["SYN"] == true && fl["ACK"] != true,
                )
            } else if let Some(u) = l4.get("UDP") {
                ("UDP", u["destination_port"].as_u64(), false)
            } else if let Some(s) = l4.get("SCTP") {
                ("SCTP", s["destination_port"].as_u64(), false)
            } else if l4.get("ICMPv4").is_some() || l4.get("ICMPv6").is_some() {
                ("ICMP", None, false)
            } else {
                ("IP", None, false)
            };
            self.counted += 1;
            for e in [&src, &dst] {
                let slot = self
                    .nodes
                    .entry(e.id.clone())
                    .or_insert_with(|| (e.clone(), BTreeSet::new()));
                if let Some(p) = &e.pod {
                    slot.1.insert(p.clone());
                }
            }
            let c = self
                .edges
                .entry((src.id.clone(), dst.id.clone(), proto.to_owned(), port))
                .or_default();
            c.flows += 1;
            c.connections += u64::from(syn);
            c.dropped += u64::from(f["verdict"] == "DROPPED");
            if t > c.last.as_str() {
                c.last = t.to_owned();
            }
        }
    }

    /// The graph as JSON: busiest edges first, capped, and only the nodes they touch.
    pub(crate) fn to_json(&self) -> Value {
        let mut edges: Vec<(&EdgeKey, &EdgeCount)> = self.edges.iter().collect();
        edges.sort_by(|a, b| b.1.flows.cmp(&a.1.flows).then_with(|| a.0.cmp(b.0)));
        let truncated = edges.len() > MAX_EDGES;
        edges.truncate(MAX_EDGES);
        let used: BTreeSet<&str> = edges
            .iter()
            .flat_map(|(k, _)| [k.0.as_str(), k.1.as_str()])
            .collect();
        let nodes: Vec<Value> = self
            .nodes
            .values()
            .filter(|(e, _)| used.contains(e.id.as_str()))
            .map(|(e, pods)| {
                json!({"id": e.id, "label": e.label, "namespace": e.namespace, "kind": e.kind, "pods": pods.len()})
            })
            .collect();
        json!({
            "nodes": nodes,
            "edges": edges.iter().map(|(k, c)| json!({
                "from": k.0, "to": k.1, "proto": k.2, "port": k.3,
                "flows": c.flows, "connections": c.connections, "dropped": c.dropped,
                "last": if c.last.is_empty() { Value::Null } else { json!(c.last) },
            })).collect::<Vec<_>>(),
            "flows_seen": self.seen,
            "flows_counted": self.counted,
            "window": {"from": self.first, "to": self.last},
            "truncated": truncated,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct FlowsQuery {
    lines: Option<u32>,
}

pub(crate) fn lines_arg(q: &FlowsQuery) -> String {
    q.lines
        .unwrap_or(DEFAULT_LINES)
        .clamp(100, MAX_LINES)
        .to_string()
}

/// The running Cilium agent pods and the node each runs on, plus every pod's node.
pub(crate) fn agents_and_nodes(
    pods: &Value,
) -> (Vec<(String, String)>, HashMap<(String, String), String>) {
    let mut agents = vec![];
    let mut nodes = HashMap::new();
    for p in pods["items"].as_array().into_iter().flatten() {
        let (ns, name) = (
            p["metadata"]["namespace"].as_str(),
            p["metadata"]["name"].as_str(),
        );
        let (Some(ns), Some(name)) = (ns, name) else {
            continue;
        };
        let node = p["spec"]["nodeName"].as_str().unwrap_or("").to_owned();
        if ns == "kube-system"
            && p["metadata"]["labels"]["k8s-app"] == "cilium"
            && p["status"]["phase"] == "Running"
        {
            agents.push((name.to_owned(), node.clone()));
        }
        nodes.insert((ns.to_owned(), name.to_owned()), node);
    }
    agents.sort();
    (agents, nodes)
}

/// `GET /v1/kube/{cluster}/flows?lines=`
pub(crate) async fn get_flows(
    State(m): State<Arc<VmManager>>,
    token_tenant: Option<Extension<TokenTenant>>,
    Path(name): Path<String>,
    Query(q): Query<FlowsQuery>,
) -> ApiResult<Json<Value>> {
    let tenant = token_tenant.map(|Extension(TokenTenant(t))| t);
    let target: Target = kube::resolve(&m, tenant.as_deref(), &name).await?;
    let mut tried = vec![];
    let mut pods = None;
    for &addr in &target.addrs {
        match kube::read(&target.creds, addr).await {
            Ok((_, p)) => {
                pods = Some(p);
                break;
            }
            Err(e) => tried.push(format!("{addr}: {}", e.message)),
        }
    }
    let pods = pods.ok_or_else(|| err(StatusCode::BAD_GATEWAY, tried.join("; ")))?;
    let (agents, pod_nodes) = agents_and_nodes(&pods);
    if agents.is_empty() {
        return Err(err(
            StatusCode::CONFLICT,
            "no running Cilium agent in kube-system: traffic comes from Cilium's Hubble, and this cluster uses another pod network (fluxctl kube uses Cilium by default)",
        ));
    }
    let lines = lines_arg(&q);
    let argv = ["hubble", "observe", "--last", lines.as_str(), "-o", "json"];
    let runs = agents.iter().map(|(pod, _)| {
        exec_output(
            &target.creds,
            &target.addrs,
            "kube-system",
            pod,
            "cilium-agent",
            &argv,
            OUTPUT_CAP,
            RUN_LIMIT,
        )
    });
    let results = futures::future::join_all(runs).await;
    let mut graph = Graph::default();
    let (mut ok, mut errors, mut cut) = (0, vec![], false);
    for ((_, node), r) in agents.iter().zip(results) {
        match r {
            Ok(o) => {
                ok += 1;
                cut |= o.truncated;
                graph.add_lines(&String::from_utf8_lossy(&o.stdout), &pod_nodes);
            }
            Err(e) => errors.push(json!({"node": node, "error": hint(&e)})),
        }
    }
    if ok == 0 {
        let first = errors
            .first()
            .and_then(|e| e["error"].as_str())
            .unwrap_or("no answer");
        return Err(ApiError {
            status: StatusCode::BAD_GATEWAY,
            message: format!("could not read flows from any Cilium agent: {first}"),
        });
    }
    let mut out = graph.to_json();
    out["cluster"] = json!(name);
    out["agents"] = json!(ok);
    out["errors"] = json!(errors);
    out["output_truncated"] = json!(cut);
    Ok(Json(out))
}

fn hint(e: &str) -> String {
    if e.contains("executable file not found") || e.contains("not found in $PATH") {
        "the Cilium agent image has no hubble command".to_owned()
    } else {
        e.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("kube_flows_fixture.jsonl");

    fn nodes() -> HashMap<(String, String), String> {
        [
            (("default", "client"), "ktgraph"),
            (("default", "web-7fbc579fd4-66958"), "ktgraph"),
            (("kube-system", "coredns-7cfb7bc9c7-xznh6"), "ktgraph"),
            (
                ("kube-system", "metrics-server-6dc596dfb8-fwzrz"),
                "ktgraph",
            ),
        ]
        .into_iter()
        .map(|((a, b), n)| ((a.to_owned(), b.to_owned()), n.to_owned()))
        .collect()
    }

    fn line(i: usize) -> &'static str {
        FIXTURE.lines().nth(i).unwrap()
    }

    #[test]
    fn endpoints_become_workloads_or_shared_nodes() {
        let v: Value = serde_json::from_str(line(0)).unwrap();
        let web = endpoint(&v["flow"]["destination"]).unwrap();
        assert_eq!(
            (web.id.as_str(), web.label.as_str(), web.kind),
            ("default/web", "web", "workload")
        );
        assert_eq!(web.pod.as_deref(), Some("web-7fbc579fd4-66958"));
        let client = endpoint(&v["flow"]["source"]).unwrap();
        assert_eq!(
            client.id, "default/client",
            "a bare pod is its own workload"
        );
        let world: Value = serde_json::from_str(line(4)).unwrap();
        let w = endpoint(&world["flow"]["destination"]).unwrap();
        assert_eq!(
            (w.id.as_str(), w.kind, w.pod.is_none()),
            ("world", "world", true)
        );
        let host: Value = serde_json::from_str(line(5)).unwrap();
        let h = endpoint(&host["flow"]["source"]).unwrap();
        assert_eq!((h.id.as_str(), h.kind), ("host", "host"));
        let dns: Value = serde_json::from_str(line(3)).unwrap();
        assert_eq!(
            endpoint(&dns["flow"]["destination"]).unwrap().kind,
            "system"
        );
        assert!(endpoint(&Value::Null).is_none());
        assert_eq!(
            endpoint(&json!({"identity": 7, "labels": []})).unwrap().id,
            "identity-7"
        );
    }

    #[test]
    fn real_flows_fold_into_edges() {
        let mut g = Graph::default();
        g.add_lines(FIXTURE, &nodes());
        assert_eq!(g.seen, 6);
        assert_eq!(g.counted, 5, "the reply is skipped");
        let j = g.to_json();
        let edge = |from: &str, to: &str, proto: &str| {
            j["edges"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["from"] == from && e["to"] == to && e["proto"] == proto)
                .cloned()
        };
        let web = edge("default/client", "default/web", "TCP").expect("client -> web");
        assert_eq!(
            (
                web["port"].as_u64(),
                web["flows"].as_u64(),
                web["connections"].as_u64()
            ),
            (Some(80), Some(2), Some(1))
        );
        assert_eq!(web["dropped"], 0);
        assert!(web["last"].as_str().unwrap().starts_with("2026-10-24T14:4"));
        let dns = edge("default/client", "kube-system/coredns", "UDP").expect("client -> dns");
        assert_eq!(dns["port"], 53);
        let world = edge("default/client", "world", "TCP").expect("client -> internet");
        assert_eq!(
            (world["connections"].as_u64(), world["flows"].as_u64()),
            (Some(1), Some(1))
        );
        assert!(edge("host", "kube-system/metrics-server", "TCP").is_some());
        assert!(
            edge("default/web", "default/client", "TCP").is_none(),
            "replies are not edges"
        );
        let ns: Vec<&str> = j["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        for id in [
            "default/client",
            "default/web",
            "world",
            "host",
            "kube-system/coredns",
        ] {
            assert!(ns.contains(&id), "{id} in {ns:?}");
        }
        let web_node = j["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == "default/web")
            .unwrap();
        assert_eq!(
            (web_node["pods"].as_u64(), web_node["kind"].as_str()),
            (Some(1), Some("workload"))
        );
        assert!(j["window"]["from"].as_str().unwrap() <= j["window"]["to"].as_str().unwrap());
        assert_eq!(j["truncated"], false);
    }

    #[test]
    fn a_cross_node_request_counts_once_on_the_senders_node() {
        // client on n1, web on n2: the request is reported by both nodes' agents.
        let mut two = nodes();
        two.insert(("default".into(), "client".into()), "n1".into());
        two.insert(
            ("default".into(), "web-7fbc579fd4-66958".into()),
            "n2".into(),
        );
        let mut as_n1 = serde_json::from_str::<Value>(line(0)).unwrap();
        as_n1["flow"]["node_name"] = json!("n1");
        let mut as_n2 = as_n1.clone();
        as_n2["flow"]["node_name"] = json!("n2");
        let mut g = Graph::default();
        g.add_lines(&format!("{as_n1}\n{as_n2}\n"), &two);
        assert_eq!(g.seen, 2);
        assert_eq!(g.counted, 1, "only the sender's node counts it");
        // From the host, the node of the pod it reaches decides.
        let host: Value = serde_json::from_str(line(5)).unwrap();
        let mut h1 = host.clone();
        h1["flow"]["node_name"] = json!("ktgraph");
        let mut h2 = host;
        h2["flow"]["node_name"] = json!("n2");
        let mut g = Graph::default();
        g.add_lines(&format!("{h1}\n{h2}\n"), &nodes());
        assert_eq!(g.counted, 1, "the metrics-server pod runs on ktgraph");
        // A flow whose pods are unknown is counted rather than lost.
        let mut g = Graph::default();
        g.add_lines(line(0), &HashMap::new());
        assert_eq!(g.counted, 1);
    }

    #[test]
    fn drops_count_and_junk_is_ignored() {
        let mut dropped = serde_json::from_str::<Value>(line(0)).unwrap();
        dropped["flow"]["verdict"] = json!("DROPPED");
        let mut g = Graph::default();
        g.add_lines(
            &format!("not json\n{{}}\n{{\"flow\":{{}}}}\n\n{dropped}\n"),
            &nodes(),
        );
        assert_eq!(
            g.seen, 2,
            "{{}} has no flow object; {{\"flow\":{{}}}} is one with nothing in it"
        );
        let j = g.to_json();
        assert_eq!(j["edges"][0]["dropped"], 1);
        assert_eq!(j["edges"][0]["flows"], 1);
    }

    #[test]
    fn edges_are_capped_busiest_first() {
        let mut g = Graph::default();
        let base = serde_json::from_str::<Value>(line(0)).unwrap();
        let mut text = String::new();
        for port in 1..=(MAX_EDGES as u64 + 20) {
            let mut v = base.clone();
            v["flow"]["l4"]["TCP"]["destination_port"] = json!(port);
            text.push_str(&format!("{v}\n"));
        }
        // One busy edge, listed last.
        let mut busy = base.clone();
        busy["flow"]["l4"]["TCP"]["destination_port"] = json!(9999);
        for _ in 0..5 {
            text.push_str(&format!("{busy}\n"));
        }
        g.add_lines(&text, &nodes());
        let j = g.to_json();
        assert_eq!(j["edges"].as_array().unwrap().len(), MAX_EDGES);
        assert_eq!(j["truncated"], true);
        assert_eq!(j["edges"][0]["port"], 9999, "the busiest edge comes first");
    }

    #[test]
    fn agents_are_the_running_cilium_pods() {
        let pod = |ns: &str, name: &str, app: Option<&str>, phase: &str, node: &str| {
            json!({"metadata": {"namespace": ns, "name": name, "labels": app.map(|a| json!({"k8s-app": a})).unwrap_or(json!({}))},
                   "spec": {"nodeName": node}, "status": {"phase": phase}})
        };
        let pods = json!({"items": [
            pod("kube-system", "cilium-b", Some("cilium"), "Running", "n2"),
            pod("kube-system", "cilium-a", Some("cilium"), "Running", "n1"),
            pod("kube-system", "cilium-c", Some("cilium"), "Pending", "n3"),
            pod("kube-system", "cilium-operator-x", Some("cilium-operator"), "Running", "n1"),
            pod("default", "web", None, "Running", "n1"),
        ]});
        let (agents, nodes) = agents_and_nodes(&pods);
        assert_eq!(
            agents,
            vec![
                ("cilium-a".to_owned(), "n1".to_owned()),
                ("cilium-b".to_owned(), "n2".to_owned())
            ]
        );
        assert_eq!(
            nodes
                .get(&("default".into(), "web".into()))
                .map(String::as_str),
            Some("n1")
        );
        assert_eq!(agents_and_nodes(&json!({})).0.len(), 0);
    }

    #[test]
    fn the_line_count_is_bounded() {
        let q = |lines| FlowsQuery { lines };
        assert_eq!(lines_arg(&q(None)), "4000");
        assert_eq!(lines_arg(&q(Some(1))), "100");
        assert_eq!(lines_arg(&q(Some(u32::MAX))), "20000");
        assert_eq!(lines_arg(&q(Some(777))), "777");
    }

    #[test]
    fn a_missing_hubble_is_explained() {
        assert!(
            hint("command terminated: exec: \"hubble\": executable file not found in $PATH")
                .contains("no hubble command")
        );
        assert_eq!(hint("timed out after 15s"), "timed out after 15s");
    }

    /// Against a real Cilium cluster with a `client` pod calling a `web` service:
    /// `FLUXVM_KUBE_LIVE_KUBECONFIG=file FLUXVM_KUBE_LIVE_ADDR=127.0.0.1:26451 cargo test -p fluxvm-api --lib
    /// kube_flows::tests::live -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "needs a running Cilium cluster with traffic"]
    async fn live_hubble_flows_become_a_graph() {
        let creds = kube::parse_kubeconfig(
            &std::fs::read_to_string(std::env::var("FLUXVM_KUBE_LIVE_KUBECONFIG").unwrap())
                .unwrap(),
        )
        .expect("credentials");
        let addr: std::net::SocketAddr = std::env::var("FLUXVM_KUBE_LIVE_ADDR")
            .unwrap()
            .parse()
            .unwrap();
        let (_, pods) = kube::read(&creds, addr)
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
        let (agents, pod_nodes) = agents_and_nodes(&pods);
        assert!(!agents.is_empty(), "no Cilium agent");
        let argv = ["hubble", "observe", "--last", "4000", "-o", "json"];
        let out = exec_output(
            &creds,
            &[addr],
            "kube-system",
            &agents[0].0,
            "cilium-agent",
            &argv,
            OUTPUT_CAP,
            RUN_LIMIT,
        )
        .await
        .expect("hubble observe");
        let mut g = Graph::default();
        g.add_lines(&String::from_utf8_lossy(&out.stdout), &pod_nodes);
        let j = g.to_json();
        println!("{}", serde_json::to_string_pretty(&j).unwrap());
        let has = |from: &str, to: &str| {
            j["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["from"] == from && e["to"] == to)
        };
        assert!(g.counted > 10, "flows counted: {}", g.counted);
        assert!(has("default/client", "default/web"), "client -> web");
        assert!(has("default/client", "world"), "client -> internet");
        assert!(
            j["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["from"] == "default/client"
                    && e["to"] == "default/web"
                    && e["port"] == 80)
        );
    }
}
