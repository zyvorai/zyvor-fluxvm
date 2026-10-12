// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Persistent volumes for the Mac cloud: block disks that outlive VMs.
//!
//! A volume is a sparse raw file on one Mac (`<state_dir>/volumes/<id>.raw`, made by that Mac's `fluxctl serve`, see
//! `POST /v1/volumes`), so it lives on exactly one node and a VM that uses it must run there. Central keeps the record
//! here, in [`TenancyState::volumes`]: who owns it, where it is, how big, and which VM it is attached to. A launch that
//! names volumes (`"volumes": ["data"]` on `POST /fleet/vms`) claims them, lands on their Mac, and gets each one as an
//! `apple.extra_disks` virtio disk whose serial is the volume id (`/dev/disk/by-id/virtio-vol-…` in the guest).
//! Deleting the VM detaches it ([`TenancyState::end_vm`]); the data stays.
//!
//! Pure state and rules, no HTTP: `central_volumes.rs` is the HTTP and node I/O half.

use crate::tenancy::{Caller, TenancyState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Largest volume central creates or grows to.
pub const MAX_VOLUME_GIB: u64 = 2048;
/// Most volumes one launch may attach.
pub const MAX_VOLUMES_PER_VM: usize = 8;
/// Most live volumes one tenant (or the operator) may hold, whatever the GiB quota says.
pub const MAX_VOLUMES_PER_OWNER: usize = 200;
const MAX_NAME_LEN: usize = 40;

/// A refusal, with the HTTP status central answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeError(pub u16, pub String);

impl std::fmt::Display for VolumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

fn bad(msg: impl Into<String>) -> VolumeError {
    VolumeError(400, msg.into())
}
fn conflict(msg: impl Into<String>) -> VolumeError {
    VolumeError(409, msg.into())
}
fn missing(key: &str) -> VolumeError {
    VolumeError(404, format!("no volume '{key}'"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    /// `vol-` and 12 hex characters: the file name on the Mac and the disk serial the guest sees.
    pub id: String,
    /// Unique among its owner's live volumes.
    pub name: String,
    /// The owning tenant; `None` is the operator's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// The Mac the file is on.
    pub node: String,
    pub size_gib: u64,
    pub created_at: DateTime<Utc>,
    /// The VM (id) it is attached to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_to: Option<String>,
    #[serde(default = "default_format")]
    pub format: String,
    /// The file on the Mac, as the node reported it at create time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// When it was deleted; kept until compaction so its lifetime can still be billed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
    /// Its Mac has not confirmed the file yet. A record left like this by a crash can still be deleted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub creating: bool,
    /// A launch is attaching it right now. In memory only: a restart drops claims together with their launches.
    #[serde(skip)]
    pub claimed: bool,
}

fn default_format() -> String {
    "raw".into()
}

impl Volume {
    pub fn active(&self) -> bool {
        self.deleted_at.is_none()
    }

    /// `creating`, `attaching`, `attached` or `available`.
    pub fn status(&self) -> &'static str {
        if self.creating {
            "creating"
        } else if self.claimed {
            "attaching"
        } else if self.attached_to.is_some() {
            "attached"
        } else {
            "available"
        }
    }

    /// The record as the API shows it (the Mac-local path stays out).
    pub fn public_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "tenant": self.tenant,
            "node": self.node,
            "size_gib": self.size_gib,
            "format": self.format,
            "created_at": self.created_at,
            "attached_to": self.attached_to,
            "status": self.status(),
        })
    }

    /// The `apple.extra_disks` entry that attaches it: a raw image on the virtio controller, named and serialled by its
    /// id so the guest finds it at `/dev/disk/by-id/virtio-<id>`.
    pub fn extra_disk(&self) -> Option<Value> {
        let path = self.path.as_deref()?;
        Some(json!({
            "name": self.id,
            "path": path,
            "kind": "image",
            "controller": "virtio",
            "block_device_id": self.id,
        }))
    }

    fn visible_to(&self, caller: &Caller) -> bool {
        match caller {
            Caller::Admin => true,
            Caller::Tenant(t) => self.tenant.as_deref() == Some(t.as_str()),
        }
    }
}

/// 1-40 lowercase letters, digits and `-`, starting and ending with a letter or digit, and not `vol-…` (that is how
/// ids look, and the routes take either).
pub fn valid_volume_name(n: &str) -> bool {
    let b = n.as_bytes();
    let alnum = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !n.is_empty()
        && n.len() <= MAX_NAME_LEN
        && b.first().is_some_and(alnum)
        && b.last().is_some_and(alnum)
        && b.iter().all(|c| alnum(c) || *c == b'-')
        && !n.starts_with("vol-")
}

fn new_volume_id() -> String {
    format!("vol-{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

impl TenancyState {
    /// Whether `tenant` holds any live volume.
    pub fn has_volumes(&self, tenant: &str) -> bool {
        self.volumes
            .iter()
            .any(|v| v.active() && v.tenant.as_deref() == Some(tenant))
    }

    /// GiB of live volumes `tenant` holds.
    pub fn volume_gib_of(&self, tenant: &str) -> u64 {
        self.volumes
            .iter()
            .filter(|v| v.active() && v.tenant.as_deref() == Some(tenant))
            .map(|v| v.size_gib)
            .sum()
    }

    /// The live volumes `caller` may see: a tenant its own, the operator all.
    pub fn volumes_for(&self, caller: &Caller) -> Vec<&Volume> {
        self.volumes
            .iter()
            .filter(|v| v.active() && v.visible_to(caller))
            .collect()
    }

    /// A live volume by id or name, as `caller` may see it. A tenant asking for another tenant's volume gets the same
    /// answer as for one that does not exist. The operator naming a volume that several tenants use the name for is
    /// asked for the id.
    pub fn find_volume(&self, caller: &Caller, key: &str) -> Result<&Volume, VolumeError> {
        let volumes = &self.volumes;
        let visible = move || {
            volumes
                .iter()
                .filter(move |v| v.active() && v.visible_to(caller))
        };
        if let Some(v) = visible().find(|v| v.id == key) {
            return Ok(v);
        }
        let mut named = visible().filter(|v| v.name == key);
        match (named.next(), named.next()) {
            (Some(v), None) => Ok(v),
            (None, _) => Err(missing(key)),
            (Some(_), Some(_)) => Err(conflict(format!(
                "several tenants have a volume named '{key}'; use its id (GET /fleet/volumes)"
            ))),
        }
    }

    fn volume_mut(&mut self, id: &str) -> Option<&mut Volume> {
        self.volumes.iter_mut().find(|v| v.id == id && v.active())
    }

    fn check_volume_quota(&self, tenant: &str, more_gib: u64) -> Result<(), VolumeError> {
        let Some(t) = self.tenants.get(tenant) else {
            return Err(bad(format!("no tenant '{tenant}'")));
        };
        if t.disabled {
            return Err(VolumeError(403, format!("tenant '{tenant}' is disabled")));
        }
        if let Some(max) = t.quota.max_volume_gib {
            let held = self.volume_gib_of(tenant);
            if held.saturating_add(more_gib) > max {
                return Err(VolumeError(
                    429,
                    format!(
                        "quota exceeded: tenant '{tenant}' holds {held} of {max} GiB of volumes; this needs {more_gib} more"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Records a new volume of `owner` on `node`, marked `creating` until its Mac confirms the file
    /// ([`TenancyState::finish_volume`]) or refuses it ([`TenancyState::abort_volume`]). Checks the name, size,
    /// uniqueness and quota, and holds the name and the GiB meanwhile.
    pub fn begin_volume(
        &mut self,
        owner: Option<&str>,
        name: &str,
        size_gib: u64,
        node: &str,
        now: DateTime<Utc>,
    ) -> Result<Volume, VolumeError> {
        if !valid_volume_name(name) {
            return Err(bad(format!(
                "bad volume name '{name}': 1-{MAX_NAME_LEN} lowercase letters, digits and '-', starting and ending with a letter or digit, not starting with 'vol-'"
            )));
        }
        if !(1..=MAX_VOLUME_GIB).contains(&size_gib) {
            return Err(bad(format!("size_gib must be 1-{MAX_VOLUME_GIB}")));
        }
        let mine: Vec<&Volume> = self
            .volumes
            .iter()
            .filter(|v| v.active() && v.tenant.as_deref() == owner)
            .collect();
        if mine.iter().any(|v| v.name == name) {
            return Err(conflict(format!("a volume named '{name}' already exists")));
        }
        if mine.len() >= MAX_VOLUMES_PER_OWNER {
            return Err(VolumeError(
                429,
                format!("at most {MAX_VOLUMES_PER_OWNER} volumes; delete one first"),
            ));
        }
        if let Some(t) = owner {
            self.check_volume_quota(t, size_gib)?;
        }
        let v = Volume {
            id: new_volume_id(),
            name: name.to_owned(),
            tenant: owner.map(str::to_owned),
            node: node.to_owned(),
            size_gib,
            created_at: now,
            attached_to: None,
            format: default_format(),
            path: None,
            deleted_at: None,
            creating: true,
            claimed: false,
        };
        self.volumes.push(v.clone());
        Ok(v)
    }

    /// The Mac made the file at `path`: the volume is ready.
    pub fn finish_volume(&mut self, id: &str, path: String) -> Option<Volume> {
        let v = self.volume_mut(id)?;
        v.creating = false;
        v.path = Some(path);
        Some(v.clone())
    }

    /// The Mac refused the file: forget the record.
    pub fn abort_volume(&mut self, id: &str) {
        self.volumes.retain(|v| v.id != id);
    }

    /// The volume `key` if `caller` may delete it now: not attached or being attached.
    pub fn volume_to_delete(&self, caller: &Caller, key: &str) -> Result<Volume, VolumeError> {
        let v = self.find_volume(caller, key)?;
        if let Some(vm) = &v.attached_to {
            return Err(conflict(format!(
                "volume '{}' is attached to VM {vm} on {}; delete the VM first (the volume keeps its data)",
                v.name, v.node
            )));
        }
        if v.claimed {
            return Err(conflict(format!(
                "volume '{}' is being attached to a VM that is launching; try again in a moment",
                v.name
            )));
        }
        Ok(v.clone())
    }

    pub fn mark_volume_deleted(&mut self, id: &str, now: DateTime<Utc>) -> bool {
        match self.volume_mut(id) {
            Some(v) => {
                v.deleted_at = Some(now);
                v.attached_to = None;
                true
            }
            None => false,
        }
    }

    /// The volume `key` if `caller` may grow it to `size_gib` now: ready, detached, larger than today and within the
    /// owner's quota.
    pub fn volume_to_resize(
        &self,
        caller: &Caller,
        key: &str,
        size_gib: u64,
    ) -> Result<Volume, VolumeError> {
        let v = self.find_volume(caller, key)?;
        if v.creating {
            return Err(conflict(format!(
                "volume '{}' is still being created",
                v.name
            )));
        }
        if v.attached_to.is_some() || v.claimed {
            return Err(conflict(format!(
                "volume '{}' is attached to a VM; delete the VM (the volume keeps its data), then resize it",
                v.name
            )));
        }
        if size_gib > MAX_VOLUME_GIB {
            return Err(bad(format!("size_gib must be at most {MAX_VOLUME_GIB}")));
        }
        if size_gib <= v.size_gib {
            return Err(bad(format!(
                "volume '{}' is {} GiB; volumes only grow, so ask for more than that",
                v.name, v.size_gib
            )));
        }
        if let Some(t) = &v.tenant {
            self.check_volume_quota(t, size_gib - v.size_gib)?;
        }
        Ok(v.clone())
    }

    pub fn set_volume_size(&mut self, id: &str, size_gib: u64) -> Option<Volume> {
        let v = self.volume_mut(id)?;
        v.size_gib = size_gib;
        Some(v.clone())
    }

    /// Claims the volumes a launch names for `caller` (the tenant the VM is for, or the operator): each must be
    /// ready and free, and all on one Mac, which the launch then has to use. Returns them; release the claim with
    /// [`TenancyState::settle_volumes`].
    pub fn claim_volumes(
        &mut self,
        caller: &Caller,
        keys: &[String],
    ) -> Result<Vec<Volume>, VolumeError> {
        if keys.len() > MAX_VOLUMES_PER_VM {
            return Err(bad(format!(
                "a VM can have at most {MAX_VOLUMES_PER_VM} volumes"
            )));
        }
        let mut picked: Vec<Volume> = Vec::new();
        for key in keys {
            let v = self.find_volume(caller, key)?;
            if picked.iter().any(|p| p.id == v.id) {
                return Err(bad(format!("volume '{key}' is named twice")));
            }
            if v.creating || v.path.is_none() {
                return Err(conflict(format!(
                    "volume '{}' is still being created",
                    v.name
                )));
            }
            if let Some(vm) = &v.attached_to {
                return Err(conflict(format!(
                    "volume '{}' is already attached to VM {vm}; a volume serves one VM at a time",
                    v.name
                )));
            }
            if v.claimed {
                return Err(conflict(format!(
                    "volume '{}' is being attached to another VM that is launching",
                    v.name
                )));
            }
            if let Some(first) = picked.first()
                && first.node != v.node
            {
                return Err(conflict(format!(
                    "volumes '{}' (on {}) and '{}' (on {}) are on different Macs; a VM can only use volumes on the Mac it runs on",
                    first.name, first.node, v.name, v.node
                )));
            }
            picked.push(v.clone());
        }
        for p in &picked {
            if let Some(v) = self.volume_mut(&p.id) {
                v.claimed = true;
            }
        }
        Ok(picked)
    }

    /// Ends a claim: attaches the volumes to `vm` when the launch worked, frees them when it did not.
    pub fn settle_volumes(&mut self, ids: &[String], vm: Option<&str>) {
        for id in ids {
            if let Some(v) = self.volume_mut(id) {
                v.claimed = false;
                if let Some(vm) = vm {
                    v.attached_to = Some(vm.to_owned());
                }
            }
        }
    }

    /// Detaches every volume attached to VM `vm_id` on `node` (the VM is gone).
    pub fn detach_volumes_of(&mut self, node: &str, vm_id: &str) {
        for v in self.volumes.iter_mut() {
            if v.node == node && v.attached_to.as_deref() == Some(vm_id) {
                v.attached_to = None;
            }
        }
    }

    /// Detaches volumes on `node` whose VM the ledger no longer has as running (deleted on the Mac, ended by
    /// reconciliation, or never recorded).
    pub fn detach_ended_vm_volumes(&mut self, node: &str) {
        let gone: Vec<String> = self
            .volumes
            .iter()
            .filter(|v| v.node == node)
            .filter_map(|v| v.attached_to.clone())
            .filter(|id| self.vm(node, id).is_none_or(|e| !e.active()))
            .collect();
        for id in gone {
            self.detach_volumes_of(node, &id);
        }
    }

    /// Whether `path` is the file of a live volume (a create body naming it directly would bypass the attach rules).
    pub fn is_volume_path(&self, path: &str) -> bool {
        let p = path.trim_end_matches('/');
        self.volumes
            .iter()
            .any(|v| v.active() && v.path.as_deref() == Some(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::{Quota, VmEntry};
    use chrono::{Duration, TimeZone};

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    }

    fn state() -> TenancyState {
        let mut s = TenancyState::default();
        for t in ["acme", "globex"] {
            s.create_tenant(t, None, Quota::default(), t0()).unwrap();
        }
        s
    }

    fn ready(
        s: &mut TenancyState,
        owner: Option<&str>,
        name: &str,
        gib: u64,
        node: &str,
    ) -> Volume {
        let v = s.begin_volume(owner, name, gib, node, t0()).unwrap();
        s.finish_volume(&v.id, format!("/state/volumes/{}.raw", v.id))
            .unwrap()
    }

    fn acme() -> Caller {
        Caller::Tenant("acme".into())
    }

    #[test]
    fn names_are_checked() {
        for ok in ["data", "pg-data-1", "a", "x9"] {
            assert!(valid_volume_name(ok), "{ok}");
        }
        let long = "x".repeat(41);
        for bad in ["", "-a", "a-", "Data", "a_b", "vol-1", "a/b", long.as_str()] {
            assert!(!valid_volume_name(bad), "{bad}");
        }
    }

    #[test]
    fn create_checks_size_uniqueness_and_quota() {
        let mut s = state();
        assert_eq!(
            s.begin_volume(Some("acme"), "d", 0, "mac1", t0())
                .unwrap_err()
                .0,
            400
        );
        assert_eq!(
            s.begin_volume(Some("acme"), "d", MAX_VOLUME_GIB + 1, "mac1", t0())
                .unwrap_err()
                .0,
            400
        );
        let v = ready(&mut s, Some("acme"), "data", 100, "mac1");
        assert!(v.id.starts_with("vol-") && v.id.len() == 16, "{}", v.id);
        assert_eq!(v.status(), "available");
        // One name per owner; another tenant may use it too.
        assert_eq!(
            s.begin_volume(Some("acme"), "data", 1, "mac1", t0())
                .unwrap_err()
                .0,
            409
        );
        ready(&mut s, Some("globex"), "data", 1, "mac2");
        // The GiB quota counts what is held.
        s.tenants.get_mut("acme").unwrap().quota.max_volume_gib = Some(150);
        assert_eq!(
            s.begin_volume(Some("acme"), "more", 51, "mac1", t0())
                .unwrap_err()
                .0,
            429
        );
        let w = s
            .begin_volume(Some("acme"), "more", 50, "mac1", t0())
            .unwrap();
        assert_eq!(w.status(), "creating");
        s.abort_volume(&w.id);
        assert_eq!(s.volume_gib_of("acme"), 100);
        // Unknown and disabled tenants hold nothing.
        assert_eq!(
            s.begin_volume(Some("nobody"), "x", 1, "mac1", t0())
                .unwrap_err()
                .0,
            400
        );
        s.tenants.get_mut("globex").unwrap().disabled = true;
        assert_eq!(
            s.begin_volume(Some("globex"), "x", 1, "mac1", t0())
                .unwrap_err()
                .0,
            403
        );
    }

    #[test]
    fn tenants_see_only_their_own_and_the_operator_needs_ids_for_shared_names() {
        let mut s = state();
        let a = ready(&mut s, Some("acme"), "data", 1, "mac1");
        let g = ready(&mut s, Some("globex"), "data", 1, "mac2");
        assert_eq!(s.find_volume(&acme(), "data").unwrap().id, a.id);
        assert_eq!(s.find_volume(&acme(), &g.id).unwrap_err().0, 404);
        assert_eq!(s.volumes_for(&acme()).len(), 1);
        assert_eq!(s.volumes_for(&Caller::Admin).len(), 2);
        assert_eq!(s.find_volume(&Caller::Admin, "data").unwrap_err().0, 409);
        assert_eq!(s.find_volume(&Caller::Admin, &g.id).unwrap().id, g.id);
        // A tenant cannot delete or grow another's volume either.
        assert_eq!(s.volume_to_delete(&acme(), &g.id).unwrap_err().0, 404);
        assert_eq!(s.volume_to_resize(&acme(), &g.id, 5).unwrap_err().0, 404);
    }

    #[test]
    fn a_launch_claims_attaches_and_deleting_the_vm_detaches() {
        let mut s = state();
        let a = ready(&mut s, Some("acme"), "data", 10, "mac1");
        let b = ready(&mut s, Some("acme"), "logs", 10, "mac1");
        let far = ready(&mut s, Some("acme"), "far", 10, "mac2");
        // Different Macs, repeats and unknown names are refused, and refusing claims nothing.
        let names = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            s.claim_volumes(&acme(), &names(&["data", "far"]))
                .unwrap_err()
                .0,
            409
        );
        assert_eq!(
            s.claim_volumes(&acme(), &names(&["data", "data"]))
                .unwrap_err()
                .0,
            400
        );
        assert_eq!(
            s.claim_volumes(&acme(), &names(&["nope"])).unwrap_err().0,
            404
        );
        assert!(s.volumes.iter().all(|v| !v.claimed));

        let got = s.claim_volumes(&acme(), &names(&["data", "logs"])).unwrap();
        assert_eq!(got.len(), 2);
        // While claimed: no second launch, no delete.
        assert_eq!(
            s.claim_volumes(&acme(), &names(&["data"])).unwrap_err().0,
            409
        );
        assert_eq!(s.volume_to_delete(&acme(), "data").unwrap_err().0, 409);
        // A failed launch frees them.
        s.settle_volumes(&[a.id.clone(), b.id.clone()], None);
        assert_eq!(
            s.find_volume(&acme(), "data").unwrap().status(),
            "available"
        );

        s.claim_volumes(&acme(), &names(&["data", "logs"])).unwrap();
        s.settle_volumes(&[a.id.clone(), b.id.clone()], Some("vm-1"));
        s.record_vm(VmEntry {
            node: "mac1".into(),
            vm_id: "vm-1".into(),
            name: "db".into(),
            tenant: Some("acme".into()),
            vcpus: 2,
            memory_mib: 2048,
            macos_guest: false,
            created_at: t0(),
            ended_at: None,
            spec: None,
            warm_pool: None,
            claimed_at: None,
            expires_at: None,
            billed_from: None,
        });
        let v = s.find_volume(&acme(), "data").unwrap();
        assert_eq!(
            (v.status(), v.attached_to.as_deref()),
            ("attached", Some("vm-1"))
        );
        assert_eq!(s.volume_to_delete(&acme(), "data").unwrap_err().0, 409);
        assert_eq!(s.volume_to_resize(&acme(), "data", 20).unwrap_err().0, 409);
        assert_eq!(
            s.claim_volumes(&acme(), &names(&["logs"])).unwrap_err().0,
            409
        );
        // The VM goes: both detach, the far one was never touched.
        assert!(s.end_vm("mac1", "vm-1", t0() + Duration::hours(1)));
        assert!(
            s.volumes_for(&acme())
                .iter()
                .all(|v| v.attached_to.is_none())
        );
        assert_eq!(
            s.find_volume(&acme(), &far.id).unwrap().status(),
            "available"
        );
        let d = s.volume_to_delete(&acme(), "data").unwrap();
        assert!(s.mark_volume_deleted(&d.id, t0() + Duration::hours(2)));
        assert_eq!(s.find_volume(&acme(), "data").unwrap_err().0, 404);
        // The name is free again.
        ready(&mut s, Some("acme"), "data", 1, "mac1");
    }

    #[test]
    fn reconciliation_detaches_volumes_of_vanished_vms() {
        let mut s = state();
        let a = ready(&mut s, Some("acme"), "data", 1, "mac1");
        s.claim_volumes(&acme(), &["data".to_owned()]).unwrap();
        s.settle_volumes(&[a.id.clone()], Some("vm-9"));
        // The ledger never heard of vm-9 (or it ended): reconciling mac1 frees the volume.
        s.reconcile_node("mac1", &Default::default(), t0() + Duration::hours(1));
        assert_eq!(s.find_volume(&acme(), "data").unwrap().attached_to, None);
    }

    #[test]
    fn resize_only_grows_within_quota() {
        let mut s = state();
        ready(&mut s, Some("acme"), "data", 10, "mac1");
        assert_eq!(s.volume_to_resize(&acme(), "data", 10).unwrap_err().0, 400);
        assert_eq!(s.volume_to_resize(&acme(), "data", 5).unwrap_err().0, 400);
        assert_eq!(
            s.volume_to_resize(&acme(), "data", MAX_VOLUME_GIB + 1)
                .unwrap_err()
                .0,
            400
        );
        s.tenants.get_mut("acme").unwrap().quota.max_volume_gib = Some(15);
        assert_eq!(s.volume_to_resize(&acme(), "data", 16).unwrap_err().0, 429);
        let v = s.volume_to_resize(&acme(), "data", 15).unwrap();
        assert_eq!(s.set_volume_size(&v.id, 15).unwrap().size_gib, 15);
    }

    #[test]
    fn tenants_with_volumes_are_busy_and_paths_are_known() {
        let mut s = state();
        let v = ready(&mut s, Some("acme"), "data", 1, "mac1");
        assert!(s.delete_tenant("acme").is_err());
        assert!(s.is_volume_path(v.path.as_deref().unwrap()));
        assert!(!s.is_volume_path("/elsewhere.raw"));
        s.mark_volume_deleted(&v.id, t0());
        assert!(!s.is_volume_path(v.path.as_deref().unwrap()));
        s.delete_tenant("acme").unwrap();
        // Old state files load, and volumes round-trip without their in-memory claim.
        let json = serde_json::to_string(&s).unwrap();
        let back: TenancyState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.volumes.len(), 1);
        let old: TenancyState = serde_json::from_str("{}").unwrap();
        assert!(old.volumes.is_empty());
    }

    #[test]
    fn the_extra_disk_is_a_virtio_image_serialled_by_id() {
        let mut s = state();
        let v = ready(&mut s, Some("acme"), "data", 1, "mac1");
        let d = v.extra_disk().unwrap();
        assert_eq!(d["block_device_id"], v.id);
        assert_eq!(d["name"], v.id);
        assert_eq!(d["controller"], "virtio");
        assert!(d["path"].as_str().unwrap().ends_with(".raw"));
        assert!(v.id.len() <= 20, "a virtio serial is at most 20 characters");
    }
}
