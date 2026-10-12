# FluxMesh Phase 6 — CGNAT public edge, DNS-01, HTTP/2 and SLOs

Phase 6 makes the Phase-5 personal-cloud edge usable when the home/office ISP
uses CGNAT or blocks inbound port forwarding.

## Blind public relay

`fluxmesh-public-relay` has a public listener and a mutually separated tunnel
listener. The edge opens the tunnel **outbound**. The relay reads only the TLS
ClientHello SNI, selects an ordered active/standby site, and forwards the
visitor's original TLS ciphertext to that site. The browser TLS session still
terminates on `fluxmesh-ingress`; the relay never receives the site's private
certificate key and never sees HTTP plaintext.

Relay routes are ordered:

```json
{
  "hostname": "photos.example.net",
  "sites": ["home-primary", "office-standby"]
}
```

The first currently registered site wins. If it disconnects, the next site is
selected for new connections.

## DNS-01 / wildcard ACME

Phase 5 HTTP-01 remains the default. Phase 6 adds DNS-01 with Cloudflare and a
provider-neutral webhook contract. Wildcards are accepted only when the ACME
challenge mode is DNS-01.

```bash
fluxctl cloud acme-dns01-cloudflare \
  --email ops@example.net \
  --domain example.net \
  --domain '*.example.net' \
  --zone-id ZONE_ID
```

The API token comes from `CLOUDFLARE_API_TOKEN` by default. Challenge TXT
records are created, waited on for the configured propagation interval, and
removed after validation.

## HTTP/2

The native TLS ingress now advertises `h2,http/1.1` via ALPN. HTTP/2 requests
are handled by Hyper and proxied to the selected FluxMesh ingress backend.
HTTP/1.1 stays on the existing raw bidirectional path so connection upgrades
and WebSockets keep their zero-copy behavior.

## HTTP/3 status

This PR does **not** claim browser HTTP/3. The repository lock already contains
a QUIC transport dependency through other components, but it does not contain
a reviewed HTTP/3 + QPACK server stack for `fluxvm-mesh`. Advertising `h3`
without that stack would be incorrect. A later PR can add h3 once it can be
compiled and tested in the normal Rust CI lane.

## IPv6 mDNS

`fluxmesh-mdns --ipv6-ifindex N` joins `ff02::fb` on the selected interface and
reflects packets over the same encrypted overlay envelope used for IPv4 mDNS.
An explicit interface index is required because IPv6 link-local multicast is
ambiguous on multi-homed hosts.

## Telemetry / SLO

`fluxmesh-edge` writes `/var/lib/fluxvm/mesh/edge-telemetry.json` after every
reconcile. `fluxmesh-metrics` exposes:

- `fluxmesh_edge_reconcile_total`
- `fluxmesh_edge_reconcile_failures_total`
- `fluxmesh_edge_acme_renewals_total`
- relay connected gauge
- public IPv4/IPv6 gauges
- ingress route count
- last reconcile duration

`GET /healthz` fails when telemetry is stale or the reconcile failure ratio is
over the configured/default threshold. `fluxctl cloud telemetry` renders the
same Prometheus snapshot and SLO result locally.

## Ownership boundary

FluxMesh still owns only host-to-host/public-edge connectivity. VM-edge CNP,
Service Fabric VIP/NAT state, and distributed Fabric intent stay with their
existing owners.
