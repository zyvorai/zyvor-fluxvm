# Lima parity: `limactl` and `fluxctl`

What a Lima user misses in FluxVM, plus the two commands that close part of it: [`fluxctl port-forward`](#port-forward) and
[`fluxctl ssh --cwd`](#a-shell-in-the-current-folder).

How this was checked (2026-11-27): Lima behaviour comes from [lima-vm.io](https://lima-vm.io/docs/) and from reading its source
at commit `9a3022aa` (2026-11-26). `limactl` was **not installed or run**, so nothing here is a side-by-side test. FluxVM
behaviour comes from reading and searching this repository. "Not found" means a search found nothing, which is weaker than
seeing the code refuse. "Unverified" means it was not checked. The two new commands were run against a Debian 13 guest on
`vz` (Apple silicon Mac) and on QEMU with user-mode networking (x86_64 Linux).

## Virtualization.framework is not a difference

Lima's `vz` driver uses the same Apple framework as FluxVM's, and is its default on macOS 13.5 or newer since Lima 1.0
([VM types](https://lima-vm.io/docs/config/vmtype/); see also [macos-containers-research.md](macos-containers-research.md)).
Both offer virtiofs shares, Rosetta, nested virtualization and macOS guests (experimental in Lima).

What FluxVM's runner does that Lima's `vz` driver at that commit does not:

| FluxVM | In Lima's source |
|---|---|
| Saves and restores a running guest's memory (`snapshot`, `restore`; macOS 14+, restore needs an unlocked session) | the `vz` snapshot methods return "unimplemented"; no call saving machine state found |
| `pause` and `resume` | no pause call found in the `vz` driver or `limactl` |
| Changes the memory balloon of a running guest (`/v1/vms/{id}/balloon`) | a balloon device is attached; no call setting its target found |
| NVMe and NBD disks, an XHCI controller with USB disk hot-attach | no NVMe, NBD or XHCI configuration found (USB mass storage is used for an ISO) |

Whether Lima has anything like FluxVM's host GPU relay, guest self-control, forks and branches, agent screen input or private
VM networks was **not checked**.

## Gaps

| `limactl` behaviour | `fluxctl` | State |
|---|---|---|
| `shell` starts in the current host folder when it is mounted, else in the guest home | `ssh --cwd`: opt-in, and an error (not a silent fallback) when the folder is not shared | **Closed here**, not the default |
| Guest ports listening on localhost are forwarded to the host automatically | nothing automatic found. Static `network.forwards` at create (`run -p`); `port-forward` for a running VM | **Partly closed here** (manual) |
| UDP port forwards (gRPC forwarder) | `port-forward` is TCP only. UDP only as set at create (`apple.usernet`, `apple.vmnet`); the `vz` NAT forwards refuse UDP | Open |
| Guest Unix socket forwards (`portForwards.guestSocket`) | only the Docker socket (`docker up`) | Open |
| The home folder is mounted read-only by default | nothing is shared unless asked (`run -v`, `shared_folders`), so `--cwd` needs a share | Open |
| `edit`: change mounts, CPUs, memory of an existing instance | `PATCH /v1/vms/{id}` changes name and labels only; `edit` only explains that; share hotplug is QEMU and Cloud Hypervisor only (and is not mounted in the guest for you); `mac update` resizes `fluxctl mac` instances only | Open |
| `copy` files and folders (`-r`) between host and instance by name | `copy-to` / `copy-from`: one file up to 64 MiB through the guest agent; `mac cp` takes `fluxctl mac` instances only | Open |
| An ssh config per instance (`ssh -F`, `show-ssh`) | only `agent ssh-config`, for agent sessions | Open |
| `shell` exits with the remote command's status | `ssh` exits 1 for any non-zero status (the message names it) | Open |
| `lima CMD` and a default instance (`LIMA_INSTANCE`) | not found | Open |
| `shell --workdir`, `--shell`, `--preserve-env`, `--sync` | not found on `ssh` | Open |
| `shell --start` starts a stopped instance | `ssh` does not start it | Open |
| `factory-reset` | not found | Open |
| Templates: one YAML with image, mounts, provisioning, port forwards and probes (`start template:docker`) | built-in image names, `vm-template` (saved VM specs), `templates` (Compose and k3s stacks), `docker up`, `kube up`; no single template of that kind | Open |
| `host.lima.internal` names the host inside the guest | no such name found | Open |

Present in both by name, behaviour not compared: create, start, stop, restart, delete, list, clone, rename, protect and
unprotect, prune, snapshot, disks, networks, start at login (`enable` and `service install`), validation (`vm-spec check`),
`doctor` (`info`), `events --follow` (`watch`), screenshot, MCP, shell completion.

## Port forward

```bash
fluxctl port-forward web 8080:80 5432    # 127.0.0.1:8080 -> guest 80, 127.0.0.1:5432 -> guest 5432
fluxctl port-forward --list              # all forwards on this machine; `--list web` for one VM
fluxctl port-forward --stop web 8080     # one; `--stop web` stops all of web's
```

- One detached `ssh -N -L` per host port, the forwarder `fluxctl docker` and `fluxctl kube` use. It listens on `127.0.0.1`
  only and connects to the guest's own `127.0.0.1`, so a service bound only to the guest's network address or only to `::1` is
  not reached.
- Needs a running VM, `sshd` in it, and a key that logs in without a prompt. The login user is `-l`, else the cloud-init user,
  else `root`.
- Host keys are treated as `fluxctl ssh` treats them: not checked for `vz` guests and loopback-forwarded QEMU guests, checked
  with your ssh settings otherwise (log in once with `fluxctl ssh` to accept a new key).
- Records are `~/.fluxvm/forwards/HOST.pid`. Nothing restarts a forward: it ends when its connection does (the VM stops, the
  host reboots) and is then listed with `running: false`. Asking again replaces it. Deleting a VM does not remove its record;
  `--stop` with its old name does.
- Linux: meant for any backend this host can SSH to (run on QEMU user-mode only). A QEMU user-mode VM needs a `network.forwards` entry for guest port 22 on
  `127.0.0.1`, set when it is created. A VM with `network.mode: none` cannot be forwarded to.
- With `--server`, the forward still runs on your machine, so the guest address must be reachable from it.

## A shell in the current folder

```bash
cd ~/code/app                  # inside a folder shared into the VM
fluxctl ssh web --cwd          # a login shell in the same folder, at its guest path
fluxctl ssh web --cwd make     # or one command there
```

- The current folder must be inside one of the VM's `shared_folders`; the deepest matching share wins and symlinks are
  resolved. Otherwise `ssh` exits with the list of what the VM shares. It never starts somewhere else instead.
- It also stops, before running anything, when the guest has not mounted the share (guests with the `mountpoint` command).
- What follows the VM is the command; ssh options cannot be combined with `--cwd`.
- Backends: `vz`, QEMU (needs `virtiofsd` on the host) and Cloud Hypervisor, which share the folder live; Cloud Hypervisor
  has **not been tested**. Firecracker is refused: its shares are copies made at start. Linux guests mount shares through
  cloud-init; a macOS guest shows them under `/Volumes/My Shared Files`, and `--cwd` there has **not been tested**.
- With `--server`, only a daemon on this machine (a loopback address) is accepted.
