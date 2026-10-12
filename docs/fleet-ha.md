# A highly available fleet registry

One `fluxvm-agent central` is a single point of failure: if it is down, no VM can be created through the fleet. With
`--ha-redis`, you run two or three centrals against one Redis. One of them leads and serves `/fleet/*`, and the
others stand by.

**Status:** implemented and tested in process (two centrals on a shared in-memory store: the standby refuses requests,
then takes over with the leader's tenants and nodes after the leader dies). Not yet run against a real Redis or on
several hosts.

```bash
# On each of central-a, central-b (and optionally central-c):
fluxvm-agent central --token "$FLEET_TOKEN" \
  --ha-redis redis://redis.internal:6379 \
  --advertise-url http://central-a.internal:7799

# On every Mac: list all centrals; the heartbeat goes to whichever one leads.
fluxvm-agent node --name studio-1 --central http://central-a.internal:7799,http://central-b.internal:7799 \
  --advertise-url http://studio-1.local:7788
```

## How it works

- **Leader lock.** A Redis key holds the leader's `--advertise-url`, with a TTL (`--ha-ttl-secs`, default 10). The
  leader renews it every third of the TTL. When the leader dies, the first standby to try after the TTL lapses takes
  the lock.
- **Promotion.** The new leader loads the mirrored state before it serves anything:
  - tenants, leases, the VM ledger, policy and drain state are replaced wholesale;
  - nodes are merged, so a node that has heartbeated to the new leader since keeps its fresher record.
- **Mirrored, fenced state.** Every tenancy change is mirrored to Redis as it happens, and the node registry every
  renewal. Each write checks, atomically in a Lua script, that the writer still holds the lock. A leader that was
  paused (a GC stall, a VM freeze, a partition) past its TTL wakes up unable to overwrite its successor's state, and
  steps down.
- **Standbys** answer every fleet request with `503`, `{"leader": "<url>"}` and an `x-fluxvm-leader` header. They
  never redirect, so bearer tokens are only ever sent to URLs you configured.
- **Losing Redis.** A leader's right to serve is a lease that ends two-thirds of a TTL after its last successful
  renewal (each Redis call is also time-limited). If the leader cannot reach Redis, it stops serving when the lease
  ends: it can no longer prove it is the only leader, and it would rather refuse requests than risk two.

## Clients

| Client | What to point at it |
|---|---|
| `fluxvm-agent node` | the comma-separated list of every central (it fails over itself, starting with the last one that answered) |
| `fluxctl fleet …`, `gha-runners`, Kairon, scripts | a load balancer or a DNS name in front of all centrals, health-checking `GET /fleet/ha/leader` (200 on the leader, 503 elsewhere) |

`GET /fleet/ha` shows a central's own view (`me`, `leader`, `is_leader`). Like `/healthz`, it needs no token.

## Choosing the TTL

The TTL is how long a dead leader blocks a takeover, so the fleet is read-only for up to the TTL plus one renewal
interval. Shorter TTLs fail over faster, but a leader that stalls for longer than the TTL steps down. 10 s is a good
default. Node heartbeats arrive every 10 s, and a node counts as unhealthy after 30 s, so a failover never marks nodes
unhealthy.

## Requirements and limits

- All centrals use the same `--token`; `--ha-redis` refuses to start without one.
- Redis itself must be highly available (Sentinel, a managed Redis, or a cluster with failover), or it becomes the new
  single point of failure.
- Quota reservations for creates in flight live only on the leader. A failover during a create can briefly let a
  tenant go one VM over quota. The reconciler corrects the ledger afterwards.
- Each central keeps its local JSON files as well, so a single central keeps working if `--ha-redis` is removed.
- Keys are `fluxvm:fleet:{<cluster>}:leader` and `…:state:<name>`. The braces are a hash tag, so this works on Redis
  Cluster too.
- A clean shutdown does not release the lock yet, so the standby takes over after the TTL either way.
- The node registry is mirrored on each renewal. A cordon or deregistration in the last third of a TTL before a leader
  crash can be lost. Tenancy changes are mirrored immediately.
