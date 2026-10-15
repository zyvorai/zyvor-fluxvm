// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Reconcile FluxMesh personal-cloud intent into FluxVM's existing Network
//! Fabric and Service Fabric REST surfaces.
//!
//! The ownership boundary stays intact: FluxMesh owns distributed personal-
//! cloud intent; FluxVM continues to own VM-edge eBPF policy and node-local
//! Maglev/NAT/DSR execution.

use crate::acl::{AclAction, AclProtocol, DestinationSelector, SourceSelector};
use crate::catalog::CloudCatalog;
use crate::http::HttpEndpoint;
use crate::{MeshError, MeshState, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FabricCnpPlan {
    pub name: String,
    pub json: String,
}

/// Compile the fail-closed subset of FluxMesh ACL semantics to FluxVM's
/// CNP-compatible Network Fabric API.
///
/// Targeted explicit deny rules are rejected rather than silently changing
/// ordered first-match ACL semantics into unordered CNP deny precedence. The
/// common personal-cloud model (specific allows + final deny-all) compiles
/// exactly and safely.
pub fn compile_network_fabric_cnp(
    mesh: &MeshState,
    catalog: &CloudCatalog,
    source_node: &str,
) -> Result<FabricCnpPlan> {
    mesh.node(source_node)?;
    let mut rules = catalog.acls.iter().collect::<Vec<_>>();
    rules.sort_by_key(|rule| (rule.priority, rule.id.clone()));
    let mut egress = Vec::new();

    for rule in rules {
        let applies_to_source = match &rule.source {
            SourceSelector::Any => true,
            SourceSelector::Node(node) => node == source_node,
        };
        if !applies_to_source {
            continue;
        }
        if rule.action == AclAction::Deny {
            let default_deny_source = match &rule.source {
                SourceSelector::Any => true,
                SourceSelector::Node(node) => node == source_node,
            };
            let default_deny = default_deny_source
                && matches!(&rule.destination, DestinationSelector::Any)
                && rule.protocol == AclProtocol::Any
                && rule.port.is_none();
            if default_deny {
                continue;
            }
            return Err(MeshError::InvalidState(format!(
                "ACL {} is a targeted deny; Network Fabric CNP compilation only accepts allow rules plus a final deny-all so ordered semantics remain fail-closed",
                rule.id
            )));
        }
        if rule.protocol == AclProtocol::Icmp {
            return Err(MeshError::InvalidState(format!(
                "ACL {} allows ICMP; the current FluxVM CNP toPorts subset cannot express ICMP without widening the rule",
                rule.id
            )));
        }
        if rule.port.is_some() && rule.protocol == AclProtocol::Any {
            return Err(MeshError::InvalidState(format!(
                "ACL {} has a port with protocol=any; specify tcp or udp before CNP compilation",
                rule.id
            )));
        }
        let cidr = match &rule.destination {
            DestinationSelector::Any => "0.0.0.0/0".to_owned(),
            DestinationSelector::Node(node) => format!("{}/32", mesh.node(node)?.overlay_ip),
            DestinationSelector::Service(service) => {
                format!("{}/32", catalog.services.service(service)?.vip)
            }
            DestinationSelector::Cidr(cidr) => cidr.to_string(),
        };
        let mut json = format!("{{\"toCIDR\":[\"{}\"]", json_escape(&cidr));
        if let Some(port) = rule.port {
            let protocol = match rule.protocol {
                AclProtocol::Tcp => "TCP",
                AclProtocol::Udp => "UDP",
                AclProtocol::Any | AclProtocol::Icmp => unreachable!(),
            };
            json.push_str(&format!(
                ",\"toPorts\":[{{\"ports\":[{{\"port\":\"{port}\",\"protocol\":\"{protocol}\"}}]}}]"
            ));
        }
        json.push('}');
        egress.push(json);
    }

    let name = format!("fluxmesh-{}", normalize_name(source_node));
    let json = format!(
        "{{\"apiVersion\":\"cilium.io/v2\",\"kind\":\"CiliumNetworkPolicy\",\"metadata\":{{\"name\":\"{}\"}},\"spec\":{{\"endpointSelector\":{{\"matchLabels\":{{\"fluxmesh.node\":\"{}\"}}}},\"enableDefaultDeny\":{{\"egress\":true,\"ingress\":false}},\"auditMode\":false,\"description\":\"FluxMesh personal-cloud egress policy for node {}\",\"egress\":[{}]}}}}",
        json_escape(&name),
        json_escape(source_node),
        json_escape(source_node),
        egress.join(",")
    );
    Ok(FabricCnpPlan { name, json })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileLedger {
    pub services: BTreeSet<String>,
    pub cnps: BTreeSet<String>,
}

impl ReconcileLedger {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        let mut out = Self::default();
        for (line_no, line) in text.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((kind, name)) = line.split_once('\t') else {
                return Err(MeshError::InvalidState(format!(
                    "invalid reconcile ledger line {}",
                    line_no + 1
                )));
            };
            match kind {
                "SERVICE" => {
                    out.services.insert(name.to_owned());
                }
                "CNP" => {
                    out.cnps.insert(name.to_owned());
                }
                _ => {
                    return Err(MeshError::InvalidState(format!(
                        "unknown reconcile ledger record {kind}"
                    )));
                }
            }
        }
        Ok(out)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        for service in &self.services {
            body.push_str(&format!("SERVICE\t{service}\n"));
        }
        for cnp in &self.cnps {
            body.push_str(&format!("CNP\t{cnp}\n"));
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(tmp, path)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FabricReconcileReport {
    pub services_upserted: usize,
    pub services_deleted: usize,
    pub cnps_upserted: usize,
    pub cnps_deleted: usize,
}

pub fn reconcile_fluxvm_fabrics(
    api: &HttpEndpoint,
    token: Option<&str>,
    mesh: &MeshState,
    catalog: &CloudCatalog,
    ledger_path: impl Into<PathBuf>,
) -> Result<FabricReconcileReport> {
    let ledger_path = ledger_path.into();
    let previous = ReconcileLedger::load(&ledger_path)?;
    let mut next = ReconcileLedger::default();
    let mut report = FabricReconcileReport::default();

    for name in catalog.services.services.keys() {
        let plan = catalog.services.plan(name)?;
        api.request_json("POST", "/v1/network/services", token, Some(&plan.to_json()))?;
        next.services.insert(name.clone());
        report.services_upserted += 1;
    }
    for stale in previous.services.difference(&next.services) {
        api.request_json(
            "DELETE",
            &format!("/v1/network/services/{stale}"),
            token,
            None,
        )?;
        report.services_deleted += 1;
    }

    let cnp = compile_network_fabric_cnp(mesh, catalog, &mesh.local_node)?;
    api.request_json("POST", "/v1/network/cnp", token, Some(&cnp.json))?;
    next.cnps.insert(cnp.name.clone());
    report.cnps_upserted += 1;
    for stale in previous.cnps.difference(&next.cnps) {
        api.request_json("DELETE", &format!("/v1/network/cnp/{stale}"), token, None)?;
        report.cnps_deleted += 1;
    }

    next.save(&ledger_path)?;
    Ok(report)
}

pub fn default_ledger_path() -> PathBuf {
    std::env::var_os("FLUXMESH_RECONCILE_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::mesh_dir().join("reconcile.v1"))
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn json_escape(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acl::{AclRule, DestinationSelector};
    use crate::catalog::CloudCatalog;
    use crate::service::ServiceProtocol;
    use crate::{DEFAULT_POOL, MeshState};

    #[test]
    fn compiles_node_and_service_allow_to_default_deny_cnp() {
        let mut mesh = MeshState::new(
            DEFAULT_POOL.parse().unwrap(),
            "home",
            "home.local",
            "home-key",
        )
        .unwrap();
        mesh.add_node("office", "office.local", "office-key", false)
            .unwrap();
        let mut catalog = CloudCatalog::default();
        catalog
            .services
            .add_service("photos", 443, ServiceProtocol::Tcp)
            .unwrap();
        catalog.acls = vec![
            AclRule {
                id: "allow-office".into(),
                priority: 10,
                action: AclAction::Allow,
                source: SourceSelector::Node("home".into()),
                destination: DestinationSelector::Node("office".into()),
                protocol: AclProtocol::Tcp,
                port: Some(22),
            },
            AclRule {
                id: "allow-photos".into(),
                priority: 20,
                action: AclAction::Allow,
                source: SourceSelector::Node("home".into()),
                destination: DestinationSelector::Service("photos".into()),
                protocol: AclProtocol::Tcp,
                port: Some(443),
            },
            AclRule {
                id: "deny-all".into(),
                priority: 1000,
                action: AclAction::Deny,
                source: SourceSelector::Any,
                destination: DestinationSelector::Any,
                protocol: AclProtocol::Any,
                port: None,
            },
        ];
        let plan = compile_network_fabric_cnp(&mesh, &catalog, "home").unwrap();
        assert!(plan.json.contains("\"enableDefaultDeny\":{\"egress\":true"));
        assert!(
            plan.json
                .contains(&format!("{}/32", mesh.node("office").unwrap().overlay_ip))
        );
        assert!(plan.json.contains("\"port\":\"443\""));
        assert!(plan.json.contains("fluxmesh.node"));
    }

    #[test]
    fn targeted_deny_is_rejected_instead_of_widening_semantics() {
        let mesh = MeshState::new(
            DEFAULT_POOL.parse().unwrap(),
            "home",
            "home.local",
            "home-key",
        )
        .unwrap();
        let mut catalog = CloudCatalog::default();
        catalog.acls.push(AclRule {
            id: "deny-one".into(),
            priority: 5,
            action: AclAction::Deny,
            source: SourceSelector::Any,
            destination: "cidr:10.0.0.0/8".parse().unwrap(),
            protocol: AclProtocol::Any,
            port: None,
        });
        assert!(compile_network_fabric_cnp(&mesh, &catalog, "home").is_err());
    }
}
