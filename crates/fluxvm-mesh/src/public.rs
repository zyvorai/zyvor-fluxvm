// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Durable public-cloud edge configuration.
//!
//! This is intentionally separate from `MeshState`: losing an ingress route or
//! DDNS setting must never corrupt the connectivity/control-plane state that
//! keeps hosts reachable.

use crate::{MeshError, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

pub const PUBLIC_CONFIG_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct IngressRoute {
    pub name: String,
    pub hostname: String,
    pub backend: SocketAddr,
    #[serde(default)]
    pub share_required: bool,
    #[serde(default)]
    pub strip_share_prefix: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DdnsProvider {
    Cloudflare {
        zone_id: String,
        record_id_v4: Option<String>,
        record_id_v6: Option<String>,
        token_env: String,
        #[serde(default = "default_cloudflare_api")]
        api_base: String,
    },
    Webhook {
        url: String,
        token_env: Option<String>,
    },
}

fn default_cloudflare_api() -> String {
    "https://api.cloudflare.com/client/v4".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DdnsConfig {
    pub hostname: String,
    pub provider: DdnsProvider,
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    #[serde(default)]
    pub proxied: bool,
}

fn default_ttl() -> u32 {
    120
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RouterProtocol {
    Auto,
    Pcp,
    NatPmp,
    Upnp,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortMapping {
    pub name: String,
    pub protocol: String,
    pub internal_port: u16,
    pub external_port: u16,
    pub lifetime_seconds: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouterConfig {
    pub gateway: IpAddr,
    #[serde(default = "default_router_protocol")]
    pub protocol: RouterProtocol,
    #[serde(default)]
    pub mappings: Vec<PortMapping>,
}

fn default_router_protocol() -> RouterProtocol {
    RouterProtocol::Auto
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct WanConfig {
    pub name: String,
    pub interface: String,
    pub gateway: Option<IpAddr>,
    #[serde(default)]
    pub metric: u32,
    #[serde(default)]
    pub preferred: bool,
    #[serde(default)]
    pub probe_targets: Vec<SocketAddr>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Dns01Provider {
    Cloudflare {
        zone_id: String,
        token_env: String,
        #[serde(default = "default_cloudflare_api")]
        api_base: String,
    },
    Webhook {
        url: String,
        token_env: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dns01Config {
    pub provider: Dns01Provider,
    #[serde(default = "default_dns01_propagation")]
    pub propagation_seconds: u32,
    /// Recursive resolver used to verify the TXT record before notifying ACME.
    #[serde(default)]
    pub validation_resolver: Option<SocketAddr>,
    /// Require the resolver's DNSSEC Authenticated Data bit for the TXT answer.
    #[serde(default)]
    pub require_dnssec_ad: bool,
}
fn default_dns01_propagation() -> u32 {
    20
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AcmeChallenge {
    #[default]
    Http01,
    Dns01,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicRelayConfig {
    pub relay_addr: SocketAddr,
    #[serde(default)]
    pub quic_relay_addr: Option<SocketAddr>,
    #[serde(default)]
    pub transport: crate::public_relay::RelayTransport,
    pub server_name: String,
    pub ca_file: PathBuf,
    pub site_id: String,
    pub secret_file: PathBuf,
    #[serde(default = "default_relay_local_ingress")]
    pub local_ingress: SocketAddr,
}
fn default_relay_local_ingress() -> SocketAddr {
    "127.0.0.1:443".parse().unwrap()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcmeConfig {
    pub directory_url: String,
    pub email: String,
    pub domains: Vec<String>,
    #[serde(default = "default_acme_renew_before_days")]
    pub renew_before_days: u32,
    #[serde(default)]
    pub challenge: AcmeChallenge,
    pub dns01: Option<Dns01Config>,
}

fn default_acme_renew_before_days() -> u32 {
    30
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OtlpConfig {
    pub endpoint: String,
    #[serde(default = "default_otlp_service_name")]
    pub service_name: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}
fn default_otlp_service_name() -> String {
    "fluxmesh-edge".into()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicCloudConfig {
    pub version: u32,
    #[serde(default)]
    pub ingress: Vec<IngressRoute>,
    pub ddns: Option<DdnsConfig>,
    pub router: Option<RouterConfig>,
    #[serde(default)]
    pub wan: Vec<WanConfig>,
    pub acme: Option<AcmeConfig>,
    #[serde(default)]
    pub public_relay: Option<PublicRelayConfig>,
    #[serde(default)]
    pub otlp: Option<OtlpConfig>,
}

impl Default for PublicCloudConfig {
    fn default() -> Self {
        Self {
            version: PUBLIC_CONFIG_VERSION,
            ingress: Vec::new(),
            ddns: None,
            router: None,
            wan: Vec::new(),
            acme: None,
            public_relay: None,
            otlp: None,
        }
    }
}

impl PublicCloudConfig {
    pub fn validate(&self) -> Result<()> {
        if self.version != PUBLIC_CONFIG_VERSION {
            return Err(MeshError::InvalidState(format!(
                "unsupported public config version {}",
                self.version
            )));
        }
        let mut names = std::collections::BTreeSet::new();
        let mut hosts = std::collections::BTreeSet::new();
        for route in &self.ingress {
            if route.name.is_empty() || !names.insert(route.name.clone()) {
                return Err(MeshError::InvalidState(format!(
                    "duplicate/empty ingress route name {:?}",
                    route.name
                )));
            }
            if route.hostname.is_empty() || !hosts.insert(route.hostname.to_ascii_lowercase()) {
                return Err(MeshError::InvalidState(format!(
                    "duplicate/empty ingress hostname {:?}",
                    route.hostname
                )));
            }
        }
        if let Some(ddns) = &self.ddns {
            if ddns.hostname.trim().is_empty() {
                return Err(MeshError::InvalidState(
                    "DDNS hostname may not be empty".into(),
                ));
            }
            if ddns.ttl == 0 {
                return Err(MeshError::InvalidState("DDNS TTL must be > 0".into()));
            }
        }
        for wan in &self.wan {
            if wan.name.trim().is_empty() || wan.interface.trim().is_empty() {
                return Err(MeshError::InvalidState(
                    "WAN name/interface may not be empty".into(),
                ));
            }
        }
        if let Some(acme) = &self.acme {
            if acme.domains.is_empty() {
                return Err(MeshError::InvalidState(
                    "ACME requires at least one domain".into(),
                ));
            }
            if acme.renew_before_days == 0 || acme.renew_before_days >= 90 {
                return Err(MeshError::InvalidState(
                    "ACME renew_before_days must be between 1 and 89".into(),
                ));
            }
            if acme.domains.iter().any(|d| d.trim().is_empty()) {
                return Err(MeshError::InvalidState(
                    "ACME domains must be non-empty".into(),
                ));
            }
            match acme.challenge {
                AcmeChallenge::Http01 => {
                    if acme.domains.iter().any(|d| d.contains('*')) {
                        return Err(MeshError::InvalidState(
                            "HTTP-01 ACME does not support wildcard domains".into(),
                        ));
                    }
                }
                AcmeChallenge::Dns01 => {
                    let dns = acme.dns01.as_ref().ok_or_else(|| {
                        MeshError::InvalidState(
                            "DNS-01 ACME requires dns01 provider configuration".into(),
                        )
                    })?;
                    if dns.propagation_seconds == 0 || dns.propagation_seconds > 300 {
                        return Err(MeshError::InvalidState(
                            "DNS-01 propagation_seconds must be 1..=300".into(),
                        ));
                    }
                    if dns.require_dnssec_ad && dns.validation_resolver.is_none() {
                        return Err(MeshError::InvalidState(
                            "DNSSEC AD validation requires validation_resolver".into(),
                        ));
                    }
                }
            }
        }
        if let Some(relay) = &self.public_relay {
            if relay.server_name.trim().is_empty() || relay.site_id.trim().is_empty() {
                return Err(MeshError::InvalidState(
                    "public relay requires server_name and site_id".into(),
                ));
            }
        }
        if let Some(otlp) = &self.otlp {
            if !(otlp.endpoint.starts_with("http://") || otlp.endpoint.starts_with("https://")) {
                return Err(MeshError::InvalidState(
                    "OTLP endpoint must be http:// or https://".into(),
                ));
            }
            if otlp.service_name.trim().is_empty() {
                return Err(MeshError::InvalidState(
                    "OTLP service_name may not be empty".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn upsert_ingress(&mut self, route: IngressRoute) -> Result<()> {
        if let Some(existing) = self.ingress.iter_mut().find(|v| v.name == route.name) {
            *existing = route;
        } else {
            self.ingress.push(route);
        }
        self.validate()
    }

    pub fn remove_ingress(&mut self, name: &str) -> bool {
        let before = self.ingress.len();
        self.ingress.retain(|route| route.name != name);
        before != self.ingress.len()
    }
}

#[derive(Clone, Debug)]
pub struct PublicConfigStore {
    path: PathBuf,
}

impl PublicConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<PublicCloudConfig> {
        if !self.path.exists() {
            return Ok(PublicCloudConfig::default());
        }
        let bytes = fs::read(&self.path)?;
        let cfg: PublicCloudConfig = serde_json::from_slice(&bytes)
            .map_err(|e| MeshError::InvalidState(format!("parse {}: {e}", self.path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, cfg: &PublicCloudConfig) -> Result<()> {
        cfg.validate()?;
        let parent = self.path.parent().ok_or_else(|| {
            MeshError::InvalidState(format!("{} has no parent", self.path.display()))
        })?;
        fs::create_dir_all(parent)?;
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(cfg)
            .map_err(|e| MeshError::InvalidState(format!("serialize public config: {e}")))?;
        fs::write(&tmp, bytes)?;
        set_private(&tmp)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn default_public_config_path() -> PathBuf {
    crate::mesh_dir().join("public.json")
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
    fn ingress_hostnames_are_unique_case_insensitively() {
        let mut cfg = PublicCloudConfig::default();
        cfg.ingress.push(IngressRoute {
            name: "one".into(),
            hostname: "Cloud.Example".into(),
            backend: "127.0.0.1:8080".parse().unwrap(),
            share_required: false,
            strip_share_prefix: false,
        });
        cfg.ingress.push(IngressRoute {
            name: "two".into(),
            hostname: "cloud.example".into(),
            backend: "127.0.0.1:8081".parse().unwrap(),
            share_required: false,
            strip_share_prefix: false,
        });
        assert!(cfg.validate().is_err());
    }
}
