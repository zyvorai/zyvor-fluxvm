# Docker engine in a VM (`fluxctl docker`)

FluxVM already answers a **Docker Engine API subset** on `{run_dir}/docker.sock` (see
[oci-sandboxes.md](oci-sandboxes.md#docker-cli-docker_host)): each container is its own microVM, which is fast and isolated, but
the subset cannot `build`, run Compose, bind-mount Mac paths or create Docker networks.

`fluxctl docker` is the other half: **a real `dockerd` in one managed FluxVM VM, with its socket on your Mac.** Everything the Docker
CLI does works, because it is the real engine: `docker build`, `docker compose up --build --wait`, bind mounts, user-defined
networks with name resolution, `tmpfs`, `read_only`, `cap_drop`, health checks.

On Linux there is a third option that is not Docker at all: FluxVM's own container runtime (`fluxctl sandbox run IMAGE --runtime container`),
managed by systemd, rootless where the kernel allows it, no Docker API. See [linux-runtime.md](linux-runtime.md) for the support matrix.

| | Built-in `docker.sock` | `fluxctl docker` |
|---|---|---|
| Containers | one microVM each | ordinary containers in one VM |
| `docker build`, Compose | build only (macOS images, forwarded to the Mac builder); no Compose | yes |
| Bind mounts of Mac paths | no | yes, for folders you share (below) |
| Docker networks | no | yes |
| Start-up | about a second per container | one VM, a few minutes the first time (it installs Docker) |
| Isolation | strongest (VM per container) | the containers share one VM |

## What the built-in `docker.sock` supports

Every path also works with a version prefix (`/v1.44/containers/json`; the engine reports API 1.44, the oldest the current docker CLI accepts). Anything not listed answers `501` with a Docker-style
`{"message": ...}` body that names `fluxctl docker up`, so the `docker` CLI prints a sentence you can act on.

| Endpoint | `docker` command | Notes |
|---|---|---|
| `GET /_ping`, `/version`, `/info` | `docker version`, `docker info` | |
| `GET /containers/json` | `docker ps` | newest first; `-a` includes stopped containers, `--latest` and `--last N` select recent containers across all states |
| `POST /containers/create` | `docker create`, `docker run` | no bind mounts |
| `GET /containers/{id}/json` | `docker inspect` | |
| `POST /containers/{id}/start`, `stop`, `wait` | `docker start`, `stop`, `wait` | |
| `POST /containers/{id}/restart` | `docker restart` | stop, then start; `t` is ignored |
| `POST /containers/{id}/kill` | `docker kill [-s SIG]` | only `KILL`/`9` (default, at once) and `TERM`/`15` (clean guest shutdown); other signals answer 501, a name that is no signal 400; a stopped container answers 409 |
| `POST /containers/{id}/pause`, `unpause` | `docker pause`, `unpause` | if the host's VM backend cannot pause, 501 says so |
| `GET /containers/{id}/top` | `docker top` | runs `ps` in the guest; `ps_args` is ignored; an image without `ps` answers 501 |
| `GET /containers/{id}/stats` | `docker stats` | `stream=false` gives one sample, otherwise one per second. CPU is the VM's lifetime average; no per-container network counters. Hosts that cannot read VM metrics answer 501 |
| `POST /containers/{id}/rename?name=` | `docker rename` | a name already in use answers 409 |
| `GET /containers/{id}/logs`, `POST /containers/{id}/attach` | `docker logs`, `attach` | console is one combined stream |
| `POST /containers/{id}/exec`, `POST /exec/{id}/start`, `GET /exec/{id}/json` | `docker exec` | no stdin |
| `DELETE /containers/{id}` | `docker rm [-f]` | |
| `GET /events` | `docker events` | container `create`, `start`, `die`, `stop`, `pause`, `unpause`, `restart`, `destroy` from the daemon's event log; `since`/`until` in Unix seconds; filter by `event`, `container` (name or ID prefix), `type` or `scope`. Repeated values within a filter are alternatives; different keys must all match |
| `GET /images/json`, `POST /images/create`, `DELETE /images/{name}` | `docker images`, `pull`, `rmi` | |
| `GET /networks`, `GET /networks/{id}` | `docker network ls`, `inspect` | one network, `fluxvm-nat` (every container is a microVM behind user-mode NAT; publish ports with `-p`). Create, connect and remove answer 501 |
| `GET /volumes`, `GET /volumes/{name}` | `docker volume ls`, `inspect` | always empty: there are no named volumes (a container's disk lives and dies with it). Create answers 501 |
| `POST /build` | `docker build` | macOS image builds via the Mac builder (details below); Linux Dockerfile builds require the managed engine |

Not run against the real `docker` CLI against a live daemon by the tests: they call the handlers in process with fakes.

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

### Run Docker commands through fluxctl

After `fluxctl docker up`, issue Docker commands directly. FluxVM invokes the Docker CLI installed on the host and selects the managed engine socket for that process. Your active Docker context stays unchanged.

```sh
fluxctl docker run --rm -it alpine sh
fluxctl docker compose up --build --wait
fluxctl docker buildx ls
fluxctl docker volume ls
fluxctl docker network inspect bridge
fluxctl docker system df
```

Install client plugins such as Compose or Buildx **on the host** to use their commands. Docker decides which features the running daemon, builder, registry credentials and installed plugins support. Registry operations use the host client's Docker configuration. A plugin may use its own configured endpoint, such as a selected Buildx builder.

FluxVM reserves `up`, `down`, `status`, `env`, `context`, `builds`, `agent`, `supervise` and `cli`. Use `cli --` to pass a reserved name or Docker global options, or supply the socket of a second managed VM:

```sh
fluxctl docker cli -- context ls
fluxctl docker cli --socket /tmp/project-engine.sock -- info
fluxctl docker cli -- --debug version
```

### Docker context (`docker --context fluxvm`)

`up` also creates a Docker CLI **context** named `fluxvm` that points at the engine socket, so you do not need `DOCKER_HOST`:

```bash
docker context ls                 # lists fluxvm next to default
docker --context fluxvm ps        # one command against FluxVM's engine
fluxctl docker context install --use   # make it the default for `docker` (docker context use fluxvm)
fluxctl docker context status     # exists? current? does the socket answer? what to run next
fluxctl docker context uninstall  # remove it; if it was current, docker returns to its default context
```

Without `--use` your current context is not changed. FluxVM writes only `~/.docker/contexts/meta/<sha256 of "fluxvm">/meta.json`
(and `currentContext` in `config.json` for `--use`; `$DOCKER_CONFIG` replaces `~/.docker` when set). It never reads, changes or
removes a context with another name, keeps every other key in `config.json`, and writes through a temp file and a rename. If
`config.json` is not valid JSON it is left alone and the error says how to fix it (`fluxctl explain DOCKER_CONFIG_INVALID`).
If the `docker` command is not installed, nothing is written and the message gives the install hint (`brew install docker` on a Mac,
`sudo apt install docker.io` on Debian-family Linux); run `context install` once it is. `DOCKER_HOST` and `DOCKER_CONTEXT` in your
environment override the current context; `status` says so. `--socket PATH` points the context at a different socket.

In Studio, the Docker page has a Docker context card with the same state and a button that copies
`fluxctl docker context install --use`; Studio only shows the state, the command is run in a terminal. The daemon reports on the
config directory of the user it runs as.

### Bind mounts

`--share HOST[:GUEST][:ro]` shares a Mac folder into the VM with virtiofs. **The guest path defaults to the host path**, so a
command run on the Mac such as `docker run -v "$PWD/conf:/etc/conf" …` or a Compose `volumes: ["./conf:/etc/conf"]` names a path
that exists, with the same content, inside the VM where `dockerd` runs. A path outside a shared folder is empty in the guest.
Shares are fixed when the VM is created; to add one, `down --purge` and `up` again (or use another `--name`).

### Reach a container's port from the Mac

A container published as `-p 127.0.0.1:8080:80` listens on the VM's loopback. Add `--publish 8080` (or `HOST:GUEST`) to `up` and
the Mac's `127.0.0.1:8080` reaches it over the engine's SSH connection:

```bash
fluxctl docker up --publish 8080
docker run -d -p 127.0.0.1:8080:80 nginx && curl -s http://127.0.0.1:8080/ | head -1
```

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
3. It starts a detached `ssh -N -L {socket}:/var/run/docker.sock` to the guest (with keepalives, plus a `-L` per `--publish`) and
   records its pid and settings next to the socket. The socket is `{run_dir}/docker-engine.sock`, beside the built-in `docker.sock`; `--socket` overrides it.

Nothing listens on a TCP port, and the guest's Docker socket is only reachable by your user through the SSH key.

## If the first boot stalls

The first boot installs packages with apt, so it needs this Mac to be online. If the VM never reports an address, or Docker is not ready in
15 minutes, the command stops and says so; for a guest that is up it includes what cloud-init says in the guest (`cloud-init status` and the end of
`/var/log/cloud-init-output.log`). `fluxctl serial NAME` shows the guest's console. Fix the network and run the same command again: it picks the VM
up where it was.

## In Studio

The **Docker** page (Platform, in the node Studio) shows the engine the daemon can reach at `/tmp/fluxvm-run-<uid>/docker-engine.sock`, the socket
`fluxctl docker up` publishes for the user the daemon runs as: its version, the containers (state, status as Docker words it, published ports,
age, with a filter and a running or not-running switch) and the images (tags, size, containers using them), and a container's log in the same
colourful, filterable window as a pod's. Each container has **Start**, **Stop** and **Restart** buttons (a stop waits for the container's grace period, 10 s by default, then kills it; a toast says what happened, including "already stopped"). Nothing else is written to the engine: no create, remove or pull, which stay with the docker CLI. It is admin only: the containers belong to the whole host. When there is no engine VM
or its socket does not answer, the page says so and names the command to run instead of failing. The routes are `GET /v1/docker` and
`GET /v1/docker/containers/{id}/logs?tail=N` and `POST /v1/docker/containers/{id}/{start|stop|restart}` (in the daemon's OpenAPI document; the request log records each action). A daemon that was given a different `--socket` for `fluxctl docker` does not see
that engine.

## Limits

- One engine VM per name; it uses its vCPUs and memory while it runs.
- The containers inside share the VM's kernel and resources. For strong per-container isolation use the built-in
  [container sandboxes](oci-sandboxes.md).
- Only the folders you `--share` exist in the guest. Docker volumes live on the VM's disk and go away with `down --purge`.
- Published container ports are on the guest's loopback, not the Mac's. Forward the ones you need with `--publish HOST[:GUEST]`
  (repeatable; Mac side is always 127.0.0.1), for example `fluxctl docker up --publish 18443 --publish 18081`. Changing
  `--publish` restarts the forward without touching the VM.
- macOS with Apple silicon only (it uses a `vz` VM).

Live check: `scripts/docker-engine-live-test.sh`.

Need Kubernetes instead? `fluxctl kube up` does the same for a single-node k3s cluster: [kubernetes-on-mac.md](kubernetes-on-mac.md).

## `docker build` on the built-in socket

On a Mac the built-in `docker.sock` forwards `docker build` to the FluxVM image builder (`fluxctl mac build`), so one command builds a
macOS image from a Macfile (pass `-f Macfile`: the docker CLI looks for `Dockerfile` itself before it uploads anything; a `Dockerfile` written in Macfile syntax works without `-f`).

```sh
fluxctl docker context install --builtin --use     # point docker at the built-in socket (repair a dead one: fluxctl docker context repair)
DOCKER_BUILDKIT=0 docker build -f Macfile -t app:1 --build-arg VER=3 .
fluxctl docker builds                              # the history, same as Studio > Images > Builds
```

What is honoured: `-t` (several), `-f`, `--build-arg`, `--label`, `--target`, `--platform`, `--no-cache`, `-q`, `.dockerignore`. The docker CLI
(v29 with the buildx plugin, the only one tried) builds with BuildKit unless `DOCKER_BUILDKIT=0` is set, even though `/_ping` says
`Builder-Version: 1`; its default builder then tries to pull and run `moby/buildkit`, which the socket refuses at once. BuildKit
(`docker buildx build`, `--secret`, `--ssh`) is refused with the sentence `DOCKER_BUILDKIT=0 docker build ...` or `fluxctl mac build --secret ... --ssh default`. Flags that
cannot apply to a macOS VM build (`--memory`, `--cpuset-cpus`, `--squash`, `--add-host`, `--network`, `--cache-from`, `--output`, git or
URL contexts) are refused before the upload with the `fluxctl` equivalent. The new image's id is the store digest as `sha256:...`;
`docker images` lists it. `docker run` of that image is not wired: it answers `fluxctl mac run NAME`.

The history (tags, flag names, outcome, time, image id; never a build-arg value) is kept in `docker-builds.json` next to the socket.
