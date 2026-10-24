// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! CPU and memory use for the Kubernetes page, from the cluster's metrics-server (`metrics.k8s.io`, which k3s ships).
//!
//! The numbers are folded into the node and pod rows `GET /v1/kube/{name}` already returns. A cluster without a working
//! metrics-server simply has no numbers: the rows are returned as before and `metrics` is false.

use serde_json::{Value, json};
use std::collections::HashMap;

/// A CPU quantity in millicores: `250m`, `123456789n`, `500u`, `2`, `1.5`.
pub(crate) fn cpu_millis(q: &str) -> Option<u64> {
    let q = q.trim();
    let (num, per_milli) = if let Some(n) = q.strip_suffix('n') {
        (n, 1_000_000.0)
    } else if let Some(n) = q.strip_suffix('u') {
        (n, 1_000.0)
    } else if let Some(n) = q.strip_suffix('m') {
        (n, 1.0)
    } else {
        (q, 0.001)
    };
    let v: f64 = num.parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| (v / per_milli).round() as u64)
}

/// A memory quantity in MiB: `12345Ki`, `100Mi`, `1Gi`, `500M`, or plain bytes.
pub(crate) fn mem_mib(q: &str) -> Option<u64> {
    let q = q.trim();
    let table: [(&str, f64); 8] = [
        ("Ki", 1024.0),
        ("Mi", 1024.0 * 1024.0),
        ("Gi", 1024.0 * 1024.0 * 1024.0),
        ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("k", 1000.0),
        ("M", 1_000_000.0),
        ("G", 1_000_000_000.0),
        ("T", 1_000_000_000_000.0),
    ];
    let (num, bytes) = table
        .iter()
        .find_map(|(suffix, mult)| q.strip_suffix(suffix).map(|n| (n, *mult)))
        .unwrap_or((q, 1.0));
    let v: f64 = num.parse().ok()?;
    (v.is_finite() && v >= 0.0).then(|| (v * bytes / (1024.0 * 1024.0)).round() as u64)
}

fn usage(item: &Value) -> Option<(u64, u64)> {
    let u = &item["usage"];
    Some((
        cpu_millis(u["cpu"].as_str()?)?,
        mem_mib(u["memory"].as_str()?)?,
    ))
}

/// Pod metrics add up their containers' usage.
fn pod_usage(item: &Value) -> Option<(u64, u64)> {
    let mut any = false;
    let (mut cpu, mut mem) = (0, 0);
    for c in item["containers"].as_array()? {
        if let Some((c_cpu, c_mem)) = usage(c) {
            any = true;
            cpu += c_cpu;
            mem += c_mem;
        }
    }
    any.then_some((cpu, mem))
}

fn percent(used: u64, of: u64) -> Option<f64> {
    (of > 0).then(|| ((used as f64 / of as f64) * 1000.0).round() / 10.0)
}

/// Adds `cpu_m`, `mem_mib` and, for nodes, `cpu_percent` and `mem_percent` to the rows that have metrics. Returns whether any did.
pub(crate) fn apply(
    nodes: &mut [Value],
    pods: &mut [Value],
    node_metrics: Option<&Value>,
    pod_metrics: Option<&Value>,
) -> bool {
    let mut any = false;
    let by_node: HashMap<&str, (u64, u64)> = node_metrics
        .and_then(|m| m["items"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|i| Some((i["metadata"]["name"].as_str()?, usage(i)?)))
        .collect();
    for n in nodes.iter_mut() {
        let Some((cpu, mem)) = n["name"]
            .as_str()
            .and_then(|name| by_node.get(name).copied())
        else {
            continue;
        };
        any = true;
        let cores = n["cpu"].as_str().and_then(|c| c.parse::<f64>().ok());
        n["cpu_m"] = json!(cpu);
        n["mem_used_mib"] = json!(mem);
        n["cpu_percent"] = json!(cores.and_then(|c| percent(cpu, (c * 1000.0) as u64)));
        n["mem_percent"] = json!(
            n["memory_mib"]
                .as_u64()
                .and_then(|total| percent(mem, total))
        );
    }
    let by_pod: HashMap<(&str, &str), (u64, u64)> = pod_metrics
        .and_then(|m| m["items"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|i| {
            Some((
                (
                    i["metadata"]["namespace"].as_str()?,
                    i["metadata"]["name"].as_str()?,
                ),
                pod_usage(i)?,
            ))
        })
        .collect();
    for p in pods.iter_mut() {
        let key = (
            p["namespace"].as_str().unwrap_or(""),
            p["name"].as_str().unwrap_or(""),
        );
        if let Some((cpu, mem)) = by_pod.get(&key).copied() {
            any = true;
            p["cpu_m"] = json!(cpu);
            p["mem_mib"] = json!(mem);
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_quantities_become_millicores() {
        for (q, want) in [
            ("250m", 250),
            ("2", 2000),
            ("1.5", 1500),
            ("123456789n", 123),
            ("500u", 1),
            ("0", 0),
            ("0n", 0),
            (" 100m ", 100),
        ] {
            assert_eq!(cpu_millis(q), Some(want), "{q}");
        }
        for bad in ["", "m", "abc", "1x", "-5m", "NaN"] {
            assert_eq!(cpu_millis(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn memory_quantities_become_mib() {
        for (q, want) in [
            ("12345Ki", 12),
            ("100Mi", 100),
            ("1Gi", 1024),
            ("2Ti", 2 * 1024 * 1024),
            ("500M", 477),
            ("1G", 954),
            ("1048576", 1),
            ("0", 0),
            ("1024k", 1),
        ] {
            assert_eq!(mem_mib(q), Some(want), "{q}");
        }
        for bad in ["", "Mi", "x", "1Zi", "-1Mi"] {
            assert_eq!(mem_mib(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn usage_is_merged_into_nodes_and_pods() {
        let mut nodes = vec![
            json!({"name": "n1", "cpu": "2", "memory_mib": 4000}),
            json!({"name": "n2", "cpu": "4", "memory_mib": 8000}),
        ];
        let mut pods = vec![
            json!({"namespace": "default", "name": "web"}),
            json!({"namespace": "default", "name": "idle"}),
        ];
        let node_metrics = json!({"items": [{"metadata": {"name": "n1"}, "usage": {"cpu": "500m", "memory": "2000Mi"}}]});
        let pod_metrics = json!({"items": [
            {"metadata": {"namespace": "default", "name": "web"}, "containers": [
                {"name": "a", "usage": {"cpu": "100m", "memory": "50Mi"}},
                {"name": "b", "usage": {"cpu": "25m", "memory": "10Mi"}}]}]});
        assert!(apply(
            &mut nodes,
            &mut pods,
            Some(&node_metrics),
            Some(&pod_metrics)
        ));
        assert_eq!(
            (
                nodes[0]["cpu_m"].as_u64(),
                nodes[0]["mem_used_mib"].as_u64()
            ),
            (Some(500), Some(2000))
        );
        assert_eq!(
            (
                nodes[0]["cpu_percent"].as_f64(),
                nodes[0]["mem_percent"].as_f64()
            ),
            (Some(25.0), Some(50.0))
        );
        assert!(
            nodes[1].get("cpu_m").is_none(),
            "a node with no metrics is left as it was"
        );
        assert_eq!(
            (pods[0]["cpu_m"].as_u64(), pods[0]["mem_mib"].as_u64()),
            (Some(125), Some(60))
        );
        assert!(pods[1].get("cpu_m").is_none());
    }

    #[test]
    fn no_metrics_changes_nothing() {
        let mut nodes = vec![json!({"name": "n1", "cpu": "2", "memory_mib": 4000})];
        let mut pods = vec![json!({"namespace": "d", "name": "p"})];
        let before = (nodes.clone(), pods.clone());
        assert!(!apply(&mut nodes, &mut pods, None, None));
        assert!(!apply(
            &mut nodes,
            &mut pods,
            Some(&json!({})),
            Some(&json!({"items": "x"}))
        ));
        assert_eq!((nodes, pods), before);
    }

    #[test]
    fn a_missing_capacity_gives_no_percentage_rather_than_a_wrong_one() {
        let mut nodes = vec![json!({"name": "n1"})];
        let nm = json!({"items": [{"metadata": {"name": "n1"}, "usage": {"cpu": "100m", "memory": "10Mi"}}]});
        apply(&mut nodes, &mut [], Some(&nm), None);
        assert_eq!(nodes[0]["cpu_m"], 100);
        assert!(nodes[0]["cpu_percent"].is_null() && nodes[0]["mem_percent"].is_null());
        assert_eq!(percent(1, 0), None);
    }
}
