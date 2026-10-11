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
`_flows_expired_total`, `_frame_drops_total` (by reason), `_tap_enqueued_total`, `_tap_dropped_total`, the bridge series
`_bridge_frames_total`, `_bridge_dropped_total{reason}`, `_bridge_truncated_total`, `_bridge_unconfirmed` (gauge) and
`_switch_forward_dropped_total` (see "Bridge delivery and loss accounting"), the
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

## Bridge delivery and loss accounting

Observation over the bridge is **best effort**: the switch never waits for the observer, so when frames are offered faster
than the observer side can take them, some observations are lost. What the bridge guarantees is that the loss is
**counted, never silent**: every frame a switch offers is either ingested or added to exactly one drop counter, so
`offered = ingested + dropped` (plus `bridge_unconfirmed`, below, which is zero in steady state).

How a frame travels: the switch encodes it into one shared, bounded byte queue (default 32,768 frames or 16 MiB, whichever
fills first; frames cut to 2048 bytes), a writer thread swaps the whole queue out and writes it with one `write` per batch,
and the daemon's reader ingests in batches of 64 per observer lock. The first record on each connection is a STATS record that
gives the daemon a baseline, and STATS is re-sent every 250 ms when anything changed.

Counters (summary `counters.*`, `/v1/netobs/summary`; Prometheus names in the metrics section):

| Counter | Meaning |
| --- | --- |
| `bridge_frames` | frames received from switches and ingested |
| `bridge_dropped` | observations lost, the sum of `bridge_dropped_by_reason` |
| `bridge_dropped_by_reason` | one entry per fixed reason, see below |
| `bridge_truncated_to_snaplen` | frames cut to the export snap length; their byte counts are understated |
| `bridge_unconfirmed` | frames a switch reports as sent that the daemon has not seen: in flight right now, or lost with a connection that died. A value that stays above zero is a loss that no counter could attribute |
| `switch_forward_dropped` | frames the switch observed but could not hand to the destination guest (full receive queue). Not an observation loss; it is what retransmits in the flows are made of |
| `tap_enqueued`, `tap_dropped` | totals over all sources: the in-process tap (usernet) plus the bridges |

`bridge_dropped_by_reason` labels (bounded, `reason="..."` in Prometheus):

* `queue_full`: the switch's queue had no room: the datapath outran the writer or the daemon. This is the one that appears
  under sustained line-rate traffic.
* `disconnected`: no connection to the daemon when the frame was offered (daemon down or restarting) or while it waited.
* `write_error`: a socket write failed or stalled past 250 ms; frames of that write that did not fully leave are counted
  here. The connection is re-made with backoff.
* `oversize`: the frame or identity could not be encoded (too many identities on one connection, or a name over 255 bytes).
* `server_rejected`: the daemon received it but could not attribute it (a frame for an identity it was never told about).
* `unspecified`: a drop reported by a switch too old to give a reason.

`drops_by_reason` (and `fluxvm_netobs_frame_drops_total`) is a different thing: the **datapath's** verdicts on frames
(`policy`, `spoofed_source`, `unknown_destination`, ...), observed correctly. A frame lost by the bridge shows up in
`bridge_dropped_by_reason`, never in `drops_by_reason`, and `drops_by_reason.queue_full` is a datapath queue (the usernet
gateway's), not the bridge's.

### What the residual under-count looks like

Measured over a real unix socket with MTU-size frames (`cargo test -p fluxvm-netobs --release --test bridge_throughput --
--ignored --nocapture`): a paced 150-500 kpps feed is delivered 99.6-100 % with the rest reported as `queue_full`; an unpaced
producer far above anything a guest can send (millions of frames per second) is delivered at 5-10 % and the other 90 %
is reported. Before the accounting fix a 300 kpps run delivered 81 % and reported 0 %.

So at line rate, observation is a **sample with a known size**: packet and byte totals for a VM can be low by
`bridge_dropped / (bridge_frames + bridge_dropped)`, and `bridge_dropped_by_reason` tells you why. Flow-level facts that need
only a few packets (the handshake, a DNS query, a ClientHello) are rarely affected; counters of bulk transfers are. TCP
retransmits are counted among the frames that were observed, so they are also understated under loss.

Paths that cannot be counted: a datagram the guest's socket buffer dropped before the switch read it (invisible to any
userspace process), and frames the kernel accepted for a connection that died before the daemon read them (these appear in
`bridge_unconfirmed`, as a total but without a reason).

### Guest-to-guest flows

A switch tap sees each frame once, as sent by one guest. So that both VMs of a guest-to-guest connection get a complete view,
the observer also accounts an allowed unicast frame in the *receiving* guest's view (`mirror_peer_views`, on by default;
the receiver is found by its MAC on the same network, learned from frames it has sent). Consequences: each VM's flow is
oriented from its own side (the server VM sees `direction: inbound`), both directions' packets and bytes count, FIN or RST
from either side closes the flow, and a VM's per-VM packet totals are its transmit plus its receive on private networks. The
mirrored copy does not add to the observer-wide `frames` and `bytes`. A guest that has not sent anything yet cannot be found
as a receiver, so its first received frames (typically the connection's SYN) are not attributed to it; broadcasts are not
mirrored.

Flows also report `oriented`: `observed` (a SYN or SYN+ACK decided client and server), `first_packet` (UDP/ICMP: the first
frame seen is the client's) or `guessed` (a TCP flow first seen mid-stream: the server is the endpoint with the well-known,
else non-ephemeral, else lower port).

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
