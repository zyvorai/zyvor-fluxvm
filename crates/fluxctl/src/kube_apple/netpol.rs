// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! A baseline NetworkPolicy set for one namespace: deny all ingress and egress, then allow DNS, traffic inside the
//! namespace, and what you name. Pure generator (golden-tested); `--apply` pipes it to `kubectl apply -f -` on a server.
//! A policy only does something if the CNI enforces it: k3s's bundled kube-router controller does, unless the cluster was
//! made with `--disable-network-policy` (as `--cilium` does, in which case Cilium enforces them).

use super::platform::Remote;
use super::recipes::dns_label_ok;
use super::script::K3S_BIN;
use anyhow::{Result, bail};

#[derive(Debug, Clone, Default)]
pub struct NetpolOpts {
    pub namespace: String,
    /// Do not add the DNS egress rule (pods then cannot resolve names).
    pub no_dns: bool,
    /// Do not allow traffic between pods of this namespace.
    pub no_same_namespace: bool,
    /// Namespaces allowed to send traffic in.
    pub allow_from: Vec<String>,
    /// CIDRs pods may send traffic to (for example a database outside the cluster).
    pub allow_egress_cidr: Vec<String>,
}

pub const PROTECTED: [&str; 3] = ["kube-system", "kube-public", "kube-node-lease"];

pub fn cidr_ok(c: &str) -> bool {
    let Some((ip, bits)) = c.split_once('/') else { return false };
    match (ip.parse::<std::net::IpAddr>(), bits.parse::<u8>()) {
        (Ok(std::net::IpAddr::V4(_)), Ok(b)) => b <= 32,
        (Ok(std::net::IpAddr::V6(_)), Ok(b)) => b <= 128,
        _ => false,
    }
}

fn header(name: &str, ns: &str) -> String {
    format!(
        "apiVersion: networking.k8s.io/v1\nkind: NetworkPolicy\nmetadata:\n  name: {name}\n  namespace: {ns}\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\n"
    )
}

pub fn baseline(o: &NetpolOpts) -> Result<String> {
    let ns = &o.namespace;
    if !dns_label_ok(ns) {
        bail!("namespace {ns:?} must be a DNS label");
    }
    if PROTECTED.contains(&ns.as_str()) {
        bail!("{ns} runs the cluster itself; a deny-all policy there would break it");
    }
    for a in &o.allow_from {
        if !dns_label_ok(a) {
            bail!("--allow-from {a:?} must be a namespace name");
        }
    }
    for c in &o.allow_egress_cidr {
        if !cidr_ok(c) {
            bail!("--allow-egress-cidr {c:?} must be a CIDR such as 10.0.0.0/8");
        }
    }
    let mut docs = vec![format!(
        "{}spec:\n  podSelector: {{}}\n  policyTypes:\n  - Ingress\n  - Egress\n",
        header("baseline-default-deny", ns)
    )];
    if !o.no_dns {
        docs.push(format!(
            "{}spec:\n  podSelector: {{}}\n  policyTypes:\n  - Egress\n  egress:\n  - to:\n    - namespaceSelector:\n        matchLabels:\n          kubernetes.io/metadata.name: kube-system\n      podSelector:\n        matchLabels:\n          k8s-app: kube-dns\n    ports:\n    - protocol: UDP\n      port: 53\n    - protocol: TCP\n      port: 53\n",
            header("baseline-allow-dns", ns)
        ));
    }
    if !o.no_same_namespace {
        docs.push(format!(
            "{}spec:\n  podSelector: {{}}\n  policyTypes:\n  - Ingress\n  - Egress\n  ingress:\n  - from:\n    - podSelector: {{}}\n  egress:\n  - to:\n    - podSelector: {{}}\n",
            header("baseline-allow-same-namespace", ns)
        ));
    }
    for a in &o.allow_from {
        docs.push(format!(
            "{}spec:\n  podSelector: {{}}\n  policyTypes:\n  - Ingress\n  ingress:\n  - from:\n    - namespaceSelector:\n        matchLabels:\n          kubernetes.io/metadata.name: {a}\n",
            header(&format!("baseline-allow-from-{a}"), ns)
        ));
    }
    if !o.allow_egress_cidr.is_empty() {
        let blocks: String = o.allow_egress_cidr.iter().map(|c| format!("    - ipBlock:\n        cidr: {c}\n")).collect();
        docs.push(format!(
            "{}spec:\n  podSelector: {{}}\n  policyTypes:\n  - Egress\n  egress:\n  - to:\n{blocks}",
            header("baseline-allow-egress-cidrs", ns)
        ));
    }
    Ok(docs.join("---\n"))
}

pub async fn apply<R: Remote>(r: &R, o: &NetpolOpts) -> Result<String> {
    let yaml = baseline(o)?;
    r.run_server(&format!(": fluxvm-op netpol-ns; set -eu; sudo -n {K3S_BIN} kubectl get namespace {} -o name", o.namespace), None)
        .await
        .map_err(|e| anyhow::anyhow!("namespace {} does not exist ({e:#}); create it first", o.namespace))?;
    r.run_server(&format!(": fluxvm-op netpol-apply; set -eu; sudo -n {K3S_BIN} kubectl apply -f -"), Some(yaml.as_bytes())).await
}

#[cfg(test)]
mod tests {
    use super::super::platform::fake::FakeRemote;
    use super::*;

    fn opts() -> NetpolOpts {
        NetpolOpts { namespace: "app".into(), ..Default::default() }
    }

    #[test]
    fn default_set_golden() {
        let want = "apiVersion: networking.k8s.io/v1\nkind: NetworkPolicy\nmetadata:\n  name: baseline-default-deny\n  namespace: app\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\nspec:\n  podSelector: {}\n  policyTypes:\n  - Ingress\n  - Egress\n\
---\n\
apiVersion: networking.k8s.io/v1\nkind: NetworkPolicy\nmetadata:\n  name: baseline-allow-dns\n  namespace: app\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\nspec:\n  podSelector: {}\n  policyTypes:\n  - Egress\n  egress:\n  - to:\n    - namespaceSelector:\n        matchLabels:\n          kubernetes.io/metadata.name: kube-system\n      podSelector:\n        matchLabels:\n          k8s-app: kube-dns\n    ports:\n    - protocol: UDP\n      port: 53\n    - protocol: TCP\n      port: 53\n\
---\n\
apiVersion: networking.k8s.io/v1\nkind: NetworkPolicy\nmetadata:\n  name: baseline-allow-same-namespace\n  namespace: app\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\nspec:\n  podSelector: {}\n  policyTypes:\n  - Ingress\n  - Egress\n  ingress:\n  - from:\n    - podSelector: {}\n  egress:\n  - to:\n    - podSelector: {}\n";
        assert_eq!(baseline(&opts()).unwrap(), want);
    }

    #[test]
    fn options_add_and_remove_policies_and_every_document_parses() {
        let o = NetpolOpts { no_dns: true, no_same_namespace: true, allow_from: vec!["ingress-nginx".into()], allow_egress_cidr: vec!["10.0.0.0/8".into(), "fd00::/8".into()], ..opts() };
        let y = baseline(&o).unwrap();
        let names: Vec<String> = serde_yaml::Deserializer::from_str(&y)
            .map(|d| {
                let v: serde_yaml::Value = serde::Deserialize::deserialize(d).unwrap();
                assert_eq!(v["kind"], "NetworkPolicy");
                v["metadata"]["name"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(names, ["baseline-default-deny", "baseline-allow-from-ingress-nginx", "baseline-allow-egress-cidrs"]);
        assert!(y.contains("kubernetes.io/metadata.name: ingress-nginx"));
        assert!(y.contains("    - ipBlock:\n        cidr: 10.0.0.0/8\n    - ipBlock:\n        cidr: fd00::/8\n"));
    }

    #[test]
    fn refuses_system_namespaces_and_odd_input() {
        for ns in PROTECTED {
            assert!(baseline(&NetpolOpts { namespace: ns.into(), ..Default::default() }).is_err());
        }
        assert!(baseline(&NetpolOpts { namespace: "A b".into(), ..Default::default() }).is_err());
        assert!(baseline(&NetpolOpts { allow_from: vec!["x\ny".into()], ..opts() }).is_err());
        assert!(baseline(&NetpolOpts { allow_egress_cidr: vec!["10.0.0.0".into()], ..opts() }).is_err());
        assert!(baseline(&NetpolOpts { allow_egress_cidr: vec!["10.0.0.0/33".into()], ..opts() }).is_err());
        assert!(cidr_ok("0.0.0.0/0") && cidr_ok("::/0"));
    }

    #[tokio::test]
    async fn apply_checks_the_namespace_then_pipes_the_yaml() {
        let r = FakeRemote::cluster(1, 0);
        apply(&r, &opts()).await.unwrap();
        assert_eq!(r.ops(), ["c-s1:netpol-ns", "c-s1:netpol-apply"]);
        assert!(r.log.borrow()[1].2.as_deref().unwrap().contains("baseline-default-deny"));
        let r = FakeRemote::cluster(1, 0);
        r.fail_on("netpol-ns", "NotFound");
        assert!(apply(&r, &opts()).await.unwrap_err().to_string().contains("does not exist"));
        assert_eq!(r.ops().len(), 1);
    }
}
