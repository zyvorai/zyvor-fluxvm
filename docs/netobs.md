# Guest-network observability (netobs)

`fluxvm-netobs` is the daemon's native view of what guests do on the network: flows, TCP health, DNS, TLS/HTTP/QUIC
metadata, per-VM roll-ups, and an opt-in, expiring packet capture. It is metadata-only by default. Everything is bounded and
nothing parses hostile input unsafely, but see "Honest limits" for what has not been exercised.

## What it records

| Layer | Recorded |
| --- | --- |
| Flows | 5-tuple, direction (outbound = the guest opened it), VLAN, first/last seen, packets and bytes each way, verdict (`allowed`, `denied`, `dropped`) and drop reason, close reason |
| TCP health | retransmits, resets, zero windows, RTT samples (sum and max) |
| DNS | query name and type, response code, latency, answers and CNAME targets (when kept); encrypted DNS (DoT, DoH by SNI against a host list, DoQ) is tagged, not decoded |
| TLS | SNI, ALPN, JA3/JA4 (client) and JA3S/JA4S (server), negotiated version and cipher, recovered by bounded reassembly of the Client/ServerHello |
| HTTP | host and request count for cleartext HTTP/1 and cleartext HTTP/2 (`h2c`) heads via HPACK |
| QUIC | v1/v2 client Initial decoded for SNI, ALPN and fingerprint |
| Roll-ups | RED-style rows (flows, packets, bytes, resets, retransmits, drops, DNS errors, HTTP requests) per VM and per peer, in 30 s windows |

Payload bytes are **not** recorded. The only thing that keeps packet bytes is a capture session (below).

## Redaction

Names (DNS query name, TLS SNI, HTTP host) pass through a `Redaction` policy before they are stored or exported, so a dropped
name is never held. Per field the mode is `keep` (lower-cased), `registrable` (last two labels; a heuristic, no public-suffix
list), `hash` (salted SHA-256 prefix, `h:<16 hex>`) or `drop`. `dns_answers` and `fingerprints` can also be switched off;
`Redaction::strict()` drops all names, answers and fingerprints.

**The daemon installs the default policy (`keep` everywhere, answers and fingerprints kept, public salt).** There is no
`fluxvm.toml` key for redaction yet; the policy is a property of the embedded `ObserverConfig`. Treat `/v1/netobs/*` as
sensitive: it shows which hostnames each guest resolves and connects to. Use tenant-scoped tokens for tenants (below).

## Capture: TTL, filter, admin

Capture is off until a session is enabled for one VM. A session needs a filter (at least one of `host`, `port`, `proto`;
whole-NIC capture is refused), has a TTL (default 60 s, **capped at 1 hour**), and is bounded by packet count (default 4096)
and bytes (default 8 MiB); oldest frames are overwritten. `snaplen` defaults to 128 bytes (headers and little payload), maximum
65535. The daemon allows 8 concurrent sessions. A stopped or expired session stays downloadable for 5 minutes. Starting,
stopping and downloading require the **admin** role (the pcap holds packet bytes); listing sessions does not.

```sh
fluxctl netobs capture start --vm web --port 53 --ttl-secs 120
fluxctl netobs capture status
fluxctl netobs capture download --vm web -O web.pcap
fluxctl netobs capture stop --vm web
```

## API

All routes are under `/v1/netobs/`. When no observer is installed the read routes answer `enabled: false` with an empty
result, so clients can probe without special cases (the SSE stream and the capture writes return 503).

| Route | Purpose |
| --- | --- |
| `GET /v1/netobs/summary` | counters, per-VM totals, top talkers, top errors, capture sessions |
| `GET /v1/netobs/flows?vm&proto&port&host&verdict&closed&limit` | flow table (live; `closed=true` adds recently closed; default limit 200) |
| `GET /v1/netobs/flows/stream?vm&since` | Server-Sent Events of flow, DNS, TLS, HTTP and drop metadata; `id:` is a sequence number, resume with `since` or `Last-Event-ID` |
| `GET /v1/netobs/dns?vm&limit` | recent DNS transactions |
| `GET /v1/netobs/history` | retained roll-up windows (240 windows of 30 s, at most 1 hour) |
| `GET/POST/DELETE /v1/netobs/capture` | list, start (admin), stop (admin; `?vm=`) |
| `GET /v1/netobs/capture.pcap?vm=` | download the capture (admin) |

A tenant-scoped token sees only its tenant's VMs: the global counters are blanked, per-VM and top-N rows are filtered, and
naming another tenant's VM in the stream or capture routes is a 404 (the list routes just return nothing for it). CLI: `fluxctl netobs summary|flows|dns|history|capture ...` (table output, `--json`
for the raw document; `flows --follow` tails the stream; `--vm` takes an id, name or unique prefix). Without `--server` or a
context the CLI talks to the local daemon's `listen` address, because the observer lives in the daemon process.

## Prometheus metrics

`/metrics` gains these series when the observer is installed (empty otherwise): `fluxvm_netobs_frames_total`,
`_bytes_total`, `_malformed_total`, `_truncated_total`, `_flows_active`, `_flows_created_total`, `_flows_evicted_total`,
`_flows_expired_total`, `_frame_drops_total` (by reason), `_tap_enqueued_total`, `_tap_dropped_total`, the
`_reasm_*` family (segments dropped, streams abandoned, incomplete, conflicts, bytes buffered),
`_encrypted_dns_flows_total`, `_quic_hellos_total`, and per VM `fluxvm_netobs_vm_packets_total` and
`fluxvm_netobs_vm_tcp_retransmits_total`. An OTLP JSON rendering of the summary exists in the crate (`otlp_metrics_json`);
no route serves it yet.

## Limits

Defaults of the daemon's observer:

- `netobs.queue_frames` 4096 frames queued between a datapath and the observer, `netobs.snaplen` 2048 bytes seen per frame
  (`[netobs]` in `fluxvm.toml`; `netobs.enabled = false` or `FLUXVM_NETOBS=0` turns it off). A full queue drops the frame
  and counts it (`tap_dropped`); the datapath never blocks.
- 65 536 live flows (least-recently-seen evicted), idle timeouts 300 s TCP, 60 s UDP, 30 s other, long flows re-exported
  every 300 s.
- Event ring 4096, DNS ring 2048, 4096 closed flows kept for at most 1 hour, 1024 VMs in the lifetime totals, 4096 roll-up
  keys per window.
- Reassembly: 16 KiB per stream, 16 pending segments, 64 MiB for the whole observer; a bound hit is counted, not an error.
- Capture limits above. The ring and history are in memory: a daemon restart clears them.

## Sources and the bridge

Frames reach the observer from two kinds of source:

- **In-process taps.** The `apple.usernet` userspace gateway offers each frame it sees, with the gateway's verdict, to the
  process-wide `NetTap`. A drained tap feeds the observer through a pump started with the daemon.
- **The bridge.** `fluxvm-vz-switch` (one process per `apple.networks` private network) cannot call the daemon's observer
  in-process. When built with the `netobs` feature and a daemon observer exists, the daemon binds
  `<runner socket dir>/netobs-bridge.sock` (mode 0600) and starts each switch with `--netobs-socket PATH --network NAME`. The switch
  exports every guest frame with its verdict over a versioned (`FXNB` v1) unix-stream protocol; it never blocks, drops and counts
  when the queue is full or the daemon is gone, and reconnects with backoff. A switch built without the feature is restarted
  without the flag.

## Honest limits

- Verified: unit tests, a replay/ingest benchmark and a switch-to-daemon bridge test over a unix socket with synthetic frames;
  the API routes (scoping, capture gating, metrics) are tested with injected frames.
- **Unverified:** real guest traffic through `apple.usernet` or a vz private network on a Mac, frame-rate behaviour of the tap
  under a live guest. By construction the only sources are the usernet gateway and private-network switches: a plain
  `network.mode = "user"` (Virtualization.framework NAT) guest is not observed at all.
- The `verdict` at the frame level of the bridge is what the switch decided; gateway-level denials with reasons show in the
  gateway log.
- Capture holds packet bytes, including any plaintext payload up to `snaplen`. It is not encrypted at rest (memory only; a
  downloaded pcap is the operator's to protect).
- Heuristics, not parsers of record: `registrable` redaction, JA3/JA4 on partial hellos, HTTP/2 only in cleartext.
