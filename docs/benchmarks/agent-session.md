# AgentSession lifecycle timings

Measured with `scripts/bench-agent-session.sh` against a real `fluxctl serve`. These are the only numbers the agent
README and the website may quote; anything else is a target, not a result.

| | |
|---|---|
| Date | 2026-10-13 |
| Host | Apple M4 Mac mini, 16 GB, macOS 27.2 |
| FluxVM | 0.4.7 + the AgentSession branch, **debug build** of `fluxctl serve` (release builds will be faster) |
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
