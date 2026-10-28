# Kubernetes on a Mac (`fluxctl kube`)

`fluxctl kube` runs **a [k3s](https://k3s.io) cluster in managed FluxVM VMs (one server, and workers if you ask), with k3s's bundled
flannel for the pod network (Cilium is opt-in), its API server and a kubeconfig on your Mac.** It is the FluxVM counterpart of `colima start --kubernetes`, built the same way as [`fluxctl docker`](docker-engine.md):
a Debian 13 VM, folders shared at matching paths, and a supervised SSH forward.

Because the cluster is a VM, it can do things a Linux kind or minikube cluster cannot: **save the whole running machine in about a
second** and put it back, so you can [undo a bad `kubectl apply`, bisect to the hour something broke, reset to a known-good state between
test runs, break the cluster on purpose, and let it sleep and wake on demand](#time-travel-for-your-cluster).

![Checkpoints on a timeline: automatic, manual, try and sleep points](assets/kube/timeline.svg)

The other half of the idea is that **you should forget the cluster is a VM.** `kubectl` already works, services have their usual ports
and `*.localhost` names, and the loop from a saved file to a running pod takes a second:

![The Mac, 127.0.0.1 and the cluster VM: ports, names and commands](assets/kube/mac-like-linux.svg)

## What 0.4.17 adds, in one picture

![The commands 0.4.17 puts around the cluster VM](assets/kube/kube-0417-map.svg)

Three groups, all talking to the same single-node cluster VM:

* **Reach it from the Mac:** [`kube share`](#open-it-on-your-phone-kube-share) (open a service on your phone), [`kube prompt`](#in-your-shell-prompt-kube-prompt), [`kube host`](#call-your-mac-from-a-pod-kube-host) (pods call a service on the Mac) and the menu bar, Raycast, Alfred and Shortcuts entries.
* **Operate it:** [`kube git`](#the-cluster-follows-your-branch-kube-git) (the cluster follows your branch), [`kube diff`](#what-changed-kube-diff-and-kube-timeline), [`kube doctor --fix`](#when-something-is-wrong-kube-doctor) and the [MCP tools](#let-an-ai-agent-operate-the-cluster-mcp-tools) for AI agents, which can run read-only.
* **Keep it tidy:** working hours and eco mode (see [Keep it running](#keep-it-running)), [`kube trim`](#give-disk-space-back-kube-trim), backup exclusion, and `kube trust --simulator` for the iOS simulator. `kube up --rosetta` runs amd64 images through the Mac's Rosetta.

These were built and unit-tested, and the release packages were installed and run, but the features were not exercised against a live cluster for 0.4.17. Whether the VM's disk passes discards through to the Mac (for `kube trim`) is still unconfirmed.

## Every command in one place

| I want to… | Run | More |
|---|---|---|
| set up a whole project with one command | `kube start` (reads `fluxvm-kube.yaml`) | [`kube start`](#a-project-in-one-command-kube-start) |
| start a cluster, stop it, throw it away | `kube up`, `kube down [--purge]`, `kube status`, `kube env`, `kube use` | [Use it](#use-it), [The kubeconfig](#the-kubeconfig) |
| run Postgres, Redis, Kafka and friends | `kube add APP`, `kube apps`, `kube remove APP`, `kube connect APP`, `kube open NAME` | [`kube add`](#services-next-to-your-app-kube-add) |
| reach a Service I already have | `kube forward SERVICE` (`--list`, `--remove`), `kube services` | [`kube forward`](#a-port-forward-that-stays-kube-forward) |
| keep a database across rebuilds | `kube db-backup APP`, `kube db-restore APP FILE` | [`db-backup`](#keep-your-data-when-you-rebuild-db-backup) |
| open `https://app.localhost` | `kube trust` (and `kube up --traefik` for Ingress hosts) | [Ingress](#ingress-hosts-just-work), [HTTPS](#https-kube-trust) |
| run the code I am writing | `kube dev` (rebuild an image), `kube sync` (copy files, no build), `kube load` (copy an image in) | [`kube dev`](#save-a-file-the-cluster-runs-it-kube-dev), [`kube sync`](#reload-without-rebuilding-kube-sync), [Images](#images) |
| see what changed between two checkpoints | `kube diff`, `kube timeline` | [`kube diff`](#what-changed-kube-diff-and-kube-timeline) |
| undo a mistake or find when it broke | `kube checkpoint`, `kube restore`, `kube checkpoints`, `kube checkpoint-rm`, `kube try`, `kube undo`, `kube bisect` | [Time travel](#time-travel-for-your-cluster) |
| start clean every time | `kube golden`, `kube reset`, `kube matrix` | [A clean start](#a-clean-start-every-time) |
| see whether my app copes with failure | `kube chaos` | [Break it on purpose](#break-it-on-purpose) |
| cost nothing while I am away | `kube sleep`, `kube wake`, `kube supervise --idle-sleep`, `--sleep-on-battery` | [Sleep and wake](#sleep-and-wake) |
| copy files to or from a pod | `kube cp` | [`kube cp`](#copy-files-kube-cp) |
| read the logs of every replica at once | `kube logs` | [`kube logs`](#every-replica-in-one-stream-kube-logs) |
| run the same images everywhere | `kube pin` | [`kube pin`](#the-same-bytes-everywhere-kube-pin) |
| see everything at a glance | `kube ps`, `kube menubar` | [`kube ps`](#one-screen-kube-ps), [Menu bar](#in-the-menu-bar-kube-menubar) |
| open a service on my phone | `kube share` | [`kube share`](#open-it-on-your-phone-kube-share) |
| see the cluster in my shell prompt | `kube prompt` | [Prompt](#in-your-shell-prompt-kube-prompt) |
| start, checkpoint and undo from Raycast | `integrations/raycast` | [Raycast](#raycast-alfred-and-shortcuts) |
| know what is wrong | `kube doctor`, and a notification from `kube supervise` | [`kube doctor`](#when-something-is-wrong-kube-doctor), [Notifications](#a-notification-when-a-pod-breaks) |
| keep it all running | `kube supervise`, `kube agent install` | [Keep it running](#keep-it-running) |
| have the cluster follow my git branch | `kube git install` | [`kube git`](#the-cluster-follows-your-branch-kube-git) |
| let pods call a program on my Mac | `kube host NAME PORT` | [`kube host`](#call-your-mac-from-a-pod-kube-host) |
| run an image that is only built for Intel | `kube up --rosetta` | [Intel-only images](#intel-only-images---rosetta) |
| let an AI agent (Claude, Cursor) operate the cluster | `fluxctl mcp serve` (kube tools; writes need `--allow-write`) | [MCP tools](#let-an-ai-agent-operate-the-cluster-mcp-tools) |
| park the cluster outside working hours or on low power | `kube supervise --active 09:00-19:00 --weekdays --eco` | [Working hours](#sleep-and-wake) |
| trust HTTPS in the iOS simulator | `kube trust --simulator` | [HTTPS](#https-kube-trust) |
| give freed disk space back to the Mac | `kube trim` | [`kube trim`](#give-disk-space-back-kube-trim) |
| work without internet | `kube fetch DIR`, `kube up --offline DIR` | [Without internet](#without-internet-on-first-boot) |

Most sections say what was proved on a real cluster and what was not; read those lines before relying on a feature.

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
| `--cni cilium\|flannel` | The pod network (default `flannel`; `cilium` is opt-in and not with `--offline`). See [The pod network](#the-pod-network). Fixed at creation. |
| `--cilium-version 1.20.3` | The Cilium chart to install. |
| `--traefik` | Keep k3s's bundled Traefik ingress (off by default). |
| `--workers N` | Add N agent nodes (0 to 8), each its own VM named `NAME-w1`… and joined to the server. See [More than one node](#more-than-one-node). |
| `--offline DIR` | Install k3s from a folder made by `fluxctl kube fetch` instead of the internet. See [Without internet](#without-internet-on-first-boot). |
| `--rosetta` | Run `linux/amd64` binaries through the Mac's Rosetta. See [Intel-only images](#intel-only-images---rosetta). Fixed at creation. |
| `--kubeconfig` | Where to write the kubeconfig (default `~/.kube/fluxvm-NAME.yaml`). |

Options marked "fixed at creation" are baked into the VM's first boot. To change one, `down --purge` and `up` again.

## The pod network

Flannel, k3s's bundled pod network, is the default, with its `host-gw` backend: pod traffic is routed, not wrapped in VXLAN (a VM has nothing to tunnel through), so the pod MTU is 1500 and there is less CPU work. The nodes of a multi-node cluster share a layer-2 network (the private card), which `host-gw` needs; `--k3s-arg=--flannel-backend=vxlan` brings the old backend back. The node is Ready about two and a half minutes after the first boot starts, and
`NetworkPolicy` is enforced by k3s's own controller. With workers, flannel is told to use the private network card
(`--flannel-iface`); its default is the card with the default route, which is the NAT card, and VMs cannot reach each other over NAT.

`--cni cilium` installs Cilium instead: k3s starts with `--flannel-backend=none --disable-network-policy`, and the server gets a
`HelmChart` manifest (a root-only file written by cloud-init) that k3s's built-in helm controller turns into a Cilium install from
`helm.cilium.io`, with the chart version pinned. kube-proxy stays on. It needs the internet (`--offline --cni cilium` is refused) and
it is the only way to get the Studio Traffic map.

**Cilium is not the default on a Mac because it did not come up on the test machine.** (On Linux, Cilium is the default; `--offline` always means flannel.) On a Mac mini (Apple silicon, 0.4.17) a Cilium cluster
stayed `NotReady` while Cilium's agent started, and the guest then stopped answering ping and SSH; the same VM and k3s with flannel
was Ready in about 160 seconds. The cause is not known. If you use `--cni cilium`, `kube up` names the Cilium pod it is waiting for.

## Reach pods and Services from the Mac: `kube route`

The cluster VM routes its own pod network (10.42.0.0/16) and Service network (10.43.0.0/16), but the Mac has no route to either, so a pod IP or a ClusterIP only answers inside the cluster. `fluxctl kube route` prints the two commands that send both ranges through the VM's address:

```
fluxctl kube route            # prints them, and says whether the routes are missing or out of date
fluxctl kube route --install  # runs them (asks for your password)
fluxctl kube route --remove --install
```

After that `curl http://<pod ip>:<port>` and `curl http://<service clusterIP>:<port>` work from the Mac. The routes are not saved across a reboot, and the VM's address (DHCP) can change when it restarts, so run `kube route --install` again if they stop answering. It knows k3s's default ranges only and refuses a cluster started with `--cluster-cidr` or `--service-cidr`. Whether the routes work end to end on a live cluster has not been checked yet.

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
A pod on one node reaches a service on another; `scripts/kube-live-test.sh` checks that for flannel and, with `FLUXVM_LIVE_CNI=cilium`,
for Cilium.

* **Decide on workers when you create the cluster.** Only VMs created with `--workers` get the private card. A cluster created
  without it cannot gain workers later: `up --workers N` stops with a message, and you `down --purge` and start again.
* The node token is written into each worker's first-boot (cloud-init) configuration, so anyone with a login on a worker VM may be
  able to read it; the VMs are yours alone, but do not reuse the token elsewhere.
* If the server's private address ever changes, workers need re-creating.

## The kubeconfig

`up` reads `/etc/rancher/k3s/k3s.yaml` from the guest over SSH, points `server:` at `https://127.0.0.1:<api-port>`, renames the
cluster, user and context from `default` to the VM name (so merging with other files does not collide), and writes it with mode 600
to `~/.kube/fluxvm-NAME.yaml`.

**Plain `kubectl` just works.** `up` also adds the cluster, user and context to your `~/.kube/config`, so `kubectl`, `helm`, `k9s`, Lens and
IDE plugins see it with no `KUBECONFIG` to export. It never switches you behind your back: the new context becomes the current one only
if you had none, so a `kubectl` pointed at another cluster stays pointed there. `fluxctl kube use` makes this cluster current. Your
file is backed up once as `~/.kube/config.pre-fluxvm`, other entries are kept untouched, and `down --purge` takes this cluster out again
(and deletes the file if it was created for it and nothing else is left). `up --no-merge-kubeconfig` leaves `~/.kube/config` alone, as does
`--kubeconfig PATH`; then to use both files:

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
  Mac already uses are skipped. `fluxctl kube services` lists them as JSON with `published`, `url` and `friendly_url` (`http://<service>.<namespace>.localhost:<port>`, which browsers and macOS resolve to loopback, so two services are told apart by name). Turn it off with
  `supervise --no-auto-publish`. Changing the set restarts the SSH forward, so `kubectl` may pause for a moment.
* **By name, on port 80.** While `supervise` runs, every published service is also reachable as
  `http://<service>.<namespace>.localhost` (and `http://<service>.localhost` for the `default` namespace): no port number, no
  `/etc/hosts`, because `*.localhost` resolves to this Mac. An unknown name answers with a page that lists the known ones. A request by
  name to a **sleeping** cluster wakes it (3 seconds measured) and is answered. macOS lets an ordinary user take port 80 only on the
  wildcard address, so the listener falls back to it and drops every connection that does not come from this Mac. If something already
  uses port 80 the names are skipped and the node-port URLs still work; `supervise --no-names` turns them off. HTTPS needs a CA you
  choose to trust ([`kube trust`](#https-kube-trust)). WebSocket upgrades pass through, so a dev server's hot reload works by name; any
  other request is told to close its connection after the response, because the name is read from the first request on a connection.
* `kubectl port-forward` also works, since it goes through the API server.

## Services next to your app: `kube add`

```bash
fluxctl kube add postgres        # postgresql://postgres:dev@127.0.0.1:5432/postgres
fluxctl kube connect postgres    # psql inside the pod: nothing to install on the Mac
fluxctl kube open mailpit        # opens http://mailpit.localhost
fluxctl kube apps                # what can be added, what is running
fluxctl kube remove postgres     # data kept; --delete-data erases it
```

`kube add` runs a ready-made Deployment, Service and (for apps that keep data) a volume in the cluster, waits until the app answers on
this Mac, and prints what to paste into your app. Each app gets its usual port on `127.0.0.1`; if something on the Mac already uses it
(a Homebrew Postgres on 5432, say) the app moves to the next free port and the printed address says so. Data lives on a volume, so
`remove` and `add` again brings it back; a second `add` is harmless.

| App | On the Mac | Login |
|---|---|---|
| `postgres` (17) | `127.0.0.1:5432` | `postgres` / `dev` |
| `redis` (7, persistent) | `127.0.0.1:6379` | none |
| `mysql` (8.4) | `127.0.0.1:3306`, database `app` | `root` / `dev` |
| `mongodb` (8) | `127.0.0.1:27017` | none |
| `rabbitmq` (4, management) | AMQP `5672`, UI `http://rabbitmq.localhost` (`15672`) | `guest` / `guest` |
| `nats` (2, JetStream) | `4222`, monitoring `http://nats.localhost` (`8222`) | none |
| `mailpit` | SMTP `1025`, inbox `http://mailpit.localhost` (`8025`) | none |
| `grafana` (11) | `http://grafana.localhost` (`3000`) | `admin` / `admin` |
| `redpanda` (Kafka API) | bootstrap `127.0.0.1:9092`, admin `http://redpanda.localhost` (`9644`) | none |
| `qdrant` (vector database) | REST `http://qdrant.localhost` (`6333`, dashboard at `/dashboard`), gRPC `6334` | none |
| `meilisearch` (1.12) | `http://meilisearch.localhost` (`7700`) | none (development mode) |
| `clickhouse` (24.12) | HTTP `8123`, native `9000` | `default` / `dev` |
| `keycloak` (26.0, dev mode) | `http://keycloak.localhost` (`8180`) | `admin` / `admin`; give the VM about 1 GiB more memory |
| `seaweedfs` (4.00, S3 storage) | master page `http://seaweedfs.localhost` (`9333`), S3 API `http://127.0.0.1:8333` | none; any S3 access key works |
| `vault` (1.18, dev mode) | `http://vault.localhost` (`8200`) | root token `dev`; secrets are in memory and vanish with the pod |
| `jaeger` (1.64, tracing) | UI `http://jaeger.localhost` (`16686`), OTLP gRPC `4317`, OTLP HTTP `4318` | none; traces are in memory |
| `temporal` (dev server) | UI `http://temporal.localhost` (`8233`), gRPC `7233` | none; state in a file on the volume |

These are development defaults with throwaway passwords, bound to loopback on the Mac; they are not for anything shared. An app's
port can be opened only if the supervisor (`fluxctl kube supervise`, which the LaunchAgent runs) or `add` itself opens it; keep them
open across restarts with `fluxctl kube agent install`. `connect` works for postgres, redis, mysql, mongodb and clickhouse (a shell in the pod
through the cluster); `open NAME` opens any published service, by its `.localhost` name while `supervise` runs.

### A port-forward that stays: `kube forward`

```sh
fluxctl kube forward web            # web:80 is on 127.0.0.1:8080, and http://web-local.localhost
fluxctl kube forward --list
fluxctl kube forward web --remove
```

For a Service you already have, this does what `kubectl port-forward` does, without the terminal you have to leave open and without dying when the pod restarts. It adds a small companion Service next to yours (`web-local`, same selector, a node port of its own) and `kube supervise` keeps the Mac port open for it. The Mac port is the Service's own port, or 8000 more when that is below 1024 (a Mac does not let you listen there), or the next free one; `--local` picks it, `--service-port` picks among several ports. Running it again changes nothing. Services without a selector (headless, external) are refused.

Proved on a real k3s: a ClusterIP Service answered on `127.0.0.1:8080` and at `web-local.localhost`; after its pod was deleted the same port answered from the new pod; running it twice kept the same node port; a Service with no selector, a missing Service and `--local 80` were refused with clear messages; `--remove` closed the port in about ten seconds. Not run: a UDP Service (the companion is TCP only), and a Service in another namespace.

### Keep your data when you rebuild: `db-backup`

```sh
fluxctl kube db-backup postgres             # postgres-20261026-101500.sql in the current directory
fluxctl kube down --purge                   # throw the cluster away…
fluxctl kube up && fluxctl kube add postgres
fluxctl kube db-restore postgres postgres-20261026-101500.sql
```

`db-backup` runs the database's own dump tool inside the pod (`pg_dumpall`, `mysqldump`, `mongodump`) and writes one file on the Mac, readable by you only; it never overwrites an existing file. `db-restore` loads such a file back, replacing what the app holds, and can be repeated safely. It first checks that the file is a complete dump of that app (a truncated file, or one from another tool or another database, is refused before anything is touched). The Postgres restore drops and recreates the databases in the dump, so a few `psql` errors for things that cannot be dropped (the `postgres` role itself) are normal and do not stop it. It works for `postgres`, `mysql` and `mongodb`; nothing needs installing on the Mac. For a whole cluster, with every app and its memory, a checkpoint is the right tool; this one is for carrying a database to a fresh cluster or another Mac.

Proved on a real k3s for all three: rows were written, backed up, the app and its data volume were deleted and added again (empty), and a restore brought the rows back (Postgres with its index; restoring a second time left two rows, not four). Not run: large databases (the whole dump is held in memory), Redis (not offered), and restoring onto a different major version of the database.

Your own Services can ask for a port of their own on the Mac the same way, with an annotation: `fluxvm.dev/local-ports: "30432=5432"`
opens `127.0.0.1:5432` for node port 30432.

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

### Intel-only images: `--rosetta`

The cluster's VM is arm64, so a program built only for Intel fails with `exec format error`. `fluxctl kube up --rosetta` gives the VM the
Mac's Rosetta and registers it for x86-64 programs at every boot, before k3s starts, so an amd64 binary inside a container runs.
It needs Rosetta installed on the Mac (`fluxvm-vz-runner install-rosetta`; `fluxctl vz doctor` checks) and, like the VM's size, is
fixed when the VM is created. `kube agent install` keeps the flag.

What this does and does not cover: it lets amd64 *programs* run. Kubernetes still pulls the image that matches the node, and an image
published only for amd64 has no arm64 entry, so the pull itself may be refused before Rosetta is involved. Whether and how that works
on this k3s is not yet proved (see the status line below); until it is, load such an image with `kube load` or build a multi-arch one.

Status: the VM request, the boot-time unit and the flag are unit-tested. Not run: a real cluster with an amd64-only image, so no
speed figure is given.

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
* **Traffic:** who talks to whom, as a service map from Cilium's Hubble flows (so it needs `--cni cilium`; a flannel
  cluster, the default, says so). Workloads are boxes, calls are arrows weighted by flow count, red where packets were dropped; click a workload
  for what it calls and what calls it, filter by namespace, name or port, and show or hide system pods. The daemon asks each Cilium
  agent for its recent flows with `hubble observe`, counts each packet event once (on the sender's node, replies skipped) and folds
  them into workload-to-workload edges. The window is however long each node's flow buffer (a few thousand flows) covers.

* **Checkpoints:** the cluster's checkpoints as a **timeline** (oldest left, coloured by kind: automatic, before a try, manual, asleep),
  where a click offers to go back to that point, and as a table with **Save**, **Restore** and **Delete**. An **Undo** banner offers to put
  the cluster back after a `fluxctl kube try`. **Sleep** saves the sleep checkpoint and stops the machine, and **Wake** restores it. Saving
  needs the cluster to be running; clusters with workers show why they cannot be checkpointed. After a restore from the Studio the kubectl port is closed until `kube
  supervise` (or `kube up`) reopens it; with supervise running that took about half a minute on a real cluster.
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

## Time travel for your cluster

Everything below rests on one thing a Linux kind, minikube or k3d cluster cannot do: **a checkpoint is the whole running machine**. Apple's
Virtualization framework writes the guest's memory and device state, and the disk is cloned with APFS, so saving takes about a second and
costs next to nothing until the disk changes. Putting a checkpoint back brings back every object and every running pod, not only what
`kubectl get -o yaml` could describe. The features are built from that, and they all work on single-node clusters.

![Checkpoints on a timeline: automatic, manual, try and sleep points, with restore, undo and bisect between them](assets/kube/timeline.svg)

| You want to | Command | Cost on a 2 vCPU, 3 GiB cluster |
|---|---|---|
| Save and return to a state | `kube checkpoint LABEL` / `kube restore LABEL` | save 1.1 s, restore 8 to 13 s |
| Undo a risky `kubectl apply` | `kube try -- CMD` then `kube undo` | save 1.5 s, undo 12.5 s |
| Keep a rolling history | `kube supervise --auto-checkpoint 1h` | about 1 s each, little disk |
| Find when it broke | `kube bisect -- TEST` | 5 restores for 7 checkpoints, 54 s |
| Start every test clean | `kube golden`, `kube reset`, `kube matrix` | reset about 8 s |
| Check your app survives failure | `kube chaos SCENARIO --check CMD` | fault, hold, restore |
| Stop paying for an idle cluster | `kube sleep`, `--idle-sleep`, `--sleep-on-battery` | sleep 6 s, wake 2 s |

### Checkpoints

```bash
fluxctl kube checkpoint before-upgrade     # save the running cluster, memory and disk
kubectl apply -f risky.yaml                # break things
fluxctl kube restore before-upgrade        # back as it was: every object, every running pod
fluxctl kube checkpoints                   # label, time, size, whether memory is saved (JSON, newest first)
fluxctl kube checkpoint-rm before-upgrade
```

`restore` stops the VM, puts the disk and the machine state back, starts it, waits for the node to be Ready, re-opens the API port forward
and steps the guest's clock. What was in the cluster at the checkpoint is what you get, including pods that were still terminating. Nothing
a tool outside the VM holds (your kubeconfig, a `--publish` forward) is changed. `size` is the disk's apparent size, not what it uses.

**The clock.** A restored guest wakes with the clock it was saved with: measured on a real cluster, 342 seconds behind after a 4 minute
gap, correcting itself in 20 to 80 seconds through the guest's time service. `kube restore`, `kube wake` and `kube undo` now restart that
service at once (skew 0 s right after the restore). A restore made from the Studio cannot (the daemon has no shell in the guest), so with `kube supervise` running the supervisor steps the clock when it reopens the port, and without it the guest catches up on its own within a minute.

Limits: a label is 1 to 40 lowercase letters, digits or dashes; a label is used once (remove it to reuse it); the cluster must be running
to be checkpointed; and a cluster made with `--workers` is refused, because its nodes are separate VMs on a private network that a snapshot
of one VM cannot put back consistently. A saved state restores only on the same Mac model and a compatible macOS.

### Try something, then undo it

`fluxctl kube try -- kubectl apply -f app.yaml` saves a checkpoint (named `try-<time>`), runs the command against this cluster, and keeps the
checkpoint. If it went wrong, `fluxctl kube undo` puts the cluster back as it was, running pods included, and forgets that checkpoint, so
running `undo` again goes one step further back. The command's exit code is passed on. `--keep N` (default 5) deletes older `try-`
checkpoints. Put cluster flags such as `--name` before the `--`; everything after it is the command. It works for a Helm upgrade, a CRD
install or a `kubectl delete ns`. On a real cluster, deleting a deployment through `try` and then `undo` brought back the deployment and its
original pod. The Studio shows an **Undo** banner while a `try` checkpoint exists.

### A rolling history

`fluxctl kube supervise --auto-checkpoint 1h --keep-auto 6` saves a checkpoint named `auto-<time>` every hour while the cluster runs and
deletes the oldest beyond the newest 6. A stopped or sleeping cluster is skipped. The shortest interval is one minute. A bad hour is one
`fluxctl kube restore auto-<time>` away, and `kube checkpoints` lists them. Off by default.

### Find when it broke

![kube bisect restoring the middle checkpoint each time until it names the first failing one](assets/kube/bisect.svg)

`fluxctl kube bisect -- COMMAND` is `git bisect` for the cluster. It takes your automatic checkpoints, oldest to newest, restores them by
binary search and runs COMMAND after each restore (exit code 0 means good). It tests the newest first (if that passes there is nothing to
find), then the oldest (if that fails it broke before the first checkpoint), then narrows down, and prints the first failing checkpoint and
the last good one. It saves the cluster as it is now (`bisect-start`) and puts that back at the end; `--stay` leaves it at the last tested
checkpoint instead. It needs at least two `auto-` checkpoints.

```bash
fluxctl kube supervise --auto-checkpoint 1h &      # collect checkpoints while you work
# ... later, something is wrong:
fluxctl kube bisect -- sh -c '! kubectl get cm bad'
# first failing checkpoint: auto-20261025-132520
# last good one:            auto-20261025-132419
```

Proved on a real cluster: 7 automatic checkpoints a minute apart, a ConfigMap created between the third and the fourth, and a test that
fails while it exists. Bisect restored 5 checkpoints in 54 seconds, named the checkpoint taken just after the change, and put the cluster
back in its broken present.

### What changed: `kube diff` and `kube timeline`

Every checkpoint is saved with a small inventory of what ran in the cluster at that moment: each Deployment, StatefulSet, DaemonSet, CronJob,
Service, Ingress, PersistentVolumeClaim, ConfigMap, Secret and Namespace, with its images, its replica count and a hash of its spec (the
cluster's own `kube-system` objects are left out). Two inventories compare without starting either checkpoint:

```bash
fluxctl kube diff                  # the newest checkpoint against the cluster now
fluxctl kube diff before-upgrade   # that checkpoint against now
fluxctl kube diff auto-20261027-100000 try-20261027-113012345
fluxctl kube timeline              # every checkpoint, with what changed since the one before
```

```
~ Deployment/shop/web: image web:1.4 -> web:1.5, replicas 2 -> 5, spec changed
+ Deployment/shop/worker
- Service/shop/legacy
(+1 -1 ~1)
```

This answers "what changed between when it worked and now" without a restore. Secret values are hashed on the Mac and never stored, so a changed
Secret shows only as `data changed`. The inventory costs one `kubectl get` before the snapshot, with a 20 s limit: a cluster that does not answer
still gets its checkpoint, just without an inventory. Checkpoints made before this existed show as `no inventory`. The inventories are small files
under the daemon's run directory and go away with their checkpoint.

Status: the inventory, the comparison, the secret handling, the file format and the timeline are unit-tested. Not run: on a real cluster, so the
`kubectl get` call and the extra time per checkpoint are unmeasured.

### A clean start every time

![golden, reset and matrix: each run starts from the same saved state](assets/kube/golden-matrix.svg)

`fluxctl kube golden` marks the cluster as it is now as the known-good state, replacing the previous one. `fluxctl kube reset` puts it back
in about 8 seconds and keeps it for the next reset. `fluxctl kube matrix --runs 10 -- ./e2e.sh` resets before every run, runs the command
and prints, for example, `7 of 10 runs passed; failed: 2, 5, 9` (exit code 1 if any failed). A test that fails some of the time is flaky;
one that fails every time is broken. After the last run the cluster is left as that run left it (`--runs` defaults to 5).

### Break it on purpose

![kube chaos: checkpoint, break, hold, check, restore](assets/kube/chaos.svg)

`fluxctl kube chaos SCENARIO --check 'COMMAND'` saves a checkpoint (`chaos-start`), causes a fault, waits `--hold` seconds (20 by default),
runs your check (exit 0 means the app coped; `KUBECONFIG` is set) and then restores the checkpoint.

| Scenario | What it does |
|---|---|
| `kill-pod` | Deletes a random pod outside `kube-system`. |
| `stop-k3s` | Stops k3s: the API server, scheduler and kubelet go away while the VM stays up. |
| `fill-disk` | Fills the server VM's disk to 90%. |

The exit code is 1 when the check fails. `--no-restore` undoes the fault where it can and leaves the cluster as it is. A pod-level chaos tool
cannot take back a dead control plane or a full disk; a restore can. Proved on a real cluster: `kill-pod` passed its check on a deployment,
`stop-k3s` failed a `kubectl get nodes` check as it should and the API came back after the restore.

### Sleep and wake

![A running cluster is saved and stopped, then restored by kubectl, a browser request or kube up](assets/kube/sleep-wake.svg)

`fluxctl kube sleep` saves the running cluster (a checkpoint named `sleep`) and stops its VM, so it costs no CPU and little memory.
`fluxctl kube wake` restores it exactly as it was and deletes that checkpoint. On a real cluster sleeping took 6 to 7 seconds and waking 2.

**What puts it to sleep, besides `kube sleep`:** while `fluxctl kube supervise` runs (the LaunchAgent does this),

* `--idle-sleep 30m` parks the cluster after that long with no traffic through the API port or the published services. It watches the bytes
  the SSH forwarder carries, with `nettop`, so an open Studio page counts as traffic. Off by default; one minute at least. `--idle-sleep 60`
  parked a real cluster after about 80 seconds.
* `--sleep-on-battery` and `--sleep-on-lid-close` (a laptop) park it while the Mac runs on battery or the lid is closed, once there has
  been no traffic for 2 minutes, so a `kubectl` that has just woken the cluster is not undone at once.

**What wakes it:** the first thing that connects. While parked, supervise keeps `127.0.0.1:<api-port>` open, and also the ports of the
services that were published when it went to sleep. The first connection wakes the cluster, is held until it answers and is then proxied
through, so you just use the cluster:

* `kubectl get pods` against a sleeping cluster answers after the wake (3 seconds measured).
* A browser or `curl` on a published service, such as `http://web.default.localhost:30080`, wakes it and gets the pod's answer (3 seconds
  measured). Only services published when it went to sleep are held; a restarted supervise forgets them until the next wake.
* `fluxctl kube up` on a sleeping cluster wakes it from the checkpoint instead of cold-booting (3 seconds measured).
* The Studio's **Wake** button.

**How long a wake takes depends on the Mac.** The 2 to 3 second figures above are from an idle Mac; on the Mac mini test machine (with other work running) a wake took about 19 seconds, `kubectl` gives up on a connection after 10 seconds ("TLS handshake timeout"), and the call that triggered the wake failed. The wake still finishes, so run the command again. `kubectl` has no setting for this timeout; `fluxctl kube wake` first avoids it.

Single-node clusters only. `kube agent install` takes `--idle-sleep`, `--auto-checkpoint`, `--keep-auto`, `--sleep-on-battery`,
`--sleep-on-lid-close`, `--active`, `--weekdays`, `--eco` and `--no-auto-publish` and passes them to the LaunchAgent.

**Working hours and energy.** `kube supervise --active 09:00-19:00 --weekdays` keeps the cluster awake only in those local hours (a window such
as `22:00-06:00` may cross midnight; `--weekdays` alone means awake all day Monday to Friday and asleep at the weekend). Outside them the
cluster sleeps once nothing has used it for 2 minutes, and it wakes by itself when the window opens. A `kubectl` call in the evening still
wakes it, and it goes back to sleep 2 minutes after you stop. `--eco` also sleeps it while the Mac is in Low Power Mode or throttling for heat
(`pmset -g` and `pmset -g therm`); an energy reason does not schedule a wake-up, the next client simply wakes it. Status: the window
arithmetic, the decisions and the `pmset` parsing are unit-tested, and the Low Power Mode line was read from a real Mac. Not run: a real sleep and
wake at the edge of a window, and the thermal output on a Mac that is actually throttling (that format is from documentation, not seen here).

### Ingress hosts just work

Start the cluster with Traefik (`fluxctl kube up --traefik`) and write an Ingress for a `*.localhost` host:

```yaml
spec:
  rules:
  - host: shop.localhost
```

While `kube supervise` runs, `http://shop.localhost` (and `https://shop.localhost` after `kube trust`) reaches it, with no port number and no `/etc/hosts` edit. Any `*.localhost` name that is not a published service's name goes to Traefik with its Host header intact, and your Ingress rules decide. A Service's own name still wins, so `http://web.localhost` reaches the Service `web`; a name no rule matches gets Traefik's 404, not the list of known names.

Proved on a real k3s with Traefik: `http://shop.localhost` and `http://api.shop.localhost` returned the two pods behind their Ingress rules, `https://shop.localhost` did too with the CA passed to curl, a host with no rule got Traefik's 404, and a request for a host outside `.localhost` was not forwarded. Not run: a browser, and an Ingress with TLS terminated by the cluster itself.

### HTTPS: `kube trust`

```sh
fluxctl kube trust
```

This creates a certificate authority of your own in `~/.fluxvm/ca` (its key is mode 0600 and never leaves that folder) and prints the one command that makes this Mac trust it. **Nothing is trusted until you run that command** (or `fluxctl kube trust --install`, which runs it and asks for your password); it changes the Mac's trust settings, so it is yours to run. Restart `kube supervise`, and every service also answers at `https://<service>.<namespace>.localhost` on port 443, with a certificate made for that exact name on first use. Only names ending in `.localhost` ever get one. The CA is limited to `localhost` names and cannot make intermediate CAs, so a client that honours name constraints rejects a certificate it signs for any other site, even if the key is stolen (checked with `openssl verify`: `shop.localhost` passes, `bank.example.com` fails with a permitted-subtree violation). Apps behind it see `x-forwarded-proto: https`. Safari, Chrome and curl use the system keychain; Firefox keeps its own store. To undo: `sudo security remove-trusted-cert -d ~/.fluxvm/ca/ca.pem` and delete the folder.

**iOS and iPadOS simulators.** `fluxctl kube trust --simulator` adds the same CA to every booted simulator (`xcrun simctl keychain booted
add-root-cert`), so an app or Safari in the simulator accepts `https://<service>.localhost` too. It changes only the simulators' own trust
stores, never this Mac's, so it asks for no password. Boot a simulator first (Xcode, or `xcrun simctl boot DEVICE`); the simulator shares this Mac's
loopback, so the `.localhost` names reach the same ports. Not run: a real simulator.

Proved on a real k3s, with the CA passed to curl rather than installed in the keychain: `https://web.default.localhost/` and `https://web.localhost/` returned the pod's page; without the CA curl refused the certificate (exit 60); an unknown name got the list of known names; a request for a name outside `.localhost` was refused at the handshake; the certificate chained to the CA with the exact name as its only SAN, server-auth only, valid one year. Not run: installing the trust into the real keychain, and a browser.

## Save a file, the cluster runs it: `kube dev`

```sh
fluxctl kube dev --app-image app:dev --deployment web
```

It builds the image in the current directory (`docker build`, or your own command with `--build`), copies it into the cluster, restarts the deployment and waits for the rollout, then watches the directory. Every save does it again. A build that fails is printed and the watcher keeps going; fix the file and save. `--once` does one cycle and stops. `.git`, `target` and `node_modules` are not watched. Use a tag other than `latest`, or Kubernetes pulls it again.

Proved on a real k3s: a pod served `v1`, I edited the file and it served `v2` with no other command, a deliberately broken Dockerfile was reported while the watcher stayed up, and fixing it gave `v3`. A cycle took about two seconds once the base image was cached.

## Reload without rebuilding: `kube sync`

```sh
fluxctl kube sync web --dir . --to /app
```

For apps that do not need compiling (Node, Python, Ruby, a static site) this is the fast loop: it copies the directory into the running pod now and again every time a file changes, in a fraction of a second, with no image build and no restart. It is what a bind mount gives you on Linux. `web` means `deploy/web`; `pod/NAME`, `statefulset/NAME` and `daemonset/NAME` work too, `--namespace` and `--container` pick the rest. `--then "kill -HUP 1"` runs a command in the pod after each copy, for a server that has to be told to reload; most dev servers (nodemon, uvicorn `--reload`, Vite) notice the change on their own. `--once` copies once and stops. `.git`, `target`, `node_modules`, `.venv` and `__pycache__` are not sent, and neither are Mac metadata files. The directory is sent whole each time, so keep it to source (more than 64 MiB is refused). A file you delete locally stays in the pod until the pod restarts. For a compiled language use `kube dev`, which rebuilds the image.

Proved on a real k3s: a busybox web server served its original page, `kube sync` replaced it with the local folder (a subfolder arrived, `node_modules` did not, no `._` files), an edit was visible in the pod about a second later, each copy took 0.2 to 0.4 s, the pod did not restart, `--then` ran, and a destination with a space and a `job/` target were refused. Not run: a real Node or Python dev server reloading, and a pod with several replicas (the copy goes to one pod).

## A project in one command: `kube start`

Put what the project needs in `fluxvm-kube.yaml` next to the code and commit it:

```yaml
name: shop                 # the cluster; optional (cpus, memory_mib, disk_gib, traefik too)
images: [app:dev]          # copied in from your Docker, as `kube load`
apps: [postgres, redis]    # as `kube add`
manifests: [k8s/]          # YAML files or folders, applied in order, paths relative to this file
forwards:                  # as `kube forward`
  - service: web
    local: 8080
```

```sh
fluxctl kube start          # builds all of it from nothing, then prints `kube ps`
```

A teammate clones the repository and runs one command. It adds nothing new underneath: it runs `up`, `load`, `add`, `apply` and `forward` in that order, and every one of them is safe to repeat, so running `start` again changes nothing. The file is checked before anything slow happens (an unknown app, a bad image name, a manifest path that leaves the folder or does not exist all stop it at once). Settings in the file win over flags. `--file` names a different project file; `kube down --purge` removes it all.

Proved on a real k3s: from no cluster, one `start` built the cluster, Redis (answered `PONG`), a web Deployment and Service from a `k8s/` folder, and a forward that served the page, in 4 minutes 26 seconds (most of it first boot); a second `start` took 7 seconds and left every object as it was; a file naming an unknown app failed at once without creating a cluster. Not run: `images:` (it uses `kube load`, proved separately), Traefik and several apps together, and workers.

### A project from an address

`--file` also takes an `https://` address, so a project can live in a repository and be started without cloning it:

```sh
fluxctl kube start --file https://raw.githubusercontent.com/acme/shop/main/fluxvm-kube.yaml
```

A project runs images, manifests and ports on your machine, so without `--yes` it fetches the file, prints what it would do and
stops:

```text
https://raw.githubusercontent.com/acme/shop/main/fluxvm-kube.yaml would do this on your Mac and in the cluster:
  cluster shop
  add apps: postgres
  apply https://raw.githubusercontent.com/acme/shop/main/k8s/web.yaml
  open a port on this Mac for default/web
Only run projects you trust. Add --yes to go ahead.
```

Manifests are fetched from beside the project file, so each is listed as a `.yaml` file (a folder cannot be listed over HTTP), at most 30
of up to 1 MiB each, and nothing may climb out of the project's directory. Only `https` is fetched, and a redirect to plain `http` is
refused.

## The same bytes everywhere: `kube pin`

A tag such as `postgres:17-alpine` can point at a different image next month. `kube pin` prints what the cluster is running right
now as tag plus digest:

```text
$ fluxctl kube pin
docker.io/library/postgres:17-alpine@sha256:3f1c...e9
docker.io/library/redis:7-alpine@sha256:81aa...07
```

Paste the lines into a manifest's `image:` field and the same bytes run on every machine. System pods are left out; `--all` includes
them. A pod that has not pulled its image yet has no digest and is skipped.

## Copy files: `kube cp`

```sh
fluxctl kube cp ./site web:/usr/share/nginx/html      # Mac -> pod: the folder `site` lands inside that directory
fluxctl kube cp web:/var/log/nginx ./logs             # pod -> Mac: into the folder ./logs (created if missing)
fluxctl kube cp pod/web-1:/etc/hosts . --namespace shop --container app
```

The pod side is `NAME:/absolute/path` (`NAME` is a Deployment; `pod/`, `statefulset/` and `daemonset/` work too). The destination is
always a directory, like `docker cp` into an existing folder. A path with a colon that is meant for the Mac starts with `./` or `/`.
Files travel as a tar stream through the VM, 64 MiB at most, so it needs nothing in the pod beyond `tar`. A missing source path is
reported before anything is copied. Copying between two pods is refused; copy to the Mac first.

## In the menu bar: `kube menubar`

`kube menubar` prints the cluster's state in the format that [SwiftBar](https://swiftbar.app) and [xbar](https://xbarapp.com) read:
the title is the pod count (`⎈ 9`), red with a warning when a pod is failing, `zzz` when the cluster sleeps. The menu lists failing
pods, the apps and their Mac ports, the `.localhost` names to open, the checkpoints, and clickable Sleep, Wake, Doctor and Status.

```sh
fluxctl kube menubar --plugin > ~/Library/Application\ Support/xbar/plugins/fluxvm-kube.30s.sh   # or SwiftBar's folder
chmod +x ~/Library/Application\ Support/xbar/plugins/fluxvm-kube.30s.sh
```

The `30s` in the file name is the refresh interval. FluxVM ships no menu-bar app of its own; this needs SwiftBar or xbar, which are
separate programs.

## The cluster follows your branch: `kube git`

```bash
cd ~/code/shop
fluxctl kube git install        # puts a post-checkout hook in this repository
git checkout -b add-orders      # the new branch starts from the cluster as it is now
# ... migrate the database, deploy, try things ...
git checkout main               # saves the cluster for add-orders, puts back the one for main
git checkout add-orders         # and back again
```

A checkpoint is a whole-machine snapshot, so each branch keeps its own databases, migrations and deployed state. On a branch checkout the hook
saves the cluster as the checkpoint `git-BRANCH` for the branch you leave and restores the one for the branch you enter, which takes about as
long as `kube restore` (8 to 13 s on a 2 vCPU cluster; over a minute on a Mac that is short of memory). A branch with no checkpoint yet starts as a copy of where you were, as a new git branch
starts from the commit you were on. The first time you leave a branch the cluster has no record of, the hook takes it to be the branch you were on (from git's reflog) and saves it, so its changes do not leak into the next branch. A branch name with a slash or capitals gets a short hash in its checkpoint label, so `feature/x` and
`feature-x` stay apart; `kube checkpoints` lists them.

It only touches a cluster that is running and awake, never a detached HEAD or a file checkout, and a problem with the cluster is printed but
never makes `git checkout` fail. The cluster remembers which branch its state belongs to per repository, so a checkout while the cluster was
stopped does not mislabel anything. The hook repeats the options `install` was given that say where the cluster is (`--api-port`, `--user`, `--kubeconfig`, `--no-merge-kubeconfig`), so install it with the same flags you started the cluster with. `kube git status` shows the hook, that record and the saved branches; `kube git uninstall` removes the hook
(a `post-checkout` hook you wrote yourself is never replaced without `--force`).

Limits: single-node clusters only (the same as checkpoints), and each saved branch takes disk, so remove old ones with `kube checkpoint-rm git-BRANCH`.
Two working copies of one repository share the cluster but are tracked separately, so do not switch both at once.

Status: the branch-name labels, the save/restore plan, the state file and the hook script are unit-tested. Not run: the hook in a real repository
against a real cluster, so the 10 s switch and a restore that fails halfway are untested.

## Let an AI agent operate the cluster: MCP tools

`fluxctl mcp serve` (registered with Claude Code, Cursor, Codex and others by `fluxctl mcp install`) now also offers the cluster as tools, with the safety net `try` and `undo` give you by hand:

| Tool | Does | Needs `--allow-write` |
|---|---|---|
| `kube_status`, `kube_doctor`, `kube_checkpoints`, `kube_diff`, `kube_timeline` | look: pods and services, the doctor's report, checkpoints, what changed | no |
| `kube_apply` | saves a `try-` checkpoint, applies a YAML manifest, then reports what changed and how to undo | yes |
| `kube_checkpoint`, `kube_restore`, `kube_undo` | save the cluster, put it back, undo the last `kube_apply` | yes |

Every tool takes an optional `cluster` (default `fluxvm-kube`). An agent that applies a bad manifest, or a good one that breaks something, gets the
diff in the same reply and can call `kube_undo`; you can do the same from a terminal with `kube undo`. The server lists the write tools only when it
was started with `--allow-write`. A manifest larger than 256 KiB is refused, and the last 5 safety checkpoints are kept.

Status: the tool list, the read-only/write split, argument checking and the report text are unit-tested. Not run: an agent driving a real cluster, so
the checkpoint-apply-diff-undo round trip through MCP is untried.

## Call your Mac from a pod: `kube host`

`kube forward` brings a Service out to the Mac; `kube host` goes the other way. A program on the Mac becomes a Service in the cluster:

```bash
ollama serve &                          # listens on 127.0.0.1:11434
fluxctl kube host ollama 11434          # runs until Ctrl-C
kubectl run t --rm -it --image=curlimages/curl -- curl http://ollama:11434
```

Pods use `ollama:11434` (or `ollama.default.svc`) and reach whatever listens on the Mac's 127.0.0.1:11434: a model server, Postgres.app, a dev
server you are debugging. The VM reaches the Mac at its network gateway address, and only a program listening there can be reached, so the
command opens a relay on that one address that forwards to the Mac's loopback. The relay lets in only the VM's own /24 network, never the
Wi-Fi, and the Service it makes (a Service without a selector and an EndpointSlice pointing at the Mac, both labelled `fluxvm.dev/host-of`) goes
away with Ctrl-C or SIGTERM (`kill`, a closed terminal, launchd). It never touches a Service it did not create. `-n NAMESPACE` picks the namespace; `--remove` clears one left behind by a crash.

This is a plain TCP relay, so it works for anything that speaks TCP. It does not give pods the Mac's GPU: a model server running on the Mac
uses it, and the pods just call that server.

Status: the address parsing, the subnet rule, the manifests and the relay are unit-tested. Not run: a real cluster. The Service, the
EndpointSlice and a pod reaching the Mac have not been tried, and whether the cluster's pod network sends a pod's traffic to the Mac
from the node's address (which the subnet rule relies on) is not yet confirmed.

## Open it on your phone: `kube share`

```sh
fluxctl kube share web          # or shop/web when the name is in several namespaces
```

A published service answers only on 127.0.0.1. `kube share` opens one extra port on the Mac's network address that relays to it,
prints `http://<your-mac>.local:PORT` (and the Wi-Fi IP address) with a QR code, and closes on Ctrl-C or after `--minutes` (60 by
default, at most 480). Scan the code with the phone's camera to try the app on a real device.

It is meant for your own network, and says so: only this Mac and private or link-local addresses (192.168.x.x, 10.x.x.x,
172.16-31.x.x, 169.254.x.x, fe80::, fd00::) are let in, so a router that forwards a public address still gets refused; at most 64
connections at once; nothing stays open after it stops. Anyone on the same Wi-Fi who knows the address can reach the service while it
runs. The service must already be open on the Mac (`kube forward`, `kube add`, or a published NodePort).

Unit-tested: picking the service, the allowed addresses, the Bonjour name, the QR code, and a relay between two local sockets.

## In your shell prompt: `kube prompt`

```sh
fluxctl kube prompt                 # ⎈ dev 12/12     ⎈ dev 10/12 ✗2     ⎈ dev 💤
fluxctl kube prompt --plain         # k8s dev 10/12 !2
fluxctl kube prompt --snippet zsh   # the lines to add (starship, zsh, bash or fish)
```

It prints one line from a small file that `kube supervise` rewrites on every check (about every 30 seconds), so it answers at once, does
not need the daemon, and never wakes a sleeping cluster. With no supervisor running, or a file older than 150 seconds, it prints
nothing, so your prompt never shows a state that is no longer true. Pods count as ready unless they are failing or have restarted
three or more times (the same rule as `kube ps`). Because the file is kept up to date, `supervise` now looks at the pods on every check
even with `--no-notify`; `--no-notify` still only turns the messages off.

## Raycast, Alfred and Shortcuts

`integrations/raycast/` has eight small script commands: start, checkpoint (with an optional label), undo the last try, sleep, wake,
status, doctor, and share with my phone. In Raycast add the folder under Settings, Extensions, Script Commands. They are plain
`bash`, so Alfred's "Run Script" and Apple Shortcuts' "Run Shell Script" can use the same files. `scripts/test-raycast-scripts.sh`
checks that they parse and have the headers Raycast reads. Not run: Raycast itself, Alfred or Shortcuts.

## Every replica in one stream: `kube logs`

`kubectl logs deploy/web` shows one pod. `kube logs` reads them all, every container, and tags each line with its pod (a
different color per pod on a terminal):

```text
$ fluxctl kube logs chat --tail 1
[pod/chat-558c5574f-j78hv/say] hello from chat-558c5574f-j78hv
[pod/chat-558c5574f-j78hv/side] sidecar
[pod/chat-558c5574f-xjtpk/say] hello from chat-558c5574f-xjtpk
[pod/chat-558c5574f-xjtpk/side] sidecar
```

`NAME` is a Deployment; `statefulset/NAME`, `daemonset/NAME` and `pod/NAME` work too. `-f` keeps streaming, `--since 10m`
limits the age, `--tail N` sets the lines per pod (default 50), `--container C` picks one container, `--previous` reads the
container before a restart, `--timestamps` adds times. The pods come from the workload's own label selector, up to 50 at
once. `NO_COLOR=1` turns the colors off, and a pipe (`| grep`, `| head`) gets plain text.

## One screen: `kube ps`

```text
$ fluxctl kube ps
cluster      dr  running  192.168.64.170
kubectl      context dr, API https://127.0.0.1:6551 answers
pods         9 fine, 1 failing
               default/crashy Error (restarts 4)
apps         redis :6379
forwards     web (default) 80 -> 127.0.0.1:8080
names        http://redis.localhost  http://web-local.localhost  http://webnp.localhost
checkpoints  1, newest first (2m ago)
```

It is `docker ps` for the whole cluster: the state, the pods that are failing (a bad status, or three or more restarts; `kube doctor` says what to do about them), the apps and forwards open on this Mac, the names that answer, and the checkpoints. A cluster that is asleep, stopped or not created gets one line saying so and what to run. Proved on a real k3s through all four states.

## When something is wrong: `kube doctor`

```sh
fluxctl kube doctor
```

It checks what tends to go wrong for a cluster on a Mac and prints the fix next to each problem: free disk on the Mac, a VPN that takes all traffic, a kubeconfig that is missing or points at the wrong port, the VM's disk, whether the VM can resolve names and reach the image registry, the VM's clock against the Mac's, and pods that are crash-looping, out of memory, cannot pull their image, or keep restarting. A sleeping cluster is reported as asleep. It exits non-zero when something failed, so it can gate a script.

`fluxctl kube doctor --fix` makes the repairs that are safe and small, inside the VM, and then checks again: it removes container images no pod uses (when the VM's disk is nearly full), restarts the VM's resolver (when it cannot resolve names) and steps the VM's clock to the Mac's (when it is more than a minute off). Each problem is fixed once, a repair that fails is reported without stopping the others, and the lines marked "`--fix` does this" in the report are exactly these. It also looks at the VM folder (`~/Library/Application Support/FluxVM`): when Time Machine has a destination and still backs it up, or Spotlight indexes it, the report says so (gigabytes of disk images that change all day) and `--fix` excludes the folder with `tmutil addexclusion` and a `.metadata_never_index` file, on the Mac and without a password. Everything else (a crash-looping pod, a full Mac disk, a VPN) is yours to decide, so it only prints the advice.

Proved on a real k3s: a healthy cluster passes every check; with one pod killed for memory and one pointing at an image that does not exist, it named both and exited 1. Not run: a VPN, a full disk and a skewed clock on a real machine (those verdicts are unit-tested only).

### A notification when a pod breaks

While `kube supervise` runs (or the LaunchAgent from `kube agent install`), it looks at the pods every 30 seconds and shows a macOS notification when one starts failing: crash-looping, out of memory, or an image that cannot be pulled. Each problem is announced once, after it has looked bad twice in a row so pods that are still starting stay quiet, and again only if the pod has been fine for a while and then breaks again. The notification points at `fluxctl kube doctor`. `--no-notify` turns it off.

Proved on a real k3s: no notification with a healthy cluster; a crash-looping pod and an out-of-memory pod each produced exactly one over three minutes while they kept restarting, and a healthy pod produced none. The messages were written to a log file through a test switch (`FLUXVM_NOTIFY_LOG`), and a real `osascript` call returned success over SSH. Not run: seeing the banner on a screen.

## Keep it running

```bash
fluxctl kube agent install --publish 30080       # LaunchAgent dev.zyvor.fluxvm.kube runs `fluxctl kube supervise`
fluxctl kube agent uninstall
```

The supervisor starts the VM if it is stopped and restores the forward if it dies. Only one supervisor can run per cluster: a second `kube supervise` for the same name is refused (it names the running one's pid), because two of them race each other's sleep and wake and can destroy the cluster's disk. With the options above it also publishes services, takes
automatic checkpoints, and sleeps and wakes the cluster. The plist carries `FLUXVM_URL` and
`FLUXVM_VZ_RUNNER` when set, never a token.

## Give disk space back: `kube trim`

Deleting images and data inside the cluster VM does not shrink its disk file on the Mac by itself. `fluxctl kube trim` runs `fstrim` in the VM so the host can reclaim the freed blocks.

It first asks the VM's root device for its discard limit. If that is 0, the disk does not pass discards through, and trim stops with that reason instead of pretending to free space. Whether your VM's disk honors discards is not yet confirmed on a real cluster; check with `du -sh` on the VM folder before and after.

## What this is not

* No `kube fork` (a second running cluster from a checkpoint): FluxVM refuses to fork a VM on the Mac's NAT, because a saved machine state cannot be restored onto a different network attachment (the copy's network card comes up disconnected), and cluster VMs use that NAT. For a clean copy per pull request use `kube golden` and `kube reset`. 
* No high availability: there is one server (control plane); extra nodes are agents only.
* The containers share the VM's kernel, unlike the per-container microVMs of the built-in sandboxes.
* Unless you use `--offline`, the first boot needs internet access.
