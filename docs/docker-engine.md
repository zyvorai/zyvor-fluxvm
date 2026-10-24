# Docker engine in a VM (`fluxctl docker`)

FluxVM already answers a **Docker Engine API subset** on `{run_dir}/docker.sock` (see
[oci-sandboxes.md](oci-sandboxes.md#docker-cli-docker_host)): each container is its own microVM, which is fast and isolated, but
the subset cannot `build`, run Compose, bind-mount Mac paths or create Docker networks.

`fluxctl docker` is the other half: **a real `dockerd` in one managed FluxVM VM, with its socket on your Mac.** Everything the Docker
CLI does works, because it is the real engine: `docker build`, `docker compose up --build --wait`, bind mounts, user-defined
networks with name resolution, `tmpfs`, `read_only`, `cap_drop`, health checks.

| | Built-in `docker.sock` | `fluxctl docker` |
|---|---|---|
| Containers | one microVM each | ordinary containers in one VM |
| `docker build`, Compose | no | yes |
| Bind mounts of Mac paths | no | yes, for folders you share (below) |
| Docker networks | no | yes |
| Start-up | about a second per container | one VM, a few minutes the first time (it installs Docker) |
| Isolation | strongest (VM per container) | the containers share one VM |

## Use it

```bash
fluxctl docker up --share ~/work                 # create (first time) or start the VM and publish the socket
eval "$(fluxctl docker env)"                     # export DOCKER_HOST=unix:///tmp/fluxvm-run-501/docker-engine.sock
docker run --rm alpine:3.22 echo hello
docker build -t app .
docker compose up --build --wait -d
fluxctl docker status                            # VM, address, whether the socket answers (JSON)
fluxctl docker down                              # stop the forward and the VM (Docker data kept)
fluxctl docker down --purge                      # delete the VM and everything Docker stored in it
```

`up` is idempotent: run it again to restart the VM or the socket forward. It prints the `export DOCKER_HOST=…` line.

### Bind mounts

`--share HOST[:GUEST][:ro]` shares a Mac folder into the VM with virtiofs. **The guest path defaults to the host path**, so a
command run on the Mac such as `docker run -v "$PWD/conf:/etc/conf" …` or a Compose `volumes: ["./conf:/etc/conf"]` names a path
that exists, with the same content, inside the VM where `dockerd` runs. A path outside a shared folder is empty in the guest.
Shares are fixed when the VM is created; to add one, `down --purge` and `up` again (or use another `--name`).

### Keep it running (LaunchAgent)

```bash
fluxctl docker agent install --share ~/work      # runs `fluxctl docker supervise` at login, restarts it if it exits
fluxctl docker agent uninstall
```

`supervise` checks every 30 seconds (`--every`) that the VM is running and the socket answers, and repairs either. It never
deletes anything. The agent logs to `~/Library/Logs/fluxvm-docker-engine.log`.

## How it works

1. `up` finds the VM named `docker-engine` (`--name`) or creates it: Debian 13, `docker.io`, `docker-compose` and
   `docker-buildx` from apt (cloud-init), your SSH key, 2 vCPUs, 3 GiB, 40 GiB disk (all flags).
2. It waits for the guest's address, for SSH, and for `docker info` to succeed.
3. It starts a detached `ssh -N -L {socket}:/var/run/docker.sock` to the guest (with keepalives) and records its pid next to the
   socket. The socket is `{run_dir}/docker-engine.sock`, beside the built-in `docker.sock`; `--socket` overrides it.

Nothing listens on a TCP port, and the guest's Docker socket is only reachable by your user through the SSH key.

## Limits

- One engine VM per name; it uses its vCPUs and memory while it runs.
- The containers inside share the VM's kernel and resources. For strong per-container isolation use the built-in
  [container sandboxes](oci-sandboxes.md).
- Only the folders you `--share` exist in the guest. Docker volumes live on the VM's disk and go away with `down --purge`.
- Published container ports are on the guest's network, not the Mac's loopback. Reach them with `fluxctl ssh docker-engine -- curl …`
  or a forward; Compose stacks that test themselves from inside the guest need nothing extra.
- macOS with Apple silicon only (it uses a `vz` VM).

Live check: `scripts/docker-engine-live-test.sh`.
