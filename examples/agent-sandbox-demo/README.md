# An agent's loop in a safe, forkable sandbox on a Mac

`run.sh` plays the loop a coding agent runs, against a local FluxVM daemon on Apple silicon, with real timings:

1. an **offline sandbox** (no network device, only `lo`) holds a small Python project whose test fails;
2. it is **forked into four running copies** that share the parent's memory snapshot;
3. each copy applies a **different fix** and runs the tests, **in parallel**;
4. the copy whose tests pass is kept and its diff printed; the other copies and the parent are deleted.

```bash
fluxctl service install            # or: fluxctl serve
examples/agent-sandbox-demo/run.sh # FLUXVM_URL / FLUXVM_TOKEN if the daemon has auth
```

The "agent" is four fixed candidate patches so the run is deterministic. Each step is one MCP tool, so a model can do
the same through `fluxctl mcp serve --allow-write` (see [docs/mcp.md](../../docs/mcp.md)): `sandbox_create` (with
`offline`), `sandbox_exec`, `vm_fork`, `sandbox_exec` per copy, `vm_delete`.

## What it measured

On an Apple M4 mini (16 GB, macOS 27.2), `debian-13` guest, daemon on loopback, two runs:

| Step | Time |
|---|---|
| Offline `standard` sandbox ready | 4.4 s, 4.5 s |
| Fork into 4 running copies | 4.0 s, 3.4 s (parent snapshot 0.65-0.68 s, slowest child 2.7-3.3 s) |
| Four fixes applied and tested in parallel | 0.3 s, 0.3 s |
| Result | attempt 1 passes, 2-4 fail, in both runs |

## What you need to know before relying on it

- **Only a sandbox with no NAT can be forked.** A default sandbox sits on the Mac's NAT, and its children would share the
  parent's MAC and address, so the daemon refuses (`400`). `offline: true` (or `apple.usernet` with no port forwards) is
  forkable. That is also the safer shape for an agent.
- **The children keep the parent's identity.** The demo forks with `"identity": "best-effort"`, which applied nothing: an
  offline sandbox has no guest agent over SSH, so the daemon cannot reset the hostname, machine-id or SSH host keys. All
  four copies report the same `machine-id`. That is fine for throwaway attempts and wrong for anything that must be
  distinct (anything that registers itself somewhere).
- **The first sandbox on a fresh daemon was slow:** 61 s with an empty state directory (image preparation and a cold
  boot), against 4.5 s afterwards. Warm the daemon (or `fluxctl sandbox warm --count N`) before timing anything.
- Memory per sandbox is measured separately: [docs/agent-density.md](../../docs/agent-density.md).
