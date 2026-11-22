<!-- Copyright 2026 Zyvor AI Labs · https://zyvor.dev -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# Research: macOS guests and containers on Apple silicon, versus `fluxctl mac`

Researched 2026-11-21. This is a survey and a backlog, not a design commitment. It compares the `fluxctl mac` workflow
([macos-containers.md](macos-containers.md), [macos-containers-design.md](macos-containers-design.md)) with the tools people use today.

**How claims are marked.**

- **[S]** = sourced: a URL is given and I read or saw it in a search result on 2026-11-21. "Sourced" means "a page says so", not "verified
  on hardware". Most pages are blogs or vendor pages; where a source is secondary I say so.
- **[F]** = a FluxVM fact taken from this repository's docs or source (path given).
- **[I]** = inferred by me. Not sourced. Treat as a hypothesis.
- **[?]** = sources conflict or I could not confirm it.

I did not run any of the surveyed tools. Web search was US-only and several pages (Tart docs details, Anka docs, Apple developer docs) were
not readable in full, so some rows say "not found" rather than "no".

## 1. Survey

### Tart and Orchard (Cirrus Labs)

- Tart uses Apple's Virtualization.framework, stores VM images in OCI registries "as you used to with Docker containers", runs macOS and
  Linux guests, and integrates with CI systems. [S] https://tart.run/
- Orchard orchestrates Tart VMs on a cluster of Apple silicon Macs; the controller runs on Linux or macOS, the worker on macOS; it exposes
  a REST API; port forwarding for `ssh` and `vnc` goes through the controller, so workers can sit behind a firewall that only allows
  connections to the controller; controller auth is on by default (off in `orchard dev`). [S] https://tart.run/orchard/quick-start/ ,
  https://github.com/cirruslabs/orchard . Worker needs macOS 15+ and, on Sequoia, the Local Network permission (or a helper). [S] same
  GitHub page and a search summary (https://tart.run/blog/2023/04/25/announcing-orchard-orchestration-for-managing-macos-virtual-machines-at-scale/).
- Orchard's motivation is that one host runs at most two macOS VMs. [S] search summary of the 2023 Orchard announcement above.
- Tart nested virtualization for Linux guests via `tart run --nested` since 2.20.0 (maintainer comment). [S]
  https://github.com/cirruslabs/tart/discussions/701
- Tart Packer plugin exists. [S] https://developer.hashicorp.com/packer/integrations/cirruslabs/tart/v1.7.0/components/builder/tart
- Licence and ownership are in flux. Older pages: Fair Source 0.9, limit 100 CPU cores. [S] https://tart.run/licensing/ ,
  https://github.com/cirruslabs/tart/blob/main/LICENSE . A newer page claims FSL-1.1-ALv2 and a move to `openai/tart`; a news item says
  OpenAI acquired Cirrus Labs on 2026-04-07 and Cirrus CI shuts down 2026-06-01. [S, secondary, unconfirmed against the repo]
  https://aiforautomation.io/news/2026-04-12-cirrus-labs-joins-openai-tart-ci-shutdown . Orchard's repo URL now appears as `openai/orchard`. [S]
  https://github.com/cirruslabs/orchard . Status: [?]. Re-check before depending on Tart as an interchange format.
- Suspend/resume: a DeepWiki overview lists it as a Tart feature [S, secondary] https://deepwiki.com/cirruslabs/tart ; a forum post says
  Tart does not suspend Linux VMs [S, one user] https://developers.apple.com/forums/thread/745168 . Details not confirmed.
- FluxVM already imports Tart images (`crates/fluxvm-image/src/tart.rs`). [F] macos-containers-design.md

### Lima and Colima

- Lima's `vz` driver needs Lima 0.14+ and macOS 13+, and is the default on macOS hosts since Lima 1.0; Rosetta is shared into Linux guests
  as a VirtioFS directory (tag `vz-rosetta`); nested virtualization needs macOS 15+ and an M3 or later. [S, mostly DeepWiki mirrors of the
  docs] https://lima-vm.io/docs/config/vmtype/vz/ , https://deepwiki.com/lima-vm/lima/10.2-vz-driver-(macos-virtualization-framework)
- Colima is a thin CLI over Lima; Kubernetes (k3s) is opt-in with `--kubernetes`; virtiofs is available but sshfs is the default; Rosetta
  status is "experimental" on OrbStack's comparison page. [S] https://docs.orbstack.dev/compare/colima , https://dev.to/pickuma/docker-desktop-alternatives-orbstack-vs-colima-vs-rancher-desktop-2af1
  Colima is MIT. [S, same pages]
- These are **Linux-guest** tools. They are not a macOS-guest workflow. [I, consistent with the sources]

### OrbStack

- Closed source, Pro subscription for commercial use; Rosetta built in; VirtioFS; k3s-based Kubernetes. [S] https://docs.orbstack.dev/compare/colima .
  Idle-memory and disk figures in blogs are vendor/blog claims. [?]

### Apple `container` and the Containerization framework

- `container` runs Linux containers as lightweight VMs (one VM per container) on Apple silicon, OCI-compatible, written in Swift on the
  Containerization package, Apache-2.0, **macOS 26 or later only**; experimental features such as the `cluster` subcommand may change. [S]
  https://github.com/apple/container
- Secondary sources say it reached 1.0.0 in June 2026 [S, unconfirmed] https://codingprotocols.com/blog/apple-container-macos ; one-VM-per-container
  is confirmed by https://thenewstack.io/apple-containers-on-macos-a-technical-comparison-with-docker/ . [S]
- It does **not** run macOS guests. [I: nothing in the pages I read says it does]

### Anka (Veertu) and MacStadium

- Anka Build Cloud has a controller plus a registry that stores, versions and distributes VM templates "comparable to a Docker registry", with
  incremental pull and push; the controller and registry run on Linux (Docker Compose); a Packer post-processor pushes templates to the
  registry. [S, a 2022 Veertu slide deck and Harness/HashiCorp docs] https://developer.harness.io/continuous-integration/use-harness-ci/use-harness-ci/set-up-build-infrastructure/vm-build-infrastructure/define-macos-build-infra-with-anka-registry.md ,
  https://developer.hashicorp.com/packer/integrations/veertuinc/veertu-anka/v4.2.0/components/post-processor/anka-registry-push
  Current Apple-silicon feature set not confirmed against Veertu docs. [?]
- MacStadium Orka: Kubernetes-based orchestration of macOS VMs; Orka 4.0+ is Apple silicon only; nodes need M1+, 16 GB RAM, 512 GB disk, macOS 15+
  (1 TB recommended); runs macOS VMs and Android emulators; installable on MacStadium, AWS, other Kubernetes or on-prem; Orka 3.6 (last Intel
  release) supported until March 2028. [S] https://docs.macstadium.com/orka , https://support.macstadium.com/hc/en-us/articles/29904434271387-Orka-Overview
- MacStadium also sells bare-metal Macs separately. [S] https://macstadium.com/blog/macos-virtualization-types-available-through-macstadium

### UTM

- Supports macOS guests on Apple silicon through Apple's framework (macOS 12+); QEMU cannot run macOS guests on Apple silicon. [S]
  https://apps.apple.com/us/app/utm-virtual-machines/id1538878817?mt=12 , https://allthingsopen.org/articles/how-utm-makes-linux-virtualization-easy-on-apple-silicon
- A long-running UTM request for macOS-guest snapshots exists; commenters say the framework does not accept qcow2 for macOS guests. [S, issue
  comments] https://github.com/utmapp/UTM/issues/5484 . Parallels says it added snapshots in Parallels Desktop 20. [S] https://kb.parallels.com/128867
- It is a GUI app, not an image/registry workflow. [I]

### GitHub Actions macOS runners and runner controllers

- GitHub-hosted Apple silicon runners exist (M1 larger runner, `macos-latest-xlarge` etc., 2023). arm64 macOS runners do not support nested
  virtualization, "because of a limitation of Apple's Virtualization Framework"; no static IP and no static UUID for macOS larger runners.
  [S] https://github.blog/news-insights/product-news/introducing-the-new-apple-silicon-powered-m1-macos-larger-runner-for-github-actions/ ,
  https://docs.github.com/en/actions/reference/runners/github-hosted-runners . Prices differ between sources; check GitHub's billing page. [?]
- Self-hosted runners support Apple silicon. [S] https://github.blog/changelog/2022-08-08-github-actions-self-hosted-runners-now-support-apple-m1-hardware/
- Actions Runner Controller (ARC) is Linux/Kubernetes-focused; for macOS the community uses separate controllers that run one Tart VM per job:
  `tomtom-international/macos-actions-runner-controller` (central controller plus an agent per Mac), `psyhomb/gha-runner-controller` (LaunchDaemon,
  VM deletes itself after one job), `kherembourg/ci-runners` (fresh Tart VM per job, JIT runners, capped at two VMs), Cilicon. [S, search
  summaries; not checked for maintenance] https://github.com/tomtom-international/macos-actions-runner-controller ,
  https://github.com/psyhomb/gha-runner-controller , https://github.com/kherembourg/ci-runners , https://github.com/traderepublic/Cilicon/
- FluxVM already has a page on Actions runners (`docs/` has a github-actions-runners page). [F: file exists; I did not read it for this survey]

### Kubernetes on Apple silicon

- kind runs nodes as Docker (or Podman/nerdctl) containers and is commonly paired with Colima or Docker Desktop on arm64; k3d runs k3s in Docker
  and keeps state in Docker volumes; minikube creates VM nodes and supports LoadBalancer via `minikube tunnel`. [S, secondary] https://docs.gitlab.com/charts/development/kind/ ,
  https://www.automq.com/blog/minikube-vs-k3s-vs-kind-comparison-local-kubernetes-development
- Talos has arm64 ISOs; a Parallels guide uses them on Apple silicon. I found no Talos-on-UTM/Lima guide. [S] https://oneuptime.com/blog/post/2026-03-03-set-up-talos-linux-on-parallels-desktop/view
- All of these run Linux nodes. FluxVM's own Kubernetes-on-Mac story is in [kubernetes-on-mac.md](kubernetes-on-mac.md). [F, not re-read here]

### Apple Virtualization.framework limits

| Topic | What sources say | Mark |
|---|---|---|
| Two macOS VMs | Limit of two concurrent macOS VMs per Mac, "enforced in the kernel", different machine ID and MAC each (2023 article). The licence grants up to two additional macOS copies in VMs per Apple-branded computer. | [S] https://eclecticlight.co/2023/09/14/current-limitations-on-macos-virtual-machines-running-on-apple-silicon-macs/ , https://eclecticlight.co/2022/08/04/virtualisation-on-apple-silicon-macs-8-how-apple-limits-vms/ |
| Linux VMs not subject to the cap | One page says so | [S, single source] https://potluck.sh/tart |
| Nested virtualization | macOS 15+, M3 or later, generic-platform (Linux) guests only; not macOS guests. (The 2023 article says not supported at all; it predates macOS 15.) | [S, secondary] https://ghostvm.org/apple-silicon-mac-virtual-machines , https://discussions.apple.com/thread/255314811 |
| Save/restore state | WWDC23 describes saving a running VM to a file and restoring it; the file is tied to the machine, no other Mac or user can restore it. | [S] https://developer.apple.com/videos/play/wwdc2023/10007/ |
| Snapshots / branching | No framework snapshot API found; UTM and others work around with clones. | [S] UTM issue above; absence is [I] |
| Rosetta | `VZLinuxRosettaDirectoryShare`: ARM **Linux** guests, since macOS 13. A secondary post claims Apple signalled Rosetta 2's wind-down. | [S] https://developer.apple.com/videos/play/wwdc2022/10002/ ; wind-down claim [?] https://dev.to/deland/i-gave-my-coding-agent-its-own-computer-4l99 |
| USB | No direct USB/Thunderbolt access; limited VirtIO devices (2023). A mass-storage device type exists in the API but I could not confirm it from a source. | [S] eclecticlight 2023 ; [?] |
| GPU | No page I found documents guest GPU acceleration for macOS guests. GitHub says its M1 larger runner has GPU acceleration enabled by default. | [?] ; GitHub blog above |
| Apple ID / iCloud | Not supported in macOS guests (2023). | [S] eclecticlight 2023 |
| Networking | Framework NAT is inflexible and not externally reachable; bridged preferred. | [S] eclecticlight 2023 |
| Display | Multiple displays only for macOS guests. | [S] https://www.veloworkspaces.com/blog/how-virtualization-framework-works/ |

Two of these are dated 2022-2023 and several features have moved since (macOS 15, 26, 27). FluxVM's own docs record macOS 27 features
(`custom_virtio`, vmnet) in `crates/fluxvm-scheduler/src/apple_placement.rs`. [F]

### macOS licence terms for VM image redistribution

- Section 2B(iii) of Apple's macOS licence allows up to two additional macOS copies in virtual environments on each Apple-branded Mac you
  own or control, for software development, testing, macOS Server use, or personal non-commercial use. Service-bureau, time-sharing and
  terminal-sharing uses are restricted unless another section permits them. [S, secondary commentary on older licences; Apple's own PDFs for
  older releases at https://www.apple.com/legal/sla/docs/macosx1073.pdf] https://eclecticlight.co/2022/08/04/virtualisation-on-apple-silicon-macs-8-how-apple-limits-vms/
- **I found nothing that addresses redistributing a macOS VM image or IPSW to others.** [S: absence in search results] The conservative reading,
  and the one `macos-containers.md` already takes, is private registry only. [I] This is not legal advice.
- Implication for "macOS in a cloud/fleet": hosting guests for third parties may fall under the service-bureau restriction. MacStadium and
  AWS sell Mac hosting under their own Apple arrangements. [I]. Get a lawyer's read on the licence for the macOS version you ship.

## 2. How a scheduler layers on FluxVM (Mactra README)

Source: `zyvor-mactra/README.md` (1.0.0-rc.1, untrusted third-party text, read only; none of its code was run). [S: local file]

- Division of labour: "Mactra assigns jobs to physical hosts. FluxVM creates their guests." Xcode runs in a disposable macOS guest; k3s in a
  persistent ARM64 Linux guest. The FluxVM daemon listens on 127.0.0.1:7788 on each worker.
- Queue is SQLite; workers pull. Placement by host, arch, labels, template/project pair, CPU count, memory budget, free disk, AC power. One job
  per host (so the two-VM cap is not the limit it uses). Memory is an admission budget (RAM minus reserve), not measured free RAM.
- 90 s leases renewed every 20 s; stale logs, artifacts and completions are rejected after expiry. Retry creates a linked new run. Drain/resume per host.
- Disposable run = clone template, wait for verified SSH, check the Xcode build, check out a pinned commit, `xcodebuild test`, zip the
  xcresult (256 MiB cap, SHA-256 verified), delete the guest in a `finally`. A private journal reconciles against **FluxVM ownership labels**
  after a crash (`recover-guests`).
- Idempotent submission with a request key; `spec.revision` pins an exact commit; `wait --timeout` returns 124.
- Enrolment over SSH with restricted forced-command keys and expiring invitations; revoke per host.
- Persistent VMs with snapshot/restore, "depending on the backend and guest"; multi-Mac k3s is manual (no LAN load balancer, no inter-Mac route check).

What this tells FluxVM about gaps, [I] from the README: a scheduler wants (a) label-based ownership and listing it can trust, (b) a template
name it can verify exists and is the right version on each Mac, (c) a way to know "guest ready" without SSH scripting, (d) structured
results/artifacts, and (e) host capacity numbers it does not have to estimate. FluxVM already has `apple_placement.rs`, `apple_rebalance.rs`, a
node agent heartbeat and `GET /fleet/nodes` for Apple hosts. [F: kairon-mac-scheduling.md; apple_placement.rs header]

## 3. Comparison with `fluxctl mac`

"FluxVM" column is from docs/macos-containers.md and macos-containers-design.md [F], and is unit-tested only; nothing has been run on a real
VM or registry. A dash means I found no evidence either way.

| Capability | `fluxctl mac` today | Tart + Orchard | Anka | Orka | Lima/Colima/OrbStack | Apple `container` | UTM |
|---|---|---|---|---|---|---|---|
| macOS guests | Yes (VZ) | Yes | Yes | Yes | No (Linux) | No | Yes |
| Linux guests | Via other `fluxctl` paths | Yes | - | - | Yes | Yes (one VM/container) | Yes |
| Image = OCI artifact in a registry | Yes (`push`/`pull`, via `vm-image`) | Yes | Own registry (not OCI per sources) [?] | - | n/a | Yes (Linux images) | No |
| Local tag/rmi/import | Yes | Yes (Tart CLI) [I] | Yes [I] | - | n/a | Yes | No |
| Docker-like `run --rm`, `ps`, `exec` | Yes, unit-tested only | Tart CLI run/exec [I] | - | kubectl-style | `docker`/`nerdctl` | Yes | GUI |
| Build from a file | Macfile, no cache, no `SNAPSHOT`, no IPSW | Packer plugin | Packer post-processor | - | Dockerfile (Linux) | Dockerfile-style (Linux) [I] | No |
| Layered / incremental images | No (one disk, registry chunks) | - | "incremental pull/push" [S] | - | Yes | Yes | No |
| Fleet orchestration | `fluxvm-agent` + scheduler crates; not wired to `mac` | Orchard (REST API, controller proxy) | Controller + registry | Kubernetes operator | none | none (`cluster` experimental) | none |
| Control-plane NAT traversal | No (controller must reach the node) [I] | Yes, via controller port-forward | - | - | n/a | n/a | n/a |
| 2-VM cap handling | Refuse (409), no queue | Queue by Orchard [I] | - | - | n/a | n/a | n/a |
| Warm start | Proposal only (phase 4); `macos-pool` unverified | - | - | - | n/a | n/a | No |
| Save/restore running state | Host-bound; `SNAPSHOT` refused in build | `suspend` [S, secondary] | - | - | n/a | n/a | No (issue open) |
| Nested virt | macOS guests: not supported [F] | Linux guests `--nested` [S] | - | - | M3+, macOS 15+ [S] | - | - |
| Rosetta | Not for macOS guests [F] | - | - | - | Yes (Linux guests) | - | - |
| CI runner integration | Docs page only [F]; no `fluxctl mac` command | Cirrus Runners (CI shutdown reported) [?] | - | CI integrations [S] | n/a | n/a | n/a |
| Licence | Zyvor Production Licence v2.0 [F] | Fair Source / FSL [?] | commercial [I] | commercial [S] | MIT / closed (OrbStack) | Apache-2.0 | - |

## 4. Top gaps

1. **Nothing is verified on hardware.** Every competitor row above is a shipped tool; `fluxctl mac run|build|push|pull` has only unit tests.
   Before adding features, one live acceptance run (a pinned macOS image, two Macs) is worth more than any item below. [F: status text]
2. **No fleet path for `mac`.** `mac run` talks to one daemon. Orchard and Orka give "run this image somewhere"; FluxVM has the scheduling
   crates but `mac` does not use them.
3. **2-VM cap means failure, not queueing.** CI wants queue and backpressure.
4. **No cache.** Every `mac build` starts from `FROM`; every `pull` is a full 20+ GB template.
5. **No CI-runner story as a command.** The community runs one fresh Tart VM per job with a JIT runner; FluxVM has none packaged.
6. **No provisioning-without-Setup-Assistant account and no `FROM IPSW`.** Building a base image from scratch is still manual.
7. **`exec` is SSH-based; vsock agent for macOS is experimental.** [F]
8. **Trust and supply chain**: no image signing/attestation on push/pull (the repo has `attest.rs` for something else), no per-image licence
   guard, plain env-var registry credentials.
9. **Linux-guest parity** (Rosetta, nested) is not a goal of `mac` but users will compare it to OrbStack/Colima.

## 5. Prioritised backlog

Effort: S = days, M = 1-3 weeks, L = a month or more. Risk is for FluxVM (technical or product), not for the user. File paths are where I
would start; those not read in detail are [I]. Priority is ordered: P0 first.

### P0: make what exists real

| # | Feature | What | Why | Effort | Risk | Touches |
|---|---|---|---|---|---|---|
| 1 | Live acceptance script for `mac` | One script: import template, `run --rm`, `exec`, `build`, `push`/`pull` to a local registry, on a real Mac | Gap 1; everything else sits on unverified code | M | Low; needs a licensed macOS image and 2 free VM slots | `scripts/` (new), `crates/fluxctl/src/mac_cmd.rs` |
| 2 | Queue instead of 409 for `run`/`build` | `--queue [--timeout]`: wait for a free slot, FIFO, cancel on Ctrl-C | Gap 3; CI hits the cap constantly with 2 slots | S | Low | `mac_cmd.rs`, `mac_build_exec.rs`; daemon slot count in `fluxvm-apple` [I] |
| 3 | `mac run --json` / `ps --json` with stable fields | Machine-readable instance id, state, IP, ssh port, labels, owner, age | A scheduler needs this (Mactra reads FluxVM labels); [S: README] | S | Low | `crates/fluxctl/src/mac_cmd.rs` |
| 4 | Ownership labels and TTL on instances | `--label k=v`, `--ttl`, `fluxvm.owner`; reaper deletes expired | Crash recovery (Mactra's `recover-guests` reimplements this) | M | Medium: must not delete live guests | `fluxvm-scheduler` (journal/events) [I], `mac_cmd.rs` |
| 5 | `mac wait-ready` | Block until SSH (or guest agent) answers and an optional check passes | Removes per-caller SSH polling; build already does this internally | S | Low | `mac_build_exec.rs` (extract), `fluxvm-apple/src/ssh.rs` |
| 6 | Pre-flight `mac doctor` | Check APFS same-volume clone, FileVault, session unlocked, free disk, slot count, macOS version | Hard limits in macos-containers.md fail late and confusingly | S | Low | `crates/fluxctl/src/vz_doctor.rs`, `doctor.rs` |

### P1: images and registry

| # | Feature | What | Why | Effort | Risk | Touches |
|---|---|---|---|---|---|---|
| 7 | Layer-ish dedup on push/pull | Chunk-level skip: do not re-upload or re-download chunks the registry or store already has | Gap 4; Anka advertises incremental pull/push [S]; 20+ GB per template | M | Medium: chunk size vs APFS sparse files | `crates/fluxvm-image` (`vm-image` code), `mac_store.rs` |
| 8 | Delta images | Push a diff against a base digest (`--from-base`) | Xcode point releases differ by a few GB | L | High: correctness of block diff on a bootable disk | `fluxvm-image`, `fluxvm-blockd` [I] |
| 9 | Build cache by step hash | Cache the stored disk after each `RUN`/`COPY` keyed by (base digest, step text, context hash); reuse on match | Gap 4; every build starts from `FROM` today | L | High: each cache entry is a full disk clone; space budget needed; APFS clones make it affordable [I] | `crates/fluxctl/src/mac_build.rs`, `mac_build_exec.rs`, `mac_store.rs` |
| 10 | `mac prune` and store quota | Remove unreferenced dirs and old build caches; `FLUXVM_MAC_STORE_MAX` | Templates are tens of GB | S | Low | `mac_store.rs` |
| 11 | Image signing and verification | Sign manifests (cosign-style) on `push`, verify on `pull` with a policy | Gap 8; the image is a root-capable machine | M | Medium: key management; pick a standard format | `mac_store.rs`, `mac_cmd.rs`, `fluxvm-image` |
| 12 | Registry credential helpers | Read Docker config / credential helpers and OS keychain, not only two env vars | Env vars leak in CI logs and `ps` [I] | S | Low | `mac_cmd.rs` |
| 13 | Pull-through cache / peer mirror on the LAN | Second Mac pulls chunks from the first Mac's store before the registry | Fleet cold-start: N Macs x 20 GB over WAN | M | Medium: auth between Macs | `fluxvm-agent`, `fluxvm-image` [I] |
| 14 | Licence guard on public registries | Refuse `push` to known public registries unless `--i-have-checked-the-licence`; stamp the artifact with an annotation | The docs say "likely not allowed"; I found no source allowing it; make the default safe | S | Low | `mac_cmd.rs` |
| 15 | `FROM IPSW` and provisioning account | Wire `macos_install.rs`/`ipsw.rs` into `build`; create the user without Setup Assistant | Gap 6; today a base image is a manual step | L | High: Apple installer behaviour changes each macOS release | `fluxvm-apple/src/macos_install.rs`, `ipsw.rs`, `mac_build.rs` |
| 16 | Tart/Anka-compatible import and export | Extend `fluxvm-image/src/tart.rs` import; add `mac export --tart` | Lets users move off or onto Tart without rebuilding | M | Medium: Tart's format and licence are moving [?] | `crates/fluxvm-image/src/tart.rs` |

### P2: fleet and scheduler integration (Mactra-style placement)

| # | Feature | What | Why | Effort | Risk | Touches |
|---|---|---|---|---|---|---|
| 17 | `fluxctl mac run --fleet` | Ask the scheduler for a host, then run there; pass a template digest, not a path | Gap 2. Mactra shows the shape: scheduler picks the host, FluxVM makes the guest [S: README] | M | Medium: auth and NAT traversal | `fluxctl/src/fleet_client.rs`, `mac_cmd.rs`, `fluxvm-scheduler/src/apple_placement.rs` |
| 18 | Template presence in placement | Nodes heartbeat which `name:tag@digest` they hold; score hosts that already have the image | Avoids a 20+ GB pull per job; Mactra checks Xcode build per template on the worker [S] | S | Low | `fluxvm-agent` heartbeat, `apple_placement.rs` |
| 19 | macOS-slot-aware placement | Count macOS guests (max 2) as a separate capacity dimension and queue per host | `AppleHostCaps` already carries running macOS guests [F]; Mactra sidesteps with one job per host [S] | S | Low | `apple_placement.rs`, `apple_rebalance.rs` |
| 20 | Leases and fencing for instances | Instance heartbeat/lease (like Mactra's 90 s/20 s); expired lease = delete + reject stale writers | Avoids zombie guests when a controller dies | M | Medium: clock and partition cases | `fluxvm-scheduler` (journal, idempotency), `fluxvm-agent` |
| 21 | Idempotent submission key | `mac run --request-key K` returns the existing instance for the same payload | CI retries; Mactra does this for jobs [S: README] | S | Low | `fluxvm-scheduler/src/idempotency.rs` [F: file exists] |
| 22 | Controller-proxied ssh/vnc | Node dials out to the controller; `mac exec`/`ssh`/`vnc` go through it | Orchard does this so workers can be behind firewalls [S] | M | Medium: another network path to harden | `fluxvm-agent`, `fluxctl/src/fleet_tunnel.rs` [F: file exists] |
| 23 | Host drain and image rollout | `drain` a Mac, then roll a new template tag across hosts with health gates | Mactra has drain/resume [S]; FluxVM has rollout docs for other things [F: `sentinel-fleet-rollout.md`] | M | Low | `fluxvm-scheduler`, `fluxctl/src/fleet_cli.rs` |
| 24 | Fleet-wide macOS pool | Reuse the warm-pool idea per host, driven by queue depth | Warm start (phase 4) is only a proposal; boot of a clone is minutes [F] | L | High: unverified `macos-pool`; saved state is host-bound | `fluxvm-apple`, `fluxvm-scheduler` warm pool, `fleet_warmpool.rs` |
| 25 | Artifact return channel | `mac cp --out` plus size/SHA-256 verified download, with a cap | Mactra's xcresult flow (256 MiB, SHA-256 verified) [S] | S | Low | `mac_cmd.rs`, `fluxctl/src/kube_cp.rs` [I] |

### P3: CI runners and security

| # | Feature | What | Why | Effort | Risk | Touches |
|---|---|---|---|---|---|---|
| 26 | `fluxctl mac runner` for GitHub Actions | Per-job ephemeral VM with a JIT runner token; VM deleted after the job; at most 2 per Mac | The community pattern (Tart + a small controller) [S]; ARC does not cover macOS [S] | M | Medium: GitHub token handling; maintain across runner releases | new `crates/fluxctl/src/mac_runner.rs`; builds on `mac run --rm` |
| 27 | Same for GitLab/Buildkite/Jenkins | A generic "run this command in a fresh VM and exit with its status" wrapper | Not tied to one CI | S | Low | `mac_cmd.rs` (`run --rm` already does most) |
| 28 | Runner image set | Documented Macfile for a runner image (Xcode, simulators, runner binary, pinned versions) | Mactra leaves this to the user: "belong to the prepared template" [S] | S | Low | `docs/`, `examples/` |
| 29 | Egress policy for macOS guests | Reuse the existing egress policy and credential broker for `mac run` instances | Untrusted PR code runs in the VM | M | Medium: macOS guests lack private networks and tagged shares [F]; may need a host-side proxy | `fluxvm-apple/src/usernet_policy.rs` [F: file exists], `docs/egress-policy.md`, `docs/credential-broker.md` |
| 30 | No-secret defaults | Read-only workspace share, scrubbed environment, no host SSH agent forwarding by default | Fresh VM per job only helps if nothing leaks in | S | Low | `mac_cmd.rs` |
| 31 | Attested hosts | Use `fluxvm-apple/src/attest.rs` so a scheduler can require attested hosts for sensitive images | Public-repo PRs should not run on a developer's Mac | M | Medium: depends on what `attest.rs` proves [I: not read] | `fluxvm-apple/src/attest.rs`, `apple_placement.rs` |
| 32 | Audit and flight record per run | Emit start/stop/image digest/commit/user to the events log; webhook | Compliance and debugging; the repo has a flight recorder and event webhooks [F: docs exist] | S | Low | `fluxvm-scheduler/src/events.rs` |
| 33 | Linux guest runner on the same Macs | A Linux (non-capped) runner image on VZ for Linux jobs | Per a single source, Linux VMs are not subject to the two-VM cap [S, one source] | M | Medium: confirm the claim on hardware | `fluxvm-apple`, `mac_runner.rs` |
| 34 | Nested-virt Linux guest for kind/k3d | Offer M3+/macOS 15+ nested Linux guest for test clusters | Lima documents the same constraints [S]; GitHub arm64 runners cannot [S] | M | Medium: hardware-gated; macOS guest cannot [F] | `fluxvm-apple` (capability.rs, host_caps.rs), `apple_placement.rs` (`nested_virtualization` flag exists) [F] |

## 6. Suggested order

1. Items 1, 3, 5, 6 (verify and make it scriptable) before anything else.
2. Items 2, 4, 14 (CI-safe behaviour, small).
3. Items 18, 19, 17 (fleet placement using what the scheduler crate already scores).
4. Items 7, 10, 12 (registry cost), then 26 and 29 (runner and security).
5. Defer 8, 9, 15, 24: large, and each carries risk that only hardware testing will size.

## 7. Open questions

- Is Tart's licence and repository ownership settled? Check before promising import/export (item 16). [?]
- Does the Linux-guest exemption from the two-VM cap hold on current macOS? One source. Test before building item 33.
- GPU in macOS guests: unresolved here; Apple's framework docs were not read.
- Which macOS licence text applies to the versions you will ship, and does fleet or multi-tenant hosting fit it? Needs a legal read, not a web search.
