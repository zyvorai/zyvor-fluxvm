# Running containers without Docker

FluxVM’s native path runs OCI images in isolated microVMs. You do not need Docker Desktop, a shared Linux VM, or a dockerd process.

```bash
# Run any OCI image (Docker Hub, GHCR, etc.) in its own VM
fluxctl sandbox run alpine:3.22 --rm -- echo hello

# With ports, volumes, restart policy and health check
fluxctl sandbox run nginx:alpine \
  -p 8080:80 \
  -v site:/usr/share/nginx/html \
  --restart always \
  --health-cmd "wget -q -O /dev/null http://127.0.0.1"
```

Each container gets its own kernel, its own network namespace, and the FluxVM guest agent over vsock. No SSH, no shared daemon.

## Why this is better than Docker

| | Native FluxVM sandbox | Traditional Docker |
|---|---|---|
| Isolation | One microVM per container | Shared kernel + namespaces |
| Attack surface | Hypervisor boundary | runc + host kernel |
| Cleanup | TTL + automatic teardown | Manual / restart policies |
| Networking | eBPF policy, offline mode, private nets | Docker networks |
| Agent | vsock guest agent (no SSH) | docker exec |
| Mac | Native vz, no Docker Desktop | Docker Desktop required |

## What already works today

- Pull, cache, and run any OCI image (linux/arm64 or linux/amd64 under Rosetta)
- Published ports, named volumes, restart policies, health checks
- Private networks and stacks (see macos-stacks.md)
- Partial Docker Engine API on `{run_dir}/docker.sock` so existing `DOCKER_HOST` scripts keep working for run/ps/logs/exec
- Warm pool for sub-second cold starts of common sizes
- Full REST, MCP, and `fluxctl` surface

## Remaining gaps (being closed)

These are the reasons `fluxctl docker up` still exists. Once closed, that path becomes unnecessary.

1. **Image builds** — `fluxctl sandbox build` (builder microVM, layer cache, Dockerfile subset or BuildKit)
2. **Bind mounts** — controlled host-path virtiofs mounts with allowlists (in addition to named volumes)
3. **Full Compose / multi-service networking** — richer stack support and optional thin Docker API coverage for networks

## Compatibility mode

If you still need full `docker build`, Compose, or arbitrary bind mounts today:

```bash
fluxctl docker up --share ~/work
eval "$(fluxctl docker env)"
docker compose up
```

This runs a real dockerd inside one FluxVM VM. Prefer the native path for new work.

See [oci-sandboxes.md](oci-sandboxes.md) for the full native contract and [docker-engine.md](docker-engine.md) for the compatibility engine.
For a minimal Docker Engine API endpoint over native sandboxes (no dockerd, opt-in with `FLUXVM_DOCKER_API`), see [DOCKER-API.md](DOCKER-API.md).
