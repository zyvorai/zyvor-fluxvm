# The platform layer of `kube apple`

Registries, hardening, add-on profiles, GitOps, load balancing, certificates, network policy, channel upgrades and multi-server
restore, for clusters made by [`fluxctl kube apple`](apple-k3s.md). It reuses the generators of
[`kube addons`](apple-k3s-addons.md) and delivers their output over the same pinned ssh connections as the rest of `kube apple`.

**Status.** Unit-tested: the generators (golden text), the order and content of every remote step against a recording fake, input
validation and secret handling. **Not run against a real cluster:** no VM was booted, no manifest below was applied, no
etcd snapshot was restored, no GitOps controller synced, no k3s was restarted by these commands. Treat the first run as an
experiment on a cluster you can recreate.

## At `create`: `--registries-file`, `--embedded-registry`, `--harden cis`, `--addons`, `--cilium`

```
fluxctl kube apple create dev --servers 3 --binary k3s-arm64 --sha256-file sha256sum-arm64.txt \
  --addons metallb --harden cis --psa-level baseline \
  --registries-file registries.yaml --embedded-registry
```

| Flag | Effect |
|---|---|
| `--registries-file FILE` | `FILE` is copied to `/etc/rancher/k3s/registries.yaml` (mode 0600) on **every** node. It is checked as YAML with only the keys k3s reads (`mirrors`, `configs`). |
| `--embedded-registry` | `embedded-registry: true` on the servers. Without `--registries-file`, a `mirrors: {"*": {}}` file is written for you; with one, it must have a `mirrors:` entry. Nodes need TCP 5001 and 6443 open to each other. |
| `--harden cis` [`--psa-level baseline\|restricted`] | The k3s CIS settings, `/etc/sysctl.d/90-kubelet.conf`, a Pod Security Admission file and a metadata-only audit policy on the servers. The sysctls are loaded before k3s restarts. Default level `restricted` (`kube-system` exempt); many images fail under it, so say `baseline` if that is too strict. This is the profile, not a benchmark result. |
| `--addons default\|minimal\|metallb\|envoy-gateway` | Bundled components to turn off, added to `--disable` and so carried in every server's unit and the journal: `minimal` = ServiceLB, local-storage and metrics-server off; `metallb` = ServiceLB off; `envoy-gateway` = nothing off, and Envoy Gateway (pinned) is installed once the cluster is up (see [docs/gateway.md](gateway.md)). Traefik is always off. |
| `--cilium` [`--cilium-kube-proxy-replacement`] | `flannel-backend: none`, `disable-network-policy: true` (and `disable-kube-proxy`). **Cilium is not installed**: the nodes have no pod network until you run the printed `helm install cilium` command (chart 1.20.5 in the text, change it if you want another). |

How it works: `create` builds the cluster as before (the `--addons` components go into the unit), then writes the files and a k3s
drop-in `/etc/rancher/k3s/config.yaml.d/40-fluxvm-platform.yaml` (your `config.yaml` is untouched) to the nodes and restarts them
**one at a time, servers first**, waiting for each. So a cluster made with these flags starts once without them and is restarted
once with them; with `--cilium` the restart is when its pod network goes away. The output of `create` gains a `platform` key
(files written, nodes restarted, notes). If that step fails the cluster exists; the error says so and names the retry:

```
fluxctl kube apple platform apply dev --harden cis --restart     # same flags; idempotent
fluxctl kube apple platform apply dev --harden cis --dry-run     # what would be written
```

Agents that join later do not get `registries.yaml` by themselves; run `platform apply --registries-file ... --restart` again after joining them.
`apply` does not take `--addons` (servers must agree on the components, and the units carry them).

## `kube apple doctor NAME --cis`, `kube doctor --cis`

Adds the read-only profile audit of `kube addons cis-check` to the report: per server, the secrets-encryption and kernel-defaults
settings, Pod Security Admission, audit log and policy, two controller/kubelet settings, the four sysctls and the two files, each
ok/warn/FAIL with the fix. It reads `40-fluxvm-platform.yaml` if present, otherwise `config.yaml`, so settings given on the k3s command
line or in other drop-ins show as missing. It checks that the profile is in place; it is not kube-bench. `kube doctor --cis`
does the same for the server VM of `kube up`.

## Multi-server snapshot restore

```
fluxctl kube apple snapshot restore dev on-demand-dev-s1-1700000000 --rejoin-servers [--from-node dev-s1]        # prints the plan
fluxctl kube apple snapshot restore dev on-demand-dev-s1-1700000000 --rejoin-servers --yes
```

Follows the k3s procedure: every server answers and the snapshot exists on `--from-node` (default the first server) before anything
is stopped; then k3s is stopped on **all** servers, `--cluster-reset-restore-path` runs on the chosen one, it starts and is waited for, and the
others are done one at a time: stop, move `/var/lib/rancher/k3s/server/db` aside to `db.pre-restore-STAMP` (delete it yourself later), start,
wait until it has rejoined. The API is down from the first stop until the first server is up. Everything after the snapshot is lost. Agents
are not touched. A server that is down blocks the restore (nothing is changed); start it, or `remove` it first. If the reset itself fails,
the other servers are stopped but still hold their data; `fluxctl kube apple start NAME` brings the cluster back without the restore.

## `upgrade --channel`: through the system-upgrade-controller

```
fluxctl kube apple upgrade dev --channel stable [--drain] [--no-backup] [--allow-downgrade]
```

Installs system-upgrade-controller v0.21.1 from the pinned manifests (CRDs, then controller), then two `Plan`s that follow
`https://update.k3s.io/v1-release/channels/CHANNEL` (`stable`, `latest`, `testing`, or a minor such as `v1.31`): servers one at a time, then
agents. Before installing anything it checks that every node is Ready, resolves the channel's current release from a server, and refuses a
downgrade or a jump over a minor release (the same rule as `upgrade --binary`); an etcd cluster gets a `pre-suc-STAMP` snapshot first. A SQLite cluster
cannot be copied while running: take `kube apple backup` yourself and add `--no-backup`.

Not the same as `--binary`: the cluster pulls the binary from the `rancher/k3s-upgrade` image, so the SHA-256 check on your Mac does not apply, and
the journal's recorded k3s version and checksum are not updated (`status` reads the real versions). The upgrade runs inside the cluster after the
command returns; watch `kubectl -n system-upgrade get plans,jobs`. It needs the guests to reach github.com, update.k3s.io and Docker Hub.

## Pinned manifests

Every install below uses a manifest identified by version, URL and SHA-256 (`fluxctl kube apple platform pins` lists them). The server
downloads it over HTTPS, checks the digest and only then runs `kubectl apply` (server-side where the manifest is large); a mismatch stops
before anything is applied. `--manifest-file FILE` sends a local copy instead (air-gapped use), checked on the Mac and again on the server against the pin or
`--sha256`. The digests were computed from the upstream release assets on 2026-11-22 (versions from each project's latest-release API at
that time); they have not been compared with a second source, and nothing was applied. To move a pin, edit `crates/fluxctl/src/kube_apple/pins.rs` (version, URL, digest together).

| Manifest | Version | Used by |
|---|---|---|
| system-upgrade-controller `crd.yaml`, `system-upgrade-controller.yaml` | v0.21.1 | `upgrade --channel` |
| Flux `install.yaml` | v2.10.1 | `platform gitops flux` |
| Argo CD `manifests/install.yaml` | v3.6.2 | `platform gitops argo` |
| MetalLB `metallb-native.yaml` | v0.16.2 | `platform metallb` |
| cert-manager `cert-manager.yaml` | v1.21.3 | `platform cert-manager` |
| metrics-server `components.yaml` | v0.9.1 | `platform metrics-server` |
| Envoy Gateway `install.yaml` (about 4 MiB; Gateway API CRDs, Envoy Gateway CRDs, controller) | v1.10.0 | `platform envoy-gateway`, `create --addons envoy-gateway` |

## GitOps: `platform gitops flux|argo`

```
fluxctl kube apple platform gitops dev flux --repo git@github.com:acme/fleet.git --path clusters/dev \
  --deploy-key-file ~/.ssh/fleet-deploy --known-hosts-file ./github-known-hosts
fluxctl kube apple platform gitops dev argo --repo git@github.com:acme/fleet.git --path clusters/dev \
  --deploy-key-file ~/.ssh/fleet-deploy --auto-sync
```

Installs the controller, waits for its CRDs and controllers, creates the repository credentials and points it at the repository (Flux: a `GitRepository`
and a `Kustomization`, interval 1m/10m, prune on; Argo CD: an `Application`, manual sync unless `--auto-sync`). This is not `flux bootstrap`: nothing is created or pushed in the repository.
The deploy key is read from a file that must be mode 600 or stricter and have no passphrase; it goes into a Secret that travels on ssh stdin to `kubectl apply -f -`, and
is never in an argument, a log or the report. Flux needs the Git server's host keys and takes them from `--known-hosts-file` (lines from `ssh-keyscan` that **you** verified;
nothing is fetched on first use). Argo CD's built-in list covers the common providers; with `--known-hosts-file` its list is replaced. Add the key's public half to the repository (read-only is enough).
An https repository needs neither key nor host keys; credentials in the URL are refused.

## Recipes

* `platform metallb NAME --address 10.89.0.200-10.89.0.220 [--pool default] [--no-auto-assign]`: installs MetalLB, waits for the controller (its admission webhook checks pools),
  then applies an `IPAddressPool` and an `L2Advertisement`. Use `--print-pool` to see the YAML without a cluster. Create the cluster with `--addons metallb`, or ServiceLB and MetalLB both answer. The addresses must be free and on the
  nodes' network (the private 10.89.x.y network of `kube apple`), which the Mac does not route to by itself.
* `platform cert-manager NAME`: installs cert-manager and waits for its three deployments. Create an issuer yourself.
* `platform envoy-gateway NAME`: installs Envoy Gateway from the pinned `install.yaml` (server-side apply) and waits for `deployment/envoy-gateway` to be Available. It creates no GatewayClass or Gateway. `create --addons envoy-gateway` does the same after the cluster is up. See [docs/gateway.md](gateway.md).
* `platform metrics-server NAME [--kubelet-insecure-tls]`: only for clusters created with metrics-server disabled (`--addons minimal`); refuses otherwise. k3s signs kubelet certificates with its own CA, which
  the stock metrics-server does not trust, so `kubectl top` may fail until you add `--kubelet-insecure-tls`, which turns that verification off.

## `platform netpol`

```
fluxctl kube apple platform netpol dev -n app [--allow-from ingress-nginx] [--allow-egress-cidr 10.0.0.0/8] [--no-dns] [--no-same-namespace] [--apply]
```

Prints (or with `--apply` sends) `baseline-default-deny` (all ingress and egress), `baseline-allow-dns` (to kube-dns in kube-system, port 53), `baseline-allow-same-namespace`, one `baseline-allow-from-NS` per
`--allow-from` and one `baseline-allow-egress-cidrs`. `kube-system`, `kube-public` and `kube-node-lease` are refused. A NetworkPolicy does nothing unless the cluster enforces them: k3s does by default, with `--cilium` Cilium does, and
`--disable-network-policy` without Cilium means nothing does. A deny-all also blocks traffic from the ingress controller and metrics scrapers until you allow them.

## Leftovers and things to verify

Everything above on a real cluster; the guest needing `curl` and outbound HTTPS; Cilium's required restart sequence; SUC's `k3s-upgrade` image against the unit this tool installs (`/usr/local/bin/k3s`); whether k3s merges a `disable:` in a unit and drop-in as expected (this layer
avoids it: add-on components go only in the unit); the CIS profile under a real scan; a restore with three servers.
