// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! ACME DNS-01 record providers for wildcard and CGNAT-safe certificates.

use crate::public::{Dns01Config, Dns01Provider};
use crate::{MeshError, Result};
use serde_json::json;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct DnsLease {
    pub name: String,
    pub provider_id: Option<String>,
}

pub async fn present(cfg: &Dns01Config, name: &str, value: &str) -> Result<DnsLease> {
    validate_name(name)?;
    match &cfg.provider {
        Dns01Provider::Cloudflare {
            zone_id,
            token_env,
            api_base,
        } => {
            let token = std::env::var(token_env).map_err(|_| {
                MeshError::InvalidState(format!("DNS-01 token env {token_env} is unset"))
            })?;
            let url = format!(
                "{}/zones/{}/dns_records",
                api_base.trim_end_matches('/'),
                zone_id
            );
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(http_err)?;
            let body = json!({"type":"TXT","name":name,"content":value,"ttl":60});
            let response = client
                .post(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .map_err(http_err)?
                .error_for_status()
                .map_err(http_err)?;
            let v: serde_json::Value = response.json().await.map_err(http_err)?;
            let id = v
                .get("result")
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    MeshError::InvalidState("Cloudflare DNS-01 response had no record id".into())
                })?;
            Ok(DnsLease {
                name: name.into(),
                provider_id: Some(id.into()),
            })
        }
        Dns01Provider::Webhook { url, token_env } => {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(http_err)?;
            let mut req = client
                .post(url)
                .json(&json!({"action":"present","type":"TXT","name":name,"value":value}));
            if let Some(env) = token_env {
                let token = std::env::var(env).map_err(|_| {
                    MeshError::InvalidState(format!("DNS-01 token env {env} is unset"))
                })?;
                req = req.bearer_auth(token);
            }
            req.send()
                .await
                .map_err(http_err)?
                .error_for_status()
                .map_err(http_err)?;
            Ok(DnsLease {
                name: name.into(),
                provider_id: None,
            })
        }
    }
}

pub async fn cleanup(cfg: &Dns01Config, lease: &DnsLease, value: &str) -> Result<()> {
    match &cfg.provider {
        Dns01Provider::Cloudflare {
            zone_id,
            token_env,
            api_base,
        } => {
            let Some(id) = lease.provider_id.as_deref() else {
                return Ok(());
            };
            let token = std::env::var(token_env).map_err(|_| {
                MeshError::InvalidState(format!("DNS-01 token env {token_env} is unset"))
            })?;
            let url = format!(
                "{}/zones/{}/dns_records/{id}",
                api_base.trim_end_matches('/'),
                zone_id
            );
            reqwest::Client::new()
                .delete(url)
                .bearer_auth(token)
                .send()
                .await
                .map_err(http_err)?
                .error_for_status()
                .map_err(http_err)?;
        }
        Dns01Provider::Webhook { url, token_env } => {
            let client = reqwest::Client::new();
            let mut req = client
                .post(url)
                .json(&json!({"action":"cleanup","type":"TXT","name":lease.name,"value":value}));
            if let Some(env) = token_env {
                let token = std::env::var(env).map_err(|_| {
                    MeshError::InvalidState(format!("DNS-01 token env {env} is unset"))
                })?;
                req = req.bearer_auth(token);
            }
            req.send()
                .await
                .map_err(http_err)?
                .error_for_status()
                .map_err(http_err)?;
        }
    }
    Ok(())
}

pub async fn wait_for_propagation(cfg: &Dns01Config) {
    tokio::time::sleep(Duration::from_secs(u64::from(
        cfg.propagation_seconds.clamp(1, 300),
    )))
    .await;
}

fn validate_name(name: &str) -> Result<()> {
    if !name.starts_with("_acme-challenge.")
        || name.len() > 253
        || name.chars().any(char::is_whitespace)
    {
        return Err(MeshError::InvalidState(format!(
            "invalid DNS-01 owner name {name:?}"
        )));
    }
    Ok(())
}
fn http_err(e: reqwest::Error) -> MeshError {
    MeshError::InvalidState(format!("DNS-01 HTTP: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_validation() {
        assert!(validate_name("_acme-challenge.example.com").is_ok());
        assert!(validate_name("example.com").is_err());
    }
}
