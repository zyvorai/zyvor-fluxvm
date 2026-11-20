# vz feature completion examples

These specs exercise the Mac `vz` control-plane surface that is implemented in FluxVM today.

| File | Feature |
|------|---------|
| `macos-install-provisioned.json` | macOS IPSW install + native `provision_*` + first-boot account fallback |
| `linux-asif-overlay.json` | DiskImageKit ASIF overlay + custom Virtio + Rosetta + USB controller |
| `linux-vmnet-udp.json` | Named shared vmnet + TCP/UDP port forwards |
| `linux-nested-usb.json` | Nested virtualization + multi-USB controllers + clipboard/audio |

Apply the accompanying patch, then validate on a Mac:

```bash
fluxctl create -f examples/vz-complete/linux-asif-overlay.json
```

Hardware gates (entitlements / SIP) still apply for shared vmnet and physical USB passthrough.
