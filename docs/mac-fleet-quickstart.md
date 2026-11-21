# Mac fleet quickstart: many Mac minis, one command line

`fluxctl mac-fleet` sets up FluxVM on several Macs over SSH from your laptop: install, daemon, fleet enrollment and, if you want,
a k3s guest on each. Nothing is pulled by a script you cannot read, nothing blind-trusts a host, and every step can be repeated.

> **Status.** Unit-tested with a fake SSH transport and golden-tested remote commands. It has **not been run against a real Mac**
> yet: expect to meet a rough edge, and use `--dry-run` first. See [What is not verified](#what-is-not-verified).

## Three commands

```bash
# 1. once per Mac: let your key in (Remote Login must be on: System Settings > General > Sharing)
ssh-copy-id admin@mini-1.local && ssh-copy-id admin@mini-2.local

# 2. look before you change anything: every ssh command is printed, nothing is sent
fluxctl mac-fleet up admin@mini-1.local admin@mini-2.local --central https://fleet.example:7790 \
    --token-file ~/join.token --k3s --dry-run

# 3. do it (first connection to each Mac: verify its host key, or add --accept-new-hostkey knowingly)
fluxctl mac-fleet up admin@mini-1.local admin@mini-2.local --central https://fleet.example:7790 \
    --token-file ~/join.token --k3s
fluxctl mac-fleet status
```

`status` prints one table:

```
HOST                  STATE  VERSION  DAEMON  VMS  MACOS  K3S      NODE
admin@mini-1.local    ok     0.4.21   up      1    1/2    running  enrolled
admin@mini-2.local    ok     0.4.21   up      0    0/2    starting joining
```

`MACOS 1/2` is the macOS guests running of the two Apple allows per Mac; k3s guests are Linux and do not use a slot.

## What `up` does on each Mac

1. **Probe** (read-only) and **preflight**, stopping with the exact fix when something is wrong: Apple silicon, macOS 14 or later, free disk
   (20 GiB, 40 with `--k3s`), and that the ssh user is the one logged in at the console (the daemon is a LaunchAgent in that session).
   Warnings, not errors: FileVault on (nothing runs after a reboot until someone unlocks the disk) and no automatic login (no session after a
   reboot). A Mac meant to run unattended wants FileVault off and automatic login as a dedicated user ([agent-density.md](agent-density.md#unattended-hosts)).
2. **Install** the release you named, pinned: `--release` (default: the version of the `fluxctl` you are running; `latest` is refused). The package
   and its published `.sha256` are downloaded on the Mac and compared; `--sha256 HEX` also pins the hash you recorded. Unpacked under
   `~/.local/share/fluxvm/<version>`, linked into `~/.local/bin`. `--release-base` points at a mirror, `--via brew` uses Homebrew (the tap was
   withdrawn after 0.4.21 and cannot pin a version).
3. **Daemon**: `fluxctl service install`, then waits for `/healthz`.
4. **Enroll** (with `--central`): the join token is written to `~/.config/fluxvm/agent.token` (mode 0600) from ssh **stdin**, `fluxctl node-agent
   install --token-file` starts the node agent, and once the node holds its own credential the token file is deleted. A Mac that is already enrolled
   is not sent the token again. Make short-lived, single-use tokens ([fleet-enroll.md](fleet-enroll.md)).
5. **k3s** (with `--k3s`): `fluxctl kube agent install`, a LaunchAgent that creates and keeps the cluster. The first start downloads an image and takes
   minutes; `--k3s-wait SECS` waits, otherwise `status` shows `starting` then `running`. `--k3s-cpus/--k3s-memory-mib/--k3s-disk-gib` size it.

Each step checks what is already true, so running `up` again on a converged Mac changes nothing. Macs run in parallel (`--jobs`, default 8). When one
fails the others finish, the table says which and why, and the exit status is non-zero.

## Inventory: `apply`

```yaml
# fleet.yaml  (JSON works too)
version: 0.4.21
central: https://fleet.example:7790
token_file: ~/join.token          # a path on this machine, never the token
user: admin
k3s: { cpus: 4, memory_mib: 8192, disk_gib: 60 }
hosts:
  - host: mini-1.local
    k3s: true
  - host: mini-2.local
    labels: [zone=lab]
  - { host: 10.0.0.7, name: studio, user: ops, port: 2222 }
```

```bash
fluxctl mac-fleet apply fleet --dry-run     # `fleet` finds fleet.yaml, .yml or .json
fluxctl mac-fleet apply fleet               # converge every host, then print status
```

`apply` adds and repairs; it does not remove Macs that left the file unless you pass `--prune` (see [mac-fleet-scheduler.md](mac-fleet-scheduler.md#removing-macs-apply---prune)), or use `remove`.
To run jobs on these Macs with a scheduler, drain a Mac, or distribute templates, see [mac-fleet-scheduler.md](mac-fleet-scheduler.md); for GitHub Actions runners, [mac-runner.md](mac-runner.md). Unknown keys in the file are errors, so a typo is caught.
Other commands take `-f fleet.yaml` and then default to all its hosts, or name some of them: `fluxctl mac-fleet status -f fleet.yaml mini-1`.

## The other commands

| Command | What it does |
|---|---|
| `doctor HOST...` | Preflight only, each finding with its exact fix. `--enroll`, `--k3s` add what those need. |
| `status HOST...` | The table above; `--json` for scripts. |
| `exec HOST... -- CMD` / `exec --all -f fleet.yaml -- CMD` | Run a command everywhere. One argument is shell text (`'df -h ~'`); several are separate words. `PATH` includes `~/.local/bin`. |
| `upgrade --release X` | Install X, repoint the LaunchAgents (they hold the versioned path) and restart them. Refuses a Mac with running VMs unless `--force`. |
| `remove --yes` | Stop and delete the LaunchAgents and the install. VM images and data stay; `--k3s` also deletes the k3s guest, `--purge` the node credential. Without `--yes` nothing happens. |

`--dry-run` works on all of them and prints lines like

```
ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 ... -- admin@mini-1.local '/bin/sh -c '\''...'\'''
```

## SSH and secrets

- The system `ssh` is used with `BatchMode=yes`: it never prompts, so a missing key fails in seconds instead of hanging a parallel run.
- Host keys are **pinned**: `StrictHostKeyChecking=yes`. A Mac whose key is not in `~/.ssh/known_hosts` (or `--known-hosts FILE`) is refused with the commands
  to check its fingerprint and add it. `--accept-new-hostkey` trusts a key on first sight (`accept-new`); it is off unless you pass it, and a *changed* key is
  always refused.
- Host names and users are validated so they cannot be read as ssh options. Versions, URLs, labels and names that go into scripts are validated or quoted.
- The join token is read from `--token-file` (must be mode 0600) or `--token-stdin`, sent on stdin, never in argv, never in a script, never printed;
  anything token-shaped in remote output is replaced by `<redacted>`.
- `exec` command text is visible in `ps` on the Mac: do not put secrets in it.

## What is not verified

Not run on any real Mac, so read the commands with `--dry-run` first:

- the whole flow over real `ssh` (the transport is only tested through a fake and by golden ssh command lines);
- `launchctl print gui/<uid>/...` from an ssh session, `fdesetup status`, `defaults read` of the login window and `stat /dev/console` without sudo;
- `fluxctl service install`, `node-agent install` and `kube agent install` being started from an ssh session (they should load into the console user's GUI session);
- the first-run k3s creation by `kube supervise`, and `fluxctl kube status` JSON as read for the K3S column;
- the upgrade rewrite of the LaunchAgent plists (`sed` of the versioned path, then `launchctl bootout` and `bootstrap`);
- `--via brew` (the tap is withdrawn).

Releases up to 0.4.21 do not ship `fluxvm-agent`, so enrolling needs a later release (`doctor --enroll` says so).
