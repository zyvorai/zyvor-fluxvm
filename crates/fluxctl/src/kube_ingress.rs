// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Customising the bundled Traefik (a `HelmChartConfig`, the supported way; `traefik.yaml` is never edited) and ServiceLB
//! node pools. Pure generators; nothing here talks to a cluster.

use anyhow::{Result, bail};
use serde_yaml::{Mapping, Value};

pub const TRAEFIK_CONFIG_PATH: &str = "/var/lib/rancher/k3s/server/manifests/traefik-config.yaml";
pub const LB_ENABLE_LABEL: &str = "svccontroller.k3s.cattle.io/enablelb";
pub const LB_POOL_LABEL: &str = "svccontroller.k3s.cattle.io/lbpool";

/// Puts `value` at the dotted `path` of `root` (`providers.kubernetesGateway.enabled`). A path that is a prefix of
/// another, or set twice, is an error.
fn put(root: &mut Mapping, path: &[&str], value: Value) -> Result<()> {
    let (first, rest) = path.split_first().expect("non-empty path");
    if rest.is_empty() {
        if root.contains_key(*first) {
            bail!("{first} is set twice or is also a parent of another key");
        }
        root.insert((*first).into(), value);
        return Ok(());
    }
    let child = root
        .entry((*first).into())
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    match child {
        Value::Mapping(m) => put(m, rest, value),
        _ => bail!("{first} is a value and cannot also hold {}", rest.join(".")),
    }
}

/// `KEY=VALUE` pairs (dotted keys, YAML scalars) as a `HelmChartConfig` for Traefik. `gateway_api` adds
/// `providers.kubernetesGateway.enabled=true`. Chart value names change between Traefik chart versions; check
/// `helm show values` for the chart your k3s ships.
pub fn traefik_helmchartconfig(sets: &[String], gateway_api: bool) -> Result<String> {
    let mut all: Vec<String> = sets.to_vec();
    if gateway_api {
        all.push("providers.kubernetesGateway.enabled=true".into());
    }
    if all.is_empty() {
        bail!("nothing to set: give --set KEY=VALUE or --gateway-api");
    }
    let mut values = Mapping::new();
    for kv in &all {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--set {kv:?} must be KEY=VALUE"))?;
        let parts: Vec<&str> = k.split('.').collect();
        if parts.iter().any(|p| {
            p.is_empty()
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        }) {
            bail!("key {k:?} must be dotted names of letters, digits, - and _");
        }
        let val: Value = serde_yaml::from_str(v).unwrap_or_else(|_| Value::String(v.into()));
        if matches!(val, Value::Mapping(_) | Value::Sequence(_)) {
            bail!("value for {k} must be a scalar");
        }
        put(&mut values, &parts, val)?;
    }
    let body = serde_yaml::to_string(&Value::Mapping(values))?;
    let indented: String = body.lines().map(|l| format!("    {l}\n")).collect();
    Ok(format!(
        "apiVersion: helm.cattle.io/v1\nkind: HelmChartConfig\nmetadata:\n  name: traefik\n  namespace: kube-system\nspec:\n  valuesContent: |-\n{indented}"
    ))
}

fn pool_ok(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 63
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && p.starts_with(|c: char| c.is_ascii_alphanumeric())
        && p.ends_with(|c: char| c.is_ascii_alphanumeric())
}

/// Node labels (for `node-label` in config.yaml, or `kubectl label node`) that put a node in a ServiceLB pool. Once any
/// node carries `enablelb`, ServiceLB runs only on labelled nodes; a Service with the same `lbpool` label uses only that pool.
pub fn lb_pool_node_labels(pool: &str) -> Result<Vec<String>> {
    if !pool_ok(pool) {
        bail!("pool {pool:?} must be a label value (letters, digits, - _ .)");
    }
    Ok(vec![
        format!("{LB_ENABLE_LABEL}=true"),
        format!("{LB_POOL_LABEL}={pool}"),
    ])
}

/// Commands to run with a working kubeconfig: label the nodes into the pool and show how to pin a Service to it.
pub fn lb_pool_commands(pool: &str, nodes: &[String]) -> Result<String> {
    let labels = lb_pool_node_labels(pool)?;
    if nodes.is_empty() {
        bail!("give at least one node");
    }
    let mut s = String::new();
    for n in nodes {
        if n.is_empty() || n.contains(char::is_whitespace) {
            bail!("node name {n:?} is not valid");
        }
        s.push_str(&format!(
            "kubectl label node {n} {} {}\n",
            labels[0], labels[1]
        ));
    }
    s.push_str(&format!(
        "# and for each Service that should use only this pool:\nkubectl label svc SERVICE {LB_POOL_LABEL}={pool}\n"
    ));
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traefik_golden() {
        let y = traefik_helmchartconfig(
            &[
                "service.spec.type=NodePort".into(),
                "ports.web.exposedPort=8080".into(),
            ],
            true,
        )
        .unwrap();
        assert_eq!(
            y,
            "apiVersion: helm.cattle.io/v1\nkind: HelmChartConfig\nmetadata:\n  name: traefik\n  namespace: kube-system\nspec:\n  valuesContent: |-\n    service:\n      spec:\n        type: NodePort\n    ports:\n      web:\n        exposedPort: 8080\n    providers:\n      kubernetesGateway:\n        enabled: true\n"
        );
    }

    #[test]
    fn values_content_is_valid_nested_yaml() {
        let y = traefik_helmchartconfig(&["a.b=true".into(), "a.c=x y".into()], false).unwrap();
        let doc: Value = serde_yaml::from_str(&y).unwrap();
        let inner: Value =
            serde_yaml::from_str(doc["spec"]["valuesContent"].as_str().unwrap()).unwrap();
        assert_eq!(inner["a"]["b"], true);
        assert_eq!(inner["a"]["c"], "x y");
    }

    #[test]
    fn bad_traefik_input() {
        assert!(traefik_helmchartconfig(&[], false).is_err());
        assert!(traefik_helmchartconfig(&["novalue".into()], false).is_err());
        assert!(traefik_helmchartconfig(&["a..b=1".into()], false).is_err());
        assert!(traefik_helmchartconfig(&["a=1".into(), "a.b=2".into()], false).is_err());
        assert!(traefik_helmchartconfig(&["a=1".into(), "a=2".into()], false).is_err());
        assert!(traefik_helmchartconfig(&["a=[1,2]".into()], false).is_err());
    }

    #[test]
    fn lb_pools() {
        assert_eq!(
            lb_pool_node_labels("edge").unwrap(),
            vec![
                "svccontroller.k3s.cattle.io/enablelb=true",
                "svccontroller.k3s.cattle.io/lbpool=edge"
            ]
        );
        let c = lb_pool_commands("edge", &["n1".into(), "n2".into()]).unwrap();
        assert!(c.starts_with("kubectl label node n1 svccontroller.k3s.cattle.io/enablelb=true svccontroller.k3s.cattle.io/lbpool=edge\n"));
        assert!(c.contains("kubectl label svc SERVICE svccontroller.k3s.cattle.io/lbpool=edge"));
        assert!(lb_pool_node_labels("bad pool").is_err());
        assert!(lb_pool_commands("edge", &[]).is_err());
    }
}
