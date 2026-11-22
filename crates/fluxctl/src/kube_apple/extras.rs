// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Small platform pieces that sit beside [`super::recipes`]: kube-prometheus-stack as a k3s `HelmChart`, a `StorageClass`
//! over the bundled local-path provisioner, Longhorn guidance (text only), and Kairon node labels on a running cluster.
//! The generators are pure; the applies go through [`Remote`]. Not run against real hardware.

use super::platform::Remote;
use super::recipes::{RecipeReport, dns_label_ok};
use super::script::K3S_BIN;
use anyhow::{Result, bail};

/// The `kube-prometheus-stack` chart version the HelmChart asks the cluster's helm controller for. The chart's contents are
/// downloaded by the cluster and are not hash-checked here.
pub const PROMETHEUS_CHART_VERSION: &str = "93.6.1";

pub const KAIRON_PREFIX: &str = "kairon.zyvor.dev/";

fn kubectl(op: &str, args: &str) -> String {
    format!(": fluxvm-op {op}; set -eu; sudo -n {K3S_BIN} kubectl {args}")
}

fn indent(s: &str, n: usize) -> String {
    s.lines()
        .map(|l| {
            if l.is_empty() {
                String::new()
            } else {
                format!("{}{l}", " ".repeat(n))
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `kube-prometheus-stack` as a k3s `HelmChart`: small footprint (short retention, no Alertmanager, the control-plane
/// scrapers k3s does not expose switched off).
pub fn prometheus_helmchart(version: &str, retention: &str) -> Result<String> {
    if version.is_empty() || !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        bail!("chart version {version:?} must be like 93.6.1");
    }
    if retention.len() > 8
        || !retention.chars().next().is_some_and(|c| c.is_ascii_digit())
        || !retention.ends_with(['h', 'd', 'w'])
    {
        bail!("retention {retention:?} must be like 24h or 7d");
    }
    let values = format!(
        "prometheus:\n  prometheusSpec:\n    retention: {retention}\n    resources:\n      requests:\n        cpu: 100m\n        memory: 400Mi\nalertmanager:\n  enabled: false\nkubeEtcd:\n  enabled: false\nkubeControllerManager:\n  enabled: false\nkubeScheduler:\n  enabled: false\nkubeProxy:\n  enabled: false\n"
    );
    Ok(format!(
        "apiVersion: helm.cattle.io/v1\nkind: HelmChart\nmetadata:\n  name: kube-prometheus-stack\n  namespace: kube-system\nspec:\n  repo: https://prometheus-community.github.io/helm-charts\n  chart: kube-prometheus-stack\n  version: {version}\n  targetNamespace: monitoring\n  createNamespace: true\n  valuesContent: |-\n{}\n",
        indent(&values, 4).trim_end()
    ))
}

pub async fn prometheus_stack<R: Remote>(
    r: &R,
    retention: &str,
    disabled: &[String],
) -> Result<RecipeReport> {
    let y = prometheus_helmchart(PROMETHEUS_CHART_VERSION, retention)?;
    let mut rep = RecipeReport {
        recipe: "prometheus-stack".into(),
        version: PROMETHEUS_CHART_VERSION.into(),
        ..Default::default()
    };
    r.run_server(
        &kubectl("prometheus-apply", "apply -f -"),
        Some(y.as_bytes()),
    )
    .await?;
    rep.applied.push(format!(
        "HelmChart kube-prometheus-stack {PROMETHEUS_CHART_VERSION} (the cluster's helm controller installs it; watch `kubectl -n monitoring get pods`)"
    ));
    rep.notes.push(format!(
        "the chart version is pinned, but its contents are fetched by the cluster and are not hash-checked; the nodes need internet access (chart {PROMETHEUS_CHART_VERSION})"
    ));
    if disabled.iter().any(|d| d == "metrics-server") {
        rep.notes.push(
            "metrics-server is disabled; `kubectl top` needs it (`platform metrics-server`)".into(),
        );
    }
    Ok(rep)
}

/// A StorageClass over the bundled local-path provisioner that binds only once a pod is scheduled (the volume lives on that
/// node's disk) and, with `retain`, keeps the data when the claim is deleted.
pub fn local_path_class(name: &str, retain: bool, make_default: bool) -> Result<String> {
    if !dns_label_ok(name) {
        bail!("class name {name:?} must be a DNS label");
    }
    let ann = if make_default {
        "  annotations:\n    storageclass.kubernetes.io/is-default-class: \"true\"\n"
    } else {
        ""
    };
    Ok(format!(
        "apiVersion: storage.k8s.io/v1\nkind: StorageClass\nmetadata:\n  name: {name}\n{ann}provisioner: rancher.io/local-path\nreclaimPolicy: {}\nvolumeBindingMode: WaitForFirstConsumer\nallowVolumeExpansion: false\n",
        if retain { "Retain" } else { "Delete" }
    ))
}

pub async fn apply_local_path<R: Remote>(r: &R, yaml: &str, disabled: &[String]) -> Result<String> {
    if disabled.iter().any(|d| d == "local-storage") {
        bail!(
            "this cluster was created with --disable local-storage, so the local-path provisioner is not running"
        );
    }
    r.run_server(
        &kubectl("storageclass-apply", "apply -f -"),
        Some(yaml.as_bytes()),
    )
    .await
}

pub const LONGHORN_GUIDANCE: &str = "Longhorn (guidance only: FluxVM does not install it)\n\
\n\
Longhorn gives replicated volumes across nodes. Its install page (longhorn.io/docs/latest/deploy/install/) lists:\n\
  * a recent Kubernetes version (check the page for the minimum);\n\
  * open-iscsi installed and the iscsid daemon running on every node;\n\
  * an NFSv4 client on every node for ReadWriteMany volumes;\n\
  * whether the default engine runs on arm64 guests is for that page to say.\n\
Things that follow from how FluxVM guests work, not from that page:\n\
  * replication needs more than one node; on a single VM it gains nothing over local-path;\n\
  * volumes live on each guest's disk, which is one file on the Mac, so size --disk-gib for them;\n\
  * nodes on one Mac fail together: keep `kube apple snapshot` and backups too.\n\
Install it from its own docs and run its environment check first. FluxVM has not run Longhorn on these guests.\n";

/// A Kairon node label from `KEY=VALUE` (key with or without the prefix), validated like any Kubernetes label.
pub fn kairon_label(spec: &str) -> Result<String> {
    let Some((k, v)) = spec.split_once('=') else {
        bail!("label {spec:?} must be KEY=VALUE, e.g. topology=tb5-mesh");
    };
    let key = if k.starts_with(KAIRON_PREFIX) {
        k.to_owned()
    } else {
        format!("{KAIRON_PREFIX}{k}")
    };
    let label = format!("{key}={v}");
    if v.is_empty() {
        bail!("label {spec:?} needs a value");
    }
    crate::kube_addons::check_label(&label)?;
    Ok(label)
}

/// Labels nodes (`kubectl label node --overwrite`): the named one, or every node the record knows.
pub async fn label_nodes<R: Remote>(
    r: &R,
    specs: &[String],
    node: Option<&str>,
) -> Result<Vec<String>> {
    if specs.is_empty() {
        bail!("give at least one --label KEY=VALUE");
    }
    let labels = specs
        .iter()
        .map(|s| kairon_label(s))
        .collect::<Result<Vec<_>>>()?;
    let nodes: Vec<String> = r.nodes().into_iter().map(|(n, _)| n).collect();
    let targets: Vec<String> = match node {
        Some(n) if nodes.iter().any(|x| x == n) => vec![n.to_owned()],
        Some(n) => bail!("no node {n:?} in this cluster; nodes: {}", nodes.join(", ")),
        None => nodes,
    };
    for t in &targets {
        r.run_server(
            &kubectl(
                "kairon-label",
                &format!("label node {t} --overwrite {}", labels.join(" ")),
            ),
            None,
        )
        .await?;
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::super::platform::fake::FakeRemote;
    use super::*;

    #[test]
    fn prometheus_helmchart_golden() {
        let y = prometheus_helmchart("93.6.1", "24h").unwrap();
        let want = "apiVersion: helm.cattle.io/v1\nkind: HelmChart\nmetadata:\n  name: kube-prometheus-stack\n  namespace: kube-system\nspec:\n  repo: https://prometheus-community.github.io/helm-charts\n  chart: kube-prometheus-stack\n  version: 93.6.1\n  targetNamespace: monitoring\n  createNamespace: true\n  valuesContent: |-\n    prometheus:\n      prometheusSpec:\n        retention: 24h\n        resources:\n          requests:\n            cpu: 100m\n            memory: 400Mi\n    alertmanager:\n      enabled: false\n    kubeEtcd:\n      enabled: false\n    kubeControllerManager:\n      enabled: false\n    kubeScheduler:\n      enabled: false\n    kubeProxy:\n      enabled: false\n";
        assert_eq!(y, want);
        let doc: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
        let values: serde_yaml::Value =
            serde_yaml::from_str(doc["spec"]["valuesContent"].as_str().unwrap()).unwrap();
        assert_eq!(values["prometheus"]["prometheusSpec"]["retention"], "24h");
        assert!(prometheus_helmchart("9;3", "24h").is_err());
        assert!(prometheus_helmchart("", "24h").is_err());
        assert!(prometheus_helmchart("93.6.1", "forever").is_err());
    }

    #[test]
    fn local_path_class_golden() {
        assert_eq!(
            local_path_class("fluxvm-local", true, false).unwrap(),
            "apiVersion: storage.k8s.io/v1\nkind: StorageClass\nmetadata:\n  name: fluxvm-local\nprovisioner: rancher.io/local-path\nreclaimPolicy: Retain\nvolumeBindingMode: WaitForFirstConsumer\nallowVolumeExpansion: false\n"
        );
        let d = local_path_class("x", false, true).unwrap();
        assert!(d.contains("is-default-class: \"true\""));
        assert!(d.contains("reclaimPolicy: Delete"));
        assert!(local_path_class("Bad", false, false).is_err());
        assert!(LONGHORN_GUIDANCE.contains("guidance only"));
    }

    #[test]
    fn kairon_labels() {
        assert_eq!(
            kairon_label("topology=tb5-mesh").unwrap(),
            "kairon.zyvor.dev/topology=tb5-mesh"
        );
        assert_eq!(
            kairon_label("kairon.zyvor.dev/rack=studio-a").unwrap(),
            "kairon.zyvor.dev/rack=studio-a"
        );
        assert!(kairon_label("topology").is_err());
        assert!(kairon_label("topology=").is_err());
        assert!(kairon_label("topology=has space").is_err());
        assert!(kairon_label("bad key=x").is_err());
    }

    #[tokio::test]
    async fn prometheus_pipes_the_helmchart_on_stdin() {
        let r = FakeRemote::cluster(1, 0);
        let rep = prometheus_stack(&r, "7d", &["metrics-server".to_owned()])
            .await
            .unwrap();
        assert_eq!(r.ops(), ["c-s1:prometheus-apply"]);
        assert!(
            r.log.borrow()[0]
                .2
                .as_deref()
                .unwrap()
                .contains("retention: 7d")
        );
        assert!(
            rep.notes
                .iter()
                .any(|n| n.contains("metrics-server is disabled"))
        );
        let r = FakeRemote::cluster(1, 0);
        assert!(prometheus_stack(&r, "forever", &[]).await.is_err());
        assert!(r.ops().is_empty());
    }

    #[tokio::test]
    async fn local_path_refused_when_the_provisioner_is_disabled() {
        let y = local_path_class("fluxvm-local", false, false).unwrap();
        let r = FakeRemote::cluster(1, 0);
        apply_local_path(&r, &y, &[]).await.unwrap();
        assert_eq!(r.ops(), ["c-s1:storageclass-apply"]);
        let r = FakeRemote::cluster(1, 0);
        assert!(
            apply_local_path(&r, &y, &["local-storage".to_owned()])
                .await
                .is_err()
        );
        assert!(r.ops().is_empty());
    }

    #[tokio::test]
    async fn labels_one_node_or_all_after_validating() {
        let r = FakeRemote::cluster(1, 1);
        let t = label_nodes(&r, &["topology=tb5-mesh".to_owned()], None)
            .await
            .unwrap();
        assert_eq!(t, ["c-s1", "c-a1"]);
        assert_eq!(r.ops(), ["c-s1:kairon-label", "c-s1:kairon-label"]);
        assert!(
            r.log.borrow()[1]
                .1
                .contains("label node c-a1 --overwrite kairon.zyvor.dev/topology=tb5-mesh")
        );
        let r = FakeRemote::cluster(1, 1);
        assert!(
            label_nodes(&r, &["x=y".to_owned()], Some("nope"))
                .await
                .is_err()
        );
        assert!(label_nodes(&r, &["x".to_owned()], None).await.is_err());
        assert!(label_nodes(&r, &[], None).await.is_err());
        assert!(r.ops().is_empty());
    }
}
