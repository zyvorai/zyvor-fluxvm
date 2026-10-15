// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Per-action approvals for agent sessions.
//!
//! A session policy can list `process.ask` patterns. A command or job that matches one does not
//! run: the daemon records a pending approval and answers `428`. An operator (admin credential
//! only; a session token can never approve) approves or denies it, and the caller repeats the
//! same request with `approval_id`.
//!
//! An approval is bound to a digest of the session, the kind of action, the exact command and its
//! parameters, the session's policy and its revision (which a restore or fork changes). It is
//! single-use: consuming it is a compare-and-set under one lock, so two retries cannot both run.
//! Records live in `<state_dir>/agent-session-approvals/<session>.json`.

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::Uuid;

/// How long a request waits for a decision.
pub(crate) const TTL_SECS: i64 = 600;
/// Longest `approval_wait_seconds` a caller may ask for.
pub(crate) const MAX_WAIT_SECS: u64 = 120;
/// Finished records are kept this long for the audit view, then dropped.
const KEEP_FINISHED_SECS: i64 = 86_400;
const MAX_PER_SESSION: usize = 200;
const SUMMARY_MAX: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
    Pending,
    Approved,
    Denied,
    Consumed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Approval {
    pub id: Uuid,
    /// `exec` or `job`.
    pub kind: String,
    /// The command, shortened for display. The digest covers the full text.
    pub summary: String,
    /// The `ask` rule that matched.
    pub matched: String,
    /// Parameters the digest covers, shown so the approver sees what is being approved.
    pub params: Value,
    pub digest: String,
    /// The session revision when it was requested.
    pub revision: u64,
    pub state: State,
    pub requested_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<DateTime<Utc>>,
}

impl Approval {
    /// The state a reader should see: a pending or approved record past its expiry is `expired`.
    pub(crate) fn effective_state(&self, now: DateTime<Utc>) -> &'static str {
        match self.state {
            State::Pending if now > self.expires_at => "expired",
            State::Approved if now > self.expires_at => "expired",
            State::Pending => "pending",
            State::Approved => "approved",
            State::Denied => "denied",
            State::Consumed => "consumed",
        }
    }
}

/// What a request is, for binding.
pub(crate) struct Subject<'a> {
    pub session: Uuid,
    pub kind: &'a str,
    pub command: &'a str,
    /// Every parameter that changes what the action does (timeouts, stdin, output cap...).
    pub params: Value,
    /// Hash of the session's policy at the time.
    pub policy_hash: &'a str,
    pub revision: u64,
}

impl Subject<'_> {
    pub(crate) fn digest(&self) -> String {
        // A fixed field order, so the digest is stable.
        let canonical = serde_json::json!([
            self.session,
            self.kind,
            self.command,
            self.params,
            self.policy_hash,
            self.revision,
        ]);
        Sha256::digest(canonical.to_string().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

pub(crate) fn hash_str(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What to do with a request that matched an `ask` rule.
#[derive(Debug)]
pub(crate) enum Decision {
    /// An approved record was consumed: run the action.
    Run(Uuid),
    /// Nobody has decided yet: answer `428` with this record.
    Pending(Box<Approval>),
    Refuse(StatusCode, String),
}

fn refuse(status: StatusCode, why: &str) -> Decision {
    Decision::Refuse(status, why.to_string())
}

fn clip(s: &str) -> String {
    if s.chars().count() <= SUMMARY_MAX {
        return s.to_string();
    }
    let mut t: String = s.chars().take(SUMMARY_MAX).collect();
    t.push('…');
    t
}

/// The pure decision over a session's records. `presented` is the `approval_id` the caller sent.
/// Mutates `all`: consumes an approval, or adds a pending one.
pub(crate) fn decide_request(
    all: &mut Vec<Approval>,
    subject: &Subject<'_>,
    matched: &str,
    presented: Option<Uuid>,
    now: DateTime<Utc>,
) -> Decision {
    let digest = subject.digest();
    if let Some(id) = presented {
        let Some(a) = all.iter_mut().find(|a| a.id == id) else {
            return refuse(
                StatusCode::NOT_FOUND,
                "unknown approval_id for this session",
            );
        };
        if a.digest != digest {
            return refuse(
                StatusCode::FORBIDDEN,
                "this approval was granted for a different command, parameters, policy or session state",
            );
        }
        return match a.state {
            State::Pending if now > a.expires_at => {
                refuse(StatusCode::FORBIDDEN, "the approval request expired")
            }
            State::Pending => Decision::Pending(Box::new(a.clone())),
            State::Denied => refuse(StatusCode::FORBIDDEN, "the approval was denied"),
            State::Consumed => refuse(StatusCode::FORBIDDEN, "the approval was already used"),
            State::Approved if now > a.expires_at => refuse(
                StatusCode::FORBIDDEN,
                "the approval expired before it was used",
            ),
            State::Approved => {
                a.state = State::Consumed;
                a.consumed_at = Some(now);
                Decision::Run(a.id)
            }
        };
    }
    // No id: an identical request that was already approved is consumed, one still waiting is
    // returned, otherwise a new request is recorded.
    if let Some(a) = all
        .iter_mut()
        .find(|a| a.digest == digest && a.state == State::Approved && now <= a.expires_at)
    {
        a.state = State::Consumed;
        a.consumed_at = Some(now);
        return Decision::Run(a.id);
    }
    if let Some(a) = all
        .iter()
        .find(|a| a.digest == digest && a.state == State::Pending && now <= a.expires_at)
    {
        return Decision::Pending(Box::new(a.clone()));
    }
    let pending = all
        .iter()
        .filter(|a| a.state == State::Pending && now <= a.expires_at)
        .count();
    if pending >= MAX_PER_SESSION {
        return refuse(
            StatusCode::TOO_MANY_REQUESTS,
            "too many pending approvals for this session",
        );
    }
    let a = Approval {
        id: Uuid::new_v4(),
        kind: subject.kind.to_string(),
        summary: clip(subject.command),
        matched: matched.to_string(),
        params: subject.params.clone(),
        digest,
        revision: subject.revision,
        state: State::Pending,
        requested_at: now,
        expires_at: now + Duration::seconds(TTL_SECS),
        decided_at: None,
        decided_by: None,
        note: None,
        consumed_at: None,
    };
    all.push(a.clone());
    Decision::Pending(Box::new(a))
}

/// Approves or denies a pending record.
pub(crate) fn decide_record(
    all: &mut [Approval],
    id: Uuid,
    approve: bool,
    by: &str,
    note: Option<String>,
    now: DateTime<Utc>,
) -> Result<Approval, (StatusCode, String)> {
    let Some(a) = all.iter_mut().find(|a| a.id == id) else {
        return Err((StatusCode::NOT_FOUND, format!("no approval {id}")));
    };
    if a.state != State::Pending {
        return Err((
            StatusCode::CONFLICT,
            format!("approval {id} is already {}", a.effective_state(now)),
        ));
    }
    if now > a.expires_at {
        return Err((StatusCode::CONFLICT, format!("approval {id} has expired")));
    }
    a.state = if approve {
        State::Approved
    } else {
        State::Denied
    };
    a.decided_at = Some(now);
    a.decided_by = Some(by.to_string());
    a.note = note.map(|n| n.chars().take(512).collect());
    Ok(a.clone())
}

/// Drops long-finished records so the file stays small.
pub(crate) fn sweep(all: &mut Vec<Approval>, now: DateTime<Utc>) {
    all.retain(|a| {
        let end = a.consumed_at.or(a.decided_at).unwrap_or(a.expires_at);
        let finished = !matches!(a.state, State::Pending | State::Approved) || now > a.expires_at;
        !(finished && (now - end).num_seconds() > KEEP_FINISHED_SECS)
    });
}

// ---------------------------------------------------------------------------------------
// Persistence

static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn path(state_dir: &std::path::Path, session: Uuid) -> PathBuf {
    state_dir
        .join("agent-session-approvals")
        .join(format!("{session}.json"))
}

pub(crate) async fn list(state_dir: &std::path::Path, session: Uuid) -> Vec<Approval> {
    match tokio::fs::read(path(state_dir, session)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Read-modify-write under one lock, so a consume cannot race another consume or a decision.
pub(crate) async fn mutate<R>(
    state_dir: &std::path::Path,
    session: Uuid,
    f: impl FnOnce(&mut Vec<Approval>) -> R,
) -> anyhow::Result<R> {
    let _g = LOCK.lock().await;
    let mut all = list(state_dir, session).await;
    let r = f(&mut all);
    let p = path(state_dir, session);
    if let Some(d) = p.parent() {
        tokio::fs::create_dir_all(d).await?;
    }
    let tmp = p.with_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(&all)?).await?;
    tokio::fs::rename(&tmp, &p).await?;
    Ok(r)
}

/// Removes a deleted session's records.
pub(crate) async fn remove(state_dir: &std::path::Path, session: Uuid) {
    let _g = LOCK.lock().await;
    let _ = tokio::fs::remove_file(path(state_dir, session)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn subject<'a>(session: Uuid, cmd: &'a str, rev: u64, policy: &'a str) -> Subject<'a> {
        Subject {
            session,
            kind: "exec",
            command: cmd,
            params: json!({"timeout_seconds": 30}),
            policy_hash: policy,
            revision: rev,
        }
    }

    fn pending(d: Decision) -> Approval {
        match d {
            Decision::Pending(a) => *a,
            other => panic!("expected pending, got {other:?}"),
        }
    }

    #[test]
    fn digest_binds_every_part() {
        let s = Uuid::new_v4();
        let base = subject(s, "rm -rf build", 3, "p").digest();
        assert_eq!(base, subject(s, "rm -rf build", 3, "p").digest());
        assert_ne!(base, subject(s, "rm -rf /", 3, "p").digest());
        assert_ne!(base, subject(s, "rm -rf build", 4, "p").digest());
        assert_ne!(base, subject(s, "rm -rf build", 3, "q").digest());
        assert_ne!(
            base,
            subject(Uuid::new_v4(), "rm -rf build", 3, "p").digest()
        );
        let mut other = subject(s, "rm -rf build", 3, "p");
        other.params = json!({"timeout_seconds": 31});
        assert_ne!(base, other.digest());
        other = subject(s, "rm -rf build", 3, "p");
        other.kind = "job";
        assert_ne!(base, other.digest());
    }

    #[test]
    fn a_request_waits_then_runs_once_after_approval() {
        let (s, now) = (Uuid::new_v4(), Utc::now());
        let sub = subject(s, "deploy", 1, "p");
        let mut all = Vec::new();
        let a = pending(decide_request(&mut all, &sub, "deploy*", None, now));
        assert_eq!(all.len(), 1);
        // Asking again returns the same pending record rather than a second one.
        let again = pending(decide_request(&mut all, &sub, "deploy*", None, now));
        assert_eq!(again.id, a.id);
        assert_eq!(all.len(), 1);
        // Presenting it before a decision is still pending.
        assert!(matches!(
            decide_request(&mut all, &sub, "deploy*", Some(a.id), now),
            Decision::Pending(_)
        ));
        decide_record(&mut all, a.id, true, "admin", Some("ok".into()), now).unwrap();
        assert!(matches!(
            decide_request(&mut all, &sub, "deploy*", Some(a.id), now),
            Decision::Run(id) if id == a.id
        ));
        // Single use.
        assert!(matches!(
            decide_request(&mut all, &sub, "deploy*", Some(a.id), now),
            Decision::Refuse(StatusCode::FORBIDDEN, _)
        ));
        // And a retry without the id does not run it again: it asks anew.
        assert!(matches!(
            decide_request(&mut all, &sub, "deploy*", None, now),
            Decision::Pending(b) if b.id != a.id
        ));
    }

    #[test]
    fn an_approval_does_not_carry_to_another_command_or_state() {
        let (s, now) = (Uuid::new_v4(), Utc::now());
        let mut all = Vec::new();
        let sub = subject(s, "deploy prod", 1, "p");
        let a = pending(decide_request(&mut all, &sub, "deploy*", None, now));
        decide_record(&mut all, a.id, true, "admin", None, now).unwrap();
        for other in [
            subject(s, "deploy staging", 1, "p"),
            subject(s, "deploy prod", 2, "p"),
            subject(s, "deploy prod", 1, "changed"),
        ] {
            assert!(matches!(
                decide_request(&mut all, &other, "deploy*", Some(a.id), now),
                Decision::Refuse(StatusCode::FORBIDDEN, _)
            ));
        }
        // The approval is still usable by the exact request.
        assert!(matches!(
            decide_request(&mut all, &sub, "deploy*", Some(a.id), now),
            Decision::Run(_)
        ));
    }

    #[test]
    fn denied_and_expired_approvals_refuse() {
        let (s, now) = (Uuid::new_v4(), Utc::now());
        let sub = subject(s, "x", 1, "p");
        let mut all = Vec::new();
        let a = pending(decide_request(&mut all, &sub, "x", None, now));
        decide_record(&mut all, a.id, false, "admin", None, now).unwrap();
        assert!(matches!(
            decide_request(&mut all, &sub, "x", Some(a.id), now),
            Decision::Refuse(StatusCode::FORBIDDEN, _)
        ));
        // A decision cannot be changed.
        assert!(decide_record(&mut all, a.id, true, "admin", None, now).is_err());

        let b = pending(decide_request(&mut all, &sub, "x", None, now));
        decide_record(&mut all, b.id, true, "admin", None, now).unwrap();
        let later = now + Duration::seconds(TTL_SECS + 1);
        assert!(matches!(
            decide_request(&mut all, &sub, "x", Some(b.id), later),
            Decision::Refuse(StatusCode::FORBIDDEN, _)
        ));
        assert_eq!(
            all.iter()
                .find(|a| a.id == b.id)
                .unwrap()
                .effective_state(later),
            "expired"
        );
        // A late decision on an unanswered request is refused.
        let c = pending(decide_request(
            &mut all,
            &subject(s, "y", 1, "p"),
            "y",
            None,
            now,
        ));
        assert!(decide_record(&mut all, c.id, true, "admin", None, later).is_err());
    }

    #[test]
    fn an_unknown_id_is_not_found_and_the_flood_is_capped() {
        let (s, now) = (Uuid::new_v4(), Utc::now());
        let mut all = Vec::new();
        assert!(matches!(
            decide_request(
                &mut all,
                &subject(s, "x", 1, "p"),
                "x",
                Some(Uuid::new_v4()),
                now
            ),
            Decision::Refuse(StatusCode::NOT_FOUND, _)
        ));
        for i in 0..MAX_PER_SESSION {
            let c = format!("cmd {i}");
            pending(decide_request(
                &mut all,
                &subject(s, &c, 1, "p"),
                "x",
                None,
                now,
            ));
        }
        assert!(matches!(
            decide_request(&mut all, &subject(s, "one more", 1, "p"), "x", None, now),
            Decision::Refuse(StatusCode::TOO_MANY_REQUESTS, _)
        ));
    }

    #[test]
    fn sweep_drops_only_long_finished_records() {
        let (s, now) = (Uuid::new_v4(), Utc::now());
        let mut all = Vec::new();
        let a = pending(decide_request(
            &mut all,
            &subject(s, "a", 1, "p"),
            "a",
            None,
            now,
        ));
        decide_record(&mut all, a.id, false, "admin", None, now).unwrap();
        pending(decide_request(
            &mut all,
            &subject(s, "b", 1, "p"),
            "b",
            None,
            now,
        ));
        sweep(&mut all, now + Duration::seconds(60));
        assert_eq!(all.len(), 2);
        sweep(
            &mut all,
            now + Duration::seconds(KEEP_FINISHED_SECS + TTL_SECS + 60),
        );
        assert!(all.is_empty());
    }

    #[test]
    fn long_commands_are_clipped_in_the_summary_only() {
        let s = Uuid::new_v4();
        let long = "x".repeat(2000);
        let sub = subject(s, &long, 1, "p");
        let mut all = Vec::new();
        let a = pending(decide_request(&mut all, &sub, "x", None, Utc::now()));
        assert!(a.summary.chars().count() <= SUMMARY_MAX + 1);
        // The digest still covers the whole text.
        assert_ne!(sub.digest(), subject(s, &"x".repeat(1999), 1, "p").digest());
    }

    #[tokio::test]
    async fn the_store_round_trips_and_consumes_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let (s, now) = (Uuid::new_v4(), Utc::now());
        let sub = subject(s, "deploy", 1, "p");
        let a = mutate(dir.path(), s, |all| {
            pending(decide_request(all, &sub, "deploy*", None, now))
        })
        .await
        .unwrap();
        mutate(dir.path(), s, |all| {
            decide_record(all, a.id, true, "admin", None, now)
        })
        .await
        .unwrap()
        .unwrap();
        // Two concurrent retries: exactly one runs.
        let run = |d: &std::path::Path| {
            let d = d.to_path_buf();
            let sub = subject(s, "deploy", 1, "p");
            async move {
                mutate(&d, s, |all| {
                    matches!(
                        decide_request(all, &sub, "deploy*", Some(a.id), Utc::now()),
                        Decision::Run(_)
                    )
                })
                .await
                .unwrap()
            }
        };
        let (x, y) = tokio::join!(run(dir.path()), run(dir.path()));
        assert_eq!(u8::from(x) + u8::from(y), 1);
        let stored = list(dir.path(), s).await;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].state, State::Consumed);
        remove(dir.path(), s).await;
        assert!(list(dir.path(), s).await.is_empty());
    }
}
