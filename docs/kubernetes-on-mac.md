# Kubernetes on a Mac (`fluxctl kube`)

`fluxctl kube` runs **a [k3s](https://k3s.io) cluster in managed FluxVM VMs (one server, and workers if you ask), with Cilium for
the pod network, its API server and a kubeconfig on your Mac.** It is the FluxVM counterpart of `colima start --kubernetes`, built the same way as [`fluxctl docker`](docker-engine.md):
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
| `--cni cilium\|flannel` | The pod network (default `cilium`; `flannel` with `--offline`). See [The pod network](#the-pod-network). Fixed at creation. |
| `--cilium-version 1.20.3` | The Cilium chart to install. |
| `--traefik` | Keep k3s's bundled Traefik ingress (off by default). |
| `--workers N` | Add N agent nodes (0 to 8), each its own VM named `NAME-w1`… and joined to the server. See [More than one node](#more-than-one-node). |
| `--offline DIR` | Install k3s from a folder made by `fluxctl kube fetch` instead of the internet. See [Without internet](#without-internet-on-first-boot). |
| `--kubeconfig` | Where to write the kubeconfig (default `~/.kube/fluxvm-NAME.yaml`). |

Options marked "fixed at creation" are baked into the VM's first boot. To change one, `down --purge` and `up` again.

## The pod network

Cilium is the default. k3s starts with `--flannel-backend=none --disable-network-policy`, and the server gets a `HelmChart` manifest
(a root-only file written by cloud-init) that k3s's built-in helm controller turns into a Cilium install from `helm.cilium.io`, with
the chart version pinned. The node becomes Ready once Cilium is up, which adds a minute or two to the first boot. Cilium also enforces
`NetworkPolicy`. kube-proxy stays on, so Services work as in stock k3s.

`--cni flannel` keeps k3s's bundled flannel. It is the default with `--offline`, because Cilium's chart and images come from the
internet (`--offline --cni cilium` is refused). With workers, flannel is told to use the private network card (`--flannel-iface`);
its default is the card with the default route, which is the NAT card, and VMs cannot reach each other over NAT.

## More than one node

```bash
fluxctl kube up --workers 2        # a server plus two agent VMs: fluxvm-kube, fluxvm-kube-w1, fluxvm-kube-w2
kubectl get nodes
```

`up` creates the server first, reads its node token over SSH, then creates each worker with that token and the server's address in
its cloud-init and waits until `1 + N` nodes are Ready. Run it again with a larger `--workers` to add nodes (for a cluster that was created with `--workers`); existing nodes are
left alone. Every worker has the same size as the server (`--cpus`, `--memory-mib`, `--disk-gib`) and the same `--share` folders.
`down` stops the server and all workers, and `down --purge` deletes them. To remove one worker, delete its node with
`kubectl delete node` and delete its VM through FluxVM.

The default FluxVM network keeps each VM apart from the others (two VMs on it cannot even ping each other), so a cluster with
workers puts every node on a second, private network named after the cluster (`apple.networks`, a `10.89.N.0/24` subnet with its own
switch, see [macos.md](macos.md)). The nodes advertise that address (`--node-ip`) and workers join the server through it, so node
and pod traffic stay on the private network while the first card still gives each VM internet access and the Mac its SSH route.
A pod on one node reaches a service on another; `scripts/kube-live-test.sh` checks that for Cilium and, with `FLUXVM_LIVE_CNI=flannel`,
for flannel.

* **Decide on workers when you create the cluster.** Only VMs created with `--workers` get the private card. A cluster created
  without it cannot gain workers later: `up --workers N` stops with a message, and you `down --purge` and start again.
* The node token is written into each worker's first-boot (cloud-init) configuration, so anyone with a login on a worker VM may be
  able to read it; the VMs are yours alone, but do not reuse the token elsewhere.
* If the server's private address ever changes, workers need re-creating.

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
* **Automatically**, while `fluxctl kube supervise` runs (the LaunchAgent does this): every NodePort and LoadBalancer service gets a
  forward on `127.0.0.1:<its node port>` within one check interval, and loses it when the service is deleted. Create a NodePort service
  and `curl http://127.0.0.1:<nodePort>` works with no flags. Only loopback; ports below 1024, the API port and ports something on the
  Mac already uses are skipped. `fluxctl kube services` lists them as JSON with `published` and `url`. Turn it off with
  `supervise --no-auto-publish`. Changing the set restarts the SSH forward, so `kubectl` may pause for a moment.
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

## See it in the Studio

`fluxctl dashboard` opens the Studio; **Platform → Kubernetes** shows each cluster the daemon's VMs belong to (the machines labelled
`fluxvm.kube`). It reads the cluster with the client certificate in `~/.kube/fluxvm-NAME.yaml` of the user the daemon runs as, through
the loopback port that file names (the forward `fluxctl kube` keeps up) and, failing that, the server VM's own address. Reading needs no
special role; the few changes below need the daemon's admin role.

* **Nodes and pods:** readiness, roles, versions and addresses; every pod with its status as `kubectl` prints it, restarts, node and
  IP, filterable by namespace and text. CPU and memory come from the cluster's metrics-server (k3s ships one): a usage bar per node,
  `kubectl top`-style columns per pod. If it is not answering yet, the page says so and shows no numbers.
* **Workloads:** Deployments, StatefulSets and DaemonSets with ready of wanted, up-to-date count and images, and Services with their
  type, cluster IP, ports (NodePorts included) and selector.
* **Events:** what the cluster has recorded, newest first, with a warnings-only switch and filters: the quickest way to see why a pod
  will not start.
* **Logs and Shell:** each pod has a window styled like Terminal.app. Logs are coloured (levels, keys, numbers, addresses, HTTP
  verbs and status codes; a line that carries its own ANSI colours keeps them), with a `-word` filter, tail size, the previous
  instance, follow, wrap, copy and download. **Shell** opens `/bin/sh` or `bash` in the container over the cluster's own `exec`; it
  needs the daemon's admin role, and an image without a shell (distroless) has nothing to open.
* **Traffic:** who talks to whom, as a service map from Cilium's Hubble flows (so it needs the default `--cni cilium`; a flannel
  cluster says so). Workloads are boxes, calls are arrows weighted by flow count, red where packets were dropped; click a workload
  for what it calls and what calls it, filter by namespace, name or port, and show or hide system pods. The daemon asks each Cilium
  agent for its recent flows with `hubble observe`, counts each packet event once (on the sender's node, replies skipped) and folds
  them into workload-to-workload edges. The window is however long each node's flow buffer (a few thousand flows) covers.

* **Changes:** a few things you do all the time, with a spinner on the button and a toast when the cluster answers. **+** and **−** on a
  Deployment or StatefulSet set its replicas (0 to 100); **Restart** rolls a Deployment, StatefulSet or DaemonSet the way
  `kubectl rollout restart` does; **Delete** removes a pod (after a confirmation; its controller makes a new one); **Cordon** and
  **Uncordon** mark a node unschedulable or schedulable again, and a cordoned node shows a *Cordoned* badge. The routes are
  `POST /v1/kube/{cluster}/pods/{ns}/{pod}/delete`, `.../workloads/{ns}/{kind}/{name}/scale` (body `{"replicas": N}`) and `/restart`
  (`kind` is `deployments`, `statefulsets` or `daemonsets`), and `.../nodes/{node}/{cordon|uncordon}`; names are checked before they
  reach a URL. Applying or editing YAML, and draining a node, stay with `kubectl`.

The Studio's **Machines** page also groups a cluster's VMs together (`kube NAME`), so they can be started or stopped as one.

## If the first boot stalls

The first boot installs packages with apt, so it needs this Mac to be online. If the VM never reports an address, or Kubernetes does not report its nodes Ready in
15 minutes, the command stops and says so; for a guest that is up it includes what cloud-init says in the guest (`cloud-init status` and the end of
`/var/log/cloud-init-output.log`). `fluxctl serial NAME` shows the guest's console. Fix the network and run the same command again: it picks the VM
up where it was.

## Checkpoints

```bash
fluxctl kube checkpoint before-upgrade     # save the running cluster, memory and disk
kubectl apply -f risky.yaml                # break things
fluxctl kube restore before-upgrade        # back as it was: every object, every running pod
fluxctl kube checkpoints                   # label, time, size, whether memory is saved (JSON, newest first)
fluxctl kube checkpoint-rm before-upgrade
```

A checkpoint is a snapshot of the server VM with its memory: Apple's Virtualization framework writes the guest's machine state and the disk
is cloned (APFS), so saving takes about a second and costs next to nothing until the disk changes. `restore` stops the VM, puts the disk
and the machine state back, starts it, waits for the node to be Ready and re-opens the API port forward; on a 2 vCPU, 3 GiB cluster that
took 13 seconds. What was in the cluster at the checkpoint is what you get, including pods that were still terminating. Nothing a tool
outside the VM holds (your kubeconfig, a `--publish` forward) is changed. `size` is the disk's apparent size, not what it uses.

Limits: a label is 1 to 40 lowercase letters, digits or dashes; a label is used once (remove it to reuse it); the cluster must be running to
be checkpointed; and a cluster made with `--workers` is refused, because its nodes are separate VMs on a private network that a snapshot
of one VM cannot put back consistently. A saved state restores only on the same Mac model and a compatible macOS.

The Studio's Kubernetes page has a **Checkpoints** tab with the same list, Save, Restore and Delete. After a restore from the Studio, `kube supervise` (or `kube up`) reopens the kubectl port.

## Try something, then undo it

`fluxctl kube try -- kubectl apply -f app.yaml` saves a checkpoint (named `try-<time>`), runs the command against this cluster, and keeps the
checkpoint. If it went wrong, `fluxctl kube undo` puts the cluster back as it was before, running pods included, and forgets that checkpoint,
so running `undo` again goes one step further back. The command's exit code is passed on. `--keep N` (default 5) deletes older `try-`
checkpoints. Put cluster flags such as `--name` before the `--`; everything after it is the command. Single-node clusters only.
On a real cluster, deleting a deployment through `try` and then `undo` brought back the deployment and its original pod (save 1.5 s, undo 12.5 s).

## Sleep and wake

`fluxctl kube sleep` saves the running cluster (a checkpoint named `sleep`) and stops its VM, so it costs no CPU and little memory.
`fluxctl kube wake` restores it exactly as it was and deletes that checkpoint. While `fluxctl kube supervise` runs, the sleeping
cluster keeps `127.0.0.1:<api-port>` open: the first `kubectl` call wakes it and is held until the API answers, so you just use
`kubectl`. On a real cluster sleeping took 6 s and the first `kubectl get pods` against the sleeping cluster answered in 3 s. Same
limits as checkpoints: single-node clusters only. Do not run `kube up` on a sleeping cluster; wake it.

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
