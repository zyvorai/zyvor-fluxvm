// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A dry-run planner that relieves overloaded Macs: which VMs could move off a host above its memory high-water mark.
//!
//! Placement packs (see [`crate::apple_placement`]); this does not level load. It only looks at hosts above
//! `high_water` (a fraction of the memory a VM may use, which already excludes macOS's headroom) and proposes moving
//! *movable* VMs onto the tightest host that stays at or under it afterwards. Virtualization.framework has no live
//! migration, so a "move" is recreate-elsewhere: only VMs that are reschedulable or ephemeral are candidates, and macOS
//! guests never are (they are bound to a machine identity). The plan changes nothing; the operator applies it.

use serde::Serialize;

/// Default fraction of allocatable memory above which a host is considered overloaded.
pub const DEFAULT_HIGH_WATER: f64 = 0.85;

#[derive(Debug, Clone, PartialEq)]
pub struct RebalanceNode {
    pub name: String,
    /// Memory VMs may use on this host (unified memory minus macOS headroom).
    pub allocatable_mib: u64,
    pub used_mib: u64,
    pub free_cpu: u32,
    /// A cordoned or unhealthy host may be a source but never a destination.
    pub accepts_vms: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RebalanceVm {
    pub id: String,
    pub node: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Reschedulable or ephemeral, and not a macOS guest.
    pub movable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Move {
    pub vm: String,
    pub from: String,
    pub to: String,
    pub vcpus: u32,
    pub memory_mib: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HostLoad {
    pub node: String,
    pub used_mib: u64,
    pub allocatable_mib: u64,
    pub utilisation: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RebalancePlan {
    pub high_water: f64,
    pub moves: Vec<Move>,
    /// Hosts still above the mark after the moves, with the reason nothing more could be done.
    pub unresolved: Vec<HostLoad>,
    pub before: Vec<HostLoad>,
    pub after: Vec<HostLoad>,
}

fn util(used: u64, alloc: u64) -> f64 {
    if alloc == 0 {
        // No usable memory at all: treat any use as overload and an empty host as full, so it never receives work.
        return if used > 0 { f64::INFINITY } else { 1.0 };
    }
    used as f64 / alloc as f64
}

fn loads(nodes: &[RebalanceNode], used: &[u64]) -> Vec<HostLoad> {
    nodes
        .iter()
        .zip(used)
        .map(|(n, u)| HostLoad {
            node: n.name.clone(),
            used_mib: *u,
            allocatable_mib: n.allocatable_mib,
            utilisation: util(*u, n.allocatable_mib),
        })
        .collect()
}

/// At most `max_moves` moves. Deterministic: hosts and VMs are taken in a fixed order (most loaded first, then name;
/// smallest VM that is enough, then id).
pub fn plan_rebalance(
    nodes: &[RebalanceNode],
    vms: &[RebalanceVm],
    high_water: f64,
    max_moves: usize,
) -> RebalancePlan {
    let high_water = high_water.clamp(0.1, 1.0);
    let mut used: Vec<u64> = nodes.iter().map(|n| n.used_mib).collect();
    let mut cpu: Vec<u32> = nodes.iter().map(|n| n.free_cpu).collect();
    let before = loads(nodes, &used);
    let mut moved: Vec<bool> = vec![false; vms.len()];
    let mut moves: Vec<Move> = Vec::new();

    // Sources, most loaded first.
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by(|&a, &b| {
        util(used[b], nodes[b].allocatable_mib)
            .partial_cmp(&util(used[a], nodes[a].allocatable_mib))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(nodes[a].name.cmp(&nodes[b].name))
    });

    for &src in &order {
        while moves.len() < max_moves && util(used[src], nodes[src].allocatable_mib) > high_water {
            let excess = used[src] as f64 - high_water * nodes[src].allocatable_mib as f64;
            // Movable VMs here, smallest first.
            let mut cand: Vec<usize> = vms
                .iter()
                .enumerate()
                .filter(|(i, v)| !moved[*i] && v.movable && v.node == nodes[src].name)
                .map(|(i, _)| i)
                .collect();
            cand.sort_by(|&a, &b| {
                vms[a]
                    .memory_mib
                    .cmp(&vms[b].memory_mib)
                    .then(vms[a].id.cmp(&vms[b].id))
            });
            // The smallest VM that clears the excess on its own, else the largest one (it reduces the most).
            let pick = cand
                .iter()
                .copied()
                .find(|&i| vms[i].memory_mib as f64 >= excess)
                .or_else(|| cand.last().copied());
            let Some(vi) = pick else { break };
            let v = &vms[vi];
            // Tightest destination that stays at or under the mark and has the CPU.
            let dst = (0..nodes.len())
                .filter(|&d| d != src && nodes[d].accepts_vms && cpu[d] >= v.vcpus)
                .filter(|&d| {
                    nodes[d].allocatable_mib > 0
                        && util(used[d] + v.memory_mib, nodes[d].allocatable_mib) <= high_water
                })
                .max_by(|&a, &b| {
                    util(used[a], nodes[a].allocatable_mib)
                        .partial_cmp(&util(used[b], nodes[b].allocatable_mib))
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(nodes[b].name.cmp(&nodes[a].name))
                });
            let Some(d) = dst else {
                // This VM has nowhere to go; a smaller one might, so try the rest before giving up on the host.
                moved[vi] = true;
                continue;
            };
            used[src] = used[src].saturating_sub(v.memory_mib);
            used[d] += v.memory_mib;
            cpu[d] -= v.vcpus;
            cpu[src] = cpu[src].saturating_add(v.vcpus);
            moved[vi] = true;
            moves.push(Move {
                vm: v.id.clone(),
                from: nodes[src].name.clone(),
                to: nodes[d].name.clone(),
                vcpus: v.vcpus,
                memory_mib: v.memory_mib,
            });
        }
    }

    let after = loads(nodes, &used);
    let unresolved = after
        .iter()
        .filter(|l| l.utilisation > high_water)
        .cloned()
        .collect();
    RebalancePlan {
        high_water,
        moves,
        unresolved,
        before,
        after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, alloc: u64, used: u64) -> RebalanceNode {
        RebalanceNode {
            name: name.into(),
            allocatable_mib: alloc,
            used_mib: used,
            free_cpu: 8,
            accepts_vms: true,
        }
    }

    fn vm(id: &str, node: &str, mem: u64, movable: bool) -> RebalanceVm {
        RebalanceVm {
            id: id.into(),
            node: node.into(),
            vcpus: 2,
            memory_mib: mem,
            movable,
        }
    }

    #[test]
    fn nothing_moves_when_no_host_is_over_the_mark() {
        let p = plan_rebalance(
            &[node("a", 10_000, 8_000), node("b", 10_000, 1_000)],
            &[vm("v1", "a", 2_000, true)],
            0.85,
            10,
        );
        assert!(p.moves.is_empty() && p.unresolved.is_empty());
    }

    #[test]
    fn moves_the_smallest_vm_that_clears_the_excess_to_the_tightest_fit() {
        // a: 9500/10000 (excess 1000 over 8500). b is 6000/10000, c is 1000/10000.
        let nodes = [
            node("a", 10_000, 9_500),
            node("b", 10_000, 6_000),
            node("c", 10_000, 1_000),
        ];
        let vms = [
            vm("small", "a", 500, true),
            vm("enough", "a", 1_200, true),
            vm("big", "a", 4_000, true),
        ];
        let p = plan_rebalance(&nodes, &vms, 0.85, 10);
        assert_eq!(p.moves.len(), 1);
        assert_eq!(p.moves[0].vm, "enough");
        assert_eq!(
            p.moves[0].to, "b",
            "tightest host that stays under the mark"
        );
        assert!(p.unresolved.is_empty());
        assert_eq!(p.after[0].used_mib, 8_300);
    }

    #[test]
    fn never_moves_what_is_not_movable_and_reports_the_host() {
        let p = plan_rebalance(
            &[node("a", 10_000, 9_500), node("b", 10_000, 0)],
            &[vm("pinned", "a", 3_000, false)],
            0.85,
            10,
        );
        assert!(p.moves.is_empty());
        assert_eq!(p.unresolved.len(), 1);
        assert_eq!(p.unresolved[0].node, "a");
    }

    #[test]
    fn a_destination_is_never_pushed_over_the_mark_or_out_of_cpu() {
        // b would go to 9000/10000 with the VM: refused. c has memory but no CPU. d is cordoned.
        let mut c = node("c", 10_000, 0);
        c.free_cpu = 1;
        let mut d = node("d", 10_000, 0);
        d.accepts_vms = false;
        let p = plan_rebalance(
            &[node("a", 10_000, 9_500), node("b", 10_000, 6_000), c, d],
            &[vm("v", "a", 3_000, true)],
            0.85,
            10,
        );
        assert!(p.moves.is_empty());
        assert_eq!(p.unresolved.len(), 1);
    }

    #[test]
    fn a_vm_with_no_home_does_not_block_a_smaller_one() {
        let nodes = [node("a", 10_000, 9_800), node("b", 10_000, 7_000)];
        // The excess is 1 300 MiB: only "huge" clears it alone, but it cannot fit on b (10 000 > 8 500); "fits" can.
        let vms = [vm("huge", "a", 3_000, true), vm("fits", "a", 1_000, true)];
        let p = plan_rebalance(&nodes, &vms, 0.85, 10);
        assert_eq!(
            p.moves.iter().map(|m| m.vm.as_str()).collect::<Vec<_>>(),
            ["fits"]
        );
    }

    #[test]
    fn max_moves_bounds_the_plan_and_the_result_is_deterministic() {
        let nodes = [
            node("a", 10_000, 10_000),
            node("b", 10_000, 0),
            node("c", 10_000, 0),
        ];
        let vms: Vec<_> = (0..6)
            .map(|i| vm(&format!("v{i}"), "a", 1_000, true))
            .collect();
        let p1 = plan_rebalance(&nodes, &vms, 0.5, 2);
        assert_eq!(p1.moves.len(), 2);
        assert_eq!(p1, plan_rebalance(&nodes, &vms, 0.5, 2));
    }

    #[test]
    fn a_host_with_no_usable_memory_is_not_a_destination() {
        let p = plan_rebalance(
            &[node("a", 10_000, 9_500), node("tiny", 0, 0)],
            &[vm("v", "a", 2_000, true)],
            0.85,
            10,
        );
        assert!(p.moves.is_empty());
    }
}
