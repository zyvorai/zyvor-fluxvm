// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppleHostCapabilities {
    pub os_version: String,
    pub cpu_count: usize,
    #[serde(rename = "maximumVmCPUs")]
    pub maximum_vm_cpus: usize,
    pub physical_memory_bytes: u64,
    pub maximum_vm_memory_bytes: u64,
    pub nested_virtualization: bool,
    pub bridged_interfaces: Vec<String>,
    pub vmnet_custom_networks: bool,
    pub vmnet_serialization: bool,
    pub custom_virtio: bool,
    pub custom_virtio_queue_backend: bool,
    pub guest_memory_mapping: bool,
    #[serde(rename = "usbPassthroughAPI")]
    pub usb_passthrough_api: bool,
    /// macOS 27 `VZEFIVariableStore` Secure Boot management.
    #[serde(default)]
    pub efi_secure_boot: bool,
    /// `installed`, `not-installed` or `not-supported`; empty from an older runner.
    #[serde(default)]
    pub rosetta: String,
    /// Whether the runner carries `com.apple.vm.networking`, which vmnet networks and bridged attachments need. `None` from
    /// an older runner that does not report it (nothing is refused then).
    #[serde(default)]
    pub vm_networking_entitlement: Option<bool>,
}

/// Query the signed Swift helper without creating a VM. The daemon itself
/// continues not to link Virtualization.framework.
pub fn host_capabilities() -> Result<AppleHostCapabilities> {
    #[cfg(target_os = "macos")]
    {
        let runner = crate::find_runner()?;
        let output = std::process::Command::new(&runner)
            .arg("host-capabilities")
            .output()
            .with_context(|| format!("running {} host-capabilities", runner.display()))?;
        if !output.status.success() {
            bail!(
                "host-capabilities failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice(&output.stdout).context("decoding VZ host capabilities")
    }
    #[cfg(not(target_os = "macos"))]
    {
        bail!("Apple host capabilities are only available on macOS")
    }
}

/// Probes the host once per process and installs the result for [`crate::validate_request`], so refusals name the
/// real macOS requirement. A failed probe (no runner, not macOS) leaves only the static table in force.
pub async fn prime_host_capabilities() {
    static ONCE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    ONCE.get_or_init(|| async {
        if let Ok(Ok(caps)) = tokio::task::spawn_blocking(host_capabilities).await {
            crate::set_host_capabilities(Some(caps));
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape emitted by `fluxvm-vz-runner host-capabilities` (Swift Codable keys).
    #[test]
    fn decodes_runner_output() {
        let json = r#"{"guestMemoryMapping":true,"maximumVmCPUs":64,"vmnetCustomNetworks":true,
            "customVirtio":true,"physicalMemoryBytes":17179869184,"maximumVmMemoryBytes":17179869184,
            "nestedVirtualization":true,"cpuCount":10,"customVirtioQueueBackend":true,
            "osVersion":"Version 27.2","usbPassthroughAPI":true,"vmnetSerialization":true,
            "bridgedInterfaces":["en0"]}"#;
        let caps: AppleHostCapabilities = serde_json::from_str(json).unwrap();
        assert_eq!(caps.maximum_vm_cpus, 64);
        assert!(caps.usb_passthrough_api && caps.guest_memory_mapping);
        assert!(!caps.efi_secure_boot && caps.rosetta.is_empty());
        let newer = json.replace('}', r#","efiSecureBoot":true,"rosetta":"installed"}"#);
        let caps: AppleHostCapabilities = serde_json::from_str(&newer).unwrap();
        assert!(caps.efi_secure_boot);
        assert_eq!(caps.rosetta, "installed");
    }
}
