// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Read-only device state and a few controls of a running `vz` VM that the runner already serves over its control
//! socket: EFI Secure Boot status, the custom Virtio device, and USB devices (mounted mass storage and passthrough).

use crate::{VmManager, audit_event};
use anyhow::{Result, bail};
use fluxvm_core::model::{BackendKind, VmRecord, VmStatus};
use serde_json::{Value, json};
use uuid::Uuid;

impl VmManager {
    /// Apple's newest macOS restore image for this Mac (`image: "macos"` with `apple.install`), and whether it is already
    /// downloaded. With `download` it is fetched now (about 25 GB; it resumes if interrupted) and `path` is its cache file.
    pub async fn macos_ipsw(&self, download: bool) -> Result<Value> {
        let info = fluxvm_apple::latest_ipsw().await?;
        let mut out = serde_json::to_value(&info)?;
        let cached = self
            .cfg
            .state_dir
            .join("images")
            .join(format!("macos-{}.ipsw", info.build_version));
        let path = if download {
            Some(fluxvm_image::ipsw::ensure(&self.cfg, &info.url, &info.build_version).await?)
        } else {
            cached.exists().then_some(cached)
        };
        out["downloaded"] = json!(path.is_some());
        if let Some(p) = path {
            out["path"] = json!(p);
        }
        if download {
            audit_event("host.macos_ipsw", &[("build", &info.build_version)]);
        }
        Ok(out)
    }

    async fn vz_running_vm(&self, id: Uuid) -> Result<VmRecord> {
        let vm = self.get(id).await?;
        if vm.backend != BackendKind::Vz {
            bail!("this is a vz backend feature");
        }
        if vm.status != VmStatus::Running {
            bail!("the VM is {:?}; start it first", vm.status);
        }
        Ok(vm)
    }

    /// `{"enabled", "kek", "db", "dbx"}`; `"as_of":"boot"` while the guest runs (the variable store is locked).
    pub async fn vz_secure_boot_status(&self, id: Uuid) -> Result<Value> {
        fluxvm_apple::vz27::secure_boot_status(&self.vz_running_vm(id).await?).await
    }

    /// Driver state and request counters of the custom Virtio device.
    pub async fn vz_custom_virtio_status(&self, id: Uuid) -> Result<Value> {
        fluxvm_apple::vz27::custom_virtio_status(&self.vz_running_vm(id).await?).await
    }

    /// Asks the custom Virtio device to reset; the guest driver re-negotiates.
    pub async fn vz_custom_virtio_reset(&self, id: Uuid) -> Result<()> {
        let vm = self.vz_running_vm(id).await?;
        fluxvm_apple::vz27::custom_virtio_reset(&vm).await?;
        audit_event("vm.custom_virtio_reset", &[("vm_id", &id.to_string())]);
        Ok(())
    }

    /// USB devices on the VM's controllers right now.
    pub async fn vz_usb_list(&self, id: Uuid) -> Result<Vec<Value>> {
        fluxvm_apple::vz27::usb_list(&self.vz_running_vm(id).await?).await
    }

    /// Resizes display `display` of a running VM (`VZGraphicsDisplay.reconfigure`, macOS 14+). The request is validated
    /// before the VM is looked up, so a bad size is reported the same whether or not the VM runs. UNVERIFIED on hardware:
    /// whether the guest follows the new size depends on its display driver.
    pub async fn vz_display_resize(
        &self,
        id: Uuid,
        width: u32,
        height: u32,
        display: u32,
        ppi: Option<u32>,
    ) -> Result<()> {
        fluxvm_apple::display_resize_request(width, height, display, ppi)?;
        let vm = self.vz_running_vm(id).await?;
        fluxvm_apple::display_resize(&vm, width, height, display, ppi).await?;
        audit_event(
            "vm.display_resize",
            &[
                ("vm_id", &id.to_string()),
                ("width", &width.to_string()),
                ("height", &height.to_string()),
                ("display", &display.to_string()),
            ],
        );
        Ok(())
    }

    /// The signed boot attestation of the VM's last launch with the host public key, for an out-of-band verifier. A host
    /// claim, not hardware attestation (see `fluxvm_apple::attest`). Available in any state once the VM has launched.
    pub async fn vz_attestation(&self, id: Uuid) -> Result<Value> {
        let vm = self.get(id).await?;
        if vm.backend != BackendKind::Vz {
            bail!("this is a vz backend feature");
        }
        fluxvm_apple::attest::delivery(&id.to_string(), &vm.workspace, &self.cfg.state_dir)
    }

    /// Host USB accessories the user has granted to FluxVMUSBAccess.app.
    pub async fn vz_usb_physical_list(&self, id: Uuid) -> Result<Vec<Value>> {
        fluxvm_apple::physical_usb_list(&self.vz_running_vm(id).await?).await
    }

    /// Passes a host USB accessory through to the guest; returns the attached device's UUID.
    pub async fn vz_usb_physical_attach(&self, id: Uuid, registry_id: u64) -> Result<String> {
        let vm = self.vz_running_vm(id).await?;
        let uuid = fluxvm_apple::physical_usb_attach(&vm, registry_id).await?;
        audit_event(
            "vm.usb_passthrough",
            &[
                ("vm_id", &id.to_string()),
                ("registry_id", &registry_id.to_string()),
            ],
        );
        Ok(uuid)
    }
}

/// Host-side cost of one `vz` VM: its runner process's resident size and CPU time (see `fluxvm_apple::runner_usage`).
#[derive(Debug, Clone, PartialEq)]
pub struct VmRunnerUsage {
    pub vm_id: Uuid,
    pub name: String,
    pub rss_bytes: u64,
    pub cpu_seconds: f64,
}

/// Samples the runner process of every running or paused `vz` VM. A VM whose process cannot be read (just exited,
/// not macOS) is left out rather than reported as zero.
pub fn runner_usage_samples(vms: &[VmRecord]) -> Vec<VmRunnerUsage> {
    vms.iter()
        .filter(|vm| {
            vm.backend == BackendKind::Vz
                && matches!(vm.status, VmStatus::Running | VmStatus::Paused)
        })
        .filter_map(|vm| {
            let u = fluxvm_apple::runner_usage(vm.pid?).ok()?;
            Some(VmRunnerUsage {
                vm_id: vm.id,
                name: vm.name.clone(),
                rss_bytes: u.rss_bytes,
                cpu_seconds: u.cpu_seconds(),
            })
        })
        .collect()
}

/// What this Mac's Virtualization.framework offers (OS version, vmnet, custom Virtio, Secure Boot, Rosetta...), asked
/// of the signed runner helper. Errors off macOS.
pub async fn apple_host_capabilities() -> Result<Value> {
    let caps = tokio::task::spawn_blocking(fluxvm_apple::host_capabilities).await??;
    Ok(serde_json::to_value(caps)?)
}
