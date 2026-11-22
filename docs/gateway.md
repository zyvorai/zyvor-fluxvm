# Gateway API on FluxVM

`fluxctl gateway` publishes a Kubernetes Gateway listener on your Mac or Linux workstation and shows which application answers. It works with Envoy Gateway on any cluster `kubectl` can reach, and FluxVM can install Envoy Gateway on its own k3s cluster.

This is a port of **Zyvor Setura 0.1.0**, the Python developer CLI from Zyvor, into `fluxctl` (Rust, no Python needed). The commands, the probe's behaviour and the demo fixture follow Setura; credit for the design is Setura's.

> Status: the code is unit-tested with loopback HTTP and TLS servers and a fake `kubectl`. When this page was written it had **not** been run against a live cluster, and Envoy Gateway had not been run by us. Treat the first real run as the test.

## Install Envoy Gateway on a FluxVM k3s cluster

```
fluxctl kube apple create dev --addons envoy-gateway      # at creation
fluxctl kube apple platform envoy-gateway dev             # or later, on an existing cluster
```

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

Reads the Gateway, requires the named listener to be HTTP or HTTPS, then finds the Services in the proxy namespace labelled `gateway.envoyproxy.io/owning-gateway-name` and `...-namespace` that expose a TCP port equal to the listener's port. It needs exactly one; zero or several is an error. It then replaces itself with `kubectl -n envoy-gateway-system port-forward --address 127.0.0.1 service/NAME LOCAL:PORT`, so Ctrl-C goes to kubectl. It listens on IPv4 loopback only. The forward ends when you stop it or the proxy pod exits; run it again after the pod is replaced. `--dry-run` does the lookup (reads only) and prints the command.

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

## Not verified

Nothing here has run against a live cluster or a real Envoy Gateway. Things to watch on the first run: that Envoy Gateway v1.10.0 generates the Service labels `publish` looks for, that the 4 MiB `install.yaml` applies cleanly on your k3s version, and that the demo Gateway reaches `Programmed` with the ClusterIP EnvoyProxy settings.
