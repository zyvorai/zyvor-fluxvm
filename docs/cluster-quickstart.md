# Cluster quickstart: bare machines to a joined fleet in three commands

FluxVM's own fleet, set up the way you set up k3s: one command on the first machine, one command on each of the others.
Kubernetes is not involved. [k3s](#optional-add-ons) is an add-on you can switch on later, never a dependency.

> **Status.** The installer logic, the Rust commands and the remote scripts are tested against fakes (fake package manager,
> fake `sudo`, fake central, fake SSH transport; plus the real central in-process for the token and node API). **Not yet run
> end to end on a real Linux or Mac pair**: expect a rough edge, and use `--dry-run` first. See
> [What is verified, and what is not](#what-is-verified-and-what-is-not).

## Three commands

```bash
# 1. On the first machine (Linux with systemd or OpenRC; macOS: fluxctl cluster init):
curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | bash -s -- --server

# 2. It ends by printing exactly this, with your URL and a one-hour token. On every other machine, Linux or Mac:
curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | FLUXVM_TOKEN=fvjt_... bash -s -- --join http://SERVER:7799

# 3. Look at the cluster, from the server:
fluxctl cluster nodes
```

Prefer to do everything from one place? Skip step 2 and bring Linux hosts in over SSH from the server:

```bash
fluxctl cluster add admin@node1 admin@node2 --join http://SERVER:7799
```

## What `--server` does

1. Prints the **exact list of packages** it is about to install and asks once (`--yes` skips the question, `--no-deps` skips the
   packages). Debian/Ubuntu: `qemu-system-x86` (arm64: `qemu-system-arm qemu-efi-aarch64`), `qemu-utils`, `cloud-image-utils`
   (`cloud-localds`). Fedora/RHEL: `qemu-kvm qemu-img cloud-utils`. Only what is missing is installed.
2. **Additive only.** It first asks the package manager to *simulate* the install (`apt-get install -s`, `dnf --assumeno`,
   `zypper --dry-run`) and refuses to continue if the simulation would remove or upgrade anything. Then one `sudo` prompt (none when
   `sudo -n` works); the rest reuses it. If there is no package manager it says which programs to install by hand.
3. Adds you to the `kvm` group when `/dev/kvm` exists, and tells you a new login is needed. Until then the self-test says so
   instead of failing.
4. Installs FluxVM (SHA-256 verified, the same as the plain installer) and starts the daemon and the **fleet central**
   (`fluxvm-agent central`) as services. Central makes its operator token itself on first start
   (`/var/lib/fluxvm-central/operator-token`, root only).
5. Writes a per-user config (`~/.config/fluxvm/config.toml`) with its own `state_dir` and `run_dir`, because `/run/fluxvm` is
   root-owned. Use it as a normal user with `export FLUXVM_CONFIG=~/.config/fluxvm/config.toml`.
6. Downloads the **Debian 13 cloud image** for this CPU into `~/.cache/fluxvm/images` (root: `/var/lib/fluxvm/images`), verified
   against Debian's `SHA512SUMS`, resumable: run the installer again after an interrupted download. A wrong checksum deletes the file.
7. Runs the **self-test**: boots a 512 MiB VM, runs `echo` in it, deletes it, prints `SELF-TEST PASS` or `SELF-TEST FAIL` with the
   exact fix (for example the `usermod -aG kvm` line). A failed self-test does not undo the install. Re-run it any time:
   `fluxctl cluster selftest`.
8. Prints **Ready**, the dashboard URL and the join command:

```
==> FluxVM server is Ready.
    Central:    http://10.1.2.3:7799
    Dashboard:  http://127.0.0.1:7799/console/   (FluxVM Studio; paste the operator token)
                from a laptop: ssh -L 7799:127.0.0.1:7799 USER@10.1.2.3, then open http://127.0.0.1:7799/console/
    Add a machine (Linux or macOS), one command, token valid for 60 minutes:
      curl -sfL https://zyvorai.github.io/zyvor-fluxvm/install.sh | FLUXVM_TOKEN=fvjt_... bash -s -- --join http://10.1.2.3:7799
```

Central listens on every interface so that nodes can reach it; **it speaks plain HTTP unless you give it `--tls-cert/--tls-key`**.
On anything but a trusted network put TLS in front, or tunnel with SSH. High availability: run a second central with `--ha-redis`
(see [fleet-ha.md](fleet-ha.md)); the installer sets up one central.

## What `--join URL` does

Everything above that applies to a node (prerequisites, install, base image, self-test), plus:

- **The token comes from a file (`--token-file`), `$FLUXVM_TOKEN` or stdin (`--token-stdin`), never an argument.** There is no
  `--token` option on purpose; passing one is refused with the reason. (`--token-stdin` needs the script saved to a file first: when you pipe the script itself into `bash`, stdin is the script, so use `$FLUXVM_TOKEN` or `--token-file`.) A value that is not a join token (`fvjt_...`) is refused,
  so an operator or fleet token cannot be used by mistake.
- It enrolls through the fleet's existing per-node enrollment ([fleet-enroll.md](fleet-enroll.md)): the node trades the join token
  for **its own credential**, which can only register that node. The agent runs as a service (`fluxvm-agent.service` or the OpenRC
  script) with the token passed by file path (`LoadCredential=`), not in argv or the environment.
- `--node-name NAME` (default: the host name), `--label key=value` (repeatable), `--advertise-url URL` (default
  `http://<this machine's address>:7788`).
- It **waits until central accepts the node**: the node's own credential is checked against central
  (`GET /fleet/nodes/{name}/overlays`) and the line `NAME is Ready` is printed. Not ready after two minutes: exit 1 with the log to
  read and the likely cause (an expired token: make a new one).
- **Idempotent.** Run it again with the same URL and name and it needs no token: it says `Already enrolled`, applies changed
  labels or advertise URL and restarts the agent. `--uninstall` stops and removes the services and their settings and keeps VM
  images and state (`/var/lib/fluxvm*`).
- **macOS** (Apple silicon): the same command hands over to `fluxctl cluster join` (install FluxVM for Mac first). It installs the
  daemon and the node agent as LaunchAgents with no sudo and deletes the join token file once the node is enrolled.
- Signed channel instead of GitHub: `--mirror URL --pubkey release.pub` runs `fluxvm-bootstrap install`, which verifies the
  Ed25519 chain (channel file, release manifest, package) of [fleet-install.md](fleet-install.md).

> A joined node registers, heartbeats and shows up Ready. For central to **create VMs on it**, central must reach the node's daemon:
> the daemon listens on `127.0.0.1:7788` by default, so set `listen` to a reachable address with API auth in `/etc/fluxvm.toml`
> (a non-loopback `listen` without tokens refuses to start) and pass that address as `--advertise-url`. The installer does not
> do this for you yet.

## Where the token is printed (the print policy)

- The join command, with the token in it, is printed **once, to a terminal**. When stdout is a pipe or a log, the command refers to
  the token file instead (`FLUXVM_TOKEN="$(cat ~/.config/fluxvm/join.token)"`) and the token is not printed. `FLUXVM_PRINT_TOKEN=1`
  forces printing.
- The token is also written to `~/.config/fluxvm/join.token`, mode 0600, for use with `--token-file`.
- It is short-lived (one hour; `--token-ttl SECS`) and can be revoked: `fluxctl cluster token revoke ID`.
- It is never an argument of any process. The server passes the operator token to `curl` through a header file, and the node
  reads the join token from a 0600 file in a private temp directory removed on exit. Shell history still records what *you* paste
  after `FLUXVM_TOKEN=`; that is why it expires.
- The operator token is never printed. The node's credential never leaves its state directory.

## `fluxctl cluster`

| Command | What it does |
|---|---|
| `cluster init` | `install.sh --server` (Linux, via the package's own `share/install.sh`) or native LaunchAgents (macOS) |
| `cluster join URL` | `install.sh --join` on Linux, native on macOS; token from `--token-file`, `$FLUXVM_TOKEN` or `--token-stdin` |
| `cluster nodes` | name, Ready/NotReady/Cordoned, VMs, vCPUs, memory, how it authenticated, last heartbeat, labels; `--json` |
| `cluster token create [--ttl --max-uses --name-pattern --write-to FILE]` | a join token, shown once (to a terminal; otherwise use `--write-to` or `--json`) |
| `cluster token list` / `revoke ID` | ids, limits, uses and expiry (never secrets) / no new nodes with it |
| `cluster remove NODE` | revoke the node's credential, then deregister it (a healthy node is refused until it has stopped; re-run) |
| `cluster add user@host ... --join URL` | bring Linux hosts in over SSH (below) |
| `cluster addon k3s [--ssh user@host]` | the optional k3s layer |
| `cluster selftest` | boot a VM, run a command, delete it: PASS or FAIL with the fix |

The operator token for `nodes`, `token` and `remove` comes from `--operator-token-file`, `$FLUXVM_AGENT_TOKEN`, or, run as a user
that can read it, central's own file (`/var/lib/fluxvm-central/operator-token`).

### What it reuses, and what is new

Reused, unchanged: central's enrollment API (`POST|GET /fleet/enroll/tokens`, `DELETE /fleet/enroll/tokens/{id}`, `POST
/fleet/enroll`, `GET /fleet/enroll/nodes`, `POST /fleet/nodes/{name}/revoke`), `GET /fleet/nodes`, `DELETE /fleet/nodes/{name}`,
`fleet_client`, the node agent (`fluxvm-agent node`), `fluxctl node-agent install` on macOS, `fluxvm-bootstrap`, the mac-fleet SSH
transport, host-key handling, result tables and `Secret` scrubbing. New: `fluxvm-central` unit files (systemd and OpenRC), the
installer's `--server`/`--join` modes, `fluxctl cluster` and its Linux remote scripts. **No new server API.**

## Bring Linux hosts in over SSH

`fluxctl cluster add admin@node1 admin@node2:2222 --join http://SERVER:7799` works like `fluxctl mac-fleet up`:

- the system `ssh` with `BatchMode=yes` (never a prompt) and `StrictHostKeyChecking=yes`: the host key must already be in
  `known_hosts`, or you pass `--accept-new-hostkey` and accept it on first use; failures say how to verify and add the key;
- each host is probed read-only first (Linux, CPU, sudo without a password, systemd/OpenRC, `/dev/kvm`, curl), and the exact fix
  is printed for each problem before anything is installed;
- a join token is minted for the run (15 minutes, as many uses as hosts), travels on SSH stdin only, lands in a 0600 file in a
  private temp directory on the host and is revoked when the run ends;
- each host fetches `install.sh` (pin it with `--installer-sha256`, or point `--installer-url` at your mirror) and runs `--join`;
- then the server waits (`--wait`, default 120 s) for each node to be healthy in central, and prints one line per host;
- one unreachable host does not stop the others; re-running is safe (a node that is already in is left alone and needs no token);
- `--dry-run` prints every `ssh` command and sends nothing. The SSH user needs root or passwordless sudo, because ssh has no
  terminal for a password prompt.

## Optional add-ons

`--with-k3s` on `--server`/`--join`, `fluxctl cluster addon k3s`, or `cluster add ... --with-k3s` run `fluxctl kube up` on the node
after it has joined. A failing add-on never undoes the cluster. Note: on Linux `fluxctl docker up` and `kube up` are being fixed to
give the VM forwarded ports (the `feat/kube-unified` branch); until that lands the add-on may hang, and the installer only calls it.

## Pre-baked engine image

First boot of a Docker VM installs Docker with apt. `scripts/build-engine-image.sh --out DIR` builds an image with Docker installed
and cloud-init finished; publish it with its `.sha256` (and `.sig`) and give the URL to the installer:
`--engine-image-url https://mirror.example/fluxvm-engine-amd64.qcow2`. The installer refuses it unless the SHA-256 matches and, when
`FLUXVM_RELEASE_PUBKEY` is set, the Ed25519 signature verifies. **Nothing is published yet**: without a URL the installer says so and
Docker VMs fall back to the apt first boot. The build script has a `--dry-run` and has not been run end to end.

## What is verified, and what is not

Verified (offline, with fakes): `scripts/test-install.sh` (package selection, additive-only refusal, one sudo, `--yes`, `--no-deps`,
the server and join flows under systemd and OpenRC with units in a temp root, token sources and refusals, token never in curl or
sudo arguments or output, idempotent re-run, not-Ready exit, self-test PASS/FAIL/SKIPPED, image checksum refusal and cache, engine
image digest, the signed-channel path with a stub `fluxvm-bootstrap`, uninstall, the macOS hand-over); `cargo test -p fluxctl`
(token resolution and refusals, the table renderers, `0600` files, the install arguments, the whole token and node API against a real
in-process central, `cluster add` against a fake SSH transport with golden remote scripts, `doctor` making `cloud-localds` required).

Not verified: a real install on a bare machine, real `apt`/`dnf`, a real `sudo` prompt, a real systemd or OpenRC, a real booted VM
in the self-test, the real Debian download, two real machines joining, a Mac as a server or as a node, `cluster add` over real SSH,
the engine-image build, and a `--server` central behind TLS. Fedora, Arch and openSUSE package names are best effort (cloud-localds
is not in Arch's repositories; the installer says so).
