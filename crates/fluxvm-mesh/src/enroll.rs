// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Zero-touch enrollment tokens for personal-cloud nodes.
//!
//! Tokens are short-lived HMAC-SHA256 capabilities. They carry only join
//! intent (node id, hostname, public key, expiry and nonce) and never a mesh
//! private key. The authority secret remains on the enrollment controller.

use crate::{MeshError, MeshState, Result};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const TOKEN_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinClaims {
    pub node_id: String,
    pub hostname: String,
    pub public_key: String,
    pub expires_ms: u64,
    pub nonce: String,
}

#[derive(Clone, Debug)]
pub struct EnrollmentAuthority {
    secret: Vec<u8>,
}

impl EnrollmentAuthority {
    pub fn from_secret(secret: Vec<u8>) -> Result<Self> {
        if secret.len() < 32 {
            return Err(MeshError::InvalidState(
                "enrollment secret must be at least 32 bytes".into(),
            ));
        }
        Ok(Self { secret })
    }

    pub fn load_or_create(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(secret) => Self::from_secret(secret),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut secret = vec![0u8; 32];
                SystemRandom::new()
                    .fill(&mut secret)
                    .map_err(|_| MeshError::InvalidState("secure RNG unavailable".into()))?;
                let tmp = path.with_extension("tmp");
                fs::write(&tmp, &secret)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
                }
                fs::rename(tmp, path)?;
                Self::from_secret(secret)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn issue(
        &self,
        node_id: impl Into<String>,
        hostname: impl Into<String>,
        public_key: impl Into<String>,
        ttl: Duration,
    ) -> Result<String> {
        let claims = JoinClaims {
            node_id: node_id.into(),
            hostname: hostname.into(),
            public_key: public_key.into(),
            expires_ms: now_ms().saturating_add(ttl.as_millis().min(u64::MAX as u128) as u64),
            nonce: random_hex(16)?,
        };
        validate_claims(&claims)?;
        let payload = encode_claims(&claims);
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.secret);
        let tag = hmac::sign(&key, payload.as_bytes());
        Ok(format!(
            "v{TOKEN_VERSION}.{}.{}",
            hex_encode(payload.as_bytes()),
            hex_encode(tag.as_ref())
        ))
    }

    pub fn verify(&self, token: &str, now_ms: u64) -> Result<JoinClaims> {
        let mut parts = token.split('.');
        let version = parts.next().unwrap_or_default();
        let payload_hex = parts.next().unwrap_or_default();
        let tag_hex = parts.next().unwrap_or_default();
        if parts.next().is_some() || version != format!("v{TOKEN_VERSION}") {
            return Err(MeshError::InvalidState(
                "invalid enrollment token format".into(),
            ));
        }
        let payload = hex_decode(payload_hex)?;
        let tag = hex_decode(tag_hex)?;
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.secret);
        hmac::verify(&key, &payload, &tag)
            .map_err(|_| MeshError::InvalidState("invalid enrollment token signature".into()))?;
        let payload = String::from_utf8(payload)
            .map_err(|_| MeshError::InvalidState("enrollment payload is not UTF-8".into()))?;
        let claims = decode_claims(&payload)?;
        validate_claims(&claims)?;
        if claims.expires_ms < now_ms {
            return Err(MeshError::InvalidState("enrollment token expired".into()));
        }
        Ok(claims)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsedNonceStore {
    path: PathBuf,
}

impl UsedNonceStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Atomically consume a nonce. Reuse is rejected even if the HMAC token is
    /// otherwise still valid.
    pub fn consume(&self, nonce: &str) -> Result<()> {
        if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(MeshError::InvalidState("invalid enrollment nonce".into()));
        }
        fs::create_dir_all(&self.path)?;
        let marker = self.path.join(nonce);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&marker) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(
                MeshError::InvalidState("enrollment token already consumed".into()),
            ),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn apply_join(mesh: &mut MeshState, claims: &JoinClaims) -> Result<std::net::Ipv4Addr> {
    mesh.add_node(
        claims.node_id.clone(),
        claims.hostname.clone(),
        claims.public_key.clone(),
        false,
    )
}

pub fn default_secret_path() -> PathBuf {
    std::env::var_os("FLUXMESH_ENROLL_SECRET")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::mesh_dir().join("enroll.hmac"))
}

pub fn default_nonce_path() -> PathBuf {
    std::env::var_os("FLUXMESH_ENROLL_NONCES")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::mesh_dir().join("enroll-used"))
}

fn validate_claims(c: &JoinClaims) -> Result<()> {
    if c.node_id.is_empty() || c.node_id.len() > 63 {
        return Err(MeshError::InvalidNodeId(c.node_id.clone()));
    }
    if c.hostname.is_empty()
        || c.hostname.len() > 255
        || c.public_key.is_empty()
        || c.nonce.len() != 32
    {
        return Err(MeshError::InvalidState("invalid enrollment claims".into()));
    }
    Ok(())
}

fn encode_claims(c: &JoinClaims) -> String {
    [
        c.node_id.as_str(),
        c.hostname.as_str(),
        c.public_key.as_str(),
        &c.expires_ms.to_string(),
        c.nonce.as_str(),
    ]
    .iter()
    .map(|v| hex_encode(v.as_bytes()))
    .collect::<Vec<_>>()
    .join(":")
}

fn decode_claims(payload: &str) -> Result<JoinClaims> {
    let fields = payload.split(':').collect::<Vec<_>>();
    if fields.len() != 5 {
        return Err(MeshError::InvalidState(
            "invalid enrollment claims payload".into(),
        ));
    }
    let string = |v: &str| -> Result<String> {
        String::from_utf8(hex_decode(v)?)
            .map_err(|_| MeshError::InvalidState("invalid enrollment string".into()))
    };
    Ok(JoinClaims {
        node_id: string(fields[0])?,
        hostname: string(fields[1])?,
        public_key: string(fields[2])?,
        expires_ms: string(fields[3])?
            .parse()
            .map_err(|_| MeshError::InvalidState("invalid enrollment expiry".into()))?,
        nonce: string(fields[4])?,
    })
}

fn random_hex(len: usize) -> Result<String> {
    let mut bytes = vec![0u8; len];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| MeshError::InvalidState("secure RNG unavailable".into()))?;
    Ok(hex_encode(&bytes))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn hex_encode(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(H[(b >> 4) as usize] as char);
        out.push(H[(b & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(value: &str) -> Result<Vec<u8>> {
    if value.len() % 2 != 0 {
        return Err(MeshError::InvalidState("invalid hex".into()));
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(MeshError::InvalidState("invalid hex".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEFAULT_POOL;

    #[test]
    fn join_token_roundtrip_and_tamper_rejection() {
        let authority = EnrollmentAuthority::from_secret(vec![7; 32]).unwrap();
        let token = authority
            .issue("mini-a", "mini-a.local", "pub-a", Duration::from_secs(60))
            .unwrap();
        let claims = authority.verify(&token, now_ms()).unwrap();
        assert_eq!(claims.node_id, "mini-a");
        let mut bad = token.into_bytes();
        let last = bad.len() - 1;
        bad[last] = if bad[last] == b'a' { b'b' } else { b'a' };
        assert!(
            authority
                .verify(std::str::from_utf8(&bad).unwrap(), now_ms())
                .is_err()
        );
    }

    #[test]
    fn verified_claims_add_peer() {
        let mut mesh =
            MeshState::new(DEFAULT_POOL.parse().unwrap(), "home", "home", "pk-home").unwrap();
        let claims = JoinClaims {
            node_id: "mini".into(),
            hostname: "mini.local".into(),
            public_key: "pk-mini".into(),
            expires_ms: now_ms() + 1000,
            nonce: "00112233445566778899aabbccddeeff".into(),
        };
        apply_join(&mut mesh, &claims).unwrap();
        assert!(mesh.node("mini").is_ok());
    }
}
