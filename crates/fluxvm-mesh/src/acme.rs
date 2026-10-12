// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Minimal native ACME v2 client (RFC 8555) for FluxMesh HTTPS ingress.
//!
//! The implementation intentionally supports the HTTP-01 path first. Router
//! automation in this crate can expose port 80, while the challenge listener
//! never serves application traffic.

use crate::dns01;
use crate::ingress::ChallengeStore;
use crate::public::Dns01Config;
use crate::{MeshError, Result};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use ring::{
    digest,
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as RingKeyPair},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize)]
struct Directory {
    #[serde(rename = "newNonce")]
    new_nonce: String,
    #[serde(rename = "newAccount")]
    new_account: String,
    #[serde(rename = "newOrder")]
    new_order: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Order {
    status: String,
    authorizations: Vec<String>,
    finalize: String,
    certificate: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct Authorization {
    status: String,
    identifier: Identifier,
    challenges: Vec<Challenge>,
}

#[derive(Clone, Debug, Deserialize)]
struct Identifier {
    value: String,
}

#[derive(Clone, Debug, Deserialize)]
struct Challenge {
    #[serde(rename = "type")]
    kind: String,
    url: String,
    token: String,
    status: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateBundle {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub domains: Vec<String>,
}

pub struct AcmeClient {
    http: reqwest::Client,
    directory: Directory,
    account_key: Vec<u8>,
    nonce: Option<String>,
    kid: Option<String>,
}

impl AcmeClient {
    pub async fn connect(directory_url: &str, account_key_path: &Path) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("Zyvor-FluxMesh/0.4.1")
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(http_err)?;
        let directory: Directory = http
            .get(directory_url)
            .send()
            .await
            .map_err(http_err)?
            .error_for_status()
            .map_err(http_err)?
            .json()
            .await
            .map_err(http_err)?;
        let account_key = load_or_create_account_key(account_key_path)?;
        Ok(Self {
            http,
            directory,
            account_key,
            nonce: None,
            kid: None,
        })
    }

    pub async fn ensure_account(&mut self, email: &str) -> Result<String> {
        let contact = if email.trim().is_empty() {
            Vec::<String>::new()
        } else {
            vec![format!("mailto:{email}")]
        };
        let body = json!({"termsOfServiceAgreed": true, "contact": contact});
        let url = self.directory.new_account.clone();
        let response = self.jws_post(&url, Some(&body), true).await?;
        if !(200..300).contains(&response.status) {
            return Err(protocol_error("new-account", &response));
        }
        let kid = response.location.ok_or_else(|| {
            MeshError::InvalidState("ACME account response had no Location header".into())
        })?;
        self.kid = Some(kid.clone());
        Ok(kid)
    }

    pub async fn obtain_http01(
        &mut self,
        email: &str,
        domains: &[String],
        challenge_store: &ChallengeStore,
        cert_path: &Path,
        key_path: &Path,
    ) -> Result<CertificateBundle> {
        validate_domains(domains)?;
        self.ensure_account(email).await?;
        let identifiers = domains
            .iter()
            .map(|name| json!({"type":"dns","value":name}))
            .collect::<Vec<_>>();
        let order_url = self.directory.new_order.clone();
        let created = self
            .jws_post(
                &order_url,
                Some(&json!({"identifiers": identifiers})),
                false,
            )
            .await?;
        if !(200..300).contains(&created.status) {
            return Err(protocol_error("new-order", &created));
        }
        let order_location = created
            .location
            .clone()
            .ok_or_else(|| MeshError::InvalidState("ACME order had no Location header".into()))?;
        let order: Order = serde_json::from_str(&created.body).map_err(json_err)?;

        for auth_url in &order.authorizations {
            self.fulfill_http01(auth_url, challenge_store).await?;
        }

        let cert_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map_err(|e| MeshError::InvalidState(format!("generate certificate key: {e}")))?;
        let params = CertificateParams::new(domains.to_vec())
            .map_err(|e| MeshError::InvalidState(format!("build CSR params: {e}")))?;
        let csr = params
            .serialize_request(&cert_key)
            .map_err(|e| MeshError::InvalidState(format!("serialize CSR: {e}")))?;
        let finalize = self
            .jws_post(
                &order.finalize,
                Some(&json!({"csr": b64url(csr.der().as_ref())})),
                false,
            )
            .await?;
        if !(200..300).contains(&finalize.status) {
            return Err(protocol_error("finalize", &finalize));
        }

        let certificate_url = self.poll_order(&order_location).await?;
        let cert_response = self.jws_post(&certificate_url, None, false).await?;
        if !(200..300).contains(&cert_response.status) {
            return Err(protocol_error("certificate", &cert_response));
        }
        if !cert_response.body.contains("BEGIN CERTIFICATE") {
            return Err(MeshError::InvalidState(
                "ACME certificate response was not PEM".into(),
            ));
        }
        atomic_write(cert_path, cert_response.body.as_bytes(), false)?;
        atomic_write(key_path, cert_key.serialize_pem().as_bytes(), true)?;
        Ok(CertificateBundle {
            cert_path: cert_path.to_owned(),
            key_path: key_path.to_owned(),
            domains: domains.to_vec(),
        })
    }

    pub async fn obtain_dns01(
        &mut self,
        email: &str,
        domains: &[String],
        dns: &Dns01Config,
        cert_path: &Path,
        key_path: &Path,
    ) -> Result<CertificateBundle> {
        validate_domains_dns01(domains)?;
        if dns.require_dnssec_ad && dns.validation_resolver.is_none() {
            return Err(MeshError::InvalidState(
                "DNS-01 DNSSEC AD verification requires validation_resolver".into(),
            ));
        }
        self.ensure_account(email).await?;
        let identifiers = domains
            .iter()
            .map(|name| json!({"type":"dns","value":name}))
            .collect::<Vec<_>>();
        let order_url = self.directory.new_order.clone();
        let created = self
            .jws_post(
                &order_url,
                Some(&json!({"identifiers": identifiers})),
                false,
            )
            .await?;
        if !(200..300).contains(&created.status) {
            return Err(protocol_error("new-order", &created));
        }
        let order_location = created
            .location
            .clone()
            .ok_or_else(|| MeshError::InvalidState("ACME order had no Location header".into()))?;
        let order: Order = serde_json::from_str(&created.body).map_err(json_err)?;
        for auth_url in &order.authorizations {
            self.fulfill_dns01(auth_url, dns).await?;
        }

        let cert_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map_err(|e| MeshError::InvalidState(format!("generate certificate key: {e}")))?;
        let params = CertificateParams::new(domains.to_vec())
            .map_err(|e| MeshError::InvalidState(format!("build CSR params: {e}")))?;
        let csr = params
            .serialize_request(&cert_key)
            .map_err(|e| MeshError::InvalidState(format!("serialize CSR: {e}")))?;
        let finalize = self
            .jws_post(
                &order.finalize,
                Some(&json!({"csr": b64url(csr.der().as_ref())})),
                false,
            )
            .await?;
        if !(200..300).contains(&finalize.status) {
            return Err(protocol_error("finalize", &finalize));
        }
        let certificate_url = self.poll_order(&order_location).await?;
        let cert_response = self.jws_post(&certificate_url, None, false).await?;
        if !(200..300).contains(&cert_response.status) {
            return Err(protocol_error("certificate", &cert_response));
        }
        if !cert_response.body.contains("BEGIN CERTIFICATE") {
            return Err(MeshError::InvalidState(
                "ACME certificate response was not PEM".into(),
            ));
        }
        atomic_write(cert_path, cert_response.body.as_bytes(), false)?;
        atomic_write(key_path, cert_key.serialize_pem().as_bytes(), true)?;
        Ok(CertificateBundle {
            cert_path: cert_path.to_owned(),
            key_path: key_path.to_owned(),
            domains: domains.to_vec(),
        })
    }

    async fn fulfill_dns01(&mut self, auth_url: &str, dns_cfg: &Dns01Config) -> Result<()> {
        let current = self.jws_post(auth_url, None, false).await?;
        if !(200..300).contains(&current.status) {
            return Err(protocol_error("authorization", &current));
        }
        let auth: Authorization = serde_json::from_str(&current.body).map_err(json_err)?;
        if auth.status == "valid" {
            return Ok(());
        }
        let challenge = auth
            .challenges
            .iter()
            .find(|c| c.kind == "dns-01")
            .ok_or_else(|| {
                MeshError::InvalidState(format!(
                    "ACME authorization for {} has no dns-01 challenge",
                    auth.identifier.value
                ))
            })?;
        if challenge.status.as_deref() == Some("invalid") {
            return Err(MeshError::InvalidState(format!(
                "ACME dns-01 challenge for {} is already invalid",
                auth.identifier.value
            )));
        }
        let key_auth = format!("{}.{}", challenge.token, self.jwk_thumbprint()?);
        let txt = b64url(digest::digest(&digest::SHA256, key_auth.as_bytes()).as_ref());
        let base = auth.identifier.value.trim_start_matches("*.");
        let owner = format!("_acme-challenge.{base}");
        let lease = dns01::present(dns_cfg, &owner, &txt).await?;
        dns01::wait_for_propagation(dns_cfg).await;
        if let Some(resolver) = dns_cfg.validation_resolver {
            if let Err(e) = crate::dnssec::verify_txt(
                resolver,
                &owner,
                &txt,
                dns_cfg.require_dnssec_ad,
                Duration::from_secs(5),
            )
            .await
            {
                let _ = dns01::cleanup(dns_cfg, &lease, &txt).await;
                return Err(e);
            }
        }
        let trigger = self
            .jws_post(&challenge.url, Some(&json!({})), false)
            .await?;
        if !(200..300).contains(&trigger.status) {
            let _ = dns01::cleanup(dns_cfg, &lease, &txt).await;
            return Err(protocol_error("challenge-trigger", &trigger));
        }
        let mut valid = false;
        for _ in 0..45 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let checked = self.jws_post(auth_url, None, false).await?;
            let state: Authorization = serde_json::from_str(&checked.body).map_err(json_err)?;
            match state.status.as_str() {
                "valid" => {
                    valid = true;
                    break;
                }
                "invalid" => break,
                _ => {}
            }
        }
        let _ = dns01::cleanup(dns_cfg, &lease, &txt).await;
        if !valid {
            return Err(MeshError::InvalidState(format!(
                "ACME DNS authorization for {} did not become valid",
                auth.identifier.value
            )));
        }
        Ok(())
    }

    async fn fulfill_http01(&mut self, auth_url: &str, store: &ChallengeStore) -> Result<()> {
        let current = self.jws_post(auth_url, None, false).await?;
        if !(200..300).contains(&current.status) {
            return Err(protocol_error("authorization", &current));
        }
        let auth: Authorization = serde_json::from_str(&current.body).map_err(json_err)?;
        if auth.status == "valid" {
            return Ok(());
        }
        let challenge = auth
            .challenges
            .iter()
            .find(|c| c.kind == "http-01")
            .ok_or_else(|| {
                MeshError::InvalidState(format!(
                    "ACME authorization for {} has no http-01 challenge",
                    auth.identifier.value
                ))
            })?;
        if challenge.status.as_deref() == Some("invalid") {
            return Err(MeshError::InvalidState(format!(
                "ACME http-01 challenge for {} is already invalid",
                auth.identifier.value
            )));
        }
        let key_auth = format!("{}.{}", challenge.token, self.jwk_thumbprint()?);
        store.put(&challenge.token, &key_auth)?;
        let trigger = self
            .jws_post(&challenge.url, Some(&json!({})), false)
            .await?;
        if !(200..300).contains(&trigger.status) {
            let _ = store.remove(&challenge.token);
            return Err(protocol_error("challenge-trigger", &trigger));
        }
        let mut valid = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let checked = self.jws_post(auth_url, None, false).await?;
            let state: Authorization = serde_json::from_str(&checked.body).map_err(json_err)?;
            match state.status.as_str() {
                "valid" => {
                    valid = true;
                    break;
                }
                "invalid" => break,
                _ => {}
            }
        }
        let _ = store.remove(&challenge.token);
        if !valid {
            return Err(MeshError::InvalidState(format!(
                "ACME authorization for {} did not become valid",
                auth.identifier.value
            )));
        }
        Ok(())
    }

    async fn poll_order(&mut self, order_url: &str) -> Result<String> {
        for _ in 0..30 {
            let response = self.jws_post(order_url, None, false).await?;
            if !(200..300).contains(&response.status) {
                return Err(protocol_error("order-poll", &response));
            }
            let order: Order = serde_json::from_str(&response.body).map_err(json_err)?;
            match order.status.as_str() {
                "valid" => {
                    return order.certificate.ok_or_else(|| {
                        MeshError::InvalidState("valid ACME order had no certificate URL".into())
                    });
                }
                "invalid" => {
                    return Err(MeshError::InvalidState("ACME order became invalid".into()));
                }
                _ => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        Err(MeshError::InvalidState("ACME order timed out".into()))
    }

    async fn jws_post(
        &mut self,
        url: &str,
        payload: Option<&Value>,
        use_jwk: bool,
    ) -> Result<AcmeResponse> {
        if self.nonce.is_none() {
            let response = self
                .http
                .head(&self.directory.new_nonce)
                .send()
                .await
                .map_err(http_err)?;
            self.nonce = replay_nonce(response.headers())?;
        }
        let nonce = self
            .nonce
            .take()
            .ok_or_else(|| MeshError::InvalidState("ACME nonce unavailable".into()))?;
        let protected = if use_jwk {
            json!({"alg":"ES256","nonce":nonce,"url":url,"jwk":self.jwk()?})
        } else {
            json!({"alg":"ES256","nonce":nonce,"url":url,"kid":self.kid.as_ref().ok_or_else(|| MeshError::InvalidState("ACME account is not initialized".into()))?})
        };
        let protected_b64 = b64url(serde_json::to_vec(&protected).map_err(json_err)?.as_slice());
        let payload_b64 = match payload {
            Some(value) => b64url(serde_json::to_vec(value).map_err(json_err)?.as_slice()),
            None => String::new(), // POST-as-GET per RFC 8555 section 6.3
        };
        let signing_input = format!("{protected_b64}.{payload_b64}");
        let signature = self.sign(signing_input.as_bytes())?;
        let body =
            json!({"protected":protected_b64,"payload":payload_b64,"signature":b64url(&signature)});
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/jose+json")
            .json(&body)
            .send()
            .await
            .map_err(http_err)?;
        self.nonce = replay_nonce(response.headers())?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await.map_err(http_err)?;
        Ok(AcmeResponse {
            status,
            location,
            body,
        })
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        let rng = SystemRandom::new();
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &self.account_key, &rng)
                .map_err(|_| MeshError::InvalidState("parse ACME account key".into()))?;
        pair.sign(&rng, message)
            .map(|v| v.as_ref().to_vec())
            .map_err(|_| MeshError::InvalidState("sign ACME JWS".into()))
    }

    fn jwk(&self) -> Result<Value> {
        let rng = SystemRandom::new();
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &self.account_key, &rng)
                .map_err(|_| MeshError::InvalidState("parse ACME account key".into()))?;
        let public = pair.public_key().as_ref();
        if public.len() != 65 || public[0] != 4 {
            return Err(MeshError::InvalidState(
                "unexpected P-256 public key encoding".into(),
            ));
        }
        Ok(json!({"crv":"P-256","kty":"EC","x":b64url(&public[1..33]),"y":b64url(&public[33..65])}))
    }

    fn jwk_thumbprint(&self) -> Result<String> {
        let jwk = self.jwk()?;
        // RFC 7638 canonical member order for this EC key.
        let canonical = format!(
            "{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{}\",\"y\":\"{}\"}}",
            jwk["x"].as_str().unwrap_or(""),
            jwk["y"].as_str().unwrap_or("")
        );
        Ok(b64url(
            digest::digest(&digest::SHA256, canonical.as_bytes()).as_ref(),
        ))
    }
}

struct AcmeResponse {
    status: u16,
    location: Option<String>,
    body: String,
}

fn load_or_create_account_key(path: &Path) -> Result<Vec<u8>> {
    if path.exists() {
        return Ok(fs::read(path)?);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
        .map_err(|_| MeshError::InvalidState("generate ACME account key".into()))?;
    atomic_write(path, pkcs8.as_ref(), true)?;
    Ok(pkcs8.as_ref().to_vec())
}

fn replay_nonce(headers: &reqwest::header::HeaderMap) -> Result<Option<String>> {
    Ok(headers
        .get("replay-nonce")
        .map(|v| v.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| MeshError::InvalidState("invalid Replay-Nonce header".into()))?)
}

fn validate_domains(domains: &[String]) -> Result<()> {
    if domains.is_empty() {
        return Err(MeshError::InvalidState(
            "ACME requires at least one domain".into(),
        ));
    }
    for domain in domains {
        if domain.is_empty()
            || domain.len() > 253
            || domain.contains('*')
            || domain.contains('/')
            || domain.contains(':')
            || domain.chars().any(char::is_whitespace)
        {
            return Err(MeshError::InvalidState(format!(
                "invalid ACME domain {domain:?}"
            )));
        }
    }
    Ok(())
}

fn validate_domains_dns01(domains: &[String]) -> Result<()> {
    if domains.is_empty() {
        return Err(MeshError::InvalidState(
            "ACME requires at least one domain".into(),
        ));
    }
    for domain in domains {
        let bare = domain.strip_prefix("*.").unwrap_or(domain);
        if bare.is_empty()
            || bare.len() > 253
            || bare.contains('*')
            || bare.contains('/')
            || bare.contains(':')
            || bare.chars().any(char::is_whitespace)
        {
            return Err(MeshError::InvalidState(format!(
                "invalid DNS-01 ACME domain {domain:?}"
            )));
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], private: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if private { 0o600 } else { 0o644 };
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(tmp, path)?;
    Ok(())
}

fn protocol_error(action: &str, response: &AcmeResponse) -> MeshError {
    let detail = response.body.chars().take(2048).collect::<String>();
    MeshError::InvalidState(format!(
        "ACME {action} returned HTTP {}: {detail}",
        response.status
    ))
}
fn http_err(e: reqwest::Error) -> MeshError {
    MeshError::InvalidState(format!("ACME HTTP: {e}"))
}
fn json_err(e: impl std::fmt::Display) -> MeshError {
    MeshError::InvalidState(format!("ACME JSON: {e}"))
}

pub fn b64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n =
            (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8) | u32::from(bytes[i + 2]);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(TABLE[((n >> 6) & 63) as usize] as char);
        out.push(TABLE[(n & 63) as usize] as char);
        i += 3;
    }
    match bytes.len() - i {
        1 => {
            let n = u32::from(bytes[i]) << 16;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
        }
        2 => {
            let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8);
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        }
        _ => {}
    }
    out
}

pub fn renewal_due(marker: &Path, max_age: Duration) -> bool {
    let Ok(raw) = fs::read_to_string(marker) else {
        return true;
    };
    let Ok(issued) = raw.trim().parse::<u64>() else {
        return true;
    };
    now_secs().saturating_sub(issued) >= max_age.as_secs()
}

pub fn mark_issued(marker: &Path) -> Result<()> {
    atomic_write(marker, now_secs().to_string().as_bytes(), true)
}

pub fn default_issued_marker_path() -> PathBuf {
    crate::mesh_dir().join("acme-issued-at")
}
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn default_account_key_path() -> PathBuf {
    crate::mesh_dir().join("acme-account.pk8")
}
pub fn default_certificate_path() -> PathBuf {
    crate::mesh_dir().join("tls/fullchain.pem")
}
pub fn default_certificate_key_path() -> PathBuf {
    crate::mesh_dir().join("tls/key.pem")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64url_matches_rfc_examples_without_padding() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert!(!b64url(b"hello world").contains('='));
    }
    #[test]
    fn domain_validation_rejects_urls() {
        assert!(validate_domains(&["https://bad".into()]).is_err());
        assert!(validate_domains(&["*.cloud.example".into()]).is_err());
        assert!(validate_domains(&["cloud.example".into()]).is_ok());
        assert!(validate_domains_dns01(&["*.cloud.example".into()]).is_ok());
    }
}
