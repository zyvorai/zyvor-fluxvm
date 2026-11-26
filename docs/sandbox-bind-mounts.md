# Controlled host-path mounts for sandboxes

Named volumes remain the safe default. This adds an opt-in path for host bind mounts via virtiofs, with explicit allowlists.

```bash
# Requires [sandbox] allow_host_mounts = true and a path under the allowlist
fluxctl sandbox run nginx:alpine \
  --mount type=bind,src=/Users/me/site,dst=/usr/share/nginx/html,ro
```

## Security model

- Off by default (`allow_host_mounts = false`).
- Paths must be under a configured allowlist (e.g. `~/src`, `~/work`).
- Default is read-only; writable requires an extra flag and is logged.
- Mounts are virtiofs shares, same mechanism as named volumes and shared folders.
- No arbitrary `/` or system path mounts.

## Status

This PR adds the design, config keys, and CLI flag skeleton. The virtiofs plumbing reuses the existing volume and shared-folder paths.

## Acceptance

- [ ] Allowlisted read-only bind mount works
- [ ] Path outside allowlist is refused
- [ ] Feature is disabled by default
- [ ] Audit log records the mount
