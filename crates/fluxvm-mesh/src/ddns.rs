// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Dynamic DNS synchronization for public personal-cloud ingress.

use crate::public::{DdnsConfig, DdnsProvider};
use crate::{MeshError, Result};
use serde_json::json;
use std::net::IpAddr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DdnsReport {
    pub hostname: String,
    pub updated_v4: bool,
    pub updated_v6: bool,
}

pub async fn sync(
    config: &DdnsConfig,
    v4: Option<IpAddr>,
    v6: Option<IpAddr>,
) -> Result<DdnsReport> {
    let http = reqwest::Client::builder()
        .user_agent("Zyvor-FluxMesh/0.4.1")
        .build()
        .map_err(http_err)?;
    let mut report = DdnsReport {
        hostname: config.hostname.clone(),
        updated_v4: false,
        updated_v6: false,
    };
    match &config.provider {
        DdnsProvider::Cloudflare {
            zone_id,
            record_id_v4,
            record_id_v6,
            token_env,
            api_base,
        } => {
            let token = std::env::var(token_env).map_err(|_| {
                MeshError::InvalidState(format!(
                    "DDNS token environment variable {token_env} is not set"
                ))
            })?;
            if let (Some(record), Some(IpAddr::V4(ip))) = (record_id_v4.as_deref(), v4) {
                cloudflare_update(
                    &http,
                    api_base,
                    zone_id,
                    record,
                    &token,
                    &config.hostname,
                    "A",
                    &ip.to_string(),
                    config.ttl,
                    config.proxied,
                )
                .await?;
                report.updated_v4 = true;
            }
            if let (Some(record), Some(IpAddr::V6(ip))) = (record_id_v6.as_deref(), v6) {
                cloudflare_update(
                    &http,
                    api_base,
                    zone_id,
                    record,
                    &token,
                    &config.hostname,
                    "AAAA",
                    &ip.to_string(),
                    config.ttl,
                    config.proxied,
                )
                .await?;
                report.updated_v6 = true;
            }
        }
        DdnsProvider::Webhook { url, token_env } => {
            let mut req = http.post(url).json(&json!({
                "hostname": &config.hostname,
                "ipv4": v4.map(|v| v.to_string()),
                "ipv6": v6.map(|v| v.to_string()),
                "ttl": config.ttl,
            }));
            if let Some(name) = token_env {
                let token = std::env::var(name).map_err(|_| {
                    MeshError::InvalidState(format!(
                        "DDNS token environment variable {name} is not set"
                    ))
                })?;
                req = req.bearer_auth(token);
            }
            let response = req.send().await.map_err(http_err)?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(MeshError::InvalidState(format!(
                    "DDNS webhook returned {status}: {}",
                    body.chars().take(1024).collect::<String>()
                )));
            }
            report.updated_v4 = v4.is_some();
            report.updated_v6 = v6.is_some();
        }
    }
    Ok(report)
}

async fn cloudflare_update(
    http: &reqwest::Client,
    base: &str,
    zone: &str,
    record: &str,
    token: &str,
    name: &str,
    record_type: &str,
    content: &str,
    ttl: u32,
    proxied: bool,
) -> Result<()> {
    let url = format!(
        "{}/zones/{}/dns_records/{}",
        base.trim_end_matches('/'),
        zone,
        record
    );
    let response = http
        .put(url)
        .bearer_auth(token)
        .json(&json!({
            "type": record_type,
            "name": name,
            "content": content,
            "ttl": ttl,
            "proxied": proxied,
        }))
        .send()
        .await
        .map_err(http_err)?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(MeshError::InvalidState(format!(
            "Cloudflare DDNS returned {status}: {}",
            body.chars().take(1024).collect::<String>()
        )));
    }
    let body: serde_json::Value = response.json().await.map_err(http_err)?;
    if body.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return Err(MeshError::InvalidState(format!(
            "Cloudflare DDNS rejected update: {body}"
        )));
    }
    Ok(())
}

fn http_err(e: reqwest::Error) -> MeshError {
    MeshError::InvalidState(format!("DDNS HTTP: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_tracks_address_families_independently() {
        let r = DdnsReport {
            hostname: "x".into(),
            updated_v4: true,
            updated_v6: false,
        };
        assert!(r.updated_v4 && !r.updated_v6);
    }
}
