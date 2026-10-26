//! HTTPS for the service names: a local certificate authority that `fluxctl kube trust` creates and prints the one command
//! to trust, and a resolver that mints a short-lived certificate for each `*.localhost` name on demand. The CA's key never
//! leaves `~/.fluxvm/ca` (mode 0600). Nothing here changes the Mac's trust settings; that is the user's one command.

use crate::docker_host as dh;
use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    crypto::ring as provider,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

const CA_NAME: &str = "FluxVM local CA";

pub fn ca_dir() -> Result<PathBuf> {
    Ok(dh::home()?.join(".fluxvm").join("ca"))
}

/// The CA's certificate and key as PEM.
pub struct Ca {
    pub cert_pem: String,
    pub key_pem: String,
}

/// The CA's certificate fields. The same function builds the one that was signed and the one used to issue leaves, so
/// the leaf's issuer name matches the stored certificate.
fn ca_params() -> Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.distinguished_name.push(DnType::CommonName, CA_NAME);
    p.distinguished_name
        .push(DnType::OrganizationName, "FluxVM");
    p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(p)
}

/// The CA if `kube trust` has made one.
pub fn existing_ca() -> Option<Ca> {
    let dir = ca_dir().ok()?;
    Some(Ca {
        cert_pem: std::fs::read_to_string(dir.join("ca.pem")).ok()?,
        key_pem: std::fs::read_to_string(dir.join("ca-key.pem")).ok()?,
    })
}

/// Loads the CA, or makes it on first use.
pub fn ensure_ca() -> Result<Ca> {
    if let Some(ca) = existing_ca() {
        return Ok(ca);
    }
    let dir = ca_dir()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let key = KeyPair::generate()?;
    let mut params = ca_params()?;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(3650);
    let cert = params.self_signed(&key)?;
    let ca = Ca {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    };
    write_file(&dir.join("ca-key.pem"), &ca.key_pem, 0o600)?;
    write_file(&dir.join("ca.pem"), &ca.cert_pem, 0o644)?;
    Ok(ca)
}

fn write_file(path: &std::path::Path, text: &str, mode: u32) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// SHA-256 of the CA certificate, as `AB:CD:…`, to compare with what Keychain Access shows.
pub fn fingerprint(cert_pem: &str) -> Option<String> {
    let der = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .next()?
        .ok()?;
    let digest = Sha256::digest(der.as_ref());
    Some(
        digest
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// The command that makes this Mac trust the CA (Safari, Chrome, curl and most tools; Firefox keeps its own store).
pub fn trust_command(ca_path: &std::path::Path) -> String {
    format!(
        "sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain {}",
        ca_path.display()
    )
}

/// Only these names get a certificate; the resolver never signs for an arbitrary host a client asks for.
pub fn name_allowed(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    (h == "localhost" || h.ends_with(".localhost"))
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        })
}

/// A certificate for exactly `host`, signed by the CA and valid for a year. One per name, because a wildcard does not
/// span the two labels of `web.shop.localhost`.
pub fn leaf(ca: &Ca, host: &str) -> Result<CertifiedKey> {
    let ca_key = KeyPair::from_pem(&ca.key_pem)?;
    let issuer = Issuer::new(ca_params()?, ca_key);
    let key = KeyPair::generate()?;
    let mut p = CertificateParams::new(vec![host.to_owned()])?;
    p.distinguished_name.push(DnType::CommonName, host);
    p.is_ca = IsCa::NoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let now = time::OffsetDateTime::now_utc();
    p.not_before = now - time::Duration::days(1);
    p.not_after = now + time::Duration::days(365);
    let cert = p.signed_by(&key, &issuer)?;
    let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let signer = provider::sign::any_supported_type(&der)?;
    Ok(CertifiedKey::new(
        vec![CertificateDer::from(cert.der().to_vec())],
        signer,
    ))
}

/// Mints and remembers a certificate for each allowed name a client asks for in the TLS hello.
#[derive(Debug)]
pub struct Resolver {
    ca: Ca,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ca(..)") // never print the key
    }
}

impl Resolver {
    pub fn new(ca: Ca) -> Self {
        Self {
            ca,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = hello.server_name()?.to_ascii_lowercase();
        if !name_allowed(&host) {
            return None;
        }
        if let Some(k) = self.cache.lock().ok()?.get(&host) {
            return Some(k.clone());
        }
        let key = Arc::new(leaf(&self.ca, &host).ok()?);
        self.cache.lock().ok()?.insert(host, key.clone());
        Some(key)
    }
}

/// The TLS acceptor for the names listener, or `None` when `kube trust` has not made a CA.
pub fn acceptor() -> Option<tokio_rustls::TlsAcceptor> {
    let ca = existing_ca()?;
    let mut config =
        rustls::ServerConfig::builder_with_provider(Arc::new(provider::default_provider()))
            .with_safe_default_protocol_versions()
            .ok()?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver::new(ca)));
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Some(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// `fluxctl kube trust`: make the CA and say how to trust it.
pub fn trust(install: bool) -> Result<String> {
    let ca = ensure_ca()?;
    let path = ca_dir()?.join("ca.pem");
    let cmd = trust_command(&path);
    let fp = fingerprint(&ca.cert_pem).unwrap_or_default();
    if install {
        let status = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .status()
            .context("running the security command")?;
        if !status.success() {
            anyhow::bail!("the security command failed ({status})");
        }
        return Ok(format!(
            "trusted {} ({fp}). Restart `fluxctl kube supervise` so it serves https://<service>.localhost",
            path.display()
        ));
    }
    Ok(format!(
        "Local CA: {}\nSHA-256: {fp}\n\nTo trust it (asks for your password; changes this Mac's trust settings):\n  {cmd}\n\nOr run `fluxctl kube trust --install`. Then restart `fluxctl kube supervise`; services answer at\nhttps://<service>.localhost. Remove trust later with:\n  sudo security remove-trusted-cert -d {}",
        path.display(),
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_localhost_names_get_a_certificate() {
        for ok in [
            "web.localhost",
            "web.shop.localhost",
            "localhost",
            "A.Localhost",
        ] {
            assert!(name_allowed(ok), "{ok}");
        }
        for bad in [
            "example.com",
            "evil.localhost.com",
            "a..localhost",
            "-x.localhost",
            "a b.localhost",
            "",
            "web.localhost.",
        ] {
            assert!(!name_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn the_trust_command_names_the_file_and_the_system_keychain() {
        let c = trust_command(std::path::Path::new("/Users/me/.fluxvm/ca/ca.pem"));
        assert!(c.starts_with("sudo security add-trusted-cert -d -r trustRoot"));
        assert!(c.contains("/Library/Keychains/System.keychain"));
        assert!(c.ends_with("/Users/me/.fluxvm/ca/ca.pem"));
    }

    #[test]
    fn a_leaf_chains_to_the_ca_and_names_its_host() {
        let key = KeyPair::generate().unwrap();
        let cert = ca_params().unwrap().self_signed(&key).unwrap();
        let ca = Ca {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        };
        let fp = fingerprint(&ca.cert_pem).unwrap();
        assert_eq!(fp.len(), 32 * 3 - 1);
        let certified = leaf(&ca, "web.shop.localhost").unwrap();
        assert_eq!(certified.cert.len(), 1);
        assert!(
            format!("{ca:?}").contains("Ca(..)"),
            "the key is never printed"
        );
    }
}
