# Registering a Mac as a fleet node

On each Mac, run the daemon and the node agent in the logged-in user's session (snapshot restore needs it; see
[launchd-notes.md](launchd-notes.md)):

```bash
fluxctl --config ~/.config/fluxvm/fluxvm.toml serve        # vz backend, with the density settings from examples/fluxvm-density.toml

fluxvm-agent node \
  --name studio-1 \
  --central https://fleet.example:7790 \
  --advertise-url http://studio-1.local:7788 \
  --token "$FLUXVM_AGENT_TOKEN" \
  --label kairon.zyvor.dev/backend.vz=true \
  --label kairon.zyvor.dev/chip=m4-max \
  --label kairon.zyvor.dev/memory-gib=64
```

To run the node agent as its own LaunchAgent (label `dev.zyvor.fluxvm-agent`, started at login, restarted if it exits) instead of by
hand, once `fluxvm-agent` is next to `fluxctl` (the macOS package ships it after 0.4.21):

```bash
fluxctl node-agent install --name studio-1 --central https://fleet.example:7790 \
  --advertise-url http://studio-1.local:7788 --token-file ~/.config/fluxvm/agent.token
fluxctl node-agent status
```

It adds the `kairon.zyvor.dev/backend.vz`, `chip` and `memory-gib` labels from this Mac itself (`--no-auto-labels` to skip,
`--label k=v` to add or override). The token is read from `--token-file`; it is never written into the plist. launchd has no start
ordering, so the agent may start before the daemon and keeps heartbeating until it is up. `fluxctl doctor` checks this agent too.

With Kairon's own `kairon-node` instead of `fluxvm-agent node`, it should read the same `GET /v1/sandboxes/density` from the local
daemon and report it the same way.

Every `fluxvm-agent node` heartbeat carries the host's totals, its VM count, the labels, and the daemon's `GET /v1/sandboxes/density` (memory-pressure
level, available memory, warm slots ready, sandbox counts). Nothing else has to report pressure; `memory_pressure` and `vm_stat` are
read by the daemon.

`fluxctl serve` listens on `127.0.0.1:7788` by default. For a remote central registry, set `listen` to an address the registry can
reach, turn on API auth, and pass that address as `--advertise-url`.

For the `agent-micro` image, register it in the catalog on every Mac (see [docs/agent-micro.md](../docs/agent-micro.md)); a Mac
without it fails creates that ask for it.
