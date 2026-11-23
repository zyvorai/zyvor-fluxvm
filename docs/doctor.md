# `fluxctl doctor`: check my whole setup

One command checks everything FluxVM depends on and fixes what it safely can. It adds no checks of its own to the
machine: it runs what [`fluxctl cluster status`](cluster-home.md) already runs and sorts the answer into a short list.

    fluxctl doctor                  # the list; changes nothing
    fluxctl doctor --fix            # apply the safe fixes, then show the list again
    fluxctl doctor -f fleet.yaml    # also reach the Macs of a fleet inventory (read-only ssh)
    fluxctl doctor --json           # for scripts and Studio (schema 1)
    fluxctl doctor --bundle         # if something is red, write a support bundle without asking

It works with no flags and with the daemon stopped (that is one of the red lines it reports).

    1 thing is broken.

    green   Virtual machines: This machine can start virtual machines.
    green   Disk space: 212 GiB free for FluxVM.
    yellow  Login session and heat: This machine is running hot: the CPU is limited to 60% speed ...
            -> Let it cool (move it out of the sun, clear the vents, stop heavy jobs) and check again.
    red     FluxVM service: FluxVM is not running properly on mini-01
            -> Run `fluxctl doctor --fix`; it is safe and does this for you.
    green   Kubernetes: No Kubernetes problems found.
    skipped Mac fleet: No Mac fleet was given, so none was checked.
            -> Run `fluxctl doctor -f inventory.yaml` to check it.
    yellow  Docker context: There is no `fluxvm` docker context yet. ...
    green   Registry logins: Logged in to ghcr.io as alice.
    green   Support bundle: Ready: one file with versions, health and logs, secrets removed.

Every yellow and red line says what to do next, with the exact command. A line that could not be checked is shown as
`skipped` and never changes the overall light. The exit code is 1 when something is red, otherwise 0.

## What each line covers

| Line | Comes from |
| --- | --- |
| Virtual machines, Disk space, Login session and heat, Other host setup | the host doctors (`fluxvm_core::health` items from `doctor`, `mac doctor` and `mac host headless-check`), plus the Mac CPU speed limit (`pmset -g therm`, `fluxvm_apple::host_health`) |
| FluxVM service | the daemon check, the LaunchAgent, stale client paths |
| Kubernetes | the k3s checks of the cluster health model |
| Mac fleet | every other machine of the `-f` inventory (or `FLUXVM_FLEET_INVENTORY`): unreachable, not installed, daemon down, and so on |
| Gateway add-on | the Gateway API items `cluster status` shows for the clusters `fluxctl kube up` made |
| Docker context | `fluxctl docker context status` |
| Registry logins | the `fluxctl mac login` file: unreadable or world-readable files are yellow; secrets are never printed |
| Support bundle | always ready; when something is red the line says so and, at a terminal, you are asked whether to create one now (`--bundle` answers yes) |

## `--fix`

`--fix` does only what is additive and reversible, and records it in the same change log as `fluxctl fix`:

* the safe fixes of `fluxctl fix` (start a stopped LaunchAgent, create a missing directory, pull a missing image);
* create or refresh the `fluxvm` docker context (it never makes it the current one);
* point MCP client entries at the current `fluxctl` when a `brew upgrade` removed the old path.

It never runs sudo and never changes a security setting. Those lines print the command for you to run.

## Studio

The **Home** page has a **Check my setup** card. Its button asks the daemon (`GET /v1/doctor`, read-only) and shows the
same list. The daemon cannot see your home folder or other machines, so the Docker context, registry logins, a fleet and
the gateway add-on show as "Not checked here" with the command to run in a terminal. When something is red the card
offers the support bundle download.

## The older report

The per-check report that `fluxctl doctor` printed before is still available as `fluxctl doctor --classic`, and
`--host-only` (used by `scripts/preflight.sh` on a machine FluxVM is not installed on) keeps that format too.
