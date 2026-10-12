// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Fail-closed personal-cloud ACLs. These rules are control-plane intent; a
//! runtime can compile them to Network Fabric, nftables, or a userspace tunnel.

use crate::{Ipv4Prefix, MeshError, Result};
use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclAction {
    Allow,
    Deny,
}

impl AclAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

impl FromStr for AclAction {
    type Err = MeshError;
    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            _ => Err(MeshError::InvalidState(format!(
                "invalid ACL action {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AclProtocol {
    Any,
    Tcp,
    Udp,
    Icmp,
}

impl AclProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Icmp => "icmp",
        }
    }
}

impl FromStr for AclProtocol {
    type Err = MeshError;
    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "any" | "*" => Ok(Self::Any),
            "tcp" => Ok(Self::Tcp),
            "udp" => Ok(Self::Udp),
            "icmp" => Ok(Self::Icmp),
            _ => Err(MeshError::InvalidState(format!(
                "invalid ACL protocol {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceSelector {
    Any,
    Node(String),
}

impl fmt::Display for SourceSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("any"),
            Self::Node(id) => write!(f, "node:{id}"),
        }
    }
}

impl FromStr for SourceSelector {
    type Err = MeshError;
    fn from_str(value: &str) -> Result<Self> {
        if value == "any" || value == "*" {
            return Ok(Self::Any);
        }
        if let Some(id) = value.strip_prefix("node:") {
            if id.is_empty() {
                return Err(MeshError::InvalidState("empty source node selector".into()));
            }
            return Ok(Self::Node(id.to_owned()));
        }
        Err(MeshError::InvalidState(format!(
            "source selector must be any or node:<id>, got {value}"
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DestinationSelector {
    Any,
    Node(String),
    Service(String),
    Cidr(Ipv4Prefix),
}

impl fmt::Display for DestinationSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("any"),
            Self::Node(id) => write!(f, "node:{id}"),
            Self::Service(name) => write!(f, "service:{name}"),
            Self::Cidr(cidr) => write!(f, "cidr:{cidr}"),
        }
    }
}

impl FromStr for DestinationSelector {
    type Err = MeshError;
    fn from_str(value: &str) -> Result<Self> {
        if value == "any" || value == "*" {
            return Ok(Self::Any);
        }
        if let Some(id) = value.strip_prefix("node:") {
            if id.is_empty() {
                return Err(MeshError::InvalidState(
                    "empty destination node selector".into(),
                ));
            }
            return Ok(Self::Node(id.to_owned()));
        }
        if let Some(name) = value.strip_prefix("service:") {
            if name.is_empty() {
                return Err(MeshError::InvalidState("empty service selector".into()));
            }
            return Ok(Self::Service(name.to_owned()));
        }
        if let Some(cidr) = value.strip_prefix("cidr:") {
            return Ok(Self::Cidr(cidr.parse()?));
        }
        Err(MeshError::InvalidState(format!(
            "destination selector must be any, node:<id>, service:<name>, or cidr:<prefix>, got {value}"
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AclRule {
    pub id: String,
    pub priority: u16,
    pub action: AclAction,
    pub source: SourceSelector,
    pub destination: DestinationSelector,
    pub protocol: AclProtocol,
    /// Inclusive destination port. None means every port. ICMP ignores it.
    pub port: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AclContext<'a> {
    pub source_node: &'a str,
    pub destination_node: Option<&'a str>,
    pub destination_service: Option<&'a str>,
    pub destination_ip: Option<Ipv4Addr>,
    pub protocol: AclProtocol,
    pub port: Option<u16>,
}

pub fn evaluate(rules: &[AclRule], ctx: &AclContext<'_>) -> AclAction {
    let mut sorted: Vec<&AclRule> = rules.iter().collect();
    sorted.sort_by_key(|r| (r.priority, r.id.clone()));
    for rule in sorted {
        if matches_rule(rule, ctx) {
            return rule.action;
        }
    }
    // Personal-cloud traffic is private but still fail-closed once ACLs are in
    // use. Operators must explicitly add the desired allow rule.
    AclAction::Deny
}

fn matches_rule(rule: &AclRule, ctx: &AclContext<'_>) -> bool {
    let source = match &rule.source {
        SourceSelector::Any => true,
        SourceSelector::Node(id) => id == ctx.source_node,
    };
    if !source {
        return false;
    }
    let destination = match &rule.destination {
        DestinationSelector::Any => true,
        DestinationSelector::Node(id) => ctx.destination_node == Some(id.as_str()),
        DestinationSelector::Service(name) => ctx.destination_service == Some(name.as_str()),
        DestinationSelector::Cidr(cidr) => ctx.destination_ip.is_some_and(|ip| cidr.contains(ip)),
    };
    if !destination {
        return false;
    }
    if rule.protocol != AclProtocol::Any && rule.protocol != ctx.protocol {
        return false;
    }
    if matches!(ctx.protocol, AclProtocol::Icmp) {
        return rule.port.is_none();
    }
    match rule.port {
        None => true,
        Some(port) => ctx.port == Some(port),
    }
}

pub fn validate_rule(rule: &AclRule) -> Result<()> {
    if rule.id.is_empty()
        || rule.id.len() > 63
        || !rule
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(MeshError::InvalidState(format!(
            "invalid ACL id {:?}",
            rule.id
        )));
    }
    if matches!(rule.protocol, AclProtocol::Icmp) && rule.port.is_some() {
        return Err(MeshError::InvalidState(
            "ICMP ACL rules may not specify a port".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acl_is_priority_ordered_and_default_deny() {
        let rules = vec![
            AclRule {
                id: "allow-photos".into(),
                priority: 100,
                action: AclAction::Allow,
                source: "node:laptop".parse().unwrap(),
                destination: "service:photos".parse().unwrap(),
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
        let allowed = AclContext {
            source_node: "laptop",
            destination_node: None,
            destination_service: Some("photos"),
            destination_ip: None,
            protocol: AclProtocol::Tcp,
            port: Some(443),
        };
        assert_eq!(evaluate(&rules, &allowed), AclAction::Allow);
        let denied = AclContext {
            port: Some(22),
            ..allowed
        };
        assert_eq!(evaluate(&rules, &denied), AclAction::Deny);
        assert_eq!(evaluate(&[], &allowed), AclAction::Deny);
    }
}
