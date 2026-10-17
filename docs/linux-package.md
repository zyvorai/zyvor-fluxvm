# The Linux package

The release builds one package per architecture, `fluxvm-<version>-linux-<x86_64|aarch64>.tar.gz` with a `.sha256`, from
the ubuntu:22.04 build container (`.github/workflows/release-linux.yml`). Nothing is compiled on a node: the same files
are installed everywhere, by `scripts/install.sh` today and by the `.deb`/`.rpm` and the pull installer as they land (see
the 10,000-node install plan).

## What is in it

`packaging/files.txt` lists every file, one per line, and `scripts/package-linux.sh` builds exactly that. It fails when a
required file is missing, so the package cannot silently lose a binary. The core group:

| Path in the package | What |
|---|---|
| `bin/fluxctl` (+ `bin/fluxvm` link) | CLI and the daemon (`fluxctl serve`) |
| `bin/fluxvm-hypervisor` | the in-tree FluxVM hypervisor |
| `bin/fluxvm-agent` | fleet central and the per-node heartbeat client |
| `bin/fluxvm-kube`, `bin/kubectl-fluxvm` | the Kubernetes operator and its kubectl plugin |
| `bin/fluxvm-microvm` | the Kubernetes MicroVM controller and node agent |
| `bin/fluxvm-guard` | VMM Guard CLI |
| `lib/bpf/*.bpf.o` | the BPF objects the daemon loads, built for the package's architecture (installed to `/usr/lib/fluxvm/bpf/`) |
| `share/guest/fluxvm-guest-agent` | the static (musl) guest agent, injected into guest images of any distro |
| `share/config.example.toml`, `share/systemd/fluxvm.service` | config template and the one hardened unit |
| `share/completions/` | bash, zsh and fish completions (native builds only) |
| `MANIFEST.sha256` | a sha256 of every other file, so what was unpacked can be checked |

Not packaged yet (group `extras` in the manifest): the Sentinel / runtime-intelligence daemons and their C loaders
(they need libbpf at run time), the fluxmesh agents, the containerd shim and container agent, `fluxvm-procbox`. They need a
decision on a separate package and on the runtime libraries it declares.

The layout keeps `bin/`, `share/config.example.toml` and `share/systemd/fluxvm.service` where `scripts/install.sh`
expects them; everything else is added next to them.

## The .deb

The release also builds `fluxvm_<version>_<amd64|arm64>.deb` from the same staged tree (`scripts/package-linux-pkgs.sh`,
`packaging/nfpm.yaml`). Layout: `/usr/bin`, `/usr/lib/fluxvm/bpf`, `/usr/share/fluxvm` (guest agent, config template),
`/usr/lib/systemd/system/fluxvm.service` (ExecStart `/usr/bin/fluxctl`), `modules-load.d`/`modprobe.d` for `nbd max_part=16`,
shell completions in the distribution directories.

The maintainer scripts (`packaging/deb/`) follow these rules:

- `/etc/fluxvm.toml` is seeded from the template only when absent and is not a conffile, so an upgrade never touches it.
- The service is enabled on install but not started (the seeded config is a template). On upgrade the running daemon is
  restarted only when `/etc/fluxvm/upgrade.conf` says `RESTART_ON_UPGRADE=yes`; the default is `no`. The unit uses
  `KillMode=process`, so a restart leaves running VMs alone.
- `/etc/qemu/bridge.conf` (`allow all`) is created if missing; the swtpm AppArmor override is added only when AppArmor is enforcing.
- `/var/lib/fluxvm` is never deleted, not on remove and not on purge.

`scripts/test-deb.sh` installs, upgrades (with an edited config), removes and purges it in an ubuntu:22.04 container; CI runs it.
`.rpm` follows in a later milestone. Not done yet in the .deb: bundled Firecracker / Cloud Hypervisor (needs `packaging/vendor.lock`).

## Building and checking

```bash
scripts/package-linux.sh --check-manifest       # validate packaging/files.txt against the repo (any OS, no build)
scripts/package-linux.sh --version 0.4.12       # build, stage and tar (Linux; see the script header for requirements)
scripts/package-linux.sh --stage-only           # stop after staging: target/package/fluxvm-<ver>-linux-<arch>/
scripts/check-glibc-floor.sh target/package/fluxvm-0.4.12-linux-x86_64 2.35   # the compatibility gate
```

To add a file to the package, add a line to `packaging/files.txt`; `scripts/test-package-manifest.sh` and CI check the file.
A cross build (`--target aarch64-unknown-linux-gnu`) builds the BPF objects for arm64 with `FLUXVM_BPF_ARCH` and leaves
the completions out, because it cannot run the binaries it makes.
