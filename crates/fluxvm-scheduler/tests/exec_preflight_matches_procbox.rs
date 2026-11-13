// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxvm_guest_protocol::exec_preflight::preflight` (used by `fluxctl exec-policy check`) must give the same answer
//! as the real decision in `fluxvm_procbox::landlock::plan` for every policy shape and Landlock ABI, so the offline
//! tool cannot drift from what the guest agent does. Pure: `plan` takes the kernel ABI as an argument.

use fluxvm_guest_protocol::exec_preflight::{MAX_KNOWN_ABI, preflight};
use fluxvm_guest_protocol::{ExecPolicy, ExecTcpRule};
use fluxvm_procbox::landlock::plan;
use fluxvm_procbox::{Policy, TcpRule};
use std::path::PathBuf;

fn rule(r: &ExecTcpRule) -> TcpRule {
    match r {
        ExecTcpRule::Any => TcpRule::Any,
        ExecTcpRule::Deny => TcpRule::Deny,
        ExecTcpRule::Ports(p) => TcpRule::Ports(p.clone()),
    }
}

/// The fields `plan` looks at, converted the way the guest agent's `to_procbox` does.
fn to_procbox(p: &ExecPolicy) -> Policy {
    Policy {
        exec: p
            .exec
            .as_ref()
            .map(|x| x.iter().map(PathBuf::from).collect()),
        tcp_connect: rule(&p.tcp_connect),
        tcp_bind: rule(&p.tcp_bind),
        scope_ipc: p.scope_ipc,
        best_effort: p.best_effort,
        ..Policy::default()
    }
}

#[test]
fn preflight_agrees_with_procbox_plan_over_the_whole_matrix() {
    let tcp = [
        ExecTcpRule::Any,
        ExecTcpRule::Deny,
        ExecTcpRule::Ports(vec![443]),
    ];
    let mut checked = 0;
    for connect in &tcp {
        for bind in &tcp {
            for scope_ipc in [false, true] {
                for best_effort in [false, true] {
                    for exec in [None, Some(vec!["/usr".to_string()])] {
                        let p = ExecPolicy {
                            tcp_connect: connect.clone(),
                            tcp_bind: bind.clone(),
                            scope_ipc,
                            best_effort,
                            exec,
                            ..ExecPolicy::default()
                        };
                        for abi in 0..=MAX_KNOWN_ABI {
                            let ours = preflight(&p, abi);
                            let real = plan(&to_procbox(&p), abi);
                            let ctx = format!("abi {abi}, policy {p:?}");
                            match real {
                                Err(_) => assert!(
                                    ours.refused.is_some(),
                                    "procbox refuses, preflight runs: {ctx}"
                                ),
                                Ok(plan) => {
                                    assert!(
                                        ours.refused.is_none(),
                                        "procbox runs, preflight refuses: {ctx}"
                                    );
                                    assert_eq!(
                                        ours.not_enforced, plan.enforcement.not_enforced,
                                        "{ctx}"
                                    );
                                    let has = |s: &str| ours.enforced.iter().any(|e| e.contains(s));
                                    assert_eq!(
                                        has("filesystem read"),
                                        plan.enforcement.filesystem,
                                        "{ctx}"
                                    );
                                    assert_eq!(
                                        has("executable allow-list"),
                                        plan.enforcement.exec_restricted,
                                        "{ctx}"
                                    );
                                    assert_eq!(
                                        has("TCP connect"),
                                        plan.enforcement.tcp_connect,
                                        "{ctx}"
                                    );
                                    assert_eq!(has("TCP bind"), plan.enforcement.tcp_bind, "{ctx}");
                                    assert_eq!(
                                        has("scoping"),
                                        plan.enforcement.scope_signal,
                                        "{ctx}"
                                    );
                                }
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert_eq!(checked, 3 * 3 * 2 * 2 * 2 * 8);
}
