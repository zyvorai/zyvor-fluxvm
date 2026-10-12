// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Multi-home endpoint failover with simple hysteresis.

use crate::{Endpoint, EndpointKind};
use std::collections::BTreeMap;
use std::net::SocketAddr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointHealth {
    pub failures: u32,
    pub successes: u32,
    pub rtt_ms: u32,
    pub last_ok_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct MultiHomeState {
    pub health: BTreeMap<SocketAddr, EndpointHealth>,
    pub active: Option<SocketAddr>,
}

impl MultiHomeState {
    pub fn select(&mut self, endpoints: &[Endpoint]) -> Option<Endpoint> {
        let mut candidates = endpoints.to_vec();
        candidates.sort_by_key(|e| {
            let h = self.health.get(&e.addr);
            (
                h.map(|v| v.failures).unwrap_or(0),
                e.priority,
                h.map(|v| v.rtt_ms).unwrap_or(u32::MAX / 4),
                endpoint_rank(e.kind),
                e.addr,
            )
        });
        let best = candidates.first()?.clone();
        if let Some(active) = self.active {
            if let Some(current) = candidates.iter().find(|e| e.addr == active) {
                let cur = self.health.get(&active);
                let best_h = self.health.get(&best.addr);
                let current_bad = cur.is_some_and(|v| v.failures >= 3);
                let meaningfully_better = match (cur, best_h) {
                    (Some(a), Some(b)) => {
                        b.failures < a.failures || b.rtt_ms.saturating_add(20) < a.rtt_ms
                    }
                    (None, Some(_)) => false,
                    _ => best.priority < current.priority,
                };
                if !current_bad && !meaningfully_better {
                    return Some(current.clone());
                }
            }
        }
        self.active = Some(best.addr);
        Some(best)
    }
}

fn endpoint_rank(kind: EndpointKind) -> u8 {
    match kind {
        EndpointKind::Lan => 0,
        EndpointKind::PublicIpv6 => 1,
        EndpointKind::PublicUdp => 2,
        EndpointKind::PeerRelay => 3,
        EndpointKind::Relay443 => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_current_path_until_it_is_bad() {
        let a = Endpoint {
            kind: EndpointKind::PublicUdp,
            addr: "198.51.100.1:51820".parse().unwrap(),
            priority: 30,
        };
        let b = Endpoint {
            kind: EndpointKind::PublicUdp,
            addr: "198.51.100.2:51820".parse().unwrap(),
            priority: 20,
        };
        let mut state = MultiHomeState {
            active: Some(a.addr),
            ..Default::default()
        };
        state.health.insert(
            a.addr,
            EndpointHealth {
                failures: 0,
                successes: 10,
                rtt_ms: 40,
                last_ok_ms: 1,
            },
        );
        state.health.insert(
            b.addr,
            EndpointHealth {
                failures: 0,
                successes: 10,
                rtt_ms: 30,
                last_ok_ms: 1,
            },
        );
        assert_eq!(state.select(&[a.clone(), b.clone()]).unwrap().addr, a.addr);
        state.health.get_mut(&a.addr).unwrap().failures = 3;
        assert_eq!(
            state.select(&[a, b]).unwrap().addr,
            "198.51.100.2:51820".parse::<SocketAddr>().unwrap()
        );
    }
}
