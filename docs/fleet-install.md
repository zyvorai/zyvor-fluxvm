# Installing FluxVM on many nodes

Nodes **pull** a signed release from a static mirror. Nothing is compiled on a node and nobody connects to it. An operator
seeds two values on each node (the mirror URL and the release public key), and the node does the rest: fetch, verify,
install, check, roll back if it did not come up.

```
CI release ──► packages + SBOM ──► make-release-manifest.sh (signs) ──► mirror (static files / object store)
                                                                          ▲
node: fluxvm-bootstrap (timer) ── channel file ── manifest ── package ───┘   verify signature → wave? → install → health → roll back
```

## What is on the mirror

```
channels/<channel>.json  + .sig     the pointer: version, serial, expiry, revoked list, rollout rules
releases/<version>/manifest.json + .sig, SHA256SUMS, sbom.cdx.json, the .tar.gz and .deb per architecture
```

Signatures are raw Ed25519 over the exact bytes of the file (`openssl pkeyutl -rawin`, openssl 3). The channel file names the
release manifest and its sha256; the manifest names each package and its sha256. A node checks the signature **before** it
parses anything, then the hashes down the chain, so a compromised mirror can only withhold releases, not substitute them.

| Rule | Where | Effect |
|---|---|---|
| `serial` | channel | monotonic; a node refuses a lower one than it accepted (stale mirror, replay) |
| `expires` | channel | a node refuses an expired channel, so a frozen mirror stops being trusted |
| `revoked` | channel | listed versions are never installed; a node on one moves off it |
| `min_upgrade_from` | channel | refuse to jump from an older version; go through an intermediate release |
| `rollout.percent`, `salt` | channel | wave: a node installs when `sha256(salt‖node-id) mod 10000 < percent×100`. Raising `percent` only adds nodes |
| `require_labels`, `exclude_labels` | channel | restrict by the node's `LABELS` |

The node id is `/etc/machine-id`; regenerate it for every cloned instance (see `deploy/cloud-init/fluxvm.yaml`).

## Publishing

`release-linux.yml` builds the tarball and `.deb` per architecture and installs them in a clean container. When
`FLUXVM_RELEASE_KEY` (secret) is set, the `publish` job then writes the SBOM, signs, attests build provenance, uploads the
tree as the `mirror-tree` artifact and, if `MIRROR_S3_URI` (variable) is set, syncs it: releases first, channel files last.
A tag publishes to `canary` at 100%. Promote with a manual run of the workflow (`channel`, `percent`), or locally:

```bash
openssl genpkey -algorithm ed25519 -out release.key && openssl pkey -in release.key -pubout -out release.pub   # once
scripts/make-release-manifest.sh --version 0.4.12 --artifacts target/package --key release.key --out mirror --channel stable --percent 5
scripts/make-release-manifest.sh --promote --version 0.4.12 --key release.key --out mirror --channel stable --percent 25 --serial $(date +%s)
```

Widen a wave by promoting again with a higher `--percent`. To stop one, publish the channel with `--percent 0` or `--revoke`.

## On a node

`scripts/fluxvm-bootstrap.sh` (installed as `fluxvm-bootstrap`) reads `/etc/fluxvm/bootstrap.conf`.

```bash
fluxvm-bootstrap check       # what would happen, installs nothing
fluxvm-bootstrap install     # what the timer runs
fluxvm-bootstrap status      # status.json: state, installed version, serial, last result
fluxvm-bootstrap rollback    # back to the previous version; the version is marked failed and not retried
fluxvm-bootstrap pin 0.4.11  # hold this node; `unpin` releases it
fluxvm-bootstrap verify      # installed files against the package hashes (dpkg -V for deb)
```

It takes a lock, waits a stable per-node plus random delay, fetches with ETag and retries with backoff that honours
`Retry-After`, verifies, downloads with resume into `/var/lib/fluxvm-bootstrap/cache`, checks the sha256 from the signed
manifest, checks disk and `/dev/kvm`, installs (`apt-get install` with the existing config kept, or an unpacked tree under
`/opt/fluxvm/<version>` with an atomic `current` symlink), restarts the service and waits for the new version to answer. If
that fails it reinstalls the previous version, records the failed one, and exits 3. `FLUXVM_AUTO_UPGRADE=0` reports only.
It writes `status.json` and, if `/var/lib/node_exporter/textfile_collector` exists, a metrics file.
`systemctl enable --now fluxvm-bootstrap.timer` schedules it every 6 hours with randomized delay (off by default).

## What is tested, and what is not

`scripts/test-bootstrap.sh` (CI, Linux) runs the installer against a local signed mirror in tarball mode: tampered artifact,
tampered channel, wrong key, missing signature, lower serial, expired, revoked, `min_upgrade_from`; waves and labels; pin;
report-only; rollback on a version that does not come up, and no retry of it; resume of a truncated download; a 429 with
`Retry-After`; the 5% wave being a subset of the 20% wave over 10,000 synthetic ids; jitter bounds.

Not tested: the `deb` install/rollback path of the bootstrap (the `.deb` itself is tested by `scripts/test-deb.sh`), systemd
timers, and anything at fleet scale. Capacity: one wave pulls about (package size × nodes) from the mirror (the tarball is
~50 MB, so 10,000 nodes is ~500 GB per wave); serve it from an object store or CDN, not GitHub, and use the jitter and waves to
spread it. Request-rate limits and a real thundering herd were not measured.
