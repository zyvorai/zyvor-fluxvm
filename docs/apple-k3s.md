# Apple-silicon native k3s (`fluxctl kube apple`)

`fluxctl kube apple` runs k3s in **persistent ARM64 Linux guests on Apple Virtualization, one VM per node**, and drives the whole
lifecycle from the Mac. It sits next to [`fluxctl kube up`](kubernetes-on-mac.md), which it does not replace:

| | `kube up` | `kube apple` |
|---|---|---|
| k3s comes from | `get.k3s.io` on first boot (cloud-init) | a binary **you** downloaded, checked against its published SHA-256 on the Mac and again in the guest, or a preinstalled service |
| SSH host key | not checked (addresses are recycled) | generated on the Mac and **pinned before the guest boots**; strict checking afterwards |
| Token | read over ssh into a command line | read on the server, **sent on stdin**, kept in a root-only file, never printed or put in an argument |
| Nodes | a server plus `--workers N` fixed at creation | any node later: `join` (the private network is always attached) |
| Crash in the middle | `up` again | a **journal** is written before each VM exists; `recover` repairs it from the VMs' ownership labels |
| Day-2 | checkpoints, dev loop, apps | `upgrade` with rollback, datastore `backup`/`restore`, `persist` checks, `status` that fails when unhealthy, `doctor`, token rotation, scheduled snapshots with S3, `remove`, `rebuild` |
| Control plane | one server | `create --servers 3`: embedded etcd, failover forwarder or kube-vip endpoint, shared flags |

Use `kube up` for a quick developer cluster with time travel; use `kube apple` when the cluster should be long-lived, pinned to a k3s
build you vetted, and spread over several VMs or Macs.

> **Verification status.** The lifecycle logic is unit-tested against a fake host (order of operations, cleanup, redaction), and the
> failover forwarder is tested over real loopback sockets. The shell scripts that run inside the guests, the Apple Virtualization
> networking, `etcd-snapshot`/`cluster-reset`, the real `ssh` tunnels, multi-server etcd, kube-vip, token rotation, scheduled snapshots and S3,
> node removal, the tunnel LaunchAgent and `rebuild` were **not run against real hardware**. Treat the first cluster as an acceptance test: see [What to verify on a Mac](#what-to-verify-on-a-mac).

## Prerequisites

* An Apple-silicon Mac, `fluxctl` and the FluxVM daemon (or `--server`).
* An **ARM64 Linux image** with systemd, cloud-init, sshd, `sha256sum` and passwordless `sudo` for the guest user. The default
  `debian-13` fits. Nodes get a second network card on a private switch named after the cluster (`10.89.N.0/24`).
* The official `k3s-arm64` release and its digest, downloaded by you (or `fluxctl kube fetch DIR`, which already verifies it):

```sh
V=v1.31.4+k3s1
curl -LO "https://github.com/k3s-io/k3s/releases/download/$V/k3s-arm64"
curl -LO "https://github.com/k3s-io/k3s/releases/download/$V/sha256sum-arm64.txt"
```

## Create a single-server cluster

```sh
fluxctl kube apple create dev --binary ./k3s-arm64 --sha256-file ./sha256sum-arm64.txt
fluxctl kube apple tunnel dev            # 127.0.0.1:16443 -> the server's API, host key pinned
KUBECONFIG=~/.kube/fluxvm-apple-dev.yaml kubectl get nodes
```

What `create` does, in this order:

1. Validates the name, SANs and the **digest of the binary on the Mac**; a mismatch stops before any VM exists.
2. Writes the **journal** (`~/.fluxvm/apple-k3s/dev.json`, mode 600; `$FLUXVM_APPLE_K3S_HOME` overrides the folder).
3. Generates an ed25519 host key on the Mac, puts it in the VM request (cloud-init `write_files` plus an `sshd_config.d` drop-in, so
   cloud-init's own host-key regeneration cannot replace it), creates the VM and records its id, then writes the ownership labels.
4. Waits for ssh with `StrictHostKeyChecking=yes` against the pinned key (under its own alias, so a recycled address never matches).
5. Checks the guest (`aarch64`, systemd, `sudo -n`, `sha256sum`), streams the binary on stdin, **re-verifies the digest in the guest**,
   installs it, writes the systemd unit (Traefik disabled, `--write-kubeconfig-mode 600`, `--node-ip` on the private card,
   `Restart=always`), starts it.
6. Waits until the node is Ready, records the k3s version and digest, and writes the kubeconfig (mode 600, pointed at the tunnel).

If any step fails, run the **same command again**: the journal and the VM are reused and only the missing steps run. A VM that already
has the expected name but belongs to someone else is never taken over.

Options: `--embedded-etcd` (use `--cluster-init`; needed for a second server and for etcd snapshots), `--tls-san NAME` (repeatable),
`--api-port` (give each cluster its own), `--cpus/--memory-mib/--disk-gib/--image`, `--kubeconfig`.
A prepared image with the k3s binary and `k3s.service` already installed uses `--preinstalled` instead of `--binary`.

## More nodes

```sh
fluxctl kube apple join dev                       # agent dev-a1 (binary copied from the server and verified)
fluxctl kube apple join dev --node big --cpus 4   # a named node
```

Each node is its own VM on the cluster's private network. The token is read on the server (`node-token`), validated, and written to the
new node through **stdin** into a root-only file that the unit reads with `--token-file`. It does not appear in an argument list, a
script, the journal, a log line or an error message (errors are scrubbed). Without `--binary`, the server's installed binary is copied and
checked against the digest recorded at creation; give `--binary/--sha256` to install another one.

A node that fails to join is **removed again** (the node object, the VM and the journal entry) so the cluster never holds a half-joined
node. `--keep-failed` keeps it for inspection. If even the VM cannot be deleted, it stays in the journal and the error says so.

`--role server` adds a control-plane node and needs a cluster created with `--embedded-etcd`.

### High availability: `create --servers 3`

```sh
fluxctl kube apple create prod --servers 3 --binary ./k3s-arm64 --sha256-file ./sha256sum-arm64.txt \
    --api-port 16443 --tls-san cluster.example --disable servicelb
fluxctl kube apple tunnel prod --install-agent          # keeps the forwarder up (see below)
KUBECONFIG=~/.kube/fluxvm-apple-prod.yaml kubectl get nodes
```

`--servers N` (1, or an odd number up to 7) needs embedded etcd and implies `--embedded-etcd`. The first server is created with
`--cluster-init`; the others (`prod-s2`, `prod-s3`) are joined **one at a time** (etcd admits one new member at a time) through the
first server that answers, each with the cluster's server token **and the same agent token**, both on stdin. If a join fails, the node
stays in the journal as unjoined and **running the same `create` command again** finishes the rest without a second VM. A server added
later with `join --role server` makes the cluster multi-server from then on.

**Flags the servers must agree on.** k3s requires `--cluster-cidr`, `--service-cidr`, `--cluster-dns`, `--cluster-domain`, `--disable`
and `--secrets-encryption` to match on every server. `create` takes them as `--cluster-cidr`, `--service-cidr`, `--cluster-dns`,
`--cluster-domain`, `--disable NAME` (repeatable; Traefik is always disabled) and `--secrets-encryption`, records them in the journal and
puts the same text in every server's unit, so they agree by construction. `kube apple doctor` reads each server's unit back and fails when
one differs (someone edited it). `--tls-san` is applied to every server too.

**The stable endpoint.** Two options, `--endpoint proxy|kube-vip`:

| | `proxy` (default with several servers) | `kube-vip` |
|---|---|---|
| What it is | a TCP forwarder on this Mac at `127.0.0.1:API_PORT` over one **pinned-host-key ssh tunnel per server** (on `API_PORT+1 … +N`) | the above, plus a kube-vip static pod on every server holding one address (`--vip 10.89.x.y`) on the private network, added to the API certificate |
| Failover | the forwarder sends each new connection to the first healthy server and skips one whose tunnel closes at once or refuses | kube-vip moves the address between servers (ARP, leader election) for anything on the private network |
| Needs | nothing on the servers | the kube-vip image pullable by the servers |

The Mac cannot reach the private network, so the forwarder is always how the Mac reaches the API; kube-vip only adds a floating address
for nodes and other VMs on the private network (agents already balance across servers by themselves once joined). The forwarder holds no
credentials and does not speak TLS: it copies bytes. Give each HA cluster an `--api-port` with eight free ports above it.

**The kubeconfig** has the usual context `apple-NAME` (through the forwarder) and one context per server (`apple-NAME-s1`, `-s2`, …) that
goes straight to that server's tunnel, for when the forwarder is not running; kubectl cannot list several servers in one cluster entry.

**Operating it.** `start` boots every server before it waits for the first to be Ready (it cannot be Ready without a majority); `stop`,
`status` (which reports `etcd has no quorum: 1 of 3 servers Ready, 2 needed`), `upgrade` (servers one at a time) and `delete` work on all
of them. An even number of servers tolerates no more failures than one fewer; `doctor` warns about it.

### Several Macs

On the Mac that owns the control plane, create the cluster with `--embedded-etcd --tls-san cluster.example` and make the API reachable at a
stable address. Copy the token privately (`sudo cat /var/lib/rancher/k3s/server/token` in the server, over a secure channel) into a file
with mode 600 on the other Mac, then:

```sh
fluxctl kube apple join remote --url https://cluster.example:6443 --token-file ./server-token \
    --binary ./k3s-arm64 --sha256-file ./sha256sum-arm64.txt
```

The other Mac's FluxVM creates a VM and joins it; `remote` is only a local record (no local server). The two Macs must reach each other
(see [cross-mac-networks](cross-mac-networks.md)); `kube apple` does not provision a load balancer and does not test the route. Three
servers on three Macs are needed to survive one Mac failing with embedded etcd. Test failover before relying on it.

## The kubeconfig and the tunnel

```sh
fluxctl kube apple kubeconfig dev --output ./dev.yaml   # re-export; replaces the file atomically, mode 600
fluxctl kube apple tunnel dev [--stop|--status]
```

The tunnel is `ssh -N -L 127.0.0.1:PORT:127.0.0.1:6443` with strict host-key checking, loopback only, `ExitOnForwardFailure`, and keepalives.
Its pid is recorded and only signalled after its command line is confirmed to be this tunnel. The kubeconfig names the cluster, user and
context `apple-NAME`.

## Status and health

```sh
fluxctl kube apple status dev      # JSON; exit status 1 when not healthy
fluxctl kube apple list
fluxctl kube apple logs dev --node dev-a1 --lines 200
```

`status` checks, per node: the VM state, ssh, the k3s unit, readiness and kubelet version; and for the cluster `/readyz`, the tunnel,
agents newer than the server (a skew violation) and a cluster still stuck in `Creating`/`Bootstrapping`. Every problem is listed in
`problems`; `healthy` is true only when there are none.

## Stop, start and persistence

```sh
fluxctl kube apple stop dev        # agents first: stop k3s, sync, stop the VM
fluxctl kube apple start dev --tunnel   # server first; waits for the server Ready, then the agents, then all nodes
fluxctl kube apple persist cycle dev    # mark, stop, start, verify
```

`persist mark` writes a random nonce into a ConfigMap (`kube-system/fluxvm-persist-canary`, i.e. the datastore) and into a file on the
guest's disk, and records the guest's boot id. `persist verify` checks that the boot id **changed** (the guest really restarted; use
`--allow-same-boot` for a service-only restart), and that both copies of the nonce are intact. Use it after a Mac reboot or an upgrade too.
For application data, deploy a workload on a local-path PersistentVolume and compare its data the same way; that is not automated.

## etcd snapshots (`--embedded-etcd` clusters)

```sh
fluxctl kube apple snapshot save dev [LABEL]       # k3s etcd-snapshot save
fluxctl kube apple snapshot ls dev
fluxctl kube apple snapshot delete dev NAME
fluxctl kube apple snapshot prune dev --prefix on-demand --retention 5
fluxctl kube apple snapshot pull dev NAME [--output FILE]   # private file + .sha256 on the Mac (contains the token)
fluxctl kube apple snapshot restore dev NAME --yes          # cluster-reset on the server
```

Names are validated to letters, digits and `- _ .`. A SQLite cluster refuses these commands and says to use `backup`. A pulled snapshot can be given to
`restore` (the datastore restore), which checks its digest first. Restoring with several servers: wipe the others' etcd data and rejoin them (k3s's rule).

## Upgrade

```sh
fluxctl kube apple upgrade dev --binary ./k3s-arm64 --sha256-file ./sha256sum-arm64.txt [--drain] [--no-backup]
```

1. Refuses an unhealthy cluster and a wrong digest (before sending anything).
2. Stages the binary on the server as `k3s.new` (digest re-checked in the guest) and asks it for its version.
3. Refuses a **downgrade** (`--allow-downgrade`) and any jump of more than one **minor** release (Kubernetes skew policy).
4. Takes a datastore backup, unless `--no-backup`.
5. For each node, **servers first**: stage, optional `drain`, keep the old binary as `k3s.prev`, swap, restart, wait until that node is Ready
   **at the new version**, optional `uncordon`.
6. If a node does not come back, **that node is rolled back** to `k3s.prev`, staged files are removed, and the upgrade stops. Nodes already
   upgraded stay (a newer server with older agents is within the skew policy). The error names the nodes upgraded and the backup file.
   A datastore schema change by a newer server may not be reversible: restore the backup if you need to go back fully.

Running it again with the same binary is a no-op.

## Backup and restore of the datastore

```sh
fluxctl kube apple backup dev [--output FILE]
fluxctl kube apple restore dev ~/.fluxvm/apple-k3s/backups/dev/dev-20261121-101500.tar.gz --yes
```

* **SQLite** (default): k3s is stopped for the copy (seconds; a shell `trap` starts it again whatever happens), then `db/` and `token` are archived.
* **Embedded etcd**: `k3s etcd-snapshot save` runs while the cluster keeps working; the snapshot and `token` are archived.
* The archive is written **mode 600, never over an existing file**, with `FILE.sha256` beside it. **It contains the cluster token**: protect it like a key.
* `restore` needs `--yes`, verifies the SHA-256 (the sidecar, or `--sha256`) **on the Mac**, lists the archive **in the guest** to prove it holds what a
  restore needs, and only then stops k3s. SQLite restore keeps the old directory as `db.pre-restore-<time>`. Etcd restore runs
  `k3s server --cluster-reset --cluster-reset-restore-path`; with several servers, wipe the others' etcd data and rejoin them. Agents re-register by themselves.

## Recover after a crash: `recover`

The journal is saved before the daemon is asked for a VM, and the VM's ownership labels (`fluxvm.stack=apple-k3s-NAME`,
`fluxvm.service=ROLE.NODE`, `fluxvm.spec-hash=OWNER`) are written right after. If the Mac or `fluxctl` dies in between, `create`/`join`
resume by themselves for the common cases; `recover` does the explicit repair:

```sh
fluxctl kube apple recover dev [--adopt-unlabelled] [--delete-orphans]
```

| Finding | What happens |
|---|---|
| node in the journal without an id, VM with its name **and our owner label** | the id is adopted |
| same, VM with that name but **no labels** | reported as *ambiguous*; `--adopt-unlabelled` adopts and labels it |
| journal node with an id whose VM lost its labels | labels are written again |
| agent in the journal whose VM was never created | dropped from the journal |
| server in the journal with no VM | reported: run `create` again |
| journal VM id that no longer exists | reported as missing |
| VM with this cluster's labels and our owner, not in the journal | *orphan*: reported; `--delete-orphans` stops and deletes it |
| VM with this cluster's labels and **another owner** | *foreign*: reported, **never touched** |

## A lost journal: `rebuild`

`recover` repairs a journal. If the **journal file itself is gone** (a wiped home folder, a new Mac user), `rebuild` writes a new one from what
can still be known, and says what cannot:

```sh
fluxctl kube apple rebuild dev                       # prints the plan; writes nothing
fluxctl kube apple rebuild dev --accept-host-keys --api-port 16443 [--cpus ... --image ...]
```

It lists the VMs that carry the cluster's ownership labels (`fluxvm.stack=apple-k3s-NAME`), refuses if they name more than one owner, never
adopts a VM without labels or with another owner's, and never creates or deletes a VM. It then starts any stopped guest and reads the rest from
the guests themselves.

| Recovered | From |
|---|---|
| owner id, node names, roles, VM names and ids | the ownership labels |
| datastore kind (etcd or SQLite), `--tls-san` list, shared server flags, server count | the first server's systemd unit |
| which nodes are registered, node versions, k3s version | the cluster (`kubectl get nodes`) and the binary |
| the snapshot schedule (without S3 keys), the kube-vip endpoint and its address | the servers' drop-in and manifest |

| **Not recovered** | Why, and what to do |
|---|---|
| the SSH host keys pinned at creation | they lived only in the journal (and in the daemon's VM request, which the daemon does not expose). `rebuild` **pins what each guest presents now**, trusting it on first use, because the VM is one this daemon owns. That is weaker than the creation-time pin: do it on a network you trust, which is why `--accept-host-keys` is required |
| the published SHA-256 of the k3s binary | only the digest of the binary installed in the guest is recorded; it is not checked against a published one. Run `upgrade` with a verified binary to re-anchor it |
| VM sizing (cpus, memory, disk, image) | labels do not carry it; the flags you pass (or the defaults) are recorded and matter only for nodes added later |
| the persistence canary, rotation stamps, nodes that joined from another Mac | not stored anywhere else |
| the API port, the tunnel LaunchAgent | pass `--api-port`; run `tunnel NAME --install-agent` again |
| VMs without labels, or with another owner's | never adopted; `recover --adopt-unlabelled` is the explicit path for your own |

If a guest cannot be reached, the journal is still written with what is known (phase `Bootstrapping`) and the error says what is missing.

## Token hygiene and rotation

* The server token is created by k3s and stays in `/var/lib/rancher/k3s/server/token`. Agents join with a **separate agent token** that `create`
  generates on the Mac, sends on stdin and the server reads from a root-only file (`--agent-token-file`). An agent therefore cannot make itself a
  server. A joining *server* is the only case that reads the server token (and also gets the agent token, so every server holds the same one).
* No token is in a VM request or cloud-init, an argument, a script, the journal or a message (messages are scrubbed).

```sh
fluxctl kube apple token rotate dev --agent          # the agent token
fluxctl kube apple token rotate dev --server         # the server token (k3s token rotate)
fluxctl kube apple token rotate dev --agent --server
```

New values are made on the Mac, sent on stdin, and recorded nowhere but the guests' root-only files; the journal keeps only a timestamp.
Everything goes **servers first, one at a time**, each restarted and waited for before the next, so an HA cluster stays up (a single server has
a short API outage).

* **Agent token:** every server's `agent-token` file is replaced and the server restarted; then every agent's token file is replaced and the
  agent restarted. Running agents keep working throughout; the token is only used when a node (re-)registers.
* **Server token:** `k3s token rotate` runs on one server with the old and new tokens passed on **stdin into the environment** (`K3S_TOKEN`,
  `K3S_NEW_TOKEN`), never an argument; then every server gets the new value in `/etc/rancher/k3s/token` plus a `token-file:` drop-in and is
  restarted. The report says whether k3s's own `server/token` file shows the new value.
* A run that stops half-way is simply run again. **Backups and snapshots made before a server-token rotation need the old token to restore.**
* Nodes that joined from another Mac keep the old token in their own files; rotate those from there.

## Scheduled snapshots and S3

k3s snapshots embedded etcd every 12 hours by default and keeps five. `snapshot schedule` sets your own policy on every server (a drop-in
under `/etc/rancher/k3s/config.yaml.d/`, servers restarted one at a time):

```sh
fluxctl kube apple snapshot schedule dev --cron "0 */6 * * *" --retention 20 --compress
fluxctl kube apple snapshot schedule dev --cron "0 */6 * * *" --retention 20 \
    --s3-endpoint s3.example:9000 --s3-bucket k3s-snaps --s3-region eu-west-1 --s3-folder dev \
    --s3-credentials-file ~/.config/fluxvm/s3-creds        # mode 600: access_key=… / secret_key=… lines
fluxctl kube apple snapshot schedule dev --show            # each server's non-secret config and the age of its newest snapshot
fluxctl kube apple snapshot schedule dev --reset           # back to k3s's defaults
```

Credentials come from `--s3-credentials-file` (refused unless only you can read it) or `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`; there is
no argument for them. They go to the servers on stdin. Retention is k3s's own (it prunes locally and in S3). Local copies are kept as well, so a
snapshot is on the server's disk **and** in the bucket; `doctor` warns when no S3 target is set and when the newest snapshot is older than 36
hours (fails after 7 days). Whether an upload actually reached the bucket is not something `fluxctl` can see: look in the bucket.

## Removing a node: `remove`

```sh
fluxctl kube apple remove dev dev-a2 --yes           # an agent
fluxctl kube apple remove prod prod-s3 --yes         # a server of an HA cluster
fluxctl kube apple remove prod prod-s3 --yes --force # the node is dead
```

In order: refuse to break etcd quorum (a server leaves only if a majority of the current members is Ready) and never remove the only server; cordon;
drain (`--no-drain` skips it); stop and disable k3s on the node so it cannot re-register; delete the node object; (server) show what the remaining servers logged
about the etcd member and that no etcd node is left listed; delete the VM; drop the node from the journal. k3s's etcd controller removes the **member**
when the node object is deleted; `fluxctl` reports k3s's log line and the node list rather than claiming more. Every step looks at what is already done, so a run
that stopped half-way is finished by running it again. `--force` skips cordon, drain and stop (and tolerates their failure) for a node whose VM is dead.
Removing one of three servers leaves a cluster that no longer survives another failure; the report says so.

## The tunnel that stays up

```sh
fluxctl kube apple tunnel dev --install-agent      # a LaunchAgent: dev.zyvor.fluxvm.k3s-tunnel.dev
fluxctl kube apple tunnel dev --agent-status       # installed / loaded / pid
fluxctl kube apple tunnel dev --remove-agent
fluxctl kube apple tunnel dev --foreground         # what the agent runs
```

The agent runs `fluxctl kube apple tunnel NAME --foreground` at login and restarts it whenever it exits (launchd `KeepAlive`, 10 s throttle). With one
server that is a single ssh tunnel; with several it is the failover forwarder plus an ssh tunnel per server. The guests' addresses are looked up
again on each start, and the forwarder restarts a dead ssh tunnel itself every few seconds; when the number of servers in the journal changes it exits so
launchd starts it with the new set. It is a LaunchAgent, not a daemon: it needs the logged-in session (the daemon's address and your ssh identity). The plist
holds no secret. If the daemon is not at its default address, run `tunnel` yourself or set the daemon address in the environment the agent starts in.

## `doctor`

```sh
fluxctl kube apple doctor dev [--json]
```

Each finding is `ok`, `warn` or `FAIL` with the command that fixes it; the exit status is non-zero when any fails. It checks: servers joined vs wanted, etcd quorum
(degraded vs lost), an even server count, whether every VM runs, that every server's unit has the journal's shared flags, the forwarder or the kube-vip
manifest on every server, the tunnel LaunchAgent, the kubeconfig file, the snapshot schedule, offsite copy and age (etcd clusters), and the age of the
token rotations. It reads the cluster and the guests; it changes nothing. This is separate from `fluxctl kube doctor`, which checks the `kube up` clusters.

## Acceptance on hardware: `scripts/test-kube-apple.sh`

```sh
FLUXCTL=./target/release/fluxctl K3S_BINARY=./k3s-arm64 K3S_SHA256_FILE=./sha256sum-arm64.txt \
    scripts/test-kube-apple.sh --yes [--servers 1|3] [--agent] [--s3]
scripts/test-kube-apple.sh --self-test        # offline, no Mac needed
```

It runs, against a throwaway cluster of its own, create (three servers by default), tunnel, join, `persist cycle`, snapshots and the schedule (S3 with
`--s3`), agent and server token rotation, a failover check (stops a server's VM and expects the API to keep answering), `remove` of an agent and a server, the
tunnel LaunchAgent (`--agent`), `rebuild` after moving the journal away, and `delete`. It exits 0 when every check passed, 1 when any failed, 2 when a
precondition is missing, and lists skipped checks. `--self-test` runs the script against a fake `fluxctl` and `kubectl` to check its own logic (phase order,
failure reporting, cleanup, exit codes, that S3 secrets stay out of arguments); that is all it proves.

## What to verify on a Mac

## Delete

```sh
fluxctl kube apple delete dev --yes
```

Stops the tunnel; deletes agents, then the server; then removes the journal, known-hosts file and kubeconfig. Only VMs the journal recorded
(or that carry the cluster's owner label) are deleted. A node whose VM cannot be deleted stays in the journal and the command exits non-zero;
run it again to retry just that node.

## Security notes

* The private host key of a node is part of its VM request (needed to pin it before first boot), so it is stored by the daemon like the rest of the request.
* The token is in the guests' `/var/lib/rancher/k3s/server` and `/etc/rancher/k3s/token` (root, 0600) and in backups (0600). It is not in the journal, argv, scripts or logs.
* All user-supplied text that reaches a shell (names, SANs, URLs, versions) is validated to plain characters first; digests are validated before use.
* Tunnel and kubeconfig listen/point at 127.0.0.1 only.
* The S3 keys of a snapshot schedule come from a mode-600 file or the environment, travel on stdin and live in a root-only drop-in on each
  server (`/etc/rancher/k3s/config.yaml.d/50-fluxvm-snapshots.yaml`). They are not in the journal, an argument, a script or a message.
* The forwarder's backends are `127.0.0.1` ports of ssh tunnels with the node's host key pinned; the plist of the tunnel LaunchAgent holds no secret.

## What to verify on a Mac

Not exercised against real hardware at the time of writing:

1. `create` with the default image: pinned host key accepted by sshd (drop-in `HostKey`), unit starts, `/readyz`, kubeconfig works through the tunnel.
2. `join` of an agent over the private network (`10.89.x`) including flannel with `--flannel-iface`.
3. `stop`/`start` and `persist cycle`, with a PersistentVolume workload.
4. `upgrade` between two real patch releases, and a forced rollback.
5. SQLite `backup`/`restore` round trip; etcd snapshot and cluster-reset restore.
6. `recover` after killing `fluxctl` between VM creation and labelling.
7. Multi-Mac `join --url` over a real network, and multi-server failover.
8. Behaviour on images other than `debian-13` (package names, `ip`, `modprobe`).

9. `create --servers 3`: the joins one at a time, the shared flags in all three units, a three-member etcd, the forwarder failing over when a server's VM is stopped
   (and failing back), the per-server kubeconfig contexts.
10. `kube-vip`: the image pulls, the address follows the leader, the manifest's mounts and environment are right for k3s (the manifest was written from kube-vip's documented
    control-plane mode and has **never been run**).
11. `token rotate --server`: that this k3s build reads `K3S_TOKEN` and `K3S_NEW_TOKEN` for `k3s token rotate`, whether `server/token` changes, and that a rotated cluster accepts a
    new joining server. `--agent`: that running agents survive and a new agent joins with the new token.
12. `snapshot schedule`: the `config.yaml.d` drop-in is honoured by the installed k3s, the cron fires, S3 uploads reach the bucket and prune there.
13. `remove` of a server: that deleting the node object really removes the etcd member (k3s's controller does this; `fluxctl` only reports the log line and node list), and that
    removal of one of three servers leaves a healthy two-member cluster.
14. The tunnel LaunchAgent across a logout/login and a reboot, and the forwarder's `ssh` children after a guest restarts with a new address.
15. `rebuild` after deleting the journal, including that the re-pinned host keys match and `status` is healthy afterwards.

`scripts/test-kube-apple.sh` automates 1 to 15 where it can (see above). Its `--self-test` only checks the script itself.

Not implemented: automatic LAN load balancing beyond the forwarder and kube-vip, restoring etcd with multiple servers automatically (the other servers are still wiped and
rejoined by hand), HA for clusters that join from another Mac (each Mac's servers are journalled where they run), and recovering the host keys, SHA-256 and sizing of a lost
journal (see `rebuild`).

To put Kairon (Kubernetes-native VM orchestration) on a cluster made this way, and register Macs and Linux hosts as its VM nodes, see [kairon-on-kube.md](kairon-on-kube.md): `fluxctl kube kairon install`.
