// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The certificates the production profile needs, made the way Kairon's `scripts/gen-webhook-tls-certs.sh` and
//! `scripts/gen-migration-mtls-certs.sh` make them (ECDSA P-256, a throwaway CA, the same names and key usages), but in
//! process: no `openssl`, no files on disk, and the private keys go straight into a Secret on stdin and are never printed.

use anyhow::Result;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

pub struct Certs {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
    /// RFC 3339 expiry of the leaf.
    pub not_after: String,
}

fn ca_params(cn: &str) -> Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.distinguished_name.push(DnType::CommonName, cn);
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    Ok(p)
}

/// `2027-11-22T10:11:12Z`: whole seconds, UTC, so that it compares correctly as text.
fn utc_seconds(t: time::OffsetDateTime) -> String {
    chrono::DateTime::from_timestamp(t.unix_timestamp(), 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn issue(
    ca_cn: &str,
    cn: &str,
    sans: Vec<String>,
    ekus: Vec<ExtendedKeyUsagePurpose>,
    days: i64,
) -> Result<Certs> {
    let now = time::OffsetDateTime::now_utc();
    let ca_key = KeyPair::generate()?;
    let mut cp = ca_params(ca_cn)?;
    cp.not_before = now - time::Duration::days(1);
    cp.not_after = now + time::Duration::days(days.max(1) + 1);
    let ca = cp.self_signed(&ca_key)?;
    // The issuer is rebuilt from the same fields, so the leaf's issuer name equals the CA certificate's subject.
    let issuer = Issuer::new(ca_params(ca_cn)?, ca_key);

    let key = KeyPair::generate()?;
    let mut p = CertificateParams::new(sans)?;
    p.distinguished_name.push(DnType::CommonName, cn);
    p.is_ca = IsCa::NoCa;
    p.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    p.extended_key_usages = ekus;
    p.not_before = now - time::Duration::days(1);
    let end = now + time::Duration::days(days.max(1));
    p.not_after = end;
    let cert = p.signed_by(&key, &issuer)?;
    Ok(Certs {
        ca_pem: ca.pem(),
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
        not_after: utc_seconds(end),
    })
}

/// Serving certificate for the validating webhook: server auth only (the API server is the client), names
/// `SERVICE`, `SERVICE.NS` and `SERVICE.NS.svc`.
pub fn webhook(service: &str, namespace: &str, days: i64) -> Result<Certs> {
    let sans = vec![
        service.to_string(),
        format!("{service}.{namespace}"),
        format!("{service}.{namespace}.svc"),
    ];
    issue(
        "kairon-webhook-test-ca",
        &format!("{service}.{namespace}.svc"),
        sans,
        vec![ExtendedKeyUsagePurpose::ServerAuth],
        days,
    )
}

/// Control-plane certificate for the live-migration mTLS peer: client and server auth, SAN `server_name`.
pub fn migration(server_name: &str, days: i64) -> Result<Certs> {
    issue(
        "kairon-migration-ca",
        server_name,
        vec![server_name.to_string()],
        vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ],
        days,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn der(pem: &str) -> Vec<u8> {
        rustls_pemfile::certs(&mut pem.as_bytes())
            .next()
            .unwrap()
            .unwrap()
            .as_ref()
            .to_vec()
    }
    fn contains(hay: &[u8], needle: &str) -> bool {
        hay.windows(needle.len()).any(|w| w == needle.as_bytes())
    }

    #[test]
    fn webhook_cert_names_the_service_three_ways() {
        let c = webhook("kairon-controller-webhook", "kairon-system", 365).unwrap();
        let d = der(&c.cert_pem);
        for n in [
            "kairon-controller-webhook",
            "kairon-controller-webhook.kairon-system",
            "kairon-controller-webhook.kairon-system.svc",
        ] {
            assert!(contains(&d, n), "{n}");
        }
        assert!(c.key_pem.contains("PRIVATE KEY"));
        assert!(c.ca_pem.contains("BEGIN CERTIFICATE"));
        assert_ne!(der(&c.ca_pem), d);
        assert!(
            c.not_after.starts_with("20") && c.not_after.len() == 20 && c.not_after.ends_with('Z')
        );
    }

    #[test]
    fn migration_cert_is_for_kairon_node() {
        let c = migration("kairon-node", 90).unwrap();
        assert!(contains(&der(&c.cert_pem), "kairon-node"));
    }

    #[test]
    fn every_call_makes_new_keys() {
        let a = migration("kairon-node", 90).unwrap();
        let b = migration("kairon-node", 90).unwrap();
        assert_ne!(a.key_pem, b.key_pem);
    }
}
