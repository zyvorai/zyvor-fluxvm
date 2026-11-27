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

These are wired into `fluxctl`, the REST API and the daemon, and covered by the tests and live scripts listed in
[oci-sandboxes.md](oci-sandboxes.md#verified):

- Pull, cache, and run any OCI image (linux/arm64, or linux/amd64 under Rosetta on Apple silicon)
- Published TCP and UDP ports, named volumes (up to four per sandbox, virtiofs), restart policies, health checks
- Private networks and stacks: `fluxctl up` with a `fluxvm.toml`, and `fluxctl import-compose` to convert the parts of a
  `docker-compose.yml` that map onto container services (see [macos-stacks.md](macos-stacks.md))
- Partial Docker Engine API on `{run_dir}/docker.sock`, so existing `DOCKER_HOST` scripts keep working for `pull`, `images`, `run`,
  `ps`, `rm`, `logs`, `exec`, `stop`, `restart`, `kill`, `pause`/`unpause`, `top`, `stats`, `rename` and `events`
- Warm pool for sub-second cold starts of common sizes
- Full REST, MCP, and `fluxctl` surface

## Remaining gaps

These are the reasons `fluxctl docker up` still exists. Status is what the code on `main` does today, not what a design doc
plans. "Scaffold" means a source file exists but is not compiled into `fluxctl` (it has no `mod` declaration in
`crates/fluxctl/src/main.rs`), so the command or flag does not exist yet.

### 1. Image builds

| Path | Today | What happens |
|---|---|---|
| `fluxctl sandbox build` | **Missing (scaffold)** | `crates/fluxctl/src/sandbox_build.rs` defines the flags and prints a TODO; it is not compiled, so `fluxctl sandbox build` is an unknown command. |
| `docker build` on the FluxVM socket | **macOS images only** | `POST /build` (classic builder only) is forwarded to `fluxctl mac build`, which builds **macOS VM images** from a Macfile, not Linux OCI images. On a non-Mac host it is refused before the context is uploaded. BuildKit (`docker buildx`, `/grpc`, `/session`) and `--memory`, `--cpu-*`, `--network`, `--cache-from` are refused with a message naming the alternative. |
| `build:` in an imported Compose file | **Refused** | `fluxctl import-compose` treats `build:` as an error: build and push the image first. |

**Workaround:** build the image elsewhere (CI, `docker buildx` on any machine, `buildah`) and push it to a registry FluxVM can pull
from, or run a real engine with `fluxctl docker up` and `docker build` there.

**Planned:** a builder microVM behind `fluxctl sandbox build` that reuses the OCI unpack path, shares the context over virtiofs,
keeps a layer cache, and registers the result in the local image store (`docs/sandbox-build.md`, branch `feat/sandbox-build`).

### 2. Bind mounts (host paths)

| Path | Today | What happens |
|---|---|---|
| `fluxctl sandbox run -v` / REST `volumes` | **Named volumes only** | Volumes are FluxVM-managed directories under `volumes_dir`; there is no way to name an arbitrary host path. |
| `docker run -v /host:/guest` on the FluxVM socket | **Refused** | A non-empty `HostConfig.Binds` returns 400 "bind mounts are not supported by the FluxVM Docker shim; use named volumes via fluxctl". |
| Compose bind mounts via `import-compose` | **Dropped with a warning** | `source:target` host paths have no equivalent; use a named volume. |
| `fluxctl sandbox run --mount type=bind,...` | **Missing (scaffold)** | `crates/fluxctl/src/sandbox_mount.rs` has a parser and an allowlist check but is not compiled (it would not build as is: it calls `.context` without importing `anyhow::Context`). There is no `--mount` flag and no `allow_host_mounts` config key yet, despite what `docs/sandbox-bind-mounts.md` shows as the intended syntax. |

**Workaround:** put the data in a named volume (copy it in through the guest agent's file transfer, or seed the volume directory on
the host while no sandbox uses it), or use `fluxctl docker up --share ~/work`, which shares a host directory into the engine VM.

**Planned:** controlled virtiofs host-path mounts, off by default, limited to an allowlist of canonicalised host prefixes,
read-only unless asked otherwise (`docs/sandbox-bind-mounts.md`, branch `feat/sandbox-bind-mounts`).

### 3. Docker networks and Compose as a daemon

| Path | Today | What happens |
|---|---|---|
| `docker network create` on the FluxVM socket | **Refused** | Every container sits behind user-mode NAT; `GET /networks` lists only `fluxvm-nat`. |
| `docker compose up` against the FluxVM socket | **Not supported** | Compose needs networks and named volumes, which the shim refuses. Use `fluxctl import-compose` + `fluxctl up` instead. |
| Service discovery / health-ordered start for stacks | **Partly** | `fluxctl up` already orders by `depends_on` and resolves service names (all names with `network = "private"`). `crates/fluxctl/src/sandbox_stack.rs` (topological order, a placeholder `10.89.0.x` address plan) is a scaffold and not compiled. |

**Workaround:** `fluxctl import-compose docker-compose.yml --out fluxvm.toml` and `fluxctl up`; or `fluxctl docker up` for a real
engine.

**Planned:** wire stack networking onto the private-network IPAM and add the thin Docker network API on top
(`docs/sandbox-stacks.md`, branch `feat/sandbox-stacks-networking`).

### 4. Smaller gaps in the Docker API shim

- **Named volumes:** `docker volume create` is refused and `GET /volumes` is empty (a container's disk lives and dies with it);
  named volumes exist only through `fluxctl` and the REST API.
- **`docker cp`:** there are no `/containers/{id}/archive` endpoints; use the guest agent's file transfer through `fluxctl`/REST.
- **BuildKit:** never served (see image builds).

## Importing an image you already have in OrbStack or Docker Desktop

FluxVM pulls from a registry (`fluxctl oci pull`, `fluxctl sandbox run`, or `docker pull` against the built-in `{run_dir}/docker.sock`). It does not load a `docker save` archive or an OCI layout directory into the native OCI cache, and the built-in socket does not implement `docker load`.

How to export an image from Docker Desktop or OrbStack, push it to a registry FluxVM can reach (or load it into a `fluxctl kube` cluster), and verify the import: [IMPORT-IMAGES.md](IMPORT-IMAGES.md).

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
