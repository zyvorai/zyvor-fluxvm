// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Richer stack and Compose-style networking for sandboxes.
//!
//! Builds on the existing stack machinery in compose.rs and macos-stacks.

use anyhow::Result;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct StackService {
    pub name: String,
    pub image: String,
    pub depends_on: Vec<String>,
    pub ports: Vec<String>,
    pub networks: Vec<String>,
}

pub fn resolve_order(services: &[StackService]) -> Result<Vec<String>> {
    // Simple topological order by depends_on.
    let mut order = Vec::new();
    let mut seen = std::collections::HashSet::new();
    fn visit(
        name: &str,
        services: &[StackService],
        order: &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
    ) -> Result<()> {
        if seen.contains(name) {
            return Ok(());
        }
        if let Some(svc) = services.iter().find(|s| s.name == name) {
            for dep in &svc.depends_on {
                visit(dep, services, order, seen)?;
            }
        }
        seen.insert(name.to_string());
        order.push(name.to_string());
        Ok(())
    }
    for svc in services {
        visit(&svc.name, services, &mut order, &mut seen)?;
    }
    Ok(order)
}

pub fn hosts_for(network: &str, members: &[StackService]) -> HashMap<String, String> {
    // Placeholder: real implementation uses the private network IPAM.
    let mut hosts = HashMap::new();
    for (i, svc) in members.iter().enumerate() {
        if svc.networks.contains(&network.to_string()) {
            hosts.insert(svc.name.clone(), format!("10.89.0.{}", i + 2));
        }
    }
    hosts
}
