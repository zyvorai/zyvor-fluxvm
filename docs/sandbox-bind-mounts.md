# Controlled host-path mounts

Status: **not available yet.** `crates/fluxctl/src/sandbox_mount.rs` (parser + allowlist check) is a scaffold that is not
compiled into `fluxctl`; there is no `--mount` flag or `allow_host_mounts` key on `main`. The example shows the intended
syntax. See [DOCKER-FREE.md](DOCKER-FREE.md#2-bind-mounts-host-paths) for workarounds.

```bash
# Requires [sandbox] allow_host_mounts = true and path under allowlist
fluxctl sandbox run nginx:alpine \
  --mount type=bind,src=$HOME/site,dst=/usr/share/nginx/html,ro
```

Implementation: `crates/fluxctl/src/sandbox_mount.rs`.
