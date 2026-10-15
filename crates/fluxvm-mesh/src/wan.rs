// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Multi-WAN health scoring and Linux policy-routing plans.

use crate::public::WanConfig;
use crate::{MeshError, Result};
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WanHealth {
    pub successes: u32,
    pub failures: u32,
    pub best_rtt_ms: u32,
    pub healthy: bool,
}
#[derive(Clone, Debug, Default)]
pub struct WanState {
    pub health: BTreeMap<String, WanHealth>,
    pub active: Option<String>,
}

impl WanState {
    pub fn probe(&mut self, links: &[WanConfig], timeout: Duration) {
        for link in links {
            let mut ok = 0u32;
            let mut fail = 0u32;
            let mut best = u32::MAX;
            for target in &link.probe_targets {
                let started = Instant::now();
                match TcpStream::connect_timeout(target, timeout) {
                    Ok(_) => {
                        ok += 1;
                        best = best.min(started.elapsed().as_millis().min(u32::MAX as u128) as u32)
                    }
                    Err(_) => fail += 1,
                }
            }
            let healthy = link.probe_targets.is_empty() || ok > 0;
            self.health.insert(
                link.name.clone(),
                WanHealth {
                    successes: ok,
                    failures: fail,
                    best_rtt_ms: if best == u32::MAX { 0 } else { best },
                    healthy,
                },
            );
        }
    }
    pub fn select<'a>(&mut self, links: &'a [WanConfig]) -> Option<&'a WanConfig> {
        let mut candidates = links
            .iter()
            .filter(|l| self.health.get(&l.name).is_none_or(|h| h.healthy))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|l| {
            (
                !l.preferred,
                l.metric,
                self.health.get(&l.name).map(|h| h.failures).unwrap_or(0),
                self.health.get(&l.name).map(|h| h.best_rtt_ms).unwrap_or(0),
                l.name.clone(),
            )
        });
        let best = *candidates.first()?;
        if let Some(active) = self.active.as_deref()
            && let Some(current) = links.iter().find(|l| l.name == active)
            && self.health.get(active).is_none_or(|h| h.healthy)
        {
            return Some(current);
        }
        self.active = Some(best.name.clone());
        Some(best)
    }
}

pub fn linux_policy_plan(links: &[WanConfig], active: &str) -> Result<Vec<Vec<String>>> {
    if links.is_empty() {
        return Ok(Vec::new());
    }
    if !links.iter().any(|l| l.name == active) {
        return Err(MeshError::InvalidState(format!(
            "active WAN {active} does not exist"
        )));
    }
    let mut out = Vec::new();
    for (i, link) in links.iter().enumerate() {
        validate_ifname(&link.interface)?;
        let table = 110 + i as u32;
        if let Some(gw) = link.gateway {
            out.push(vec![
                "ip".into(),
                "route".into(),
                "replace".into(),
                "default".into(),
                "via".into(),
                gw.to_string(),
                "dev".into(),
                link.interface.clone(),
                "table".into(),
                table.to_string(),
                "metric".into(),
                link.metric.to_string(),
            ]);
        } else {
            out.push(vec![
                "ip".into(),
                "route".into(),
                "replace".into(),
                "default".into(),
                "dev".into(),
                link.interface.clone(),
                "table".into(),
                table.to_string(),
                "metric".into(),
                link.metric.to_string(),
            ]);
        }
        let priority = 20_000 + i as u32;
        out.push(vec![
            "ip".into(),
            "rule".into(),
            "del".into(),
            "priority".into(),
            priority.to_string(),
        ]);
        out.push(vec![
            "ip".into(),
            "rule".into(),
            "add".into(),
            "priority".into(),
            priority.to_string(),
            "fwmark".into(),
            format!("0x{:x}", table),
            "lookup".into(),
            table.to_string(),
        ]);
    }
    let selected = links.iter().position(|l| l.name == active).unwrap();
    let table = 110 + selected as u32;
    out.push(vec![
        "ip".into(),
        "rule".into(),
        "del".into(),
        "priority".into(),
        "100".into(),
    ]);
    out.push(vec![
        "ip".into(),
        "rule".into(),
        "add".into(),
        "priority".into(),
        "100".into(),
        "lookup".into(),
        table.to_string(),
    ]);
    Ok(out)
}
fn validate_ifname(v: &str) -> Result<()> {
    if v.is_empty()
        || v.len() > 15
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(MeshError::InvalidState(format!(
            "invalid WAN interface {v:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::public::WanConfig;
    #[test]
    fn preferred_wan_wins_initially() {
        let links = vec![
            WanConfig {
                name: "backup".into(),
                interface: "eth1".into(),
                gateway: None,
                metric: 100,
                preferred: false,
                probe_targets: vec![],
            },
            WanConfig {
                name: "fiber".into(),
                interface: "eth0".into(),
                gateway: None,
                metric: 10,
                preferred: true,
                probe_targets: vec![],
            },
        ];
        let mut s = WanState::default();
        assert_eq!(s.select(&links).unwrap().name, "fiber");
        assert!(!linux_policy_plan(&links, "fiber").unwrap().is_empty());
    }
}
