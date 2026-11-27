# Docker Engine API adapter (`FLUXVM_DOCKER_API`)

A small, opt-in Docker Engine API endpoint over the FluxVM control plane. Docker SDKs, `curl` and the basic
`docker` CLI commands can list, create, start, stop and read the logs of containers. Each container is an
**OCI sandbox: one microVM**, the same records `fluxctl sandbox` and the built-in `docker.sock` show.

It lives in two places:

| Piece | Where | What it does |
|---|---|---|
| HTTP surface | `crates/fluxvm-docker-api` (`router`, `serve`, `ListenAddr`) | Docker routes, request and response shapes, `/v1.NN` prefixes, error bodies, log framing. It knows nothing about VMs |
| Control-plane wiring | `crates/fluxvm-api/src/docker_adapter.rs` (`FluxvmAdapter`) | Implements the `DockerAdapter` trait with real `VmManager` calls (`list`, `create_sandbox`, `start`, `stop`, `sandbox_logs`, the console file) |

The create mapping (`Image`/`Cmd`/`Env`/`WorkingDir` → `SandboxCreateRequest`) and the ID/name lookup are
**shared with the `docker.sock` shim** ([docker-engine.md](docker-engine.md)), so the two Docker surfaces always
agree.

## How it compares

| | `FLUXVM_DOCKER_API` (this) | Built-in `docker.sock` | `fluxctl docker` |
|---|---|---|---|
| Default | off everywhere | on on macOS (`docker_sock`), off on Linux | off until `fluxctl docker up` |
| Listener | unix socket or loopback TCP that you pick | `{run_dir}/docker.sock` | a socket forwarded from a VM |
| Surface | list, create, inspect, start, stop, logs | a larger subset (exec, pull, events, stats, build…) | the full engine (real `dockerd`) |
| Containers | one microVM each | one microVM each | ordinary containers in one VM |

Use this adapter when you want a minimal, well-tested Docker endpoint, for example on Linux where `docker.sock` is
off or for an SDK-driven tool, and the bigger surfaces when you need more.

## Turn it on

The daemon starts the adapter only when `FLUXVM_DOCKER_API` is set when it starts:

```bash
# unix socket (created with mode 0600)
FLUXVM_DOCKER_API=unix:///run/fluxvm/docker-api.sock fluxctl daemon …
export DOCKER_HOST=unix:///run/fluxvm/docker-api.sock

# or loopback TCP
FLUXVM_DOCKER_API=tcp://127.0.0.1:2375 fluxctl daemon …
export DOCKER_HOST=tcp://127.0.0.1:2375
```

For a systemd unit, put `Environment=FLUXVM_DOCKER_API=unix:///run/fluxvm/docker-api.sock` in a drop-in.

Accepted values: `unix:///abs/path`, `unix:/abs/path`, `/abs/path`, `tcp://127.0.0.1:PORT`, `127.0.0.1:PORT`,
`[::1]:PORT`. The Docker API has **no authentication**, so:

- non-loopback TCP addresses (`0.0.0.0`, LAN IPs) are refused;
- the socket is `0600`; a stale socket at the path is replaced, any other file there is left alone and the adapter
  does not start;
- the `docker_sock` path is refused, so the adapter never replaces the built-in shim's socket.

A bad value is logged (`Docker API adapter not started`) and the daemon keeps running.

Then:

```bash
docker version
docker create --name web nginx:alpine
docker start web
docker ps            # docker ps -a, docker ps -n 2
docker logs web      # --tail N, -f
docker stop web
curl --unix-socket /run/fluxvm/docker-api.sock http://d/v1.44/containers/json?all=1
```

## Endpoints

Every path also works under a version prefix (`/v1.44/containers/json`). Responses carry `Api-Version: 1.44`,
`Ostype: linux` and `Server: FluxVM`. Errors are Docker-shaped: `{"message": "..."}`.

| Endpoint | Docker command | FluxVM call | Notes |
|---|---|---|---|
| `GET`/`HEAD /_ping` | handshake | none | `OK`, `text/plain` |
| `GET /version` | `docker version` | none | `ApiVersion` 1.44, `MinAPIVersion` 1.24, `Os` linux |
| `GET /containers/json` | `docker ps [-a] [-n N]` | `VmManager::list`, OCI sandboxes only | newest first. Without `all` only running and paused containers; a positive `limit` returns the newest N in any state. Non-empty `filters` answer 501 |
| `POST /containers/create?name=` | `docker create` | `VmManager::create_sandbox` | name rule `[a-zA-Z0-9][a-zA-Z0-9_.-]+` (else 400); a name in use answers 409; no `Image` answers 400; `platform` answers 501 |
| `GET /containers/{id}/json` | `docker inspect` (the CLI also calls it before `logs`) | `resolve_container` + `sandbox_logs` | `State`, `Config.Image`, `Config.Tty` (always false), exit code once the process exited |
| `POST /containers/{id}/start` | `docker start` | `VmManager::start` | 204; 304 when it is already running or paused |
| `POST /containers/{id}/stop?t=` | `docker stop` | `VmManager::stop` (graceful guest shutdown, then the VMM is killed) | 204; 304 when it is already stopped. `t` is accepted and not used; `signal` answers 501 |
| `GET /containers/{id}/logs` | `docker logs [-f] [--tail N]` | `sandbox_logs(tail)`, then the console file for `follow` | see below |

`{id}` is a full ID, a unique ID prefix or a name (with or without `/`). An exact name wins over an ID prefix, as in
Docker. Unknown containers answer `404 No such container: X`. Any other path or method answers **501** with a
message that lists what is supported.

### Request and response mapping

| Docker | FluxVM |
|---|---|
| `Image` | `oci.image` (pulled into the OCI cache by `create_sandbox`) |
| `Cmd` | `oci.command` |
| `Env` (`["K=V"]`) | `oci.env` |
| `WorkingDir` | `oci.workdir` |
| `name` query | sandbox name |
| `HostConfig.Binds` | refused (400), as in `docker.sock` |
| `Tty: true` | refused (501): the sandbox console is a plain stream |
| `Id` | sandbox UUID without dashes (32 hex characters) |
| `Names` | `["/" + name]` |
| `Image`, `ImageID` | the `OCI_IMAGE_LABEL` and OCI cache labels |
| `Created` | `created_at` (Unix seconds) |
| `State` | `Running`→`running`, `Paused`→`paused`, `Creating`→`created`, `Stopped`/`Failed`→`exited` |
| `Status` | `Up`, `Up (paused)`, `Created`, `Exited`, `Exited (error)` |
| `Labels` | the sandbox labels |

### Logs

- `stdout`/`stderr`: at least one is required (400 otherwise, like Docker). The FluxVM console is **one combined
  stream**, reported as stdout; `stderr=1` alone returns nothing.
- `tail`: `all` (default, at most 10 000 lines), `N`, or `0` for none. Bad values answer 400.
- `follow`: sends the tail, then whatever the console gains, until the process exits or the VM stops. It reads the
  file after `stdout` was cut, so a line written between the two reads can appear twice.
- `timestamps`, `since`, `until`: 501 (FluxVM does not record per-line times). `since=0` (the CLI default) is fine.
- Framing: `application/vnd.docker.multiplexed-stream` with the 8-byte Docker header per chunk
  (`[1|2, 0, 0, 0, len_be32]`); a TTY container would get the raw bytes as `application/vnd.docker.raw-stream`.

## Lifecycle notes

- `create_sandbox` **boots** the microVM, so a created container is running before `docker start`. `start` on a
  container that is still booting answers 204 so `docker run`-style create-then-start works.
- `docker rm`, `exec`, `attach`, `pull`, `images`, `kill`, `restart`, `wait` and events are not part of this
  adapter (501). The built-in `docker.sock` has most of them; see [docker-engine.md](docker-engine.md).

## Tests

- `crates/fluxvm-docker-api`: handler tests through the full router (`tower::oneshot`) against an in-process,
  test-only fake that implements `DockerAdapter`: one or more per endpoint, plus version prefixes, error bodies,
  501s, log framing (multiplexed and TTY), log parameter errors, listener parsing and the "never remove a
  non-socket" rule.
- `crates/fluxvm-api/src/docker_adapter.rs`: `FluxvmAdapter` against a **real `VmManager`** over a temporary
  store with sandbox records: list filtering and order, the router over the real control plane, name conflicts,
  the shared create mapping (bind mounts refused), start/stop no-op transitions, 404s, log tail/follow from a
  console file, and the `docker_sock` collision check.
- Two tests that boot a real sandbox (`create` and `start` end to end) are `#[ignore]`: they need a host with a
  VM backend. Run them there with `cargo test -p fluxvm-api docker_adapter -- --ignored`.

## Gaps and next steps

1. `docker rm`, `wait`, `kill`, `restart` and `exec` through `DockerAdapter`, and moving the matching
   `docker.sock` handlers onto the same trait so there is one implementation.
2. Separate stdout/stderr: the guest's init would have to tag the two streams on the console or over vsock.
3. TTY containers, and `attach` with stdin.
4. `filters` for `GET /containers/json` (`status`, `name`, `label`).
5. A `--docker-api-listen` flag and a config key as well as the environment variable; `fluxctl docker-context`
   support for this endpoint.
6. `GET /images/json` and `POST /images/create` (the OCI cache already supports both through `docker.sock`).
7. Honour `stop?t=` by passing a shutdown timeout to the scheduler.
