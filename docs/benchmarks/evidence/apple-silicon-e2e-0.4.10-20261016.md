# Apple silicon E2E 0.4.10 — 2026-10-16T11:39:59Z

Host: arm64 macOS 27.2; `fluxctl` 0.4.10 (Homebrew); unlocked login session; daemon on `127.0.0.1:7788`.

| Suite | Result | Notes |
|---|---|---|
| macos-live-test | PASS | |
| oci-live-test | PASS | `FLUXVM_OCI_BOOT_DIR` = state `oci/boot` |
| oci-udp-ports | PASS | |
| vz-devices | PASS | |
| vz-usernet | PASS | |
| vz-sandbox-volume | PASS | |
| vz-vsock-services | FAIL | BrokenPipe on AF_VSOCK CID 2 port 5001 after SSH ok |
| vz-asif-overlay | PASS | |
| vz-usb-buses | FAIL | guest sees empty disk list after hot-attach on bus 0/1 |
| host-gpu (local-model STAND_IN) | PASS | |
| vz-host-gpu-live-test | PASS | `scripts/vz-host-gpu-live-test.sh` |
| vz-rewind | PASS | |
| vz-macos-session | PASS | IPSW `macos-26A434`; 13/13 checks |
| vz-agent-session | SKIP | no `agent-micro-arm64.raw` on host |
| vz-https | SKIP | no `agent-micro-arm64.raw` on host |
| macos-guest-live | SKIP | needs template `disk.raw` + SSH key (session covered via IPSW) |
| Developer ID package | PASS | `FLUXVM_SIGN_IDENTITY` NX667LTDQ2; hardened runtime |
| Notarization | BLOCKED | no App Store Connect API key / `notarytool` profile; no `MACOS_NOTARY_*` secrets |
| Provisioning profiles | MISSING | runner / vmnetd / usbd profiles not on host |
| Mac App Store | NON-GOAL | Developer ID + Homebrew / GitHub only |

Logs under `/tmp/fluxvm-e2e-0410/` on the test Mac.
