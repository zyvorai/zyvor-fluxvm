# Fleet placement: anti-affinity, topology spread and automatic reschedule

`POST /fleet/vms` already places a VM by free capacity and `nodeSelector`. Two more optional fields shape *automatic* placement, and an
opt-in pass can recreate `reschedulable` VMs when their node stops heartbeating.

## Anti-affinity and topology spread

```json
{
  "name": "web-1",
  "reschedulable": true,
  "antiAffinity":   {"group": "web", "mode": "required"},
  "topologySpread": {"key": "zone", "maxSkew": 1, "mode": "preferred", "group": "web"}
}
```

Both fields are removed before the create is forwarded to a node, like `nodeSelector`.

- **Group.** A VM belongs to the group it was created with (`antiAffinity.group`, or `topologySpread.group`; if both are given they
  must match). Groups are per tenant: two tenants using the name `web` do not see each other's VMs, and the operator's own VMs form a
  separate set. The ledger remembers membership (`placement_groups` in `fleet-tenancy.json`) and forgets it when the VM ends.
- **`antiAffinity`**: at most one member of the group per node.
- **`topologySpread`**: members are counted per value of the node label `key` (set with `fluxvm-agent node --label zone=a`). A node
  may take the next member only if its zone's count plus one is at most `maxSkew` above the emptiest zone *that has a node able to take
  the VM* (a zone whose nodes are all down or full does not pin the minimum at zero). `maxSkew` defaults to 1. A node without the label
  is outside every domain and fails a `required` spread. This is Kubernetes' `topologySpreadConstraints` rule.
- **`mode`**: `required` filters candidates; if none is left the create fails with `503` and a message naming the rule. `preferred`
  (the default) is tried first and dropped when it would leave no candidate, so the VM is still placed. With both fields, a node must
  satisfy every `preferred` rule first; only if no node does is the set of `preferred` rules ignored as a whole (required rules always
  hold). It is not a weighted score.
- An explicit `"node"` bypasses both rules, as it bypasses `nodeSelector`; the VM still joins the group and counts for later creates.
- `reschedulable` VMs keep the fields in their stored spec, so a drain or an automatic reschedule re-applies them. During a drain the
  VM being moved still counts at its old node until it is deleted.

Limits: membership is only known for VMs created through central after this change (earlier VMs are not in any group); the check is
made at create time, so a later uncordon or a node's death does not rebalance anything by itself.

## Does heartbeat loss reschedule a VM?

Not by default. Before this change, a node whose last heartbeat is older than 30 seconds was only skipped for placement and listed
as unreachable; the ledger reconcile skips it too. A VM moved only through `POST /fleet/nodes/{name}/drain?mode=reschedule`, which an
operator runs and which needs the node to answer.

### Automatic reschedule (opt-in)

Set `FLUXVM_AUTO_RESCHEDULE_GRACE_SECS` on `fluxvm-agent central` (minimum 60; unset or 0 keeps it off). Optional:
`FLUXVM_AUTO_RESCHEDULE_FENCE_MARGIN_SECS` (default 30). A pass runs every 10 seconds on the leader. A node's active
`reschedulable` VMs are recreated elsewhere, through the same code as a drain, only when **all** of these hold:

1. This central is the leader and has been for at least the grace period. A restarted or newly elected central must not read its
   own downtime as every node dying (`last_seen` is only saved about once a minute).
2. The node's last heartbeat is older than the grace period.
3. The node is **fenced**. The node agent was started with `FLUXVM_NODE_SELF_FENCE_SECS=F` (minimum 20): if it has had no accepted
   heartbeat for `F` seconds it stops every VM on its own daemon (`POST /v1/vms/{id}/stop`, retried every second until all stops
   succeed) and advertises `self_fence:F` in its heartbeat. Central waits until the heartbeat is older than `F + margin`; the
   margin covers a slow stop. Set `F` well above the heartbeat interval times 8 (the node's retry ceiling).

A silent node that never advertised `self_fence` is **never** rescheduled: central cannot tell a crashed host from a partitioned one
still running the VM, and two copies of a VM on shared storage is worse than an outage. Such nodes are logged and reported
(`unfenced_nodes`); use drain by hand once you know the host is off.

After a reschedule the old ledger entry is closed and the node is cordoned and marked draining, so it takes no new VMs when it comes
back until an operator uncordons it. A failed recreate leaves the old entry in place and is retried on the next pass.

**What this does and does not guarantee.** It rests on the node agent running its watchdog and the local daemon honouring the stop.
It does not cover a node whose agent process is frozen (SIGSTOP, a stalled hypervisor) or whose daemon refuses the stop; a clock
that runs much faster on central than on the node; or a stop request the daemon needs auth for (the watchdog calls the local daemon
as the heartbeat does, without a token). The trade is availability for safety: a node that loses central for `F` seconds stops its
VMs even if central was the one that failed. Disk state is not carried (as with drain, a reschedule is create-elsewhere), and stopped
copies stay on the old node for the operator to delete. This is tested in process (decision rule, timings, the ledger and placement
effects); it has not been run against a real network partition.
