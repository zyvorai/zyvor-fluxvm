// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A bounded, in-memory history of host memory and per-VM runner usage (`GET /v1/host/history`).
//!
//! A dashboard that connects after the daemon has been up for a while can then draw full sparklines at once instead of
//! starting empty. The ring holds the last hour at five-second spacing and is **lost when the daemon restarts**; it is
//! a convenience for viewers, not a metrics store (use `/metrics` for that).

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

/// Seconds between samples.
pub const INTERVAL_SECONDS: u64 = 5;
/// One hour of samples.
pub const CAPACITY: usize = 720;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct VmSample {
    pub vm_id: Uuid,
    pub cpu_seconds_total: f64,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Sample {
    pub ts: DateTime<Utc>,
    pub memory_available_mib: Option<u64>,
    pub memory_total_mib: Option<u64>,
    pub memory_pressure: Option<String>,
    pub vms: Vec<VmSample>,
}

#[derive(Default)]
pub struct HostHistory {
    ring: Mutex<VecDeque<Sample>>,
}

impl HostHistory {
    pub fn push(&self, s: Sample) {
        let mut r = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        r.push_back(s);
        while r.len() > CAPACITY {
            r.pop_front();
        }
    }

    /// Samples strictly after `since`, oldest first.
    pub fn since(&self, since: Option<DateTime<Utc>>) -> Vec<Sample> {
        let r = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        r.iter()
            .filter(|s| since.is_none_or(|t| s.ts > t))
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.ring.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

static GLOBAL: OnceLock<Arc<HostHistory>> = OnceLock::new();

pub fn global() -> Arc<HostHistory> {
    GLOBAL.get_or_init(Default::default).clone()
}

/// Starts the sampler once per process. Without a Tokio runtime (some unit tests build a router synchronously) it does
/// nothing. The task holds only a weak reference, so it ends when the manager is dropped.
pub fn start_sampler(manager: &Arc<crate::VmManager>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if STARTED.set(()).is_err() {
        return;
    }
    let weak = Arc::downgrade(manager);
    let history = global();
    handle.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(INTERVAL_SECONDS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(m) = weak.upgrade() else { break };
            let vms = m.list().await;
            drop(m);
            if let Ok(s) = tokio::task::spawn_blocking(move || sample_now(&vms)).await {
                history.push(s);
            }
        }
    });
}

/// One sample of the host and of every running or paused `vz` VM (blocking: reads `ps`-class data).
pub fn sample_now(vms: &[fluxvm_core::model::VmRecord]) -> Sample {
    let host = fluxvm_core::pressure_admission::sample_host();
    let usage = crate::vz_devices::runner_usage_samples(vms);
    Sample {
        ts: Utc::now(),
        memory_available_mib: host.mem_available_mib,
        memory_total_mib: host.mem_total_mib,
        memory_pressure: host
            .level
            .and_then(|l| serde_json::to_value(l).ok())
            .and_then(|v| v.as_str().map(str::to_string)),
        vms: usage
            .into_iter()
            .map(|u| VmSample {
                vm_id: u.vm_id,
                cpu_seconds_total: u.cpu_seconds,
                rss_bytes: u.rss_bytes,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Sample {
        Sample {
            ts: DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap(),
            memory_available_mib: Some(1000),
            memory_total_mib: Some(16384),
            memory_pressure: Some("normal".into()),
            vms: Vec::new(),
        }
    }

    #[test]
    fn the_ring_is_bounded_and_ordered() {
        let h = HostHistory::default();
        for i in 0..(CAPACITY as i64 + 10) {
            h.push(at(i));
        }
        let all = h.since(None);
        assert_eq!(all.len(), CAPACITY);
        assert_eq!(all[0].ts, at(10).ts, "the oldest ten were dropped");
        assert!(all.windows(2).all(|w| w[0].ts < w[1].ts));
    }

    #[test]
    fn since_is_exclusive() {
        let h = HostHistory::default();
        for i in 0..5 {
            h.push(at(i * 5));
        }
        let got = h.since(Some(at(10).ts));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ts, at(15).ts);
        assert!(h.since(Some(at(20).ts)).is_empty());
    }

    #[test]
    fn a_live_sample_has_the_host_and_no_vms_for_an_empty_list() {
        let s = sample_now(&[]);
        assert!(s.vms.is_empty());
        assert!(s.ts <= Utc::now());
    }
}
