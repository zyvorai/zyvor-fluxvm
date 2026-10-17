#!/usr/bin/env bash
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# Render deploy/k8s/*.yaml into one apply-able file with every image pinned to
# ghcr.io/zyvorai/fluxvm:<version>@sha256:<digest>. The checked-in manifests carry
# the placeholders __VERSION__ and __DIGEST__ on purpose: a manifest that names
# no real digest must not be appliable by accident.
#
#   scripts/render-k8s.sh --version 0.4.8 --digest sha256:<64 hex> [--out fluxvm-k8s-0.4.8.yaml]
#
# Left out: microvm/secret.yaml (holds a token you create) and
# microvm/servicemonitor.yaml (needs the Prometheus Operator CRDs); apply those yourself.
set -euo pipefail

VERSION="" DIGEST="" OUT=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --digest)  DIGEST="${2:-}";  shift 2 ;;
    --out)     OUT="${2:-}";     shift 2 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.]+)?$ ]] || { echo "--version must be like 1.2.3, got '$VERSION'" >&2; exit 2; }
[[ "$DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "--digest must be sha256:<64 lowercase hex>, got '$DIGEST'" >&2; exit 2; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
K="$ROOT/deploy/k8s"
OUT="${OUT:-fluxvm-k8s-${VERSION}.yaml}"

# Order matters: namespace, CRDs and RBAC before the workloads that use them.
FILES=(
  namespace.yaml rbac.yaml configmap.yaml crd.yaml daemonset.yaml
  microvm/crd.yaml microvm/rbac.yaml microvm/controller.yaml microvm/node-agent.yaml
)

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
for f in "${FILES[@]}"; do
  [[ -f "$K/$f" ]] || { echo "missing $K/$f" >&2; exit 1; }
  {
    printf -- '---\n# source: deploy/k8s/%s\n' "$f"
    sed -e "s/__VERSION__/${VERSION}/g" -e "s/__DIGEST__/${DIGEST#sha256:}/g" "$K/$f"
    printf '\n'
  } >> "$tmp"
done

if grep -n '__VERSION__\|__DIGEST__' "$tmp"; then
  echo "unreplaced placeholder left in the output" >&2; exit 1
fi
# Every fluxvm image reference must now be pinned by digest.
if grep -E 'image:[[:space:]]*ghcr\.io/zyvorai/fluxvm' "$tmp" | grep -vq '@sha256:[0-9a-f]\{64\}'; then
  echo "an image reference is not pinned by digest" >&2; exit 1
fi
n="$(grep -cE "image:[[:space:]]*ghcr\.io/zyvorai/fluxvm:${VERSION}@${DIGEST}" "$tmp" || true)"
[[ "$n" -ge 1 ]] || { echo "no image reference was rendered" >&2; exit 1; }

mv "$tmp" "$OUT"
trap - EXIT
echo "wrote $OUT ($n image references pinned to ${DIGEST})"
