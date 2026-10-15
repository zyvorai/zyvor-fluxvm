// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Pure placement scoring for a fleet of Apple hosts (Mac Mini / Mac Studio) running the `vz` backend.
//!
//! A host advertises [`AppleHostCaps`] (the runner's `capabilities` control command plus the host's free
//! CPU and memory); a request is reduced to an [`ApplePlacementRequest`]. [`score`] is `None` when the host
//! cannot run the VM at all and otherwise prefers the tightest fit, so small agent VMs pack onto busy hosts
//! and large hosts stay free for large VMs. Kairon (or any fleet scheduler) calls [`pick`].

use fluxvm_core::model::{AppleGuest, CreateVmRequest};
use serde::{Deserialize, Serialize};

/// What a host can offer right now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppleHostCaps {
    pub free_cpu: u32,
    pub free_memory_mib: u64,
    pub nested_virtualization: bool,
    /// macOS 26+ vmnet custom networks.
    pub vmnet: bool,
    /// macOS 27+ custom Virtio devices.
    pub custom_virtio: bool,
    pub bridged_interfaces: Vec<String>,
    /// Running macOS guests; Apple's licence allows two per host.
    #[serde(default)]
    pub macos_guests: u32,
    /// macOS 27+ EFI Secure Boot for Linux guests.
    #[serde(default)]
    pub secure_boot: bool,
    /// Rosetta for Linux is installed.
    #[serde(default)]
    pub rosetta: bool,
}

/// Memory kept back for macOS itself: the larger of 3 GiB and a quarter of the Mac's unified memory. Kairon's Mac
/// node reports allocatable memory the same way, so a fleet scheduler and `fluxvm-agent central` agree.
pub fn headroom_mib(total_mib: u64) -> u64 {
    (3 * 1024).max(total_mib / 4)
}

/// Memory VMs may use on a Mac with `total_mib` of unified memory.
pub fn allocatable_mib(total_mib: u64) -> u64 {
    total_mib.saturating_sub(headroom_mib(total_mib))
}

/// Apple's limit on concurrently running macOS guests per host.
pub const MAX_MACOS_GUESTS: u32 = 2;

/// This Mac already runs [`MAX_MACOS_GUESTS`] macOS guests. The API maps it to 409: it clears when one stops.
#[derive(Debug)]
pub struct MacosGuestLimit;

impl std::fmt::Display for MacosGuestLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this Mac already runs {MAX_MACOS_GUESTS} macOS guests (Apple's licence limit); stop one or use a Linux guest"
        )
    }
}

impl std::error::Error for MacosGuestLimit {}

/// Admission for one daemon: macOS guests that hold a slot (not stopped or failed), and an error once a new one
/// would be the third. The kernel enforces this too, but only at start, with an opaque VZ error.
pub fn enforce_macos_guest_limit(
    req: &CreateVmRequest,
    existing: &[fluxvm_core::model::VmRecord],
) -> anyhow::Result<()> {
    use fluxvm_core::model::VmStatus;
    let is_macos = |r: &CreateVmRequest| {
        r.apple
            .as_ref()
            .is_some_and(|a| a.guest_os == AppleGuest::Macos)
    };
    if !is_macos(req) {
        return Ok(());
    }
    let active = existing
        .iter()
        .filter(|vm| {
            matches!(
                vm.status,
                VmStatus::Creating | VmStatus::Running | VmStatus::Paused
            ) && is_macos(&vm.request)
        })
        .count();
    if active >= MAX_MACOS_GUESTS as usize {
        return Err(MacosGuestLimit.into());
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplePlacementRequest {
    pub vcpus: u32,
    pub memory_mib: u64,
    pub macos_guest: bool,
    pub needs_nested: bool,
    pub needs_vmnet: bool,
    pub needs_custom_virtio: bool,
    pub needs_secure_boot: bool,
    pub needs_rosetta: bool,
    pub bridge_interface: Option<String>,
}

impl ApplePlacementRequest {
    pub fn from_request(req: &CreateVmRequest) -> Self {
        let apple = req.apple.clone().unwrap_or_default();
        Self {
            vcpus: u32::from(req.vcpus),
            memory_mib: req.memory_mib,
            macos_guest: apple.guest_os == AppleGuest::Macos,
            needs_nested: apple.nested_virtualization,
            needs_vmnet: apple.vmnet.is_some(),
            needs_custom_virtio: apple.custom_virtio,
            needs_secure_boot: req.secure_boot == Some(true),
            needs_rosetta: apple.rosetta,
            bridge_interface: apple.bridge_interface,
        }
    }
}

/// Higher is better; `None` means the host cannot take the VM.
pub fn score(c: &AppleHostCaps, r: &ApplePlacementRequest) -> Option<i64> {
    if c.free_cpu < r.vcpus || c.free_memory_mib < r.memory_mib {
        return None;
    }
    if r.macos_guest && c.macos_guests >= MAX_MACOS_GUESTS {
        return None;
    }
    if (r.needs_nested && !c.nested_virtualization)
        || (r.needs_vmnet && !c.vmnet)
        || (r.needs_custom_virtio && !c.custom_virtio)
        || (r.needs_secure_boot && !c.secure_boot)
        || (r.needs_rosetta && !c.rosetta)
    {
        return None;
    }
    if let Some(i) = &r.bridge_interface
        && !c.bridged_interfaces.iter().any(|x| x == i)
    {
        return None;
    }
    let cpu_after = i64::from(c.free_cpu - r.vcpus);
    let mem_after = i64::try_from((c.free_memory_mib - r.memory_mib) / 64).unwrap_or(i64::MAX);
    Some(-(cpu_after.saturating_mul(1024).saturating_add(mem_after)))
}

/// Index of the best host, or `None` if no host fits. Ties go to the earlier host.
pub fn pick(hosts: &[AppleHostCaps], r: &ApplePlacementRequest) -> Option<usize> {
    hosts
        .iter()
        .enumerate()
        .filter_map(|(i, c)| score(c, r).map(|s| (i, s)))
        .fold(None, |best: Option<(usize, i64)>, (i, s)| match best {
            Some((_, b)) if b >= s => best,
            _ => Some((i, s)),
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn studio() -> AppleHostCaps {
        AppleHostCaps {
            free_cpu: 24,
            free_memory_mib: 196_608,
            nested_virtualization: false,
            vmnet: true,
            custom_virtio: false,
            bridged_interfaces: vec!["en0".into()],
            macos_guests: 0,
            secure_boot: true,
            rosetta: false,
        }
    }

    fn small() -> ApplePlacementRequest {
        ApplePlacementRequest {
            vcpus: 1,
            memory_mib: 512,
            ..Default::default()
        }
    }

    #[test]
    fn headroom_is_the_larger_of_three_gib_and_a_quarter() {
        assert_eq!(headroom_mib(8 * 1024), 3 * 1024);
        assert_eq!(headroom_mib(12 * 1024), 3 * 1024);
        assert_eq!(headroom_mib(64 * 1024), 16 * 1024);
        assert_eq!(allocatable_mib(16 * 1024), 12 * 1024);
        assert_eq!(allocatable_mib(2 * 1024), 0, "never underflows");
    }

    #[test]
    fn feature_mismatch_is_rejected() {
        let r = ApplePlacementRequest {
            vcpus: 8,
            memory_mib: 32_768,
            needs_nested: true,
            ..Default::default()
        };
        assert_eq!(score(&studio(), &r), None);
        let r = ApplePlacementRequest {
            bridge_interface: Some("en1".into()),
            ..small()
        };
        assert_eq!(score(&studio(), &r), None);
        let r = ApplePlacementRequest {
            needs_custom_virtio: true,
            ..small()
        };
        assert_eq!(score(&studio(), &r), None);
        let r = ApplePlacementRequest {
            needs_rosetta: true,
            ..small()
        };
        assert_eq!(score(&studio(), &r), None);
        let r = ApplePlacementRequest {
            needs_secure_boot: true,
            ..small()
        };
        assert!(score(&studio(), &r).is_some());
        let old = AppleHostCaps {
            secure_boot: false,
            ..studio()
        };
        assert_eq!(score(&old, &r), None);
    }

    #[test]
    fn not_enough_cpu_or_memory_is_rejected() {
        let r = ApplePlacementRequest {
            vcpus: 25,
            ..small()
        };
        assert_eq!(score(&studio(), &r), None);
        let r = ApplePlacementRequest {
            memory_mib: 196_609,
            ..small()
        };
        assert_eq!(score(&studio(), &r), None);
    }

    #[test]
    fn third_macos_guest_is_rejected() {
        let full = AppleHostCaps {
            macos_guests: 2,
            ..studio()
        };
        let mac = ApplePlacementRequest {
            macos_guest: true,
            ..small()
        };
        assert_eq!(score(&full, &mac), None);
        assert!(score(&full, &small()).is_some());
    }

    fn vm(guest: &str, status: &str) -> fluxvm_core::model::VmRecord {
        serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(),
            "name": "m",
            "backend": "vz",
            "status": status,
            "pid": null,
            "created_at": "2026-01-01T00:00:00Z",
            "expires_at": null,
            "workspace": "/tmp/w",
            "disk": "/tmp/w/root.raw",
            "seed_disk": null,
            "tap_name": null,
            "control_socket": null,
            "log_path": "/tmp/w/console.log",
            "error": null,
            "request": {
                "name": "m", "image": "/img", "vcpus": 1, "memory_mib": 256,
                "backend": "vz", "apple": {"guest_os": guest}
            }
        }))
        .unwrap()
    }

    fn create(guest: &str) -> CreateVmRequest {
        serde_json::from_value(serde_json::json!({
            "name": "n", "image": "/img", "vcpus": 1, "memory_mib": 256,
            "backend": "vz", "apple": {"guest_os": guest}
        }))
        .unwrap()
    }

    #[test]
    fn daemon_refuses_a_third_running_macos_guest() {
        let two = [vm("macos", "running"), vm("macos", "paused")];
        let err = enforce_macos_guest_limit(&create("macos"), &two).unwrap_err();
        assert!(err.downcast_ref::<MacosGuestLimit>().is_some(), "{err}");
        assert!(err.to_string().contains("licence limit"), "{err}");
        // Linux guests are not counted against, or limited by, the two slots.
        assert!(enforce_macos_guest_limit(&create("linux"), &two).is_ok());
    }

    #[test]
    fn stopped_and_failed_macos_guests_free_their_slot() {
        let held = [
            vm("macos", "running"),
            vm("macos", "stopped"),
            vm("macos", "failed"),
            vm("linux", "running"),
        ];
        assert!(enforce_macos_guest_limit(&create("macos"), &held).is_ok());
    }

    #[test]
    fn tightest_fit_wins() {
        let mini = AppleHostCaps {
            free_cpu: 8,
            free_memory_mib: 16_384,
            ..studio()
        };
        let hosts = [studio(), mini, AppleHostCaps::default()];
        assert_eq!(pick(&hosts, &small()), Some(1));
        let big = ApplePlacementRequest {
            vcpus: 16,
            memory_mib: 65_536,
            ..Default::default()
        };
        assert_eq!(pick(&hosts, &big), Some(0));
        assert_eq!(pick(&[AppleHostCaps::default()], &small()), None);
    }

    #[test]
    fn request_conversion_reads_apple_options() {
        let req: CreateVmRequest = serde_json::from_str(
            r#"{"name":"t","backend":"vz","image":"/x.raw","vcpus":2,"memory_mib":1024,
                "apple":{"bridge_interface":"en0","nested_virtualization":true}}"#,
        )
        .unwrap();
        let r = ApplePlacementRequest::from_request(&req);
        assert_eq!((r.vcpus, r.memory_mib), (2, 1024));
        assert!(r.needs_nested && !r.macos_guest && !r.needs_vmnet);
        assert_eq!(r.bridge_interface.as_deref(), Some("en0"));
    }
}
