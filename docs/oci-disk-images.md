# VM disk images as OCI artifacts

`fluxctl image push` and `pull-disk` move a single VM disk (raw or qcow2) through any OCI registry, the way KubeVirt's
containerDisk does, but with no skopeo or docker: the push and pull paths are pure Rust in `fluxvm_image::oci_push`.

For whole VM bundles (a macOS template directory), chunked and zstd-compressed with zero chunks skipped, use
[`fluxctl vm-image`](vm-images.md) instead. `image push` keeps the disk as one uncompressed layer, so any OCI tool can
fetch the blob and the layer digest is the file's own sha256.

**Status:** tested against an in-process mock registry (token auth, chunked upload, blob skip, digest checks).
See the PR for what was run against a real `registry:2`.

```bash
export FLUXVM_REGISTRY_USER=me FLUXVM_REGISTRY_PASSWORD=$TOKEN   # omit for anonymous / local registries

fluxctl image push debian-13.qcow2 ghcr.io/acme/disks/debian:13            # format detected from the file
fluxctl image push disk.raw localhost:5000/disks/test:1 --format raw --arch amd64
fluxctl image pull-disk ghcr.io/acme/disks/debian:13 ./debian-13.qcow2    # the output file must not exist
fluxctl image pull-disk ghcr.io/acme/disks/debian@sha256:… ./pinned.qcow2
```

Both print JSON (manifest digest, sizes, format, virtual size). Plain HTTP is used only for `localhost` and
`127.0.0.1` registries.

## Layout

An OCI image manifest (schema 2, OCI 1.1) with `artifactType: application/vnd.fluxvm.disk.v1`:

| Part | Media type |
|---|---|
| config | `application/vnd.fluxvm.disk.config.v1+json`: `architecture`, `os: linux`, `rootfs.diff_ids`, `disk.format`, `disk.virtualSize` |
| layer (exactly one) | `application/vnd.fluxvm.disk.layer.v1.raw` or `….qcow2`: the file, uncompressed |

## Behaviour

- The file is streamed: hashed once up front (so an existing layer is skipped with `HEAD`), then sent in `--chunk-mib`
  chunks (default 64; a file that fits in one chunk goes up in a single `PUT`). Memory use is one chunk. A file that
  changes while it is being pushed aborts the push.
- Auth follows the Basic or Bearer challenge; the token asks for `pull,push` on the repository.
- The registry's `Docker-Content-Digest` for the manifest must equal the locally computed digest.
- `pull-disk` verifies a digest-pinned manifest, the config against its descriptor, and the layer's size and sha256 while
  streaming into a temporary file that is renamed onto the destination only when everything matched.
- `--format` must agree with the file's magic (a qcow2 file cannot be pushed as raw by mistake).
