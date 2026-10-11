# Ephemeral GitHub Actions runners on FluxVM

`fluxvm-agent gha-runners` gives every queued GitHub Actions job its own fresh VM on the fleet. It works for macOS jobs
(Xcode CI on a tenant's leased Macs) and Linux jobs (arm64, with Rosetta for amd64 binaries). Each VM serves one job and
is then deleted.

**Status:** implemented and tested against a fake GitHub API and fleet. Not yet run against github.com.

## How it works

Every poll (15 s by default):

1. **Retire.** For every runner it started, if GitHub no longer knows the runner, the job is done (a just-in-time runner
   unregisters itself after one job), so the VM is deleted. A runner older than `job_timeout_secs` is unregistered and
   its VM deleted.
2. **Scale up.** For each new queued job in the configured repositories, the scaler finds the first pool whose labels
   include all of the job's labels. If the pool has room (`max_runners`), it:
   1. asks GitHub for a JIT runner configuration;
   2. creates a VM through central (`POST /fleet/vms`), so leases, quotas and placement apply;
   3. starts the runner in the guest.

On start the scaler adopts runners from a previous run and deletes orphans: a `fluxvm-gha-*` VM without a runner, or a
runner registration without a VM.

## Setup

1. **Image.** Install the [actions runner](https://github.com/actions/runner/releases) into the image at
   `/opt/actions-runner`, owned by a non-root user such as `runner`. For macOS, do this in the template; for Linux, in
   the base disk. Then distribute the image with `fluxctl vm-image push`.
2. **Tokens.**
   - `GITHUB_TOKEN`: a fine-grained token with Actions read access and runner administration. That is
     "Administration: write" on the repositories, or "Self-hosted runners: write" on the organization.
   - `FLUXVM_AGENT_TOKEN`: a fleet tenant token, so runner VMs land on that tenant's leased Macs and count against its
     quota.
3. **Config.** See [examples/gha-runners.json](../examples/gha-runners.json).

```bash
export GITHUB_TOKEN=github_pat_… FLUXVM_AGENT_TOKEN=fvt_…
fluxvm-agent gha-runners --config gha-runners.json
```

In the workflow:

```yaml
runs-on: [self-hosted, macOS, ARM64, fluxvm]
```

## Config

| Field | Default | Meaning |
|---|---|---|
| `github.owner`, `github.repos` | (required) | Repositories whose queued jobs are watched |
| `github.scope` | `repo` | `repo` registers runners on each repository; `org` registers them on the organization |
| `github.runner_group_id` | `1` | Runner group for JIT registration |
| `github.api_url` | `https://api.github.com` | For GitHub Enterprise Server, use `https://HOST/api/v3` |
| `fleet.url` | (required) | `fluxvm-agent central` |
| `pools[].labels` | (required) | Include `self-hosted` |
| `pools[].vm` | (required) | A `POST /fleet/vms` body; `name` and, if unset, `ttl_seconds` are filled in |
| `pools[].launch` | `cloud-init` | `cloud-init` appends the start command to `cloud_init.runcmd` (Linux). `exec` runs it through the guest agent after boot (macOS) |
| `pools[].runner_user` | (none) | Run the runner as this user. Unset runs it as root with `RUNNER_ALLOW_RUNASROOT=1` |
| `pools[].max_runners` | `2` | Concurrent runners in the pool. On macOS, keep this at or below 2 × leased Macs (Apple's two-guest limit) |
| `pools[].job_timeout_secs` | `21600` | Hard limit per runner |
| `pools[].boot_timeout_secs` | `600` | How long `exec` waits for the guest agent |
| `webhook.listen` | (off) | Listen for GitHub `workflow_job` webhooks at `POST /github/webhook`; queued jobs start a runner at once |
| `webhook.secret_env` | `GITHUB_WEBHOOK_SECRET` | The webhook secret. Deliveries without a valid `X-Hub-Signature-256` get a `401` |

### Webhook mode

Polling alone starts a runner up to `poll_interval_secs` after a job is queued. With `webhook` set, a `queued` delivery
starts a runner immediately, and a `completed` delivery retires finished runners immediately. Polling keeps running
as a safety net for missed deliveries and restarts.

To set it up, add a webhook on the repository or organization:
- payload URL `https://<host>/github/webhook` (put TLS in front of the listener);
- content type `application/json`;
- the secret from `GITHUB_WEBHOOK_SECRET`;
- event "Workflow jobs" only.

Deliveries for repositories that are not in `github.repos` are ignored.

## Security notes

- GitHub lists are followed page by page (`Link: rel="next"`, up to 20 pages of 100), and only on the configured API
  host, so the token is never sent anywhere else.

- A JIT config can register exactly one runner, once. It is passed in the create body (`cloud-init`) or the exec call,
  and is never logged. Runner VMs are created without `"reschedulable"`, so central never stores the body.
- `ttl_seconds` defaults to `job_timeout_secs + boot_timeout_secs`, so a VM goes away even if the scaler dies.
- Values spliced into the guest command (runner directory, user, JIT config) are checked against a strict character set.

## Not done yet

- Warm pools: a VM restored from a snapshot already waiting for a job.
- GitLab and Buildkite.
