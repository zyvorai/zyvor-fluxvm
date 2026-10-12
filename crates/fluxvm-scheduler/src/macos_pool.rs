// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A pool of at most two warm, slot-bound macOS guests.
//!
//! Apple allows two macOS guests to run at once per Mac, and a macOS guest's saved state is bound to one machine
//! identifier and one auxiliary storage, so a macOS guest cannot be forked or branched ([`crate::fork`]). A slot is
//! therefore an ordinary stopped macOS VM, set up once by hand (Setup Assistant, auto-login), with a `warm` snapshot
//! taken while it was running. Unlike [`crate::sandbox_pool`], a slot is not consumed:
//!
//! - **checkout** restores the slot's `warm` snapshot. `start_from_snapshot` puts the snapshot's disk and auxiliary
//!   storage back before restoring memory, so every checkout starts from the same pristine state;
//! - **return** stops the slot. Nothing the guest wrote is kept, and the next checkout restores `warm` again.
//!
//! Restoring needs an unlocked login session on the Mac (`docs/macos.md`), and a restore that fails is an error: there
//! is no cold-boot fallback, since a macOS cold boot takes minutes and ends at the lock screen.

use crate::VmManager;
use crate::apple_placement::MAX_MACOS_GUESTS;
use anyhow::{Context, Result, bail};
use fluxvm_core::model::{AppleGuest, BackendKind, VmPatch, VmRecord, VmStatus};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

pub const POOL_LABEL: &str = "fluxvm.pool";
pub const POOL_VALUE: &str = "macos";
/// `idle` (stopped, free to check out) or `leased` (checked out; running or being returned).
pub const STATE_LABEL: &str = "fluxvm.pool.state";
const IDLE: &str = "idle";
const LEASED: &str = "leased";
pub const WARM_TAG: &str = "warm";

/// Serialises enrolling and checking out, so two callers never take the same slot.
static POOL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn is_macos_vz(vm: &VmRecord) -> bool {
    vm.backend == BackendKind::Vz
        && vm
            .request
            .apple
            .as_ref()
            .is_some_and(|a| a.guest_os == AppleGuest::Macos)
}

pub(crate) fn is_slot(vm: &VmRecord) -> bool {
    is_macos_vz(vm) && vm.labels.get(POOL_LABEL).map(String::as_str) == Some(POOL_VALUE)
}

fn state(vm: &VmRecord) -> Option<&str> {
    vm.labels.get(STATE_LABEL).map(String::as_str)
}

/// The first slot that can be checked out: marked idle and actually stopped.
pub(crate) fn pick_idle(vms: &[VmRecord]) -> Option<&VmRecord> {
    vms.iter()
        .find(|v| is_slot(v) && state(v) == Some(IDLE) && v.status == VmStatus::Stopped)
}

fn label(state: &str) -> BTreeMap<String, Option<String>> {
    BTreeMap::from([
        (POOL_LABEL.to_owned(), Some(POOL_VALUE.to_owned())),
        (STATE_LABEL.to_owned(), Some(state.to_owned())),
    ])
}

impl VmManager {
    /// Turns a prepared, running macOS guest into a pool slot: saves its `warm` snapshot, stops it and marks it idle.
    /// At most [`MAX_MACOS_GUESTS`] slots exist; more could never run at once.
    pub async fn macos_pool_enroll(self: &Arc<Self>, id: Uuid) -> Result<VmRecord> {
        let _guard = POOL_LOCK.lock().await;
        let vm = self.get(id).await?;
        if !is_macos_vz(&vm) {
            bail!("only a vz macOS guest can join the macOS pool");
        }
        if is_slot(&vm) {
            bail!("VM {id} is already a pool slot");
        }
        if !matches!(vm.status, VmStatus::Running | VmStatus::Paused) {
            bail!(
                "enrol a running guest that is logged in and set up (status={:?}): its state is saved as the pristine copy",
                vm.status
            );
        }
        let slots = self.list().await.iter().filter(|v| is_slot(v)).count();
        if slots >= MAX_MACOS_GUESTS as usize {
            bail!(
                "the macOS pool already has {slots} slots (Apple allows {MAX_MACOS_GUESTS} macOS guests at once)"
            );
        }
        self.create_vm_snapshot(id, WARM_TAG)
            .await
            .context("saving the warm snapshot")?;
        self.stop(id).await.context("stopping the new slot")?;
        let vm = self
            .patch(
                id,
                VmPatch {
                    name: None,
                    labels: label(IDLE),
                },
            )
            .await?;
        crate::audit_event("macos_pool.enroll", &[("vm_id", &id.to_string())]);
        Ok(vm)
    }

    /// Restores a free slot's `warm` snapshot and marks it leased. Fails when no slot is free, or the restore fails.
    pub async fn macos_pool_checkout(self: &Arc<Self>) -> Result<VmRecord> {
        let _guard = POOL_LOCK.lock().await;
        let all = self.list().await;
        let Some(slot) = pick_idle(&all) else {
            let slots = all.iter().filter(|v| is_slot(v)).count();
            bail!("no free macOS pool slot ({slots} enrolled); return one or enrol a guest");
        };
        let id = slot.id;
        if !self
            .list_vm_snapshots(id)
            .await?
            .iter()
            .any(|s| s.tag == WARM_TAG)
        {
            bail!("pool slot {id} has no {WARM_TAG:?} snapshot; enrol it again");
        }
        self.start_from_snapshot(id, WARM_TAG)
            .await
            .with_context(|| format!("restoring pool slot {id}"))?;
        let vm = self
            .patch(
                id,
                VmPatch {
                    name: None,
                    labels: label(LEASED),
                },
            )
            .await?;
        crate::audit_event("macos_pool.checkout", &[("vm_id", &id.to_string())]);
        Ok(vm)
    }

    /// Stops a leased slot and marks it idle. What the guest wrote is discarded by the next checkout.
    pub async fn macos_pool_return(self: &Arc<Self>, id: Uuid) -> Result<VmRecord> {
        let _guard = POOL_LOCK.lock().await;
        let vm = self.get(id).await?;
        if !is_slot(&vm) {
            bail!("VM {id} is not a macOS pool slot");
        }
        if state(&vm) != Some(LEASED) {
            bail!("pool slot {id} is not checked out");
        }
        self.stop(id).await.context("stopping the pool slot")?;
        let vm = self
            .patch(
                id,
                VmPatch {
                    name: None,
                    labels: label(IDLE),
                },
            )
            .await?;
        crate::audit_event("macos_pool.return", &[("vm_id", &id.to_string())]);
        Ok(vm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(guest: &str, status: &str, pool: Option<&str>, st: Option<&str>) -> VmRecord {
        let mut labels = serde_json::Map::new();
        if let Some(p) = pool {
            labels.insert(POOL_LABEL.into(), p.into());
        }
        if let Some(s) = st {
            labels.insert(STATE_LABEL.into(), s.into());
        }
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "name": "m", "backend": "vz", "status": status, "pid": null,
            "created_at": "2026-01-01T00:00:00Z", "expires_at": null, "workspace": "/nonexistent/w",
            "disk": "/nonexistent/w/root.raw", "seed_disk": null, "tap_name": null,
            "control_socket": null, "log_path": "/nonexistent/w/console.log", "error": null,
            "labels": labels,
            "request": {"name": "m", "image": "/img", "vcpus": 4, "memory_mib": 8192,
                        "backend": "vz", "apple": {"guest_os": guest}},
        }))
        .unwrap()
    }

    #[test]
    fn only_a_stopped_idle_macos_slot_is_picked() {
        let idle = vm("macos", "stopped", Some("macos"), Some("idle"));
        let vms = [
            vm("macos", "stopped", None, None),
            vm("macos", "running", Some("macos"), Some("leased")),
            // Marked idle but still running (e.g. a return that did not finish): not free.
            vm("macos", "running", Some("macos"), Some("idle")),
            // A Linux sandbox-pool slot is not a macOS slot.
            vm("linux", "stopped", Some("sandbox"), Some("idle")),
            idle.clone(),
        ];
        assert_eq!(pick_idle(&vms).map(|v| v.id), Some(idle.id));
        assert!(pick_idle(&vms[..4]).is_none());
    }

    #[test]
    fn slot_needs_the_macos_label_and_a_macos_guest() {
        assert!(is_slot(&vm("macos", "stopped", Some("macos"), None)));
        assert!(!is_slot(&vm("linux", "stopped", Some("macos"), None)));
        assert!(!is_slot(&vm("macos", "stopped", Some("sandbox"), None)));
    }
}
