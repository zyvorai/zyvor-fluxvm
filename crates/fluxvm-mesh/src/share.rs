// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Short-lived HMAC share links for public ingress.

use crate::{MeshError, Result};
use ring::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const VERSION: &str = "v1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareClaims {
    pub route: String,
    pub expires_ms: u64,
    pub nonce: String,
}

#[derive(Clone, Debug)]
pub struct ShareAuthority {
    key: Vec<u8>,
}

impl ShareAuthority {
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let key = if path.exists() {
            fs::read(path)?
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut key = vec![0u8; 32];
            SystemRandom::new().fill(&mut key).map_err(|_| {
                MeshError::InvalidState("share secret random generation failed".into())
            })?;
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, &key)?;
            set_private(&tmp)?;
            fs::rename(tmp, path)?;
            key
        };
        if key.len() < 32 {
            return Err(MeshError::InvalidState(
                "share secret must be at least 32 bytes".into(),
            ));
        }
        Ok(Self { key })
    }

    pub fn issue(&self, route: &str, ttl: Duration) -> Result<String> {
        validate_route(route)?;
        let expires_ms = now_ms().saturating_add(ttl.as_millis().min(u64::MAX as u128) as u64);
        let mut nonce = [0u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| MeshError::InvalidState("share nonce generation failed".into()))?;
        let nonce = hex(&nonce);
        let body = format!(
            "{VERSION}.{}.{expires_ms}.{nonce}",
            encode(route.as_bytes())
        );
        let sig = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, &self.key),
            body.as_bytes(),
        );
        Ok(format!("{}.{}", body, hex(sig.as_ref())))
    }

    pub fn verify(&self, token: &str, expected_route: &str, at_ms: u64) -> Result<ShareClaims> {
        let parts = token.split('.').collect::<Vec<_>>();
        if parts.len() != 5 || parts[0] != VERSION {
            return Err(MeshError::InvalidState("invalid share token format".into()));
        }
        let route = String::from_utf8(decode(parts[1])?)
            .map_err(|_| MeshError::InvalidState("share route is not UTF-8".into()))?;
        validate_route(&route)?;
        if route != expected_route {
            return Err(MeshError::InvalidState("share token route mismatch".into()));
        }
        let expires_ms = parts[2]
            .parse::<u64>()
            .map_err(|_| MeshError::InvalidState("invalid share expiry".into()))?;
        if at_ms > expires_ms {
            return Err(MeshError::InvalidState("share token expired".into()));
        }
        let signature = unhex(parts[4])?;
        let signed = parts[..4].join(".");
        // `hmac::verify` compares in constant time.
        if hmac::verify(
            &hmac::Key::new(hmac::HMAC_SHA256, &self.key),
            signed.as_bytes(),
            &signature,
        )
        .is_err()
        {
            return Err(MeshError::InvalidState("invalid share signature".into()));
        }
        Ok(ShareClaims {
            route,
            expires_ms,
            nonce: parts[3].into(),
        })
    }
}

pub fn token_from_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/s/")?;
    let (token, suffix) = rest.split_once('/').unwrap_or((rest, ""));
    (!token.is_empty()).then_some((token, suffix))
}

pub fn default_share_secret_path() -> PathBuf {
    crate::mesh_dir().join("share.key")
}

fn validate_route(route: &str) -> Result<()> {
    if route.is_empty()
        || route.len() > 64
        || !route
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(MeshError::InvalidState(format!(
            "invalid share route {route:?}"
        )));
    }
    Ok(())
}

fn encode(input: &[u8]) -> String {
    hex(input)
}
fn decode(value: &str) -> Result<Vec<u8>> {
    unhex(value)
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(H[(b >> 4) as usize] as char);
        out.push(H[(b & 15) as usize] as char);
    }
    out
}

fn unhex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(MeshError::InvalidState("invalid hex".into()));
    }
    (0..value.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&value[i..i + 2], 16)
                .map_err(|_| MeshError::InvalidState("invalid hex".into()))
        })
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(unix)]
fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_private(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_is_route_bound_and_expires() {
        let auth = ShareAuthority { key: vec![7; 32] };
        let token = auth.issue("photos", Duration::from_secs(60)).unwrap();
        let now = now_ms();
        assert_eq!(auth.verify(&token, "photos", now).unwrap().route, "photos");
        assert!(auth.verify(&token, "files", now).is_err());
        assert!(auth.verify(&token, "photos", now + 61_000).is_err());
    }
    #[test]
    fn parses_share_path() {
        assert_eq!(
            token_from_path("/s/abc/photos/1.jpg"),
            Some(("abc", "photos/1.jpg"))
        );
    }
}
