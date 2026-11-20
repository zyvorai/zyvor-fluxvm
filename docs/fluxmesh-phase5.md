# FluxMesh Phase 5 — public personal-cloud edge

Phase 5 turns the Phase 1–4 encrypted host mesh into a usable personal cloud that can be reached from the public Internet without hand-editing router, DNS and TLS configuration.

## Ownership boundary

FluxMesh still does **not** replace FluxVM Network Fabric or Service Fabric:

- Network Fabric owns VM-edge security/eBPF policy.
- Service Fabric owns node-local VIP/Maglev/NAT/DSR state.
- FluxMesh owns host-to-host connectivity, public ingress, enrollment, edge reachability and consumer/SMB router integration.

Public ingress forwards to an explicit backend socket. A backend can be a local service, a Service Fabric VIP, or a mesh address.

## Added in Phase 5

### Native HTTPS ingress

`fluxmesh-ingress serve` and `fluxctl cloud serve-ingress` terminate TLS and route by HTTP `Host` to entries in `/var/lib/fluxvm/mesh/public.json`.

```bash
fluxctl cloud expose photos photos.example.com 100.96.0.12:8080 --share-required --strip-share-prefix
fluxctl cloud serve-ingress --http01-listen 0.0.0.0:80
```

The ingress reloads certificate/key material for each accepted TLS connection, so ACME renewals become active without restarting the edge process.

### Short-lived share links

Share links are HMAC-SHA256 signed, route-bound and expiring. The secret is local and mode 0600 on Unix.

```bash
fluxctl cloud share photos --ttl-seconds 3600
# https://photos.example.com/s/v1....../
```

A token issued for `photos` cannot authorize another route.

### Native ACME v2 / HTTP-01

FluxMesh includes an RFC 8555 ACME client using ES256 JWS, account-key persistence, POST-as-GET, HTTP-01 authorization, CSR generation and atomic PEM install.

```bash
fluxctl cloud acme \
  --email admin@example.com \
  --domain photos.example.com \
  --domain files.example.com
```

The default directory is Let's Encrypt production. Use `--directory` for staging/Pebble. Phase 5 intentionally supports HTTP-01 first; wildcard DNS-01 is not claimed.

`fluxmesh-edge` renews certificates on a 60-day cadence (appropriate for common 90-day ACME certificates) and hot-publishes them through ingress reload.

### Dynamic DNS

Cloudflare is supported directly, plus a generic authenticated webhook contract.

```bash
export CLOUDFLARE_API_TOKEN=...
fluxctl cloud ddns-cloudflare home.example.com \
  --zone-id ZONE --record-id-v4 RECORD_A --record-id-v6 RECORD_AAAA
fluxctl cloud ddns-sync --ipv4 203.0.113.10 --ipv6 2001:db8::10
```

The long-running edge can source IPv4 from configured STUN observers and IPv6 from a `PublicIpv6` mesh endpoint or `FLUXMESH_PUBLIC_IPV6`.

### Router automation

FluxMesh attempts real PCP MAP, NAT-PMP and UPnP IGD AddPortMapping operations. `auto` tries them in that order.

```bash
fluxctl cloud router-map 192.168.1.1 https tcp 443 443 --protocol auto
fluxctl cloud router-map 192.168.1.1 acme tcp 80 80 --protocol auto
fluxctl cloud router-sync
```

Mappings are configuration, not a security bypass: the ingress/share policy still applies at the host.

### Overlay mDNS reflection

`fluxmesh-mdns` reflects IPv4 mDNS packets through unicast UDP over the already encrypted overlay, with a SHA-256-based duplicate cache to stop reflection loops.

```bash
fluxmesh-mdns 0.0.0.0:53530 100.96.0.12:53530 100.96.0.13:53530
```

Phase 5 bridges IPv4 mDNS (`224.0.0.251:5353`). Native IPv6 mDNS reflection is a later extension.

### DHCPv6 Prefix Delegation

`fluxmesh-pd` implements the DHCPv6 Solicit → Advertise → Request → Reply IA_PD/IAPREFIX flow and emits an idempotent Linux route plan.

```bash
sudo fluxmesh-pd '[::]:546' '[ff02::1:2]:547' 2 fluxmesh0
```

The third argument is the numeric IPv6 interface scope ID for link-local multicast.

### Multi-WAN

WAN uplinks are health-scored with TCP probes. Selection keeps the active WAN until it is unhealthy rather than flapping on small latency changes. Linux plans use dedicated routing tables plus a high-priority active table rule.

```bash
fluxctl cloud wan-add fiber eth0 --gateway 192.168.1.1 --preferred --probe 1.1.1.1:443
fluxctl cloud wan-add lte wwan0 --gateway 192.168.8.1 --metric 200 --probe 8.8.8.8:443
```

### Edge reconciler

Run one process for continuous public-edge convergence:

```bash
sudo fluxmesh-edge run \
  --stun 1.1.1.1:3478 \
  --stun 8.8.8.8:3478
```

It owns only Phase 5 public-edge state:

1. ACME renewal;
2. DDNS synchronization;
3. PCP/NAT-PMP/UPnP refresh;
4. WAN health and routing policy;
5. HTTP-01 listener;
6. HTTPS ingress.

### Diagnostics

```bash
fluxctl cloud diagnose --ingress 127.0.0.1:443
```

The report checks mesh/public state, key/cert files and Unix permissions, required host tools, and ingress reachability. Warnings do not fail the command; hard state errors do.

## Files

| Path | Purpose |
|---|---|
| `<mesh dir>/public.json` | ingress/DDNS/router/WAN/ACME desired state |
| `<mesh dir>/share.key` | share-link HMAC secret |
| `<mesh dir>/acme-account.pk8` | ACME P-256 account key |
| `<mesh dir>/acme-issued-at` | renewal cadence marker |
| `<mesh dir>/acme-http01/` | transient HTTP-01 challenges |
| `<mesh dir>/tls/fullchain.pem` | ingress certificate chain |
| `<mesh dir>/tls/key.pem` | ingress certificate private key |

## Production notes

- Port 80 must reach the selected edge while HTTP-01 validation is in progress.
- Public ingress is HTTP/1.1 aware. WebSocket upgrade works because traffic becomes a byte stream after the initial request header; HTTP/2 termination is not claimed in Phase 5.
- `fluxmesh-pd` normally needs permission to bind UDP 546.
- Router protocols depend on the gateway supporting PCP, NAT-PMP or UPnP and may be disabled by enterprise policy.
- macOS public ingress and ACME work as user-space sockets; Linux policy-route application is Linux-only.
