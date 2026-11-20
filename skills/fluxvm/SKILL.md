---
name: fluxvm
description: Run untrusted or risky commands in a disposable FluxVM sandbox session, checkpoint and roll back, fork to try approaches, and review file changes before applying them. Use when a task should not touch the host directly (installing dependencies, running generated code, destructive refactors).
---

# FluxVM sandbox sessions

FluxVM gives you a hardware-isolated machine through MCP tools. Prefer it over running a command on the host when the
command is untrusted, generated, destructive, or needs packages you do not want installed locally.

Setup is done once by the user: `fluxctl mcp install <client> --allow-write`. If the tools below are missing, say so
and stop; do not try to install anything yourself.

## The loop

1. `session_create` with `policy`:
   - `offline` when no network is needed (also the only kind that can `session_fork` on a Mac);
   - `developer-safe` for GitHub, crates.io, PyPI, npm and the Go proxy;
   - a host list with `allow` for anything else. Do not use `open` unless the user asked for it.
2. `session_exec` to work. One shell string per call. Output and the exit code come back; a non-zero exit is data, not
   an error. Work under `/workspace` (created for you) so `session_artifacts` can report what changed.
3. `session_checkpoint` before any step you might want to undo; `session_restore` to go back.
4. `session_preview` (port) when you start a web server the user should see: it returns an expiring URL; share it
   only with who should see the app. Offline sessions have no address to preview.
5. For anything that runs longer than one call (a server, a build): `session_job_start`, then `session_job_read`
   with the `next_offset` it returned; `session_job_kill` to stop it. There is no stdin.
6. `session_file_write` / `session_file_read` / `session_files` to move files in and out and to look around; they
   are faster and safer than `cat`/heredocs through `session_exec`. About 1.5 MiB per write.
7. `session_git` (`status`, `diff`, `commit`, `branch`, `log`) for the workspace repo. If it answers that git is not
   installed, install it first (`sudo -n apt-get install -y git`; needs a session with network access). It never pushes.
8. `session_artifacts` to see added/modified/deleted files under `/workspace` before reporting the result.
9. `session_delete` when done. Sessions also expire by TTL.

## Trying several approaches

- On a forkable session (`capabilities.fork` is true in the session record), `session_fork` with `count` gives
  independent children that start from the current state. Run one candidate per child, compare, keep the best, delete
  the rest. Children do not merge back: apply the winner's change to the parent yourself.
- On any session, `sandbox_speculate` (with the session's `sandbox_id` as `vm`) runs a command in an isolated copy and
  returns a changeset; `sandbox_changeset_decide` with `approve` then `apply` writes it. Apply fails with a conflict if
  files changed since, which is correct: re-speculate.

## Limits you should state to the user rather than hide

- Create takes seconds, not milliseconds. Restore of a memory checkpoint takes seconds.
- `git` is not in the stock `debian-13` image: a `repo` clone reports why in `workspace.note`.
- Filesystem and process policy sections are recorded but not enforced yet; the network allow-list is.
- A session on the Mac's NAT (`open`) cannot fork.
