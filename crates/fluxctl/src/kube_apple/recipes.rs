// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Recipes for common cluster add-ons, each installed from a pinned, checksum-verified manifest ([`super::pins`]): a MetalLB
//! address pool (replacing ServiceLB), cert-manager, and a stock metrics-server for clusters that turned the bundled one off.
//! The generators (`metallb_pool_yaml`) are pure; the installs go through [`Remote`]. Not run against real hardware.

use super::pins::{self, Pin, Source};
use super::platform::Remote;
use super::script::K3S_BIN;
use anyhow::{Context, Result, bail};
use serde::Serialize;

/// Where a manifest comes from.
pub enum PinSrc {
    /// The first server downloads the pinned URL and checks the pinned SHA-256.
    Download,
    /// A file from this Mac (air-gapped, or a different version), checked against `sha` here and again on the server.
    File { data: Vec<u8>, sha: String },
}

/// Gets one manifest onto a server, verifies it and applies it. Nothing is applied on a digest mismatch.
pub async fn apply_pin<R: Remote>(r: &R, pin: &Pin, src: PinSrc) -> Result<String> {
    match src {
        PinSrc::Download => r.run_server(&pins::apply_script(pin, Source::Download, pin.sha256), None).await,
        PinSrc::File { data, sha } => {
            pins::verify_bytes(&data, &sha)?;
            r.run_server(&pins::apply_script(pin, Source::Stdin, &sha), Some(&data)).await
        }
    }
}

/// Reads `--manifest-file` (at most 8 MiB) and the digest to hold it to: `--sha256`, or the pin's own.
pub fn manifest_file(path: &std::path::Path, sha256: Option<&str>, pin: &Pin) -> Result<PinSrc> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > 8 * 1024 * 1024 {
        bail!("{} is larger than 8 MiB", path.display());
    }
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let sha = match sha256 {
        Some(s) => pins::parse_sha256_override(s)?,
        None => pin.sha256.to_owned(),
    };
    pins::verify_bytes(&data, &sha)?;
    Ok(PinSrc::File { data, sha })
}

#[derive(Debug, Serialize, Default)]
pub struct RecipeReport {
    pub recipe: String,
    pub version: String,
    pub applied: Vec<String>,
    pub notes: Vec<String>,
}

fn kubectl(op: &str, args: &str) -> String {
    format!(": fluxvm-op {op}; set -eu; sudo -n {K3S_BIN} kubectl {args}")
}

// ---------------------------------------------------------------- MetalLB

/// One address entry of a pool: an IPv4 CIDR (`10.89.0.192/28`) or a range (`10.89.0.200-10.89.0.220`).
pub fn pool_address_ok(a: &str) -> bool {
    if let Some((ip, bits)) = a.split_once('/') {
        return ip.parse::<std::net::Ipv4Addr>().is_ok() && bits.parse::<u8>().is_ok_and(|b| (8..=32).contains(&b));
    }
    match a.split_once('-') {
        Some((lo, hi)) => match (lo.parse::<std::net::Ipv4Addr>(), hi.parse::<std::net::Ipv4Addr>()) {
            (Ok(l), Ok(h)) => u32::from(l) <= u32::from(h),
            _ => false,
        },
        None => false,
    }
}

pub fn dns_label_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// An `IPAddressPool` and an `L2Advertisement` for it (layer 2 / ARP: the right mode for VMs on one private network).
pub fn metallb_pool_yaml(name: &str, addresses: &[String], auto_assign: bool) -> Result<String> {
    if !dns_label_ok(name) {
        bail!("pool name {name:?} must be a DNS label (lowercase letters, digits, -)");
    }
    if addresses.is_empty() {
        bail!("give at least one --address (a CIDR such as 10.89.0.192/28 or a range 10.89.0.200-10.89.0.220)");
    }
    for a in addresses {
        if !pool_address_ok(a) {
            bail!("address {a:?} must be an IPv4 CIDR (/8 to /32) or a range LOW-HIGH");
        }
    }
    let list: String = addresses.iter().map(|a| format!("  - {a}\n")).collect();
    Ok(format!(
        "apiVersion: metallb.io/v1beta1\n\
         kind: IPAddressPool\n\
         metadata:\n  name: {name}\n  namespace: metallb-system\n\
         spec:\n  autoAssign: {auto_assign}\n  addresses:\n{list}\
         ---\n\
         apiVersion: metallb.io/v1beta1\n\
         kind: L2Advertisement\n\
         metadata:\n  name: {name}\n  namespace: metallb-system\n\
         spec:\n  ipAddressPools:\n  - {name}\n"
    ))
}

pub struct MetalLbReq {
    pub name: String,
    pub addresses: Vec<String>,
    pub auto_assign: bool,
    pub src: PinSrc,
    /// The cluster's `--disable` list, to warn about ServiceLB.
    pub disabled: Vec<String>,
}

pub async fn metallb<R: Remote>(r: &R, req: MetalLbReq) -> Result<RecipeReport> {
    // Validate everything before the first install.
    let pool = metallb_pool_yaml(&req.name, &req.addresses, req.auto_assign)?;
    let mut rep = RecipeReport { recipe: "metallb".into(), version: pins::METALLB.version.into(), ..Default::default() };
    if !req.disabled.iter().any(|d| d == "servicelb") {
        rep.notes.push("ServiceLB is still enabled and also answers for LoadBalancer Services; create the cluster with --disable servicelb (or --addons metallb) so the two do not compete".into());
    }
    apply_pin(r, &pins::METALLB, req.src).await?;
    rep.applied.push(format!("metallb {}", pins::METALLB.version));
    // The pool is checked by MetalLB's admission webhook, which exists only once the controller runs.
    r.run_server(&kubectl("metallb-rollout", "-n metallb-system rollout status deployment/controller --timeout=180s"), None).await?;
    r.run_server(&kubectl("metallb-pool", "apply -f -"), Some(pool.as_bytes())).await?;
    rep.applied.push(format!("IPAddressPool and L2Advertisement {}", req.name));
    rep.notes.push("the addresses must be unused and on the nodes' own network (for kube apple, the private 10.89.x.y network); they are not reachable from the Mac unless you route to that network".into());
    Ok(rep)
}

// ---------------------------------------------------------------- cert-manager and metrics-server

pub async fn cert_manager<R: Remote>(r: &R, src: PinSrc) -> Result<RecipeReport> {
    let mut rep = RecipeReport { recipe: "cert-manager".into(), version: pins::CERT_MANAGER.version.into(), ..Default::default() };
    apply_pin(r, &pins::CERT_MANAGER, src).await?;
    rep.applied.push(format!("cert-manager {}", pins::CERT_MANAGER.version));
    for d in ["cert-manager", "cert-manager-cainjector", "cert-manager-webhook"] {
        r.run_server(&kubectl("cert-manager-rollout", &format!("-n cert-manager rollout status deployment/{d} --timeout=180s")), None).await?;
    }
    rep.notes.push("create an Issuer or ClusterIssuer next; the webhook is ready, so it is accepted at once".into());
    Ok(rep)
}

pub const INSECURE_TLS_PATCH: &str =
    r#"[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"--kubelet-insecure-tls"}]"#;

pub async fn metrics_server<R: Remote>(r: &R, src: PinSrc, kubelet_insecure_tls: bool, disabled: &[String]) -> Result<RecipeReport> {
    let mut rep = RecipeReport { recipe: "metrics-server".into(), version: pins::METRICS_SERVER.version.into(), ..Default::default() };
    if !disabled.iter().any(|d| d == "metrics-server") {
        bail!("k3s's bundled metrics-server is still on and owns the same objects; create the cluster with --disable metrics-server (or --addons minimal) if you want this one");
    }
    apply_pin(r, &pins::METRICS_SERVER, src).await?;
    rep.applied.push(format!("metrics-server {}", pins::METRICS_SERVER.version));
    if kubelet_insecure_tls {
        r.run_server(&kubectl("metrics-patch", &format!("-n kube-system patch deployment metrics-server --type=json -p '{INSECURE_TLS_PATCH}'")), None).await?;
        rep.applied.push("added --kubelet-insecure-tls (the kubelet serving certificates are signed by the k3s CA, which metrics-server does not trust; the connection to the kubelets is not verified)".into());
    } else {
        rep.notes.push("if `kubectl top nodes` reports certificate errors, the kubelet certificates are signed by the k3s CA; re-run with --kubelet-insecure-tls to skip that verification (a security trade-off)".into());
    }
    r.run_server(&kubectl("metrics-rollout", "-n kube-system rollout status deployment/metrics-server --timeout=180s"), None).await?;
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::super::platform::fake::FakeRemote;
    use super::*;

    #[test]
    fn pool_addresses() {
        for ok in ["10.89.0.192/28", "10.89.0.200-10.89.0.220", "10.0.0.5-10.0.0.5", "192.168.1.1/32"] {
            assert!(pool_address_ok(ok), "{ok}");
        }
        for bad in ["10.89.0.0/7", "10.89.0.0/33", "10.89.0.220-10.89.0.200", "10.89.0.1", "fe80::/64", "a-b", "10.89.0.1/x", ""] {
            assert!(!pool_address_ok(bad), "{bad}");
        }
    }

    #[test]
    fn pool_golden() {
        let y = metallb_pool_yaml("lan", &["10.89.0.200-10.89.0.220".into(), "10.89.1.0/28".into()], true).unwrap();
        assert_eq!(
            y,
            "apiVersion: metallb.io/v1beta1\n\
kind: IPAddressPool\n\
metadata:\n  name: lan\n  namespace: metallb-system\n\
spec:\n  autoAssign: true\n  addresses:\n  - 10.89.0.200-10.89.0.220\n  - 10.89.1.0/28\n\
---\n\
apiVersion: metallb.io/v1beta1\n\
kind: L2Advertisement\n\
metadata:\n  name: lan\n  namespace: metallb-system\n\
spec:\n  ipAddressPools:\n  - lan\n"
        );
        for d in serde_yaml::Deserializer::from_str(&y) {
            let _: serde_yaml::Value = serde::Deserialize::deserialize(d).unwrap();
        }
        assert!(metallb_pool_yaml("Bad_Name", &["10.0.0.0/24".into()], true).is_err());
        assert!(metallb_pool_yaml("x", &[], true).is_err());
        assert!(metallb_pool_yaml("x", &["10.0.0.0/24\n  - evil".into()], true).is_err());
    }

    #[tokio::test]
    async fn metallb_installs_pinned_manifest_waits_for_the_controller_then_adds_the_pool() {
        let r = FakeRemote::cluster(1, 0);
        let rep = metallb(&r, MetalLbReq { name: "lan".into(), addresses: vec!["10.89.0.200-10.89.0.220".into()], auto_assign: true, src: PinSrc::Download, disabled: vec![] }).await.unwrap();
        let ops = r.ops();
        assert_eq!(ops, ["c-s1:pin-apply", "c-s1:metallb-rollout", "c-s1:metallb-pool"]);
        assert!(r.log.borrow()[0].1.contains(pins::METALLB.sha256));
        assert!(rep.notes.iter().any(|n| n.contains("ServiceLB is still enabled")));
        let rep = metallb(&FakeRemote::cluster(1, 0), MetalLbReq { name: "lan".into(), addresses: vec!["10.0.0.0/24".into()], auto_assign: false, src: PinSrc::Download, disabled: vec!["servicelb".into()] }).await.unwrap();
        assert!(!rep.notes.iter().any(|n| n.contains("ServiceLB")));
    }

    #[tokio::test]
    async fn bad_pool_installs_nothing() {
        let r = FakeRemote::cluster(1, 0);
        assert!(metallb(&r, MetalLbReq { name: "lan".into(), addresses: vec!["nope".into()], auto_assign: true, src: PinSrc::Download, disabled: vec![] }).await.is_err());
        assert!(r.log.borrow().is_empty());
    }

    #[tokio::test]
    async fn local_manifest_must_match_its_digest_before_anything_is_sent() {
        let r = FakeRemote::cluster(1, 0);
        let bad = PinSrc::File { data: b"apiVersion: v1\n".to_vec(), sha: pins::CERT_MANAGER.sha256.into() };
        assert!(cert_manager(&r, bad).await.unwrap_err().to_string().contains("nothing was sent"));
        assert!(r.log.borrow().is_empty());
        let data = b"apiVersion: v1\n".to_vec();
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&data));
        cert_manager(&r, PinSrc::File { data: data.clone(), sha: sha.clone() }).await.unwrap();
        let log = r.log.borrow();
        assert_eq!(log[0].2.as_deref(), Some("apiVersion: v1\n"));
        assert!(log[0].1.contains(&sha) && log[0].1.contains("cat > \"$T\""));
        assert_eq!(r.ops().iter().filter(|o| o.ends_with("cert-manager-rollout")).count(), 3);
    }

    #[tokio::test]
    async fn metrics_server_refuses_while_the_bundled_one_runs_and_patches_only_when_asked() {
        let r = FakeRemote::cluster(1, 0);
        assert!(metrics_server(&r, PinSrc::Download, false, &[]).await.is_err());
        assert!(r.log.borrow().is_empty());
        let d = vec!["metrics-server".to_owned()];
        let rep = metrics_server(&r, PinSrc::Download, false, &d).await.unwrap();
        assert!(!r.ops().iter().any(|o| o.ends_with("metrics-patch")));
        assert!(rep.notes[0].contains("--kubelet-insecure-tls"));
        let r = FakeRemote::cluster(1, 0);
        metrics_server(&r, PinSrc::Download, true, &d).await.unwrap();
        assert!(r.log.borrow().iter().any(|(_, s, _)| s.contains("--kubelet-insecure-tls") && s.contains("patch deployment metrics-server")));
    }
}
