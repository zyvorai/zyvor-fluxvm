# Support bundle

One file with what a maintainer needs to help you, with the secrets taken out.

```sh
fluxctl support-bundle                 # writes ~/fluxvm-support-<timestamp>.tar.gz
fluxctl support-bundle --out ~/b.tgz   # somewhere else
fluxctl support-bundle --log-lines 300 # fewer log lines (10 to 10000, default 1000)
fluxctl support-bundle --json          # print the path, size, files and skipped parts as JSON
fluxctl support-bundle --list          # show exactly which files it would hold; write nothing
fluxctl support-bundle --yes           # do not ask "Write it? [Y/n]" on a terminal
```

Before anything is written it prints every file (with its size) and every skipped part, then, on a terminal, asks once. The bundle is
built on this machine and is never sent anywhere. In Studio, **Show what is included** (`GET /v1/support-bundle/preview`, admin) lists
the same files before you download.

If `fluxctl` itself crashes (a panic), it prints the normal report and then tells you to run `fluxctl support-bundle`.

It needs no flags, no set-up and no running daemon. It prints the path and the line
**Review before sharing; secrets are removed but check for anything private.** The file is readable by you only (mode 600) and is never
replaced if it exists. Nothing is uploaded: you decide who gets it.

In Studio: **Home**, **Support bundle**, **Download support bundle** (the daemon builds the same archive; `POST /v1/support-bundle`, admin
role; **Copy as fluxctl** copies the command above). A token limited to one tenant cannot download it, because the bundle describes the
whole host.

## What is inside

| File | What |
| --- | --- |
| `README.txt`, `MANIFEST.txt` | The review note; every file with its size and how many secrets were removed; **every part that was skipped and why** |
| `versions.json` | fluxctl, daemon, OS, architecture, kernel, host name, listen address |
| `cluster-health.json` | The same document as `fluxctl cluster status --json` (from the daemon, the same data as `GET /v1/cluster/health`) |
| `instances.json` | VM list: id, name, status, backend, size, error, log path. The request body (environment, user data, keys) is **not** included |
| `clusters.json` | The cluster list (`GET /v1/kube`) |
| `config-effective.json` | The settings in effect, with tokens removed |
| `host.txt` | CPUs, memory, free disk, `df`, network interfaces |
| `logs/daemon.log` | The last N lines of the daemon log (`journalctl -u fluxvm`, or `/var/log/fluxvm.log`, the Homebrew log, `~/Library/Logs/fluxvm.log`) |
| `logs/vm-NAME-ID.console.log` | The last lines (at most 500) of the console log of up to 20 VMs, newest first |

## When the daemon is down

The bundle is still written. The health report, the settings, the host facts and any logs on disk are collected locally; the
VM list and cluster list need the daemon and are listed under "Skipped" with the reason and the next step (usually
`fluxctl fix`, or `fluxctl explain DAEMON_DOWN`). `fluxctl support-bundle` prints the same list.

## What is removed

Secrets never enter the bundle, in two ways.

1. **Files that are never read:** kubeconfigs (`kubeconfig`, `fluxvm-*.yaml` under `.kube`), private keys and key stores (`*.pem`,
   `*.key`, `id_*`, `*.p12`, `*.pfx`, `*.jks`), `.env*`, `credentials*`, `secrets*`, `.netrc`, `.npmrc`, `.pgpass`, and anything under
   `.ssh`, `.kube`, `.aws`, `.gnupg`, `.docker`.
2. **Text that is redacted** (`fluxvm_core::support_bundle::redact`, a pure function with its own tests). Replaced with `[REDACTED]`:
   PEM private key blocks (also a truncated one, to the end of the text); `Authorization`, `Proxy-Authorization`, `Cookie`,
   `Set-Cookie`, `X-Api-Key` and similar header values; `bearer` tokens; the password in `scheme://user:password@host`; secret query
   parameters (`?token=`, `?api_key=`, signatures); any `key: value`, `key=value` or JSON `"key": "value"` whose key contains
   `password`, `secret`, `token`, `api key`, `private key`, `credential`, `client-key-data`, `access key`, `cookie`, `session id` and
   similar (also lists of them); command-line flags such as `--token X`, `--password X`, `curl -u user:pass`; Docker `"auth"`;
   well-known token formats (GitHub, AWS access key id, Slack, Anthropic/OpenAI-style `sk-`, Stripe, Google API keys, npm, GitLab, JWTs,
   k3s join tokens, `fxst_` session tokens, age keys).

Then a **final scan** reads the finished archive back and runs every rule again over every file. If anything still matches, or an
excluded file name is present, **no bundle is written** and you get `SUPPORT_BUNDLE_SECRET_LEFT` (`fluxctl explain
SUPPORT_BUNDLE_SECRET_LEFT`): send `fluxctl cluster status --json` and `fluxctl doctor` output by hand instead.

### What the redaction does not catch

- A secret with no key name, no known prefix and no `Authorization`-style header in front of it (for example a bare random password
  on a line of its own, or a password that is part of a free-text sentence).
- Multi-line values (a YAML block scalar under `password: |`) beyond their first line, and PEM bodies cut into pieces with no `BEGIN`
  line (logs that wrap a key line by line).
- `-pPASSWORD` style flags and secrets in non-URL connection strings that do not use a secret-looking key name.
- Private data that is not a credential: host names, IP addresses, VM names and image names, user names, file paths. This is why the
  note says to review the files before you share them.
- It over-redacts on purpose: a harmless key such as `max_tokens: 100` also loses its value.

## Errors

| Code | Meaning |
| --- | --- |
| `SUPPORT_BUNDLE_SECRET_LEFT` | The final scan found something that looks like a secret; nothing was written |
| `SUPPORT_BUNDLE_WRITE_FAILED` | The file could not be created or written (folder missing, disk full, file exists): use `--out` |

## Sharing an instance instead

`fluxctl share NAME` (a stopped macOS instance) writes one self-contained file and prints the `fluxctl import FILE` line that restores it
on another Mac; `--serve` also offers it once over local HTTP (127.0.0.1 unless `--bind` says otherwise) under a one-time token, then
stops. Registry logins, tokens, build secrets, SSH keys, environment values and host folders are never read into it and are listed in the
output; anything stored inside the guest's own disk travels with it, and the output says so. Nothing is uploaded.
