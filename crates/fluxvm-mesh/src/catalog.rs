// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Durable Phase-2 policy/catalog state. Connectivity state remains in the
//! Phase-1 `state.v1`; service/DNS/ACL/gateway/relay intent is a separate
//! atomic catalog so fast path observations do not rewrite policy state.

use crate::acl::{
    AclAction, AclProtocol, AclRule, DestinationSelector, SourceSelector, validate_rule,
};
use crate::gateway::{ExitNodePolicy, GatewayPolicies, SubnetPolicy};
use crate::relay::{RelayRecord, RelayRegistry, RelayTransport};
use crate::service::{
    DEFAULT_SERVICE_POOL, EndpointHealth, ServiceCatalog, ServiceEndpoint, ServiceProtocol,
};
use crate::{MeshError, MeshState, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const CATALOG_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloudCatalog {
    pub version: u32,
    pub dns_suffix: String,
    pub services: ServiceCatalog,
    pub acls: Vec<AclRule>,
    pub gateways: GatewayPolicies,
    pub relays: RelayRegistry,
}

impl Default for CloudCatalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            dns_suffix: crate::dns::DEFAULT_DNS_SUFFIX.into(),
            services: ServiceCatalog::default(),
            acls: Vec::new(),
            gateways: GatewayPolicies::default(),
            relays: RelayRegistry::default(),
        }
    }
}

impl CloudCatalog {
    pub fn upsert_acl(&mut self, rule: AclRule) -> Result<()> {
        validate_rule(&rule)?;
        self.acls.retain(|r| r.id != rule.id);
        self.acls.push(rule);
        self.acls.sort_by_key(|r| (r.priority, r.id.clone()));
        Ok(())
    }

    pub fn remove_acl(&mut self, id: &str) -> bool {
        let before = self.acls.len();
        self.acls.retain(|r| r.id != id);
        before != self.acls.len()
    }
}

pub struct CatalogStore {
    path: PathBuf,
}

impl CatalogStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self, catalog: &CloudCatalog) -> Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        body.push_str(&format!("CATALOG\t{}\n", catalog.version));
        body.push_str(&format!(
            "DNS_SUFFIX\t{}\n",
            hex_encode(&catalog.dns_suffix)
        ));
        body.push_str(&format!("SERVICE_POOL\t{}\n", catalog.services.ipam.pool));
        for svc in catalog.services.services.values() {
            body.push_str(&format!(
                "SERVICE\t{}\t{}\t{}\t{}\n",
                hex_encode(&svc.name),
                svc.vip,
                svc.port,
                svc.protocol.as_str()
            ));
            for endpoint in &svc.endpoints {
                body.push_str(&format!(
                    "SERVICE_ENDPOINT\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    hex_encode(&svc.name),
                    hex_encode(&endpoint.node_id),
                    endpoint.address,
                    endpoint.port,
                    endpoint.weight,
                    endpoint.health.as_str()
                ));
            }
        }
        for rule in &catalog.acls {
            body.push_str(&format!(
                "ACL\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                hex_encode(&rule.id),
                rule.priority,
                rule.action.as_str(),
                hex_encode(&rule.source.to_string()),
                hex_encode(&rule.destination.to_string()),
                rule.protocol.as_str(),
                rule.port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into())
            ));
        }
        for exit in catalog.gateways.exits.values() {
            body.push_str(&format!(
                "EXIT\t{}\t{}\n",
                hex_encode(&exit.node_id),
                if exit.enabled { 1 } else { 0 }
            ));
            for source in &exit.allowed_nodes {
                body.push_str(&format!(
                    "EXIT_ALLOW\t{}\t{}\n",
                    hex_encode(&exit.node_id),
                    hex_encode(source)
                ));
            }
        }
        for subnet in &catalog.gateways.subnets {
            body.push_str(&format!(
                "SUBNET\t{}\t{}\n",
                subnet.prefix,
                hex_encode(&subnet.via_node)
            ));
            for source in &subnet.allowed_nodes {
                body.push_str(&format!(
                    "SUBNET_ALLOW\t{}\t{}\t{}\n",
                    subnet.prefix,
                    hex_encode(&subnet.via_node),
                    hex_encode(source)
                ));
            }
        }
        for relay in catalog.relays.relays.values() {
            body.push_str(&format!(
                "RELAY\t{}\t{}\t{}\t{}\t{}\t{}\n",
                hex_encode(&relay.id),
                relay.endpoint,
                relay.transport.as_str(),
                relay.priority,
                relay.load_percent,
                if relay.healthy { 1 } else { 0 }
            ));
        }

        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(tmp, &self.path)?;
        Ok(())
    }

    pub fn load(&self, mesh: &MeshState) -> Result<CloudCatalog> {
        if !self.path.exists() {
            return Ok(CloudCatalog::default());
        }
        let text = fs::read_to_string(&self.path)?;
        let mut version = None;
        let mut dns_suffix = None;
        let mut service_pool = None;
        let mut records: BTreeMap<&str, Vec<(usize, Vec<&str>)>> = BTreeMap::new();
        for (line_no, line) in text.lines().enumerate() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.first().copied() {
                Some("CATALOG") if fields.len() == 2 => {
                    version = Some(parse_u32(fields[1], line_no)?);
                }
                Some("DNS_SUFFIX") if fields.len() == 2 => {
                    dns_suffix = Some(hex_decode(fields[1])?)
                }
                Some("SERVICE_POOL") if fields.len() == 2 => {
                    service_pool = Some(fields[1].parse()?)
                }
                Some(
                    kind @ ("SERVICE" | "SERVICE_ENDPOINT" | "ACL" | "EXIT" | "EXIT_ALLOW"
                    | "SUBNET" | "SUBNET_ALLOW" | "RELAY"),
                ) => {
                    records.entry(kind).or_default().push((line_no, fields));
                }
                Some(kind) => {
                    return Err(MeshError::InvalidState(format!(
                        "catalog line {}: invalid {kind} record",
                        line_no + 1
                    )));
                }
                None => {}
            }
        }
        let version =
            version.ok_or_else(|| MeshError::InvalidState("missing CATALOG version".into()))?;
        if version != CATALOG_VERSION {
            return Err(MeshError::InvalidState(format!(
                "unsupported cloud catalog version {version}"
            )));
        }
        let pool = service_pool.unwrap_or(DEFAULT_SERVICE_POOL.parse()?);
        let mut catalog = CloudCatalog {
            version,
            dns_suffix: dns_suffix.unwrap_or_else(|| crate::dns::DEFAULT_DNS_SUFFIX.into()),
            services: ServiceCatalog::new(pool),
            acls: Vec::new(),
            gateways: GatewayPolicies::default(),
            relays: RelayRegistry::default(),
        };

        for (line_no, f) in records.remove("SERVICE").unwrap_or_default() {
            if f.len() != 5 {
                return invalid_record(line_no, "SERVICE");
            }
            let name = hex_decode(f[1])?;
            let vip: Ipv4Addr = parse(f[2], line_no, "IPv4 VIP")?;
            let port = parse_u16(f[3], line_no)?;
            let protocol = ServiceProtocol::from_str(f[4])?;
            catalog
                .services
                .restore_service(name, vip, port, protocol)?;
        }
        for (line_no, f) in records.remove("SERVICE_ENDPOINT").unwrap_or_default() {
            if f.len() != 7 {
                return invalid_record(line_no, "SERVICE_ENDPOINT");
            }
            let service = hex_decode(f[1])?;
            let node_id = hex_decode(f[2])?;
            let address: IpAddr = parse(f[3], line_no, "endpoint address")?;
            let port = parse_u16(f[4], line_no)?;
            let weight = parse_u16(f[5], line_no)?;
            let health = EndpointHealth::from_str(f[6])?;
            catalog.services.add_endpoint(
                mesh,
                &service,
                ServiceEndpoint {
                    node_id,
                    address,
                    port,
                    weight,
                    health,
                },
            )?;
        }
        for (line_no, f) in records.remove("ACL").unwrap_or_default() {
            if f.len() != 8 {
                return invalid_record(line_no, "ACL");
            }
            let rule = AclRule {
                id: hex_decode(f[1])?,
                priority: parse_u16(f[2], line_no)?,
                action: AclAction::from_str(f[3])?,
                source: SourceSelector::from_str(&hex_decode(f[4])?)?,
                destination: DestinationSelector::from_str(&hex_decode(f[5])?)?,
                protocol: AclProtocol::from_str(f[6])?,
                port: if f[7] == "-" {
                    None
                } else {
                    Some(parse_u16(f[7], line_no)?)
                },
            };
            catalog.upsert_acl(rule)?;
        }

        let mut exit_allowed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (line_no, f) in records.remove("EXIT_ALLOW").unwrap_or_default() {
            if f.len() != 3 {
                return invalid_record(line_no, "EXIT_ALLOW");
            }
            let node = hex_decode(f[1])?;
            let source = hex_decode(f[2])?;
            mesh.node(&source)?;
            exit_allowed.entry(node).or_default().insert(source);
        }
        for (line_no, f) in records.remove("EXIT").unwrap_or_default() {
            if f.len() != 3 {
                return invalid_record(line_no, "EXIT");
            }
            let node = hex_decode(f[1])?;
            mesh.node(&node)?;
            let enabled = parse_bool(f[2], line_no)?;
            catalog.gateways.exits.insert(
                node.clone(),
                ExitNodePolicy {
                    node_id: node.clone(),
                    enabled,
                    allowed_nodes: exit_allowed.remove(&node).unwrap_or_default(),
                },
            );
        }
        if !exit_allowed.is_empty() {
            return Err(MeshError::InvalidState(
                "EXIT_ALLOW references an EXIT that does not exist".into(),
            ));
        }

        let mut subnet_allowed: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
        for (line_no, f) in records.remove("SUBNET_ALLOW").unwrap_or_default() {
            if f.len() != 4 {
                return invalid_record(line_no, "SUBNET_ALLOW");
            }
            let prefix = f[1].to_owned();
            let via = hex_decode(f[2])?;
            let source = hex_decode(f[3])?;
            mesh.node(&source)?;
            subnet_allowed
                .entry((prefix, via))
                .or_default()
                .insert(source);
        }
        for (line_no, f) in records.remove("SUBNET").unwrap_or_default() {
            if f.len() != 3 {
                return invalid_record(line_no, "SUBNET");
            }
            let prefix = f[1].parse()?;
            let via = hex_decode(f[2])?;
            mesh.node(&via)?;
            let key = (f[1].to_owned(), via.clone());
            catalog.gateways.subnets.push(SubnetPolicy {
                prefix,
                via_node: via,
                allowed_nodes: subnet_allowed.remove(&key).unwrap_or_default(),
            });
        }
        if !subnet_allowed.is_empty() {
            return Err(MeshError::InvalidState(
                "SUBNET_ALLOW references a SUBNET that does not exist".into(),
            ));
        }

        for (line_no, f) in records.remove("RELAY").unwrap_or_default() {
            if f.len() != 7 {
                return invalid_record(line_no, "RELAY");
            }
            catalog.relays.upsert(RelayRecord {
                id: hex_decode(f[1])?,
                endpoint: parse::<SocketAddr>(f[2], line_no, "relay endpoint")?,
                transport: RelayTransport::from_str(f[3])?,
                priority: parse_u16(f[4], line_no)?,
                load_percent: parse_u8(f[5], line_no)?,
                healthy: parse_bool(f[6], line_no)?,
            })?;
        }
        Ok(catalog)
    }
}

fn invalid_record<T>(line_no: usize, kind: &str) -> Result<T> {
    Err(MeshError::InvalidState(format!(
        "catalog line {}: malformed {kind} record",
        line_no + 1
    )))
}

fn parse<T: FromStr>(value: &str, line_no: usize, what: &str) -> Result<T> {
    value.parse().map_err(|_| {
        MeshError::InvalidState(format!(
            "catalog line {}: invalid {what} {value}",
            line_no + 1
        ))
    })
}

fn parse_u8(value: &str, line_no: usize) -> Result<u8> {
    parse(value, line_no, "u8")
}
fn parse_u16(value: &str, line_no: usize) -> Result<u16> {
    parse(value, line_no, "u16")
}
fn parse_u32(value: &str, line_no: usize) -> Result<u32> {
    parse(value, line_no, "u32")
}
fn parse_bool(value: &str, line_no: usize) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(MeshError::InvalidState(format!(
            "catalog line {}: invalid boolean {value}",
            line_no + 1
        ))),
    }
}

fn hex_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(value.len() * 2);
    for b in value.as_bytes() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(value: &str) -> Result<String> {
    if !value.len().is_multiple_of(2) {
        return Err(MeshError::InvalidState(
            "odd-length hex catalog field".into(),
        ));
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().as_chunks::<2>().0 {
        let hi = nibble(pair[0])?;
        let lo = nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    String::from_utf8(out).map_err(|_| MeshError::InvalidState("non-UTF8 catalog field".into()))
}

fn nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(MeshError::InvalidState("invalid hex catalog field".into())),
    }
}

pub fn default_catalog_path() -> PathBuf {
    if let Ok(path) = std::env::var("FLUXMESH_CATALOG") {
        return PathBuf::from(path);
    }
    crate::mesh_dir().join("catalog.v1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::{AclAction, AclProtocol};
    use crate::service::{EndpointHealth, ServiceEndpoint};
    use crate::{DEFAULT_POOL, MeshState};
    use std::env;

    #[test]
    fn catalog_roundtrip_keeps_every_plane() {
        let mut mesh =
            MeshState::new(DEFAULT_POOL.parse().unwrap(), "home", "home", "key").unwrap();
        mesh.add_node("office", "office", "key2", false).unwrap();
        let mut c = CloudCatalog::default();
        c.services
            .add_service("photos", 443, ServiceProtocol::Tcp)
            .unwrap();
        c.services
            .add_endpoint(
                &mesh,
                "photos",
                ServiceEndpoint {
                    node_id: "office".into(),
                    address: "10.42.0.2".parse().unwrap(),
                    port: 8443,
                    weight: 2,
                    health: EndpointHealth::Ready,
                },
            )
            .unwrap();
        c.upsert_acl(AclRule {
            id: "allow-photos".into(),
            priority: 10,
            action: AclAction::Allow,
            source: "node:home".parse().unwrap(),
            destination: "service:photos".parse().unwrap(),
            protocol: AclProtocol::Tcp,
            port: Some(443),
        })
        .unwrap();
        c.gateways.enable_exit(&mesh, "office", ["home"]).unwrap();
        c.relays
            .upsert(RelayRecord {
                id: "blr1".into(),
                endpoint: "198.51.100.4:443".parse().unwrap(),
                transport: RelayTransport::Tcp443,
                priority: 10,
                load_percent: 7,
                healthy: true,
            })
            .unwrap();

        let path = env::temp_dir().join(format!("fluxmesh-catalog-{}.txt", std::process::id()));
        let store = CatalogStore::new(&path);
        store.save(&c).unwrap();
        let loaded = store.load(&mesh).unwrap();
        fs::remove_file(path).ok();
        assert_eq!(loaded, c);
    }
}
