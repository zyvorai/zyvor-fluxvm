# Windows 11 ARM on the `vz` backend

`fluxctl create --guest windows` boots a Windows 11 ARM64 installer under Apple's Virtualization.framework, with no
UTM and no QEMU. **v1 boots the installer and nothing more**: you finish the install by hand. There is no in-guest
agent, no unattended install and no guest IP discovery yet.

Apple does not list Windows as a supported guest, and Virtualization.framework has no TPM
([macos.md](macos.md#honest-limits)). Treat this as experimental.

## What you need

- An Apple silicon Mac, with the `vz` runner built (`cargo build -p fluxctl`).
- The Windows 11 ARM64 ISO from Microsoft.
- The [virtio-win](https://github.com/virtio-win/virtio-win-pkg-scripts) ISO. Windows has no in-box virtio-blk or
  virtio-net driver, so the installer cannot see the disk until you load `viostor` from it.

## Install

```bash
fluxctl create win11 --guest windows \
  --media ~/iso/Win11_ARM64.iso --virtio-win ~/iso/virtio-win.iso \
  --vcpus 4 --memory-mib 8192 --disk-size-gib 64 --window
```

`--image` is optional here: the installer ISO stands in for it, and the VM gets a blank sparse root disk of
`--disk-size-gib` (default 64, minimum 32). Both ISOs are attached read-only on USB.

In the installer:

1. At "Select where to install", choose **Load driver**, browse the virtio-win ISO to `viostor\w11\ARM64` and load it.
   The disk appears. (Load `NetKVM\w11\ARM64` too if you want the network during setup.)
2. If setup stops at "This PC can't run Windows 11" (no TPM), press **Shift+F10**, run `regedit`, and under
   `HKLM\SYSTEM\Setup` create the key `LabConfig` with DWORDs `BypassTPMCheck`, `BypassSecureBootCheck` and
   `BypassRAMCheck` set to `1`. Go back one step and continue.
3. After the first reboot, install the rest of the virtio-win drivers from the ISO.

## Limits

Refused at admission for Windows guests: `self_control`, `host_gpu`, `rosetta`, `clipboard`, `custom_virtio`,
`apple.install`, private `networks`, `nat_mtu`, `usernet`, `init_config`, `firstboot`, `egress_allow`,
`vsock_services`, `agent.enabled` and direct kernel boot.

Not done yet: unattended install, a guest agent, IP discovery, snapshots and golden-image cloning, TPM.
