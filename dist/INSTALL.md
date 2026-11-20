# Installing zyvor-fabric v0.1.0

A self-contained Linux (x86_64) distribution of **zyvor-fabric**, the control
plane that drives [FluxVM](https://github.com/zyvorai/fluxvm) as its VM
backend. No `cargo`, `npm`, or network access is required on the target
machine — everything needed is already built into the package.

## What's in the package

```
zyvor-fabric-0.1.0-linux-x86_64/
├── bin/            zyvor-fabricd, zyvorctl, fluxvm
├── vendor/         guestkit-agent-cli, zyvor-guest-agent, fluxvm-guest-agent
├── configs/        default zyvor-fabricd.toml, fluxvm.toml, pam.d, logrotate.d
├── systemd/        zyvor-fabricd.service, fluxvm.service
├── web/            the built web dashboard
├── install.sh      offline installer (this is the only script you run)
└── VERSION
```

## Prerequisites

- A Linux x86_64 host with systemd (tested on Debian/Ubuntu and RHEL-family
  distributions)
- KVM support (`/dev/kvm` present) for VM acceleration
- Root access to install system-wide (binaries under `/usr/bin`, `/usr/local/bin`,
  systemd units, and runtime state under `/var/lib/zyvor-fabricd`, `/var/lib/fluxvm`)

## Install

```bash
tar xzf zyvor-fabric-0.1.0-linux-x86_64.tar.gz
cd zyvor-fabric-0.1.0-linux-x86_64
sudo ./install.sh --start
```

`--start` enables and starts both `fluxvm.service` and `zyvor-fabricd.service`
immediately after installing. Leave it off if you'd rather review the generated
configs under `/etc/zyvor-fabricd/` and `/etc/fluxvm.toml` first, then start
them yourself:

```bash
sudo systemctl enable --now fluxvm.service
sudo systemctl enable --now zyvor-fabricd.service
```

The installer is idempotent for configuration: it only writes
`/etc/zyvor-fabricd/zyvor-fabricd.toml` and `/etc/fluxvm.toml` if they don't
already exist, so re-running `install.sh` to upgrade binaries won't clobber
any local config changes.

## First login

zyvor-fabricd generates a random admin password on first startup:

```bash
sudo cat /var/lib/zyvor-fabricd/.admin_password
```

Open the dashboard (self-signed TLS by default — your browser will warn once,
that's expected for a fresh install):

```
https://<host>:9095
```

Log in as `admin` with that password. From the API instead of the UI:

```bash
TOKEN=$(curl -sk -X POST https://<host>:9095/api/auth/login \
  -H "Content-Type: application/json" \
  -d '{"username": "admin", "password": "<password-from-file>"}' | jq -r .token)

curl -sk -H "Authorization: Bearer $TOKEN" https://<host>:9095/api/vms
```

## Quick tutorial: create your first VM

Via the CLI:

```bash
zyvorctl create my-first-vm --image=/path/to/image.qcow2 --cpus=2 --memory=2048
zyvorctl start my-first-vm
zyvorctl list
```

Via the API:

```bash
curl -sk -X POST https://<host>:9095/api/vms \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"name": "my-first-vm", "image": "/path/to/image.qcow2", "cpus": 2, "memory": 2048}'

curl -sk -X POST https://<host>:9095/api/vms/my-first-vm/start \
  -H "Authorization: Bearer $TOKEN"
```

From the dashboard: **Create VM** → fill in image/CPU/memory → **Create**, then
open the VM's **Console** tab for an in-browser terminal or VNC session.

### Networking

The **Network** page lets you create bridges, VLANs, bonds, and taps, and
configure a per-bridge DHCP server (Bridges tab → the router icon on a bridge
row). A bridge with DHCP configured also serves DNS for any Zones/Policies
you set up on the **Network Security → DNS** page — one nameserver handles
both your internal records and normal internet resolution for VMs on that
bridge.

## Verify the install

```bash
zyvor-fabricd-ctl status
zyvor-fabricd-ctl verify
```

`verify` runs an end-to-end smoke test (service up, API responding,
authentication working, VM create/delete) and reports pass/fail for each
check.

## Evaluation trial

This build includes a 30-day evaluation trial, starting from first launch.
Existing VMs and read access (viewing VMs, networks, etc.) remain available
after the trial lapses — only new writes (create/modify/delete) require a
current trial or license. Check remaining days at any time:

```bash
curl -sk -H "Authorization: Bearer $TOKEN" https://<host>:9095/api/license
# {"trial":true,"days_remaining":30,"expired":false}
```

## Troubleshooting

**Service won't start** — check the logs:

```bash
sudo journalctl -u zyvor-fabricd -n 50 --no-pager
sudo journalctl -u fluxvm -n 50 --no-pager
```

**Dashboard unreachable** — confirm the service is listening:

```bash
curl -sk https://localhost:9095/health   # expect: OK
```

**Permission errors managing VMs** — zyvor-fabricd needs to run as root (the
systemd unit already does this); if running the binary manually for testing,
use `sudo`.

## Upgrading

Extract a newer release's tarball and re-run `sudo ./install.sh` — binaries
and systemd units are overwritten, existing config files and VM state are
left alone. Restart both services afterward:

```bash
sudo systemctl restart fluxvm.service zyvor-fabricd.service
```
