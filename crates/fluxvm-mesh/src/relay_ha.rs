// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Relay HA and geo-aware selection.

use crate::relay::{RelayRecord, RelayRegistry};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayTelemetry {
    pub region: String,
    pub rtt_ms: u32,
    pub failures: u32,
    pub capacity_percent: u8,
}

#[derive(Clone, Debug, Default)]
pub struct RelaySelector {
    pub telemetry: BTreeMap<String, RelayTelemetry>,
    pub preferred_regions: Vec<String>,
}

impl RelaySelector {
    pub fn choose<'a>(&self, registry: &'a RelayRegistry) -> Option<&'a RelayRecord> {
        registry
            .relays
            .values()
            .filter(|r| r.healthy)
            .min_by_key(|relay| {
                let t = self.telemetry.get(&relay.id);
                let region_rank =
                    t.and_then(|v| self.preferred_regions.iter().position(|r| r == &v.region))
                        .unwrap_or(self.preferred_regions.len() + 1) as u32;
                let rtt = t.map(|v| v.rtt_ms).unwrap_or(u32::MAX / 4);
                let failures = t.map(|v| v.failures).unwrap_or(1000);
                let capacity = t.map(|v| v.capacity_percent as u32).unwrap_or(100);
                (
                    region_rank,
                    failures,
                    capacity,
                    rtt,
                    relay.priority,
                    relay.id.clone(),
                )
            })
    }

    pub fn ordered<'a>(&self, registry: &'a RelayRegistry) -> Vec<&'a RelayRecord> {
        let mut out = registry
            .relays
            .values()
            .filter(|r| r.healthy)
            .collect::<Vec<_>>();
        out.sort_by_key(|relay| {
            let t = self.telemetry.get(&relay.id);
            let region_rank = t
                .and_then(|v| self.preferred_regions.iter().position(|r| r == &v.region))
                .unwrap_or(self.preferred_regions.len() + 1) as u32;
            (
                region_rank,
                t.map(|v| v.failures).unwrap_or(1000),
                t.map(|v| v.capacity_percent as u32).unwrap_or(100),
                t.map(|v| v.rtt_ms).unwrap_or(u32::MAX / 4),
                relay.priority,
                relay.id.clone(),
            )
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{RelayRecord, RelayTransport};

    #[test]
    fn prefers_region_then_health_score() {
        let mut registry = RelayRegistry::default();
        registry
            .upsert(RelayRecord {
                id: "eu".into(),
                endpoint: "192.0.2.1:443".parse().unwrap(),
                transport: RelayTransport::Tcp443,
                priority: 10,
                load_percent: 20,
                healthy: true,
            })
            .unwrap();
        registry
            .upsert(RelayRecord {
                id: "in".into(),
                endpoint: "192.0.2.2:443".parse().unwrap(),
                transport: RelayTransport::Tcp443,
                priority: 10,
                load_percent: 40,
                healthy: true,
            })
            .unwrap();
        let mut selector = RelaySelector {
            preferred_regions: vec!["in-west".into()],
            ..Default::default()
        };
        selector.telemetry.insert(
            "eu".into(),
            RelayTelemetry {
                region: "eu".into(),
                rtt_ms: 40,
                failures: 0,
                capacity_percent: 10,
            },
        );
        selector.telemetry.insert(
            "in".into(),
            RelayTelemetry {
                region: "in-west".into(),
                rtt_ms: 80,
                failures: 0,
                capacity_percent: 20,
            },
        );
        assert_eq!(selector.choose(&registry).unwrap().id, "in");
    }
}
