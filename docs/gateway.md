# Gateway API on FluxVM

`fluxctl gateway` publishes a Kubernetes Gateway listener on your Mac or Linux workstation and shows which application answers. It works with Envoy Gateway on any cluster `kubectl` can reach, and FluxVM can install Envoy Gateway on its own k3s cluster.

This is a port of **Zyvor Setura 0.1.0**, the Python developer CLI from Zyvor, into `fluxctl` (Rust, no Python needed). The commands, the probe's behaviour and the demo fixture follow Setura; credit for the design is Setura's.

> Status: unit-tested with loopback HTTP and TLS servers and a fake `kubectl`, and **run once on a live FluxVM cluster** (a single-node k3s v1.37.2 in a QEMU/KVM guest on an x86-64 Linux host, Envoy Gateway v1.10.0): `platform envoy-gateway`, `demo apply`, `publish`, `explain`, `check`, `inspect` and `demo teardown` all worked after the two fixes described under [What the live run found](#what-the-live-run-found). Not run: macOS guests, HTTPS listeners, a second cluster, MetalLB.

## Quick start: one command

```
fluxctl gateway expose web -n shop --cluster dev
```

This is all most people need. For the Service `web` in namespace `shop` it:

1. checks that Envoy Gateway is on the cluster. If not it asks one question (`Install it now? [y/N]`); `--yes` skips the question. With no terminal and no `--yes` it fails and prints the exact command to run. On a FluxVM cluster (`--cluster NAME`) it installs the pinned add-on; on another cluster it prints the `helm install` command instead.
2. creates a GatewayClass `fluxvm-envoy` only if the cluster has no GatewayClass for Envoy Gateway, then an EnvoyProxy (ClusterIP), a Gateway `fx-web` and an HTTPRoute `fx-web` in `shop`, routing `web.localhost` to the Service. Every object has the label `app.kubernetes.io/managed-by=fluxctl-gateway`.
3. waits for the Gateway to be Programmed, picks the first free local port from 8080 (a real bind, not a guess; `--local-port N` to choose, and a taken port is an error that says so), and prints the URL and a `curl --resolve` line.
4. forwards until Ctrl-C. If the proxy pod is replaced the forward ends; fluxctl says so on stderr, finds the Service again and reconnects with a backoff (it gives up after a dozen quick failures in a row).

```
fluxctl gateway list                         # what fluxctl exposed: host, route accepted?, gateway ready?
fluxctl gateway unexpose web -n shop         # remove it
fluxctl gateway expose web -n shop --dry-run # print the objects and commands, run nothing
```

Options: `--port SERVICE_PORT` (needed when the Service has several), `--host NAME` (default `SERVICE.localhost`), `--cluster NAME` or `--kube-context CTX`. `unexpose` and `list` only ever look at, or delete, objects with the managed-by label above; an object named `fx-web` that you made yourself is left alone, and `expose` refuses to overwrite it. `unexpose` keeps the shared GatewayClass and Envoy Gateway. Each exposed Service gets its own Envoy proxy.

`*.localhost` names resolve to this machine in most browsers; fluxctl has not verified that for any particular one. If yours does not, use the printed `curl --resolve HOST:PORT:127.0.0.1 http://HOST:PORT/`.

Errors say what to do next: no `kubectl` (how to install it), no reachable cluster (`--cluster` / `--kube-context`), Envoy Gateway missing and declined, Service not found (`kubectl -n NS get services`), port in use.

> Status of `expose`, `unexpose`, `list` and the `publish` changes below: written with a fake kubectl and loopback-socket unit tests; **not run against a live cluster**.

## Install Envoy Gateway on a FluxVM k3s cluster

```
fluxctl kube cluster create dev                           # on a terminal: asks "Install the Envoy gateway so you can reach apps by name? [Y/n]"
fluxctl kube cluster create dev --gateway                 # install it, no question (scripts)
fluxctl kube cluster create dev --no-gateway              # do not install it, no question
fluxctl kube cluster create dev --addons envoy-gateway    # the same as --gateway, by add-on name
fluxctl kube cluster platform envoy-gateway dev           # or later, on an existing cluster
```

The question is asked only when stdin is a terminal and neither `--gateway`, `--no-gateway` nor `--addons` was given. With no terminal and no flag (CI, scripts) nothing is asked and no gateway is installed, exactly as before; a script that passes `--addons` is never changed. `--gateway` cannot be combined with another `--addons` profile (a profile is one choice) and `--gateway --no-gateway` is refused with a message saying which to drop. The choice is recorded in the `platform.choices` list of the JSON `create` prints, for example `gateway: installed (you chose yes when asked)`, or `gateway: not installed; add it later with fluxctl kube cluster platform envoy-gateway dev`. `kube apple` is the old name of `kube cluster` and takes the same flags. `fluxctl kube up` (the single k3s VM) has no add-on profiles and is unchanged.

If you skip it, nothing is lost: `fluxctl gateway expose SERVICE` offers to install Envoy Gateway the first time (`--yes` to skip the question), and the health code `ENVOY_GATEWAY_NOT_INSTALLED` in `fluxctl cluster status` / `fluxctl fix` names the one command that fixes it: `fluxctl kube cluster platform envoy-gateway NAME`. In Studio, the Gateways tab of the Kubernetes page shows "Envoy Gateway is not installed on this cluster" with that command and a "Copy as fluxctl" button when the controller is missing.

Both install the release `install.yaml` of Envoy Gateway (Gateway API CRDs, Envoy Gateway CRDs, the controller in `envoy-gateway-system`) with server-side apply, then wait for `deployment/envoy-gateway` to be Available. It is the same pinned, checksum-verified path as Flux, Argo CD, MetalLB, cert-manager and metrics-server:

| Pin | Value |
|---|---|
| Version | v1.10.0 |
| URL | `https://github.com/envoyproxy/gateway/releases/download/v1.10.0/install.yaml` |
| SHA-256 | `f87f4d59b983aa760da52a2a0002c612028106fe69beefdf3d30cfb03905944e` |

The digest was computed from a download of that URL on 2026-11-22 and equals the digest GitHub lists for the release asset. The manifest is checked before `kubectl apply`; a different file is a hard error. For an air-gapped host use `--manifest-file FILE [--sha256 HEX]` on `platform envoy-gateway`. `fluxctl kube apple platform pins` lists every pin. The images that manifest runs are pinned by tag inside upstream's file, not by us.

The add-on creates no GatewayClass or Gateway: you write your own, or use the demo. It turns nothing else off, so Envoy's `LoadBalancer` Services are answered by ServiceLB (or by MetalLB if you chose `--addons metallb` instead; the two profiles are alternatives, since `--addons` takes one value, so install MetalLB later with `platform metallb`).

For clusters that are not FluxVM's, install Envoy Gateway yourself (for example `helm install eg oci://docker.io/envoyproxy/gateway-helm --version v1.10.0 -n envoy-gateway-system --create-namespace`); the commands below only need `kubectl` access.

## Which cluster the commands talk to

`publish`, `inspect` and `demo` run the `kubectl` on your `PATH`, without a shell.

* `--cluster NAME` uses `~/.kube/fluxvm-NAME.yaml`, the kubeconfig `fluxctl kube up` writes.
* `--kube-context CONTEXT` selects a context of the kubeconfig kubectl would use anyway (`KUBECONFIG`, `~/.kube/config`).
* With neither, kubectl's current context. (`--context` is fluxctl's own daemon context and is not used here.)

`explain` and `check` do not use kubectl at all.

## Try it: the demo

```
fluxctl gateway demo apply --cluster dev --dry-run     # print the kubectl commands
fluxctl gateway demo apply --cluster dev               # apply, wait for the app and the Gateway
fluxctl gateway publish dev -n fluxvm-gateway-demo --listener web --cluster dev
```

In another terminal:

```
fluxctl gateway explain http://orders.fluxvm.test:8080/checkout --address 127.0.0.1 --backend orders-v1
fluxctl gateway demo print --checks > checks.json && fluxctl gateway check checks.json
fluxctl gateway inspect orders -n fluxvm-gateway-demo --cluster dev
fluxctl gateway demo teardown --cluster dev
```

The fixture (`fluxctl gateway demo print`) creates the namespace `fluxvm-gateway-demo`, a cluster-scoped GatewayClass `fluxvm-demo-envoy`, an EnvoyProxy with a ClusterIP Service, a Gateway `dev` with an HTTP listener `web` on 8080 for `orders.fluxvm.test`, a small Python app (pinned by digest) that answers with `X-Setura-Backend: orders-v1`, and an HTTPRoute `orders` for `/checkout`. `apply` and `teardown` change the cluster, so without `--dry-run` they need `--cluster`, `--kube-context` or `--yes`. `teardown` deletes exactly the objects the fixture names.

To open the demo in a browser add a temporary hosts entry `127.0.0.1 orders.fluxvm.test` and remove it afterwards; `--address` works without touching DNS.

## `gateway publish GATEWAY --listener NAME`

```
fluxctl gateway publish dev -n fluxvm-gateway-demo --listener web [--port 8080] [--proxy-namespace envoy-gateway-system] [--dry-run]
```

Reads the Gateway, requires the named listener to be HTTP or HTTPS, then finds the Services in the proxy namespace labelled `gateway.envoyproxy.io/owning-gateway-name` and `...-namespace` that expose a TCP port equal to the listener's port. It needs exactly one; zero or several is an error. It then replaces itself with `kubectl -n envoy-gateway-system port-forward --address 127.0.0.1 service/NAME LOCAL:PORT`, so Ctrl-C goes to kubectl. It listens on IPv4 loopback only. The forward ends when you stop it or the proxy pod exits; run it again after the pod is replaced. `--dry-run` does the lookup (reads only) and prints the command. Without `--port` it uses the first free local port from 8080 (a real bind test); an explicit `--port` that is taken is refused before anything runs. When kubectl exits because the proxy pod was replaced, `publish` says so on stderr, finds the Service again and reconnects with a backoff instead of exiting (Ctrl-C still stops it; exit codes 0, 1, 2 as before). The `--port` default of 8080 is gone: use `--port N` to pin one.

## `gateway explain URL`

One direct HTTP/1.1 GET from this machine, with each step reported:

```
dns      overridden   addresses=127.0.0.1
tcp      ok           peer=127.0.0.1
tls      unused
http     ok           status=200 expected=200
backend  ok           identity=orders-v1
```

| Option | Meaning |
|---|---|
| `--address IP` | Connect to this numeric IP. The name is not looked up; `Host` and the TLS server name stay the URL's hostname. |
| `--backend ID` | Expect the `X-Setura-Backend` response header to equal this. |
| `--ca FILE` | Trust this PEM bundle for this request, in addition to the system roots. |
| `--status N` | Expected status (default 200; 404 is a fine expectation for a negative test). |
| `--timeout S` | Seconds for each connect, read and write; more than 0 and at most 60 (default 4). |
| `--json` | The report as JSON (`url`, `ok`, `steps`, `elapsed_ms`). |

Steps are `untested`, `unused` (plain HTTP has no TLS step; no `--backend` means no backend step), `overridden` (dns with `--address`), `ok` or `failed` (with a `reason`). A failure stops the later steps, which stay `untested`. A status mismatch is an http failure, and the backend is not checked after it. The URL must be absolute `http` or `https`, ASCII, without credentials, spaces, control characters or a fragment. Proxy environment variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`) are ignored: the probe opens its own socket. The body is not read.

Exit codes: **0** it passed, **1** a step failed (an observed result), **2** a usage or input error.

## `gateway check FILE`

A JSON array of probes; each is an object with `url` and optionally `address`, `status` (integer), `backend`, `ca` and `timeout`. Unknown keys are refused. Prints the reports as a JSON array. Exit 0 when all passed, 1 when any failed, 2 for an invalid file (before anything runs from the invalid case onward; nothing is printed).

```json
[
  {"url": "http://orders.fluxvm.test:8080/checkout", "address": "127.0.0.1", "status": 200, "backend": "orders-v1"},
  {"url": "http://orders.fluxvm.test:8080/unmatched", "address": "127.0.0.1", "status": 404}
]
```

## `gateway inspect ROUTE`

```
fluxctl gateway inspect orders -n fluxvm-gateway-demo
```

Prints JSON for an HTTPRoute: its generation; for each parent the controller and conditions, each marked `current` when its `observedGeneration` equals the route's generation (a stale condition describes an older version of the route); and for each distinct backend Service whether the referenced port exists and `endpoint_records_by_ready_condition` (`true`, `false`, `unknown` counts over the Service's EndpointSlices; `unknown` is an endpoint that did not report readiness). Other backend kinds are listed as unsupported. A backend that cannot be read gets an `observation_error` instead of failing the command.

## Differences from Setura 0.1.0

* Rust, inside `fluxctl`; the probe uses rustls (TLS 1.2 and 1.3) rather than Python's `ssl`.
* The kubectl context flag is `--kube-context` (fluxctl already uses `--context`); `--cluster NAME` is new.
* URLs must be ASCII (Setura passed non-ASCII hosts to the system IDNA handling).
* The demo's names are `fluxvm-gateway-demo`, `fluxvm-demo-envoy` and `orders.fluxvm.test`; `demo apply|teardown` replace `scripts/cluster-smoke.sh`.
* `X-Setura-Backend` stays the identity header, so existing Setura backends keep working.

## What the live run found

* **k3s 1.37 already installs the Gateway API CRDs** (standard channel, v1.6.3) with a ValidatingAdmissionPolicy that refuses
  experimental-channel CRDs on top of them. Envoy Gateway's `install.yaml` carries experimental ones, so applying it as shipped failed
  on `udproutes`. The add-on now checks the pinned SHA-256 first, then, if the cluster already has `gateways.gateway.networking.k8s.io`,
  applies the manifest without its own `*.gateway.networking.k8s.io` CRDs (Envoy's `*.gateway.envoyproxy.io` and the `x-k8s.io`
  extension CRDs stay).
* **A single-server create never worked on Linux**: the node unit insisted on a `10.89.x.y` private-network card (macOS only) and failed with
  `no address on the private network card`. Without that card the unit now uses the guest's default-route address and card.
* `publish` found the one Envoy Service (the labels match v1.10.0), the demo Gateway reached `Programmed`, `explain` returned the backend
  identity `orders-v1`, `/unmatched` returned the expected 404, a wrong `--backend` exited 1, `check` passed both cases, and `inspect`
  showed `Accepted` and `ResolvedRefs` with one ready endpoint.

## Not verified

macOS guests (Apple Virtualization), HTTPS listeners and `--ca`, MetalLB, more than one cluster, and Envoy Gateway versions other than
v1.10.0 on k3s versions other than v1.37.2.

## In Studio

The Kubernetes page has a **Gateways** tab for each cluster. It lists the Gateways with their listeners and the HTTPRoutes attached
to them, each with a status in plain words: "Ready", "Waiting for the controller" (the controller has not yet seen the latest edit) or
"Not accepted: <reason>". Backends show how many endpoints are ready. Every route row has a "Copy as fluxctl" button
(`fluxctl gateway inspect ROUTE -n NS`). With no Gateways the tab explains how to start: `fluxctl gateway expose SERVICE`, which
installs Envoy Gateway on first use.

The data comes from `GET /v1/kube/{cluster}/gateways` (`schema: 1`): read-only, objects only (never Secrets), through the same client
certificate as the Workloads tab. If the cluster cannot be read the answer carries `error` with a sentence and the next command
instead of failing. Not compiled, type-checked or rendered when this section was written.
