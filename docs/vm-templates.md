<!-- Copyright 2026 Zyvor AI Labs · https://zyvor.dev -->
<!-- SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0 -->
# VM environments: a ready virtual machine with one command

`fluxctl templates` also lists VM environments. Each one is a virtual machine that is created, set up and checked by
one command:

    fluxctl templates ls
    fluxctl templates run debian --dry-run     # what it would do; changes nothing
    fluxctl templates run debian               # create or start it, then check it is ready
    fluxctl ssh debian

(`fluxctl up` is unchanged: it brings up a `fluxvm.toml` stack or sets up a new machine, see [up.md](up.md). For a
throwaway VM use `fluxctl run`.)

## The environments

| Name | Image | Size (CPUs, memory, disk) | What is checked |
|---|---|---|---|
| `debian` | Debian 13 | 2, 2 GiB, 20 GiB | git, curl, jq, passwordless sudo |
| `ubuntu` | Ubuntu 24.04 | 2, 2 GiB, 20 GiB | the same |
| `build` | Debian 13 | 4, 4 GiB, 40 GiB | a C program builds and runs; C++, make, CMake, Ninja, a Python venv |
| `amd64` | Debian 13 (ARM64) | 4, 4 GiB, 30 GiB | Rosetta is registered; an x86-64 binary runs; an x86-64 program is built and runs |
| `kvm` | Debian 13 | 4, 4 GiB, 30 GiB | `/dev/kvm` is usable by your login; QEMU is installed |

### Where each one works

| | macOS, Apple silicon (`vz`) | Linux x86-64 with KVM (`qemu`) |
|---|---|---|
| `debian`, `build` | yes | yes |
| `ubuntu` | yes, but see the known problem below | yes |
| `amd64` | yes: needs Rosetta for Linux (`fluxvm-vz-runner install-rosetta`) | **no**: Rosetta is an Apple technology. Guests there are already x86-64, so use `build` |
| `kvm` | Apple M3 or later, macOS 15 or later | the host's `kvm_intel` or `kvm_amd` module needs `nested` on |
| `--network` (private VM-to-VM network) | yes | **no**: only the `vz` backend has it |
| `--share` | yes | needs `virtiofsd` |
| `--snapshot`, `--reset` | saved memory and disk; restoring needs an unlocked login session ([macos.md](macos.md#snapshots)) | QEMU internal snapshot |

Other hosts (ARM64 Linux, Intel Macs, other systems) are refused with the reason: the QEMU backend builds an x86 `q35`
machine, and `vz` needs Apple silicon.

**Known problem:** with FluxVM 0.4.x on a Mac, the setting `PerSourcePenalties no` that FluxVM writes into the guest's
sshd configuration stops sshd on Ubuntu 24.04, so `ubuntu` does not become ready there until that is fixed.

macOS guests are not an environment (they need an installed image of your own): use `fluxctl mac run`
([macos-containers.md](macos-containers.md)). A Docker engine is `fluxctl docker up` and Kubernetes is
`fluxctl kube up`; they are not repeated here.

## Running it again is safe

`templates run NAME` first looks, then does the one thing that is needed:

| What it finds | What it does |
|---|---|
| no VM of that name | creates it |
| stopped (or failed) | starts it |
| paused | resumes it |
| running | nothing; only the readiness checks |
| running, but your options differ from how it was made | reports the difference and keeps the VM; `--recreate` rebuilds it |
| a VM of that name that this template did not make | stops with an error and does not touch it (`--vm OTHER` picks another name) |

Nothing is deleted or rewound unless you pass `--recreate`, `--reset` or `down --delete`, and those ask first
(`--yes` skips the question; without a terminal they need it).

How a VM is recognised: the create request writes `/etc/fluxvm-template` (the template name and a hash of the
definition) into the guest, and that request is stored with the VM. The labels `fluxvm.template` and
`fluxvm.template-hash` are set too, so `fluxctl list` shows them.

## Ready means checked

After the VM is up, the command waits for SSH, then for cloud-init to finish the first boot, then runs the template's
checks in the guest. Each check is reported. The exit status is 0 only when all of them pass. A VM that is not ready is
left running so you can look at it; run the same command again to keep waiting and check again.
`--timeout SECONDS` sets how long the first-boot setup may take (1200 on macOS, 1800 on Linux).
On a Mac the guest reports its address only after the first-boot packages are installed, so a first run can print
`still waiting` for a few minutes before SSH is tried.

Mounted `--share` folders and configured `--network` addresses are checked too.

## Plan before you run

`--dry-run` prints the VM that would be created (image, CPUs, memory, disk, ports, shares, packages, first-boot
commands), what "ready" means, the action, and a preflight of the host: backend support, Rosetta or nested
virtualization, free disk and memory, busy ports, whether the image is cached, KVM and required programs on Linux, and on
macOS the same request validation a create runs. A `[blocked]` line names its fix and stops a real run before anything
changes. A dry run creates no VM and no SSH key.

## Options

    --vm NAME            the VM's name (default: the template's)
    --cpus N  --memory-mib N  --disk-gib N
    -p [ADDR:]HOST:GUEST     forward a TCP port (on 127.0.0.1 unless ADDR is given)
    --share HOST:GUEST[:ro]  share a folder
    --network NAME       macOS: VMs on the same private network reach each other directly
    --snapshot           once ready, save the snapshot `ready`
    --reset              go back to `ready` (loses what was written since)
    --recreate           delete the VM and its disk and create it again
    --dry-run  --json  --yes  --timeout SECONDS

    fluxctl templates down NAME            # stop; the disk is kept
    fluxctl templates down NAME --delete   # delete the VM and its disk

`--json` prints one document (`"schema": 1`): `template`, `vm`, `dry_run`, `changed`, `applied`, `plan`, `ready`,
`readiness`, `address`, `error`.

## Your own environment

    fluxctl templates show debian > mine.toml
    fluxctl templates run ./mine.toml --dry-run

The file has `name`, `title`, `summary`, `image` (a built-in image name, a catalog name or a path), `cpus`,
`memory_mib`, `disk_gib`, `packages`, `run` (first-boot commands, run as root; `{user}` is the login), `rosetta`,
`nested_virtualization`, `[[check]]` entries (`name`, `command`), and `[arch.x86_64]` / `[arch.aarch64]` sections for
packages and checks that differ by CPU. Unknown keys are refused. A template file cannot share host folders or forward
ports: only the command line can. Its commands run in the guest, never on your machine.

## Images on Linux

Built-in image names now resolve on x86-64 and ARM64 Linux as they already did on macOS: downloaded from the vendor over
HTTPS, checked against the vendor's checksum file and cached in `<state_dir>/images`. On x86-64 the built-in names are
`debian-13` and `ubuntu-24.04` (amd64 images). A file or catalog entry of the same name still wins.

## With a daemon

Without `--server` the command uses the local state directory, like `fluxctl up`. On a Linux system install, where the
daemon owns the state, use `fluxctl --server http://127.0.0.1:7788 templates run debian`. Host checks are then left to
the daemon. The server must be on the same machine (readiness is checked over SSH to an address only that host reaches),
and run the same version of FluxVM: an older Linux daemon does not know the built-in image names.

## Not covered

Studio's Templates page and `GET /v1/stack-templates` list only the Compose and k3s templates.
