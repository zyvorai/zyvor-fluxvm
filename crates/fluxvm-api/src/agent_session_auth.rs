// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Session-scoped credentials: an expiring bearer token that works on exactly one agent
//! session and only for the operations it was minted with.
//!
//! A token looks like `fxst_<session>_<token id>_<secret>`. The secret is shown once; the store
//! keeps its SHA-256. Records live in `<state_dir>/agent-session-tokens/<session>.json`.
//!
//! The auth middleware turns a valid token into a [`SessionScope`] and the read-only role, and
//! refuses any path outside `/v1/agents/sessions/<session>`. Handlers then call [`authorize`],
//! which never falls back to the admin check once a scope is present.

use crate::{ApiError, ApiResult, require_admin};
use axum::{Extension, http::StatusCode};
use chrono::{DateTime, Utc};
use fluxvm_core::config::{Role, constant_time_eq};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) const TOKEN_PREFIX: &str = "fxst_";
/// Longest a session token may live.
pub(crate) const MAX_TTL_SECS: u64 = 86_400;
pub(crate) const DEFAULT_TTL_SECS: u64 = 3_600;
const MAX_TOKENS_PER_SESSION: usize = 32;

/// What a scoped token may do. Restore, fork, delete, terminals and token management are not
/// grantable: they stay admin-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Op {
    /// Session record, events, trajectory, artifacts, preview list.
    #[serde(rename = "read")]
    Read,
    #[serde(rename = "exec")]
    Exec,
    #[serde(rename = "jobs")]
    Jobs,
    #[serde(rename = "files.read")]
    FilesRead,
    #[serde(rename = "files.write")]
    FilesWrite,
    /// Every git verb, including commit and branch.
    #[serde(rename = "git")]
    Git,
    #[serde(rename = "checkpoint")]
    Checkpoint,
    #[serde(rename = "preview")]
    Preview,
    /// The guest display: screenshot and keyboard/mouse input (`vz` sessions).
    #[serde(rename = "screen")]
    Screen,
}

impl Op {
    pub(crate) const ALL: [Op; 9] = [
        Op::Read,
        Op::Exec,
        Op::Jobs,
        Op::FilesRead,
        Op::FilesWrite,
        Op::Git,
        Op::Checkpoint,
        Op::Preview,
        Op::Screen,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Exec => "exec",
            Op::Jobs => "jobs",
            Op::FilesRead => "files.read",
            Op::FilesWrite => "files.write",
            Op::Git => "git",
            Op::Checkpoint => "checkpoint",
            Op::Preview => "preview",
            Op::Screen => "screen",
        }
    }

    pub(crate) fn parse(s: &str) -> Option<Op> {
        Op::ALL.into_iter().find(|o| o.as_str() == s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TokenRecord {
    pub id: Uuid,
    /// Hex SHA-256 of the secret.
    pub hash: String,
    pub ops: Vec<Op>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenFile {
    session: Uuid,
    /// The session's tenant at mint time, so a scoped caller sees the session as its tenant would.
    tenant: Option<String>,
    tokens: Vec<TokenRecord>,
}

/// A request's resolved session token, carried as a request extension.
#[derive(Debug, Clone)]
pub(crate) struct SessionScope {
    pub session: Uuid,
    pub token_id: Uuid,
    pub ops: Vec<Op>,
    pub tenant: Option<String>,
}

fn store_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("agent-session-tokens")
}

fn store_path(state_dir: &Path, session: Uuid) -> PathBuf {
    store_dir(state_dir).join(format!("{session}.json"))
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Serializes read-modify-write of the store files within this process.
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn read_file(state_dir: &Path, session: Uuid) -> Option<TokenFile> {
    let bytes = tokio::fs::read(store_path(state_dir, session)).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_file(state_dir: &Path, f: &TokenFile) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(store_dir(state_dir)).await?;
    let path = store_path(state_dir, f.session);
    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(f)?).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// A freshly minted token. `secret_token` is the only time the credential is visible.
pub(crate) struct Minted {
    pub record: TokenRecord,
    pub secret_token: String,
}

/// Mints a token for `session`. `expires_at` is already capped by the caller.
pub(crate) async fn mint(
    state_dir: &Path,
    session: Uuid,
    tenant: Option<String>,
    ops: Vec<Op>,
    label: Option<String>,
    expires_at: DateTime<Utc>,
) -> ApiResult<Minted> {
    let _g = WRITE_LOCK.lock().await;
    let mut f = read_file(state_dir, session).await.unwrap_or(TokenFile {
        session,
        tenant: tenant.clone(),
        tokens: Vec::new(),
    });
    let now = Utc::now();
    f.tokens.retain(|t| !t.revoked && t.expires_at > now);
    if f.tokens.len() >= MAX_TOKENS_PER_SESSION {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!(
                "a session has at most {MAX_TOKENS_PER_SESSION} live tokens; revoke one"
            ),
        });
    }
    let id = Uuid::new_v4();
    let secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let record = TokenRecord {
        id,
        hash: sha256_hex(&secret),
        ops,
        label,
        created_at: now,
        expires_at,
        revoked: false,
    };
    f.tokens.push(record.clone());
    write_file(state_dir, &f).await?;
    let secret_token = format!(
        "{TOKEN_PREFIX}{}_{}_{secret}",
        session.simple(),
        id.simple()
    );
    Ok(Minted {
        record,
        secret_token,
    })
}

/// The live (unexpired, unrevoked) tokens of a session. Never includes secrets or hashes.
pub(crate) async fn list(state_dir: &Path, session: Uuid) -> Vec<TokenRecord> {
    let now = Utc::now();
    read_file(state_dir, session)
        .await
        .map(|f| {
            f.tokens
                .into_iter()
                .filter(|t| !t.revoked && t.expires_at > now)
                .map(|mut t| {
                    t.hash.clear();
                    t
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Revokes one token. `false` when the session has no such live token.
pub(crate) async fn revoke(state_dir: &Path, session: Uuid, token: Uuid) -> ApiResult<bool> {
    let _g = WRITE_LOCK.lock().await;
    let Some(mut f) = read_file(state_dir, session).await else {
        return Ok(false);
    };
    let mut hit = false;
    for t in &mut f.tokens {
        if t.id == token && !t.revoked {
            t.revoked = true;
            hit = true;
        }
    }
    if hit {
        write_file(state_dir, &f).await?;
    }
    Ok(hit)
}

/// Removes every token of a session (session deleted).
pub(crate) async fn purge(state_dir: &Path, session: Uuid) {
    let _g = WRITE_LOCK.lock().await;
    let _ = tokio::fs::remove_file(store_path(state_dir, session)).await;
}

fn parse(token: &str) -> Option<(Uuid, Uuid, &str)> {
    let rest = token.strip_prefix(TOKEN_PREFIX)?;
    let mut it = rest.splitn(3, '_');
    let session = Uuid::parse_str(it.next()?).ok()?;
    let id = Uuid::parse_str(it.next()?).ok()?;
    let secret = it.next()?;
    (secret.len() == 64).then_some((session, id, secret))
}

/// Resolves a presented bearer token. `None` for anything malformed, unknown, expired or revoked.
pub(crate) async fn resolve(state_dir: &Path, token: &str) -> Option<SessionScope> {
    let (session, id, secret) = parse(token)?;
    let f = read_file(state_dir, session).await?;
    let rec = f.tokens.iter().find(|t| t.id == id)?;
    if rec.revoked || rec.expires_at <= Utc::now() {
        return None;
    }
    if !constant_time_eq(&rec.hash, &sha256_hex(secret)) {
        return None;
    }
    Some(SessionScope {
        session,
        token_id: id,
        ops: rec.ops.clone(),
        tenant: f.tenant,
    })
}

/// Whether a scoped token may reach `path` at all: only its own session's routes.
pub(crate) fn path_allowed(path: &str, session: Uuid) -> bool {
    let base = format!("/v1/agents/sessions/{session}");
    path == base || path.strip_prefix(&base).is_some_and(|r| r.starts_with('/'))
}

/// The gate for a session handler. With a scope: it must be this session and grant `op`, and the
/// admin check is never consulted. Without one: the caller must be an admin.
pub(crate) fn authorize(
    role: Role,
    scope: &Option<Extension<SessionScope>>,
    id: Uuid,
    op: Op,
) -> ApiResult<()> {
    match scope {
        Some(Extension(sc)) => {
            if sc.session != id {
                return Err(ApiError {
                    status: StatusCode::NOT_FOUND,
                    message: format!("agent session {id} not found"),
                });
            }
            if !sc.ops.contains(&op) {
                return Err(ApiError {
                    status: StatusCode::FORBIDDEN,
                    message: format!("this session token does not grant {}", op.as_str()),
                });
            }
            Ok(())
        }
        None => require_admin(role),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exp(secs: i64) -> DateTime<Utc> {
        Utc::now() + chrono::Duration::seconds(secs)
    }

    #[tokio::test]
    async fn a_minted_token_resolves_until_revoked_or_expired() {
        let d = tempfile::tempdir().unwrap();
        let s = Uuid::new_v4();
        let m = mint(d.path(), s, Some("t".into()), vec![Op::Exec], None, exp(60))
            .await
            .unwrap();
        let sc = resolve(d.path(), &m.secret_token).await.unwrap();
        assert_eq!(sc.session, s);
        assert_eq!(sc.ops, vec![Op::Exec]);
        assert_eq!(sc.tenant.as_deref(), Some("t"));
        // The store keeps a hash, not the secret.
        let raw = std::fs::read_to_string(store_path(d.path(), s)).unwrap();
        assert!(!raw.contains(&m.secret_token[m.secret_token.len() - 64..]));
        assert!(revoke(d.path(), s, m.record.id).await.unwrap());
        assert!(resolve(d.path(), &m.secret_token).await.is_none());
        let e = mint(d.path(), s, None, vec![Op::Read], None, exp(-5))
            .await
            .unwrap();
        assert!(resolve(d.path(), &e.secret_token).await.is_none());
    }

    #[tokio::test]
    async fn a_token_does_not_resolve_for_another_session_or_with_a_wrong_secret() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let m = mint(d.path(), a, None, vec![Op::Read], None, exp(60))
            .await
            .unwrap();
        // Same token id and secret under another session id: no record there.
        let forged = m
            .secret_token
            .replacen(&a.simple().to_string(), &b.simple().to_string(), 1);
        assert!(resolve(d.path(), &forged).await.is_none());
        let wrong = format!("{}{}", &m.secret_token[..m.secret_token.len() - 4], "0000");
        assert!(resolve(d.path(), &wrong).await.is_none());
        assert!(resolve(d.path(), "fxst_garbage").await.is_none());
        assert!(resolve(d.path(), "not-a-session-token").await.is_none());
    }

    #[test]
    fn scoped_tokens_reach_only_their_own_session_paths() {
        let s = Uuid::new_v4();
        let o = Uuid::new_v4();
        assert!(path_allowed(&format!("/v1/agents/sessions/{s}"), s));
        assert!(path_allowed(&format!("/v1/agents/sessions/{s}/exec"), s));
        assert!(!path_allowed(&format!("/v1/agents/sessions/{o}/exec"), s));
        assert!(!path_allowed("/v1/agents/sessions", s));
        assert!(!path_allowed("/v1/vms", s));
        assert!(!path_allowed(&format!("/v1/agents/sessions/{s}x/exec"), s));
    }

    #[test]
    fn authorize_never_falls_back_to_admin_for_a_scoped_caller() {
        let s = Uuid::new_v4();
        let sc = Some(Extension(SessionScope {
            session: s,
            token_id: Uuid::new_v4(),
            ops: vec![Op::Exec],
            tenant: None,
        }));
        assert!(authorize(Role::ReadOnly, &sc, s, Op::Exec).is_ok());
        assert_eq!(
            authorize(Role::Admin, &sc, s, Op::FilesWrite)
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            authorize(Role::Admin, &sc, Uuid::new_v4(), Op::Exec)
                .unwrap_err()
                .status,
            StatusCode::NOT_FOUND
        );
        assert!(authorize(Role::Admin, &None, s, Op::Exec).is_ok());
        assert!(authorize(Role::ReadOnly, &None, s, Op::Exec).is_err());
    }

    #[test]
    fn op_names_round_trip() {
        for o in Op::ALL {
            assert_eq!(Op::parse(o.as_str()), Some(o));
        }
        assert_eq!(Op::parse("restore"), None);
    }
}
