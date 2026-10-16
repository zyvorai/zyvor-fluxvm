# AgentSession lifecycle timings

Measured with `scripts/bench-agent-session.sh` against a real `fluxctl serve`. These are the only numbers the agent
README and the website may quote; anything else is a target, not a result.

| | |
|---|---|
| Date | 2026-10-13 |
| Host | Apple M4 Mac mini, 16 GB, macOS 27.2 |
| FluxVM | Historical: 0.4.7 + the AgentSession branch, **debug build** of `fluxctl serve` (sessions ship in 0.4.8+; current release 0.4.10 is faster as a release build) |
| Guest | the default sandbox: `debian-13`, 2 vCPU, 2 GiB, `vz`, SSH exec (no vsock agent) |
| Runs | 5, `policy=open`, snapshots are memory+disk, state dir already holding the image and a warm slot |

| Step | Median | Worst |
|---|---|---|
| create session (sandbox ready for commands) | 6.8 s | 26.9 s |
| first exec (`true`) | 0.12 s | 0.13 s |
| checkpoint (memory + disk) | 0.72 s | 0.78 s |
| restore that checkpoint | 4.1 s | 4.4 s |
| delete | 3.5 s | 4.1 s |

## Same host, `policy=offline` (no network card; the sandbox can be forked)

5 runs; the fork is 2 children with `identity=best-effort` and the children are deleted afterwards.

| Step | Median | Worst |
|---|---|---|
| create session | 4.6 s | 5.1 s |
| first exec (`true`) | 0.39 s | 0.40 s |
| checkpoint (memory + disk) | 0.68 s | 0.72 s |
| restore | 3.9 s | 4.3 s |
| fork into 2 running children | 2.5 s | 2.5 s |
| delete | 1.6 s | 1.6 s |

An offline guest has no NAT path, so exec goes over vsock to the guest's sshd and costs more per call (0.39 s vs 0.12 s).
A live check ran a command in each of two children and in the parent: all three started from the parent's state, wrote
different files, and the parent saw none of them.

## Release build (same host, same image)

`cargo build --release -p fluxctl`, `fluxctl serve` on a fresh state dir that already held the image. Both tables are
`scripts/bench-agent-session.sh`; the script now counts failed creates and prints why.

| Step (seconds, median / worst) | `policy=offline`, 10 runs | `policy=open`, 15 runs |
|---|---|---|
| create session | 4.71 / 5.42 | 1.04 / 7.46 |
| first exec (`true`) | 0.10 / 0.11 | 0.11 / 0.13 |
| checkpoint (memory + disk) | 0.68 / 0.80 | 0.66 / 1.18 |
| restore | 3.64 / 14.65 | 2.86 / 4.62 |
| fork into 2 children | 2.25 / 4.29 | not applicable |
| delete | 1.73 / 4.19 | 2.23 / 3.44 |

What this says, and does not say:

- **A release build did not make create faster.** Offline create is 4.7 s in both builds: the time is the VM, not the
  Rust code. The debug build's "release will be faster" note above was wrong for these steps.
- **`open` create is 1.0 s when a warm slot is available** (the pool pre-boots NAT sandboxes) and about 7.5 s when it is
  not; repeated back-to-back creates alternate between the two while the pool refills. Offline sessions do not use the
  pool, which is why they are always 4-5 s. A warm slot for offline sessions is the obvious next speed-up, unbuilt.
- **Create can fail or stall under churn.** In an earlier 10-run `open` pass one create returned HTTP 400 (sandbox
  created, then removed by the daemon; the reason was not captured because the script swallowed the body) and a probe
  loop saw one create take about 120 s. The 15-run pass above had 0 failures, so this is intermittent, **not
  characterised, and not fixed**. It lives in sandbox creation, below the session layer.
- Restore had one 14.7 s outlier in the offline table.
- Still not met: the <300 ms claim, <150 ms restore and <100 ms fork targets.

## How to read this

- **Create is seconds, not milliseconds.** The worst run (26.9 s) was the first one, which had no warm slot to claim and
  paid a cold boot plus the pool refill; the median is dominated by claiming a warm slot and restoring it. The goal of
  sub-second claim/restore in `docs/agent-sessions.md` is a **target** and is not met on `vz` today.
- **Exec is fast** because the daemon already holds an SSH connection path to the guest; this is the per-command cost,
  not boot.
- **Restore is also seconds**: on `vz` it relaunches the VM from the saved state.
- **Fork** works on `vz` for a session with no network card of its own (the offline table above); it returned 409 for
  the NAT sessions of the `open` table, by design. The in-tree KVM fork (Linux) was not measured: this host has no KVM.
- One host, one guest image, a debug build, 5 runs: treat the table as an order of magnitude, not a benchmark suite.
  The script prints the same table for your own hardware.
