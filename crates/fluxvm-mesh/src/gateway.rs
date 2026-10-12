// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Exit-node and subnet-router policy plus a deterministic Linux forwarding
//! plan. The plan is data; callers decide when/how to apply it.

use crate::{Ipv4Prefix, MeshError, MeshState, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitNodePolicy {
    pub node_id: String,
    pub enabled: bool,
    pub allowed_nodes: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubnetPolicy {
    pub prefix: Ipv4Prefix,
    pub via_node: String,
    pub allowed_nodes: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GatewayPolicies {
    pub exits: BTreeMap<String, ExitNodePolicy>,
    pub subnets: Vec<SubnetPolicy>,
}

impl GatewayPolicies {
    pub fn enable_exit<I, S>(&mut self, mesh: &MeshState, node_id: &str, allowed: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        mesh.node(node_id)?;
        let mut allowed_nodes = BTreeSet::new();
        for source in allowed {
            let source = source.as_ref();
            mesh.node(source)?;
            allowed_nodes.insert(source.to_owned());
        }
        self.exits.insert(
            node_id.to_owned(),
            ExitNodePolicy {
                node_id: node_id.to_owned(),
                enabled: true,
                allowed_nodes,
            },
        );
        Ok(())
    }

    pub fn disable_exit(&mut self, node_id: &str) -> bool {
        self.exits.remove(node_id).is_some()
    }

    pub fn set_subnet<I, S>(
        &mut self,
        mesh: &MeshState,
        prefix: Ipv4Prefix,
        via_node: &str,
        allowed: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        mesh.node(via_node)?;
        let mut allowed_nodes = BTreeSet::new();
        for source in allowed {
            let source = source.as_ref();
            mesh.node(source)?;
            allowed_nodes.insert(source.to_owned());
        }
        self.subnets
            .retain(|p| !(p.prefix == prefix && p.via_node == via_node));
        self.subnets.push(SubnetPolicy {
            prefix,
            via_node: via_node.to_owned(),
            allowed_nodes,
        });
        self.subnets
            .sort_by_key(|p| (p.via_node.clone(), p.prefix.to_string()));
        Ok(())
    }

    pub fn can_use_exit(&self, source_node: &str, exit_node: &str) -> bool {
        self.exits.get(exit_node).is_some_and(|policy| {
            policy.enabled
                && (policy.allowed_nodes.is_empty() || policy.allowed_nodes.contains(source_node))
        })
    }

    pub fn can_use_subnet(&self, source_node: &str, via_node: &str, prefix: Ipv4Prefix) -> bool {
        self.subnets
            .iter()
            .find(|p| p.via_node == via_node && p.prefix == prefix)
            .is_some_and(|p| p.allowed_nodes.is_empty() || p.allowed_nodes.contains(source_node))
    }

    pub fn linux_plan(
        &self,
        mesh: &MeshState,
        local_node: &str,
        mesh_if: &str,
        uplink_if: &str,
    ) -> Result<LinuxGatewayPlan> {
        validate_iface(mesh_if)?;
        validate_iface(uplink_if)?;
        mesh.node(local_node)?;

        let mut source_ips = BTreeSet::new();
        if let Some(exit) = self.exits.get(local_node).filter(|p| p.enabled) {
            if exit.allowed_nodes.is_empty() {
                source_ips.extend(
                    mesh.nodes
                        .iter()
                        .filter(|(id, _)| id.as_str() != local_node)
                        .map(|(_, n)| n.overlay_ip),
                );
            } else {
                for id in &exit.allowed_nodes {
                    source_ips.insert(mesh.node(id)?.overlay_ip);
                }
            }
        }

        let mut subnet_rules = Vec::new();
        for policy in self.subnets.iter().filter(|p| p.via_node == local_node) {
            let allowed: Vec<_> = if policy.allowed_nodes.is_empty() {
                mesh.nodes
                    .iter()
                    .filter(|(id, _)| id.as_str() != local_node)
                    .map(|(_, n)| n.overlay_ip)
                    .collect()
            } else {
                policy
                    .allowed_nodes
                    .iter()
                    .map(|id| mesh.node(id).map(|n| n.overlay_ip))
                    .collect::<Result<Vec<_>>>()?
            };
            subnet_rules.push((policy.prefix, allowed));
        }

        let mut nft = String::new();
        nft.push_str("table inet fluxmesh {\n");
        nft.push_str("  chain forward { type filter hook forward priority 0; policy drop;\n");
        nft.push_str("    ct state established,related accept\n");
        for ip in &source_ips {
            nft.push_str(&format!(
                "    iifname \"{mesh_if}\" oifname \"{uplink_if}\" ip saddr {ip}/32 accept\n"
            ));
        }
        for (prefix, allowed) in &subnet_rules {
            for ip in allowed {
                nft.push_str(&format!(
                    "    iifname \"{mesh_if}\" ip saddr {ip}/32 ip daddr {prefix} accept\n"
                ));
            }
        }
        nft.push_str("  }\n");
        nft.push_str(
            "  chain postrouting { type nat hook postrouting priority srcnat; policy accept;\n",
        );
        for ip in &source_ips {
            nft.push_str(&format!(
                "    oifname \"{uplink_if}\" ip saddr {ip}/32 masquerade\n"
            ));
        }
        nft.push_str("  }\n}\n");

        Ok(LinuxGatewayPlan {
            sysctl_argv: vec![vec![
                "sysctl".into(),
                "-w".into(),
                "net.ipv4.ip_forward=1".into(),
            ]],
            nft_script: nft,
        })
    }
}

fn validate_iface(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 15
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(MeshError::InvalidState(format!(
            "invalid Linux interface name {value:?}"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinuxGatewayPlan {
    pub sysctl_argv: Vec<Vec<String>>,
    pub nft_script: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_POOL, MeshState};

    #[test]
    fn exit_plan_is_scoped_to_allowed_nodes() {
        let mut mesh =
            MeshState::new(DEFAULT_POOL.parse().unwrap(), "gateway", "gw", "key").unwrap();
        let laptop = mesh.add_node("laptop", "laptop", "key2", false).unwrap();
        mesh.add_node("guest", "guest", "key3", false).unwrap();
        let mut policies = GatewayPolicies::default();
        policies.enable_exit(&mesh, "gateway", ["laptop"]).unwrap();
        assert!(policies.can_use_exit("laptop", "gateway"));
        assert!(!policies.can_use_exit("guest", "gateway"));
        let plan = policies
            .linux_plan(&mesh, "gateway", "fluxmesh0", "enp1s0")
            .unwrap();
        assert!(plan.nft_script.contains(&format!("{laptop}/32")));
        assert!(
            !plan
                .nft_script
                .contains(&format!("{}/32", mesh.node("guest").unwrap().overlay_ip))
        );
        assert!(plan.nft_script.contains("masquerade"));
    }
}
