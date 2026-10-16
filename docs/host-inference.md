# Host inference for Apple VZ sandboxes

Linux guests on Apple Virtualization cannot run MLX or use Metal GPU
passthrough. FluxVM instead wires the **host** OpenAI-compatible model server
into the guest over vsock when `apple.host_gpu` is set.

```text
guest curl http://127.0.0.1:8080/v1/...
        │  (cloud-init fluxvm-infer forwarder)
        ▼
vsock CID 2 port 8080  →  runner TCP relay  →  127.0.0.1:{host_infer_port}
                                                    │
                                    mlx_lm.server / llama-server / stand-in
```

No guest network card is required. Offline and allow-listed sandboxes work.

## Quick start

```bash
# terminal 1 — real model, or a stub:
fluxctl infer serve --stand-in
# or:  mlx_lm.server --port 8080
# or:  llama-server --port 8080

fluxctl infer status          # GET /v1/host/infer

# sandbox with host GPU:
curl -s http://127.0.0.1:7788/v1/sandboxes -H 'Content-Type: application/json' -d '{
  "name": "infer-demo",
  "offline": true,
  "host_gpu": true,
  "ttl_seconds": 900
}'

# agent session:
fluxctl agent run --policy offline --host-gpu
fluxctl agent exec <id> -- 'curl -s "$OPENAI_BASE_URL/models"'
```

Or set `"apple": {"host_gpu": true}` on any `vz` Linux create spec. The daemon
adds the vsock relay to `[apple].host_infer_port` (default `8080`) and installs
the guest forwarder plus `OPENAI_BASE_URL` / `FLUXVM_INFER_URL`.

## Config

```toml
[apple]
host_infer_port = 8080   # loopback port of the host model server
```

## Guest contract

| Item | Value |
|------|--------|
| Guest URL | `http://127.0.0.1:8080/v1` |
| Env | `OPENAI_BASE_URL`, `FLUXVM_INFER_URL` (injected on exec; also `/etc/profile.d/fluxvm-infer.sh`) |
| Vsock port | `8080` (reserved for this feature when `host_gpu` is set) |

## Limits

- Host Metal only: the guest never sees a GPU device.
- Quotas / token accounting for inference are not enforced yet.
- Custom Virtio bulk path for lower latency is a follow-up (see pillar 1 roadmap).
- OCI sandboxes: set `apple.host_gpu` on the underlying VM spec; env injection for
  `oci-init` processes may need the image to read `/etc/environment`.

## Demo and live test

[`examples/agent-sandbox-demo/local-model.sh`](../examples/agent-sandbox-demo/local-model.sh)
(`STAND_IN=1` or a real model on `:8080`) and
[`scripts/vz-host-gpu-live-test.sh`](../scripts/vz-host-gpu-live-test.sh) exercise the path against a
running daemon. Prefer `host_gpu: true` for new scripts.
