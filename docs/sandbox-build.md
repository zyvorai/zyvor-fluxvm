# `fluxctl sandbox build` — OCI image builds without Docker

Build OCI images using a short-lived FluxVM builder microVM. The result is cached by layer digest and can be used immediately with `fluxctl sandbox run`.

```bash
# Build from a Dockerfile in the current directory
fluxctl sandbox build -t myapp:1.0 .

# Then run it
fluxctl sandbox run myapp:1.0 --rm -- echo built
```

## How it works

1. A builder microVM is started (or claimed from a warm pool of builders).
2. The build context is shared via virtiofs (or uploaded for remote).
3. The guest runs a minimal builder (initially a thin wrapper around the existing rootfs unpack path, later BuildKit or a custom Dockerfile interpreter).
4. Layers are written to the host OCI blob store and rootfs cache exactly as `fluxctl oci pull` does today.
5. The final image is registered under the given tag.

## Status

This PR adds:

- CLI command skeleton (`fluxctl sandbox build`)
- REST endpoint sketch (`POST /v1/sandboxes/build`)
- Documentation and acceptance criteria
- Tracking for the builder-VM integration (reuses the existing unpack builder VM pattern)

Full layer-cache + Dockerfile parsing lands in follow-up commits on this branch.

## Acceptance

- [ ] `fluxctl sandbox build -t app .` produces a runnable image
- [ ] Subsequent builds of the same layers are cache hits
- [ ] Built images appear in `fluxctl oci ls` and can be run with `fluxctl sandbox run`
- [ ] No Docker required on the host
