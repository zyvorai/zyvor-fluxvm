// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Wire types for the guest job supervisor: long-lived commands the agent runs, with separate
//! stdout/stderr spools that can be read from an offset, a stdin pipe that can be closed, and
//! signalling of the whole process tree.
//!
//! The agent still serves one JSON line per connection, so a job is driven by short calls:
//! `JobStart`, then `JobRead` (a long poll bounded by `wait_ms`), `JobWrite`, `JobSignal`.

use crate::ExecEnforcement;
use serde::{Deserialize, Serialize};

/// Advertised in [`crate::AgentResponse::Capabilities`] when the agent has the job supervisor.
pub const FEATURE_JOBS_V2: &str = "jobs-v2";

/// Upper bound on one `JobRead` long poll, whatever the caller asks for.
pub const MAX_JOB_WAIT_MS: u64 = 25_000;
/// Upper bound on the bytes one `JobRead` returns.
pub const MAX_JOB_READ_BYTES: u32 = 1024 * 1024;
/// Default cap on a job's retained output per stream.
pub const DEFAULT_JOB_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    Running,
    /// Ended on its own; see `exit_code`.
    Exited,
    /// Ended on a signal it was not asked to stop on; see `signal`.
    Signaled,
    TimedOut,
    /// Stopped by a `JobSignal`.
    Canceled,
}

impl JobPhase {
    pub fn is_final(self) -> bool {
        self != JobPhase::Running
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    pub job_id: String,
    pub pid: u32,
    pub phase: JobPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Bytes kept and readable, per stream.
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// Bytes read from the process and discarded because the cap was reached.
    pub stdout_dropped: u64,
    pub stderr_dropped: u64,
    pub started_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_unix_ms: Option<u64>,
    /// What the guest enforced, for a job started with a `policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcement: Option<ExecEnforcement>,
}

/// Valid job ids: what the host generates (`job-<uuid>`), safe as a directory name.
pub fn valid_job_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
