# Controlled host-path mounts

Status: parser + allowlist check landed. Virtiofs wiring is the next step.

```bash
# Requires [sandbox] allow_host_mounts = true and path under allowlist
fluxctl sandbox run nginx:alpine \
  --mount type=bind,src=$HOME/site,dst=/usr/share/nginx/html,ro
```

Implementation: `crates/fluxctl/src/sandbox_mount.rs`.
