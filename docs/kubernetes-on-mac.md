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
| `--workers N` | Add N agent nodes (0 to 8), each its own VM named `NAME-w1`… and joined to the server. See [More than one node](#more-than-one-node). |
| `--offline DIR` | Install k3s from a folder made by `fluxctl kube fetch` instead of the internet. See [Without internet](#without-internet-on-first-boot). |
| `--kubeconfig` | Where to write the kubeconfig (default `~/.kube/fluxvm-NAME.yaml`). |

Options marked "fixed at creation" are baked into the VM's first boot. To change one, `down --purge` and `up` again.

## More than one node

```bash
fluxctl kube up --workers 2        # a server plus two agent VMs: fluxvm-kube, fluxvm-kube-w1, fluxvm-kube-w2
kubectl get nodes
```

`up` creates the server first, reads its node token over SSH, then creates each worker with that token and the server's address in
its cloud-init and waits until `1 + N` nodes are Ready. Run it again with a larger `--workers` to add nodes; existing nodes are
left alone. Every worker has the same size as the server (`--cpus`, `--memory-mib`, `--disk-gib`) and the same `--share` folders.
`down` stops the server and all workers, and `down --purge` deletes them. To remove one worker, delete its node with
`kubectl delete node` and delete its VM through FluxVM.

The workers reach the server by its address on the Mac's VM network (`192.168.64.x`). The token is written into each worker's
first-boot (cloud-init) configuration, so anyone with a login on a worker VM may be able to read it; the VMs are yours alone,
but do not reuse the token elsewhere. The pod network (flannel) spans the VMs, so a pod on one
node reaches a service on another; `scripts/kube-live-test.sh` checks that. If the server's address ever changes, workers need
re-creating.

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

The cluster has its own containerd, separate from the `fluxctl docker` engine. `fluxctl kube load` copies images from the Docker
CLI into every node (server and workers):

```bash
eval "$(fluxctl docker env)"             # the Docker CLI talks to the engine VM
docker build -t app:dev .
fluxctl kube load app:dev                # docker save, then k3s ctr images import on each node
kubectl run app --image=app:dev --image-pull-policy=Never --restart=Never
```

Give loaded images a tag other than `:latest` (Kubernetes pulls `:latest` again by default) or set `imagePullPolicy: IfNotPresent` or
`Never`. `fluxctl kube load --archive app.tar` imports an existing `docker save` archive instead. Image names may contain letters,
digits and `. _ / : @ -` only.

## Without internet on first boot

By default the first boot downloads the installer and k3s from `get.k3s.io` and GitHub, and the k3s system images from the
registry. To avoid that, download them once, then build clusters from the folder:

```bash
fluxctl kube fetch ~/k3s-offline                      # k3s-arm64, k3s-airgap-images-arm64.tar.zst, install.sh
fluxctl kube up --offline ~/k3s-offline --workers 1
```

`fetch` takes the stable release, or `--k3s-version v1.31.4+k3s1`, and checks the binary and the image archive against the release's
`sha256sum-arm64.txt` before moving them into place. The folder is shared read-only into each VM at the same path; cloud-init copies
the binary and images into place and runs the installer with `INSTALL_K3S_SKIP_DOWNLOAD=true`. Offline nodes also install no apt
packages, so nothing is fetched on first boot. The folder path must be absolute and use only letters, digits and `- _ . /`.

Two things still need a network or a local source: the `debian-13` base image (FluxVM fetches it the first time it is used) and the images
your own workloads use (pull them, or `fluxctl kube load` them). The installer script is fetched without a checksum because the
project does not publish one; read `install.sh` in the folder before using it somewhere that matters.

## Keep it running

```bash
fluxctl kube agent install --publish 30080       # LaunchAgent dev.zyvor.fluxvm.kube runs `fluxctl kube supervise`
fluxctl kube agent uninstall
```

The supervisor starts the VM if it is stopped and restores the forward if it dies. The plist carries `FLUXVM_URL` and
`FLUXVM_VZ_RUNNER` when set, never a token.

## What this is not

* No high availability: there is one server (control plane); extra nodes are agents only.
* The containers share the VM's kernel, unlike the per-container microVMs of the built-in sandboxes.
* Unless you use `--offline`, the first boot needs internet access.
