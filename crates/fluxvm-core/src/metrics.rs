// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Process-wide counters for Prometheus `/metrics` (atomics, no locks).

use std::sync::atomic::{AtomicU64, Ordering};

static AUTH_DENY_TOTAL: AtomicU64 = AtomicU64::new(0);
static EGRESS_DENY_TOTAL: AtomicU64 = AtomicU64::new(0);
static VM_CREATE_TOTAL: AtomicU64 = AtomicU64::new(0);
static VM_CREATE_DURATION_MS_TOTAL: AtomicU64 = AtomicU64::new(0);
static VM_START_TOTAL: AtomicU64 = AtomicU64::new(0);
static VM_START_DURATION_MS_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_FAILED_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_CHILDREN_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_DURATION_MS_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_PARENT_SNAPSHOT_MS_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_RESTORE_MS_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_IDENTITY_MS_TOTAL: AtomicU64 = AtomicU64::new(0);
static FORK_IDENTITY_FAILED_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn inc_auth_deny() {
    AUTH_DENY_TOTAL.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_egress_deny() {
    EGRESS_DENY_TOTAL.fetch_add(1, Ordering::Relaxed);
}

pub fn record_vm_create(duration_ms: u64) {
    VM_CREATE_TOTAL.fetch_add(1, Ordering::Relaxed);
    VM_CREATE_DURATION_MS_TOTAL.fetch_add(duration_ms, Ordering::Relaxed);
}

pub fn record_vm_start(duration_ms: u64) {
    VM_START_TOTAL.fetch_add(1, Ordering::Relaxed);
    VM_START_DURATION_MS_TOTAL.fetch_add(duration_ms, Ordering::Relaxed);
}

pub fn auth_deny_total() -> u64 {
    AUTH_DENY_TOTAL.load(Ordering::Relaxed)
}

pub fn egress_deny_total() -> u64 {
    EGRESS_DENY_TOTAL.load(Ordering::Relaxed)
}

pub fn vm_create_total() -> u64 {
    VM_CREATE_TOTAL.load(Ordering::Relaxed)
}

pub fn vm_create_duration_ms_total() -> u64 {
    VM_CREATE_DURATION_MS_TOTAL.load(Ordering::Relaxed)
}

pub fn vm_start_total() -> u64 {
    VM_START_TOTAL.load(Ordering::Relaxed)
}

pub fn vm_start_duration_ms_total() -> u64 {
    VM_START_DURATION_MS_TOTAL.load(Ordering::Relaxed)
}

/// One finished fork: `children` created, the parent's snapshot hold, total wall time, and the per-child restore and
/// identity sums (so a mean per child is `sum / children`).
pub fn record_fork(
    children: u64,
    parent_snapshot_ms: u64,
    total_ms: u64,
    restore_ms_sum: u64,
    identity_ms_sum: u64,
) {
    FORK_TOTAL.fetch_add(1, Ordering::Relaxed);
    FORK_CHILDREN_TOTAL.fetch_add(children, Ordering::Relaxed);
    FORK_PARENT_SNAPSHOT_MS_TOTAL.fetch_add(parent_snapshot_ms, Ordering::Relaxed);
    FORK_DURATION_MS_TOTAL.fetch_add(total_ms, Ordering::Relaxed);
    FORK_RESTORE_MS_TOTAL.fetch_add(restore_ms_sum, Ordering::Relaxed);
    FORK_IDENTITY_MS_TOTAL.fetch_add(identity_ms_sum, Ordering::Relaxed);
}

/// A fork that was refused or rolled back after it started.
pub fn record_fork_failed() {
    FORK_FAILED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// A strict identity reset that could not be confirmed (the fork it belonged to failed).
pub fn record_fork_identity_failed() {
    FORK_IDENTITY_FAILED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// `(forks, failed, children, total_ms, parent_snapshot_ms, restore_ms, identity_ms, identity_failed)`
pub fn fork_totals() -> (u64, u64, u64, u64, u64, u64, u64, u64) {
    (
        FORK_TOTAL.load(Ordering::Relaxed),
        FORK_FAILED_TOTAL.load(Ordering::Relaxed),
        FORK_CHILDREN_TOTAL.load(Ordering::Relaxed),
        FORK_DURATION_MS_TOTAL.load(Ordering::Relaxed),
        FORK_PARENT_SNAPSHOT_MS_TOTAL.load(Ordering::Relaxed),
        FORK_RESTORE_MS_TOTAL.load(Ordering::Relaxed),
        FORK_IDENTITY_MS_TOTAL.load(Ordering::Relaxed),
        FORK_IDENTITY_FAILED_TOTAL.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_counters_accumulate_by_delta() {
        // Process-wide atomics: assert on deltas, other tests may touch them.
        let before = fork_totals();
        record_fork(4, 300, 3000, 8000, 2400);
        record_fork_failed();
        record_fork_identity_failed();
        let after = fork_totals();
        assert_eq!(after.0 - before.0, 1);
        assert!(after.1 - before.1 >= 1);
        assert_eq!(after.2 - before.2, 4);
        assert_eq!(after.3 - before.3, 3000);
        assert_eq!(after.4 - before.4, 300);
        assert_eq!(after.5 - before.5, 8000);
        assert_eq!(after.6 - before.6, 2400);
        assert!(after.7 - before.7 >= 1);
    }
}
