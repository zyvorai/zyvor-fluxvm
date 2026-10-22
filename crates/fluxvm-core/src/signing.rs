// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Generic Ed25519 sign/verify over raw bytes, verified against the operator's
//! configured `trusted_signers`. Shared by the image catalog and signed host
//! policy bundles (exec allow-lists, guard rule packs).
//!
//! The signer's *name* is derived from which configured key verified the
//! signature; it is never read from the signed payload.

use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rand_core::UnwrapErr;
use rand::rngs::SysRng;

use crate::config::TrustedSigner;

/// A trusted signer's label paired with its parsed public key.
pub type ParsedSigner = (String, VerifyingKey);

pub fn parse_public_key(b64: &str) -> Result<VerifyingKey> {
    let bytes = B64
        .decode(b64)
        .context("trusted_signers entry is not valid base64")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("public key must be 32 bytes, got {}", v.len()))?;
    VerifyingKey::from_bytes(&arr).context("invalid Ed25519 public key")
}

pub fn parse_trusted_signers(signers: &[TrustedSigner]) -> Result<Vec<ParsedSigner>> {
    signers
        .iter()
        .map(|s| Ok((s.name.clone(), parse_public_key(&s.public_key)?)))
        .collect::<Result<_>>()
        .context("parsing trusted_signers")
}

/// A fresh keypair, both halves base64-encoded: `(private, public)`.
pub fn generate_keypair() -> (String, String) {
    let signing_key = SigningKey::generate(&mut UnwrapErr(SysRng));
    (
        B64.encode(signing_key.to_bytes()),
        B64.encode(signing_key.verifying_key().to_bytes()),
    )
}

/// Signs `payload` with a base64 private key; returns the base64 signature.
pub fn sign_bytes(private_key_b64: &str, payload: &[u8]) -> Result<String> {
    let key_bytes = B64
        .decode(private_key_b64)
        .context("private key is not valid base64")?;
    let key_arr: [u8; 32] = key_bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("private key must be 32 bytes, got {}", v.len()))?;
    let sig = SigningKey::from_bytes(&key_arr).sign(payload);
    Ok(B64.encode(sig.to_bytes()))
}

/// Verifies `signature_b64` over `payload` against every trusted signer and
/// returns the name of the one that matched.
pub fn verify_bytes<'a>(
    payload: &[u8],
    signature_b64: &str,
    trusted: &'a [ParsedSigner],
) -> Result<&'a str> {
    let sig_bytes = B64
        .decode(signature_b64)
        .context("signature is not valid base64")?;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("signature must be 64 bytes, got {}", v.len()))?;
    let sig = Signature::from_bytes(&sig_arr);
    trusted
        .iter()
        .find(|(_, key)| key.verify(payload, &sig).is_ok())
        .map(|(name, _)| name.as_str())
        .context("signature does not match any configured trusted_signers")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer(name: &str, public_key: String) -> TrustedSigner {
        TrustedSigner {
            name: name.into(),
            public_key,
        }
    }

    #[test]
    fn sign_then_verify_reports_the_matching_signer() {
        let (_, other_pub) = generate_keypair();
        let (private, public) = generate_keypair();
        let trusted =
            parse_trusted_signers(&[signer("other", other_pub), signer("ci", public)]).unwrap();
        let sig = sign_bytes(&private, b"payload").unwrap();
        assert_eq!(verify_bytes(b"payload", &sig, &trusted).unwrap(), "ci");
    }

    #[test]
    fn verify_rejects_tampered_payload_and_wrong_signer() {
        let (private, _) = generate_keypair();
        let (_, other_pub) = generate_keypair();
        let trusted = parse_trusted_signers(&[signer("other", other_pub)]).unwrap();
        let sig = sign_bytes(&private, b"payload").unwrap();
        assert!(verify_bytes(b"payload", &sig, &trusted).is_err());
        assert!(verify_bytes(b"tampered", &sig, &trusted).is_err());
    }

    #[test]
    fn malformed_inputs_are_errors_not_panics() {
        assert!(parse_public_key("!!!").is_err());
        assert!(parse_public_key("AAAA").is_err());
        assert!(sign_bytes("AAAA", b"x").is_err());
        assert!(verify_bytes(b"x", "AAAA", &[]).is_err());
    }
}
