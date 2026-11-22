# Tutorial: two bare Mac minis to a running 2-node k3s

You have two Apple-silicon Mac minis, a laptop, and nothing installed. By the end `kubectl get nodes` lists two nodes: a k3s server on
`mini-1` and an agent on `mini-2`. Every step is a `fluxctl` command, every step can be repeated, and each one says what it did.

> **Status.** The pieces are unit-tested (fake SSH transport, golden remote commands, a fake environment for `mac quickstart`). The whole
> path has **not been run end to end on two real Macs**: use `--dry-run` where offered and treat the first run as an acceptance test. See
> [What is not verified](#what-is-not-verified).

## What you need

- Two Apple-silicon Macs, macOS 14 or later, about 40 GiB free on each, on the same network, with a user logged in at the console (the
  daemon is a LaunchAgent in that session). Remote Login on: System Settings > General > Sharing.
- `fluxctl` on your laptop ([install-macos.md](install-macos.md)).
- Your SSH key on both Macs: `ssh-copy-id admin@mini-1.local && ssh-copy-id admin@mini-2.local`.

## 1. Look before you change anything

```bash
fluxctl mac-fleet doctor admin@mini-1.local admin@mini-2.local --k3s
```

Each problem prints the exact fix. The first time, ssh refuses a Mac whose host key it has not seen and says how to check the fingerprint
(`fluxctl explain SSH_HOSTKEY_MISMATCH` lists the options). Once you have checked it, add `--accept-new-hostkey` to the commands below, or
put the key in `~/.ssh/known_hosts` yourself.

## 2. Put FluxVM on both Macs

Write the inventory once; later commands reuse it with `-f fleet.yaml`.

```yaml
# fleet.yaml
user: admin
k3s: { cpus: 4, memory_mib: 8192, disk_gib: 60 }
hosts:
  - { host: mini-1.local, name: mini-1, k3s: true }
  - { host: mini-2.local, name: mini-2 }
```

```bash
fluxctl mac-fleet apply -f fleet.yaml --dry-run          # prints every ssh command, sends nothing
fluxctl mac-fleet apply -f fleet.yaml --accept-new-hostkey
fluxctl mac-fleet status -f fleet.yaml
```

`apply` installs a pinned, checksum-verified FluxVM, starts the daemon and, on `mini-1` only, a `kube agent` that creates the k3s server.
Wait until `status` shows `K3S running` for `mini-1` (the first start downloads k3s and takes a few minutes). Details:
[mac-fleet-quickstart.md](mac-fleet-quickstart.md).

## 3. Try a macOS instance on one Mac (optional)

Run this on a Mac, or through `fluxctl mac-fleet exec mini-1 -- ...`:

```bash
fluxctl mac quickstart --pull registry.example/macos-base:15
```

The guided flow: `mac doctor` with a fix for each problem, a template (imported with `--from DIR` or pulled with `--pull REF`), a hello-world
instance, and k3s with `--k3s`. It prompts only on a terminal; `--yes` takes the defaults and `--dry-run` shows the steps. A Mac runs at
most 2 macOS VMs at once: a `409` means both slots are taken (`fluxctl explain MAC_SLOT_LIMIT`). The k3s guest is Linux and uses no slot.

## 4. Join `mini-2` as an agent node

Copy the server token from `mini-1` over a secure channel into a file of mode 0600, then have `mini-2` create a VM and join it
([apple-k3s.md](apple-k3s.md#several-macs) has the full flags, including the verified k3s binary):

```bash
fluxctl mac-fleet exec mini-1 -- fluxctl kube apple status dev      # the API address to use as --url
fluxctl mac-fleet exec mini-2 -- fluxctl kube apple join remote --url https://mini-1.local:6443 \
    --token-file ~/server-token --binary ~/k3s-arm64 --sha256-file ~/sha256sum-arm64.txt
```

The two Macs must reach each other on the API port ([cross-mac-networks.md](cross-mac-networks.md) for private networks). A node that fails
to join is removed again, so you can run the command again.

## 5. Use it

```bash
fluxctl mac-fleet exec mini-1 -- fluxctl kube apple kubeconfig dev --output ~/dev.yaml
KUBECONFIG=~/dev.yaml kubectl get nodes        # after copying the file to the machine where you run kubectl
```

You should see the server and the joined agent `Ready`.

## When something fails

| You see | Run |
|---|---|
| `409`, both macOS slots taken | `fluxctl explain MAC_SLOT_LIMIT` |
| `503`, screen locked or no login session | `fluxctl explain MAC_SESSION_LOCKED` |
| `Host key verification failed` | `fluxctl explain SSH_HOSTKEY_MISMATCH` |
| `no such image` | `fluxctl explain MAC_TEMPLATE_MISSING` |
| `No space left on device` | `fluxctl explain MAC_DISK_FULL` |

`fluxctl mac cheatsheet` lists the commands you will use most. For tab completion of image, instance and host names:

```bash
source <(fluxctl completions zsh); source <(fluxctl mac complete-script zsh)     # or bash
```

## What is not verified

- Everything above on two real Macs: the ssh flow, the LaunchAgents started from an ssh session, and the first k3s creation.
- The cross-Mac join (step 4): [cross-mac-networks.md](cross-mac-networks.md) says nothing has run on two real Macs yet, and the exact
  flags of `kube apple join` may differ in your release (`fluxctl kube apple join --help`).
- `mac quickstart` drives the real `mac` and `kube` commands through the same binary; its flow is tested with a fake environment, not with a
  real macOS guest.
