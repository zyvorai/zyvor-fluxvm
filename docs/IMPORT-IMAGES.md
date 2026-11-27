# Importing container images from OrbStack or Docker Desktop

FluxVM's native container path pulls an image from an **OCI registry** and
caches a rootfs under `<state_dir>/oci`. It does not read a `docker save`
archive, an OCI layout directory, or a local tarball path as the `image`
field of a sandbox. There is no `fluxctl oci import` or `fluxctl image load`.

If the image already lives in [OrbStack](https://docs.orbstack.dev/docker) or
[Docker Desktop](https://docs.docker.com/desktop/), export it with the Docker
CLI those products install, publish it to a registry FluxVM can reach, then
pull it with FluxVM. This document uses only commands that exist today and
names the gaps where a command is missing.

For the native run contract see [oci-sandboxes.md](oci-sandboxes.md) and
[DOCKER-FREE.md](DOCKER-FREE.md). For a full `dockerd` in a FluxVM VM see
[docker-engine.md](docker-engine.md). `fluxctl import-image` /
`POST /v1/images/import` import **VM disks** (OVA, OVF, VMDK, VHD(X), qcow2),
not container images; that path is [import-vmware.md](import-vmware.md).

## What FluxVM actually accepts

| Goal | What exists | What does not |
|---|---|---|
| Run the image as a one-VM-per-container sandbox | A registry reference: `fluxctl oci pull REF [--platform linux/arm64\|linux/amd64]`, `fluxctl sandbox run REF`, or `POST /v1/oci/images` with `{"image":"REF","platform":"linux/arm64"}` (admin). `docker pull REF` also works against FluxVM's built-in `{run_dir}/docker.sock` (`POST /images/create`). | `fluxctl oci pull` of a file path, `oci:/path`, or `docker-archive:…`. `docker load` / `POST /images/load` on the built-in socket (501; the message points at `fluxctl docker up`). |
| Load the image into a `fluxctl kube` k3s cluster | `fluxctl kube load IMAGE…` (runs `docker save -o` from the **current** Docker CLI context, then `k3s ctr images import` on each node) or `fluxctl kube load --archive FILE`. | A FluxVM-native OCI-cache import from that archive. The cluster's containerd is separate from the sandbox cache. |
| Load the image into the compatibility engine | After `fluxctl docker up` and `eval "$(fluxctl docker env)"`, `docker load -i FILE` works, because that socket is a real `dockerd`. | That image does not appear in `fluxctl oci ls` and is not used by `fluxctl sandbox run`. |
| Inspect or retag in a registry | `fluxctl image inspect\|tags\|tag REF` talks to the registry HTTP API ([oci-registry-tools.md](oci-registry-tools.md)). | Those commands do not ingest a tarball. `fluxctl image push` / `pull-disk` move a **VM disk** (raw or qcow2), not a container image. |

`ImageRef` parsing only accepts a registry name (`alpine:3.22`,
`ghcr.io/org/app:1.2`, `localhost:5000/app:dev`, or a `@sha256:…` digest).
Docker Hub short names are expanded (`alpine` → `registry-1.docker.io/library/alpine:latest`).
A path such as `./app.tar` is not a valid reference.

Plain HTTP is used only when the registry host **starts with** `localhost` or
`127.0.0.1` (a port is fine: `localhost:5000`). Every other host, including
`[::1]`, a LAN IP, or `host.docker.internal`, is HTTPS, and certificates are
verified. FluxVM has no `--insecure-registry` or skip-TLS flag on the puller.

## Talk to the right Docker engine

OrbStack and Docker Desktop both expose a Docker CLI and a Docker context.
OrbStack also ships `orb` / `orbctl`, but those manage OrbStack itself (Linux
machines, `orb docker migrate` from Desktop). There is no `orb save` for
images: use `docker`.

```bash
docker context ls
docker context show
```

Typical context names and sockets (always confirm with `docker context ls`
or `docker context inspect NAME --format '{{ .Endpoints.docker.Host }}'`):

| Product | Context name | Typical socket |
|---|---|---|
| OrbStack | `orbstack` | `unix://$HOME/.orbstack/run/docker.sock` |
| Docker Desktop (Mac) | `desktop-linux` | `unix://$HOME/.docker/run/docker.sock` (Desktop may also symlink `/var/run/docker.sock`) |
| Docker Desktop (Linux) | `desktop-linux` | `unix://$HOME/.docker/desktop/docker.sock` |

Use one command against a specific engine, or switch the current context:

```bash
docker --context orbstack images
docker --context desktop-linux images
docker context use orbstack          # or: docker context use desktop-linux
```

`DOCKER_HOST` and `DOCKER_CONTEXT` override the current context. This matters
on a FluxVM machine: `eval "$(fluxctl docker env)"` exports `DOCKER_HOST` to
FluxVM's managed engine socket. After that, `docker save` / `docker images`
no longer see OrbStack or Docker Desktop. Unset `DOCKER_HOST` (or pass
`--context orbstack` / `--context desktop-linux`) before exporting.

## Preferred path: push to a registry, then pull with FluxVM

If the image can be pushed to a registry the FluxVM daemon can reach, do that
and skip the tarball.

### Local registry on loopback (HTTP)

A registry published on the Mac at `localhost:5000` is the straightforward
offline option, because FluxVM will speak HTTP to it.

```bash
# Run the registry with the same engine that holds the image (example: OrbStack)
docker --context orbstack run -d --restart unless-stopped --name fluxvm-registry \
  -p 5000:5000 registry:2

docker --context orbstack tag myapp:dev localhost:5000/myapp:dev
docker --context orbstack push localhost:5000/myapp:dev

fluxctl oci pull localhost:5000/myapp:dev
# or, later, just:
fluxctl sandbox run localhost:5000/myapp:dev --rm -- uname -m
```

Replace `--context orbstack` with `--context desktop-linux` for Docker Desktop.
Docker's client treats `localhost` as an insecure registry by default, so the
push should not need extra daemon config.

Do not push to `127.0.0.1:5000` from Docker and pull as `localhost:5000` from
FluxVM if you care about the exact name you tagged; use the same host string
on both sides. A tag of `192.168.1.10:5000/myapp:dev` will make FluxVM use
HTTPS and fail unless that registry has a valid certificate.

Private registries other than anonymous loopback need
`[[apple.oci_registry_credentials]]` (username plus `password` or
`keychain_service`). See [oci-sandboxes.md](oci-sandboxes.md#configuration).

### A hosted registry (GHCR, Docker Hub, …)

```bash
docker --context orbstack tag myapp:dev ghcr.io/org/myapp:dev
docker --context orbstack push ghcr.io/org/myapp:dev
fluxctl oci pull ghcr.io/org/myapp:dev
```

Configure credentials as above before the FluxVM pull.

## Workaround: `docker save`, convert, then push

Use this when you want a file you can copy, or when you would rather not
`docker push` from Desktop or OrbStack.

`docker image save` and `docker save` are the same command
([Docker CLI](https://docs.docker.com/reference/cli/docker/image/save/)).
Write an uncompressed tar (`fluxctl kube load` also uses `docker save -o`):

```bash
docker --context orbstack image inspect myapp:dev --format '{{.Os}}/{{.Architecture}}'
docker --context orbstack save -o myapp.tar myapp:dev
```

`--platform os/arch` on `docker image save` exists from Engine API 1.48 and
saves only variants already in the local store. On an older CLI, pull the
platform you want first (`docker pull --platform linux/arm64 myapp:dev`), then
save. Do not use `docker export`: that flattens a *container's* filesystem
and drops the image config (entrypoint, env, layers).

FluxVM cannot ingest `myapp.tar` itself. Copy it into a registry with
[skopeo](https://github.com/containers/skopeo/blob/main/docs/skopeo-copy.1.md)
or [crane](https://github.com/google/go-containerregistry/blob/main/cmd/crane/doc/crane_push.md):

```bash
# Optional: confirm architecture and config before pushing
skopeo inspect docker-archive:myapp.tar

# Optional intermediate: an OCI layout directory (tag after the path is required)
skopeo copy docker-archive:myapp.tar oci:./myapp-oci:dev

# Into the local registry (HTTP: disable TLS verify on the destination)
skopeo copy --dest-tls-verify=false \
  docker-archive:myapp.tar \
  docker://localhost:5000/myapp:dev

# Same job with crane. --insecure allows HTTP. A directory PATH is an OCI
# layout; a file PATH is a docker-style tarball (including docker save).
# If the tar contains more than one image, add --index.
crane push --insecure myapp.tar localhost:5000/myapp:dev

fluxctl oci pull localhost:5000/myapp:dev
```

`docker-archive:path[:reference]` is the transport for a `docker save` file;
`oci:path:tag` is an OCI layout directory. Keep the archive uncompressed
unless you have checked that your skopeo or crane build accepts gzip.

## k3s: `fluxctl kube load`

To run the image in a `fluxctl kube` cluster, you do **not** need the native
OCI cache. From the engine that already has the image:

```bash
# Current context must be OrbStack or Docker Desktop, not FluxVM's socket
unset DOCKER_HOST
docker context use orbstack          # or desktop-linux
fluxctl kube load myapp:dev
# or, after docker save -o myapp.tar myapp:dev:
fluxctl kube load --archive myapp.tar
```

Give the image a tag other than `:latest`, or set `imagePullPolicy` to
`IfNotPresent` or `Never`. Details and the Intel/Rosetta caveat are in
[kubernetes-on-mac.md](kubernetes-on-mac.md#images). Pass images *or*
`--archive`, not both.

## Compatibility engine only: `docker load`

```bash
fluxctl docker up
eval "$(fluxctl docker env)"
docker load -i myapp.tar
docker images
```

This is a real `dockerd`. It is the right path if you only need `docker run` /
Compose inside that VM. It is not an import into FluxVM sandboxes. The
built-in `{run_dir}/docker.sock` used by `fluxctl serve` still does not
implement `docker load`.

## Multi-arch and platform

On Apple silicon, OrbStack and Docker Desktop default to `linux/arm64`.
That matches FluxVM's default for `fluxctl oci pull` and `fluxctl sandbox run`
(`linux/arm64`). Accepted values are `linux/arm64`, `linux/arm64/v8`,
`aarch64`, `linux/amd64`, `amd64`, `x86_64`, and `x86-64`.

- `fluxctl oci pull` / `POST /v1/oci/images` **default to `linux/arm64` even
  on an x86-64 Linux host**. If you are pulling on that host, pass
  `--platform linux/amd64` (or set `platform` in the JSON). `fluxctl sandbox
  run` on Linux, when `--platform` is omitted, uses the host architecture
  instead; the two commands are not the same.
- `fluxctl sandbox run --platform linux/amd64` (and the same flag on
  `fluxctl oci pull`) pulls the amd64 manifest and runs it under Rosetta on a
  Mac. Rosetta must be installed (`fluxctl vz doctor` checks). The amd64 and
  arm64 variants of one tag have different manifest digests and therefore
  separate cached rootfs files.
- A registry image that has no variant for the platform you asked for fails
  with `{ref} has no linux/{arch} image` before anything is cached.
- `docker save` writes whatever is already in that engine's image store. A
  new enough engine may store several platforms under one tag; FluxVM still
  selects a single platform at pull time. Prefer saving or pushing one
  platform, or build and push a multi-arch index and let FluxVM pick.
- `fluxctl kube up --rosetta` lets amd64 *binaries* run in the cluster VM.
  Kubernetes may still refuse to *pull* an amd64-only image for an arm64
  node; `kube load` sidesteps that pull. See
  [kubernetes-on-mac.md](kubernetes-on-mac.md#intel-only-images-rosetta).

Check before you export or pull:

```bash
docker --context orbstack image inspect myapp:dev --format '{{.Os}}/{{.Architecture}}'
skopeo inspect docker-archive:myapp.tar
fluxctl image inspect localhost:5000/myapp:dev -o table
```

## Verifying the import

```bash
fluxctl oci ls
fluxctl sandbox run localhost:5000/myapp:dev --rm -- uname -m
```

`fluxctl oci ls` lists cached rootfs entries (manifest digest, references,
architecture, size). A successful `sandbox run` of a known command is the
real check that the layers unpacked and the image config (entrypoint, env,
user) was applied. Compare `uname -m` with the platform you pulled
(`aarch64` for arm64, `x86_64` for amd64 under Rosetta).

`fluxctl image inspect` talks to the registry, not the local cache: use it to
confirm the push landed before `oci pull`. After a `kube load`, use
`kubectl` / `crictl` on the cluster, not `fluxctl oci ls`.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `docker save` / `docker images` does not list the image you built in OrbStack or Desktop | Wrong context, or `DOCKER_HOST` still points at FluxVM (`echo $DOCKER_HOST`; `docker context show`). |
| `docker load` against FluxVM's default `{run_dir}/docker.sock` fails with 501 | The built-in shim has no `/images/load`. Push to a registry and `fluxctl oci pull`, or use `fluxctl docker up` if you wanted the compatibility engine. |
| `invalid image reference` or `invalid repository` on `fluxctl oci pull ./app.tar` | FluxVM only accepts registry references. Push the tar with skopeo or crane first. |
| `{ref} has no linux/arm64 image` | You saved or pushed an amd64-only image and pulled with the default platform. Pass `--platform linux/amd64` (and have Rosetta, on a Mac), or export the arm64 variant. |
| HTTPS or certificate errors against a local registry | The registry host is not `localhost` / `127.0.0.1`, or you used `[::1]`. Retag to `localhost:PORT` or give the registry a real certificate. FluxVM cannot skip TLS. |
| Registry auth errors on GHCR or a private host | Add `[[apple.oci_registry_credentials]]` and restart or retry the pull. Anonymous pull has no credentials. |
| `exec format error` in a kube pod | An amd64 image on an arm64 node without `kube up --rosetta`, or the other way around. Check the image's architecture and the node. |
| Layers extract to a Docker `manifest.json` plus layer tars, not a rootfs | A `docker save` archive was unpacked as if it were a rootfs tarball. That is not an ingestion path for OCI sandboxes. Use skopeo/crane into a registry. |

## Gaps

These are missing today; the workarounds above are the supported path.

- No command or API to load a `docker-archive` or an OCI layout into the
  native OCI cache (`fluxctl oci pull` and `POST /v1/oci/images` are
  registry-only).
- The built-in Docker Engine API does not implement `POST /images/load`
  (`docker load`).
- The puller has no insecure-registry setting, no TLS skip, and no HTTP
  exception for `[::1]` or LAN addresses.
- `fluxctl oci pull` defaults to `linux/arm64` on every host; it does not
  follow the host architecture.
- `POST /v1/templates` can build a **sandbox VM template** from a registry
  reference (via `skopeo` + `umoci`) or from a local *rootfs* tarball. That is
  not the OCI sandbox cache, there is no `fluxctl` subcommand for it, and a
  `docker save` archive is not a rootfs tarball.
- Image *builds* remain a Docker-free gap (`fluxctl sandbox build` is the
  planned path). Build in OrbStack, Docker Desktop, or `fluxctl docker up`,
  then import as above.

