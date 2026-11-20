# Egress policy for user-mode (slirp) guests

A guest on QEMU user-mode networking has no interface on the host, so the tc dataplane (`network/policy`) cannot govern
it. Its traffic is re-originated by the **QEMU process**, and that is what `bpf/fluxvm_qemu_egress.bpf.c` filters
(`BPF_CGROUP_INET_EGRESS` on `fluxvm.slice/<id>.scope`). Bridged/tap guests are governed by the tc dataplane instead;
the same program also confines QEMU's own sockets for them.

## Modes

| mode | effect |
|---|---|
| `loopback` | only `127.0.0.0/8` and `::1` leave the cgroup. **Default**; a guest has no network. |
| `allowlist` | loopback plus the rules. |
| `audit` | everything allowed; traffic an allow-list would have dropped is recorded. Use it to learn what a guest needs. |
| `open` | everything allowed. |

A rule is `{cidr, proto: any|tcp|udp, port?, note?}`. `port` needs `tcp` or `udp`; omit it to allow every port. Each
network can carry at most four distinct `(proto, port)` pairs once the ports of covering networks are folded in
(the kernel returns only the longest matching prefix, so FluxVM folds them in when it writes the map).

## Where the policy comes from

1. the VM's own policy (`PUT /v1/vms/{id}/network/egress`, or `CreateVmRequest.egress`),
2. the node default (`PUT /v1/network/egress/defaults`),
3. `[sandbox.dataplane] qemu_egress_mode` / `qemu_egress_rules` in `fluxvm.toml`,
4. `loopback`.

A running VM takes a change at once; a stopped one at its next start. The node default reaches running VMs only with
`?apply_running=true`. A VM that started before this feature has no policy maps and must be restarted.

## Observe, suggest, apply

```
fluxctl network egress-set  <vm> --mode audit
# …use the guest…
fluxctl network egress-observed <vm>
fluxctl network egress-suggest  <vm>            # rules that would allow what was seen
fluxctl network egress-suggest  <vm> --apply    # add them and switch to allowlist
```

Hosts hit on the same port within one /24 collapse to the /24.

## Things that surprise people

* **Cilium rewrites service addresses before the hook.** A guest dialling a ClusterIP or NodePort is seen by the filter
  as the backend `pod-ip:port`. Allow the backend (or its CIDR), not the service address. A `hostPort` on the node IP is
  not rewritten.
* IPv4 only. IPv6 is dropped in `loopback`/`allowlist` and allowed in `audit`/`open`.
* `fluxctl network policy` / the tc dataplane and this policy are independent; a bridged guest needs the former.

REST: see `docs/api.md`. Config example: `config.example.toml`.
