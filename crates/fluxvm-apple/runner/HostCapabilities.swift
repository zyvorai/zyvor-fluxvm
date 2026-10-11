
// Copyright 2026 Zyvor AI Labs
// SPDX-License-Identifier: Apache-2.0
import Foundation
import Virtualization
import Security

struct FluxAppleHostCapabilities: Codable {
    let osVersion: String
    let cpuCount: Int
    let maximumVmCPUs: Int
    let physicalMemoryBytes: UInt64
    let maximumVmMemoryBytes: UInt64
    let nestedVirtualization: Bool
    let bridgedInterfaces: [String]
    let vmnetCustomNetworks: Bool
    let vmnetSerialization: Bool
    let customVirtio: Bool
    let customVirtioQueueBackend: Bool
    let guestMemoryMapping: Bool
    let usbPassthroughAPI: Bool
    let efiSecureBoot: Bool
    let rosetta: String          // installed | not-installed | not-supported
    /// Whether this runner carries `com.apple.vm.networking`, which vmnet networks and bridged attachments need to start their
    /// interface (without it Virtualization reports "Internal Network Error" once the VM runs). Apple grants it only through a
    /// provisioning profile; an ad-hoc signed binary that claims it is killed at launch, so a runner that answers this is
    /// either legitimately provisioned or does not claim it.
    let vmNetworkingEntitlement: Bool
}

private func fluxHasEntitlement(_ name: String) -> Bool {
    guard let task = SecTaskCreateFromSelf(nil),
          let value = SecTaskCopyValueForEntitlement(task, name as CFString, nil) else { return false }
    return (value as? Bool) == true
}

func fluxAppleHostCapabilities() -> FluxAppleHostCapabilities {
    let p = ProcessInfo.processInfo
    let nested: Bool
    if #available(macOS 15.0, *) {
        nested = VZGenericPlatformConfiguration.isNestedVirtualizationSupported
    } else {
        nested = false
    }
    let has26: Bool = {
        if #available(macOS 26.0, *) { return true }
        return false
    }()
    let has27: Bool = {
        if #available(macOS 27.0, *) { return true }
        return false
    }()

    return FluxAppleHostCapabilities(
        osVersion: p.operatingSystemVersionString,
        cpuCount: p.processorCount,
        maximumVmCPUs: VZVirtualMachineConfiguration.maximumAllowedCPUCount,
        physicalMemoryBytes: p.physicalMemory,
        maximumVmMemoryBytes: VZVirtualMachineConfiguration.maximumAllowedMemorySize,
        nestedVirtualization: nested,
        bridgedInterfaces: VZBridgedNetworkInterface.networkInterfaces.map { $0.identifier },
        vmnetCustomNetworks: has26,
        vmnetSerialization: has26,
        customVirtio: has27,
        customVirtioQueueBackend: has27,
        guestMemoryMapping: has27,
        // The API exists on 27; using it still requires an AccessoryAccess UI
        // broker and explicit user consent, so this is not a readiness flag.
        usbPassthroughAPI: has27,
        efiSecureBoot: has27,
        rosetta: {
            switch VZLinuxRosettaDirectoryShare.availability {
            case .installed: return "installed"
            case .notInstalled: return "not-installed"
            default: return "not-supported"
            }
        }(),
        vmNetworkingEntitlement: fluxHasEntitlement("com.apple.vm.networking")
    )
}
