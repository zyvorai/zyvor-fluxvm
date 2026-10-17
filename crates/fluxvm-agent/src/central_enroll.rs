//! Per-node enrollment: a node joins with a one-time/batch join token and gets its own credential, instead of every
//! node holding the fleet-admin token.
//!
//! - A join token (`fvjt_<id>.<secret>`) is created by the operator, stored as a hash only, and may expire, be limited
//!   to `max_uses` nodes and to node names matching a pattern.
//! - `POST /fleet/enroll` (authenticated by the join token alone) binds a node name to a node-generated `node_id` and
//!   returns the node's credential (`fvnc_<node_id>.<generation>.<mac>`). The mac is an HMAC of the name, id and
//!   generation under a secret only central holds, so central stores no credential: repeating the request with the
//!   same token and `node_id` returns the same credential and costs no use (a lost response does not burn one).
//! - A different `node_id` for an enrolled name is `409`; the operator moves a name to new hardware with
//!   `POST /fleet/nodes/{name}/rebind`.
//! - A node credential is accepted only for `POST /fleet/register` as that node, `POST /fleet/enroll/renew` and
//!   `GET /fleet/nodes/{name}/overlays` for itself; it is not an operator or tenant token.
//! - Revoking a node works while it is healthy; rotating keeps the previous credential valid for a short window.
//!
//! `--node-auth legacy|both|enrolled` (`FLUXVM_NODE_AUTH`, default `both`) decides whether the fleet token may still
//! register nodes.

use super::{AppError, Fleet, persist_json};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::OnceLock;

pub(super) const JOIN_PREFIX: &str = "fvjt_";
pub(super) const CRED_PREFIX: &str = "fvnc_";
/// How long the previous credential stays valid after a rotation.
const ROTATION_WINDOW_SECS: i64 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAuth {
    /// Enrollment is off; the fleet token registers nodes (the behaviour before enrollment existed).
    Legacy,
    /// Both work (the default while nodes migrate).
    Both,
    /// Only enrolled nodes may register.
    Enrolled,
}

impl std::str::FromStr for NodeAuth {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "legacy" => Ok(NodeAuth::Legacy),
            "both" => Ok(NodeAuth::Both),
            "enrolled" => Ok(NodeAuth::Enrolled),
            _ => Err(format!("'{s}' is not legacy, both or enrolled")),
        }
    }
}

static MODE: OnceLock<NodeAuth> = OnceLock::new();

/// Sets the mode for this process (from `--node-auth`); the first call wins.
pub fn set_node_auth(mode: NodeAuth) {
    let _ = MODE.set(mode);
}

pub(super) fn node_auth() -> NodeAuth {
    *MODE.get_or_init(|| {
        std::env::var("FLUXVM_NODE_AUTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(NodeAuth::Both)
    })
}

/// Who a node credential identified; the middleware puts it in the request extensions.
#[derive(Debug, Clone)]
pub(super) struct NodeIdentity(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JoinToken {
    hash: String,
    created: i64,
    expires: Option<i64>,
    max_uses: Option<u32>,
    uses: u32,
    name_pattern: Option<String>,
    revoked: bool,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Enrolled {
    node_id: String,
    token_id: String,
    generation: u32,
    #[serde(default)]
    prev_valid_until: Option<i64>,
    #[serde(default)]
    revoked: bool,
    enrolled_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct EnrollState {
    #[serde(default)]
    secret: String,
    #[serde(default)]
    tokens: BTreeMap<String, JoinToken>,
    #[serde(default)]
    nodes: BTreeMap<String, Enrolled>,
    /// node_id -> name, rebuilt on load.
    #[serde(skip)]
    by_id: HashMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum EnrollError {
    BadToken,
    BadRequest(&'static str),
    NameNotAllowed,
    Conflict(&'static str),
    Exhausted,
}

impl EnrollError {
    fn into_app(self) -> AppError {
        match self {
            EnrollError::BadToken => AppError(
                StatusCode::UNAUTHORIZED,
                "invalid, expired or revoked join token".into(),
            ),
            EnrollError::BadRequest(m) => AppError(StatusCode::BAD_REQUEST, m.into()),
            EnrollError::NameNotAllowed => AppError(
                StatusCode::FORBIDDEN,
                "this join token does not allow that node name".into(),
            ),
            EnrollError::Conflict(m) => AppError(StatusCode::CONFLICT, m.into()),
            EnrollError::Exhausted => AppError(
                StatusCode::FORBIDDEN,
                "this join token has no uses left".into(),
            ),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn random_hex(n_uuids: usize) -> String {
    (0..n_uuids)
        .map(|_| uuid::Uuid::new_v4().simple().to_string())
        .collect()
}

fn hash_secret(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

fn eq(a: &str, b: &str) -> bool {
    super::constant_time_eq(a, b)
}

/// `*` matches any run of characters; everything else is literal.
fn glob(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let mut rest = name;
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(p) else {
                return false;
            };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(p);
        } else {
            let Some(at) = rest.find(p) else { return false };
            rest = &rest[at + p.len()..];
        }
    }
    true
}

fn valid_node_id(id: &str) -> bool {
    (16..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl EnrollState {
    fn ensure_secret(&mut self) {
        if self.secret.is_empty() {
            self.secret = random_hex(2);
        }
    }

    fn reindex(&mut self) {
        self.by_id = self
            .nodes
            .iter()
            .map(|(n, e)| (e.node_id.clone(), n.clone()))
            .collect();
    }

    fn mac(&self, name: &str, node_id: &str, generation: u32) -> String {
        hex(&hmac_sha256(
            self.secret.as_bytes(),
            format!("cred|{name}|{node_id}|{generation}").as_bytes(),
        ))
    }

    fn credential(&self, name: &str, e: &Enrolled) -> String {
        format!(
            "{CRED_PREFIX}{}.{}.{}",
            e.node_id,
            e.generation,
            self.mac(name, &e.node_id, e.generation)
        )
    }

    /// Creates a join token; the plaintext is returned once and only its hash is kept.
    pub(super) fn create_token(
        &mut self,
        now: i64,
        ttl_secs: Option<i64>,
        max_uses: Option<u32>,
        name_pattern: Option<String>,
        note: String,
    ) -> (String, String) {
        let id = random_hex(1)[..12].to_owned();
        let secret = random_hex(2);
        self.tokens.insert(
            id.clone(),
            JoinToken {
                hash: hash_secret(&secret),
                created: now,
                expires: ttl_secs.map(|t| now + t),
                max_uses,
                uses: 0,
                name_pattern,
                revoked: false,
                note,
            },
        );
        (id.clone(), format!("{JOIN_PREFIX}{id}.{secret}"))
    }

    /// The token id behind a presented join token when it is genuine, unexpired and not revoked.
    fn check_token(&self, presented: &str, now: i64) -> Result<String, EnrollError> {
        let rest = presented
            .strip_prefix(JOIN_PREFIX)
            .ok_or(EnrollError::BadToken)?;
        let (id, secret) = rest.split_once('.').ok_or(EnrollError::BadToken)?;
        let t = self.tokens.get(id).ok_or(EnrollError::BadToken)?;
        if !eq(&t.hash, &hash_secret(secret)) || t.revoked || t.expires.is_some_and(|e| e <= now) {
            return Err(EnrollError::BadToken);
        }
        Ok(id.to_owned())
    }

    /// Enrolls `name` as `node_id` (or returns its existing credential for the same token and id).
    pub(super) fn enroll(
        &mut self,
        presented: &str,
        name: &str,
        node_id: &str,
        now: i64,
    ) -> Result<String, EnrollError> {
        let token_id = self.check_token(presented, now)?;
        if name.is_empty() || name.len() > 253 || name.chars().any(|c| c.is_control() || c == '/') {
            return Err(EnrollError::BadRequest("bad node name"));
        }
        if !valid_node_id(node_id) {
            return Err(EnrollError::BadRequest(
                "node_id must be 16-64 letters, digits or dashes",
            ));
        }
        if let Some(p) = &self.tokens[&token_id].name_pattern
            && !glob(p, name)
        {
            return Err(EnrollError::NameNotAllowed);
        }
        if let Some(e) = self.nodes.get(name) {
            if e.node_id != node_id {
                return Err(EnrollError::Conflict(
                    "that node name is already enrolled by a different node; the operator can rebind it",
                ));
            }
            if e.token_id != token_id {
                return Err(EnrollError::Conflict(
                    "that node is enrolled with a different join token",
                ));
            }
            if e.revoked {
                return Err(EnrollError::Conflict("that node's credential was revoked"));
            }
            return Ok(self.credential(name, e));
        }
        if self.by_id.contains_key(node_id) {
            return Err(EnrollError::Conflict(
                "that node_id is enrolled under another name",
            ));
        }
        {
            let t = &self.tokens[&token_id];
            if t.max_uses.is_some_and(|m| t.uses >= m) {
                return Err(EnrollError::Exhausted);
            }
        }
        self.ensure_secret();
        if let Some(t) = self.tokens.get_mut(&token_id) {
            t.uses += 1;
        }
        let e = Enrolled {
            node_id: node_id.to_owned(),
            token_id,
            generation: 1,
            prev_valid_until: None,
            revoked: false,
            enrolled_at: now,
        };
        let cred = self.credential(name, &e);
        self.by_id.insert(node_id.to_owned(), name.to_owned());
        self.nodes.insert(name.to_owned(), e);
        Ok(cred)
    }

    /// The node name behind a presented credential, if it is currently valid.
    pub(super) fn authenticate(&self, presented: &str, now: i64) -> Option<String> {
        let rest = presented.strip_prefix(CRED_PREFIX)?;
        let mut it = rest.splitn(3, '.');
        let (node_id, generation, mac) = (it.next()?, it.next()?.parse::<u32>().ok()?, it.next()?);
        let name = self.by_id.get(node_id)?;
        let e = self.nodes.get(name)?;
        if e.revoked || self.secret.is_empty() {
            return None;
        }
        let current = generation == e.generation;
        let previous =
            generation + 1 == e.generation && e.prev_valid_until.is_some_and(|t| t > now);
        if !(current || previous) {
            return None;
        }
        eq(mac, &self.mac(name, node_id, generation)).then(|| name.clone())
    }

    /// New generation for `name`; the previous credential works for a window. Returns the new credential.
    pub(super) fn rotate(&mut self, name: &str, now: i64) -> Option<String> {
        let e = self.nodes.get_mut(name)?;
        if e.revoked {
            return None;
        }
        e.generation += 1;
        e.prev_valid_until = Some(now + ROTATION_WINDOW_SECS);
        let e = e.clone();
        Some(self.credential(name, &e))
    }

    pub(super) fn revoke(&mut self, name: &str) -> bool {
        self.nodes.get_mut(name).map(|e| e.revoked = true).is_some()
    }

    /// Forget the binding so the name can enroll again from new hardware (with a join token).
    pub(super) fn rebind(&mut self, name: &str) -> bool {
        match self.nodes.remove(name) {
            Some(e) => {
                self.by_id.remove(&e.node_id);
                true
            }
            None => false,
        }
    }

    /// A deregistered node loses its credential.
    pub(super) fn forget(&mut self, name: &str) {
        self.rebind(name);
    }
}

/// Loaded enrollment state plus where it is saved.
pub(super) struct Enroll {
    pub(super) state: tokio::sync::Mutex<EnrollState>,
    path: Option<PathBuf>,
    lock: Option<PathBuf>,
    /// Tighter than the heartbeat bucket: enrollment is rare and each attempt hashes a secret.
    pub(super) admission: super::registry::Admission,
}

impl Default for Enroll {
    fn default() -> Self {
        Enroll {
            state: Default::default(),
            path: None,
            lock: None,
            admission: super::registry::Admission::new(500.0, 1000.0, 256),
        }
    }
}

impl Enroll {
    pub(super) fn open(state_dir: &std::path::Path) -> Self {
        let path = state_dir.join("fleet-enroll.json");
        let mut st: EnrollState = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        st.reindex();
        Enroll {
            state: tokio::sync::Mutex::new(st),
            path: Some(path),
            lock: Some(state_dir.join("fleet-enroll.lock")),
            admission: super::registry::Admission::new(500.0, 1000.0, 256),
        }
    }

    fn save(&self, st: &EnrollState) {
        if let (Some(p), Some(l)) = (&self.path, &self.lock) {
            persist_json(p, l, st);
        }
    }

    /// A node that was deregistered stops working at once.
    pub(super) async fn forget(&self, name: &str) {
        let mut st = self.state.lock().await;
        st.forget(name);
        self.save(&st);
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

#[derive(Deserialize)]
pub(super) struct EnrollRequest {
    name: String,
    node_id: String,
}

/// `POST /fleet/enroll`, authenticated by the join token in the `Authorization` header.
pub(super) async fn enroll(
    State(fleet): State<Fleet>,
    headers: HeaderMap,
    Json(req): Json<EnrollRequest>,
) -> Result<Json<Value>, AppError> {
    if node_auth() == NodeAuth::Legacy {
        return Err(AppError(
            StatusCode::NOT_FOUND,
            "enrollment is off (--node-auth legacy)".into(),
        ));
    }
    let _permit = fleet.registry.enroll.admission.admit().map_err(|s| {
        AppError(
            StatusCode::TOO_MANY_REQUESTS,
            format!("enrollment is busy; retry in {s}s"),
        )
    })?;
    let presented = bearer(&headers).ok_or(EnrollError::BadToken.into_app())?;
    let mut st = fleet.registry.enroll.state.lock().await;
    let before = st.nodes.len();
    let uses_before: u32 = st.tokens.values().map(|t| t.uses).sum();
    let cred = st
        .enroll(presented, &req.name, &req.node_id, Utc::now().timestamp())
        .map_err(EnrollError::into_app)?;
    if st.nodes.len() != before || st.tokens.values().map(|t| t.uses).sum::<u32>() != uses_before {
        fleet.registry.enroll.save(&st);
        tracing::info!(node = %req.name, "node enrolled");
    }
    Ok(Json(json!({"credential": cred, "node": req.name})))
}

/// `POST /fleet/enroll/renew`: a node (authenticated by its current credential) gets the next generation.
pub(super) async fn renew(
    State(fleet): State<Fleet>,
    axum::Extension(id): axum::Extension<NodeIdentity>,
) -> Result<Json<Value>, AppError> {
    let mut st = fleet.registry.enroll.state.lock().await;
    let cred = st.rotate(&id.0, Utc::now().timestamp()).ok_or(AppError(
        StatusCode::FORBIDDEN,
        "no credential to renew".into(),
    ))?;
    fleet.registry.enroll.save(&st);
    Ok(Json(json!({"credential": cred, "node": id.0})))
}

#[derive(Deserialize, Default)]
pub(super) struct NewToken {
    #[serde(default)]
    ttl_secs: Option<i64>,
    #[serde(default)]
    max_uses: Option<u32>,
    #[serde(default)]
    name_pattern: Option<String>,
    #[serde(default)]
    note: String,
}

/// `POST /fleet/enroll/tokens` (operator): the token is shown once.
pub(super) async fn create_token(
    State(fleet): State<Fleet>,
    Json(req): Json<NewToken>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    if req.ttl_secs.is_some_and(|t| t <= 0) || req.max_uses == Some(0) {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "ttl_secs and max_uses must be positive".into(),
        ));
    }
    let mut st = fleet.registry.enroll.state.lock().await;
    let (id, token) = st.create_token(
        Utc::now().timestamp(),
        req.ttl_secs,
        req.max_uses,
        req.name_pattern,
        req.note,
    );
    fleet.registry.enroll.save(&st);
    Ok((
        StatusCode::CREATED,
        Json(json!({"id": id, "token": token, "shown_once": true})),
    ))
}

/// `GET /fleet/enroll/tokens`: ids and limits, never secrets.
pub(super) async fn list_tokens(State(fleet): State<Fleet>) -> Json<Value> {
    let st = fleet.registry.enroll.state.lock().await;
    let items: Vec<Value> = st
        .tokens
        .iter()
        .map(|(id, t)| {
            json!({"id": id, "created": t.created, "expires": t.expires, "max_uses": t.max_uses, "uses": t.uses,
                   "name_pattern": t.name_pattern, "revoked": t.revoked, "note": t.note})
        })
        .collect();
    Json(json!({"items": items}))
}

/// `DELETE /fleet/enroll/tokens/{id}`: no new nodes with it; nodes already enrolled keep their credentials.
pub(super) async fn revoke_token(
    State(fleet): State<Fleet>,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let mut st = fleet.registry.enroll.state.lock().await;
    match st.tokens.get_mut(&id) {
        Some(t) => t.revoked = true,
        None => {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("no join token '{id}'"),
            ));
        }
    }
    fleet.registry.enroll.save(&st);
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /fleet/enroll/nodes`.
pub(super) async fn list_enrolled(State(fleet): State<Fleet>) -> Json<Value> {
    let st = fleet.registry.enroll.state.lock().await;
    let items: Vec<Value> = st
        .nodes
        .iter()
        .map(|(n, e)| json!({"name": n, "node_id": e.node_id, "token_id": e.token_id, "generation": e.generation,
                             "revoked": e.revoked, "enrolled_at": e.enrolled_at}))
        .collect();
    Json(json!({"items": items}))
}

/// `POST /fleet/nodes/{name}/revoke|rotate|rebind` (operator).
pub(super) async fn node_action(
    State(fleet): State<Fleet>,
    Path((name, action)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let mut st = fleet.registry.enroll.state.lock().await;
    let now = Utc::now().timestamp();
    let out = match action.as_str() {
        "revoke" if st.revoke(&name) => json!({"revoked": name}),
        "rebind" if st.rebind(&name) => json!({"rebound": name}),
        "rotate" => match st.rotate(&name, now) {
            Some(c) => json!({"credential": c, "previous_valid_secs": ROTATION_WINDOW_SECS}),
            None => {
                return Err(AppError(
                    StatusCode::NOT_FOUND,
                    format!("no enrolled, unrevoked node '{name}'"),
                ));
            }
        },
        "revoke" | "rebind" => {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("no enrolled node '{name}'"),
            ));
        }
        _ => {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                "unknown node action".into(),
            ));
        }
    };
    fleet.registry.enroll.save(&st);
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;
    const ID1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ID2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn st_with(max_uses: Option<u32>, pattern: Option<&str>) -> (EnrollState, String) {
        let mut st = EnrollState::default();
        let (_, tok) = st.create_token(
            NOW,
            Some(3600),
            max_uses,
            pattern.map(str::to_owned),
            String::new(),
        );
        (st, tok)
    }

    #[test]
    fn hmac_matches_rfc_4231_case_1() {
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn enrolls_and_authenticates_and_stores_no_secret() {
        let (mut st, tok) = st_with(None, None);
        let cred = st.enroll(&tok, "n1", ID1, NOW).unwrap();
        assert_eq!(st.authenticate(&cred, NOW).as_deref(), Some("n1"));
        let json = serde_json::to_string(&st).unwrap();
        assert!(!json.contains(&tok), "the join token must not be stored");
        assert!(!json.contains(&cred), "the credential must not be stored");
        assert_eq!(st.authenticate("fvnc_junk", NOW), None);
        let mut forged = cred.clone();
        forged.pop();
        forged.push(if cred.ends_with('0') { '1' } else { '0' });
        assert_eq!(st.authenticate(&forged, NOW), None);
    }

    #[test]
    fn a_repeated_enroll_returns_the_same_credential_and_burns_no_use() {
        let (mut st, tok) = st_with(Some(1), None);
        let a = st.enroll(&tok, "n1", ID1, NOW).unwrap();
        let b = st.enroll(&tok, "n1", ID1, NOW).unwrap();
        assert_eq!(a, b);
        assert_eq!(st.enroll(&tok, "n2", ID2, NOW), Err(EnrollError::Exhausted));
    }

    #[test]
    fn a_name_taken_by_another_node_is_a_conflict_until_rebound() {
        let (mut st, tok) = st_with(None, None);
        let old = st.enroll(&tok, "n1", ID1, NOW).unwrap();
        assert!(matches!(
            st.enroll(&tok, "n1", ID2, NOW),
            Err(EnrollError::Conflict(_))
        ));
        assert_eq!(
            st.authenticate(&old, NOW).as_deref(),
            Some("n1"),
            "the original is intact"
        );
        assert!(
            matches!(
                st.enroll(&tok, "other", ID1, NOW),
                Err(EnrollError::Conflict(_))
            ),
            "one id, one name"
        );
        assert!(st.rebind("n1"));
        let new = st.enroll(&tok, "n1", ID2, NOW).unwrap();
        assert_eq!(st.authenticate(&new, NOW).as_deref(), Some("n1"));
        assert_eq!(
            st.authenticate(&old, NOW),
            None,
            "the old hardware's credential is dead"
        );
    }

    #[test]
    fn tokens_expire_revoke_and_restrict_names() {
        let (mut st, tok) = st_with(None, Some("rack1-*"));
        assert_eq!(
            st.enroll(&tok, "rack2-a", ID1, NOW),
            Err(EnrollError::NameNotAllowed)
        );
        assert!(st.enroll(&tok, "rack1-a", ID1, NOW).is_ok());
        assert_eq!(
            st.enroll(&tok, "rack1-b", ID2, NOW + 3600),
            Err(EnrollError::BadToken),
            "expired"
        );
        let id = tok
            .strip_prefix(JOIN_PREFIX)
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .to_owned();
        st.tokens.get_mut(&id).unwrap().revoked = true;
        assert_eq!(
            st.enroll(&tok, "rack1-b", ID2, NOW),
            Err(EnrollError::BadToken),
            "revoked"
        );
        let wrong = format!("{JOIN_PREFIX}{id}.{}", "0".repeat(64));
        assert_eq!(
            st.enroll(&wrong, "rack1-c", ID2, NOW),
            Err(EnrollError::BadToken)
        );
        assert_eq!(
            st.enroll("fvt_notajointoken", "rack1-c", ID2, NOW),
            Err(EnrollError::BadToken)
        );
    }

    #[test]
    fn revoking_stops_a_healthy_node_and_rotation_has_a_window() {
        let (mut st, tok) = st_with(None, None);
        let g1 = st.enroll(&tok, "n1", ID1, NOW).unwrap();
        let g2 = st.rotate("n1", NOW).unwrap();
        assert_ne!(g1, g2);
        assert!(
            st.authenticate(&g1, NOW + 10).is_some(),
            "previous credential inside the window"
        );
        assert!(st.authenticate(&g2, NOW + 10).is_some());
        assert!(
            st.authenticate(&g1, NOW + ROTATION_WINDOW_SECS + 1)
                .is_none(),
            "and not after it"
        );
        assert!(st.revoke("n1"));
        assert!(st.authenticate(&g2, NOW).is_none());
        assert!(st.rotate("n1", NOW).is_none());
    }

    #[test]
    fn a_credential_does_not_work_after_the_node_is_forgotten() {
        let (mut st, tok) = st_with(None, None);
        let c = st.enroll(&tok, "n1", ID1, NOW).unwrap();
        st.forget("n1");
        assert!(st.authenticate(&c, NOW).is_none());
    }

    #[test]
    fn bad_input_is_rejected() {
        let (mut st, tok) = st_with(None, None);
        assert!(matches!(
            st.enroll(&tok, "n1", "short", NOW),
            Err(EnrollError::BadRequest(_))
        ));
        assert!(matches!(
            st.enroll(&tok, "", ID1, NOW),
            Err(EnrollError::BadRequest(_))
        ));
        assert!(matches!(
            st.enroll(&tok, "a/b", ID1, NOW),
            Err(EnrollError::BadRequest(_))
        ));
    }

    #[test]
    fn globs() {
        assert!(
            glob("a*", "abc")
                && glob("*c", "abc")
                && glob("a*c", "abc")
                && glob("*", "")
                && glob("abc", "abc")
        );
        assert!(!glob("a*", "b") && !glob("abc", "abd") && !glob("a*c*e", "abde"));
    }

    #[tokio::test]
    async fn one_hundred_uses_among_one_hundred_fifty_concurrent_enrolls() {
        let (st, tok) = st_with(Some(100), None);
        let st = std::sync::Arc::new(tokio::sync::Mutex::new(st));
        let mut tasks = Vec::new();
        for i in 0..150 {
            let (st, tok) = (st.clone(), tok.clone());
            tasks.push(tokio::spawn(async move {
                let id = format!("{i:032}");
                st.lock()
                    .await
                    .enroll(&tok, &format!("n{i}"), &id, NOW)
                    .is_ok()
            }));
        }
        let mut ok = 0;
        for t in tasks {
            ok += t.await.unwrap() as u32;
        }
        assert_eq!(ok, 100);
        assert_eq!(st.lock().await.nodes.len(), 100);
    }

    use crate::central::{CentralConfig, router};
    use axum::body::Body;
    use tower::ServiceExt;

    async fn call(
        app: &axum::Router,
        method: &str,
        uri: &str,
        token: &str,
        body: Value,
    ) -> (u16, Value) {
        let mut b = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if !token.is_empty() {
            b = b.header("authorization", format!("Bearer {token}"));
        }
        let resp = app
            .clone()
            .oneshot(b.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn beat(name: &str) -> Value {
        json!({"name": name, "fluxvm_url": "http://x", "vcpus_total": 4, "memory_mib_total": 4096, "vm_count": 0})
    }

    #[tokio::test]
    async fn over_http_a_node_credential_is_a_node_and_nothing_more() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some("admin".into()),
            reconcile_interval: None,
            ha: None,
        });
        // The operator makes a join token; a stranger cannot.
        let (s, _) = call(&app, "POST", "/fleet/enroll/tokens", "nope", json!({})).await;
        assert_eq!(s, 401);
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/enroll/tokens",
            "admin",
            json!({"max_uses": 5}),
        )
        .await;
        assert_eq!(s, 201, "{v}");
        let join = v["token"].as_str().unwrap().to_owned();
        // The join token enrolls, and is good for nothing else.
        let (s, _) = call(&app, "GET", "/fleet/nodes", &join, json!({})).await;
        assert_eq!(s, 401, "a join token is not an API token");
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/enroll",
            &join,
            json!({"name": "n1", "node_id": ID1}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let cred = v["credential"].as_str().unwrap().to_owned();
        let (s, v2) = call(
            &app,
            "POST",
            "/fleet/enroll",
            &join,
            json!({"name": "n1", "node_id": ID1}),
        )
        .await;
        assert!(
            (s, &v2["credential"]) == (200, &json!(cred)),
            "idempotent: {v2}"
        );
        let (s, _) = call(
            &app,
            "POST",
            "/fleet/enroll",
            &join,
            json!({"name": "n1", "node_id": ID2}),
        )
        .await;
        assert_eq!(s, 409);
        let (s, _) = call(
            &app,
            "POST",
            "/fleet/enroll",
            "fvjt_bad.bad",
            json!({"name": "n9", "node_id": ID2}),
        )
        .await;
        assert_eq!(s, 401);

        // The credential registers its own node, only that node, and cannot reach anything else.
        let (s, v) = call(&app, "POST", "/fleet/register", &cred, beat("n1")).await;
        assert_eq!(s, 200, "{v}");
        let (s, _) = call(&app, "POST", "/fleet/register", &cred, beat("someone-else")).await;
        assert_eq!(s, 403);
        for (m, u) in [
            ("GET", "/fleet/nodes"),
            ("GET", "/fleet/enroll/tokens"),
            ("POST", "/fleet/enroll/tokens"),
            ("DELETE", "/fleet/nodes/n1"),
        ] {
            let (s, _) = call(&app, m, u, &cred, json!({})).await;
            assert_eq!(s, 403, "{m} {u}");
        }
        let (s, _) = call(&app, "GET", "/fleet/nodes/n1/overlays", &cred, json!({})).await;
        assert_eq!(s, 200);
        let (s, _) = call(&app, "GET", "/fleet/nodes/other/overlays", &cred, json!({})).await;
        assert_eq!(s, 403);
        // The fleet token still registers while the mode is `both`.
        let (s, _) = call(&app, "POST", "/fleet/register", "admin", beat("legacy")).await;
        assert_eq!(s, 200);

        // Renewal hands out the next generation; the old one keeps working for the window.
        let (s, v) = call(&app, "POST", "/fleet/enroll/renew", &cred, json!({})).await;
        assert_eq!(s, 200);
        let cred2 = v["credential"].as_str().unwrap().to_owned();
        assert_ne!(cred, cred2);
        assert_eq!(
            call(&app, "POST", "/fleet/register", &cred2, beat("n1"))
                .await
                .0,
            200
        );
        assert_eq!(
            call(&app, "POST", "/fleet/register", &cred, beat("n1"))
                .await
                .0,
            200
        );

        // Revoking works on a healthy node and stops only that node.
        let (s, _) = call(&app, "POST", "/fleet/nodes/n1/revoke", "admin", json!({})).await;
        assert_eq!(s, 200);
        assert_eq!(
            call(&app, "POST", "/fleet/register", &cred2, beat("n1"))
                .await
                .0,
            401
        );
        assert_eq!(
            call(&app, "POST", "/fleet/register", "admin", beat("legacy"))
                .await
                .0,
            200
        );

        // Enrollment survives a restart of central.
        let (s, v) = call(
            &app,
            "POST",
            "/fleet/enroll",
            &join,
            json!({"name": "n2", "node_id": ID2}),
        )
        .await;
        assert_eq!(s, 200, "{v}");
        let c2 = v["credential"].as_str().unwrap().to_owned();
        let app2 = router(CentralConfig {
            state_dir: dir.path().to_path_buf(),
            token: Some("admin".into()),
            reconcile_interval: None,
            ha: None,
        });
        assert_eq!(
            call(&app2, "POST", "/fleet/register", &c2, beat("n2"))
                .await
                .0,
            200
        );
        assert_eq!(
            call(&app2, "POST", "/fleet/register", &cred2, beat("n1"))
                .await
                .0,
            401,
            "revocation persisted"
        );
    }
}
