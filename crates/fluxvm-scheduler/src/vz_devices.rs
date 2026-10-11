// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Read-only device state and a few controls of a running `vz` VM that the runner already serves over its control
//! socket: EFI Secure Boot status, the custom Virtio device, and USB devices (mounted mass storage and passthrough).

use crate::{VmManager, audit_event};
use anyhow::{Result, bail};
pub use fluxvm_apple::display_resize_reply;
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
        crate::require_live(&vm, false)?;
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
    /// before the VM is looked up, so a bad size is reported the same whether or not the VM runs. This only updates the
    /// connector's preferred mode: the guest follows when a KMS client switches its active mode (see `DISPLAY_RESIZE_NOTE`).
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

/// Host-side cost of one `vz` VM: the runner process plus, when it can be attributed, the Virtualization XPC service
/// that actually runs the guest (see `fluxvm_apple::vm_usage`).
#[derive(Debug, Clone, PartialEq)]
pub struct VmRunnerUsage {
    pub vm_id: Uuid,
    pub name: String,
    pub rss_bytes: u64,
    pub cpu_seconds: f64,
    /// Whether the guest's XPC service was found and included; false means the numbers are the runner process only.
    pub service_attributed: bool,
}

/// Samples every running or paused `vz` VM. A VM whose runner cannot be read (just exited, not macOS) is left out
/// rather than reported as zero.
pub fn runner_usage_samples(vms: &[VmRecord]) -> Vec<VmRunnerUsage> {
    let live: Vec<(&VmRecord, u32)> = vms
        .iter()
        .filter(|vm| {
            vm.backend == BackendKind::Vz
                && matches!(vm.status, VmStatus::Running | VmStatus::Paused)
        })
        .filter_map(|vm| Some((vm, vm.pid?)))
        .collect();
    if live.is_empty() {
        return Vec::new();
    }
    let targets: Vec<(u32, std::path::PathBuf)> = live
        .iter()
        .map(|(vm, pid)| (*pid, vm.workspace.clone()))
        .collect();
    live.iter()
        .zip(fluxvm_apple::vm_usage(&targets))
        .filter_map(|((vm, _), u)| {
            let u = u?;
            Some(VmRunnerUsage {
                vm_id: vm.id,
                name: vm.name.clone(),
                rss_bytes: u.usage.rss_bytes,
                cpu_seconds: u.usage.cpu_seconds(),
                service_attributed: !u.service_pids.is_empty(),
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

/// `GET /v1/host/apple/capabilities`: the capability table judged against the probed Mac.
pub async fn apple_capabilities_json() -> Value {
    fluxvm_apple::prime_host_capabilities().await;
    let host = fluxvm_apple::current_host_capabilities();
    json!({
        "os_version": host.as_ref().map(|h| h.os_version.clone()),
        "probed": host.is_some(),
        "items": fluxvm_apple::capability_report(host.as_ref()),
    })
}

/// `GET /v1/host/overview` without the netobs flag (the API layer owns the observer): daemon version, host, VM counts,
/// per-VM runner usage and helper availability.
pub async fn apple_host_overview_json(
    vms: Vec<VmRecord>,
    branches: usize,
    uptime_seconds: Option<u64>,
) -> anyhow::Result<Value> {
    use std::collections::BTreeMap;
    let mut by_status: BTreeMap<String, u64> = BTreeMap::new();
    let mut by_backend: BTreeMap<String, u64> = BTreeMap::new();
    for vm in &vms {
        *by_status
            .entry(format!("{:?}", vm.status).to_lowercase())
            .or_default() += 1;
        *by_backend
            .entry(format!("{:?}", vm.backend).to_lowercase())
            .or_default() += 1;
    }
    let total = vms.len();
    let sampled = vms.clone();
    let (rows, host_mem) = tokio::task::spawn_blocking(move || {
        (
            runner_usage_samples(&sampled),
            fluxvm_core::pressure_admission::sample_host(),
        )
    })
    .await?;
    let usage: Vec<Value> = rows
        .iter()
        .map(|u| {
            let vm = vms.iter().find(|v| v.id == u.vm_id);
            json!({
                "vm_id": u.vm_id,
                "name": u.name,
                "status": vm.map(|v| format!("{:?}", v.status).to_lowercase()),
                "cpu_seconds_total": u.cpu_seconds,
                "rss_bytes": u.rss_bytes,
                "service_attributed": u.service_attributed,
                "started_at": vm.and_then(|v| v.pid).and_then(fluxvm_apple::process_started_at),
            })
        })
        .collect();
    fluxvm_apple::prime_host_capabilities().await;
    let caps = fluxvm_apple::current_host_capabilities();
    Ok(json!({
        "daemon": {
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": uptime_seconds,
        },
        "host": {
            "os_version": caps.as_ref().map(|c| c.os_version.clone()),
            "cpu_count": std::thread::available_parallelism().map(|n| n.get()).ok(),
            "memory_total_mib": host_mem.mem_total_mib,
            "memory_available_mib": host_mem.mem_available_mib,
            "memory_pressure": host_mem.level,
            "nested_virtualization": caps.as_ref().map(|c| c.nested_virtualization),
            "rosetta": caps.as_ref().map(|c| c.rosetta.clone()),
        },
        "vms": {"total": total, "by_status": by_status, "by_backend": by_backend},
        "branches": branches,
        "usage": usage,
        "helpers": {
            "runner": fluxvm_apple::find_runner().is_ok(),
            "switch": fluxvm_apple::vznet::find_switch().is_ok(),
            "blockd": fluxvm_apple::blockd::find_blockd().is_ok(),
            "usernet": fluxvm_apple::usernet::find_usernet().is_ok(),
        },
        "networking": {
            "vm_networking_entitlement": caps.as_ref().and_then(|c| c.vm_networking_entitlement),
        },
    }))
}

#[cfg(test)]
mod overview_tests {
    use super::*;

    #[tokio::test]
    async fn the_overview_has_the_documented_shape_with_no_vms() {
        let v = apple_host_overview_json(Vec::new(), 3, Some(42))
            .await
            .unwrap();
        assert_eq!(v["daemon"]["uptime_seconds"], 42);
        assert!(
            v["daemon"]["version"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
        assert_eq!(v["vms"]["total"], 0);
        assert_eq!(v["branches"], 3);
        assert!(v["usage"].as_array().unwrap().is_empty());
        for h in ["runner", "switch", "blockd", "usernet"] {
            assert!(v["helpers"][h].is_boolean(), "{h}");
        }
        assert!(v["host"]["cpu_count"].as_u64().unwrap_or(0) >= 1);
    }

    #[tokio::test]
    async fn the_capability_report_lists_every_table_row() {
        let v = apple_capabilities_json().await;
        let n = v["items"].as_array().unwrap().len();
        assert_eq!(n, fluxvm_apple::CAPABILITIES.len());
        assert!(v["items"][0]["feature"].is_string());
        assert!(v["items"][0]["min_macos"].as_u64().unwrap() >= 14);
    }
}
