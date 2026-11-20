# Mac vz feature completion (control plane)

This document records the pure-Rust / example completion of the high-impact `vz` features. The Swift runner and Virtualization.framework paths already exist in-tree; this change finishes the control-plane surface that can be validated without an Apple Silicon host.

## Status

| Feature | Control plane | Runner / hardware |
|---------|---------------|-------------------|
| Unattended macOS provisioning (`provision_*`) | Present + example | macOS 27, needs hardware verify |
| First-boot account fallback (`username` / `auto_login`) | **Added** | Helper runs inside guest |
| ASIF / DiskImageKit overlay | Present + example | macOS 26/27 |
| Named shared vmnet + UDP forwards | Present + example | Needs signed `fluxvm-vmnetd` |
| USB controller + mass-storage hotplug | Present + example | Verified path |
| Physical USB Accessory Access | Present | Needs signed UI helper + consent |
| Custom Virtio | Present + example | Verified on M4 (control + bulk) |
| Nested virtualization | Present + example | Host capability gated |
| Rosetta | Present + example | Linux guest only |
| Agent / container sandboxes | Present | Verified on M4 |
| Snapshots / warm pools | Present | Verified (Linux); macOS guest hand-verified |
| Multi-Mac fleet placement | Present | Loopback only; needs 2+ Macs |

## What this change adds

1. `AppleFirstBoot` account fields and helper support (username, full name, password/password_file, auto_login).
2. Example specs under `examples/vz-complete/` for every major feature above.
3. Unit test coverage for the first-boot share writer (already green on Linux).

## What still needs a Mac

- Compiling `fluxvm-vz-runner`, `fluxvm-vmnetd`, and `FluxVMUSBAccess.app` with the correct entitlements.
- End-to-end IPSW install + first-boot account creation.
- Two-runner shared vmnet.
- Physical USB attach with user consent.
- Multi-Mac Kairon placement.

See `docs/macos.md` and `docs/macos-vz27-remaining-complete.md` for the full capability matrix and hardware log.
