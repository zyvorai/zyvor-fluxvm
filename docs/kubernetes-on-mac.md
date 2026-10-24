# Kubernetes on a Mac (`fluxctl kube`)

`fluxctl kube` runs **a single-node [k3s](https://k3s.io) cluster in one managed FluxVM VM, with its API server and a kubeconfig on
your Mac.** It is the FluxVM counterpart of `colima start --kubernetes`, built the same way as [`fluxctl docker`](docker-engine.md):
a Debian 13 VM, folders shared at matching paths, and a supervised SSH forward.

It is not the same thing as `fluxvm-kube` (see [architecture.md](architecture.md)), which is an operator that runs *inside* a
cluster you already have.

## Use it

```bash
fluxctl kube up                                  # create (first time) or start the VM; prints export KUBECONFIG=…
eval "$(fluxctl kube env)"                       # export KUBECONFIG=~/.kube/fluxvm-fluxvm-kube.yaml
kubectl get nodes
kubectl create deployment web --image=nginx:1.29-alpine && kubectl expose deployment web --type=NodePort --port=80
fluxctl kube status                              # VM, address, whether the API answers, kubeconfig path (JSON)
fluxctl kube down                                # stop the forward and the VM (cluster state kept)
fluxctl kube down --purge                        # delete the VM, the cluster's state and the kubeconfig
```

`up` is idempotent: run it again to restart the VM or the forward. The first run downloads k3s and takes a few minutes.

| Flag | Meaning |
|---|---|
| `--name` | VM, node and kubeconfig context name (default `fluxvm-kube`). Use another name for a second cluster. |
| `--cpus`, `--memory-mib`, `--disk-gib` | VM size (default 2 vCPUs, 4096 MiB, 30 GiB). |
| `--api-port` | The Mac port for the API server, on 127.0.0.1 only (default 6443). |
| `--publish HOST[:GUEST]` | Forward a Mac loopback port to the guest, for NodePort services (30000-32767). Repeatable. |
| `--share HOST[:GUEST][:ro]` | Share a Mac folder into the VM at the same path, for `hostPath` volumes. Fixed at creation. |
| `--k3s-version v1.31.4+k3s1` | Pin the k3s release (default: the installer's stable channel). Fixed at creation. |
| `--k3s-arg=--disable=metrics-server` | Extra `k3s server` argument. Repeatable. Fixed at creation. |
| `--traefik` | Keep k3s's bundled Traefik ingress (off by default). |
| `--kubeconfig` | Where to write the kubeconfig (default `~/.kube/fluxvm-NAME.yaml`). |

Options marked "fixed at creation" are baked into the VM's first boot. To change one, `down --purge` and `up` again.

## The kubeconfig

`up` reads `/etc/rancher/k3s/k3s.yaml` from the guest over SSH, points `server:` at `https://127.0.0.1:<api-port>`, renames the
cluster, user and context from `default` to the VM name (so merging with other files does not collide), and writes it with mode 600
to `~/.kube/fluxvm-NAME.yaml`. Your own `~/.kube/config` is never touched. To use both:

```bash
export KUBECONFIG=~/.kube/config:$HOME/.kube/fluxvm-fluxvm-kube.yaml
kubectl config use-context fluxvm-kube
```

The API server certificate carries `127.0.0.1` as a SAN, so `kubectl` verifies it normally.

## Reaching services

* **Inside the cluster** everything works as usual (ClusterIP, DNS, NetworkPolicy via k3s's embedded controller).
* **From the Mac**, publish a NodePort: `fluxctl kube up --publish 30080` then `curl http://127.0.0.1:30080`. Changing `--publish`
  restarts only the forward.
* `kubectl port-forward` also works, since it goes through the API server.

## Images

The cluster has its own containerd. Images built with `fluxctl docker` are **not** visible to it. Push them to a registry the cluster
can reach, or import one directly:

```bash
docker save app:dev | ssh "$USER@$(fluxctl kube status | jq -r .guest_ip)" sudo k3s ctr images import -
```

## Keep it running

```bash
fluxctl kube agent install --publish 30080       # LaunchAgent dev.zyvor.fluxvm.kube runs `fluxctl kube supervise`
fluxctl kube agent uninstall
```

The supervisor starts the VM if it is stopped and restores the forward if it dies. The plist carries `FLUXVM_URL` and
`FLUXVM_VZ_RUNNER` when set, never a token.

## What this is not

* One node. There is no multi-node mode.
* The containers share the VM's kernel, unlike the per-container microVMs of the built-in sandboxes.
* `k3s` is downloaded from `get.k3s.io` on first boot, so that boot needs internet access.
