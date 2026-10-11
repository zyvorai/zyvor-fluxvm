// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Fork and branch-start plans: what a request would be refused for and what it is expected to cost, without doing it.
//!
//! The refusals come from the same checks the real call runs (`check_vz_forkable`, `check_fork_network_mode`,
//! `admit_fork_batch`, name validation), so a plan that says `ok` is not a promise but a plan that says it would be
//! refused is accurate. The prediction is a **bound from measurements**, never a model: memory is a low and a high
//! figure, and the time estimate comes from forks recorded on this Mac (it is absent until there is one).

use crate::fork::{ForkSpec, MAX_FORK_COUNT};
use crate::{VmManager, validate_vm_name};
use anyhow::Result;
use fluxvm_core::model::{BackendKind, VmRecord};
use serde::Serialize;
use std::sync::Arc;
use uuid::Uuid;

/// The resident size of a freshly restored child, measured on an M4 right after the restore (an idle 2 GiB Debian
/// guest: 260 MiB). It **grows** as the guest faults its pages back in: the same children held about 700 MiB each twelve
/// seconds later, close to their parent's 735 MiB. So it is only the last-resort floor, used when neither fork children
/// nor the parent can be measured.
const FRESH_RESTORE_RSS_MIB: u64 = 256;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PlanRefusal {
    /// `count`, `parallelism`, `hooks`, `forkable`, `network`, `quota`, `name`.
    pub code: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Predicted {
    /// Extra resident memory the children are expected to add, lower and upper bound, MiB.
    pub extra_rss_mib_low: u64,
    pub extra_rss_mib_high: u64,
    /// Where the low bound came from.
    pub extra_rss_basis: String,
    /// Logical size of the disk each child clones (APFS clones share blocks until written), bytes.
    pub disk_logical_bytes_each: Option<u64>,
    /// Estimated wall time, milliseconds; absent when no fork has been recorded on this Mac.
    pub est_total_ms: Option<u64>,
    pub est_basis: Option<String>,
    pub host_available_mib: Option<u64>,
    /// Available memory left if the children cost the low / high bound (negative: over-committed).
    pub host_available_after_low_mib: Option<i64>,
    pub host_available_after_high_mib: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ForkPlan {
    pub ok: bool,
    pub count: u32,
    pub parallelism: u8,
    pub refusals: Vec<PlanRefusal>,
    pub predicted: Predicted,
}

/// Memory bounds. Per-child low bound: the larger of the source's running fork children's average resident size and the
/// source's own (a restored child faults its pages back in and converges towards its parent's working set, so young
/// children understate it), capped at the guest's memory; with neither measurable, a fresh-restore floor. The high bound
/// is every child touching all of its memory.
pub fn memory_bounds(
    guest_mib: u64,
    count: u32,
    children_mib: Option<u64>,
    parent_mib: Option<u64>,
) -> (u64, u64, String) {
    let (per, basis) = match (children_mib, parent_mib) {
        (Some(c), Some(p)) if c >= p => (
            c,
            format!(
                "this source's running fork children ({c} MiB each, above the source's own {p} MiB)"
            ),
        ),
        (c, Some(p)) => (
            p,
            match c {
                Some(c) => format!(
                    "the source's own resident size ({p} MiB); its fork children hold {c} MiB so far and converge towards it as they fault pages back in"
                ),
                None => format!(
                    "the source's own resident size ({p} MiB): children converge towards it as they fault pages back in"
                ),
            },
        ),
        (Some(c), None) => (
            c,
            format!("this source's running fork children ({c} MiB each)"),
        ),
        (None, None) => (
            FRESH_RESTORE_RSS_MIB,
            format!(
                "{FRESH_RESTORE_RSS_MIB} MiB per child, the size right after a restore; it grows towards the source's working set"
            ),
        ),
    };
    let per_low = per.min(guest_mib);
    (
        per_low * u64::from(count),
        guest_mib * u64::from(count),
        basis,
    )
}

/// `parent_snapshot_ms + ceil(count / parallelism) * per_child_total_ms`, from recorded `vm.fork` /
/// `vm.branch.start` events: the medians of their parent hold and per-child totals. `None` without a usable record.
pub fn estimate_ms(
    events: &[crate::events::VmEvent],
    count: u32,
    parallelism: u8,
) -> Option<(u64, String)> {
    let mut parent: Vec<u64> = Vec::new();
    let mut child: Vec<u64> = Vec::new();
    let mut runs = 0usize;
    for e in events {
        if e.event != "vm.fork" && e.event != "vm.branch.start" {
            continue;
        }
        let Some(report) = e.fields.get("report") else {
            continue;
        };
        let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(report) else {
            continue;
        };
        let totals: Vec<u64> = rows.iter().filter_map(|r| r["total_ms"].as_u64()).collect();
        if totals.is_empty() {
            continue;
        }
        runs += 1;
        child.extend(totals);
        if let Some(p) = e
            .fields
            .get("parent_snapshot_ms")
            .and_then(|v| v.parse::<u64>().ok())
        {
            parent.push(p);
        }
    }
    if child.is_empty() {
        return None;
    }
    let median = |v: &mut Vec<u64>| {
        v.sort_unstable();
        v[v.len() / 2]
    };
    let per_child = median(&mut child);
    let parent_ms = if parent.is_empty() {
        0
    } else {
        median(&mut parent)
    };
    let waves = u64::from(count).div_ceil(u64::from(parallelism.max(1)));
    Some((
        parent_ms + waves * per_child,
        format!("median of {runs} recorded fork(s) on this Mac"),
    ))
}

impl VmManager {
    /// What `POST /v1/vms/{id}/fork` with `spec` would be refused for, and what it would cost.
    pub async fn plan_fork(self: &Arc<Self>, id: Uuid, spec: ForkSpec) -> Result<ForkPlan> {
        let src = self.get(id).await?;
        Ok(self.plan_for(&src, None, &spec, &src.name, false).await)
    }

    /// What `POST /v1/branches/{id}/start` with `spec` would be refused for, and what it would cost.
    pub async fn plan_branch_start(self: &Arc<Self>, id: Uuid, spec: ForkSpec) -> Result<ForkPlan> {
        let (template, name) = self.branch_template(id)?;
        Ok(self
            .plan_for(&template, Some(name.clone()), &spec, &name, true)
            .await)
    }

    async fn plan_for(
        self: &Arc<Self>,
        src: &VmRecord,
        prefix_default: Option<String>,
        spec: &ForkSpec,
        display: &str,
        from_branch: bool,
    ) -> ForkPlan {
        let mut refusals: Vec<PlanRefusal> = Vec::new();
        let mut refuse = |code: &'static str, e: anyhow::Error| {
            refusals.push(PlanRefusal {
                code,
                reason: format!("{e:#}"),
            });
        };
        if spec.count == 0 || spec.count > MAX_FORK_COUNT {
            refuse(
                "count",
                anyhow::anyhow!("count must be between 1 and {MAX_FORK_COUNT}"),
            );
        }
        if let Some(p) = spec.parallelism
            && !(1..=MAX_FORK_COUNT as u8).contains(&p)
        {
            refuse(
                "parallelism",
                anyhow::anyhow!("parallelism must be between 1 and {MAX_FORK_COUNT}"),
            );
        }
        if let Err(e) = spec.post() {
            refuse("hooks", e);
        }
        if src.backend == BackendKind::Vz || from_branch {
            // Counts are reported once above; the rest of the checks see a count they accept.
            let count = spec.count.clamp(1, MAX_FORK_COUNT);
            if let Err(e) = crate::fork::check_vz_forkable(src, count) {
                refuse("forkable", e);
            }
            if let Err(e) = crate::fork::check_fork_network_mode(src, spec.network) {
                refuse("network", e);
            }
        } else if let Err(e) = crate::fork::check_forkable(src, spec.count.clamp(1, MAX_FORK_COUNT))
        {
            refuse("forkable", e);
        }
        if spec.count >= 1
            && spec.count <= MAX_FORK_COUNT
            && let Err(e) = self.admit_fork_batch(src, spec.count).await
        {
            refuse("quota", e);
        }
        let prefix = spec
            .name_prefix
            .clone()
            .or(prefix_default)
            .unwrap_or_else(|| format!("{display}-fork"));
        for n in 1..=spec.count.clamp(1, MAX_FORK_COUNT) {
            if let Err(e) = validate_vm_name(&format!("{prefix}-{n}")) {
                refuse("name", e);
                break;
            }
        }

        let count = spec.count.clamp(1, MAX_FORK_COUNT);
        let parallelism = spec
            .parallelism
            .unwrap_or_else(|| count.min(8) as u8)
            .clamp(1, MAX_FORK_COUNT as u8);
        let predicted = self.predict(src, count, parallelism).await;
        ForkPlan {
            ok: refusals.is_empty(),
            count: spec.count,
            parallelism,
            refusals,
            predicted,
        }
    }

    async fn predict(self: &Arc<Self>, src: &VmRecord, count: u32, parallelism: u8) -> Predicted {
        let all = self.list().await;
        let siblings: Vec<VmRecord> = all
            .into_iter()
            .filter(|v| {
                v.labels
                    .get(crate::fork::FORKED_FROM_LABEL)
                    .is_some_and(|f| f == &src.id.to_string())
            })
            .collect();
        let parent = src.clone();
        let ((children, parent_rss), host) = tokio::task::spawn_blocking(move || {
            let mib = |b: u64| b / 1_048_576;
            let rows = crate::vz_devices::runner_usage_samples(&siblings);
            let children = (!rows.is_empty())
                .then(|| mib(rows.iter().map(|r| r.rss_bytes).sum::<u64>() / rows.len() as u64));
            let parent_rss = crate::vz_devices::runner_usage_samples(std::slice::from_ref(&parent))
                .first()
                .map(|r| mib(r.rss_bytes));
            (
                (children, parent_rss),
                fluxvm_core::pressure_admission::sample_host(),
            )
        })
        .await
        .unwrap_or(((None, None), Default::default()));
        let (low, high, basis) = memory_bounds(src.request.memory_mib, count, children, parent_rss);
        let est = crate::events::global()
            .and_then(|l| {
                l.list(&crate::events::EventFilter {
                    event: Some("vm.".into()),
                    ..Default::default()
                })
                .ok()
            })
            .and_then(|ev| estimate_ms(&ev, count, parallelism));
        let disk = std::fs::metadata(&src.disk).ok().map(|m| m.len());
        let avail = host.mem_available_mib;
        Predicted {
            extra_rss_mib_low: low,
            extra_rss_mib_high: high,
            extra_rss_basis: basis,
            disk_logical_bytes_each: disk,
            est_total_ms: est.as_ref().map(|e| e.0),
            est_basis: est.map(|e| e.1),
            host_available_mib: avail,
            host_available_after_low_mib: avail.map(|a| a as i64 - low as i64),
            host_available_after_high_mib: avail.map(|a| a as i64 - high as i64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::VmEvent;
    use chrono::Utc;
    use std::collections::BTreeMap;

    fn ev(kind: &str, parent: &str, totals: &[u64]) -> VmEvent {
        let rows: Vec<serde_json::Value> = totals
            .iter()
            .map(|t| serde_json::json!({"total_ms": t}))
            .collect();
        VmEvent {
            ts: Utc::now(),
            event: kind.into(),
            vm_id: None,
            fields: BTreeMap::from([
                ("parent_snapshot_ms".to_string(), parent.to_string()),
                (
                    "report".to_string(),
                    serde_json::Value::Array(rows).to_string(),
                ),
            ]),
        }
    }

    #[test]
    fn memory_bounds_use_the_floor_cap_at_the_guest_and_scale_with_count() {
        let (low, high, basis) = memory_bounds(2048, 4, None, None);
        assert_eq!((low, high), (4 * 256, 4 * 2048));
        assert!(basis.contains("right after a restore"));
        // A guest smaller than the floor is capped at its own memory.
        let (low, high, _) = memory_bounds(128, 2, None, None);
        assert_eq!((low, high), (256, 256));
        // The parent's resident size beats young children, which understate where they settle.
        let (low, _, basis) = memory_bounds(2048, 3, Some(221), Some(735));
        assert_eq!(low, 3 * 735);
        assert!(basis.contains("735") && basis.contains("221") && basis.contains("converge"));
        // Children that have grown past the parent win.
        let (low, _, basis) = memory_bounds(2048, 3, Some(900), Some(735));
        assert_eq!(low, 2700);
        assert!(basis.contains("above the source's own"));
        // Either alone is used as it is.
        assert_eq!(memory_bounds(2048, 2, None, Some(500)).0, 1000);
        assert_eq!(memory_bounds(2048, 2, Some(400), None).0, 800);
        // Never above what the guest could use.
        assert_eq!(memory_bounds(512, 2, None, Some(900)).0, 1024);
    }

    #[test]
    fn the_estimate_needs_a_record_and_uses_waves_of_the_parallelism() {
        assert!(estimate_ms(&[], 4, 4).is_none());
        // An event without a report (an old fork) gives nothing.
        let mut old = ev("vm.fork", "300", &[]);
        old.fields.remove("report");
        assert!(estimate_ms(&[old], 4, 4).is_none());
        let events = vec![
            ev("vm.fork", "350", &[1500, 1700]),
            ev("vm.branch.start", "0", &[1600]),
        ];
        // Medians: per-child 1600 (of 1500, 1700, 1600), parent 350 or 0 (median of [350, 0] = 350).
        let (ms, basis) = estimate_ms(&events, 4, 2).unwrap();
        assert_eq!(ms, 350 + 2 * 1600);
        assert!(basis.contains("2 recorded"));
        // Fully parallel: one wave.
        assert_eq!(estimate_ms(&events, 4, 8).unwrap().0, 350 + 1600);
    }
}
