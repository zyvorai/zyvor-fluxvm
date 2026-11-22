// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Pure functions for `kube kairon`: the pinned release, the `helm` and `kubectl` argument lists, node labels and the Secret
//! manifests. No I/O here, so every output is pinned by a golden test.

use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use std::path::Path;

pub const CHART_REF: &str = "oci://ghcr.io/zyvorai/charts/kairon";
/// The Kairon release this fluxctl installs. Chart version and app version are the same number.
pub const CHART_VERSION: &str = "0.10.6";
/// Manifest digest of `ghcr.io/zyvorai/charts/kairon:0.10.6`, read from the registry (`crane digest`) and from `helm pull`'s own
/// `Digest:` line on 2026-11-22. `helm pull` is refused when it reports anything else.
pub const CHART_DIGEST: &str =
    "sha256:eb952429842601e30bc6cd6467f4039638ae21d11e6281607a79473b19a6b255";
pub const RELEASE: &str = "kairon";
pub const NAMESPACE: &str = "kairon-system";
pub const CAPABLE_LABEL: &str = "kairon.zyvor.dev/capable";
pub const WEBHOOK_SECRET: &str = "kairon-webhook-tls";
pub const WEBHOOK_SERVICE: &str = "kairon-controller-webhook";
pub const MIGRATION_SECRET: &str = "kairon-migration-tls";
/// The DNS name the chart's `migration.tlsServerName` expects on the control-plane certificate.
pub const MIGRATION_SERVER_NAME: &str = "kairon-node";
pub const NOT_AFTER_ANNOTATION: &str = "fluxvm.zyvor.dev/not-after";
pub const CRD_GROUP: &str = "kairon.zyvor.dev";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Evaluation,
    Production,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Profile::Evaluation => "evaluation",
            Profile::Production => "production",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub version: String,
    pub digest: String,
}

impl Pin {
    pub fn pinned() -> Pin {
        Pin {
            version: CHART_VERSION.into(),
            digest: CHART_DIGEST.into(),
        }
    }
    /// An override needs both halves: a version without its digest would defeat the pin.
    pub fn from_flags(version: Option<&str>, digest: Option<&str>) -> Result<Pin> {
        match (version, digest) {
            (None, None) => Ok(Pin::pinned()),
            (Some(v), Some(d)) => {
                check_version(v)?;
                check_digest(d)?;
                Ok(Pin {
                    version: v.into(),
                    digest: d.into(),
                })
            }
            (Some(v), None) if v == CHART_VERSION => Ok(Pin::pinned()),
            (Some(_), None) => {
                bail!(
                    "--chart-version other than {CHART_VERSION} needs --chart-digest sha256:... (the digest of that chart in the registry)"
                )
            }
            (None, Some(_)) => bail!("--chart-digest needs --chart-version"),
        }
    }
}

fn check_version(v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && v.split('.').count() == 3
        && v.split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        bail!("--chart-version {v:?} must look like 0.10.6");
    }
    Ok(())
}

fn check_digest(d: &str) -> Result<()> {
    let ok = d
        .strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()));
    if !ok {
        bail!("--chart-digest {d:?} must be sha256: followed by 64 hex digits");
    }
    Ok(())
}

/// The `Digest:` line `helm pull` prints for an OCI chart.
pub fn parse_pulled_digest(helm_output: &str) -> Option<String> {
    helm_output
        .lines()
        .find_map(|l| l.trim().strip_prefix("Digest:"))
        .map(|d| d.trim().to_string())
}

/// Refuses unless helm pulled exactly the pinned manifest.
pub fn verify_digest(helm_output: &str, pin: &Pin) -> Result<()> {
    match parse_pulled_digest(helm_output) {
        Some(d) if d.eq_ignore_ascii_case(&pin.digest) => Ok(()),
        Some(d) => bail!(
            "the registry served chart {} with digest {d}, not the pinned {}; nothing was installed. \
             Do not continue unless you know why the chart changed",
            pin.version,
            pin.digest
        ),
        None => bail!(
            "helm pull did not report a digest, so the chart cannot be verified; nothing was installed"
        ),
    }
}

/// Which cluster every call targets.
#[derive(Debug, Clone)]
pub struct Target {
    pub kubeconfig: std::path::PathBuf,
    pub context: Option<String>,
}

impl Target {
    fn flags(&self) -> Vec<String> {
        let mut v = vec!["--kubeconfig".into(), self.kubeconfig.display().to_string()];
        if let Some(c) = &self.context {
            v.push("--context".into());
            v.push(c.clone());
        }
        v
    }
    pub fn kubectl(&self, args: &[&str]) -> Vec<String> {
        let mut v = self.flags();
        v.extend(args.iter().map(|s| s.to_string()));
        v
    }
    pub fn helm(&self, args: &[&str]) -> Vec<String> {
        self.kubectl(args)
    }
}

pub fn helm_pull_args(pin: &Pin, dest: &Path) -> Vec<String> {
    vec![
        "pull".into(),
        CHART_REF.into(),
        "--version".into(),
        pin.version.clone(),
        "--destination".into(),
        dest.display().to_string(),
    ]
}

#[derive(Debug, Clone, Default)]
pub struct InstallOpts {
    pub production: bool,
    pub ui: bool,
    pub image_cache_dir: Option<String>,
    /// base64 CA for `webhook.caBundle` (production only). A CA certificate is public.
    pub ca_bundle: Option<String>,
}

/// `helm upgrade --install` for the already verified chart archive. `values_file` is the production overlay extracted from
/// that same archive.
pub fn helm_install_args(
    t: &Target,
    chart: &Path,
    o: &InstallOpts,
    values_file: Option<&Path>,
) -> Result<Vec<String>> {
    let mut a: Vec<String> = ["upgrade", "--install", RELEASE]
        .iter()
        .map(|s| s.to_string())
        .collect();
    a.push(chart.display().to_string());
    for s in ["-n", NAMESPACE, "--create-namespace", "--timeout", "5m"] {
        a.push(s.into());
    }
    if o.production {
        let Some(vf) = values_file else {
            bail!("the production profile needs the chart's values-production.yaml");
        };
        let Some(ca) = &o.ca_bundle else {
            bail!("the production profile needs the webhook CA bundle");
        };
        a.push("-f".into());
        a.push(vf.display().to_string());
        a.push("--set".into());
        a.push(format!("webhook.tlsSecretName={WEBHOOK_SECRET}"));
        a.push("--set".into());
        a.push(format!("webhook.caBundle={ca}"));
    }
    if o.ui {
        a.push("--set".into());
        a.push("ui.enabled=true".into());
    }
    if let Some(d) = &o.image_cache_dir {
        a.push("--set".into());
        a.push(format!("node.imageCacheDir={d}"));
    }
    let mut full = t.helm(&[]);
    full.extend(a);
    Ok(full)
}

/// The labels one node gets. `backend` is the Kairon backend name (`vz` for a Mac, and for a Linux FluxVM host whatever
/// FluxVM backend Kairon was told to use).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeLabels {
    pub backend: Option<String>,
    pub chip: Option<String>,
    pub memory_gib: Option<u64>,
}

pub fn label_pairs(l: &NodeLabels) -> Vec<String> {
    let mut v = vec![format!("{CAPABLE_LABEL}=true")];
    if let Some(b) = &l.backend {
        v.push(format!("{CRD_GROUP}/backend.{b}=true"));
    }
    if let Some(c) = &l.chip {
        v.push(format!("{CRD_GROUP}/chip={c}"));
    }
    if let Some(m) = l.memory_gib {
        v.push(format!("{CRD_GROUP}/memory-gib={m}"));
    }
    v
}

pub fn check_label_value(what: &str, v: &str) -> Result<()> {
    let ok = !v.is_empty()
        && v.len() <= 63
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && v.starts_with(|c: char| c.is_ascii_alphanumeric());
    if !ok {
        bail!("{what} {v:?}: letters, digits, - _ . only, at most 63 characters");
    }
    Ok(())
}

pub fn label_args(t: &Target, node: &str, l: &NodeLabels) -> Vec<String> {
    let mut a = vec!["label".to_string(), "node".into(), node.into()];
    a.extend(label_pairs(l));
    a.push("--overwrite".into());
    let mut full = t.kubectl(&[]);
    full.extend(a);
    full
}

fn b64(s: &str) -> String {
    B64.encode(s.as_bytes())
}

/// The webhook serving certificate as a `kubernetes.io/tls` Secret. `ca.crt` rides along (the type allows extra keys) so that
/// a later `upgrade` can read the CA back for `webhook.caBundle`. `not_after` is recorded as an annotation for `status`.
pub fn webhook_secret_yaml(cert_pem: &str, key_pem: &str, ca_pem: &str, not_after: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {WEBHOOK_SECRET}\n  namespace: {NAMESPACE}\n  annotations:\n    {NOT_AFTER_ANNOTATION}: \"{not_after}\"\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\ntype: kubernetes.io/tls\ndata:\n  tls.crt: {}\n  tls.key: {}\n  ca.crt: {}\n",
        b64(cert_pem),
        b64(key_pem),
        b64(ca_pem)
    )
}

pub fn migration_secret_yaml(
    cert_pem: &str,
    key_pem: &str,
    ca_pem: &str,
    not_after: &str,
) -> String {
    format!(
        "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {MIGRATION_SECRET}\n  namespace: {NAMESPACE}\n  annotations:\n    {NOT_AFTER_ANNOTATION}: \"{not_after}\"\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\ntype: Opaque\ndata:\n  ca.crt: {}\n  tls.crt: {}\n  tls.key: {}\n",
        b64(ca_pem),
        b64(cert_pem),
        b64(key_pem)
    )
}

pub fn namespace_yaml() -> String {
    format!("apiVersion: v1\nkind: Namespace\nmetadata:\n  name: {NAMESPACE}\n")
}

/// The base64 CA bundle for `webhook.caBundle`.
pub fn ca_bundle(ca_pem: &str) -> String {
    b64(ca_pem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn t() -> Target {
        Target {
            kubeconfig: PathBuf::from("/h/.kube/fluxvm-fluxvm-kube.yaml"),
            context: None,
        }
    }

    #[test]
    fn pin_matches_the_chart_that_was_checked() {
        let p = Pin::pinned();
        assert_eq!(p.version, "0.10.6");
        assert!(check_digest(&p.digest).is_ok());
    }

    #[test]
    fn pin_overrides_need_both_halves() {
        assert!(Pin::from_flags(None, None).unwrap() == Pin::pinned());
        assert!(Pin::from_flags(Some("0.10.6"), None).is_ok());
        assert!(Pin::from_flags(Some("0.10.7"), None).is_err());
        assert!(Pin::from_flags(None, Some(CHART_DIGEST)).is_err());
        assert!(Pin::from_flags(Some("0.10.7"), Some("sha256:abc")).is_err());
        assert!(Pin::from_flags(Some("0.10.x"), Some(CHART_DIGEST)).is_err());
        assert!(Pin::from_flags(Some("0.10.7"), Some(CHART_DIGEST)).is_ok());
    }

    #[test]
    fn digest_from_helm_pull_output() {
        let out = format!("Pulled: ghcr.io/zyvorai/charts/kairon:0.10.6\nDigest: {CHART_DIGEST}\n");
        assert_eq!(parse_pulled_digest(&out).as_deref(), Some(CHART_DIGEST));
        assert!(verify_digest(&out, &Pin::pinned()).is_ok());
    }

    #[test]
    fn digest_mismatch_and_missing_are_refused() {
        let bad = "Pulled: x\nDigest: sha256:0000000000000000000000000000000000000000000000000000000000000000\n";
        let e = verify_digest(bad, &Pin::pinned()).unwrap_err().to_string();
        assert!(e.contains("not the pinned") && e.contains("nothing was installed"));
        assert!(verify_digest("Pulled: x\n", &Pin::pinned()).is_err());
    }

    #[test]
    fn pull_args_golden() {
        assert_eq!(
            helm_pull_args(&Pin::pinned(), Path::new("/tmp/d")).join(" "),
            "pull oci://ghcr.io/zyvorai/charts/kairon --version 0.10.6 --destination /tmp/d"
        );
    }

    #[test]
    fn evaluation_install_args_golden() {
        let a = helm_install_args(
            &t(),
            Path::new("/tmp/d/kairon-0.10.6.tgz"),
            &InstallOpts::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            a.join(" "),
            "--kubeconfig /h/.kube/fluxvm-fluxvm-kube.yaml upgrade --install kairon /tmp/d/kairon-0.10.6.tgz -n kairon-system --create-namespace --timeout 5m"
        );
    }

    #[test]
    fn production_install_args_golden() {
        let mut tt = t();
        tt.context = Some("lab".into());
        let o = InstallOpts {
            production: true,
            ui: true,
            image_cache_dir: Some("/var/lib/kairon-node/image-cache".into()),
            ca_bundle: Some("Q0E=".into()),
        };
        let a = helm_install_args(&tt, Path::new("/c/k.tgz"), &o, Some(Path::new("/c/v.yaml")))
            .unwrap();
        assert_eq!(
            a.join(" "),
            "--kubeconfig /h/.kube/fluxvm-fluxvm-kube.yaml --context lab upgrade --install kairon /c/k.tgz -n kairon-system --create-namespace --timeout 5m -f /c/v.yaml --set webhook.tlsSecretName=kairon-webhook-tls --set webhook.caBundle=Q0E= --set ui.enabled=true --set node.imageCacheDir=/var/lib/kairon-node/image-cache"
        );
    }

    #[test]
    fn production_without_its_inputs_is_refused() {
        let o = InstallOpts {
            production: true,
            ..Default::default()
        };
        assert!(helm_install_args(&t(), Path::new("/c"), &o, None).is_err());
        assert!(helm_install_args(&t(), Path::new("/c"), &o, Some(Path::new("/v"))).is_err());
    }

    #[test]
    fn label_commands_golden() {
        let l = NodeLabels {
            backend: Some("vz".into()),
            chip: Some("m4-max".into()),
            memory_gib: Some(64),
        };
        assert_eq!(
            label_args(&t(), "studio-1", &l).join(" "),
            "--kubeconfig /h/.kube/fluxvm-fluxvm-kube.yaml label node studio-1 kairon.zyvor.dev/capable=true kairon.zyvor.dev/backend.vz=true kairon.zyvor.dev/chip=m4-max kairon.zyvor.dev/memory-gib=64 --overwrite"
        );
        assert_eq!(
            label_pairs(&NodeLabels::default()),
            vec!["kairon.zyvor.dev/capable=true"]
        );
        assert!(check_label_value("--chip", "m4 max").is_err());
        assert!(check_label_value("--chip", "m4-max").is_ok());
    }

    #[test]
    fn webhook_secret_golden_with_fake_certs() {
        let y = webhook_secret_yaml("CERT", "KEY", "CA", "2027-11-22T00:00:00Z");
        assert_eq!(
            y,
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: kairon-webhook-tls\n  namespace: kairon-system\n  annotations:\n    fluxvm.zyvor.dev/not-after: \"2027-11-22T00:00:00Z\"\n  labels:\n    app.kubernetes.io/managed-by: fluxctl\ntype: kubernetes.io/tls\ndata:\n  tls.crt: Q0VSVA==\n  tls.key: S0VZ\n  ca.crt: Q0E=\n"
        );
    }

    #[test]
    fn migration_secret_golden_with_fake_certs() {
        let y = migration_secret_yaml("CERT", "KEY", "CA", "2027-02-20T00:00:00Z");
        assert!(y.contains("name: kairon-migration-tls\n"));
        assert!(y.contains("type: Opaque\n"));
        assert!(y.ends_with("  ca.crt: Q0E=\n  tls.crt: Q0VSVA==\n  tls.key: S0VZ\n"));
    }
}
