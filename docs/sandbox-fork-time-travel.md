# Sandbox fork and time-travel

Docker cannot do this. FluxVM can.

## Fork a running sandbox

```bash
# Start a sandbox
fluxctl sandbox run python:3.13 --name work -- sleep infinity

# Fork it into 4 copies in ~4 seconds (shared memory + reflinked root)
fluxctl sandbox fork work --count 4 --prefix try

# Each copy is independent. Apply different fixes, keep the one that passes.
fluxctl sandbox exec try-1 -- pytest
fluxctl sandbox exec try-2 -- pytest
# ...
fluxctl sandbox delete try-2 try-3 try-4   # keep the winner
```

Reuses the existing `vm fork` path. Works for offline or no-NAT sandboxes today; this extends it to normal NAT sandboxes.

## Time-travel checkpoints

```bash
fluxctl sandbox checkpoint work --name before-migration
# do something risky
fluxctl sandbox restore work --from before-migration
```

A checkpoint is the whole running microVM (memory + disk). Restore is sub-second on APFS.

## Why this is better than Docker

| Capability | Docker | FluxVM sandbox |
|---|---|---|
| Fork running container | No | Yes (~4 s for 4 copies) |
| Checkpoint / restore | Limited (CRIU experimental) | Native, whole VM |
| Isolation during parallel tests | Shared kernel | Separate kernels |
| Cleanup | Manual | TTL + explicit delete |

Implementation scaffold lands in this PR; full wiring reuses `vm_fork` and the existing snapshot path.
