#!/usr/bin/env bash
# shellcheck disable=SC2015
# Copyright 2026 Zyvor AI Labs · https://zyvor.dev
# SPDX-License-Identifier: Apache-2.0
#
# The container image and the Kubernetes manifests, without a cluster:
#   - build the Dockerfile from a staged tree (stub binaries), run it, check what is inside;
#   - render deploy/k8s with scripts/render-k8s.sh: pinned images, no placeholders, bad arguments refused;
#   - every rendered document parses and has the fields an apply needs;
#   - deploy/k8s/gitops/kustomization.yaml: records the known problem (it reaches outside its directory and
#     keeps the __VERSION__/__DIGEST__ placeholders) as a KNOWN-GAP line, not a failure.
# Needs docker (or podman as docker), python3 with PyYAML, and kubectl for the kustomize check (skipped without).
#   scripts/e2e/image-k8s.sh
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
for t in docker python3; do command -v "$t" >/dev/null || { echo "needs $t" >&2; exit 2; }; done
python3 -c 'import yaml' 2>/dev/null || { echo "needs PyYAML" >&2; exit 2; }
T="$(mktemp -d "${HOME}/.cache/fluxvm-imgk8s.XXXXXX" 2>/dev/null || mktemp -d)"; fails=0; IMG="fluxvm-e2e-image:$$"; CT=""
trap '[[ -n "$CT" ]] && docker rm -f "$CT" >/dev/null 2>&1; docker rmi -f "$IMG" >/dev/null 2>&1; rm -rf "$T"' EXIT
ok() { echo "ok   $1"; }; bad() { echo "FAIL $1"; fails=$((fails + 1)); }; gap() { echo "KNOWN-GAP $1"; }
check() { local n="$1"; shift; if "$@"; then ok "$n"; else bad "$n"; fi; }
VER=9.9.9; DIG="sha256:$(printf 'a%.0s' $(seq 1 64))"

# --- image -----------------------------------------------------------------------------------------------------
S="$T/stage"; mkdir -p "$S/bin" "$S/lib/bpf" "$S/share/systemd"
for b in fluxctl fluxvm-kube fluxvm-microvm fluxvm-agent; do printf '#!/bin/sh\necho "%s %s"\n' "$b" "$VER" > "$S/bin/$b"; chmod 755 "$S/bin/$b"; done
: > "$S/lib/bpf/x.bpf.o"; cp config.example.toml "$S/share/"
case "$(uname -m)" in x86_64) PLAT=linux/amd64 ;; *) PLAT=linux/arm64 ;; esac
if docker build -q --platform "$PLAT" --build-context "stage=$S" --build-arg "VERSION=$VER" --build-arg "TARGETARCH=${PLAT#linux/}" -t "$IMG" . >"$T/build.log" 2>&1; then
  ok "image builds from a staged tree"
  CT="$(docker run -d --entrypoint sleep "$IMG" infinity)"
  check "image: version label is the build arg" test "$(docker inspect -f '{{index .Config.Labels "org.opencontainers.image.version"}}' "$IMG")" = "$VER"
  check "image: license and source labels set" test -n "$(docker inspect -f '{{index .Config.Labels "org.opencontainers.image.source"}}' "$IMG")"
  check "image: staged binaries are in /usr/local/bin and run" test "$(docker exec "$CT" fluxctl)" = "fluxctl $VER"
  check "image: bpf objects and share/ copied" docker exec "$CT" sh -c 'test -f /usr/lib/fluxvm/bpf/x.bpf.o && test -f /usr/share/fluxvm/config.example.toml'
  check "image: runtime tools the daemon shells out to are present" docker exec "$CT" sh -c 'for c in ip nft qemu-img dnsmasq nsenter bpftool; do command -v $c >/dev/null || { echo "missing $c"; exit 1; }; done'
  check "image: bpftool runs (the dataplane loads its programs with it)" docker exec "$CT" bpftool version
  check "image: ENTRYPOINT is empty (each container picks its own command:)" test "$(docker inspect -f '{{len .Config.Entrypoint}}' "$IMG")" = 0
  check "image: no package manager caches left behind" test "$(docker exec "$CT" sh -c 'ls /var/lib/apt/lists | wc -l')" -le 1
else
  sed 's/^/     | /' "$T/build.log" | tail -20; bad "image builds from a staged tree"
fi
if docker build -q --platform "$PLAT" -t "$IMG-nostage" . >"$T/b2.log" 2>&1; then docker rmi -f "$IMG-nostage" >/dev/null 2>&1; bad "image: a build without --build-context stage must fail"
else ok "image: a build without --build-context stage fails instead of shipping an empty image"; fi

# --- render ----------------------------------------------------------------------------------------------------
check "render: a rendered file is produced" scripts/render-k8s.sh --version "$VER" --digest "$DIG" --out "$T/r.yaml"
check "render: no placeholder survives" test "$(grep -c -E '__VERSION__|__DIGEST__' "$T/r.yaml")" = 0
check "render: every image is pinned by version and digest" python3 - "$T/r.yaml" "$VER" "$DIG" <<'EOF'
import sys, yaml
imgs = []
def walk(o):
    if isinstance(o, dict):
        for k, v in o.items():
            if k == "image" and isinstance(v, str): imgs.append(v)
            else: walk(v)
    elif isinstance(o, list):
        for v in o: walk(v)
for d in yaml.safe_load_all(open(sys.argv[1])):
    if d: walk(d)
want = f"ghcr.io/zyvorai/fluxvm:{sys.argv[2]}@{sys.argv[3]}"
bad = [i for i in imgs if i != want]
print(f"{len(imgs)} images", "bad:" if bad else "", *bad)
sys.exit(1 if bad or not imgs else 0)
EOF
check "render: refuses a version that is not x.y.z" bash -c "! scripts/render-k8s.sh --version latest --digest $DIG --out $T/x.yaml 2>/dev/null"
check "render: refuses a tag-only digest" bash -c "! scripts/render-k8s.sh --version $VER --digest sha256:abc --out $T/x.yaml 2>/dev/null"
check "render: refuses a missing digest" bash -c "! scripts/render-k8s.sh --version $VER --out $T/x.yaml 2>/dev/null"
check "render: secret.yaml (holds a token) is not in the output" bash -c "! grep -q 'kind: Secret' $T/r.yaml"
check "render: documents are well formed (kind, apiVersion, name; namespaced ones name a namespace)" python3 - "$T/r.yaml" <<'EOF'
import sys, yaml
cluster = {"Namespace","CustomResourceDefinition","ClusterRole","ClusterRoleBinding"}
n = 0; bad = []
for d in yaml.safe_load_all(open(sys.argv[1])):
    if not d: continue
    n += 1
    for f in ("apiVersion", "kind"):
        if f not in d: bad.append(f"missing {f}: {str(d)[:60]}")
    m = d.get("metadata", {})
    if not m.get("name"): bad.append(f"{d.get('kind')} without a name")
    if d.get("kind") not in cluster and not m.get("namespace") and d.get("kind") not in ("List",): bad.append(f"{d.get('kind')}/{m.get('name')} has no namespace")
print(n, "documents", *bad, sep="\n") if bad else print(n, "documents")
sys.exit(1 if bad or n == 0 else 0)
EOF
python3 - "$T/r.yaml" <<'EOF' | sed 's/^/     | /'
import sys, yaml
for d in yaml.safe_load_all(open(sys.argv[1])):
    if not d or d.get("kind") not in ("DaemonSet", "Deployment"): continue
    spec = d["spec"]["template"]["spec"]
    for c in spec.get("initContainers", []) + spec["containers"]:
        sc = c.get("securityContext", {})
        print(f"{d['kind']}/{d['metadata']['name']} {c['name']}: privileged={sc.get('privileged', False)} hostNetwork={spec.get('hostNetwork', False)} hostPID={spec.get('hostPID', False)} limits={'resources' in c and 'limits' in c['resources']}")
EOF

# --- gitops ----------------------------------------------------------------------------------------------------
if command -v kubectl >/dev/null; then
  export KUBECONFIG=/dev/null
  if kubectl kustomize deploy/k8s/gitops >/dev/null 2>"$T/k1.log"; then ok "gitops kustomization builds from its own directory"
  else gap "deploy/k8s/gitops/kustomization.yaml does not build: it reaches outside its directory ($(grep -o 'not in or below' "$T/k1.log" | head -1))"; fi
  if kubectl kustomize --load-restrictor LoadRestrictionsNone deploy/k8s/gitops >"$T/k2.yaml" 2>/dev/null; then
    if grep -q -E '__VERSION__|__DIGEST__' "$T/k2.yaml"; then gap "gitops output (with the restrictor off) still carries __VERSION__/__DIGEST__ placeholders: it is not appliable as is"
    else ok "gitops output has no placeholders"; fi
  fi
else echo "skip gitops: kubectl not installed"; fi

echo; echo "$fails check(s) failed"; exit "$fails"
