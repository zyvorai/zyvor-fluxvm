# Agent mode for sandboxes

A sandbox designed for AI agents, not just humans.

```bash
# Create an agent sandbox with a hard budget and scoped token
fluxctl sandbox agent run \
  --image python:3.13 \
  --budget-seconds 300 \
  --budget-mib 1024 \
  --allow-host github.com \
  --token-scope "exec,read:/workspace" \
  --name agent-1

# The token is printed once and never stored in the VM record
# MCP clients can use it immediately
```

## Features

- **Hard budgets** — CPU time, memory, network bytes. Exceeding them kills the sandbox.
- **Scoped tokens** — The agent can only do what the token allows (exec, read specific paths, no network, etc.).
- **Egress allow-list** — Only the hosts you name.
- **Change-set** — Every file the agent touches is recorded; you can review or revert.
- **One-command MCP** — `fluxctl mcp serve` already exists; agent sandboxes are first-class tools.
- **Fork for parallel agents** — Combine with sandbox fork to run multiple agents on the same starting state.

## Why this is better than Docker

Docker has no concept of agent budgets, scoped tokens, or change-sets. You bolt them on with wrappers. FluxVM has them natively because the sandbox is already a governed microVM.

Implementation reuses the existing agent session machinery and adds the `sandbox agent` CLI surface.
