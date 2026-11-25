// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The usage sampler: once a minute it reads the same per-VM counters as `GET /v1/stats` ([`crate::node_stats`]) and adds
//! the difference to the ledger in [`fluxvm_core::usage`]. The ledger lives in `<state_dir>/usage.json` (written every five
//! minutes and when the daemon stops being reachable by `Weak`), holds at most 90 days, and is read back at start, so a
//! restart loses at most five minutes. Nothing to turn on.

use chrono::Utc;
use fluxvm_core::model::{VmRecord, VmStatus};
use fluxvm_core::usage::{Ledger, Obs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const INTERVAL_SECONDS: u64 = 60;
const SAVE_EVERY: u32 = 5;

static GLOBAL: OnceLock<Arc<Mutex<Ledger>>> = OnceLock::new();

pub fn global() -> Arc<Mutex<Ledger>> {
    GLOBAL.get_or_init(Default::default).clone()
}

pub fn file(state_dir: &Path) -> PathBuf {
    state_dir.join("usage.json")
}

/// The saved ledger, or an empty one when there is none or it cannot be read (a damaged file must not stop the daemon).
pub fn load(state_dir: &Path) -> Ledger {
    std::fs::read(file(state_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(state_dir: &Path, l: &Ledger) -> std::io::Result<()> {
    let p = file(state_dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(l)?)?;
    std::fs::rename(&tmp, &p)
}

/// What the sampler sees, from the node stats of this tick and the VM records (for labels).
pub fn observations(stats: &crate::node_stats::NodeStats, vms: &[VmRecord]) -> Vec<Obs> {
    stats
        .vms
        .iter()
        .map(|s| {
            let rec = vms.iter().find(|v| v.id.to_string() == s.id);
            Obs {
                id: s.id.clone(),
                name: s.name.clone(),
                labels: rec.map(|v| v.labels.clone()).unwrap_or_default(),
                running: rec.is_some_and(|v| v.status == VmStatus::Running),
                memory_mib: s.memory_mib,
                cpu_seconds_total: s.cpu_seconds_total,
                disk_read_bytes_total: s.disk_read_bytes_total,
                disk_write_bytes_total: s.disk_write_bytes_total,
                net_rx_bytes_total: s.net_rx_bytes_total,
                net_tx_bytes_total: s.net_tx_bytes_total,
            }
        })
        .collect()
}

/// Starts the sampler once per process (a no-op without a Tokio runtime, like the host history). Holds only a weak
/// reference to the manager.
pub fn start_sampler(manager: &Arc<crate::VmManager>) {
    static STARTED: OnceLock<()> = OnceLock::new();
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    if STARTED.set(()).is_err() {
        return;
    }
    let dir = manager.cfg.state_dir.clone();
    let ledger = global();
    *ledger.lock().unwrap_or_else(|e| e.into_inner()) = load(&dir);
    let weak = Arc::downgrade(manager);
    handle.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(INTERVAL_SECONDS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut n = 0u32;
        loop {
            tick.tick().await;
            let Some(m) = weak.upgrade() else { break };
            let vms = m.list().await;
            drop(m);
            let d = dir.clone();
            let obs = tokio::task::spawn_blocking(move || {
                let stats = crate::node_stats::sample(&vms, &d);
                observations(&stats, &vms)
            })
            .await;
            let Ok(obs) = obs else { continue };
            let snapshot = {
                let mut l = ledger.lock().unwrap_or_else(|e| e.into_inner());
                l.observe(Utc::now(), &obs);
                n += 1;
                n.is_multiple_of(SAVE_EVERY).then(|| l.clone())
            };
            if let Some(l) = snapshot {
                let d = dir.clone();
                let _ = tokio::task::spawn_blocking(move || save(&d, &l)).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_and_loads_and_survives_a_damaged_file() {
        let d = std::env::temp_dir().join(format!("fluxvm-usage-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(load(&d).is_empty());
        let mut l = Ledger::default();
        let mk = |cpu: f64| Obs {
            id: "a".into(),
            name: "a".into(),
            running: true,
            memory_mib: 1024,
            cpu_seconds_total: Some(cpu),
            ..Default::default()
        };
        l.observe(Utc::now(), &[mk(1.0)]);
        l.observe(Utc::now() + chrono::Duration::seconds(60), &[mk(5.0)]);
        save(&d, &l).unwrap();
        assert_eq!(load(&d), l);
        std::fs::write(file(&d), b"{ not json").unwrap();
        assert!(load(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
