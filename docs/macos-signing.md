# Signing FluxVM for macOS

Out of the box, every FluxVM binary on a Mac is signed **ad hoc**. That is enough for Linux and macOS VMs, NAT,
`apple.usernet`, private `apple.networks`, shared folders, Rosetta, snapshots and USB disk images.

Three features need entitlements that Apple restricts:

| Feature | Entitlement | Which binary |
|---|---|---|
| `apple.vmnet` networks, the shared vmnet broker, tenant networks (`isolate_tenant_networks`) | `com.apple.vm.networking` | the runner, and the broker `fluxvm-vmnetd` |
| `apple.bridge_interface` (VMs directly on the LAN) | `com.apple.vm.networking` | the runner |
| Physical USB passthrough (Accessory Access) | `com.apple.developer.accessory-access.usb` | `FluxVMUSBAccess.app` |

macOS honours a restricted entitlement only when three things are all true:
1. the executable is signed with a **Developer ID** certificate;
2. it sits in an **app bundle that embeds a provisioning profile** granting that entitlement;
3. it is **launched from that bundle**.

An ad hoc binary that claims one of them is killed at launch (exit 137). This is what happened to `fluxvm-vmnetd` and
the USB helper on the test Mac.

## What the release package contains

`scripts/package-macos.sh` builds and signs everything:

```
fluxvm-<version>-macos-arm64/
  bin/fluxctl, fluxvm-vz-switch, fluxvm-blockd, fluxvm-usernet
  bin/fluxvm-vz-runner                  virtualization entitlement only (always works)
  bin/FluxVMRunner.app                  only with a runner profile: + com.apple.vm.networking
  libexec/FluxVMNetworking.app          the vmnet broker (+ vm.networking with its profile)
  libexec/FluxVMUSBAccess.app           the USB consent helper (+ accessory access with its profile); only when the build
                                        SDK has the AccessoryAccess framework
  share/launchd/*.plist                 launch agents for the two helpers (__PREFIX__ = the unpacked package)
```

The daemon picks `bin/FluxVMRunner.app/Contents/MacOS/fluxvm-vz-runner` when it exists and falls back to the plain
runner. `FLUXVM_VZ_RUNNER` still overrides both. Without a profile, a bundle is signed without its restricted
entitlement: it is shipped, starts, and admission refuses the features above with a clear error, exactly as today.

**The USB helper is optional in a package.** It links Apple's `AccessoryAccess` framework, which is in the macOS 27 SDK
but not in every Xcode a hosted CI runner has. `scripts/package-macos.sh` checks that the SDK can import it and, when it
cannot (or with `FLUXVM_SKIP_USBD=1`), builds the package without `FluxVMUSBAccess.app` and its launch agent and says so;
the release workflow's summary notes it too. Everything else in the package is unchanged, and USB passthrough then
needs `crates/fluxvm-apple/macos/usbd/build-install.sh` on a Mac with the SDK (and, for claiming devices, a profile).

## One-time Apple setup

1. **Developer ID Application certificate** for the team, in Certificates, Identifiers & Profiles. Export it with its
   private key as a `.p12`.
2. **Ask Apple for the restricted entitlements.**
   - `com.apple.vm.networking` is granted on request, per team, through Apple's entitlement request form for
     virtualization networking. Explain the use: a VM manager that attaches guests to vmnet and bridged networks.
   - Accessory Access USB is a capability you enable for the App ID once your team has it.
   - Neither can be self-granted, and approval can take a while.
3. **App IDs** (explicit, not wildcard): `dev.zyvor.fluxvm.runner`, `dev.zyvor.fluxvm.vmnetd` and
   `dev.zyvor.fluxvm.usbd`. Enable the granted capability on each.
4. **Provisioning profiles** of type *Developer ID* for each App ID. Download the three `.provisionprofile` files.
5. **Notarization credentials.** Either an App Store Connect API key (`.p8`, key id, issuer id), or
   `xcrun notarytool store-credentials` for local use.

## Building a signed package locally

```bash
export FLUXVM_SIGN_IDENTITY="Developer ID Application: Zyvor AI Labs (ABCDE12345)"
export FLUXVM_PROFILE_RUNNER=~/profiles/FluxVM_Runner.provisionprofile
export FLUXVM_PROFILE_VMNETD=~/profiles/FluxVM_Networking.provisionprofile
export FLUXVM_PROFILE_USBD=~/profiles/FluxVM_USB.provisionprofile
export FLUXVM_NOTARIZE=1 FLUXVM_NOTARY_PROFILE=fluxvm-notary
scripts/package-macos.sh --version 0.5.0
```

Before signing, the script checks each profile:
- it was issued for the right bundle id;
- it grants the entitlement being claimed;
- it belongs to the same team as the identity.

It then:
1. writes the entitlements, including `com.apple.application-identifier` and `com.apple.developer.team-identifier`;
2. signs with the hardened runtime and a secure timestamp;
3. verifies every signature;
4. notarizes the whole package and staples the three bundles.

Each profile is optional. Leave one out and that bundle is built without its restricted entitlement.

## In CI

`.github/workflows/release-macos.yml` signs and notarizes when these repository secrets exist. Without them it builds
the ad hoc package as before, so forks still work.

| Secret | Content |
|---|---|
| `MACOS_DEVELOPER_ID_P12` | base64 of the Developer ID Application `.p12` |
| `MACOS_DEVELOPER_ID_P12_PASSWORD` | its password |
| `MACOS_PROFILE_RUNNER`, `MACOS_PROFILE_VMNETD`, `MACOS_PROFILE_USBD` | base64 of each `.provisionprofile` (each optional) |
| `MACOS_NOTARY_KEY_P8` | base64 of the App Store Connect API key |
| `MACOS_NOTARY_KEY_ID`, `MACOS_NOTARY_ISSUER` | its key id and issuer id |

The certificate goes into a temporary keychain that is deleted at the end of the job. The smoke test then:
- verifies every bundle;
- runs `stapler validate` and `spctl` on each bundle when the package was notarized;
- checks that `FluxVMRunner.app` carries `com.apple.vm.networking`.

## Installing the helpers from a package

```bash
FLUXVM_PACKAGE_DIR=/opt/homebrew/opt/fluxvm crates/fluxvm-apple/macos/vmnetd/build-install.sh
FLUXVM_PACKAGE_DIR=/opt/homebrew/opt/fluxvm crates/fluxvm-apple/macos/usbd/build-install.sh
fluxctl vz host        # vm_networking_entitlement should now be true
```

Run without `FLUXVM_PACKAGE_DIR`, the scripts still build and sign ad hoc from source for development. The broker is then
signed with the virtualization entitlement only, so it starts, but VMs cannot use its networks.

## Status

The scripts and the workflow are written but **not yet run on a Mac**: no certificate, profile or Apple approval exists
yet. The first signed release is the real test. Check `fluxctl vz host`, a VM on `apple.vmnet`, two VMs on one named
network through the broker, and USB passthrough. Those are the production gates in
[macos-vz27-remaining-complete.md](macos-vz27-remaining-complete.md).
