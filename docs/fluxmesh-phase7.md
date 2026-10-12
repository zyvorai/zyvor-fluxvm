# FluxMesh Phase 7 — HTTP/3, QUIC relay transport, active/active edge and DNSSEC checks

Phase 7 turns the Phase-6 public edge into a standards-oriented production path.
It is intentionally additive: FluxMesh still owns host/public-edge connectivity;
Network Fabric remains the VM-edge policy engine and Service Fabric remains the
node-local VIP/Maglev dataplane.

## Real browser HTTP/3

`fluxmesh-edge` now starts a UDP/QUIC listener (default `[::]:443`) backed by
`h3` + `h3-quinn` + Quinn. The public certificate and routing/share-link policy
are the same ones used by HTTP/1.1 and HTTP/2. HTTP/2 responses advertise:

```text
Alt-Svc: h3=":443"; ma=86400
```

so a browser can discover the QUIC path after an ordinary HTTPS connection.
HTTP/3 counters are exported through the existing edge telemetry endpoint.

**Boundary:** browser QUIC is a direct UDP edge path. The blind CGNAT relay
continues to accept visitor TCP/TLS on its public listener. We do not translate
browser HTTP/3 into TCP or claim HTTP/3-through-CGNAT.

## QUIC-first edge → relay tunnel

The edge's outbound relay channel can use QUIC with ALPN
`fluxmesh-relay/1`. `transport=auto` tries QUIC first and falls back to the
Phase-6 TLS/TCP tunnel. `transport=quic` makes failure explicit; `tls-tcp`
forces the compatibility path.

Both transports authenticate REGISTER, DATA and heartbeat messages with the
existing per-site HMAC secret. Visitor bytes are still the original TLS
ciphertext; the relay never receives the application's certificate private key.

Relay server example:

```json
{
  "public_listen": "0.0.0.0:443",
  "tunnel_listen": "0.0.0.0:7444",
  "quic_tunnel_listen": "0.0.0.0:7444",
  "tls_cert": "/etc/fluxmesh-relay/server.pem",
  "tls_key": "/etc/fluxmesh-relay/server.key",
  "routes": [{
    "hostname": "*.example.net",
    "sites": ["home-a", "home-b"],
    "mode": "active-active"
  }],
  "sites": [
    {"id":"home-a","secret_file":"/etc/fluxmesh-relay/home-a.secret"},
    {"id":"home-b","secret_file":"/etc/fluxmesh-relay/home-b.secret"}
  ]
}
```

## Active/active public edge leases

A connected site is eligible only while its authenticated heartbeat lease is
fresh. Active/standby chooses the first healthy configured site. Active/active
uses rendezvous hashing over the visitor request id, spreading new connections
while keeping deterministic selection for a given key.

Session ids make reconnects race-safe: a stale connection cannot refresh or
remove a newer site registration.

## DNSSEC-aware DNS-01

DNS-01 can optionally verify the expected TXT value through a configured trusted
recursive resolver with EDNS0 DO and require the response AD bit before ACME is
notified:

```bash
fluxctl cloud acme-dns01-cloudflare \
  --email ops@example.net \
  --domain example.net \
  --domain '*.example.net' \
  --zone-id "$ZONE" \
  --validation-resolver 1.1.1.1:53 \
  --require-dnssec-ad
```

This is deliberately a **DNSSEC-aware verifier**, not an in-process validating
resolver. When `--require-dnssec-ad` is used, the configured recursive resolver
is the trust boundary and must perform DNSSEC validation itself.

## OTLP/HTTP tracing

`fluxctl cloud otlp <endpoint>` enables lightweight OTLP/HTTP JSON spans without
pulling the full OpenTelemetry SDK into FluxMesh. Phase 7 emits:

- `fluxmesh.edge.reconcile`
- `fluxmesh.ingress.request` with `http.protocol=h2`
- `fluxmesh.ingress.request` with `http.protocol=h3`

The exporter has a bounded timeout and never fails an application request if the
collector is unavailable.

## Deployment ports

For direct HTTP/3, allow/forward both TCP/443 and UDP/443 to the edge. For a
CGNAT-only site, no inbound home port is required; the edge creates its relay
connection outbound. On the public relay, expose TCP/443 to visitors and the
configured TCP/QUIC tunnel port to edges.
