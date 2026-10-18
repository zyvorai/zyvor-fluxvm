# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
# One image, several entrypoints: `fluxctl serve` (VMM REST API), `fluxvm-kube`
# (DisposableVm operator), `fluxvm-microvm` (scheduled MicroVM controller /
# node-agent). Deploy selects via each container's `command:` - see
# deploy/k8s/daemonset.yaml and deploy/k8s/microvm/.
#
# This Dockerfile does NOT compile anything. The binaries are the ones the Linux
# release already built in the ubuntu:22.04 container (glibc 2.35, the
# compatibility floor enforced by scripts/check-glibc-floor.sh) and shipped in
# the release tarball. The image copies that staged tree, so the image runs the
# exact bytes that were tested and released, and the runtime base below
# (ubuntu 24.04, glibc 2.39) is never older than the glibc they were linked on.
# Rebuilding here on a newer glibc, as an earlier version of this file did,
# would have produced binaries the 22.04 floor rejects.
#
# Build (one architecture per build; the tree must match the target platform):
#
#   tar -xzf fluxvm-<ver>-linux-x86_64.tar.gz -C stage     # or aarch64
#   docker buildx build --platform linux/amd64 \
#     --build-context stage=stage/fluxvm-<ver>-linux-x86_64 \
#     --build-arg VERSION=<ver> -t fluxvm:<ver> .
#
# The multi-arch manifest is assembled afterwards by
# .github/workflows/publish-image.yml; see docs/container-image.md.

# `stage` is supplied with --build-context. The empty fallback only makes a
# forgotten flag fail at the COPY below instead of silently using something else.
FROM scratch AS stage

# TODO(vendor.lock): Cloud Hypervisor and Firecracker used to be downloaded at
# build time ("latest" from the GitHub API, via scripts/install-*.sh and
# python3). That is gone on purpose. Once packaging/vendor.lock lands (pinned
# version + SHA-256 per arch), the workflow should fetch and verify those two
# binaries and pass them as `--build-context vendor=<dir>` containing
# `cloud-hypervisor` and `firecracker`. Until then the stage is empty and the
# image has no cloud-hypervisor/firecracker; the qemu backend works. (No stage
# is declared yet: an empty one would be dead weight and an untested COPY.)

# Pinned by index digest (looked up from the registry, ubuntu:24.04 as of
# 2026-10-18). Refresh with `crane digest docker.io/library/ubuntu:24.04`.
FROM docker.io/library/ubuntu:24.04@sha256:68ee22004a9109b44973313b02ff7b1de250f1de0f5ecb628d73f579aaff33ac AS runtime

ARG TARGETARCH
ARG VERSION=unknown
ENV DEBIAN_FRONTEND=noninteractive

# Runtime dependencies only, matching packaging/nfpm.yaml:
#   libhivex0 / libsystemd0   NEEDED by the binaries (check-glibc-floor allowlist)
#   qemu-system-x86|arm       QEMU backend, chosen by target architecture
#   qemu-utils / cloud-image-utils   qcow2 tooling, cloud-localds seed images
#   iproute2 / dnsmasq-base / nftables   TAP/bridge setup, per-netns DHCP, rules
#   linux-tools-generic       bpftool: ubuntu 24.04 has no `bpftool` package, the binary ships in the kernel tools
#                             package, so it is linked into PATH below (vnet.rs loads its programs with it)
#   util-linux                chroot/nsenter for the host-prep init container
RUN case "${TARGETARCH}" in \
      amd64) QEMU=qemu-system-x86 ;; \
      arm64) QEMU=qemu-system-arm ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
         "${QEMU}" qemu-utils cloud-image-utils \
         iproute2 dnsmasq-base nftables linux-tools-generic \
         libhivex0 libsystemd0 ca-certificates util-linux \
    && ln -s "$(ls /usr/lib/linux-tools/*/bpftool | head -n1)" /usr/local/bin/bpftool \
    && rm -rf /var/lib/apt/lists/*

COPY --from=stage bin/ /usr/local/bin/
COPY --from=stage lib/bpf/ /usr/lib/fluxvm/bpf/
COPY --from=stage share/ /usr/share/fluxvm/
# TODO(vendor.lock): COPY --from=vendor / /usr/local/bin/ goes here, see above.

LABEL org.opencontainers.image.title="fluxvm" \
      org.opencontainers.image.source="https://github.com/zyvorai/zyvor-fluxvm" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.version="${VERSION}"

ENTRYPOINT []
