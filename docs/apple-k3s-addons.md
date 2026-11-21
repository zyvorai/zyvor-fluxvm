# k3s add-ons: generated configuration for `fluxctl kube`

`fluxctl kube addons` generates the files a k3s node reads (`/etc/rancher/k3s/config.yaml`, `registries.yaml`, hardening files) and
a few recipes around them. It **does not create, join, upgrade or back up a cluster**; that lifecycle is `kube up`, `kube down` and
friends (see [kubernetes-on-mac.md](kubernetes-on-mac.md)). Every generator is a pure function whose output is checked against
golden text in unit tests. The only side effects are optional, behind `--ssh [user@]host`, and use your own `ssh`/`scp` setup.

Without `--ssh`, every subcommand only prints. With it, files are written as root (`sudo`) and a previous `config.yaml` is kept once
as `config.yaml.fluxvm-bak`. Nothing restarts k3s unless you pass `--restart k3s` (or `k3s-agent`).

> `kube up` passes some k3s flags on the command line (for example `--disable traefik`). Keys in `config.yaml` and command-line flags
> are both read by k3s, and the command line wins for scalars. Check `systemctl cat k3s` before assuming a key you generate is the one in effect.

## `kube addons config`

One command assembles one `config.yaml` from flags. Conflicting values for one key are an error, never a silent winner.

| Flag | Writes |
|---|---|
| `--disable traefik\|servicelb\|local-storage\|metrics-server\|coredns` (repeatable) | `disable:` list. `--enable X` only exists so `--enable X --disable X` is caught; k3s ships all four enabled |
| `--cilium` [`--cilium-kube-proxy-replacement`] | `flannel-backend: none`, `disable-network-policy: true`, optionally `disable-kube-proxy: true`; prints the `helm install cilium` line to run next |
| `--label k=v`, `--taint k=v:Effect` | `node-label`, `node-taint` (validated against Kubernetes syntax) |
| `--kube-reserved`, `--system-reserved`, `--eviction-hard` | `kubelet-arg` entries, e.g. `kube-reserved=cpu=100m,memory=256Mi` |
| `--tls-san X` | `tls-san:` list |
| `--embedded-registry` | `embedded-registry: true` (k3s's Spegel-based mirror; see Registries) |
| `--secrets-encryption` | `secrets-encryption: true` |
| `--cis` | the CIS profile below (implies secrets encryption) |

```
fluxctl kube addons config --disable traefik --disable servicelb --label disk=ssd --kube-reserved cpu=100m,memory=256Mi \
  --ssh debian@192.168.64.5 --restart k3s
```

Embedded-etcd HA, scheduled etcd snapshots, upgrades and tokens belong to the cluster-lifecycle work (`feat/apple-k3s`) and are deliberately not generated
here. Servers must agree on `disable`, `secrets-encryption`, `disable-network-policy` and `disable-kube-proxy`; this tool writes one node's file, so
apply the same flags to every server.

## Hardening

`--secrets-encryption` is the single key. `--cis` follows the k3s hardening guide for current releases: `protect-kernel-defaults`, API server
audit logging and a Pod Security Admission file, controller-manager and kubelet arguments. The files it points at come from
`fluxctl kube addons harden-files` (printed, or installed with `--ssh`): kernel sysctls in `/etc/sysctl.d/90-kubelet.conf`, `psa.yaml` (enforce
`restricted`, `kube-system` exempt) and an audit policy that logs metadata only, so Secrets never reach the audit log.
**Install the files and load the sysctls before restarting k3s with `--cis`**; with `protect-kernel-defaults` k3s refuses to start if the
sysctls are wrong. `restricted` rejects many ordinary images; edit `psa.yaml` to `baseline` if that is too strict.
This generates the profile; it has not been scanned with kube-bench.

### Secrets encryption and key rotation

`fluxctl kube addons secrets --ssh HOST` prints `k3s secrets-encrypt status` (enabled, rotation stage, whether server hashes match).
`--rotate --yes` runs `k3s secrets-encrypt rotate-keys` (k3s 1.30+) after checking that encryption is enabled and the hashes match, then prints the status
again. Without `--yes` nothing runs. Rotation re-encrypts every Secret and can corrupt a cluster if done wrongly: take a checkpoint first, and in an HA
cluster run it on one server only, then restart k3s on the others.

### Checking a node: `cis-check`

`fluxctl kube addons cis-check --ssh HOST` is read-only. It reads `/etc/rancher/k3s/config.yaml`, the four sysctls and the PSA and audit files, and
reports each as ok/warn/FAIL (exit 1 on FAIL). It sees only `config.yaml`: settings passed on the k3s command line or in a drop-in show as missing. It checks
that the profile is in place; it is not a CIS benchmark run (use kube-bench for that). It is separate from `kube doctor`, which is not changed.

## Bundled add-ons beyond on/off

* **Traefik:** `fluxctl kube addons traefik --set service.spec.type=NodePort --set ports.web.exposedPort=8080 [--gateway-api] [--ssh HOST]` renders a
  `HelmChartConfig` named `traefik` in `kube-system` (the supported way; `traefik.yaml` is never edited). With `--ssh` it is written to
  `/var/lib/rancher/k3s/server/manifests/traefik-config.yaml` and k3s applies it. Chart value names change between Traefik chart versions; check the chart your k3s ships.
* **ServiceLB pools:** `fluxctl kube addons lb-pool edge --node n1 --node n2` prints the `node-label:` lines for config.yaml and the `kubectl label node` commands
  (`svccontroller.k3s.cattle.io/enablelb=true`, `.../lbpool=edge`) plus the Service label. Once any node has `enablelb`, ServiceLB runs only on labelled nodes.
  To use MetalLB instead, `--disable servicelb`.

## Registries and airgap

`fluxctl kube addons registry --mirror docker.io=https://mirror:5000 --auth mirror:5000=bob=REG_PW --ca mirror:5000=/etc/ssl/ca.pem [--rewrite REG=REGEX=>REPL] [--insecure HOST] [--ssh HOST]`
renders `registries.yaml` (mirrors, rewrites, `configs` with auth and TLS). The password is read from the named environment variable, not
argv. k3s reads the file at start-up, so restart afterwards.

**Embedded registry mirror (Spegel):** `config --embedded-registry` on the servers plus `registry --embedded-mirror '*'` (or one registry) on every node
that should pull through it. Nodes need TCP 5001 and 6443 reachable from each other. Images imported by hand are shared only when tagged with a mirror-enabled registry.

`fluxctl kube addons airgap FILE...` plans the preload of `.tar`, `.tar.gz`, `.tgz`, `.tar.zst`, `.tar.lz4` archives or `.txt` image lists into
`/var/lib/rancher/k3s/agent/images/` (k3s imports that folder at start). `.tar` and `.tar.gz` are also imported at once with `k3s ctr images import`;
the other formats need a k3s restart. With `--ssh` it copies and imports, otherwise it prints the plan. For fetching the archives themselves see `kube fetch`.

## Kubeconfig merge and context names

`fluxctl kube addons kubeconfig --from k3s.yaml --vm dev [--server https://127.0.0.1:6443] [--use]` takes a kubeconfig with one cluster and
one user (k3s names all three `default`), renames cluster, user and context to `fluxvm-dev` (`--prefix`, or `--name` for an exact name), optionally
replaces the server address, and merges it into `~/.kube/config` (`--into` for another file). Same-named entries are replaced, everything else is kept,
`current-context` is only changed with `--use`, the old file is saved once as `config.fluxvm-bak`, and the file is made mode 0600. `--remove` takes it back out.

## Preflight

`fluxctl kube addons preflight --ssh HOST [--agent] [--etcd] [--vm-cpus N --vm-memory-mib M]` checks CPU, memory and disk against k3s minimums
(server 2 CPU / 2 GiB, agent 1 CPU / 512 MiB, with warnings below comfortable sizes), time sync, that 6443, 10250 (and 2379/2380 with `--etcd`) are free,
cgroup v2 and the `cpu`, `memory` and `pids` controllers, and the Apple Virtualization limits (a VM cannot have more vCPUs or memory than the Mac, and
leaving macOS under 4 GiB is flagged). Exit status 1 on any FAIL. It is a check of a machine *before* k3s; `kube doctor` examines a running cluster.

## A virtiofs-backed volume

`fluxctl kube addons pv NAME --path /Users/me/work [--namespace dev] [--size 5Gi] [--create]` prints a `hostPath` PersistentVolume and a claim bound to
it. Share the folder first with `kube up --share` (the guest path defaults to the Mac path). Single node only. Good for source trees and caches;
not for databases, because virtiofs sync and locking behaviour differs from a local disk.

## Rosetta for amd64 images

`fluxctl kube addons rosetta` prints the steps: create the VM with `kube up --rosetta`, verify the binfmt registration, pre-pull the amd64 variant
with `ctr images pull --platform linux/amd64` and use `imagePullPolicy: Never`, and what to expect (no AVX, slower; do not run databases that way).

## Status

Unit-tested (pure generators with golden output, parsers, validation, and the SSH steps against a fake guest). **Not run on real hardware:** no
generated file was applied to a live k3s node, no VM was booted, and the `ssh`/`scp` implementation, the Cilium, CIS, registry, embedded-mirror,
HelmChartConfig and ServiceLB-pool settings, secrets rotation, the Rosetta pull workaround and the virtiofs volume were not exercised. Key names follow the k3s documentation and should be confirmed against the k3s
version you install.
