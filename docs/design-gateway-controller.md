# Design: a FluxVM Gateway API controller and proxy

> Status: **proposal, nothing built.** This page is the design to agree on before any code. Every statement about the existing code was
> read from the repository on 2026-11-22; every estimate is a guess and says so.

## Why

FluxVM installs Envoy Gateway as an add-on (`kube cluster platform envoy-gateway`, tested live on a k3s v1.37.2 guest; see
[gateway.md](gateway.md)). It works, but:

* Envoy Gateway and Envoy are two large upstream projects (about 4 MiB of manifest, two Deployments, their own CRDs), pinned by us and
  upgraded by us. One of the two bugs the live run found came from that coupling: k3s 1.37 ships its own Gateway API CRDs, which
  collide with the ones in Envoy's manifest (the other bug, the Linux single-server create, was unrelated).
* On a small Mac or Linux workstation cluster most users want one thing: hostnames and paths routed to Services, with TLS. That needs a
  small part of what Envoy offers.
* FluxVM already has its own HTTP code. It just is not Kubernetes-aware (next section).

This is **not** a plan to replace Envoy everywhere. Envoy Gateway stays as the full-featured option (rate limiting, ext_authz, WASM,
xDS, mTLS policies). The goal is a **default that is built in, small, and good enough** for the common case.

## What exists today

| Piece | Where | What it does | What it lacks for Kubernetes |
| --- | --- | --- | --- |
| HTTPS reverse ingress | `fluxvm-mesh/src/ingress.rs` (about 500 lines) | TLS termination (cert reloaded per connection, so ACME renewals apply), HTTP/1.1 and h2 (hyper), upgrades, ACME HTTP-01 | `IngressRoute` is `{name, hostname, backend: SocketAddr, share_required, strip_share_prefix}`: one hostname to **one fixed address**. No paths, headers, weights, redirects, per-host certificates, health or retries. It also has a hand-written raw proxy path (`proxy_connection`, 64 KiB header limit) |
| HTTP/3 ingress, ACME client, public-edge reconciler | `fluxvm-mesh` (`http3.rs`, `acme.rs`, `edge.rs`, `public.rs`) | Personal-cloud exposure of a node to the internet | Host-level, not cluster-level |
| L7 egress proxy | `fluxvm-network/src/egress_proxy*`, `http_acl.rs` | Allow/deny by method, host and path; TLS interception | Outbound only |
| Service Fabric | `fluxvm-network/src/service.rs` | Node-local VIP load balancing on BPF (Maglev, NAT/DSR) | L4, node-local |
| Kubernetes controller scaffolding | `crates/fluxvm-kube` | `kube` 4.2 and `k8s-openapi`, a reconcile loop for the `DisposableVm` CRD | No Gateway API types yet |
| `fluxctl gateway` | `crates/fluxctl/src/gateway/` | Client-side probe, `publish`, `inspect`, demo | It is a client; it routes nothing |

Nothing outside `fluxctl gateway` reads `GatewayClass`, `Gateway` or `HTTPRoute`.

## Proposal

A new crate **`fluxvm-gateway`**, one binary, run in the cluster as a Deployment, that is both controller and data plane.

```
 kube API (Gateway, HTTPRoute, Service, EndpointSlice, Secret, ReferenceGrant)
      │ watch (kube-rs)
      ▼
 ┌───────────────────────────── fluxvm-gateway pod(s) ─────────────────────────────┐
 │ reflectors ─► resolver ─► route table (immutable snapshot, swapped atomically)   │
 │                   │                                  │                           │
 │                   ▼                                  ▼                           │
 │        status writer (leader only,          proxy (hyper + rustls, per listener) │
 │        Lease): conditions, generation       match ─► pick endpoint ─► forward    │
 └──────────────────────────────────────────────────────────────────────────────────┘
```

* **GatewayClass** controller name: `gateway.fluxvm.dev/gatewayclass-controller`. `fluxctl kube cluster platform fluxvm-gateway`
  would install the CRD-less manifest (Gateway API CRDs come from k3s 1.37, or from our own pinned copy on older k3s).
* **No xDS and no control-plane/data-plane split.** Each pod watches the API itself and builds its own route table. That removes the
  hardest moving part of the Envoy design; the cost is that every replica has read access to routes and endpoints. Fine at workstation
  scale; stated as a limit.
* **One Deployment and one ClusterIP Service per Gateway**, created by the controller (same shape Envoy Gateway uses), labelled
  `gateway.fluxvm.dev/owning-gateway-name` and `-namespace`. `fluxctl gateway publish` learns this second label pair; the demo, `explain`,
  `check` and `inspect` need no change.
* **Proxy built on hyper only.** Do **not** reuse the mesh's raw proxy path (hand-parsed headers are a request-smuggling risk we should
  not carry into a general proxy). Shared pieces worth extracting into a small `fluxvm-http-proxy` crate: the rustls acceptor with
  reload-on-renewal, h2/upgrade handling, and ACME. This extraction is a refactor of `fluxvm-mesh` and is its own, reviewable step.
* **Endpoints** come from EndpointSlices (ready conditions), so the proxy load-balances across pod IPs directly instead of the Service
  VIP (round-robin first; passive ejection of failing endpoints in phase 3).

## Scope by phase

Estimates are guesses at lines of Rust including tests, for sizing only.

**Phase 1: HTTP routing that passes the Core HTTPRoute conformance tests it claims** (about 4-6k lines)
* HTTP listeners; `HTTPRoute` hostnames (with wildcard rules), path `Exact` and `PathPrefix`, method and header matches, the spec's
  precedence order; `backendRefs` with weights; `RequestHeaderModifier`, `RequestRedirect`.
* Correct status: `Accepted`, `ResolvedRefs` and `Programmed` with `observedGeneration`, listener `attachedRoutes`, a per-parent
  entry written only for our GatewayClass. Status accuracy is where most hand-rolled gateways fail, so it is in phase 1.
* Cross-namespace `backendRefs` honouring `ReferenceGrant`.

**Phase 2: TLS and rewrites** (about 3-4k lines)
* HTTPS listeners terminating TLS from `Secret`s (SNI per hostname), cert-manager-issued certificates, HTTP-to-HTTPS redirect.
* `URLRewrite`, request timeouts, simple retries, `RequestMirror` if conformance asks for it.

**Phase 3: more route types and resilience** (sizing deferred until phase 2 ships)
* `GRPCRoute`, `TLSRoute` passthrough, `TCPRoute`; `BackendTLSPolicy`; passive endpoint ejection; HTTP/3 through the existing mesh code.
* A bridge from a Gateway listener to the **mesh public edge** (`IngressRoute`), so a Gateway can be reachable from the internet through
  FluxMesh with ACME. That bridge needs `IngressRoute` to grow paths and several backends, or a new route type; decided then.

## Explicit non-goals

xDS, WASM, external authorization, rate limiting, mTLS policy engines, observability pipelines beyond metrics and access logs,
and a claim of full conformance. Users who need those use the Envoy add-on; both can run in one cluster under different
GatewayClasses.

## How it is verified

1. Unit tests for the matcher (the precedence rules are the classic bug source), the resolver (status conditions), and the proxy on
   loopback HTTP and TLS servers, as the Setura port already does.
2. The upstream **Gateway API conformance suite** (Core HTTPRoute profile), run against a real cluster. The rig is the one used for
   the Envoy live run: a FluxVM single-node k3s guest on the Linux host. Pass and fail are published per feature, not rounded up.
3. Side-by-side: the same demo and `fluxctl gateway check` file against Envoy and against `fluxvm-gateway`.
4. A soak of a few hours with a request load, watching memory and file descriptors (a hand-written proxy leaks in ways tests miss).

## Risks

* **Security surface.** A general HTTP proxy is attack surface. Mitigation: hyper's parser only, header and body size limits, no raw
  path, a fuzz target for the matcher, and a plain statement in the docs that Envoy is the hardened option.
* **Conformance is a long tail.** Phase 1 may pass a core subset first. The status writer and precedence rules take longer than the
  proxy.
* **Two data planes to document.** The default must be obvious; Envoy stays opt-in (`--addons envoy-gateway`).
* **Image delivery.** The controller needs a container image. Building and publishing it is a release action: it is yours to trigger,
  not something this work does on its own. For development, a `fluxctl` subcommand can run the same code locally against a
  kubeconfig, so the work is testable before any image exists.
* **Release cost.** A new crate means new release artifacts, SBOM and licence review.

## Decisions needed

1. **Go or no-go on phase 1**, given the conformance tail above.
2. **Data-plane image:** a new image published by the project (recommended), or run the controller only locally through `fluxctl`
   during development.
3. **Extract `fluxvm-http-proxy` from `fluxvm-mesh` first** (recommended, small and reviewable) or write the new proxy standalone and
   unify later.
4. **Default GatewayClass:** should `kube up` install `fluxvm-gateway` by default once phase 1 passes conformance, with Envoy opt-in?

## Sequence if approved

1. Extract the shared TLS/h2/ACME pieces (no behaviour change; the mesh tests must stay green).
2. `fluxvm-gateway` skeleton: reflectors, route table, matcher with tests; local run through `fluxctl`.
3. Proxy and status writer; run the demo and `fluxctl gateway check` against it on the Linux rig.
4. Conformance suite run, fix, repeat; then the add-on installer and docs.
