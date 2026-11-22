// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Pinned upstream manifests for the platform commands. Every manifest is identified by a version, the URL it is published
//! at, and the SHA-256 of the bytes at that URL when the pin was made. The guest downloads it (or receives a copy from the
//! Mac on stdin), checks the digest and only then runs `kubectl apply`; a different digest is a hard error. Nothing is
//! applied from a moving target such as `latest` or `main`.
//!
//! To move a pin: download the new file, run `shasum -a 256` on it, change `version`, `url` and `sha256` together, and
//! re-run the tests. Pins are checked against the upstream release pages by the maintainer; they have not been applied to
//! a live cluster by this change (see docs/apple-k3s-platform.md).

use super::script::K3S_BIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    pub key: &'static str,
    pub version: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// Large or CRD-heavy manifests need server-side apply (the client-side last-applied annotation would exceed 256 KiB).
    pub server_side: bool,
    /// For a manifest whose objects name no namespace: `kubectl apply -n`.
    pub namespace: Option<&'static str>,
}

pub const SUC_CRD: Pin = Pin {
    key: "system-upgrade-controller-crd",
    version: "v0.21.1",
    url: "https://github.com/rancher/system-upgrade-controller/releases/download/v0.21.1/crd.yaml",
    sha256: "1393ba034e7691273fc519b721a7f72a0cdbef8c7b051c9e408be96090b8478d",
    server_side: true,
    namespace: None,
};
pub const SUC: Pin = Pin {
    key: "system-upgrade-controller",
    version: "v0.21.1",
    url: "https://github.com/rancher/system-upgrade-controller/releases/download/v0.21.1/system-upgrade-controller.yaml",
    sha256: "5b46172058b198f8297c9966aacdcd9da2604f740254f0beae74e61b1ad84eac",
    server_side: false,
    namespace: None,
};
pub const FLUX: Pin = Pin {
    key: "flux",
    version: "v2.10.1",
    url: "https://github.com/fluxcd/flux2/releases/download/v2.10.1/install.yaml",
    sha256: "4db2fb9ce179622327d3a9a36a765bbfc0329fd50690e8d37da7dd0918b09392",
    server_side: true,
    namespace: None,
};
pub const ARGOCD: Pin = Pin {
    key: "argocd",
    version: "v3.6.2",
    url: "https://raw.githubusercontent.com/argoproj/argo-cd/v3.6.2/manifests/install.yaml",
    sha256: "c948064d38294d857712ef78444768749a14293613098932f8533e37d067a2a2",
    server_side: true,
    namespace: Some("argocd"),
};
pub const METALLB: Pin = Pin {
    key: "metallb",
    version: "v0.16.2",
    url: "https://raw.githubusercontent.com/metallb/metallb/v0.16.2/config/manifests/metallb-native.yaml",
    sha256: "e5f7c268f114c446dc60aaad71b7cabf861f637743e033e354d7d48592807c6d",
    server_side: true,
    namespace: None,
};
pub const CERT_MANAGER: Pin = Pin {
    key: "cert-manager",
    version: "v1.21.3",
    url: "https://github.com/cert-manager/cert-manager/releases/download/v1.21.3/cert-manager.yaml",
    sha256: "c04a51295adda2b1a775146751f973e787190778c7f35180861d9678876e5c74",
    server_side: true,
    namespace: None,
};
pub const METRICS_SERVER: Pin = Pin {
    key: "metrics-server",
    version: "v0.9.1",
    url: "https://github.com/kubernetes-sigs/metrics-server/releases/download/v0.9.1/components.yaml",
    sha256: "3c7f57cdbee0f57fb0259a150e8f6d6cb4f29d72d1aa5dce8803151dd6d4c2a1",
    server_side: false,
    namespace: None,
};

/// Envoy Gateway's release `install.yaml` (Gateway API CRDs, Envoy Gateway CRDs, the controller in `envoy-gateway-system`).
/// The SHA-256 was computed from a download of this URL and matches the digest GitHub publishes for the release asset.
/// About 4 MiB, so it needs server-side apply.
pub const ENVOY_GATEWAY: Pin = Pin {
    key: "envoy-gateway",
    version: "v1.10.0",
    url: "https://github.com/envoyproxy/gateway/releases/download/v1.10.0/install.yaml",
    sha256: "f87f4d59b983aa760da52a2a0002c612028106fe69beefdf3d30cfb03905944e",
    server_side: true,
    namespace: None,
};

pub const ALL: [Pin; 8] = [
    SUC_CRD,
    SUC,
    FLUX,
    ARGOCD,
    METALLB,
    CERT_MANAGER,
    METRICS_SERVER,
    ENVOY_GATEWAY,
];

/// Where the manifest comes from in [`apply_script`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The guest downloads the pinned URL over HTTPS.
    Download,
    /// The bytes arrive on stdin (a file the user supplied, already checked on the Mac).
    Stdin,
}

pub fn sha256_ok(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A script that gets the manifest, checks it against `sha256` (the pin's unless overridden) and applies it. The digest check
/// comes first and `set -e` stops everything after a mismatch, so nothing unverified reaches `kubectl`.
pub fn apply_script(pin: &Pin, source: Source, sha256: &str) -> String {
    assert!(sha256_ok(sha256));
    let get = match source {
        Source::Download => format!(
            "curl -fsSL --proto '=https' --tlsv1.2 --max-time 300 -o \"$T\" '{}'",
            pin.url
        ),
        Source::Stdin => "cat > \"$T\"".to_owned(),
    };
    let mut flags = if pin.server_side {
        " --server-side --force-conflicts".to_owned()
    } else {
        String::new()
    };
    if let Some(ns) = pin.namespace {
        flags.push_str(&format!(" -n {ns}"));
    }
    // Derived from the verified file, after the digest check: see [`reuses_installed_gateway_api_crds`].
    let filter = if reuses_installed_gateway_api_crds(pin) {
        gateway_api_crd_filter()
    } else {
        String::new()
    };
    format!(
        ": fluxvm-op pin-apply; set -eu; T=$(mktemp); trap 'rm -f \"$T\"' EXIT; {get}; \
         echo '{sha256}  '\"$T\" | sha256sum -c - >&2; {filter}\
         sudo -n {K3S_BIN} kubectl apply{flags} -f \"$T\""
    )
}

/// Envoy Gateway's `install.yaml` carries the Gateway API CRDs (experimental channel). k3s 1.37 installs the standard-channel
/// set itself, with a ValidatingAdmissionPolicy that refuses experimental CRDs on top of them (found on a live cluster), so when
/// the cluster already has them the manifest is applied without its own `*.gateway.networking.k8s.io` CRDs. Envoy's CRDs
/// (`*.gateway.envoyproxy.io`) and the `x-k8s.io` extension CRDs stay.
pub fn reuses_installed_gateway_api_crds(pin: &Pin) -> bool {
    pin.key == "envoy-gateway"
}

fn gateway_api_crd_filter() -> String {
    format!(
        "if sudo -n {K3S_BIN} kubectl get crd gateways.gateway.networking.k8s.io >/dev/null 2>&1; then \
         awk 'BEGIN{{RS=\"\\n---\\n\"; ORS=\"\\n---\\n\"}} !($0 ~ /kind: CustomResourceDefinition/ && $0 ~ /name: [a-z]+\\.gateway\\.networking\\.k8s\\.io/) {{print}}' \"$T\" > \"$T.f\" && mv \"$T.f\" \"$T\"; fi; "
    )
}

pub fn parse_sha256_override(s: &str) -> anyhow::Result<String> {
    if !sha256_ok(s) {
        anyhow::bail!("--sha256 must be 64 hexadecimal characters");
    }
    Ok(s.to_ascii_lowercase())
}

/// Verifies a user-supplied manifest file against a digest on the Mac before it is sent anywhere.
pub fn verify_bytes(data: &[u8], sha256: &str) -> anyhow::Result<()> {
    let got = super::model::sha256_hex(data);
    if got != sha256.to_ascii_lowercase() {
        anyhow::bail!(
            "the manifest's SHA-256 is {got}, not the expected {sha256}; nothing was sent"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envoy_skips_the_gateway_api_crds_k3s_already_has_after_the_digest_check() {
        let sc = apply_script(&ENVOY_GATEWAY, Source::Download, ENVOY_GATEWAY.sha256);
        let (sum, filter) = (sc.find("sha256sum -c").unwrap(), sc.find("awk ").unwrap());
        assert!(
            sum < filter,
            "the digest is checked before the manifest is changed: {sc}"
        );
        assert!(
            sc.contains("get crd gateways.gateway.networking.k8s.io"),
            "{sc}"
        );
        assert!(sc.contains("networking\\.k8s\\.io"), "{sc}");
        let other = apply_script(&METALLB, Source::Download, METALLB.sha256);
        assert!(
            !other.contains("awk "),
            "only Envoy's manifest is filtered: {other}"
        );
    }

    #[test]
    fn pins_are_well_formed_and_unique() {
        let mut keys = std::collections::BTreeSet::new();
        for p in ALL {
            assert!(sha256_ok(p.sha256), "{}", p.key);
            assert!(p.url.starts_with("https://"), "{}", p.key);
            assert!(
                p.url.contains(p.version),
                "{} url must carry the pinned version",
                p.key
            );
            assert!(
                !p.url.contains("latest") && !p.url.contains("/main/"),
                "{}",
                p.key
            );
            assert!(keys.insert(p.key));
        }
    }

    #[test]
    fn download_script_verifies_before_applying() {
        let s = apply_script(&FLUX, Source::Download, FLUX.sha256);
        let verify = s.find("sha256sum -c").unwrap();
        let apply = s.find("kubectl apply").unwrap();
        assert!(s.find("curl").unwrap() < verify && verify < apply);
        assert!(s.contains("--server-side --force-conflicts"));
        assert!(s.starts_with(": fluxvm-op pin-apply;"));
    }

    #[test]
    fn stdin_script_golden() {
        let s = apply_script(&SUC, Source::Stdin, SUC.sha256);
        assert_eq!(
            s,
            ": fluxvm-op pin-apply; set -eu; T=$(mktemp); trap 'rm -f \"$T\"' EXIT; cat > \"$T\"; \
             echo '5b46172058b198f8297c9966aacdcd9da2604f740254f0beae74e61b1ad84eac  '\"$T\" | sha256sum -c - >&2; \
             sudo -n /usr/local/bin/k3s kubectl apply -f \"$T\""
        );
    }

    #[test]
    fn verify_bytes_rejects_a_different_file() {
        let good = super::super::model::sha256_hex(b"abc");
        assert!(verify_bytes(b"abc", &good).is_ok());
        assert!(verify_bytes(b"abd", &good).is_err());
        assert!(parse_sha256_override("zz").is_err());
    }
}
