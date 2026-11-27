# fluxctl share

Ngrok-style port exposure for FluxVM guests.

## Goal

```bash
fluxctl share 8080
# prints: https://abc123.fluxvm.share
```

## Design

1. A small tunnel client runs on the host.
2. It registers with a FluxVM relay (or a self-hosted alternative).
3. Incoming traffic is forwarded to the local port.
4. The public URL is printed and optionally stored as a VM annotation.

## Status

Stub only. The CLI command returns a clear error until the tunnel is wired.
