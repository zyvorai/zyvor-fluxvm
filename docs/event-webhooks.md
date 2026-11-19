# Outbound event webhooks

The daemon appends every VM event to its event log (`GET /v1/events`, `GET /v1/events/stream`). With `[[event_sinks]]` in the host
config it also POSTs each event as JSON to the URLs you list, signed so the receiver can check who sent it.

```toml
[[event_sinks]]
name = "ops-hook"
url = "https://hooks.example.com/fluxvm"
events = ["vm.", "node."]          # name prefixes; empty = every event
secret_file = "/etc/fluxvm/hook.secret"
```

`secret_file` is required and holds the HMAC key (a trailing newline is ignored). The secret is never inline in the config, never
logged, and is re-read on every attempt, so rotating it needs no restart. The daemon warns when the file is readable by group or
other. The daemon refuses to start on an invalid sink (bad URL, missing secret file path, duplicate name). Other keys:
`allow_hosts`, `queue_size` (1024), `max_attempts` (5), `backoff_base_ms` (500), `backoff_max_ms` (30000), `timeout_secs` (10).
See `config.example.toml`. Delivery starts from the end of the log when the daemon starts; history is not replayed.

## What is sent

`POST <url>` with `Content-Type: application/json`, the event as the body (`{"ts", "event", "vm_id"?, "fields"}`, the same shape
as `GET /v1/events`), and these headers:

| Header | Value |
|---|---|
| `X-Fluxvm-Signature` | `sha256=<hex>`: HMAC-SHA256 of `"<timestamp>.<body>"` with the secret |
| `X-Fluxvm-Timestamp` | Unix seconds when this attempt was signed |
| `X-Fluxvm-Delivery` | One id per event, the same on every retry: dedupe on it |
| `X-Fluxvm-Event` | The event name |

The timestamp is inside the signature so a captured request cannot be replayed with a new timestamp. To verify, recompute the HMAC
over the exact raw body bytes, compare in constant time, and reject timestamps more than a few minutes old
(`fluxvm_api::event_sinks::verify` does this). Sinks see every event of the host, not a tenant's slice.

## Delivery

Each sink has its own bounded queue and worker, so a slow receiver never delays another sink or the daemon. A 2xx is success.
Network errors, timeouts, 5xx, 408 and 429 are retried with exponential backoff (`base * 2^(retry-1)`, capped at `backoff_max_ms`).
Any other 4xx, and any 3xx (redirects are never followed), is final. Delivery is at-least-once and a retried event can arrive after a
later one.

Events that cannot be delivered are appended to `<state_dir>/event-sinks-dead.jsonl`, one JSON object per line
(`ts, sink, url, attempts, reason, event`). Reasons: `retries exhausted: ...`, a final HTTP status, `queue_full` (the queue held
`queue_size` events, so the new one was dropped), or `blocked: ...`. The URL is logged without its query string. Nothing replays
the file; it is for you to inspect or re-send. The file is not rotated.

## SSRF guard

A sink URL must be http or https, carry no credentials, and not point at link-local space (169.254.0.0/16, fe80::/10), the cloud
metadata addresses (169.254.169.254, `fd00:ec2::254`, 100.100.100.200) or names (`metadata.google.internal`), or unspecified, multicast
or broadcast addresses. This is checked at startup and again at every attempt against the addresses the host name resolves to, and the
connection goes to exactly those addresses (a name that later resolves to a metadata address is refused). Loopback and private
(RFC 1918) addresses are allowed, because delivering to an internal receiver is the normal case; run the daemon where it cannot reach
anything it should not. `allow_hosts = ["169.254.169.254"]` exempts a host or IP literal.

## Limits

Config-file only: there is no `fluxctl` subcommand to add or remove sinks, and changes need a daemon restart. Sinks get the VM
events the daemon writes (and not the `fluxvm-agent central` activity log). The queue is in memory: events queued when the daemon stops
are lost, not dead-lettered.
