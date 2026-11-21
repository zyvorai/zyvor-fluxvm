# k3s on Apple silicon: research and `fluxctl kube` backlog

Researched 2026-11-21. Base branch `feat/mac-docker`. This is a research note, not a spec.

**Labels.** `[S]` = sourced from a page named in the Sources section (fetched or returned by search on that date). `[I]` = inferred by the author from sources plus FluxVM's own docs, or from FluxVM code/docs read in this repo. `[U]` = looked for, not found or not verified. Third-party blog and forum claims are marked `[S-3p]` and are weaker than vendor docs. Nothing here was run against a live cluster.

## 1. What FluxVM already has

Read from `docs/kubernetes-on-mac.md`, `docs/cilium-cni.md`, `docs/ebpf-cilium.md` and `crates/fluxctl/src/kube_*.rs` (CLI in `main.rs`, first-boot/cloud-init text in `kube_host.rs`).

* k3s server in a Debian 13 VM, optional `--workers 0..8` as separate VMs on a private `apple.networks` subnet, `--node-ip`/`--flannel-iface` set for workers; flannel `host-gw` default, `--cni cilium` opt-in (Helm via k3s HelmChart; documented as not coming up on the Mac mini test machine); Traefik off by default (`--traefik`); `--k3s-version`, repeatable `--k3s-arg` (fixed at creation), `--rosetta`, `--share` (virtiofs, same path), `--offline DIR` + `kube fetch` (airgap binary and image tarball, sha256 checked) `[I: repo docs]`.
* Kubeconfig: read from `/etc/rancher/k3s/k3s.yaml` over SSH, `server:` rewritten to `127.0.0.1:<api-port>`, renamed, mode 600, merged into `~/.kube/config` with a one-time backup `[I: repo docs]`.
* Time travel (whole-VM checkpoint/restore/try/undo/bisect/golden/reset/diff/timeline/chaos/sleep/wake), `kube add` apps, `kube db-backup`, `kube load`, `kube dev/sync/forward/route/trust/share/host/pin/cp/logs/ps/doctor/trim/git/menubar/prompt`, MCP tools `[I: repo docs]`.
* Stated limits in the repo docs: single server only ("No high availability"), checkpoints refused for clusters with workers, workers cannot be added after creation unless `--workers` was used at creation, node token lands in worker cloud-init, no `kube fork` `[I: repo docs]`.
* Searched `crates/fluxctl/src` and `docs/kubernetes-on-mac.md` for `registries.yaml`, `secrets-encryption`, `cluster-init`, `etcd-snapshot`, `system-upgrade`, `embedded-registry`, `protect-kernel`: no matches `[I]`. So those are not exposed as first-class features today (they can only be passed by hand via `--k3s-arg` at creation).
* Apple VZ facts from FluxVM's own docs: virtiofs shares, bridged/vmnet-capable runner, usernet, private-network switches, no TPM, GPU not given to pods (`docs/macos.md`, `docs/kubernetes-on-mac.md` "kube host") `[I: repo docs]`.

## 2. Landscape on a Mac

| Tool | How it runs k8s on Apple silicon | Multi-node / HA | Notes and gaps | Source |
|---|---|---|---|---|
| Colima | `colima start --kubernetes` runs k3s inside its Lima VM | Single node `[S-3p]` | Kubernetes is opt-in, can be turned off to save RAM | [S-3p] dev.to / sumguy comparisons (Sec. 6) |
| Rancher Desktop | Built on Lima on macOS, ships k3s, k3s version selector | Single node `[S-3p]` | Most k8s knobs of the desktop tools `[S-3p]`; VZ backend use not verified `[U]` | [S-3p] dev.to comparison |
| OrbStack | Built-in single-node Kubernetes | Single node `[S-3p]` | Distribution not stated by vendor in the results; one source says k3s (`[S-3p]`, unverified); macOS 14+ | [S-3p] comparisons |
| Lima | Config-driven Linux VMs (`vz` or qemu) | Networking decides: `vzNAT` host-only access, `user-v2` VM-to-VM without host access, `socket_vmnet` (shared/bridged) for VM-to-VM plus host but needs an external binary and sudoers | A 2024 user reported a 3-node k8s try with vzNAT + user-v2 where Services were reachable only via NodePort on the vzNAT interface and not from the host (may be outdated) | [S] lima-vm.io network docs, [S-3p] Lima discussion #2388 |
| Multipass | Ubuntu VMs | not verified `[U]` | Driver/arm64 specifics not fetched (redirect not followed) `[U]` | none |
| Tart | Linux VMs via `tart create --linux`, prebuilt Ubuntu/Debian/Fedora images, macOS 13+ | not verified `[U]` | Shared dirs must be mounted manually in the Linux guest; custom Linux VMs need manual install, SSH not ready out of the box; softnet/bridged not covered on the page fetched | [S] tart.run quick-start |
| UTM | not researched `[U]` | | | none |
| k3d | k3s in Docker containers; Docker runs inside a Linux VM on a Mac `[I]` | Multi-node, built-in LoadBalancer per a comparison table `[S-3p]` | Containers share the Docker VM kernel; arm64 works since k3s supports arm64 `[S-3p, 2020]` | [S-3p] comparisons |
| kind | k8s-in-Docker | Multi-node; needs MetalLB for LoadBalancer `[S-3p]` | Same Docker-VM caveat `[I]` | [S-3p] |
| minikube | VM or Docker driver | Multi-node possible `[U]` | `minikube tunnel` needed for LoadBalancer; Docker driver faster, less isolated `[S-3p]` | [S-3p] |
| Talos | `talosctl cluster create` via docker provisioner (needs Docker Desktop/OrbStack/Colima socket) or qemu provisioner (open macOS issues reported); community Apple `container` provisioner exists, nodes ephemeral | Yes | On macOS the kubeconfig API address is not reachable from the host, the published localhost port must be used `[S-3p]` | [S-3p] siderolabs discussion #13587, guides |
| FluxVM `kube` | k3s in managed VZ VMs | 1 server + 0..8 agents; no HA | Whole-VM checkpoints, Rosetta, private-net workers; no etcd/HA/snapshots/upgrades/hardening exposed | repo docs `[I]` |

## 3. k3s facts that matter, with sources

**Embedded etcd HA** `[S: docs.k3s.io/datastore/ha-embedded]`. Odd number of servers, at least three; first server `--cluster-init` plus a shared token; others join with the same token and `--server https://<s1>:6443`; agents join any server; fixed registration address needs `--tls-san`. These flags must match on all servers: `--cluster-dns`, `--cluster-domain`, `--cluster-cidr`, `--service-cidr`, `--disable-helm-controller`, `--disable-kube-proxy`, `--disable-network-policy`, anything in `--disable`, `--secrets-encryption`. A single-node SQLite cluster can be converted by restarting with `--cluster-init`. Embedded etcd can be slow on slow disks.

**Tokens** `[S: docs.k3s.io/cli/server via search]`. `--token`/`K3S_TOKEN` is the shared secret; `--agent-token` lets agents join without server membership. Token file path `/var/lib/rancher/k3s/server/token` is only from a third-party guide `[S-3p]`. Snapshots are protected with a key derived from the token; snapshot file plus token exposes encrypted resources and CA private keys `[S: etcd-snapshot]`.

**Snapshots** `[S: docs.k3s.io/cli/etcd-snapshot]`. `k3s etcd-snapshot save|ls|delete|prune`; scheduled snapshots on by default (`0 */12 * * *`, retention 5, dir `${data-dir}/server/db/snapshots`), `--etcd-snapshot-compress`, S3 flags incl. `--etcd-s3-config-secret`; restore is `k3s server --cluster-reset --cluster-reset-restore-path=<PATH>` with the server stopped; other servers delete `/var/lib/rancher/k3s/server/db/` and rejoin; restoring on a new host needs the original token. Note: the docs describe snapshots as etcd-only; a single-server SQLite cluster (FluxVM default today) has no etcd snapshot `[I]`.

**Upgrades** `[S: docs.k3s.io/upgrades/automated]`. system-upgrade-controller, `Plan` CRD (`upgrade.cattle.io/v1`), `rancher/k3s-upgrade` image, server plan first, agent plan `prepare` waits on it, `concurrency`, `cordon`, `window`, channel or fixed `version`; does not enforce minor-version skipping; downgrades fail and leave nodes cordoned; upgrade jobs are privileged with host root mounted rw.

**Airgap and registries** `[S: docs.k3s.io/installation/private-registry, /registry-mirror]`. `/etc/rancher/k3s/registries.yaml` read at start, needed on every node that pulls, restart on change; `mirrors` (`endpoint`, `rewrite`) and `configs` (`auth`, `tls`). Embedded registry mirror: `--embedded-registry` on servers, enable per registry in `registries.yaml` (`"*"` wildcard), nodes need each other on TCP 5001 and 6443; manually imported images are pinned and shared only if tagged with a mirror-enabled registry; import with `ctr -n k8s.io`.

**Bundled add-ons** `[S: docs.k3s.io/networking/networking-services]`. CoreDNS `--disable=coredns`; Traefik customised by a `HelmChartConfig` named `traefik` in `kube-system`, not by editing `traefik.yaml`, Gateway API via `providers.kubernetesGateway.enabled`; ServiceLB creates a DaemonSet per LoadBalancer Service using hostPort, node pools via labels `svccontroller.k3s.cattle.io/enablelb` and `lbpool`, disable with `--disable=servicelb` for MetalLB. local-path and metrics-server: the storage page fetch came back empty `[U]`; `--disable=metrics-server` is used in FluxVM docs `[I]`.

**CNI** `[S: docs.k3s.io/networking/basic-network-options]`. flannel backends vxlan (default), host-gw (needs L2 between nodes), wireguard-native (needs WireGuard on all nodes); Cilium needs `--flannel-backend=none` and `--disable-network-policy`; delete `cilium_host`, `cilium_net`, `cilium_vxlan` and iptables rules before `k3s-killall.sh`/uninstall or host connectivity may be lost; dual-stack only at creation; `--flannel-iface` is per agent.

**Secrets encryption** `[S: docs.k3s.io/cli/secrets-encrypt]`. `secrets-encrypt status|enable|disable|rotate-keys` (recommended on v1.30+), legacy `prepare/rotate/reencrypt`; in HA pick one server, restart S1 then others, check hashes match; reencrypt about 5 secrets/s; wrong rotation can corrupt the cluster. Enabling on an existing cluster is listed for March 2026 releases (v1.33.10, v1.34.6, v1.35.3+k3s1).

**CIS hardening** `[S: docs.k3s.io/security/hardening-guide]`. `protect-kernel-defaults: true` plus `/etc/sysctl.d/90-kubelet.conf` (`vm.panic_on_oom=0`, `vm.overcommit_memory=1`, `kernel.panic=10`, `kernel.panic_on_oops=1`); `secrets-encryption: true`; audit log and policy under `/var/lib/rancher/k3s/server`; Pod Security Admission via `admission-control-config-file` on v1.25+; NetworkPolicies in the manifests dir (default blocks metrics-server and Traefik until allowed); some controls remain manual (1.1.20, 1.2.9, 1.2.11, 4.2.13, 5.x).

**Kubeconfig** `[S: docs.k3s.io/cluster-access]`. `/etc/rancher/k3s/k3s.yaml`, `system:admin` in `system:masters`; k3s refreshes the embedded certs on every start, so copies go stale and must be refreshed. FluxVM re-reads at `up` `[I]`; whether it refreshes on every later start is not verified `[U]`.

**Apple VZ limits** `[S-3p]`. No GPU/Metal compute to Linux guests (Apple container team statement, virtio-gpu 2D only); workarounds (krunkit/libkrun, third-party Metal passthrough) are outside the Virtualization framework and unverified. virtiofs performance and the vmnet/bridged entitlement requirements: not found in this pass `[U]`. FluxVM's `kube host` relay is the documented route for Mac-side GPU services `[I]`.

**Rosetta** `[I]`: FluxVM `--rosetta` registers Rosetta for amd64 programs but Kubernetes may refuse to pull an amd64-only image on an arm64 node (repo docs). No external source consulted `[U]`.

## 4. Comparison: FluxVM vs the field (k3s-relevant)

| Capability | Colima | Rancher Desktop | OrbStack | k3d/kind | Talos | FluxVM `kube` |
|---|---|---|---|---|---|---|
| Real VM per node | no (single VM) `[S-3p]` | no `[S-3p]` | no `[S-3p]` | no, containers `[I]` | qemu path / containers | yes |
| Multi-node | no | no | no | yes | yes | yes (agents only) |
| HA control plane | no | no | no | partial `[U]` | yes | no |
| Whole-cluster snapshot incl. memory | no `[I]` | no `[I]` | no `[I]` | no `[I]` | no `[I]` | yes (single node) |
| etcd snapshot / restore UX | no `[I]` | no `[I]` | no `[I]` | no `[I]` | talosctl `[U]` | none |
| In-place k3s upgrade | version selector `[S-3p]` | version selector `[S-3p]` | `[U]` | recreate `[I]` | talosctl `[U]` | recreate only |
| Airgap | `[U]` | `[U]` | `[U]` | `[U]` | `[U]` | yes (`--offline`) |
| Private registries / mirrors | `[U]` | `[U]` | `[U]` | `[U]` | `[U]` | no |
| Hardening profile | no `[I]` | no `[I]` | no `[I]` | no `[I]` | secure by design `[U]` | no |

`[I]` in a competitor cell means "not seen in the sources read", not a proof of absence.

## 5. Top gaps

1. No HA: a three-server embedded-etcd cluster is not creatable, and workers must be chosen at creation.
2. No etcd snapshot, schedule, S3 or restore flow; the best backup is a VM checkpoint, which is refused for multi-node clusters.
3. No upgrade path: changing k3s version means `down --purge`.
4. No registry mirror or private-registry config, no embedded registry mirror for multi-node image sharing (today `kube load` copies to every node).
5. No hardening or secrets-encryption profile; token handling is documented as a risk (cloud-init stores it).
6. Cilium on Apple silicon unresolved (documented failure); no lighter encrypted/eBPF alternative offered such as wireguard-native flannel.
7. Add-on control is limited to `--traefik` and raw `--k3s-arg`; no ServiceLB/CoreDNS/metrics-server/local-path toggles or HelmChartConfig helper.
8. Kubeconfig staleness after cert refresh and no multi-server endpoint handling `[I]`.
9. Cannot add workers to a cluster created without `--workers`.
10. Networking limits for multi-node are handled via private switch, but bridged/vmnet-shared modes (L2 reach from other LAN machines, external agents joining) are not offered for k3s `[I]`.

## 6. Backlog: `fluxctl kube` k3s features

Effort S (<= 2 days), M (about a week), L (multi-week). Files: CLI is in `crates/fluxctl/src/main.rs` (kube subcommands), first-boot text in `crates/fluxctl/src/kube_host.rs`, docs in `docs/kubernetes-on-mac.md`; new modules follow the existing `kube_*.rs` pattern. Rationale cites the section above. All items are proposals `[I]`; the k3s behaviour they rely on is sourced as noted.

| # | Feature | Why | Eff | Risk | Files |
|---|---|---|---|---|---|
| 1 | `kube etcd-snapshot save/ls/delete/prune/restore` wrapping `k3s etcd-snapshot` over SSH (needs etcd, see 9) `[S]` | Gap 2; datastore-level backup, works for multi-node where checkpoints are refused | M | Restore stops k3s and needs the token; test on throwaway cluster first | new `kube_etcd.rs`, `main.rs`, `kube_host.rs` |
| 2 | `kube up --etcd` : start single server with `--cluster-init` (embedded etcd instead of SQLite) so snapshots and later HA work `[S]` | Prerequisite for 1, 3, 4; flag fixed at creation | S | Slightly more RAM/disk IO `[S: slow disk caveat]` | `main.rs`, `kube_host.rs` |
| 3 | `kube up --servers 3` : HA control plane, servers join with `--server` and shared token `[S]` | Gap 1 | L | Checkpoints refused for multi-VM (existing limit); 3x resources; flags must match on all servers `[S]` | `main.rs`, `kube_host.rs`, `kube_checkpoint.rs` guard |
| 4 | Stable registration address for HA (`--tls-san`, VIP or Mac-side forward), kubeconfig pointing at it `[S]` | HA needs one API endpoint; today it is `127.0.0.1:<port>` to one VM | M | Choice of VIP technique on VZ not researched `[U]` | `main.rs`, `kube_forward.rs`, `kube_host.rs` |
| 5 | Scheduled snapshots config: `--snapshot-cron`, `--snapshot-retention`, `--snapshot-compress`, surfaced in `kube status` `[S]` | k3s already snapshots every 12 h by default on etcd `[S]`; surface and tune | S | Disk growth in 30 GiB VM | `main.rs`, `kube_host.rs`, `kube_doctor.rs` |
| 6 | Snapshot export to the Mac (`kube etcd-snapshot pull`) and optional S3 target via `--etcd-s3*` `[S]` | Off-VM backup; snapshots contain CA keys so store securely `[S]` | M | Secret handling (token + snapshot = key exposure `[S]`); file perms 600 | `kube_etcd.rs`, `kube_backup.rs` |
| 7 | `kube upgrade --to vX` using system-upgrade-controller (apply controller manifest, server then agent Plans) `[S]` | Gap 3 | M | Privileged upgrade jobs `[S]`; no downgrade `[S]`; needs internet or airgap images; take a checkpoint first (single node) | new `kube_upgrade.rs`, `main.rs` |
| 8 | `kube upgrade --check` : compare running version to the stable channel and warn on skipped minors `[S: controller does not enforce skew]` | Cheap safety net | S | Network call to the channel URL | `kube_upgrade.rs`, `kube_doctor.rs` |
| 9 | Pre-upgrade guard: auto `kube checkpoint` (single node) or etcd snapshot (HA) before 7 `[I]` | Rollback story unique to FluxVM | S | Checkpoint refused with workers | `kube_upgrade.rs`, `kube_checkpoint.rs` |
| 10 | `kube registry add HOST --mirror URL [--user/--pass-stdin] [--ca]` writing `/etc/rancher/k3s/registries.yaml` on all nodes and restarting k3s `[S]` | Gap 4; corporate registries, pull-through caches | M | Restart disrupts workloads; credentials on disk; must reach every node `[S: needed on every pulling node]` | new `kube_registry.rs`, `main.rs`, `kube_host.rs` |
| 11 | Same as 10 at creation time: `kube up --registries-file FILE` (cloud-init) `[S]` | Fresh clusters pull through mirror from first boot | S | Secrets in cloud-init (same exposure as token today) | `main.rs`, `kube_host.rs` |
| 12 | `kube up --embedded-registry` (Spegel-based mirror) plus `registries.yaml` wildcard `[S]` | Multi-node image sharing; reduces pulls; `kube load` could import once | M | Needs TCP 5001 between nodes `[S]`; private subnet already allows `[I]`; marked behaviours like `latest` unsupported `[S]` | `main.rs`, `kube_host.rs`, `kube_apps.rs` |
| 13 | `kube load` : import once, rely on embedded mirror to spread, tag with mirror-enabled registry `[S]` | Faster than copying to each node | S | Depends on 12; image must be tagged right `[S]` | `kube_host.rs` (load path), `main.rs` |
| 14 | `kube fetch --with-images LIST` : add workload images to the airgap folder and have first boot import them `[S: ctr -n k8s.io import]` | Complete offline clusters (gap noted in repo docs) | M | Folder size | `main.rs`, `kube_host.rs` |
| 15 | `kube up --secrets-encryption` and `kube secrets rotate-keys/status` wrapping `k3s secrets-encrypt` `[S]` | Gap 5; also CIS item | S/M | Rotation can corrupt if done wrong `[S]`; enabling on existing cluster only on recent releases `[S]` | new `kube_secrets.rs`, `main.rs`, `kube_host.rs` |
| 16 | `kube up --harden cis` : `protect-kernel-defaults`, sysctl file, secrets-encryption, audit policy/log dir, PSA config, base NetworkPolicies `[S]` | Gap 5 | L | Breaks workloads (PSA restricted, default-deny blocks metrics-server/Traefik `[S]`); some controls stay manual `[S]` | new `kube_harden.rs`, `main.rs`, `kube_host.rs`, `kube_doctor.rs` |
| 17 | `kube doctor --cis` : read-only check of the hardening settings above on a running cluster `[S: settings list]` | Value without the risk of 16 | S | Version-dependent settings `[S]` | `kube_doctor.rs` |
| 18 | Token hygiene: use `--agent-token` for workers, write token with 0600 outside cloud-init plain text where possible, `kube token rotate` `[S: --agent-token]` | Repo docs flag token in cloud-init as a risk; agents should not hold server-join power `[S]` | M | Cloud-init is the only first-boot channel today `[I]`; rotation procedure not researched `[U]` | `main.rs`, `kube_host.rs`, docs |
| 19 | `kube add-node` : add workers to a cluster created without `--workers` by giving every cluster a private-network card at creation (flag default on) `[I]` | Gap 9 | M | Changes network layout of all new clusters; existing VMs unaffected | `main.rs`, `kube_host.rs` |
| 20 | `kube remove-node NAME` : cordon, drain, `kubectl delete node`, delete VM `[I]` (repo docs describe the manual steps) | Currently manual | S | Data loss on node-local volumes (local-path is node-local `[I]`) | `main.rs`, new helper |
| 21 | `kube up --disable coredns,servicelb,metrics-server,local-storage,traefik` shorthand plus validation that servers agree `[S: --disable must match]` | Gap 7; clearer than raw `--k3s-arg` | S | Disabling CoreDNS needs another DNS `[S]` | `main.rs`, `kube_host.rs` |
| 22 | `kube ingress traefik set KEY=VAL` writing a `HelmChartConfig` named `traefik` in the manifests dir; `--gateway-api` toggle `[S]` | Supported way to customise Traefik (do not edit `traefik.yaml`) `[S]` | S | Chart value keys changed in chart v40 (`service.spec.type`) `[S]` | new `kube_ingress.rs`, `main.rs` |
| 23 | `kube lb pool` : label nodes for ServiceLB (`svccontroller.k3s.cattle.io/enablelb`, `lbpool`) `[S]` and option `--metallb` that disables ServiceLB `[S]` | LoadBalancer control on multi-node | S/M | MetalLB address pool design on the private subnet `[I]` | `main.rs`, `kube_route.rs` |
| 24 | `kube up --flannel-backend wireguard-native|vxlan|host-gw` first-class, with a guard that wireguard module exists in the guest image `[S]` | Encrypted node-to-node traffic without Cilium; `host-gw` already default | S | Kernel module availability in Debian 13 VM not checked `[U]` | `main.rs`, `kube_host.rs` |
| 25 | Cilium recovery: `kube doctor` detects Cilium stall, captures agent logs, offers `kube cni-reset` that removes `cilium_*` interfaces and rules before uninstall `[S: manual cleanup warning]` | Repo docs say Cilium fails on the Mac mini, cause unknown | M | Needs root-cause work; may stay unsolved | `kube_doctor.rs`, `kube_host.rs`, `docs/cilium-cni.md` |
| 26 | Cilium kube-proxy replacement and Hubble as options once 25 is understood `[I]` | Ties to `docs/ebpf-cilium.md` | L | Depends on 25; kernel features in guest not verified `[U]` | `kube_host.rs`, `main.rs` |
| 27 | Kubeconfig refresh: `kube env --refresh` and automatic re-read on `up`/`wake`, since k3s refreshes embedded certs on each start `[S]` | Prevent expired copies | S | Merge logic into `~/.kube/config` must stay non-destructive | `main.rs`, `kube_names.rs` |
| 28 | HA-aware kubeconfig: multiple servers or the stable address from item 4 `[I]` | Needed with 3 | S | Depends on 4 | `main.rs` |
| 29 | `kube up --net bridged|vmnet-shared` for k3s nodes: external agents (another Mac or Linux box) can join `[I]` | Gap 10; real multi-host k3s | L | Entitlement and vmnet behaviour not verified `[U]`; security exposure of API on LAN | `main.rs`, `kube_host.rs`, `docs/macos.md` |
| 30 | `kube dual-stack` creation flag (`--cluster-cidr`/`--service-cidr` pairs) `[S]` | Only possible at creation `[S]` | S | Rarely needed on a Mac | `main.rs`, `kube_host.rs` |
| 31 | Local-path volumes on virtiofs: `kube volume` helper mapping a `--share` folder to a PV with correct `hostPath`/nodeAffinity `[I]` | Repo docs already share folders for hostPath; PV UX is manual. virtiofs performance not researched `[U]` | M | Permission/ownership mapping; performance unknown | new `kube_volume.rs`, `main.rs` |
| 32 | `kube doctor` additions: etcd health, snapshot age, registry reachability, node-to-node 5001/6443 reachability `[S: ports]` | Observability for new features | S | Low | `kube_doctor.rs` |
| 33 | amd64 image helper for Rosetta: detect single-arch amd64 image refs and suggest `kube load` or multi-arch build `[I: repo docs say pull may be refused]` | Prevents a known confusing failure | S | Needs registry manifest inspection | `kube_pin.rs`, `main.rs`, docs |
| 34 | MCP tools for snapshot/upgrade (read-only `status` always, writes behind `--allow-write`) `[I]` | Matches existing MCP model | S | Write tools need guard | `kube_mcp.rs` |
| 35 | Studio/menubar surface for snapshot age and upgrade availability `[I]` | UX | M | Low | `kube_menubar.rs`, `kube_ps.rs` |

### Suggested order

Quick wins: 2, 5, 17, 21, 22, 27, 32, 33. Then 1, 7, 10, 12 (backup, upgrade, registries). Then 15, 16, 18. HA (3, 4, 28) and bridged networking (29) last, because they interact with the checkpoint limitation and unverified vmnet behaviour.

## 7. Open questions (not found in this pass)

* Multipass driver on Apple silicon, UTM Kubernetes workflows, minikube multi-node specifics on Mac.
* virtiofs performance for PV-style workloads, and the exact vmnet/bridged entitlement requirements.
* local-path-provisioner and metrics-server details (page fetch was empty).
* Why Cilium stalls under VZ on the Mac mini (repo docs: cause unknown).
* Whether wireguard-native flannel works in the Debian 13 guest kernel.
* OrbStack's and Rancher Desktop's primary-source feature statements (only blog comparisons were read).

## Sources (all accessed 2026-11-21)

Vendor/primary:
* https://docs.k3s.io/datastore/ha-embedded
* https://docs.k3s.io/cli/etcd-snapshot
* https://docs.k3s.io/cli/secrets-encrypt
* https://docs.k3s.io/cli/server (via search result only)
* https://docs.k3s.io/upgrades/automated
* https://docs.k3s.io/installation/private-registry
* https://docs.k3s.io/installation/registry-mirror
* https://docs.k3s.io/networking/networking-services
* https://docs.k3s.io/networking/basic-network-options
* https://docs.k3s.io/security/hardening-guide
* https://docs.k3s.io/cluster-access
* https://tart.run/quick-start/
* https://lima-vm.io/docs/config/network/ and /network/vmnet/ (via search result summaries)

Third-party (weaker, `[S-3p]`):
* https://github.com/lima-vm/lima/discussions/2388 (2024 networking options)
* https://github.com/siderolabs/talos/discussions/13587 and https://github.com/BinHsu/aegis-apple-container-provisioner-talos (Talos on Apple container, 2026)
* https://github.com/apple/container/discussions/62 (GPU passthrough)
* https://dev.to/wango/an-architecture-to-run-ai-agents-safely-and-efficiently-in-a-linux-vm-on-apple-silicon-1hni, https://www.veloworkspaces.com/blog/how-virtualization-framework-works/, https://scaleninja.com/macvisor/virtualization-framework/ (GPU/virtio-gpu limits)
* https://dev.to/pickuma/docker-desktop-alternatives-orbstack-vs-colima-vs-rancher-desktop-2af1, https://sumguy.com/colima-vs-orbstack-vs-docker-desktop/ (Colima/OrbStack/Rancher Desktop)
* https://sivakumarvunnam.github.io/blogs/minikube-vs-kind-vs-k3s, https://pub.towardsai.net/minikube-vs-kind-vs-k3d-which-local-kubernetes-cluster-should-you-choose-234b08d8a9ab (k3d/kind/minikube)
* https://oneuptime.com/blog/post/2026-02-09-k3s-embedded-etcd-edge-ha/view (token file path)
