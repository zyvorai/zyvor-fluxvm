# Admin basics

Operate FluxVM on a host: service, ports, auth, logs, and production gates.

## Service

```bash
sudo systemctl enable --now fluxvm    # if unit installed by deploy
# or foreground:
sudo fluxctl --config /etc/fluxvm.toml serve
```

TTL reaper and warm-pool backfill run only while `serve` is up.

## Host dependencies

```bash
./scripts/bootstrap-host.sh
# Windows offline customize needs libhivex + nbd:
sudo modprobe nbd max_part=16
```

Remote: `./scripts/deploy-remote.sh USER@HOST`.

## Ports

| Port | Role |
|------|------|
| **7788** | FluxVM REST (`fluxctl serve`) |
| **9108** | Optional MicroVM Prometheus (`MICROVM_METRICS_ADDR`) |

Fabric (separate product) typically listens on **9095** and proxies FluxVM.

## Health

```bash
curl -sf http://127.0.0.1:7788/healthz
curl -sf http://127.0.0.1:7788/readyz | jq .
```

Both are auth-exempt. Use `/readyz` when dataplane must be ready before work.

## Auth (opt-in)

Default: open API (every request is admin). Before exposing beyond localhost:

```toml
[auth]
require = true

[[auth.tokens]]
token = "replace-me"
role = "admin"
name = "ops"
# tenant = "team-a"   # optional scope
```

```bash
curl -sf -H "Authorization: Bearer replace-me" http://127.0.0.1:7788/v1/vms
```

List/filter: `GET /v1/vms?tenant=team-a`. See [PRODUCTION.md](../PRODUCTION.md)
and [SECURITY.md](https://github.com/zyvorai/zyvor-fluxvm/blob/main/SECURITY.md).

## State and logs

| Path | Role |
|------|------|
| `<state_dir>/vms.json` | VM inventory (flock via `vms.lock`) |
| `<state_dir>/instances/<uuid>/console.log` | Per-VM console |
| `<state_dir>/instances/<uuid>/serial.sock` | QEMU serial socket (`fluxctl serial`) |
| `<state_dir>/instances/<uuid>/disks/` | QEMU data disks (`fluxctl disk`) |
| `<state_dir>/events.jsonl` | Lifecycle/audit events (`fluxctl events`) |
| `<state_dir>/vm-templates.json` | VM templates (`fluxctl vm-template`) |
| `<state_dir>/backups/` | VM backups (`fluxctl backup`) |
| `journalctl -u fluxvm -f` | Daemon journal |

## AppArmor and Kairon

On Ubuntu the AppArmor profile (`deploy/apparmor/fluxvm`) runs in enforce mode. After
upgrading, reload it:

```bash
sudo install -m 0644 deploy/apparmor/fluxvm /etc/apparmor.d/fluxvm
sudo apparmor_parser -r /etc/apparmor.d/fluxvm
```

If a Kairon Machine goes to `Error` with `opening base image
/var/lib/kairon-node/image-cache/...: Permission denied`, the loaded profile is older
than the rule that lets FluxVM read Kairon's image cache. Confirm with
`journalctl -k | grep 'apparmor="DENIED"'`, then reload as above. If you edited
`/etc/apparmor.d/fluxvm` by hand (for example an extra QEMU path), merge the new rule
instead of overwriting the file. A `storage=shared` disk outside `/var/lib/fluxvm`
also needs the profile's file-lock permission for QEMU's image lock.

## Live migration across hosts

A migration receiver boots the same concrete machine type as the source (for example
`pc-q35-noble`), and creating one fails up front when the receiving host's QEMU does
not list that type: migrate to the same or a newer QEMU release. A receiver that is
still receiving past its time limit is kept until the transfer ends. Details:
[operations](../operations.md).

## Production checklist

```bash
./scripts/release-checklist.sh
```

- [PRODUCTION.md](../PRODUCTION.md)
- [examples/create-vm-prod.json](../../examples/create-vm-prod.json)
- [production tutorials](../tutorials/production/README.md)

## Related

- [Getting started](getting-started.md)
- [Configuration](configuration.md)
- [Common workflows](workflows.md)
- [Kubernetes deployment](kubernetes-deployment.md)
