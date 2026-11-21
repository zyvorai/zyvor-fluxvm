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
| Day-2 | checkpoints, dev loop, apps | `upgrade` with rollback, datastore `backup`/`restore`, `persist` checks, `status` that fails when unhealthy |

Use `kube up` for a quick developer cluster with time travel; use `kube apple` when the cluster should be long-lived, pinned to a k3s
build you vetted, and spread over several VMs or Macs.

> **Verification status.** The lifecycle logic is unit-tested against a fake host (order of operations, cleanup, redaction). The shell
> scripts that run inside the guests, the Apple Virtualization networking, `etcd-snapshot`/`cluster-reset`, and the real `ssh` tunnel
> were **not run against real hardware**. Treat the first cluster as an acceptance test: see [What to verify on a Mac](#what-to-verify-on-a-mac).

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

A lost journal cannot be rebuilt from labels, because the pinned host keys live in it; delete the guests (`fluxctl list`, `fluxctl rm`) and create again.

## Token hygiene

* The server token is created by k3s and stays in `/var/lib/rancher/k3s/server/token`. Agents join with a **separate agent token** that `create`
  generates on the Mac, sends on stdin and the server reads from a root-only file (`--agent-token-file`). An agent therefore cannot make itself a
  server. A joining *server* is the only case that reads the server token.
* No token is in a VM request or cloud-init, an argument, a script, the journal or a message (messages are scrubbed).
* Rotation of the tokens is not implemented.

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

Not implemented: HA (`join --role server` exists, load-balanced endpoint does not), token rotation, scheduled-snapshot tuning, automatic LAN load balancing, a published token rotation, restoring etcd with multiple servers automatically, a LaunchAgent that
keeps the tunnel open (use `kube agent`-style supervision yourself, or re-run `tunnel`).
