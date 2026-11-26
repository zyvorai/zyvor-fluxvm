# Container sandboxes (OCI images — no Docker required)

A sandbox can be a container image instead of a full Debian VM. Each container gets its own lightweight Linux VM: a small kernel boots straight into `fluxvm-oci-init`, which mounts the image as the root filesystem and runs its entrypoint.

**There is no Docker, no shared Linux VM, and no SSH.** The sandbox is still an ordinary FluxVM sandbox, with a TTL, quotas, an egress policy, exec and file transfer, and the REST, MCP and `fluxctl` APIs.

This is the preferred way to run containers. The full Docker engine (`fluxctl docker up`) is only a compatibility mode for the remaining gaps (image builds, arbitrary bind mounts, Compose daemon). See [DOCKER-FREE.md](DOCKER-FREE.md).

```bash
fluxctl sandbox run alpine:3.22 --rm -- sh -c 'echo hello from $(hostname); exit 3'
# hello from alpine-1a2b3c
echo $?   # 3

curl -s -X POST localhost:7788/v1/sandboxes -H 'Content-Type: application/json' \
  -d '{"ttl_seconds": 600, "profile": "tiny", "oci": {"image": "python:3.13-alpine", "command": ["sleep", "infinity"]}}'
curl -s -X POST localhost:7788/v1/sandboxes/$ID/process -d '{"command": "python3 -V"}' -H 'Content-Type: application/json'
curl -s localhost:7788/v1/sandboxes/$ID/logs?lines=50
```

```bash
# A web server with a published port, a persistent volume, restarts and a health check
fluxctl sandbox run nginx:alpine -p 8080:80 -v site:/usr/share/nginx/html --restart always \
  --health-cmd "wget -q -O /dev/null http://127.0.0.1" --health-interval 5
curl localhost:8080
```

MCP: `sandbox_create` with `oci_image` (plus optional `oci_command`, `oci_env` and `oci_ports`), then `sandbox_exec`, `sandbox_read_file` and `sandbox_write_file` as for any sandbox. `sandbox_logs` returns the console and the exit code.

**On Linux** the same command runs the image with FluxVM's own container runtime (`--runtime container`) or, for untrusted code, is refused until the one-VM-per-container flow is extended to the Linux backends. See [linux-runtime.md](linux-runtime.md).

Several containers that work together are a stack: see [macos-stacks.md](macos-stacks.md#container-services), which can also import a `docker-compose.yml`.

## Compared with Apple's `container` and with Docker

| | Apple `container` | FluxVM native sandbox | Docker Desktop / dockerd |
|---|---|---|---|
| Isolation | One VM per container | One VM per container | Shared kernel |
| Lifetime | Until stopped | TTL, quotas, admission | Restart policies |
| Network | NAT | NAT, offline, egress allow-list, private nets | Docker networks |
| Access | CLI | REST, MCP, `fluxctl`, tokens, tenants, audit | Docker CLI / API |
| Exec | Own agent | FluxVM guest agent over vsock | docker exec |
| Image build | `container build` | Not yet (planned: `fluxctl sandbox build`) | `docker build` |
| Bind mounts | Yes | Named volumes only (bind mounts planned) | Yes |

## Remaining gaps vs full Docker

- Image builds (`docker build`) — planned as builder microVM with layer cache
- Arbitrary host bind mounts — planned as controlled virtiofs with path allowlists
- Full Compose daemon / Docker networks — stacks + private nets cover most cases; thin API shim can cover the rest

Until those land, use `fluxctl docker up` only when you specifically need them. For everything else, prefer `fluxctl sandbox`.

## Request

`POST /v1/sandboxes` with an `oci` object. `ttl_seconds`, `profile`, `vcpus`, `memory_mib`, `offline`, `allow_hosts`, `http_proxy_port(s)`, `volumes` and `name` apply as for other sandboxes. Without a profile or size the VM gets 1 vCPU and 512 MiB (minimum).

| Field | Default | Meaning |
|---|---|---|
| `image` | (required) | `alpine:3.22`, `ghcr.io/org/app:1.2`, `nginx@sha256:…`. Docker Hub names are expanded. |
| `platform` | `linux/arm64` | `linux/amd64` runs under Rosetta. |
| `command` | image's `Cmd` | Replaces `Cmd`. |
| `entrypoint` | image's `Entrypoint` | Replaces `Entrypoint` and drops `Cmd`. |
| `env` | `[]` | `KEY=value`. |
| `secret_env` | `{}` | Write-only environment (never stored in VM record). |
| `workdir` | image's `WorkingDir` or `/` | |
| `user` | image's `User` or `65534:65534` | |
| `read_only_root` | `true` | |
| `exit_policy` | `keep` | `poweroff` for `fluxctl sandbox run`. |
| `ports` | `[]` | `HOST:CONTAINER` or `PORT`, optional `/udp`. |
| `expose` | `[]` | Ports reachable by other sandboxes on the same host. |
| `gateway_hosts` | `[]` | Extra `/etc/hosts` entries pointing at the NAT gateway. |
| `restart` | `no` | `no`, `on-failure`, `always`. |
| `max_restarts` | unlimited | |
| `healthcheck` | none | Command, interval, timeout, retries, start_period. |

`fluxctl sandbox run IMAGE [flags] -- CMD…` creates the sandbox with `exit_policy: poweroff`, streams the console, and exits with the container's exit code.

(The rest of the original document — boot process, warm pool, security defaults, published ports, Rosetta, private networks, volumes, restarts — remains unchanged and is still authoritative.)
