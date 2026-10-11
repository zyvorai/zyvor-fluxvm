# Shared vmnet broker

Apple's macOS 26+ design requires VMs on one custom network to share the same `vmnet_network_ref`. FluxVM runs each VZ VM in its own
runner process, so a named network needs one owner process. That is `fluxvm-vmnetd`, in `crates/fluxvm-apple/macos/vmnetd/`.

Apple provides the supported cross-process mechanism: `vmnet_network_copy_serialization` over XPC, then
`vmnet_network_create_with_serialization` in the receiving process. The serialisation object is passed as an XPC object and is never
turned into JSON or text.

## What exists

- **Service.** `fluxvm-vmnetd` is a per-user launch agent that listens on the Mach service `dev.zyvor.fluxvm.vmnetd`. It needs macOS 26+.
- **Commands.** One XPC dictionary per request, with an `op` of:
  - `acquire`: create the named network from the request (`mode`, `subnet`, `mask`, optional `reserved_ip` with `mac`, and
    `forwards`), or return the existing one; the reply carries the serialised network (`network`) and the reference count (`refs`).
  - `release`: drop one reference; the network is removed when the count reaches zero.
  - `list`: names and reference counts of the networks the broker holds.
- **Names.** 1 to 64 characters from `[A-Za-z0-9._-]`, checked when the VM is admitted (`validate_vmnet`). The broker itself only checks for a non-empty name of at most 64 bytes.
- **Reuse.** Acquiring an existing name with a different configuration (mode, subnet, mask, MAC, reserved address or forwards) is
  rejected with "network name already exists with a different configuration".
- **State.** The broker keeps networks in memory only. Nothing is persisted, so restarting it drops every named network.
- **Runner side.** `apple.vmnet.name` makes a network named and shareable; the runner acquires it through
  `VmnetSerialization.swift` and releases it when the VM stops. An unnamed `apple.vmnet` keeps the existing per-runner network and
  does not touch the broker. If the broker is not running, a named network fails with `vmnetd XPC connection failed`.
- **Install.** `crates/fluxvm-apple/macos/vmnetd/build-install.sh` builds with `xcrun swiftc` for macOS 26, ad-hoc signs with
  `com.apple.vm.networking` from `Entitlements.plist`, installs the binary to `/usr/local/libexec/fluxvm-vmnetd` with `sudo`, and
  bootstraps the launch agent with `launchctl`.

## What is verified

On the test Mac (Apple M4, macOS 27.2, SIP on) the original build, ad-hoc signed with `com.apple.vm.networking`, was killed at
launch (exit 137): that entitlement is restricted and needs an Apple-issued provisioning profile. Signed with only
`com.apple.security.virtualization` the broker starts under launchd and `vmnet_network_create` succeeds in it. **But a VM cannot use
the network:** Virtualization starts the VM and then reports `Internal Network Error` (`network-disconnected`), the same for a
named network through the broker and for an unnamed one created inside the runner, so the cause is the runner lacking
`com.apple.vm.networking`, not the sharing. This is therefore **unusable on a Mac without that entitlement's provisioning profile**.
Admission now refuses `apple.vmnet` (and `apple.bridge_interface`) when the runner does not hold it. For networks shared between
guests without the entitlement use `apple.networks` (private layer-2 networks, `fluxvm-vz-switch`), and for a controlled NAT
`apple.usernet`.

Two runners sharing one `apple.vmnet.name` has never run, for that reason.
