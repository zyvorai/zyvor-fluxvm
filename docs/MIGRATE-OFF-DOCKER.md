# Migrating off Docker Desktop or OrbStack

## Steps

1. Install FluxVM and start the control plane.
2. Replace `docker run` with `fluxctl run` or `fluxctl kube run`.
3. Replace Compose files with FluxVM networks and `fluxctl kube` apps, or keep them behind the optional adapter.
4. Move volumes with `fluxctl import` (planned) or manual copy.
5. Point CI at the FluxVM API instead of the Docker socket.

## Status

Guidance only. No existing files are modified.
