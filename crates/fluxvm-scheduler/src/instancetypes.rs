// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Create-time glue for instancetypes and preferences (`fluxvm_core::instancetype`): resolves the
//! selectors in a create body against the host's catalog, and records what was resolved with the VM.
//!
//! The resolved values are written into the VM's own spec, so that spec is the snapshot: later edits
//! or deletes of a class change nothing for existing VMs. The snapshot file
//! (`<workspace>/instancetype.json`) and the `fluxvm.io/instancetype` / `fluxvm.io/preference` labels
//! say which class and version produced them.

use crate::VmManager;
use anyhow::{Context, Result};
use fluxvm_core::instancetype::{Catalog, ResolvedSnapshot, SNAPSHOT_FILE, resolve_body};
use fluxvm_core::model::{CreateVmRequest, VmPatch, VmRecord};
use std::path::Path;

pub const INSTANCETYPE_LABEL: &str = "fluxvm.io/instancetype";
pub const PREFERENCE_LABEL: &str = "fluxvm.io/preference";

/// Turns a create body into a request, resolving `instancetype`/`preference` first. A conflict (the
/// body sets a field its instancetype defines) is an error here, before anything is created.
pub fn resolve_create_body(
    state_dir: &Path,
    mut body: serde_json::Value,
) -> Result<(CreateVmRequest, Option<ResolvedSnapshot>)> {
    let snapshot = if body.get("instancetype").is_some() || body.get("preference").is_some() {
        resolve_body(&mut body, &Catalog::load(state_dir)?)?
    } else {
        None
    };
    let req = serde_json::from_value(body).context("invalid VM spec")?;
    Ok((req, snapshot))
}

/// Writes the snapshot into the VM's workspace and labels the VM with the classes it came from.
pub async fn record_snapshot(
    m: &VmManager,
    vm: VmRecord,
    snapshot: &ResolvedSnapshot,
) -> Result<VmRecord> {
    let path = vm.workspace.join(SNAPSHOT_FILE);
    std::fs::write(&path, serde_json::to_vec_pretty(snapshot)?)
        .with_context(|| format!("writing {}", path.display()))?;
    let mut labels = std::collections::BTreeMap::new();
    if let Some(l) = snapshot.instancetype_label() {
        labels.insert(INSTANCETYPE_LABEL.to_string(), Some(l));
    }
    if let Some(p) = &snapshot.preference {
        labels.insert(PREFERENCE_LABEL.to_string(), Some(p.name.clone()));
    }
    m.patch(vm.id, VmPatch { name: None, labels }).await
}

/// The snapshot recorded for a VM, if it was created from a class.
pub fn read_snapshot(workspace: &Path) -> Option<ResolvedSnapshot> {
    serde_json::from_slice(&std::fs::read(workspace.join(SNAPSHOT_FILE)).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bodies_resolve_into_requests() {
        let dir = tempfile::tempdir().unwrap();
        let (req, snap) = resolve_create_body(
            dir.path(),
            json!({"name": "a", "backend": "qemu", "image": "/i.qcow2", "instancetype": "large", "preference": "linux"}),
        )
        .unwrap();
        assert_eq!((req.vcpus, req.memory_mib), (4, 8192));
        assert_eq!(
            snap.unwrap().instancetype_label().as_deref(),
            Some("large:1")
        );

        // No selectors: the body is left alone and there is no snapshot.
        let (req, snap) = resolve_create_body(
            dir.path(),
            json!({"name": "a", "backend": "qemu", "image": "/i.qcow2", "vcpus": 3}),
        )
        .unwrap();
        assert_eq!(req.vcpus, 3);
        assert!(snap.is_none());

        let e = resolve_create_body(
            dir.path(),
            json!({"name": "a", "backend": "qemu", "image": "/i", "instancetype": "small", "vcpus": 3}),
        )
        .unwrap_err();
        assert!(e.to_string().contains("`vcpus`"), "{e:#}");
        assert!(
            resolve_create_body(
                dir.path(),
                json!({"name": "a", "backend": "qemu", "image": "/i", "instancetype": "nope"})
            )
            .is_err()
        );
    }

    #[test]
    fn snapshots_round_trip_through_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = json!({"instancetype": "small"});
        let snap = resolve_body(&mut body, &Catalog::builtin())
            .unwrap()
            .unwrap();
        std::fs::write(
            dir.path().join(SNAPSHOT_FILE),
            serde_json::to_vec(&snap).unwrap(),
        )
        .unwrap();
        assert_eq!(read_snapshot(dir.path()), Some(snap));
        assert!(read_snapshot(&dir.path().join("nope")).is_none());
    }
}
