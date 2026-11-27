# Innovative features

Ideas that play to FluxVM's strengths (multi-hypervisor, density, eBPF, vsock agent)
instead of copying Docker.

## 1. Dirty-log guided warm pools

Use the dirty-log bitmap from incremental snapshots to decide which pages
are hot. Prefetch only those pages into a fork-backed pool so the next
clone starts faster than a cold restore.

Status: design. Builds on the dirty-log work already on main.

## 2. Cross-hypervisor placement hints

When a VM is created, score each backend (Firecracker, Cloud Hypervisor,
in-tree KVM, Apple VZ) for the workload and record the reason in an
annotation. Operators can override; the default is the highest score.

Status: design.

## 3. Agent-driven identity reset on clone

After a fork or snapshot restore, the guest agent automatically resets
hostname, machine-id and entropy so clones do not collide. Already partly
present; this makes it the default for every clone path.

Status: partly implemented.

## 4. eBPF anomaly events

Surface a simple event when a VM's network behavior diverges from its
baseline (new outbound destinations, sudden packet rate). Operators can
subscribe via the existing metrics path.

Status: design.

## 5. One-command density report

`fluxctl density` prints PSS/private/RSS per VM and the estimated savings
if the next N clones used the warm pool.

Status: design.
