# `fluxctl mac runner`: one ephemeral macOS VM per GitHub Actions job

> **Status.** Unit-tested with a fake GitHub and a fake ssh transport. **Not run against GitHub, and no real macOS guest was booted.** Try it on
> a throwaway repository first. See [What is not verified](#what-is-not-verified).

For each job: a just-in-time (JIT) runner is registered through the GitHub API, the scheduler of [mac-fleet-scheduler.md](mac-fleet-scheduler.md)
picks a Mac, that Mac creates a fresh guest from your template (`fluxctl mac run --rm`), the runner inside takes exactly one job and exits, and
the guest is deleted. A runner that never got a job is unregistered. Nothing is shared between jobs.

```bash
# the template: a macOS image with the Actions runner unpacked in /Users/admin/actions-runner (tool installs belong in the template)
fluxctl mac-fleet templates distribute -f fleet.yaml --template registry.example/ci-mac:1@sha256:<64 hex>

fluxctl mac runner --repo acme/app --token-file ~/.config/gh-runner.token \
    --template registry.example/ci-mac:1@sha256:<64 hex> --fleet fleet.yaml --label fluxvm
# workflow: runs-on: [self-hosted, macOS, ARM64, fluxvm]
```

Without `--fleet` the jobs run on this Mac (the same scripts run through `/bin/sh`). `--once` serves one job and exits; `--plan` prints what would happen
and calls nothing.

## The token and the JIT configuration

- The GitHub token needs permission to create runners: a fine-grained token with "Administration: write" on the repository, or "Self-hosted runners: write" on the
  organisation, plus "Actions: read" to see queued jobs. It is read from `--token-file` (must be mode 0600) or `--token-stdin`; there is no flag that takes the
  token itself. It is held in memory and used only as the `Authorization` header of requests to `--api-url` (https only).
- GitHub answers each runner request with a JIT configuration. It goes to the Mac on **ssh stdin**, is read into the `ACTIONS_RUNNER_INPUT_JITCONFIG` environment variable
  there and copied into the guest with `mac run -e`. It is in no argument on this machine or on the Mac, in no script, and is scrubbed from the job's output. The guest
  command that starts the runner does carry it (that is how `mac run -e` hands it over), so it is visible to processes inside that one-job guest; it is single use.
- Error text from GitHub (the `message` of a failed request) is shown; request headers and the token never are.

## How many VMs

Never more than **2 per Mac** (Apple's limit): `--max-jobs` can lower the total, not raise it above 2 times the number of Macs. Every start goes through the scheduler,
which counts the guests a Mac already runs (including ones that are not the runner's) and waits for a slot up to `--queue-timeout`; if none frees, the JIT runner is removed
again and the loop tries at its next look. Runners to start = queued jobs plus the busy ones plus `--warm`, capped. For `--repo` the queue is read from the Actions API (jobs
`queued` whose labels all belong to the runner: `self-hosted`, `macOS`, `ARM64` and your `--label`s, in runs that are queued or in progress, first 50 runs of each); for `--org` GitHub gives no
org-wide queue, so use `--warm N` (default 1) to keep N runners waiting.

## What is not verified

- Any request to GitHub (`generate-jitconfig`, listing runs, jobs and runners, deleting a runner): the shapes were written from the REST documentation and are only tested against a fake.
- That the Actions runner in a guest accepts its JIT configuration from `ACTIONS_RUNNER_INPUT_JITCONFIG` and exits after one job; that `mac run -e` passes the value into the guest intact.
- Everything the scheduler page lists, over real ssh and real guests.
- Killing the `fluxctl mac runner` process leaves running guests and registered runners behind (no signal handler yet): `fluxctl mac ps` shows guests named `fleet-*`, and unused JIT runners are removed by GitHub after a day.
