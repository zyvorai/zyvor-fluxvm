# OCI registry tools

`fluxctl image ...` manages repositories in any OCI registry (GHCR, Docker Hub, Harbor, ECR, Zot, `registry:2`) over
its HTTP API. It is client-side only: no daemon, no `docker` or `crane` binary. It reuses the reference parsing and
bearer-token auth of the OCI sandbox puller.

Credentials come from `FLUXVM_REGISTRY_USER` and `FLUXVM_REGISTRY_PASSWORD`; without them requests are anonymous.
Output is JSON by default; `-o table` prints a readable summary.

**Status:** implemented and tested against an in-process mock registry (token auth, tag pagination, referrers API and
fallback, attach, retag, delete, index build). Not yet run against a public registry.

| Command | What it does |
|---|---|
| `fluxctl image inspect REF` | Manifest or index: digest, media type, config (platform, created, entrypoint, cmd, env, labels), layers and sizes, index platforms, annotations, subject |
| `fluxctl image tags REF [--page-size N]` | All tags of the repository, following `Link: rel="next"` pagination |
| `fluxctl image tag REF NEW_TAG` | Retag: downloads the manifest and uploads the identical bytes under `NEW_TAG` (digest unchanged) |
| `fluxctl image rm REPO@sha256:...` | `DELETE /manifests/<digest>` |
| `fluxctl image referrers REF [--artifact-type T]` | Artifacts whose `subject` is REF (SBOMs, signatures, attestations) |
| `fluxctl image attach REF FILE --artifact-type T` | Publish FILE as an artifact with `subject` = REF |
| `fluxctl image index create REF MANIFEST...` | Build a multi-architecture OCI image index |

```bash
export FLUXVM_REGISTRY_USER=me FLUXVM_REGISTRY_PASSWORD=$TOKEN
fluxctl image inspect ghcr.io/acme/app:1.0 -o table
fluxctl image tags ghcr.io/acme/app
fluxctl image tag ghcr.io/acme/app:1.0 stable
fluxctl image attach ghcr.io/acme/app:1.0 sbom.spdx.json --artifact-type application/spdx+json
fluxctl image referrers ghcr.io/acme/app:1.0 --artifact-type application/spdx+json
fluxctl image index create ghcr.io/acme/app:1.0 ghcr.io/acme/app:1.0-arm64 ghcr.io/acme/app:1.0-amd64
fluxctl image rm ghcr.io/acme/app@sha256:<digest>
```

## Behavior worth knowing

- **Delete is by digest only.** A manifest delete removes every tag pointing at it, so a tag reference is refused
  before any request is sent. When the registry answers 405/403/401 the error says deletion is disabled or not
  permitted (`registry:2` needs `REGISTRY_STORAGE_DELETE_ENABLED=true`; GHCR and Docker Hub delete through their own
  package APIs and do not support this call).
- **Referrers fall back.** `referrers` calls `GET /v2/<repo>/referrers/<digest>`. On 404 it reads the index at tag
  `sha256-<hex>` (the OCI 1.1 tag schema); a missing fallback tag means no referrers. The JSON says which path was
  used in `via` (`api` or `tag-schema`). `--artifact-type` is always applied client-side.
- **Attach** uploads an empty config (`application/vnd.oci.empty.v1+json`) and FILE as the single layer (media type
  `--layer-media-type`, default the artifact type; the file name goes in `org.opencontainers.image.title`), then PUTs
  a manifest with `artifactType` and `subject` set to the target's digest, size and media type. If the registry does
  not answer with `OCI-Subject` (no referrers API), `attach` updates the `sha256-<hex>` index itself. That is a
  read-modify-write: two concurrent attaches to one subject on such a registry can lose an entry. Files up to 256 MiB.
- **Index create** reads each listed manifest's image config for `architecture`, `os`, `variant` and `os.version`. All
  manifests must be in the index's own repository (copy others first), may not themselves be indexes, and may not
  repeat a platform. The target must be a tag. The index is published as `application/vnd.oci.image.index.v1+json`.
- **Retag** keeps the manifest bytes and media type, so Docker v2 manifests stay Docker v2.
- Limits: JSON documents up to 4 MiB; tag listing stops after 1000 pages.

Code: `fluxvm_image::oci_manage` (library) and `crates/fluxctl/src/image_mgmt.rs` (CLI).
