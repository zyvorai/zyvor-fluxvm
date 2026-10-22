// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Compact, dependency-free mirror of `fluxvm-procbox`'s `Policy` and
//! `Enforcement`, carried on `AgentRequest::Exec` / `AgentResponse::Exec`.
//!
//! The JSON shape matches procbox's policy JSON, so a policy file written for
//! `fluxvm-procbox` can be passed through unchanged (unknown fields are
//! ignored). The guest agent converts these to the real procbox types; this
//! crate stays free of procbox's dependencies.

use serde::{Deserialize, Serialize};

/// Landlock TCP rule for one direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecTcpRule {
    Any,
    Deny,
    Ports(Vec<u16>),
}

/// What seccomp does on a denied syscall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecSeccompMode {
    Errno,
    Kill,
}

/// Namespace isolation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecIsolation {
    #[default]
    Off,
    Auto,
    Strict,
}

/// Unprivileged identity to run as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRunAs {
    pub uid: u32,
    pub gid: u32,
}

/// Per-request confinement for a guest command. Defaults are the safe ones
/// (same as procbox): no network, IPC scoping on, seccomp errno.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecPolicy {
    pub read: Vec<String>,
    pub write: Vec<String>,
    /// Executable allow-list. `None`: `read` and `write` paths may also be executed. `Some`: they
    /// may not, and only files under these paths run. Commands run through `/bin/sh -c`, so the
    /// list needs the shell and the dynamic loader as well as the programs themselves.
    pub exec: Option<Vec<String>>,
    pub tcp_connect: ExecTcpRule,
    pub tcp_bind: ExecTcpRule,
    pub scope_ipc: bool,
    pub seccomp: Option<ExecSeccompMode>,
    pub allow_namespaces: bool,
    pub max_memory: Option<u64>,
    pub max_processes: Option<u64>,
    pub cpu_seconds: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub clean_env: bool,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
    pub best_effort: bool,
    pub max_output_bytes: Option<usize>,
    pub run_as: Option<ExecRunAs>,
    pub isolation: ExecIsolation,
    pub allow_unix: bool,
    pub allow_udp: bool,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        Self {
            read: Vec::new(),
            write: Vec::new(),
            exec: None,
            tcp_connect: ExecTcpRule::Deny,
            tcp_bind: ExecTcpRule::Deny,
            scope_ipc: true,
            seccomp: Some(ExecSeccompMode::Errno),
            allow_namespaces: false,
            max_memory: None,
            max_processes: None,
            cpu_seconds: None,
            timeout_secs: None,
            clean_env: false,
            env: Vec::new(),
            cwd: None,
            best_effort: false,
            max_output_bytes: None,
            run_as: None,
            isolation: ExecIsolation::Off,
            allow_unix: false,
            allow_udp: false,
        }
    }
}

/// What the guest actually enforced for one confined exec.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecEnforcement {
    pub landlock_abi: u32,
    pub filesystem: bool,
    /// Executing files was limited to the policy's `exec` paths.
    #[serde(default)]
    pub exec_restricted: bool,
    pub tcp_connect: bool,
    pub tcp_bind: bool,
    pub scope_abstract_unix: bool,
    pub scope_signal: bool,
    pub seccomp: bool,
    pub uid_dropped: bool,
    pub namespaces: bool,
    pub network_isolated: bool,
    pub seccomp_sockets: bool,
    /// Everything the policy asked for that this run did NOT enforce.
    pub not_enforced: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_json_is_the_safe_default() {
        let p: ExecPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p, ExecPolicy::default());
        assert_eq!(p.tcp_connect, ExecTcpRule::Deny);
        assert!(p.scope_ipc);
    }

    #[test]
    fn exec_allow_list_defaults_to_unrestricted_and_round_trips() {
        let p: ExecPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p.exec, None);
        let p: ExecPolicy = serde_json::from_str(r#"{"exec":["/usr/bin","/lib64"]}"#).unwrap();
        assert_eq!(
            p.exec,
            Some(vec!["/usr/bin".to_string(), "/lib64".to_string()])
        );
        // An empty list is a policy too: nothing may run.
        let p: ExecPolicy = serde_json::from_str(r#"{"exec":[]}"#).unwrap();
        assert_eq!(p.exec, Some(vec![]));
        // An old agent's reply has no `exec_restricted`.
        let e: ExecEnforcement = serde_json::from_str(
            r#"{"landlock_abi":7,"filesystem":true,"tcp_connect":false,"tcp_bind":false,"scope_abstract_unix":false,"scope_signal":false,"seccomp":true,"uid_dropped":false,"namespaces":false,"network_isolated":false,"seccomp_sockets":false,"not_enforced":[]}"#,
        )
        .unwrap();
        assert!(!e.exec_restricted);
    }

    #[test]
    fn procbox_shaped_json_parses_and_ignores_unknown_fields() {
        let p: ExecPolicy = serde_json::from_str(
            r#"{"read":["/usr"],"tcp_connect":{"ports":[443]},"seccomp":"kill","max_abi":3,"isolation":"auto"}"#,
        )
        .unwrap();
        assert_eq!(p.read, vec!["/usr".to_string()]);
        assert_eq!(p.tcp_connect, ExecTcpRule::Ports(vec![443]));
        assert_eq!(p.seccomp, Some(ExecSeccompMode::Kill));
        assert_eq!(p.isolation, ExecIsolation::Auto);
    }
}
