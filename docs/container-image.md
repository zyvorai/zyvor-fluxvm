# Container image

`ghcr.io/zyvorai/fluxvm` is one multi-arch (amd64, arm64) image that carries every FluxVM binary. Which one runs is chosen
by the container's `command:` (`fluxvm serve`, `fluxvm-kube`, `fluxvm-microvm controller|node-agent`). The Kubernetes
manifests that use it are in [deploy/k8s](../deploy/k8s/README.md).

## How it is built

The image is **assembled, not compiled**. [`Dockerfile`](../Dockerfile) copies the staged tree that the Linux release
already built (`bin/`, `lib/bpf/`, `share/` of `fluxvm-<ver>-linux-<arch>.tar.gz`, see [linux-package.md](linux-package.md)) and
installs the runtime packages on top. Two consequences:

- **No glibc mismatch.** The release binaries are linked on `ubuntu:22.04` (glibc 2.35, the floor enforced by
  `scripts/check-glibc-floor.sh`). The earlier Dockerfile rebuilt them on Ubuntu 24.04, producing binaries the floor gate had
  never seen. The runtime base is now `ubuntu:24.04` (glibc 2.39), which is never older than the builder.
- **The image runs the released bytes.** What was tested and signed in the tarball is what is in the image.

Runtime packages mirror `packaging/nfpm.yaml`: `libhivex0`, `libsystemd0`, `qemu-system-x86` (amd64) or `qemu-system-arm`
(arm64), `qemu-utils`, `cloud-image-utils`, `iproute2`, `dnsmasq-base`, `nftables`, `bpftool` (linked from `linux-tools-generic`; Ubuntu 24.04 has no `bpftool` package), `util-linux`. There is no
`python3`, `curl`, or build toolchain, and nothing is downloaded at build time except Ubuntu packages.

The base image is pinned by index digest (`ubuntu:24.04@sha256:68ee2200...`; both amd64 and arm64 are in that index). To
refresh it: `crane digest docker.io/library/ubuntu:24.04`, edit the `FROM` line, and rebuild.

### Cloud Hypervisor and Firecracker

The old image fetched "latest" of both from GitHub at build time. That is removed. They are **not in the image yet**: they
will be added from pinned, checksummed binaries once `packaging/vendor.lock` exists (see the `TODO(vendor.lock)` markers in
the Dockerfile). Until then the QEMU backend is the supported one in the container.

### Building by hand

```sh
mkdir -p stage && tar -xzf fluxvm-0.4.8-linux-x86_64.tar.gz -C stage
docker buildx build --platform linux/amd64 \
  --build-context stage=stage/fluxvm-0.4.8-linux-x86_64 \
  --build-arg VERSION=0.4.8 -t fluxvm:0.4.8 .
```

Use the tarball that matches `--platform`; the architectures are not interchangeable.

## How it is published

[`publish-image.yml`](../.github/workflows/publish-image.yml), on a `v*` tag or by hand (`workflow_dispatch`, input
`version`; a moving tag such as `latest` is rejected):

1. **plan** validates the version and computes the tags: `<ver>` always, `<major>.<minor>` for final releases only. No
   manifest is ever tagged `latest`.
2. **build** (per architecture, on a runner of that architecture): downloads `fluxvm-<ver>-linux-<arch>.tar.gz` from the
   `v<ver>` release, checks its `.sha256`, builds the image with `provenance: mode=max` and `sbom: true`, and pushes it by
   digest only. On a tag push it waits up to 60 minutes for `release-linux.yml` to upload the tarball. The arm64 leg is
   `continue-on-error` because the arm64 package is still experimental.
3. **merge** joins the pushed digests with `docker buildx imagetools create`, then:
   - signs the manifest list keyless (`cosign sign`, GitHub OIDC, recorded in the Rekor log);
   - attaches a `cosign attest` statement (custom predicate: version, Dockerfile commit, workflow run, SHA-256 of each
     release tarball);
   - **verify gate**: `cosign verify` and `cosign verify-attestation` against this workflow's identity, plus checks that
     provenance and an SBOM are present and that each tag resolves to the signed digest. The job fails, and no manifest is
     attached to the release, if any of it fails;
   - renders `fluxvm-k8s-<ver>.yaml` (below) and attaches it, with its `.sha256`, to the `v<ver>` release.

### Verifying an image yourself

```sh
cosign verify ghcr.io/zyvorai/fluxvm@sha256:<digest> \
  --certificate-identity-regexp '^https://github.com/zyvorai/zyvor-fluxvm/\.github/workflows/publish-image\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

## Kubernetes

The manifests under `deploy/k8s` name their image as
`ghcr.io/zyvorai/fluxvm:__VERSION__@sha256:__DIGEST__`. The placeholders are deliberate: the checked-in files cannot be
applied by mistake. Use the release asset, which has real values:

```sh
kubectl apply -f fluxvm-k8s-0.4.8.yaml
```

or render it yourself for a version and digest you have verified:

```sh
scripts/render-k8s.sh --version 0.4.8 --digest sha256:<digest> --out fluxvm-k8s-0.4.8.yaml
```

The script rejects a malformed digest, refuses to leave a placeholder behind, and checks that every image reference ends
up pinned by digest. It renders the namespace, RBAC, ConfigMap, both CRDs, the node DaemonSet and the MicroVM controller and
node agent. It leaves out `microvm/secret.yaml` (a token you create) and `microvm/servicemonitor.yaml` (needs the
Prometheus Operator CRDs).

### Host preparation

The node DaemonSet has a `host-prep` init container (the same image, **privileged**, only that container) that runs before the VMM
starts, on every pod start:

- fails if `/dev/kvm` is missing;
- runs the host's `modprobe nbd max_part=16` (it `chroot`s into the host root mounted at `/host`), and warns if `nbd` was
  already loaded with a smaller `max_part`;
- writes `/etc/qemu/bridge.conf` containing `allow all` on the host, which the `fluxctl` container mounts read-only
  at `/etc/qemu` so the image's `qemu-bridge-helper` sees it. `allow all` lets any bridge be used by QEMU TAP networking;
  narrow it if the node is shared.

If the init container fails the pod stays in `Init:Error`, with the reason in its log.

### What a DaemonSet still cannot do

- **Create `/dev/kvm`.** Virtualization has to be on in firmware and the `kvm` modules present. The init container only
  checks.
- **Provide a kernel.** It cannot upgrade or replace the node kernel, and cannot add `nbd` to a kernel built without it
  or without a module tree at `/lib/modules/$(uname -r)`; `modprobe` then fails. Distros that strip modules (some
  container-optimised OSes) are out.
- **Change an already loaded `nbd`.** `max_part` is only read at load time. Fixing it needs `rmmod` while idle, or a reboot.
- **Persist across reboot.** The module load and `bridge.conf` are redone when the pod starts again, but anything that
  needs them before the kubelet starts a pod (host services, other DaemonSets) cannot rely on them.
- **Make the host bridge.** Bridges and macvtap parents for `network_mode: tap|macvtap` are host network configuration.
- **Stage VM images.** There is still no Kubernetes-native image pull; images live under `/var/lib/fluxvm/images`.
- **Replace a hardened host.** The pod is `hostNetwork` and holds `NET_ADMIN`, `SYS_ADMIN` and more; the init container is
  fully privileged. Pod Security admission must allow `privileged` in `fluxvm-system`, and SELinux/AppArmor policy
  on the node may still block the helper binaries.
- **Run Cloud Hypervisor or Firecracker** until the vendor lock lands (above).

## Status of this change

The Dockerfile, workflow and manifests were written on a machine without Docker. What was checked there: YAML parses,
`shellcheck` on `scripts/render-k8s.sh` and on every `run:` block of the workflow, `kubeconform` on the rendered manifest
(19 resources, none invalid; 5 skipped as unknown kinds), and the render script's failure paths. **Not checked:** that the
image builds, that the packages exist under those names on arm64, that `qemu-bridge-helper` works inside the container,
that `host-prep` succeeds on a real node, and the whole publish workflow. The first run is by `workflow_dispatch` against a
version that already has a release.
