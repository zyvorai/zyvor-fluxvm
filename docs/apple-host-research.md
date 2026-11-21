# Apple host features: research, gaps and backlog

Researched 2026-11-21 (macOS 27.3 on an Apple M-series Mac mini; FluxVM at commit 358b97e1). Companion to
[macos.md](macos.md) and [macos-architecture.md](macos-architecture.md); the user-facing result is [macos-host.md](macos-host.md).

Every claim carries a tag:

- **[sourced]** a page I fetched or a search result I read, linked in the Sources list (date 2026-11-21).
- **[local]** this repository's own docs or code, or a command I ran read-only on the development Mac on 2026-11-21.
- **[inferred]** from memory, from a third-party summary, or by reasoning. Treat as a lead, not a fact.

Limits of this research. Apple's documentation site renders with JavaScript, so `WebFetch` returned only the framework index
(`developer.apple.com/documentation/virtualization.md`) and bare page titles for individual classes. Per-class availability versions
below therefore come from search results, third-party bindings and the repository, and are tagged accordingly. Nothing was
verified against a second Mac or a booted guest for this document.

## 1. Virtualization.framework: what it offers, what FluxVM does with it

The framework's own index lists these device topics: audio, graphics, keyboards and pointing devices, memory (balloon), network,
randomization, serial ports, shared directories, sockets, storage, consoles, clipboard sharing, USB devices and custom Virtio
drivers; and the articles "Running Intel Binaries in Linux VMs" and "Accelerating the performance of Rosetta" [sourced: 1].

| Capability | What Apple documents or reports | FluxVM today | Gap |
| --- | --- | --- | --- |
| Save / restore state | `saveMachineState(to:)` / `restoreMachineState(from:)` exist from macOS 14 [sourced: 2]. The VM must be paused; restore checks the saved configuration, including a persistent machine identifier [sourced: 2, 3]. A forum thread reports validation failing for a Linux guest with "Unsupported graphics device in configuration" [sourced: 4]. One project reports restore failing with "permission denied" on a locked Mac [sourced: 3]. | Linux and macOS guests snapshot and restore; restore needs an unlocked session; warm pool, fork, hibernate use it [local: macos.md "Honest limits"] | No pre-flight of `validateSaveRestoreSupport` result per config; no recorded host OS build to refuse a restore after a macOS update [inferred] |
| Rosetta for Linux | `VZLinuxRosettaDirectoryShare`, macOS 13+, Apple silicon; `availability` is `.installed`, `.notInstalled` or `.notSupported`; the guest mounts it as a virtiofs share and registers a binfmt handler [sourced: 5]. Apple also documents a TSO kernel option to speed it up [sourced: 1]. | `rosetta: true`, `rosetta_cache`, `runner install-rosetta`, `vz doctor` check, `kube up --rosetta` [local] | The availability value is contested on macOS 27 (see 1.1). TSO-enabled guest kernels are not provided or checked [inferred] |
| virtiofs | Directory sharing devices; Linux mounts by tag. [sourced: 1] | `shared_folders`, tagged shares, `share-set`, macOS automount share [local] | none noted |
| vsock | `VZVirtioSocketDevice`, port-based host/guest channel [sourced: 1] | Linux and (experimental) macOS agent over vsock, extra vsock services [local] | macOS guest agent verified on one M4 only [local] |
| Display / graphics | macOS guests need a `VZMacGraphicsDeviceConfiguration` (paravirtualised Metal, several displays); Linux guests use `VZVirtioGraphicsDeviceConfiguration` (virtio-gpu, 2D and basic 3D) [sourced: 6, third-party]. | 1 to 8 displays, `window: true`, display resize [local] | Multi-display unverified on hardware [local]. No GPU passthrough exists; paravirtual only [sourced: 6] |
| Clipboard | A "Clipboard sharing" topic exists [sourced: 1]; reported as SPICE agent based, macOS 13+, with Linux guests [sourced: 6]. A project reports UTM ships SPICE guest tools for macOS guests, so macOS may work with third-party tools [sourced: 6, unverified]. | Linux only via `spice-vdagent`; refused for macOS guests because the macOS 27 SDK shows no public API [local] | Unverified on hardware [local]. A vsock `pbcopy`/`pbpaste` bridge for macOS guests is a possible route [inferred] |
| Audio | An "Audio" topic: guests play and capture through the host's audio devices [sourced: 1] | Output on by default, `microphone` opt-in [local] | none noted |
| USB mass storage | A "USB Devices" topic [sourced: 1]; XHCI controller [local: macOS 15+ for hot-attach] | Disk-image hot-attach and detach; physical passthrough needs a restricted entitlement and is unverified [local] | Physical passthrough blocked on an Apple-issued profile [local] |
| Nested virtualization | `isNestedVirtualizationSupported`, macOS 15, M3 and later, Linux guests only (`VZGenericPlatformConfiguration`) [sourced: 7]. Reports of "not supported on this device" on macOS 15.2 show the flag must be checked [sourced: 7]. | `nested_virtualization: true` fails clearly if unsupported [local] | `vz doctor` does not print the support flag as its own line [inferred] |
| Memory balloon | A "Memory" topic for a balloon device that changes guest memory [sourced: 1] | Balloon API and idle reclaim; **reclaim unverified on hardware** [local] | Verify; no pressure-driven policy [inferred] |
| Networking | NAT works without special entitlements; bridged networking needs `com.apple.vm.networking`, a restricted entitlement that needs a provisioning profile Apple grants on request [sourced: 8, 9]. `vmnet` shared and host modes do not need it [sourced: 8]. The newer `VmnetNetworkDeviceAttachment` (macOS 26) is reported to need neither the entitlement nor root [sourced: 10]. | NAT, usernet gateway, vmnet and bridged modes; the ad-hoc signed broker and a runner claiming the entitlement are killed at launch on the test Mac [local: macos.md hardware log] | Whether the macOS 26 attachment avoids that kill is untested here (backlog 20) [inferred] |
| macOS guests | Apple silicon only; a licence limit of two concurrent macOS VMs [local: macos.md]; Apple's own page could not be fetched [unverified] | Enforced, template store, warm pool [local] | none |

### 1.1 Rosetta on macOS 27: conflicting reports, and a local observation

A UTM pull request reports that on macOS 27.0 the availability check always returns `.installed`, and that a clean macOS 27.0 VM lacked the
runtime file that only exists after an explicit install [sourced: 11]. A separate issue says the check returns installed with the files
under `/Library/Apple/usr/libexec/oah/RosettaLinux/` [sourced: 11, as summarised by search]. I ran `fluxctl mac host rosetta` on the
macOS 27.3 development Mac: the runner reported `installed` while `/Library/Apple/usr/libexec/oah/libRosettaRuntime` did not exist
[local, 2026-11-21]. That file is an inferred install marker, not an API [inferred], so `mac host rosetta` reports the disagreement as a
warning and says to boot a guest with `rosetta: true` to be sure, instead of picking a side.

## 2. Mac mini as a host

| Topic | Facts | Used by |
| --- | --- | --- |
| `caffeinate` | `-d` display, `-i` idle, `-m` disk, `-s` system sleep (valid only on AC power), `-u` user active, `-t` timeout, `-w pid` [sourced: 12]. It holds a process-scoped assertion that ends with the process [sourced: 13]. | `mac host keepawake` runs `caffeinate -i -m -s [-d]` under launchd [local] |
| `pmset` | Changing settings needs root; reading does not. `sleep`, `displaysleep`, `disksleep`, `womp` (Wake on LAN), `autorestart`, `powernap`, `ttyskeepawake`; `-g therm`, `-g assertions`, `-g custom`, `-g batt` read state [sourced: 14]. `disablesleep` is reported by third parties to disable all sleep until reset [sourced: 13]. | `mac host status`, `tune` |
| Sleep and wake | `pmset -g` lists what holds sleep off, for example `sleep 0 (sleep prevented by screensharingd, powerd, caffeinate)` [local capture]. | status, `headless-check` |
| Auto-login and FileVault | Automatic login is unavailable with FileVault on, by design; a setting named `DisableFDEAutoLogin` is documented by one source [sourced: 15]. FileVault state reads with `fdesetup status` without sudo [local]. After a reboot with FileVault on, the pre-boot unlock waits for a person [inferred]. `fdesetup authrestart` for a planned unattended restart [inferred; check `man fdesetup`]. | `headless-check` |
| Unlocked session | Restore of saved VM state needs an unlocked login session [local: launchd-notes.md]. The session's lock state is in the `IOConsoleUsers` entry of `ioreg -n Root -d1 -r` (`CGSSessionScreenIsLocked`). On the development Mac the top-level `IOConsoleLocked` said `No` while the session entry said `Yes`; the module decides on the session entry and prints both in `status --json` [local]. Which one is authoritative is unproven [inferred]. | `headless-check` |
| Headless display | A dummy HDMI/DisplayPort plug mostly fixes the low default resolution of Screen Sharing; reports differ on whether it is needed on Apple silicon [sourced: 16, anecdotal]. No public API to create a virtual display was found [inferred]. | `headless-check` warns when no display is online |
| Thermal and power | `pmset -g therm` says `No thermal warning level has been recorded` when nominal; a limit below 100 means throttling (this repo's `kube_policy` parser) [local]. A Mac mini has no battery: `pmset -g batt` reports `AC Power` [local]. `sysctl kern.memorystatus_vm_pressure_level` returned `1` [local; meaning of values inferred]. | `status` |
| launchd | VM restore needs a LaunchAgent in the user's session, not a LaunchDaemon [local: launchd-notes.md]. | `keepawake` is a LaunchAgent |

## 3. Apple `container` and the Containerization framework

- `apple/container` is a CLI that runs Linux containers as lightweight VMs; it needs an Apple silicon Mac and macOS 26 or later, and
  older macOS is unsupported [sourced: 17]. Its `BUILDING.md` allows macOS 15 as a minimum build host [sourced: search result of 17].
- `apple/containerization` is the Swift package under it: OCI images and registries, ext4 creation, each container in its own VM
  (optionally its own IP), the `vminitd` init reached over vsock, Rosetta support for `linux/amd64`, and a `VirtualMachineManager`
  abstraction with a Virtualization.framework backend on macOS [sourced: 18]. Its README does not mention memory ballooning,
  save/restore or vmnet; an open issue asks for pause-aware state and VZ save/restore [sourced: 18, 3].
- Comparison with FluxVM: FluxVM already has per-sandbox microVMs, snapshots with memory restore, forks and a warm pool [local],
  which the Containerization README does not describe [sourced: 18]. `container` targets one developer's containers on macOS 26+;
  FluxVM targets a managed fleet of VMs and macOS guests. Interop (sharing images, or using the framework as a backend) is a backlog item, not a recommendation.

## 4. Implemented from this research

`fluxctl mac host status | keepawake | headless-check | tune | rosetta` ([macos-host.md](macos-host.md)). Read-only except
`keepawake install|uninstall` (writes one LaunchAgent, no sudo) and `tune --apply --only NAME` (runs the named `sudo pmset -a` commands
the user asked for). Parsers are tested on output captured on the development Mac (`crates/fluxctl/src/mac_host_fixtures/`);
decisions and the apply path are tested against a fake host. Run read-only on that Mac, `headless-check` flagged its locked session and
FileVault, `tune` proposed two settings, and `rosetta` showed the runner/marker disagreement above.

## 5. Backlog

Effort: S under a day, M a few days, L a week or more. Risk: L low, M medium, H high. Items marked * are the suggested first ten.

| # | Item | Effort | Risk | Files |
| --- | --- | --- | --- | --- |
| 1* | Run `validateSaveRestoreSupport` per VM config before the first snapshot and return the framework's reason (the "Unsupported graphics device" case) | S | L | `crates/fluxvm-apple/runner/Runner.swift`, `crates/fluxvm-apple/src/runner.rs` |
| 2* | Refuse or cold-boot a restore when the saved state's macOS build differs from the host's; store the build with each snapshot | M | L | `crates/fluxvm-apple/src/runner.rs`, snapshot metadata in `fluxvm-core` |
| 3* | Daemon-side unlocked-session guard: check the session lock before a restore path and say "run `fluxctl mac host headless-check`" instead of a framework error | M | L | `crates/fluxvm-apple/src/runner.rs`, `control.rs` |
| 4* | Publish host facts (session unlocked, sleep held, thermal, free disk) in the `fluxvm-agent node` heartbeat so central avoids a locked or throttled Mac for `vz` | M | M | `crates/fluxvm-agent`, `crates/fluxvm-scheduler` (`apple_placement`) |
| 5* | Run `mac host headless-check --json` on every Mac from `mac-fleet doctor` and `status` | S | L | `crates/fluxctl/src/mac_fleet*.rs` |
| 6* | Test whether the macOS 26 `VmnetNetworkDeviceAttachment` runs ad-hoc signed without `com.apple.vm.networking`, which would unblock named vmnet networks without Apple's profile | M | M | `crates/fluxvm-apple/runner/VmnetOptions.swift`, `AdvancedNetwork.swift`, `docs/VMNET_BROKER.md` |
| 7* | Verify balloon reclaim on hardware and add a memory-pressure input (`kern.memorystatus_vm_pressure_level`) to idle reclaim and admission | M | M | `crates/fluxvm-apple`, `crates/fluxvm-scheduler` |
| 8* | Thermal-aware admission: reuse the `pmset -g therm` limit to stop starting new VMs while throttled | M | L | `crates/fluxctl/src/kube_policy.rs`, `crates/fluxvm-scheduler` |
| 9* | Tie the keep-awake assertion to the daemon: hold `IOPMAssertion` only while a VM runs (needs a runner or daemon change) instead of always | M | M | `crates/fluxvm-apple/src/runner.rs`, `mac_host.rs` |
| 10* | Pending-update guard: report `AutomaticallyInstallMacOSUpdates` and warn, because an automatic update reboots the Mac and (inferred) invalidates saved state | S | L | `crates/fluxctl/src/mac_host.rs` |
| 11 | Make `vz doctor`'s Rosetta check use the same runner-plus-marker logic as `mac host rosetta` | S | L | `crates/fluxctl/src/vz_doctor.rs` |
| 12 | Check guest images for a TSO-capable kernel and document Apple's Rosetta performance article | M | M | `scripts/build-oci-boot.sh`, `docs/macos.md` |
| 13 | Boot-verify Rosetta on macOS 27.x with a clean guest, since reports conflict | S | L | `scripts/oci-live-test.sh` |
| 14 | Print the nested-virtualization support flag as its own `vz doctor` line (M3+, macOS 15+, Linux guests) | S | L | `crates/fluxctl/src/vz_doctor.rs` |
| 15 | Verify SPICE clipboard for Linux guests on hardware | S | L | `scripts/vz-devices-live-test.sh` |
| 16 | A vsock clipboard bridge (`pbcopy`/`pbpaste`) for macOS guests | M | M | macOS guest agent under `crates/fluxvm-apple/macos` |
| 17 | Audio options: input device choice and mute, documented per guest | S | L | `crates/fluxvm-apple/runner/Runner.swift` |
| 18 | Verify multi-display macOS guests on hardware | S | L | `scripts/vz-display-resize-live-test.sh` |
| 19 | Pursue a Developer ID build with Apple's provisioning profile (USB passthrough, bridged and named vmnet) | L | H | `crates/fluxvm-apple/build.rs`, `Entitlements*.plist` |
| 20 | Wake-on-LAN helper for `mac-fleet`: send a magic packet to a sleeping node (`womp` is checked by `tune`) | M | L | `crates/fluxctl/src/mac_fleet*.rs` |
| 21 | `pmset repeat wakeorpoweron` support in `tune` for a scheduled wake (needs sudo, opt-in) | S | M | `crates/fluxctl/src/mac_host.rs` |
| 22 | Guided auto-login: print the exact `sysadminctl -autologin` steps; never take a password on a command line | S | M | `crates/fluxctl/src/mac_host.rs` |
| 23 | Optional `tune` item for the screen lock (a per-user `defaults write`), behind its own `--only` name and a security warning | S | M | `crates/fluxctl/src/mac_host.rs` |
| 24 | LaunchAgent hardening: `LimitLoadToSessionType`, `ThrottleInterval`, `ExitTimeOut` for every `dev.zyvor.fluxvm*` agent | S | L | `crates/fluxctl/src/launch_agent.rs`, `kube_apple`, `kube_host.rs` |
| 25 | Exclude VM disks and the image store from Spotlight and Time Machine (`mdutil`, `tmutil addexclusion`) as a `tune` item | S | L | `crates/fluxctl/src/mac_host.rs`, `kube_backup.rs` |
| 26 | `GET /v1/host/power`: the status JSON through the daemon API and a `/metrics` gauge for locked session and sleep | M | L | `crates/fluxvm-api`, `crates/fluxctl/src/mac_host.rs` |
| 27 | FileVault planned-restart helper around `fdesetup authrestart` (password via stdin only) | M | M | `crates/fluxctl/src/mac_host.rs` |
| 28 | Evaluate `apple/container` image and registry interoperability with `fluxctl image` | M | L | `docs/`, `crates/fluxvm-image` |
| 29 | Evaluate the Containerization package as an alternative backend for OCI sandboxes (macOS 26+ only, Swift) | L | H | new crate or runner |
| 30 | Watch upstream issue on pause-aware VZ state in `apple/containerization` for design overlap | S | L | `docs/` |
| 31 | Battery and charge reporting in `status` for laptop hosts | S | L | `crates/fluxctl/src/mac_host.rs` |
| 32 | Doctor check that the console user equals the daemon's user (already warned as "session owner") for the fleet node agent | S | L | `crates/fluxctl/src/node_agent.rs` |

## Sources

1. Apple, Virtualization framework index, https://developer.apple.com/documentation/virtualization.md (fetched 2026-11-21)
2. Apple Developer Forums thread 745168, https://developer.apple.com/forums/thread/745168 (search result 2026-11-21)
3. apple/containerization issue 998, https://github.com/apple/containerization/issues/998; zyvorai/zyvor-fluxvm PR 162 (search results 2026-11-21)
4. Same forum thread 745168 (search summary of the "Unsupported graphics device" report)
5. WWDC22 session 10002, https://developer.apple.com/videos/play/wwdc2022/10002/ (search result 2026-11-21)
6. How Apple's Virtualization Framework Works, Device by Device, https://www.veloworkspaces.com/blog/how-virtualization-framework-works/ ; Code-Hex/vz docs, https://pkg.go.dev/github.com/Code-Hex/vz/v3 (third party, 2026-11-21)
7. Lima discussion 1777, https://github.com/lima-vm/lima/discussions/1777 ; hada.io summary, https://news.hada.io/topic?id=15318 (2026-11-21)
8. Kernova issue 90, https://github.com/nicholas-lonsinger/kernova/issues/90
9. Apple Developer Forums 656411 and 710763, https://developer.apple.com/forums/thread/656411
10. Code-Hex/vz PR 205, https://github.com/Code-Hex/vz/pull/205
11. UTM PR 8018, https://github.com/utmapp/UTM/pull/8018 (search result 2026-11-21)
12. https://ss64.com/mac/caffeinate.html (fetched 2026-11-21)
13. https://keepmacawake.com/blog/terminal-command-keep-mac-awake and https://chris-parmer.com/pmset-a-disable-sleep/ (search results)
14. https://ss64.com/mac/pmset.html (fetched 2026-11-21)
15. https://discussions.apple.com/thread/256199871 ; https://oper.io/?p=Circumventing_MacOS_FileVault_Autologin_Restrictions (search results)
16. https://forums.macrumors.com/threads/anyone-using-his-mac-mini-headless.2231131/ ; https://www.podfeet.com/blog/2025/07/virtual-monitor-dongle/ (search results)
17. https://github.com/apple/container (fetched 2026-11-21)
18. https://github.com/apple/containerization (fetched 2026-11-21)
