# Kairon on FluxVM's own Kubernetes

Kairon is Kubernetes-native VM orchestration: **Kubernetes declares, Kairon orchestrates, FluxVM executes.**
`fluxctl kube kairon` puts the Kairon control plane on the k3s cluster FluxVM created (`fluxctl kube up` or
`fluxctl kube apple create`), checks that it works, and tells you in plain English what to fix when it does not.

```
 the k3s cluster FluxVM made               FluxVM hosts (Kairon nodes)
 ┌───────────────────────────┐             ┌──────────────────────────────┐
 │ kairon-controller         │  Machines   │ Linux/KVM host: fluxvm daemon │
 │ CRDs (Machine, ...)       │ ──────────▶ │   + kairon-node (DaemonSet)   │
 │ kairon-ui (optional)      │             ├──────────────────────────────┤
 │ webhook (production)      │             │ Mac: fluxctl serve (vz)       │
 └───────────────────────────┘             │   + kairon-node (native)      │
   control plane: it stores state;         └──────────────────────────────┘
   it does not run the VMs                   these run the VMs, through the
                                             daemon at 127.0.0.1:7788
```

The k3s VM is the **control plane**. VMs run on **FluxVM hosts that are Kairon nodes**: a Linux/KVM machine running the FluxVM
daemon, or a Mac running FluxVM with the `vz` backend. A cluster with no such node is a complete, healthy Kairon control plane
with nowhere to place a VM; `status` and `selftest` say so.

## Install

```sh
fluxctl kube up                              # or: fluxctl kube apple create ...; writes ~/.kube/fluxvm-fluxvm-kube.yaml
fluxctl kube kairon install                  # the evaluation profile: nothing else to prepare
fluxctl kube kairon install --node kvm-1     # or label a FluxVM host as a Kairon node in the same step
fluxctl kube kairon status                   # re-run the health checks any time
```

Needs `helm` and `kubectl` on the machine you run it from. With no flags the target is the cluster `kube up` made; `--name`
picks another FluxVM-created cluster, `--kubeconfig`/`--context` a specific one. **It installs into whatever cluster the
kubeconfig points at**, and the first line it prints is that cluster's API server address. Do not point it at a cluster that
you did not create for Kairon. `--dry-run` prints every step and contacts nothing.

What `install` does, in order (nothing in the cluster changes before step 2):

1. `helm pull oci://ghcr.io/zyvorai/charts/kairon --version 0.10.6` into a private temporary directory.
2. **Refuses** unless helm reports the pinned manifest digest `sha256:eb952429...b255`, then installs from that very archive
   (no second download, so nothing can change between the check and the install).
3. `--production` only: makes the Secrets (below).
4. Labels each `--node`: `kairon.zyvor.dev/capable=true`, and with `--backend`, `--chip`, `--memory-gib` the labels the Mac
   node agent uses (`kairon.zyvor.dev/backend.vz=true`, `chip`, `memory-gib`).
5. `helm upgrade --install kairon <archive> -n kairon-system --create-namespace`.
6. Runs the health checks until they pass or `--wait` seconds (default 300) pass. Exit status 1 if any failed.

Other useful flags: `--ui` (the dashboard), `--image-cache-dir PATH` (where `kairon-node` caches images; the self-test needs
one, and the directory must exist on each node owned by UID 65532), `--chart-version V --chart-digest sha256:...` (another
release: both are needed, so a version never goes without its digest).

### Production profile

`--production` adds Kairon's production overlay (taken from the verified chart archive, not from the internet): admission webhook
with `failurePolicy: Fail`, mTLS live migration, network default-deny, a random dashboard password. It needs two Secrets, which
fluxctl makes and applies on `kubectl`'s stdin (private keys are never printed, never in an argument list, never in a file):

| Secret | Contents | Made like |
|---|---|---|
| `kairon-webhook-tls` | `tls.crt`, `tls.key`, `ca.crt`; SAN `kairon-controller-webhook[.kairon-system[.svc]]`, server auth only | `scripts/gen-webhook-tls-certs.sh` (ECDSA P-256, throwaway CA) |
| `kairon-migration-tls` | `ca.crt`, `tls.crt`, `tls.key`; SAN `kairon-node`, client and server auth | `scripts/gen-migration-mtls-certs.sh` (control-plane part) |

Differences from the scripts, on purpose: certificates are made in process (no `openssl`), the webhook certificate lasts a year
by default (`--cert-days`; the script's 30 days would silently start rejecting every Machine write), and its expiry is recorded on
the Secret so `status` fails once it has expired and names the fix. The per-node data-plane Secret is not made: it only matters with
`migration.adapterHostPath`, which this command does not set. These CAs are self-signed and for evaluation; use cert-manager or
your own CA for a real deployment (Kairon says the same). Existing Secrets are reused on `upgrade`; `--rotate-certs` replaces them.

## Status

`fluxctl kube kairon status` prints one line per check and a fix for each failure:

| Check | Passes when |
|---|---|
| CRDs | the core Kairon CRDs exist and are Established |
| controller | `kairon-controller` has all replicas ready |
| dashboard | `kairon-ui` is ready (skipped when `--ui` was not used) |
| node agent | the `kairon-node` DaemonSet is ready on every capable node |
| admission webhook | production only: its CA bundle is set, a controller pod answers behind its Service, the certificate has not expired |
| Kairon nodes | every node labelled `kairon.zyvor.dev/capable=true` is Ready |

The webhook check looks at the Service's endpoints; it does not send a test Machine through the webhook.

## Upgrade and uninstall

`fluxctl kube kairon upgrade` (same flags as `install`) pulls and verifies the pinned chart, **applies its CRDs first**
(`helm show crds | kubectl apply --server-side`: Helm only applies `crds/` on the first install), then runs the Helm upgrade.
Values come from the flags you pass, so repeat `--production`, `--ui` and friends.

`fluxctl kube kairon uninstall` removes the Helm release. The CRDs, every Machine, the Secrets and the node labels stay, because
deleting a CRD deletes all its objects; `--purge --yes` removes the CRDs and the Secrets fluxctl made.

## Self-test

```sh
fluxctl kube kairon selftest [--profile quick|standard|full]
```

`kaironctl selftest` boots a tiny CirrOS VM and checks power, networking, hotplug and more. It needs somewhere to boot it, so
fluxctl first looks at the registered nodes:

* a **Ready Kairon node that is not a control-plane node** is a FluxVM host: the test runs (it is Kairon that places the VM there);
* no Kairon node, only control-plane nodes, or none Ready: it prints what is missing and exits 2 (a failure, not a skip). Nothing is
  downloaded or run.

Registering a host: join the Linux/KVM machine that runs the FluxVM daemon to the cluster as a k3s agent (see
[apple-k3s.md](apple-k3s.md) for `join`), then `fluxctl kube kairon upgrade --node NAME` (or `install --node NAME` the first time); or, on a Mac, run `kairon-node` (below).

`kaironctl` is downloaded from the release into `~/.cache/fluxvm/kairon/0.10.6/` and **checked against the release's
`SHA256SUMS` before it is run**; a mismatch deletes the file and stops, and if the checksums cannot be fetched nothing is run.

## Macs

The control plane lives in the k3s VM (`kube up` or `kube apple create`). A Mac joins as a Kairon **node**, not as part of the
control plane:

1. On the Mac, run the daemon on the vz backend in the logged-in session: `fluxctl serve`, or the LaunchAgents from
   [deploy/launchd-notes.md](../deploy/launchd-notes.md). `fluxctl mac doctor` checks the prerequisites.
2. Run Kairon's own `kairon-node` natively on the Mac. On macOS it creates and heartbeats the Mac's Kubernetes **Node** itself (a Mac
   has no kubelet), with `kairon.zyvor.dev/capable=true`, `kairon.zyvor.dev/backend.vz=true`, the chip and memory labels, and the taint
   `kairon.zyvor.dev/vm-only:NoSchedule`. It needs the cluster's API address and a ServiceAccount token (`KAIRON_KUBE_URL`,
   `KAIRON_KUBE_TOKEN`, `KAIRON_KUBE_CA`, `NODE_NAME`), and the RBAC in Kairon's `deploy/macos/rbac.yaml`. See
   [Kairon on macOS](https://github.com/zyvorai/zyvor-kairon/blob/main/docs/macos.md) and
   [deploy/kairon-node-mac.md](../deploy/kairon-node-mac.md).
3. `fluxctl node-agent install` runs `fluxvm-agent node` as a LaunchAgent: it reports the Mac's capacity, labels and memory pressure
   to a *fleet central* (`fluxvm-agent central`). That is FluxVM's own fleet registry, **separate from Kubernetes**: it does not
   make the Mac a Kubernetes node. Use it when you also want FluxVM's fleet view; the Kubernetes Node comes from `kairon-node`.
4. Check: `fluxctl kube kairon status` shows the Mac under "Kairon nodes" as `(darwin, vz)`; `scripts/stack-check.sh` checks the Mac side.

What Kairon itself says it can and cannot do on a Mac (its `docs/macos.md`, not re-tested here):

* Verified by Kairon on one M4 against a k3s cluster running in a VM on the same Mac: the Mac registers as a Ready Node, a `vz` Machine is
  scheduled to it, FluxVM boots Debian 13, `status.guestIP` is set, SSH works, deleting the Machine deletes the VM.
* **Not verified by Kairon:** macOS guests, live migration, CSI, the VNC console relay, kairon-ui, multi-Mac clusters, Intel Macs.
* Linux-only features stay Linux-only: eBPF edge, migration, CPU pinning, VFIO. NAT networking only (no port forwards, no tap, no
  hotplug, no TPM or secure boot, no data disks or cdroms on the vz backend). Memory is accounted from the Node's allocatable figure.
* Because `kaironctl-linux-*` is the only bare binary on the release, the Mac self-test uses the `kairon_darwin_*` plugin archive, which Kairon's
  docs describe as the same command tree; that was not run here.

## What has been verified

Offline: the pinned chart digest was read from the registry (`crane digest`) and from `helm pull`'s own output and agree; the chart
renders (`helm template`, evaluation and production overlays) and all 35 rendered resources pass `kubeconform`; the Rust code is unit-tested
with golden tests (Helm arguments, label commands, Secret manifests with fake certificates, health parsing from captured
`kubectl` JSON, the install flows against a fake runner that records every call).

**Not done:** a live install on a FluxVM-created cluster, a real `kaironctl selftest`, and the `kaironctl` checksum check against a
real release: when this was written `https://github.com/zyvorai/zyvor-kairon/releases/download/v0.10.6/SHA256SUMS` answered 404 (the repo had
no GitHub releases, although the README and chart point at v0.10.6), so the download path fails closed and says so. The webhook
check was not exercised against a real API server.

## Checklist for the first live run

On a cluster FluxVM created for this (never an existing one):

```sh
fluxctl kube up --name kairon-test --api-port 16443 --cpus 4 --memory-mib 8192   # the control plane
fluxctl kube kairon install --name kairon-test --dry-run
fluxctl kube kairon install --name kairon-test
fluxctl kube kairon status  --name kairon-test                                   # no node yet: expect only "Kairon nodes" to fail
# register a FluxVM host (k3s agent joined to the cluster, or kairon-node on a Mac), then:
fluxctl kube kairon upgrade --name kairon-test --node HOST   # upgrade re-runs the labels and the same release
fluxctl kube kairon selftest --name kairon-test
fluxctl kube kairon install --name kairon-test --production  # on a second fresh cluster: production needs a clean install of its own
fluxctl kube kairon uninstall --name kairon-test --purge --yes
```
