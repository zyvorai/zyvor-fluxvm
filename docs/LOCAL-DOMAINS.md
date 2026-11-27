# Local domains and HTTPS

Design for Kiri-style local domains on FluxVM.

## Goal

Every running VM or container gets a stable name that resolves on the host:

- `http://<name>.fluxvm.local`
- `https://<name>.fluxvm.local` (served by a local CA)

## Components

1. **Name registry**
   - Maps VM id / name to a stable hostname.
   - Stored in the FluxVM control plane (not in the guest).

2. **Resolver**
   - On macOS: `/etc/resolver/fluxvm.local` or a small mDNS responder.
   - On Linux: a dnsmasq or systemd-resolved drop-in.
   - Points `*.fluxvm.local` at the FluxVM gateway.

3. **Local CA**
   - Generated once under `~/.fluxvm/tls/`.
   - Issues short-lived certs for each name.
   - On macOS the CA is added to the system trust store (user action).

4. **Gateway**
   - Terminates HTTPS and proxies to the VM's port.
   - Falls back to HTTP if no cert is available.

## CLI

```bash
fluxctl domains list
fluxctl domains set NAME --http 8080 --https 8443
fluxctl domains reset NAME
```

## Status

Design only. Implementation is the next PR.
