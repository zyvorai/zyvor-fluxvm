// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Cross-node service discovery for FluxMesh.
//!
//! FluxVM Service Fabric remains the node-local dataplane. This module stores
//! the distributed endpoint intent and renders a small plan that can be fed to
//! Service Fabric on every node.

use crate::{Ipv4Prefix, MeshError, MeshState, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;

pub const DEFAULT_SERVICE_POOL: &str = "100.88.0.0/16";

fn validate_name(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || value.starts_with('-')
        || value.ends_with('-')
    {
        return Err(MeshError::InvalidState(format!(
            "invalid service name {value:?}"
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceProtocol {
    Tcp,
    Udp,
}

impl ServiceProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

impl fmt::Display for ServiceProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ServiceProtocol {
    type Err = MeshError;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            _ => Err(MeshError::InvalidState(format!(
                "invalid service protocol {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointHealth {
    Ready,
    Draining,
    Unhealthy,
}

impl EndpointHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Unhealthy => "unhealthy",
        }
    }
}

impl FromStr for EndpointHealth {
    type Err = MeshError;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "ready" | "up" | "healthy" => Ok(Self::Ready),
            "draining" | "drain" => Ok(Self::Draining),
            "unhealthy" | "down" => Ok(Self::Unhealthy),
            _ => Err(MeshError::InvalidState(format!(
                "invalid endpoint health {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceEndpoint {
    pub node_id: String,
    pub address: IpAddr,
    pub port: u16,
    pub weight: u16,
    pub health: EndpointHealth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshService {
    pub name: String,
    pub vip: Ipv4Addr,
    pub port: u16,
    pub protocol: ServiceProtocol,
    pub endpoints: Vec<ServiceEndpoint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceIpam {
    pub pool: Ipv4Prefix,
    allocations: BTreeMap<String, Ipv4Addr>,
}

impl ServiceIpam {
    pub fn new(pool: Ipv4Prefix) -> Self {
        Self {
            pool,
            allocations: BTreeMap::new(),
        }
    }

    pub fn allocations(&self) -> &BTreeMap<String, Ipv4Addr> {
        &self.allocations
    }

    pub fn restore(&mut self, name: String, ip: Ipv4Addr) -> Result<()> {
        validate_name(&name)?;
        if !self.pool.contains(ip) {
            return Err(MeshError::InvalidState(format!(
                "service VIP {ip} is outside {}",
                self.pool
            )));
        }
        if self
            .allocations
            .iter()
            .any(|(existing_name, existing_ip)| existing_name != &name && existing_ip == &ip)
        {
            return Err(MeshError::InvalidState(format!(
                "duplicate service VIP {ip}"
            )));
        }
        self.allocations.insert(name, ip);
        Ok(())
    }

    pub fn allocate(&mut self, name: &str) -> Result<Ipv4Addr> {
        validate_name(name)?;
        if let Some(ip) = self.allocations.get(name) {
            return Ok(*ip);
        }
        let count = self.pool.usable_host_count();
        if count == 0 || self.allocations.len() as u64 >= count {
            return Err(MeshError::PoolExhausted);
        }
        let network = u32::from(self.pool.network());
        let first = if self.pool.prefix_len() >= 31 {
            network
        } else {
            network + 1
        };
        let used: BTreeSet<u32> = self.allocations.values().copied().map(u32::from).collect();
        // Stable FNV-1a, duplicated locally so service allocation does not
        // depend on the private node-IPAM helper.
        let mut hash = 0xcbf29ce484222325u64;
        for b in name.bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let start = hash % count;
        for offset in 0..count {
            let candidate = first as u64 + ((start + offset) % count);
            let candidate = candidate as u32;
            if !used.contains(&candidate) {
                let ip = Ipv4Addr::from(candidate);
                if self.pool.contains(ip) {
                    self.allocations.insert(name.to_owned(), ip);
                    return Ok(ip);
                }
            }
        }
        Err(MeshError::PoolExhausted)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceCatalog {
    pub ipam: ServiceIpam,
    pub services: BTreeMap<String, MeshService>,
}

impl Default for ServiceCatalog {
    fn default() -> Self {
        Self::new(DEFAULT_SERVICE_POOL.parse().expect("valid service pool"))
    }
}

impl ServiceCatalog {
    pub fn new(pool: Ipv4Prefix) -> Self {
        Self {
            ipam: ServiceIpam::new(pool),
            services: BTreeMap::new(),
        }
    }

    pub fn add_service(
        &mut self,
        name: impl Into<String>,
        port: u16,
        protocol: ServiceProtocol,
    ) -> Result<Ipv4Addr> {
        let name = name.into();
        validate_name(&name)?;
        if port == 0 {
            return Err(MeshError::InvalidState("service port may not be 0".into()));
        }
        if let Some(existing) = self.services.get_mut(&name) {
            existing.port = port;
            existing.protocol = protocol;
            return Ok(existing.vip);
        }
        let vip = self.ipam.allocate(&name)?;
        self.services.insert(
            name.clone(),
            MeshService {
                name,
                vip,
                port,
                protocol,
                endpoints: Vec::new(),
            },
        );
        Ok(vip)
    }

    pub fn restore_service(
        &mut self,
        name: String,
        vip: Ipv4Addr,
        port: u16,
        protocol: ServiceProtocol,
    ) -> Result<()> {
        validate_name(&name)?;
        if port == 0 {
            return Err(MeshError::InvalidState("service port may not be 0".into()));
        }
        self.ipam.restore(name.clone(), vip)?;
        if self
            .services
            .insert(
                name.clone(),
                MeshService {
                    name: name.clone(),
                    vip,
                    port,
                    protocol,
                    endpoints: Vec::new(),
                },
            )
            .is_some()
        {
            return Err(MeshError::InvalidState(format!("duplicate service {name}")));
        }
        Ok(())
    }

    pub fn service(&self, name: &str) -> Result<&MeshService> {
        self.services
            .get(name)
            .ok_or_else(|| MeshError::InvalidState(format!("unknown service {name}")))
    }

    pub fn service_mut(&mut self, name: &str) -> Result<&mut MeshService> {
        self.services
            .get_mut(name)
            .ok_or_else(|| MeshError::InvalidState(format!("unknown service {name}")))
    }

    pub fn add_endpoint(
        &mut self,
        mesh: &MeshState,
        service: &str,
        endpoint: ServiceEndpoint,
    ) -> Result<()> {
        mesh.node(&endpoint.node_id)?;
        if endpoint.port == 0 || endpoint.weight == 0 || endpoint.weight > 32 {
            return Err(MeshError::InvalidState(
                "service endpoint requires port > 0 and weight 1..32".into(),
            ));
        }
        let svc = self.service_mut(service)?;
        if let Some(existing) = svc.endpoints.iter_mut().find(|e| {
            e.node_id == endpoint.node_id
                && e.address == endpoint.address
                && e.port == endpoint.port
        }) {
            *existing = endpoint;
        } else {
            svc.endpoints.push(endpoint);
        }
        svc.endpoints
            .sort_by_key(|e| (e.node_id.clone(), e.address.to_string(), e.port));
        Ok(())
    }

    pub fn set_endpoint_health(
        &mut self,
        service: &str,
        node_id: &str,
        address: IpAddr,
        port: u16,
        health: EndpointHealth,
    ) -> Result<()> {
        let svc = self.service_mut(service)?;
        let endpoint = svc
            .endpoints
            .iter_mut()
            .find(|e| e.node_id == node_id && e.address == address && e.port == port)
            .ok_or_else(|| MeshError::InvalidState("service endpoint not found".into()))?;
        endpoint.health = health;
        Ok(())
    }

    pub fn remove_endpoint(
        &mut self,
        service: &str,
        node_id: &str,
        address: IpAddr,
        port: u16,
    ) -> Result<bool> {
        let svc = self.service_mut(service)?;
        let before = svc.endpoints.len();
        svc.endpoints
            .retain(|e| !(e.node_id == node_id && e.address == address && e.port == port));
        Ok(before != svc.endpoints.len())
    }

    pub fn plan(&self, service: &str) -> Result<ServiceFabricPlan> {
        let svc = self.service(service)?;
        Ok(ServiceFabricPlan {
            name: svc.name.clone(),
            vip: IpAddr::V4(svc.vip),
            port: svc.port,
            protocol: svc.protocol,
            backends: svc
                .endpoints
                .iter()
                .map(|e| ServiceFabricBackend {
                    address: e.address,
                    port: e.port,
                    weight: e.weight,
                    state: e.health,
                })
                .collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceFabricBackend {
    pub address: IpAddr,
    pub port: u16,
    pub weight: u16,
    pub state: EndpointHealth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceFabricPlan {
    pub name: String,
    pub vip: IpAddr,
    pub port: u16,
    pub protocol: ServiceProtocol,
    pub backends: Vec<ServiceFabricBackend>,
}

impl ServiceFabricPlan {
    /// Dependency-free JSON renderer for the existing Service Fabric REST API.
    pub fn to_json(&self) -> String {
        let backends = self
            .backends
            .iter()
            .map(|b| {
                format!(
                    "{{\"address\":\"{}\",\"port\":{},\"weight\":{},\"enabled\":{},\"state\":\"{}\"}}",
                    b.address,
                    b.port,
                    b.weight,
                    true,
                    b.state.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"name\":\"{}\",\"vip\":\"{}\",\"port\":{},\"protocol\":\"{}\",\"algorithm\":\"maglev\",\"mode\":\"nat\",\"exposure\":\"east-west\",\"backends\":[{}]}}",
            self.name,
            self.vip,
            self.port,
            self.protocol.as_str(),
            backends
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_POOL, MeshState};

    fn mesh() -> MeshState {
        let mut mesh = MeshState::new(
            DEFAULT_POOL.parse().unwrap(),
            "home",
            "home.local",
            "home-key",
        )
        .unwrap();
        mesh.add_node("office", "office.local", "office-key", false)
            .unwrap();
        mesh
    }

    #[test]
    fn service_vip_is_stable() {
        let mut c = ServiceCatalog::default();
        let a = c.add_service("photos", 443, ServiceProtocol::Tcp).unwrap();
        let b = c.add_service("photos", 8443, ServiceProtocol::Tcp).unwrap();
        assert_eq!(a, b);
        assert_eq!(c.service("photos").unwrap().port, 8443);
    }

    #[test]
    fn service_fabric_plan_matches_existing_shape() {
        let mut c = ServiceCatalog::default();
        c.add_service("photos", 443, ServiceProtocol::Tcp).unwrap();
        c.add_endpoint(
            &mesh(),
            "photos",
            ServiceEndpoint {
                node_id: "office".into(),
                address: "10.42.0.10".parse().unwrap(),
                port: 8443,
                weight: 2,
                health: EndpointHealth::Ready,
            },
        )
        .unwrap();
        let json = c.plan("photos").unwrap().to_json();
        assert!(json.contains("\"algorithm\":\"maglev\""));
        assert!(json.contains("10.42.0.10"));
        assert!(json.contains("\"weight\":2"));
    }
}
