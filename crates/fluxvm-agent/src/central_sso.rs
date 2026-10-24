// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Sign-in for people: OpenID Connect single sign-on for the console, team members with roles, and sessions.
//!
//! # Flow
//!
//! `GET /auth/login` sends the browser to the identity provider (authorization code flow with PKCE, `state` and
//! `nonce` kept here for ten minutes and `state` also bound to the browser by a short-lived cookie). The provider sends
//! it back to `GET /auth/callback`, where central swaps the code for tokens at the provider's token endpoint and
//! verifies the ID token: signature against the provider's JWKS (RS256/384/512, PS256/384/512, ES256/384), `iss`,
//! `aud` (and `azp`), `exp`, `nonce` and `email_verified`. The verified email then decides access:
//!
//! - an operator email (`--sso-operator`, or `operator_emails` in the policy) signs in as the operator;
//! - a member of a tenant signs in as that tenant with their role (`?tenant=` picks one when they are in several);
//! - anyone else is refused with a page saying so.
//!
//! A sign-in issues a session: a random token, stored only as its SHA-256 in the tenancy state (so it survives a
//! restart and follows HA failover), sent as an `HttpOnly; SameSite=Lax` cookie (`Secure` when the public URL is
//! https). [`caller_from_cookie`] turns the cookie back into a caller for `auth_middleware`. Changes made with a cookie
//! must carry the `X-FluxVM-Console` header and, when the browser sends one, a same-origin `Origin`: a cross-site page
//! can make the browser send the cookie, but not that header.
//!
//! # Roles
//!
//! The role is read from the tenancy state on every request, so changing or removing a member applies at once.
//! See [`role_refusal`]: a viewer only reads; a member manages VMs, volumes, snapshots and networks; an admin does every
//! tenant action including members, but cannot touch owners; an owner does everything. Tokens and API keys keep their
//! old meaning: the tenant's primary token acts as an owner, named keys as admins (read-only keys only read) that
//! cannot change members, like they cannot change keys.

use super::*;
use crate::tenancy::{MemberError, MemberRole, normalize_email};
use axum::extract::Query;
use axum::http::{HeaderMap, Method};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::JwkSet};
use std::time::{Duration as StdDuration, Instant};

/// The session cookie.
pub(super) const SESSION_COOKIE: &str = "fluxvm_session";
/// Binds a sign-in's `state` to the browser that started it.
const STATE_COOKIE: &str = "fluxvm_sso_state";
/// The header the console sends with every call; required on changes made with the session cookie.
pub(super) const CONSOLE_HEADER: &str = "x-fluxvm-console";
/// How long a started sign-in may take.
const LOGIN_TTL_SECS: i64 = 600;
/// Most sign-ins in flight at once (the oldest is dropped).
const MAX_PENDING: usize = 1024;
/// How long the provider's discovery document and keys are trusted before they are fetched again.
const PROVIDER_TTL: StdDuration = StdDuration::from_secs(3600);
/// Least time between two JWKS fetches for an unknown key id.
const JWKS_MIN_REFRESH: StdDuration = StdDuration::from_secs(30);
/// Largest answer read from the identity provider.
const MAX_IDP_BODY: usize = 1024 * 1024;
/// Clock skew allowed on `exp`.
const LEEWAY_SECS: i64 = 60;
/// Signature algorithms accepted on ID tokens (never `none` or a shared-secret HMAC).
const ALLOWED_ALGS: [Algorithm; 8] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

// ---- configuration --------------------------------------------------------------------------------------------------

/// `fluxvm-agent central` flags for single sign-on (flattened into the command).
#[derive(clap::Args, Debug, Clone)]
pub struct SsoArgs {
    /// OpenID Connect issuer URL (e.g. `https://acme.okta.com`); turns console SSO on. Discovery is read from
    /// `{issuer}/.well-known/openid-configuration`.
    #[arg(long, env = "FLUXVM_OIDC_ISSUER")]
    pub oidc_issuer: Option<String>,
    /// The client (application) id registered at the identity provider.
    #[arg(long, env = "FLUXVM_OIDC_CLIENT_ID")]
    pub oidc_client_id: Option<String>,
    /// The client secret (omit for a public client; PKCE is always used).
    #[arg(long, env = "FLUXVM_OIDC_CLIENT_SECRET", hide_env_values = true)]
    pub oidc_client_secret: Option<String>,
    /// Read the client secret from this file instead (first line).
    #[arg(
        long,
        env = "FLUXVM_OIDC_CLIENT_SECRET_FILE",
        conflicts_with = "oidc_client_secret"
    )]
    pub oidc_client_secret_file: Option<PathBuf>,
    /// This central's URL as browsers reach it (e.g. `https://cloud.example.com`); the redirect URI registered at
    /// the provider is `{public-url}/auth/callback`.
    #[arg(long, env = "FLUXVM_PUBLIC_URL")]
    pub public_url: Option<String>,
    /// The provider's name on the console's sign-in button ("Sign in with Okta").
    #[arg(long, env = "FLUXVM_OIDC_PROVIDER_NAME", default_value = "SSO")]
    pub oidc_provider_name: String,
    /// An email that signs in as the operator (repeatable, or comma-separated in the environment).
    #[arg(
        long = "sso-operator",
        env = "FLUXVM_SSO_OPERATORS",
        value_delimiter = ','
    )]
    pub sso_operators: Vec<String>,
    /// Hours a console session lasts (1-720).
    #[arg(long, env = "FLUXVM_SESSION_HOURS", default_value_t = 12)]
    pub session_hours: u32,
}

/// Validated SSO settings. Not `Debug`: it holds the client secret.
#[derive(Clone)]
pub struct SsoConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    /// Without a trailing `/`.
    pub public_url: String,
    pub provider_name: String,
    /// Lowercase emails that sign in as the operator.
    pub operators: Vec<String>,
    pub session_ttl: chrono::Duration,
}

impl SsoConfig {
    fn redirect_uri(&self) -> String {
        format!("{}/auth/callback", self.public_url)
    }

    fn secure_cookies(&self) -> bool {
        self.public_url.starts_with("https://")
    }
}

/// `https://`, or `http://` to a loopback host (a provider or console on the same machine, and tests).
fn acceptable_url(u: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(u) else {
        return false;
    };
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => matches!(
            url.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("[::1]")
        ),
        _ => false,
    }
}

impl SsoArgs {
    /// The settings, `None` when SSO is off, or why the flags do not make sense.
    pub fn into_config(self) -> anyhow::Result<Option<SsoConfig>> {
        let Some(issuer) = self.oidc_issuer.filter(|s| !s.trim().is_empty()) else {
            if self.oidc_client_id.is_some() || !self.sso_operators.is_empty() {
                anyhow::bail!("--oidc-client-id and --sso-operator need --oidc-issuer");
            }
            return Ok(None);
        };
        let issuer = issuer.trim().trim_end_matches('/').to_owned();
        if !acceptable_url(&issuer) {
            anyhow::bail!(
                "--oidc-issuer must be an https:// URL (http:// only for localhost), not '{issuer}'"
            );
        }
        let Some(client_id) = self.oidc_client_id.filter(|s| !s.trim().is_empty()) else {
            anyhow::bail!("--oidc-issuer needs --oidc-client-id");
        };
        let Some(public_url) = self.public_url.filter(|s| !s.trim().is_empty()) else {
            anyhow::bail!(
                "--oidc-issuer needs --public-url (this central's URL as browsers reach it, e.g. https://cloud.example.com)"
            );
        };
        let public_url = public_url.trim().trim_end_matches('/').to_owned();
        if !acceptable_url(&public_url) {
            anyhow::bail!(
                "--public-url must be an https:// URL (http:// only for localhost), not '{public_url}'"
            );
        }
        let client_secret = match (self.oidc_client_secret, self.oidc_client_secret_file) {
            (Some(s), _) => Some(s.trim().to_owned()),
            (None, Some(path)) => {
                let raw = fs::read_to_string(&path).map_err(|e| {
                    anyhow::anyhow!("reading --oidc-client-secret-file {}: {e}", path.display())
                })?;
                Some(raw.lines().next().unwrap_or("").trim().to_owned())
            }
            (None, None) => None,
        }
        .filter(|s| !s.is_empty());
        let mut operators = Vec::new();
        for e in &self.sso_operators {
            match normalize_email(e) {
                Some(e) => operators.push(e),
                None => anyhow::bail!("--sso-operator '{e}' is not an email address"),
            }
        }
        if !(1..=720).contains(&self.session_hours) {
            anyhow::bail!("--session-hours must be between 1 and 720");
        }
        let provider_name = self
            .oidc_provider_name
            .trim()
            .chars()
            .take(40)
            .collect::<String>();
        Ok(Some(SsoConfig {
            issuer,
            client_id: client_id.trim().to_owned(),
            client_secret,
            public_url,
            provider_name: if provider_name.is_empty() {
                "SSO".into()
            } else {
                provider_name
            },
            operators,
            session_ttl: chrono::Duration::hours(i64::from(self.session_hours)),
        }))
    }
}

// ---- runtime --------------------------------------------------------------------------------------------------------

/// A sign-in between `/auth/login` and `/auth/callback`.
struct Pending {
    nonce: String,
    verifier: String,
    created: DateTime<Utc>,
    tenant: Option<String>,
}

/// The parts of the provider's discovery document central uses.
#[derive(Debug, Clone, Deserialize)]
struct Provider {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

/// One signing key from the provider's JWKS.
#[derive(Clone)]
struct KeyEntry {
    kid: Option<String>,
    alg: Option<Algorithm>,
    key: DecodingKey,
}

#[derive(Default)]
struct Jwks {
    keys: Vec<KeyEntry>,
    fetched: Option<Instant>,
}

/// SSO state of one central. Without a config every `/auth` route says SSO is off and cookies are ignored.
pub(super) struct Sso {
    cfg: Option<SsoConfig>,
    http: reqwest::Client,
    pending: std::sync::Mutex<HashMap<String, Pending>>,
    /// Sign-in starts per client address in the last minute (see [`LOGINS_PER_ADDRESS_PER_MINUTE`]).
    logins: std::sync::Mutex<HashMap<std::net::IpAddr, std::collections::VecDeque<Instant>>>,
    provider: tokio::sync::Mutex<Option<(Instant, Provider)>>,
    jwks: tokio::sync::Mutex<Jwks>,
}

impl Default for Sso {
    fn default() -> Self {
        Sso::new(None)
    }
}

/// Sign-in starts one client address may make in a minute. `/auth/login` is public and each start holds a pending
/// sign-in (the oldest is dropped past [`MAX_PENDING`]), so without this one caller could evict everyone's.
const LOGINS_PER_ADDRESS_PER_MINUTE: usize = 20;
/// Client addresses remembered at once; past this, an address not yet seen is refused until the window clears.
const MAX_LOGIN_ADDRESSES: usize = 10_000;

impl Sso {
    /// Counts one sign-in start from `addr`; `false` when it has made too many in the last minute.
    pub(super) fn allow_login(&self, addr: std::net::IpAddr, now: Instant) -> bool {
        let window = StdDuration::from_secs(60);
        let mut map = self.logins.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(&addr) && map.len() >= MAX_LOGIN_ADDRESSES {
            map.retain(|_, q| {
                while q
                    .front()
                    .is_some_and(|t| now.saturating_duration_since(*t) >= window)
                {
                    q.pop_front();
                }
                !q.is_empty()
            });
            if map.len() >= MAX_LOGIN_ADDRESSES {
                return false;
            }
        }
        let q = map.entry(addr).or_default();
        while q
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= window)
        {
            q.pop_front();
        }
        if q.len() >= LOGINS_PER_ADDRESS_PER_MINUTE {
            return false;
        }
        q.push_back(now);
        true
    }

    pub(super) fn new(cfg: Option<SsoConfig>) -> Self {
        Sso {
            cfg,
            http: reqwest::Client::builder()
                .timeout(StdDuration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            pending: std::sync::Mutex::new(HashMap::new()),
            logins: std::sync::Mutex::new(HashMap::new()),
            provider: tokio::sync::Mutex::new(None),
            jwks: tokio::sync::Mutex::new(Jwks::default()),
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.cfg.is_some()
    }

    /// Remembers a sign-in under its `state`, dropping expired ones and, when full, the oldest.
    fn remember(&self, state: String, p: Pending) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let now = Utc::now();
        pending.retain(|_, x| (now - x.created).num_seconds() < LOGIN_TTL_SECS);
        if pending.len() >= MAX_PENDING
            && let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, x)| x.created)
                .map(|(k, _)| k.clone())
        {
            pending.remove(&oldest);
        }
        pending.insert(state, p);
    }

    /// Takes (once) the sign-in started with `state`, if it has not expired.
    fn take(&self, state: &str) -> Option<Pending> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let p = pending.remove(state)?;
        ((Utc::now() - p.created).num_seconds() < LOGIN_TTL_SECS).then_some(p)
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let resp = self
            .http
            .get(url)
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = resp.status();
        let body = read_capped(resp).await?;
        if !status.is_success() {
            return Err(format!("{url} answered {status}"));
        }
        serde_json::from_slice(&body).map_err(|e| format!("{url}: {e}"))
    }

    /// The provider's discovery document (cached for an hour; a stale copy is used when the provider is down).
    async fn provider(&self) -> Result<Provider, String> {
        let cfg = self.cfg.as_ref().ok_or("single sign-on is off")?;
        let mut cached = self.provider.lock().await;
        if let Some((at, p)) = cached.as_ref()
            && at.elapsed() < PROVIDER_TTL
        {
            return Ok(p.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", cfg.issuer);
        let fetched = self.get_json::<Provider>(&url).await.and_then(|p| {
            if p.issuer.trim_end_matches('/') != cfg.issuer {
                return Err(format!(
                    "the discovery document names issuer '{}', not the configured '{}'",
                    p.issuer, cfg.issuer
                ));
            }
            for u in [&p.authorization_endpoint, &p.token_endpoint, &p.jwks_uri] {
                if !acceptable_url(u) {
                    return Err(format!("the provider's endpoint '{u}' is not https"));
                }
            }
            Ok(p)
        });
        match fetched {
            Ok(p) => {
                *cached = Some((Instant::now(), p.clone()));
                Ok(p)
            }
            Err(e) => match cached.as_ref() {
                Some((_, p)) => {
                    tracing::warn!(error = %e, "OIDC discovery failed; using the last copy");
                    Ok(p.clone())
                }
                None => Err(e),
            },
        }
    }

    /// The provider's signing keys, fetched again when stale or when `kid` is unknown (at most every 30 s).
    async fn keys(&self, provider: &Provider, kid: Option<&str>) -> Result<Vec<KeyEntry>, String> {
        let mut jwks = self.jwks.lock().await;
        let known = kid.is_none_or(|k| jwks.keys.iter().any(|e| e.kid.as_deref() == Some(k)));
        let refresh = match jwks.fetched {
            None => true,
            Some(at) => at.elapsed() > PROVIDER_TTL || (!known && at.elapsed() > JWKS_MIN_REFRESH),
        };
        if refresh {
            match self.get_json::<JwkSet>(&provider.jwks_uri).await {
                Ok(set) => {
                    jwks.keys = key_entries(set);
                    jwks.fetched = Some(Instant::now());
                }
                Err(e) if jwks.keys.is_empty() => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "JWKS fetch failed; using the keys already known")
                }
            }
        }
        Ok(jwks.keys.clone())
    }

    /// Swaps an authorization code for the provider's tokens and returns the ID token.
    async fn exchange(
        &self,
        cfg: &SsoConfig,
        provider: &Provider,
        code: &str,
        verifier: &str,
    ) -> Result<String, String> {
        let mut form: Vec<(&str, String)> = vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.to_owned()),
            ("redirect_uri", cfg.redirect_uri()),
            ("code_verifier", verifier.to_owned()),
        ];
        let mut req = self
            .http
            .post(&provider.token_endpoint)
            .header(header::ACCEPT, "application/json");
        // client_secret_basic is the default method when the provider does not list any.
        let basic = provider.token_endpoint_auth_methods_supported.is_empty()
            || provider
                .token_endpoint_auth_methods_supported
                .iter()
                .any(|m| m == "client_secret_basic");
        match &cfg.client_secret {
            Some(secret) if basic => req = req.basic_auth(&cfg.client_id, Some(secret)),
            Some(secret) => {
                form.push(("client_id", cfg.client_id.clone()));
                form.push(("client_secret", secret.clone()));
            }
            None => form.push(("client_id", cfg.client_id.clone())),
        }
        let resp = req
            .form(&form)
            .send()
            .await
            .map_err(|e| format!("the token endpoint could not be reached: {e}"))?;
        let status = resp.status();
        let body = read_capped(resp).await?;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if !status.is_success() {
            let why = v
                .get("error_description")
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            return Err(format!(
                "the provider refused the sign-in code ({status}): {why}"
            ));
        }
        v.get("id_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                "the provider's token answer has no id_token (is the 'openid' scope allowed?)"
                    .into()
            })
    }

    /// Verifies an ID token's signature and claims; the verified identity.
    async fn verify(
        &self,
        cfg: &SsoConfig,
        provider: &Provider,
        id_token: &str,
        nonce: &str,
    ) -> Result<Identity, String> {
        let header = jsonwebtoken::decode_header(id_token).map_err(|e| format!("ID token: {e}"))?;
        let keys = self.keys(provider, header.kid.as_deref()).await?;
        let claims = verify_signature(id_token, &keys, &cfg.issuer, &cfg.client_id)?;
        check_claims(
            &claims,
            &cfg.issuer,
            &cfg.client_id,
            nonce,
            Utc::now().timestamp(),
        )
    }
}

/// Reads a response body, refusing more than [`MAX_IDP_BODY`].
async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        if out.len() + chunk.len() > MAX_IDP_BODY {
            return Err("the identity provider's answer is too large".into());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn key_entries(set: JwkSet) -> Vec<KeyEntry> {
    use jsonwebtoken::jwk::{KeyAlgorithm, PublicKeyUse};
    set.keys
        .iter()
        .filter(|jwk| !matches!(jwk.common.public_key_use, Some(PublicKeyUse::Encryption)))
        .filter_map(|jwk| {
            let alg = match jwk.common.key_algorithm {
                None => None,
                Some(KeyAlgorithm::RS256) => Some(Algorithm::RS256),
                Some(KeyAlgorithm::RS384) => Some(Algorithm::RS384),
                Some(KeyAlgorithm::RS512) => Some(Algorithm::RS512),
                Some(KeyAlgorithm::PS256) => Some(Algorithm::PS256),
                Some(KeyAlgorithm::PS384) => Some(Algorithm::PS384),
                Some(KeyAlgorithm::PS512) => Some(Algorithm::PS512),
                Some(KeyAlgorithm::ES256) => Some(Algorithm::ES256),
                Some(KeyAlgorithm::ES384) => Some(Algorithm::ES384),
                // An encryption or HMAC key is never used to verify an ID token.
                Some(_) => return None,
            };
            match DecodingKey::from_jwk(jwk) {
                Ok(key) => Some(KeyEntry {
                    kid: jwk.common.key_id.clone(),
                    alg,
                    key,
                }),
                Err(e) => {
                    tracing::debug!(error = %e, "skipping a JWK central cannot use");
                    None
                }
            }
        })
        .collect()
}

// ---- ID token checks ------------------------------------------------------------------------------------------------

/// The claims of an ID token central reads. `aud` and `email_verified` are kept raw: providers differ in their shape.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct IdClaims {
    #[serde(default)]
    iss: String,
    #[serde(default)]
    sub: String,
    #[serde(default)]
    aud: Value,
    #[serde(default)]
    exp: i64,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Value,
}

/// Who signed in, after every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Identity {
    pub email: String,
    /// The provider's id for the person, logged with each sign-in.
    pub sub: String,
}

/// Verifies the signature (and `iss`, `aud`, `exp` once more) with the key the token's header names.
fn verify_signature(
    id_token: &str,
    keys: &[KeyEntry],
    issuer: &str,
    client_id: &str,
) -> Result<IdClaims, String> {
    let header = jsonwebtoken::decode_header(id_token).map_err(|e| format!("ID token: {e}"))?;
    if !ALLOWED_ALGS.contains(&header.alg) {
        return Err(format!(
            "ID token signed with {:?}, which is not accepted",
            header.alg
        ));
    }
    let entry = match header.kid.as_deref() {
        Some(kid) => keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
        None if keys.len() == 1 => keys.first(),
        None => None,
    }
    .ok_or("the ID token's signing key is not in the provider's JWKS")?;
    if entry.alg.is_some_and(|a| a != header.alg) {
        return Err("the ID token's algorithm does not match its key".into());
    }
    let mut v = Validation::new(header.alg);
    v.leeway = LEEWAY_SECS as u64;
    // `issuer` is kept without a trailing `/`; some providers (Auth0) put one in `iss`. `check_claims` compares the same way.
    v.set_issuer(&[issuer.to_owned(), format!("{issuer}/")]);
    v.set_audience(&[client_id]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    jsonwebtoken::decode::<IdClaims>(id_token, &entry.key, &v)
        .map(|d| d.claims)
        .map_err(|e| format!("ID token rejected: {e}"))
}

fn eq_ct(a: &str, b: &str) -> bool {
    constant_time_eq(a, b)
}

/// The OpenID Connect claim checks, on claims whose signature is already verified.
pub(super) fn check_claims(
    c: &IdClaims,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    now: i64,
) -> Result<Identity, String> {
    if c.iss.trim_end_matches('/') != issuer {
        return Err(format!("the ID token is from '{}', not '{issuer}'", c.iss));
    }
    let auds: Vec<&str> = match &c.aud {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !auds.contains(&client_id) {
        return Err("the ID token is not meant for this client (aud)".into());
    }
    // With several audiences the authorized party must be this client; when present it must be anyway.
    if (auds.len() > 1 || c.azp.is_some()) && c.azp.as_deref() != Some(client_id) {
        return Err("the ID token was issued to another client (azp)".into());
    }
    if c.exp == 0 || c.exp + LEEWAY_SECS <= now {
        return Err("the ID token has expired".into());
    }
    if !c.nonce.as_deref().is_some_and(|n| eq_ct(n, nonce)) {
        return Err("the ID token's nonce does not match this sign-in".into());
    }
    if c.sub.is_empty() {
        return Err("the ID token has no subject".into());
    }
    let email = c
        .email
        .as_deref()
        .and_then(normalize_email)
        .ok_or("the provider sent no email address (is the 'email' scope allowed?)")?;
    let verified = match &c.email_verified {
        Value::Bool(b) => *b,
        Value::String(s) => s.eq_ignore_ascii_case("true"),
        _ => false,
    };
    if !verified {
        return Err(format!("the provider has not verified {email}"));
    }
    Ok(Identity {
        email,
        sub: c.sub.clone(),
    })
}

// ---- PKCE and randomness --------------------------------------------------------------------------------------------

/// Base64url without padding (RFC 4648 §5).
fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let take = chunk.len() + 1;
        for i in 0..take {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// The S256 code challenge of a PKCE verifier (RFC 7636).
pub(super) fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    b64url(&Sha256::digest(verifier.as_bytes()))
}

/// 64 random hex characters (two v4 UUIDs): a state, nonce or PKCE verifier (43-128 unreserved characters).
fn random_hex() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

// ---- cookies and CSRF -----------------------------------------------------------------------------------------------

/// The value of cookie `name` in the request's `Cookie` headers.
pub(super) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn cookie(cfg: &SsoConfig, name: &str, value: &str, path: &str, max_age: i64) -> String {
    format!(
        "{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        if cfg.secure_cookies() { "; Secure" } else { "" }
    )
}

/// Whether a change made with the session cookie comes from the console: it carries [`CONSOLE_HEADER`] (which a
/// cross-site form or `fetch` without CORS cannot add) and, if the browser names an `Origin`, it is this central's.
pub(super) fn console_request(headers: &HeaderMap, cfg: &SsoConfig) -> bool {
    headers.contains_key(CONSOLE_HEADER) && origin_ok(headers, cfg, false)
}

/// Whether the request's `Origin` is this central's (`required`: it must also be present).
fn origin_ok(headers: &HeaderMap, cfg: &SsoConfig, required: bool) -> bool {
    match headers.get(header::ORIGIN) {
        None => !required,
        Some(origin) => {
            let origin = origin.to_str().unwrap_or("");
            reqwest::Url::parse(&cfg.public_url)
                .map(|u| u.origin().ascii_serialization() == origin)
                .unwrap_or(false)
        }
    }
}

// ---- pages ----------------------------------------------------------------------------------------------------------

fn esc_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// A small page for the browser when a sign-in cannot finish, with a way on.
fn page(status: StatusCode, title: &str, text: &str, retry: bool) -> Response {
    let action = if retry {
        r#"<a class="btn" href="/auth/login">Try again</a> <a href="/console">Back to the console</a>"#
    } else {
        r#"<a class="btn" href="/console">Back to the console</a>"#
    };
    let html = format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{t} · FluxVM Cloud</title><style>
:root{{color-scheme:light dark;--bg:#f5f5f7;--fg:#1d1d1f;--muted:#6e6e73;--panel:#fff;--accent:#0071e3}}
@media (prefers-color-scheme:dark){{:root{{--bg:#000;--fg:#f5f5f7;--muted:#a1a1a6;--panel:#1c1c1e;--accent:#2997ff}}}}
body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:var(--bg);color:var(--fg);font:15px/1.5 -apple-system,BlinkMacSystemFont,"SF Pro Text","Helvetica Neue",sans-serif;padding:24px 16px;box-sizing:border-box}}
main{{background:var(--panel);border-radius:18px;padding:32px;max-width:440px;width:100%;box-shadow:0 2px 12px rgba(0,0,0,.08)}}
h1{{font-size:22px;margin:0 0 8px}}p{{color:var(--muted);margin:0 0 20px}}a{{color:var(--accent)}}
.btn{{display:inline-block;background:var(--accent);color:#fff;border-radius:980px;padding:8px 18px;text-decoration:none;margin-right:12px}}
</style></head><body><main><h1>{t}</h1><p>{x}</p>{action}</main></body></html>"#,
        t = esc_html(title),
        x = esc_html(text),
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        html,
    )
        .into_response()
}

fn sso_off() -> Response {
    page(
        StatusCode::NOT_FOUND,
        "Single sign-on is off",
        "This central has no identity provider configured. Sign in with your access token instead.",
        false,
    )
}

// ---- /auth routes ---------------------------------------------------------------------------------------------------

/// `GET /auth/config`: whether the console should offer SSO.
pub(super) async fn config(State(fleet): State<Fleet>) -> Json<Value> {
    match &fleet.sso.cfg {
        Some(cfg) => Json(json!({
            "enabled": true,
            "provider": cfg.provider_name,
            "login_url": "/auth/login",
        })),
        None => Json(json!({"enabled": false})),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct LoginQuery {
    /// Sign in to this tenant when the person belongs to several.
    #[serde(default)]
    tenant: Option<String>,
}

/// `GET /auth/login`: sends the browser to the identity provider.
pub(super) async fn login(State(fleet): State<Fleet>, req: Request) -> Response {
    let Some(cfg) = fleet.sso.cfg.as_ref() else {
        return sso_off();
    };
    let q = Query::<LoginQuery>::try_from_uri(req.uri())
        .map(|Query(q)| q)
        .unwrap_or_default();
    // Rate limit per client address (behind a proxy on this machine, the address it forwarded). Without a peer
    // address (an in-process test) there is nothing to count.
    if let Some(addr) = super::signup::client_addr(&req)
        && !fleet.sso.allow_login(addr, Instant::now())
    {
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many sign-in attempts",
            "Wait a minute and try again.",
            false,
        );
    }
    let provider = match fleet.sso.provider().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC discovery failed");
            return page(
                StatusCode::BAD_GATEWAY,
                "The sign-in service is not reachable",
                "Central could not reach your identity provider. Try again in a minute; if it keeps happening, tell your operator.",
                true,
            );
        }
    };
    let state = random_hex();
    let nonce = random_hex();
    let verifier = random_hex();
    let challenge = pkce_challenge(&verifier);
    let tenant = q.tenant.filter(|t| {
        !t.is_empty()
            && t.len() <= 63
            && t.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    let url = match reqwest::Url::parse_with_params(
        &provider.authorization_endpoint,
        &[
            ("response_type", "code"),
            ("client_id", cfg.client_id.as_str()),
            ("redirect_uri", cfg.redirect_uri().as_str()),
            ("scope", "openid email profile"),
            ("state", state.as_str()),
            ("nonce", nonce.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ],
    ) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, "bad authorization endpoint");
            return page(
                StatusCode::BAD_GATEWAY,
                "The sign-in service is misconfigured",
                "The identity provider's authorization endpoint is not a valid URL. Tell your operator.",
                false,
            );
        }
    };
    fleet.sso.remember(
        state.clone(),
        Pending {
            nonce,
            verifier,
            created: Utc::now(),
            tenant,
        },
    );
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, url.as_str())
        .header(
            header::SET_COOKIE,
            cookie(cfg, STATE_COOKIE, &state, "/auth", LOGIN_TTL_SECS),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Where a verified person may sign in: `Ok(None)` is the operator, `Ok(Some(tenant))` a tenant, `Err` a refusal.
fn access_for(
    t: &TenancyState,
    cfg: &SsoConfig,
    email: &str,
    hint: Option<&str>,
) -> Result<Option<String>, (StatusCode, &'static str, String)> {
    let operator = is_operator(t, cfg, email);
    let memberships = t.memberships(email);
    let hinted = hint.and_then(|h| memberships.iter().find(|(id, _)| id == h));
    let tenant = match (hinted, operator, memberships.first()) {
        (Some((id, _)), _, _) => Some(id.clone()),
        (None, true, _) => None,
        (None, false, Some((id, _))) => Some(id.clone()),
        (None, false, None) => {
            // Someone who signed up themselves (`POST /signup`) is made their tenant's owner by
            // [`TenancyState::adopt_signup_owner`] before this runs; anyone else needs an invitation.
            return Err((
                StatusCode::FORBIDDEN,
                "You don't have access yet",
                format!(
                    "{email} is not a member of any team on this cloud. Ask an admin of your team to add you on the Members page, then sign in again."
                ),
            ));
        }
    };
    if let Some(id) = &tenant
        && t.tenants.get(id).is_some_and(|x| x.disabled)
    {
        return Err((
            StatusCode::FORBIDDEN,
            "Your team's account is disabled",
            format!("The tenant '{id}' is disabled. Ask your operator to enable it."),
        ));
    }
    Ok(tenant)
}

/// `GET /auth/callback`: finishes a sign-in and starts a session.
pub(super) async fn callback(
    State(fleet): State<Fleet>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(cfg) = fleet.sso.cfg.as_ref() else {
        return sso_off();
    };
    if let Some(err) = q.error.as_deref() {
        let why: String = q
            .error_description
            .as_deref()
            .unwrap_or(err)
            .chars()
            .take(200)
            .collect();
        return page(
            StatusCode::UNAUTHORIZED,
            "Sign-in was not completed",
            &format!("Your identity provider said: {why}"),
            true,
        );
    }
    let (Some(code), Some(state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This is not a sign-in link",
            "The link is missing its code. Start the sign-in from the console.",
            true,
        );
    };
    let Some(pending) = fleet.sso.take(state) else {
        return page(
            StatusCode::BAD_REQUEST,
            "This sign-in has expired",
            "It took longer than ten minutes, or the link was already used. Start again.",
            true,
        );
    };
    if !cookie_value(&headers, STATE_COOKIE).is_some_and(|c| eq_ct(&c, state)) {
        return page(
            StatusCode::BAD_REQUEST,
            "Sign-in started in another browser",
            "Finish the sign-in in the browser where you started it.",
            true,
        );
    }
    let provider = match fleet.sso.provider().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC discovery failed");
            return page(
                StatusCode::BAD_GATEWAY,
                "The sign-in service is not reachable",
                "Central could not reach your identity provider. Try again in a minute.",
                true,
            );
        }
    };
    let identity = match fleet
        .sso
        .exchange(cfg, &provider, code, &pending.verifier)
        .await
    {
        Ok(id_token) => {
            fleet
                .sso
                .verify(cfg, &provider, &id_token, &pending.nonce)
                .await
        }
        Err(e) => Err(e),
    };
    let identity = match identity {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, "SSO sign-in failed");
            return page(
                StatusCode::UNAUTHORIZED,
                "Sign-in could not be verified",
                &format!("{e}. Try again; if it keeps happening, tell your operator."),
                true,
            );
        }
    };
    let mut t = fleet.tenancy.lock().await;
    // The email a tenant signed up with (and nobody's member yet) becomes that tenant's owner.
    if !is_operator(&t, cfg, &identity.email)
        && let Some(id) = t.adopt_signup_owner(&identity.email, Utc::now())
    {
        tracing::info!(email = %identity.email, tenant = %id, "SSO sign-in of a sign-up owner: made them the owner");
    }
    let tenant = match access_for(&t, cfg, &identity.email, pending.tenant.as_deref()) {
        Ok(x) => x,
        Err((status, title, text)) => {
            drop(t);
            tracing::info!(email = %identity.email, "SSO sign-in refused: no access");
            return page(status, title, &text, false);
        }
    };
    let token = t.create_session(
        &identity.email,
        tenant.as_deref(),
        cfg.session_ttl,
        Utc::now(),
    );
    fleet.persist_tenancy(&t);
    drop(t);
    let caller = match &tenant {
        Some(id) => Caller::Tenant(id.clone()),
        None => Caller::Admin,
    };
    let mut ev = audit::event(
        &caller,
        "POST".into(),
        "/auth/session".into(),
        None,
        StatusCode::CREATED,
        None,
    );
    ev.user = Some(identity.email.clone());
    fleet.audit.record(ev);
    tracing::info!(email = %identity.email, sub = %identity.sub, tenant = ?tenant, "SSO sign-in");
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/console")
        .header(
            header::SET_COOKIE,
            cookie(
                cfg,
                SESSION_COOKIE,
                &token,
                "/",
                cfg.session_ttl.num_seconds(),
            ),
        )
        .header(
            header::SET_COOKIE,
            cookie(cfg, STATE_COOKIE, "", "/auth", 0),
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// `POST /auth/logout`: ends the session and clears the cookie.
pub(super) async fn logout(State(fleet): State<Fleet>, headers: HeaderMap) -> Response {
    let Some(cfg) = fleet.sso.cfg.as_ref() else {
        return StatusCode::NO_CONTENT.into_response();
    };
    if !console_request(&headers, cfg) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "sign out from the console"})),
        )
            .into_response();
    }
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        let mut t = fleet.tenancy.lock().await;
        if t.end_session(&token) {
            fleet.persist_tenancy(&t);
        }
    }
    (
        StatusCode::NO_CONTENT,
        [(header::SET_COOKIE, cookie(cfg, SESSION_COOKIE, "", "/", 0))],
    )
        .into_response()
}

/// `GET /auth/session`: who the session cookie belongs to, for the console.
pub(super) async fn session(State(fleet): State<Fleet>, headers: HeaderMap) -> Response {
    let not_signed_in = || {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "not signed in with SSO"})),
        )
            .into_response()
    };
    let Some(cfg) = fleet.sso.cfg.as_ref() else {
        return not_signed_in();
    };
    let Some(token) = cookie_value(&headers, SESSION_COOKIE) else {
        return not_signed_in();
    };
    let t = fleet.tenancy.lock().await;
    let Some(s) = t.session(&token, Utc::now()).cloned() else {
        return not_signed_in();
    };
    let Ok(user) = resolve(&t, cfg, &s) else {
        return not_signed_in();
    };
    let tenants: Vec<Value> = t
        .memberships(&s.email)
        .into_iter()
        .map(|(id, role)| {
            json!({
                "id": id,
                "display_name": t.tenants.get(&id).and_then(|x| x.display_name.clone()),
                "role": role,
            })
        })
        .collect();
    Json(json!({
        "email": user.email,
        "operator": user.tenant.is_none(),
        "tenant": user.tenant,
        "role": user.role,
        "expires_at": s.expires_at,
        "tenants": tenants,
        "can_be_operator": is_operator(&t, cfg, &s.email),
    }))
    .into_response()
}

// ---- the session cookie in auth_middleware --------------------------------------------------------------------------

/// The person behind a request made with the session cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionUser {
    pub email: String,
    /// `None`: the operator.
    pub tenant: Option<String>,
    /// The member's role (`None` for the operator).
    pub role: Option<MemberRole>,
}

/// Put next to the [`Caller`] by `auth_middleware` on every request: the session user, if the cookie was used.
#[derive(Debug, Clone, Default)]
pub(super) struct Who(pub Option<SessionUser>);

pub(super) enum FromCookie {
    /// No session cookie (or SSO is off): bearer-token rules apply.
    Absent,
    User(Caller, SessionUser),
    Reject(Response),
}

fn is_operator(t: &TenancyState, cfg: &SsoConfig, email: &str) -> bool {
    cfg.operators.iter().any(|o| o == email)
        || t.policy
            .operator_emails
            .iter()
            .any(|o| o.trim().eq_ignore_ascii_case(email))
}

/// The session's person as of now: still an operator, or still a member of an enabled tenant.
fn resolve(
    t: &TenancyState,
    cfg: &SsoConfig,
    s: &crate::tenancy::Session,
) -> Result<SessionUser, (StatusCode, String)> {
    match &s.tenant {
        None if is_operator(t, cfg, &s.email) => Ok(SessionUser {
            email: s.email.clone(),
            tenant: None,
            role: None,
        }),
        None => Err((
            StatusCode::UNAUTHORIZED,
            format!("{} is no longer an operator; sign in again", s.email),
        )),
        Some(id) => {
            let Some(tenant) = t.tenants.get(id) else {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    format!("the tenant '{id}' no longer exists; sign in again"),
                ));
            };
            let Some(m) = t.member(id, &s.email) else {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    format!("{} is no longer a member of '{id}'; sign in again", s.email),
                ));
            };
            if tenant.disabled {
                return Err((StatusCode::FORBIDDEN, format!("tenant '{id}' is disabled")));
            }
            Ok(SessionUser {
                email: s.email.clone(),
                tenant: Some(id.clone()),
                role: Some(m.role),
            })
        }
    }
}

/// Resolves the session cookie of a request that carries no bearer token.
/// (Takes the method and headers, not the request: its body is not `Sync`, so a borrow of it cannot be held across
/// the tenancy lock's `await`.)
pub(super) async fn caller_from_cookie(
    fleet: &Fleet,
    method: &Method,
    headers: &HeaderMap,
) -> FromCookie {
    let Some(cfg) = fleet.sso.cfg.as_ref() else {
        return FromCookie::Absent;
    };
    let Some(token) = cookie_value(headers, SESSION_COOKIE) else {
        return FromCookie::Absent;
    };
    let reject = |status: StatusCode, msg: String| {
        FromCookie::Reject((status, Json(json!({ "error": msg }))).into_response())
    };
    if !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
        && !console_request(headers, cfg)
    {
        return reject(
            StatusCode::FORBIDDEN,
            "cross-site request refused: changes made with the console's session must come from the console".into(),
        );
    }
    // A WebSocket (a tunnel) is a GET that cannot carry the console header, and browsers do not apply CORS to it: a
    // page on another origin could otherwise open one with this cookie. Browsers always send `Origin` on it.
    if headers.contains_key(header::UPGRADE) && !origin_ok(headers, cfg, true) {
        return reject(
            StatusCode::FORBIDDEN,
            "cross-site WebSocket refused: open it from the console, or use a token".into(),
        );
    }
    let t = fleet.tenancy.lock().await;
    let Some(s) = t.session(&token, Utc::now()) else {
        return reject(
            StatusCode::UNAUTHORIZED,
            "your session has ended; sign in again".into(),
        );
    };
    match resolve(&t, cfg, s) {
        Ok(user) => {
            let caller = match &user.tenant {
                Some(id) => Caller::Tenant(id.clone()),
                None => Caller::Admin,
            };
            FromCookie::User(caller, user)
        }
        Err((status, msg)) => reject(status, msg),
    }
}

/// Why `role` may not make this call (`None`: it may). Reads are never refused here; `tenant_route_allowed` already
/// limits what a tenant can reach at all.
pub(super) fn role_refusal(role: MemberRole, method: &Method, path: &str) -> Option<String> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    // A tunnel is opened with a GET (a WebSocket upgrade), but it reaches into the VM: it counts as a change.
    let tunnel = matches!(seg.as_slice(), ["fleet", "vms", _, _, "tunnel"]);
    // A live screen and a console log can hold secrets (a typed password, an environment dump), so a viewer, who
    // otherwise sees everything, does not get them.
    let live = matches!(seg.as_slice(), ["fleet", "vms", _, _, "screen" | "logs"]);
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) && !tunnel {
        if live && role == MemberRole::Viewer {
            return Some(
                "viewers cannot see live screens or console logs, which can hold secrets; ask an admin of your team for the member role"
                    .into(),
            );
        }
        return None;
    }
    match role {
        MemberRole::Owner | MemberRole::Admin => None,
        MemberRole::Viewer => Some(
            "viewers can look but not change anything; ask an admin of your team for the member role".into(),
        ),
        MemberRole::Member => {
            // VMs (power, screen input, commands, snapshots, clones, tunnels), volumes and networks. Warm pools,
            // closing tunnels and payment methods need an admin.
            let allowed = matches!(
                seg.as_slice(),
                ["fleet", "vms" | "overlays" | "volumes" | "snapshots", ..]
            );
            (!allowed).then(|| {
                "members manage VMs, volumes, snapshots and networks; leases, warm pools, API keys, members, the budget, billing and webhooks need an admin".into()
            })
        }
    }
}

// ---- members --------------------------------------------------------------------------------------------------------

fn member_error(e: MemberError) -> AppError {
    AppError(
        StatusCode::from_u16(e.status()).unwrap_or(StatusCode::BAD_REQUEST),
        e.to_string(),
    )
}

/// The tenant a members route is about: `me` is the caller's own; another id needs the operator.
fn members_tenant(caller: &Caller, id: &str) -> Result<String, AppError> {
    match (caller, id) {
        (Caller::Tenant(t), "me") => Ok(t.clone()),
        (Caller::Admin, "me") => Err(AppError(
            StatusCode::BAD_REQUEST,
            "the operator is not a tenant; use /fleet/tenants/{id}/members".into(),
        )),
        (Caller::Tenant(t), _) => Err(AppError(
            StatusCode::FORBIDDEN,
            format!("tenant '{t}' may not call this route"),
        )),
        (Caller::Admin, id) => Ok(id.to_owned()),
    }
}

/// The member role of whoever changes members: `None` acts with full rights (the operator, or the tenant's primary
/// token); a named API key may not change members at all (a leaked key must not invite its holder).
fn changer_role(who: &Who, key: &KeyUsed) -> Result<Option<MemberRole>, AppError> {
    if key.0.is_some() {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "members are managed in the console or with the tenant's primary token, not with an API key".into(),
        ));
    }
    Ok(who.0.as_ref().and_then(|u| u.role))
}

/// Whether someone with role `actor` (`None`: full rights) may turn a member with role `current` (`None`: a new
/// member) into one with role `new` (`None`: removed).
pub(super) fn may_change(
    actor: Option<MemberRole>,
    current: Option<MemberRole>,
    new: Option<MemberRole>,
) -> Result<(), String> {
    match actor {
        None | Some(MemberRole::Owner) => Ok(()),
        Some(MemberRole::Admin) => {
            if current == Some(MemberRole::Owner) {
                Err("only an owner can change or remove an owner".into())
            } else if new == Some(MemberRole::Owner) {
                Err("only an owner can make someone an owner".into())
            } else {
                Ok(())
            }
        }
        Some(_) => Err("only admins and owners manage members".into()),
    }
}

fn who_label(who: &Who, caller: &Caller) -> String {
    match (&who.0, caller) {
        (Some(u), _) => u.email.clone(),
        (None, Caller::Admin) => "operator".into(),
        (None, Caller::Tenant(_)) => "token".into(),
    }
}

fn parse_role(raw: &str) -> Result<MemberRole, AppError> {
    MemberRole::parse(raw).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("role must be owner, admin, member or viewer, not '{raw}'"),
        )
    })
}

/// `GET /fleet/tenants/{id|me}/members`.
pub(super) async fn list_members(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let tenant = members_tenant(&caller, &id)?;
    let t = fleet.tenancy.lock().await;
    let rec = t
        .tenants
        .get(&tenant)
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("no tenant '{tenant}'")))?;
    let mut items: Vec<&crate::tenancy::Member> = rec.members.iter().collect();
    items.sort_by(|a, b| b.role.cmp(&a.role).then_with(|| a.email.cmp(&b.email)));
    Ok(Json(json!({
        "items": items.iter().map(|m| m.public_json()).collect::<Vec<_>>(),
        "sso": fleet.sso.enabled(),
    })))
}

#[derive(Debug, Deserialize)]
pub(super) struct AddMemberBody {
    email: String,
    #[serde(default)]
    role: Option<String>,
}

/// `POST /fleet/tenants/{id|me}/members`: invites a person by email (role `member` unless given).
pub(super) async fn add_member(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Extension(key): Extension<KeyUsed>,
    Extension(who): Extension<Who>,
    Path(id): Path<String>,
    Json(b): Json<AddMemberBody>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let tenant = members_tenant(&caller, &id)?;
    let actor = changer_role(&who, &key)?;
    let role = match b.role.as_deref() {
        Some(r) => parse_role(r)?,
        None => MemberRole::Member,
    };
    may_change(actor, None, Some(role)).map_err(|m| AppError(StatusCode::FORBIDDEN, m))?;
    let mut t = fleet.tenancy.lock().await;
    let m = t
        .add_member(
            &tenant,
            &b.email,
            role,
            Some(who_label(&who, &caller)),
            Utc::now(),
        )
        .map_err(member_error)?;
    fleet.persist_tenancy(&t);
    tracing::info!(tenant = %tenant, role = role.as_str(), "member added");
    Ok((
        StatusCode::CREATED,
        Json(json!({ "member": m.public_json() })),
    ))
}

#[derive(Debug, Deserialize)]
pub(super) struct RoleBody {
    role: String,
}

/// `PATCH /fleet/tenants/{id|me}/members/{email}` `{"role": "admin"}`.
pub(super) async fn set_member_role(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Extension(key): Extension<KeyUsed>,
    Extension(who): Extension<Who>,
    Path((id, email)): Path<(String, String)>,
    Json(b): Json<RoleBody>,
) -> Result<Json<Value>, AppError> {
    let tenant = members_tenant(&caller, &id)?;
    let actor = changer_role(&who, &key)?;
    let role = parse_role(&b.role)?;
    let mut t = fleet.tenancy.lock().await;
    let current = t
        .member(&tenant, &email)
        .map(|m| m.role)
        .ok_or_else(|| member_error(MemberError::UnknownMember(email.to_ascii_lowercase())))?;
    may_change(actor, Some(current), Some(role)).map_err(|m| AppError(StatusCode::FORBIDDEN, m))?;
    let (m, _) = t
        .set_member_role(&tenant, &email, role)
        .map_err(member_error)?;
    fleet.persist_tenancy(&t);
    Ok(Json(json!({ "member": m.public_json() })))
}

/// `DELETE /fleet/tenants/{id|me}/members/{email}`: removes the member and ends their sessions.
pub(super) async fn remove_member(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Extension(key): Extension<KeyUsed>,
    Extension(who): Extension<Who>,
    Path((id, email)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let tenant = members_tenant(&caller, &id)?;
    let actor = changer_role(&who, &key)?;
    let mut t = fleet.tenancy.lock().await;
    let current = t
        .member(&tenant, &email)
        .map(|m| m.role)
        .ok_or_else(|| member_error(MemberError::UnknownMember(email.to_ascii_lowercase())))?;
    may_change(actor, Some(current), None).map_err(|m| AppError(StatusCode::FORBIDDEN, m))?;
    let m = t.remove_member(&tenant, &email).map_err(member_error)?;
    fleet.persist_tenancy(&t);
    tracing::info!(tenant = %tenant, "member removed");
    Ok(Json(json!({ "removed": m.public_json() })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use tower::ServiceExt;

    // A throwaway RSA key (PKCS#1) and P-256 key (PKCS#8) made for these tests, with their public halves as JWKs.
    const RSA_PEM: &str = include_str!("testdata/sso_rsa.pem");
    const RSA_N: &str = "sb5kTm_5NNgKiX0gaYrWv25X6xjgiwlbQxpptNJtLeptyYRv6_sZBYakmtWvoz3rADR8diRGRUGwgplNNAvg1vS95rO8pjFQoqLQJHVBmicNbEfN9c9MRUBeTqCdjq-484LsS5pEo2XRn2Vt3wvdoW_EL6qC-SrMlSjmCopQrxo5_JXsvq4SODXMu2llyhlePFbByvzFM0aR8VSBcXllaoy4gq1jDp04eBcM9BKYgITypnoA26RE0OF33Td2U7OI45YnKg_G0SOhWK_2BTDQ_ApW6yEb7VlBe1HU2olpzsYrt691ufATMrrJkKEEbacqnrwIOHsHvrOAYNjQVOe9ow";
    const EC_PEM: &str = include_str!("testdata/sso_ec.pem");
    const EC_X: &str = "ip6dQTyjHYg-TAqPYqOYxQ4Vwf9STpCeCz-qualoOdw";
    const EC_Y: &str = "miysbo92GOXQm2exq0vboa9bkQL7oE9hAAFgvuJqXMg";
    const ISS: &str = "https://idp.example.com";
    const CLIENT: &str = "fluxvm-console";

    fn jwks() -> Value {
        json!({"keys": [
            {"kty": "RSA", "kid": "rsa-1", "alg": "RS256", "use": "sig", "n": RSA_N, "e": "AQAB"},
            {"kty": "EC", "kid": "ec-1", "alg": "ES256", "use": "sig", "crv": "P-256", "x": EC_X, "y": EC_Y},
        ]})
    }

    fn keys() -> Vec<KeyEntry> {
        key_entries(serde_json::from_value(jwks()).unwrap())
    }

    fn claims(nonce: &str) -> Value {
        json!({
            "iss": ISS, "sub": "00u1", "aud": CLIENT, "exp": Utc::now().timestamp() + 300,
            "iat": Utc::now().timestamp(), "nonce": nonce, "email": "Alice@Example.com", "email_verified": true,
            "name": "Alice",
        })
    }

    fn sign_rs(c: &Value, kid: &str) -> String {
        let mut h = Header::new(Algorithm::RS256);
        h.kid = Some(kid.into());
        jsonwebtoken::encode(
            &h,
            c,
            &EncodingKey::from_rsa_pem(RSA_PEM.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn sign_es(c: &Value) -> String {
        let mut h = Header::new(Algorithm::ES256);
        h.kid = Some("ec-1".into());
        jsonwebtoken::encode(&h, c, &EncodingKey::from_ec_pem(EC_PEM.as_bytes()).unwrap()).unwrap()
    }

    fn parsed(c: Value) -> IdClaims {
        serde_json::from_value(c).unwrap()
    }

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
        let v = random_hex();
        assert_eq!(v.len(), 64);
        assert_ne!(v, random_hex());
    }

    #[test]
    fn rs256_and_es256_tokens_verify_and_tampering_does_not() {
        let keys = keys();
        assert_eq!(keys.len(), 2);
        let rs = sign_rs(&claims("n1"), "rsa-1");
        let c = verify_signature(&rs, &keys, ISS, CLIENT).unwrap();
        assert_eq!(c.email.as_deref(), Some("Alice@Example.com"));
        let es = sign_es(&claims("n1"));
        assert!(verify_signature(&es, &keys, ISS, CLIENT).is_ok());
        // A changed payload breaks the signature.
        let mut parts: Vec<String> = rs.split('.').map(str::to_owned).collect();
        let mut forged = claims("n1");
        forged["email"] = json!("mallory@example.com");
        parts[1] = b64url(forged.to_string().as_bytes());
        assert!(verify_signature(&parts.join("."), &keys, ISS, CLIENT).is_err());
        // An unknown key id, another audience, another issuer, an expired token.
        assert!(verify_signature(&sign_rs(&claims("n1"), "rsa-9"), &keys, ISS, CLIENT).is_err());
        assert!(verify_signature(&rs, &keys, ISS, "other-client").is_err());
        assert!(verify_signature(&rs, &keys, "https://evil.example.com", CLIENT).is_err());
        let mut old = claims("n1");
        old["exp"] = json!(Utc::now().timestamp() - 3600);
        assert!(verify_signature(&sign_rs(&old, "rsa-1"), &keys, ISS, CLIENT).is_err());
        // An RSA-signed token naming the EC key is refused (algorithm/key mismatch).
        assert!(verify_signature(&sign_rs(&claims("n1"), "ec-1"), &keys, ISS, CLIENT).is_err());
    }

    #[test]
    fn hmac_and_none_tokens_are_refused() {
        let hs = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims("n1"),
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();
        assert!(verify_signature(&hs, &keys(), ISS, CLIENT).is_err());
        // alg "none", hand-made.
        let none = format!(
            "{}.{}.",
            b64url(br#"{"alg":"none","typ":"JWT"}"#),
            b64url(claims("n1").to_string().as_bytes())
        );
        assert!(verify_signature(&none, &keys(), ISS, CLIENT).is_err());
    }

    #[test]
    fn claim_checks() {
        let now = Utc::now().timestamp();
        let ok = check_claims(&parsed(claims("n1")), ISS, CLIENT, "n1", now).unwrap();
        assert_eq!(ok.email, "alice@example.com");
        assert_eq!(ok.sub, "00u1");
        let bad = |f: &dyn Fn(&mut Value)| {
            let mut c = claims("n1");
            f(&mut c);
            check_claims(&parsed(c), ISS, CLIENT, "n1", now).unwrap_err()
        };
        assert!(bad(&|c| c["nonce"] = json!("other")).contains("nonce"));
        assert!(
            bad(&|c| {
                c.as_object_mut().unwrap().remove("nonce");
            })
            .contains("nonce")
        );
        assert!(bad(&|c| c["email_verified"] = json!(false)).contains("verified"));
        assert!(
            bad(&|c| {
                c.as_object_mut().unwrap().remove("email_verified");
            })
            .contains("verified")
        );
        assert!(
            bad(&|c| {
                c.as_object_mut().unwrap().remove("email");
            })
            .contains("email")
        );
        assert!(bad(&|c| c["iss"] = json!("https://other.example.com")).contains("from"));
        assert!(bad(&|c| c["aud"] = json!("other")).contains("aud"));
        assert!(bad(&|c| c["exp"] = json!(now - 120)).contains("expired"));
        // Several audiences need azp naming this client.
        assert!(bad(&|c| c["aud"] = json!([CLIENT, "other"])).contains("azp"));
        let mut multi = claims("n1");
        multi["aud"] = json!([CLIENT, "other"]);
        multi["azp"] = json!(CLIENT);
        assert!(check_claims(&parsed(multi), ISS, CLIENT, "n1", now).is_ok());
        // "true" as a string is accepted (some providers send it that way).
        let mut s = claims("n1");
        s["email_verified"] = json!("true");
        assert!(check_claims(&parsed(s), ISS, CLIENT, "n1", now).is_ok());
        // Within the clock-skew leeway.
        let mut skew = claims("n1");
        skew["exp"] = json!(now - 30);
        assert!(check_claims(&parsed(skew), ISS, CLIENT, "n1", now).is_ok());
    }

    #[test]
    fn role_matrix() {
        use MemberRole::*;
        let get = Method::GET;
        let post = Method::POST;
        let delete = Method::DELETE;
        let put = Method::PUT;
        let patch = Method::PATCH;
        // (method, path, viewer, member, admin, owner)
        let cases: &[(&Method, &str, [bool; 4])] = &[
            (&get, "/fleet/vms", [true, true, true, true]),
            (&get, "/fleet/tenants/me/members", [true, true, true, true]),
            (&post, "/fleet/vms", [false, true, true, true]),
            (&delete, "/fleet/vms/mac-1/abc", [false, true, true, true]),
            (
                &post,
                "/fleet/vms/mac-1/abc/exec",
                [false, true, true, true],
            ),
            (&post, "/fleet/overlays", [false, true, true, true]),
            (&post, "/fleet/volumes", [false, true, true, true]),
            (&post, "/fleet/snapshots", [false, true, true, true]),
            (&post, "/fleet/leases", [false, false, true, true]),
            (
                &post,
                "/fleet/leases/l1/release",
                [false, false, true, true],
            ),
            (&post, "/fleet/tenants/me/keys", [false, false, true, true]),
            (
                &post,
                "/fleet/tenants/me/members",
                [false, false, true, true],
            ),
            (&put, "/fleet/tenants/me/budget", [false, false, true, true]),
            (
                &patch,
                "/fleet/tenants/me/members/a@example.com",
                [false, false, true, true],
            ),
            // The other features' routes.
            (
                &post,
                "/fleet/vms/mac-1/abc/snapshots",
                [false, true, true, true],
            ),
            (
                &post,
                "/fleet/vms/mac-1/abc/clone",
                [false, true, true, true],
            ),
            (
                &post,
                "/fleet/vms/mac-1/abc/stop",
                [false, true, true, true],
            ),
            (
                &post,
                "/fleet/vms/mac-1/abc/input",
                [false, true, true, true],
            ),
            (
                &get,
                "/fleet/vms/mac-1/abc/tunnel",
                [false, true, true, true],
            ),
            (
                &post,
                "/fleet/vms/mac-1/abc/ws-ticket",
                [false, true, true, true],
            ),
            (&delete, "/fleet/tunnels/t1", [false, false, true, true]),
            (&get, "/fleet/tunnels", [true, true, true, true]),
            (
                &post,
                "/fleet/volumes/data/resize",
                [false, true, true, true],
            ),
            (&post, "/fleet/pools", [false, false, true, true]),
            (&patch, "/fleet/pools/xcode", [false, false, true, true]),
            (&delete, "/fleet/pools/xcode", [false, false, true, true]),
            (&get, "/fleet/metrics/summary", [true, true, true, true]),
            (
                &get,
                "/fleet/metrics/vms/mac-1/abc",
                [true, true, true, true],
            ),
            (
                &post,
                "/fleet/billing/payment-method",
                [false, false, true, true],
            ),
            (&get, "/fleet/billing/invoices", [true, true, true, true]),
            // Live screens and console logs can hold secrets: not for viewers.
            (
                &get,
                "/fleet/vms/mac-1/abc/screen",
                [false, true, true, true],
            ),
            (&get, "/fleet/vms/mac-1/abc/logs", [false, true, true, true]),
            (&get, "/fleet/vms", [true, true, true, true]),
        ];
        for &(method, path, want) in cases {
            for (role, ok) in [Viewer, Member, Admin, Owner].into_iter().zip(want) {
                assert_eq!(
                    role_refusal(role, method, path).is_none(),
                    ok,
                    "{role:?} {method} {path}"
                );
            }
        }
        assert!(role_refusal(Member, &post, "/fleet/webhooks").is_some());
        assert!(role_refusal(Member, &delete, "/fleet/webhooks/whk_1").is_some());
    }

    #[test]
    fn sign_in_starts_are_limited_per_address_per_minute() {
        let sso = Sso::new(None);
        let a: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let b: std::net::IpAddr = "203.0.113.8".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..LOGINS_PER_ADDRESS_PER_MINUTE {
            assert!(sso.allow_login(a, t0));
        }
        assert!(
            !sso.allow_login(a, t0),
            "the next one from the same address is refused"
        );
        assert!(sso.allow_login(b, t0), "another address is unaffected");
        // The window slides: a minute later the first address may start again.
        assert!(sso.allow_login(a, t0 + StdDuration::from_secs(61)));
    }

    #[test]
    fn who_may_change_members() {
        use MemberRole::*;
        // Full rights: the operator and the primary token.
        assert!(may_change(None, Some(Owner), None).is_ok());
        assert!(may_change(Some(Owner), Some(Owner), Some(Viewer)).is_ok());
        assert!(may_change(Some(Owner), None, Some(Owner)).is_ok());
        assert!(may_change(Some(Admin), None, Some(Admin)).is_ok());
        assert!(may_change(Some(Admin), Some(Member), None).is_ok());
        assert!(may_change(Some(Admin), Some(Owner), None).is_err());
        assert!(may_change(Some(Admin), Some(Owner), Some(Admin)).is_err());
        assert!(may_change(Some(Admin), Some(Member), Some(Owner)).is_err());
        assert!(may_change(Some(Member), None, Some(Viewer)).is_err());
        assert!(may_change(Some(Viewer), Some(Viewer), None).is_err());
    }

    #[test]
    fn cookies_and_console_requests() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "a=1; fluxvm_session=fvs_abc ; b=2".parse().unwrap(),
        );
        assert_eq!(cookie_value(&h, SESSION_COOKIE).as_deref(), Some("fvs_abc"));
        assert_eq!(cookie_value(&h, "c"), None);
        let cfg = test_config("https://idp.example.com");
        assert!(cookie(&cfg, SESSION_COOKIE, "x", "/", 60).contains("; Secure"));
        assert!(cookie(&cfg, SESSION_COOKIE, "x", "/", 60).contains("HttpOnly"));
        assert!(!console_request(&h, &cfg), "no console header");
        h.insert(CONSOLE_HEADER, "1".parse().unwrap());
        assert!(console_request(&h, &cfg));
        h.insert(header::ORIGIN, "https://cloud.example.com".parse().unwrap());
        assert!(console_request(&h, &cfg));
        h.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        assert!(!console_request(&h, &cfg));
    }

    fn test_config(issuer: &str) -> SsoConfig {
        SsoConfig {
            issuer: issuer.into(),
            client_id: CLIENT.into(),
            client_secret: Some("s3cret".into()),
            public_url: "https://cloud.example.com".into(),
            provider_name: "Okta".into(),
            operators: vec!["root@example.com".into()],
            session_ttl: chrono::Duration::hours(12),
        }
    }

    #[test]
    fn flags_are_validated() {
        let args = |issuer: Option<&str>, public: Option<&str>| SsoArgs {
            oidc_issuer: issuer.map(str::to_owned),
            oidc_client_id: Some(CLIENT.into()),
            oidc_client_secret: None,
            oidc_client_secret_file: None,
            public_url: public.map(str::to_owned),
            oidc_provider_name: "Okta".into(),
            sso_operators: vec!["Root@Example.com".into()],
            session_hours: 12,
        };
        let cfg = args(
            Some("https://idp.example.com/"),
            Some("https://cloud.example.com/"),
        )
        .into_config()
        .unwrap()
        .unwrap();
        assert_eq!(cfg.issuer, "https://idp.example.com");
        assert_eq!(
            cfg.redirect_uri(),
            "https://cloud.example.com/auth/callback"
        );
        assert_eq!(cfg.operators, vec!["root@example.com".to_owned()]);
        assert!(
            args(
                Some("http://idp.example.com"),
                Some("https://c.example.com")
            )
            .into_config()
            .is_err()
        );
        assert!(
            args(Some("http://127.0.0.1:9000"), Some("http://localhost:7799"))
                .into_config()
                .is_ok()
        );
        assert!(
            args(Some("https://idp.example.com"), None)
                .into_config()
                .is_err()
        );
        assert!(
            args(None, None).into_config().is_err(),
            "a client id without an issuer"
        );
        let mut off = args(None, None);
        off.oidc_client_id = None;
        off.sso_operators.clear();
        assert!(off.into_config().unwrap().is_none());
    }

    // ---- the whole flow against a fake identity provider ----

    #[derive(Default)]
    struct Idp {
        /// code -> (email, nonce, PKCE challenge)
        codes: std::sync::Mutex<HashMap<String, (String, String, String)>>,
        issuer: std::sync::Mutex<String>,
    }

    async fn spawn_idp() -> (String, Arc<Idp>) {
        use axum::extract::Form;
        let idp = Arc::new(Idp::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        *idp.issuer.lock().unwrap() = base.clone();
        let st = idp.clone();
        let disco_base = base.clone();
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let b = disco_base.clone();
                    async move {
                        Json(json!({
                            "issuer": b, "authorization_endpoint": format!("{b}/authorize"),
                            "token_endpoint": format!("{b}/token"), "jwks_uri": format!("{b}/jwks"),
                            "token_endpoint_auth_methods_supported": ["client_secret_post"],
                        }))
                    }
                }),
            )
            .route("/jwks", get(|| async { Json(jwks()) }))
            .route(
                "/token",
                post(move |Form(f): Form<HashMap<String, String>>| {
                    let st = st.clone();
                    async move {
                        let code = f.get("code").cloned().unwrap_or_default();
                        let Some((email, nonce, challenge)) = st.codes.lock().unwrap().remove(&code) else {
                            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
                        };
                        let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                        if pkce_challenge(&verifier) != challenge
                            || f.get("client_secret").map(String::as_str) != Some("s3cret")
                        {
                            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": "PKCE or client secret"})));
                        }
                        let iss = st.issuer.lock().unwrap().clone();
                        let mut c = claims(&nonce);
                        c["iss"] = json!(iss);
                        c["email"] = json!(email);
                        (StatusCode::OK, Json(json!({"access_token": "at", "token_type": "Bearer", "id_token": sign_rs(&c, "rsa-1")})))
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, idp)
    }

    async fn send(app: &Router, req: axum::http::Request<axum::body::Body>) -> Response {
        app.clone().oneshot(req).await.unwrap()
    }

    fn request(method: &str, uri: &str) -> axum::http::request::Builder {
        axum::http::Request::builder().method(method).uri(uri)
    }

    fn set_cookie(resp: &Response, name: &str) -> Option<String> {
        resp.headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| v.starts_with(&format!("{name}=")))
            .map(|v| {
                v[name.len() + 1..]
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .to_owned()
            })
    }

    /// Runs a whole sign-in for `email` and returns the callback's response.
    async fn sign_in(app: &Router, idp: &Idp, email: &str, tenant_hint: Option<&str>) -> Response {
        let uri = match tenant_hint {
            Some(t) => format!("/auth/login?tenant={t}"),
            None => "/auth/login".into(),
        };
        let resp = send(
            app,
            request("GET", &uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let loc = reqwest::Url::parse(resp.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let q: HashMap<String, String> = loc.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["redirect_uri"], "http://localhost:7799/auth/callback");
        let state_cookie = set_cookie(&resp, STATE_COOKIE).unwrap();
        assert_eq!(state_cookie, q["state"]);
        let code = format!("code-{}", random_hex());
        idp.codes.lock().unwrap().insert(
            code.clone(),
            (
                email.into(),
                q["nonce"].clone(),
                q["code_challenge"].clone(),
            ),
        );
        send(
            app,
            request(
                "GET",
                &format!("/auth/callback?code={code}&state={}", q["state"]),
            )
            .header(header::COOKIE, format!("{STATE_COOKIE}={state_cookie}"))
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
    }

    async fn json_of(resp: Response) -> Value {
        let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&b).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn sign_in_roles_csrf_and_sign_out_end_to_end() {
        let (issuer, idp) = spawn_idp().await;
        let dir = tempfile::tempdir().unwrap();
        let sso = SsoArgs {
            oidc_issuer: Some(issuer.clone()),
            oidc_client_id: Some(CLIENT.into()),
            oidc_client_secret: Some("s3cret".into()),
            oidc_client_secret_file: None,
            public_url: Some("http://localhost:7799".into()),
            oidc_provider_name: "Okta".into(),
            sso_operators: vec!["root@example.com".into()],
            session_hours: 12,
        }
        .into_config()
        .unwrap();
        let (app, _) = router_with_sso(
            CentralConfig {
                state_dir: dir.path().to_path_buf(),
                token: Some("fleet-token".into()),
                reconcile_interval: None,
                ha: None,
            },
            sso,
        );
        let op = |m: &str, uri: &str, body: Option<Value>| {
            let b = request(m, uri)
                .header(header::AUTHORIZATION, "Bearer fleet-token")
                .header(header::CONTENT_TYPE, "application/json");
            b.body(axum::body::Body::from(
                body.map(|v| v.to_string()).unwrap_or_default(),
            ))
            .unwrap()
        };
        let r = send(
            &app,
            request("GET", "/auth/config")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(json_of(r).await["provider"], "Okta");
        assert_eq!(
            send(
                &app,
                op("POST", "/fleet/tenants", Some(json!({"id": "acme"})))
            )
            .await
            .status(),
            StatusCode::CREATED
        );
        let r = send(
            &app,
            op(
                "POST",
                "/fleet/tenants/acme/members",
                Some(json!({"email": "Alice@Example.com", "role": "member"})),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let r = send(&app, op("GET", "/fleet/tenants/acme/members", None)).await;
        assert_eq!(json_of(r).await["items"][0]["email"], "alice@example.com");

        // Alice signs in and lands on the console with a session cookie.
        let r = sign_in(&app, &idp, "alice@example.com", None).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers()[header::LOCATION], "/console");
        let session = set_cookie(&r, SESSION_COOKIE).unwrap();
        assert!(session.starts_with("fvs_"));
        let with_cookie = |m: &str, uri: &str, console: bool| {
            let mut b = request(m, uri)
                .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
                .header(header::CONTENT_TYPE, "application/json");
            if console {
                b = b.header(CONSOLE_HEADER, "1");
            }
            b.body(axum::body::Body::from("{}")).unwrap()
        };
        let me = json_of(send(&app, with_cookie("GET", "/auth/session", false)).await).await;
        assert_eq!(me["email"], "alice@example.com");
        assert_eq!(me["tenant"], "acme");
        assert_eq!(me["role"], "member");
        let r = send(&app, with_cookie("GET", "/fleet/tenants/me", false)).await;
        assert_eq!(r.status(), StatusCode::OK);
        // A member may not lease, and a change without the console header is refused as cross-site.
        let r = send(&app, with_cookie("POST", "/fleet/leases", true)).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(
            json_of(r).await["error"]
                .as_str()
                .unwrap()
                .contains("admin")
        );
        let r = send(
            &app,
            with_cookie("POST", "/fleet/tenants/me/members", false),
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(
            json_of(r).await["error"]
                .as_str()
                .unwrap()
                .contains("cross-site")
        );
        // Operator-only routes stay closed to a tenant session.
        assert_eq!(
            send(&app, with_cookie("GET", "/fleet/nodes", false))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        // A WebSocket from another origin (no CORS applies to it) is refused before it reaches a tunnel.
        let ws = |origin: Option<&str>| {
            let mut b = request("GET", "/fleet/vms/mac-1/abc/tunnel")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
                .header(header::UPGRADE, "websocket");
            if let Some(o) = origin {
                b = b.header(header::ORIGIN, o);
            }
            b.body(axum::body::Body::empty()).unwrap()
        };
        for origin in [Some("https://evil.example"), None] {
            let r = send(&app, ws(origin)).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
            assert!(
                json_of(r).await["error"]
                    .as_str()
                    .unwrap()
                    .contains("cross-site WebSocket")
            );
        }
        // Promoted to admin, the same session can now manage members at once; the change is in the log.
        let r = send(
            &app,
            op(
                "PATCH",
                "/fleet/tenants/acme/members/alice@example.com",
                Some(json!({"role": "admin"})),
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let add = request("POST", "/fleet/tenants/me/members")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(CONSOLE_HEADER, "1")
            .body(axum::body::Body::from(
                r#"{"email":"bob@example.com","role":"viewer"}"#,
            ))
            .unwrap();
        assert_eq!(send(&app, add).await.status(), StatusCode::CREATED);
        let owner = request("POST", "/fleet/tenants/me/members")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={session}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(CONSOLE_HEADER, "1")
            .body(axum::body::Body::from(
                r#"{"email":"carol@example.com","role":"owner"}"#,
            ))
            .unwrap();
        assert_eq!(
            send(&app, owner).await.status(),
            StatusCode::FORBIDDEN,
            "an admin cannot make owners"
        );
        let events = json_of(send(&app, op("GET", "/fleet/events", None)).await).await;
        assert!(
            events["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["user"] == "alice@example.com"
                    && e["path"] == "/fleet/tenants/me/members"
                    && e["status"] == 201),
            "{events}"
        );

        // Someone nobody invited is refused with a page, and a used sign-in cannot be replayed.
        let r = sign_in(&app, &idp, "mallory@example.com", None).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(set_cookie(&r, SESSION_COOKIE).is_none());
        let r = send(
            &app,
            request("GET", "/auth/callback?code=x&state=nope")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // An operator email signs in as the operator.
        let r = sign_in(&app, &idp, "root@example.com", None).await;
        let op_session = set_cookie(&r, SESSION_COOKIE).unwrap();
        let r = send(
            &app,
            request("GET", "/fleet/nodes")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={op_session}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        // Removing Alice ends her session at once; signing out ends the operator's.
        let r = send(
            &app,
            op(
                "DELETE",
                "/fleet/tenants/acme/members/alice@example.com",
                None,
            ),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            send(&app, with_cookie("GET", "/fleet/tenants/me", false))
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let out = request("POST", "/auth/logout")
            .header(header::COOKIE, format!("{SESSION_COOKIE}={op_session}"))
            .header(CONSOLE_HEADER, "1")
            .body(axum::body::Body::empty())
            .unwrap();
        let r = send(&app, out).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert_eq!(set_cookie(&r, SESSION_COOKIE).as_deref(), Some(""));
        let r = send(
            &app,
            request("GET", "/fleet/nodes")
                .header(header::COOKIE, format!("{SESSION_COOKIE}={op_session}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // Sessions are persisted hashed: no token in the state file.
        let raw = fs::read_to_string(dir.path().join("fleet-tenancy.json")).unwrap();
        assert!(!raw.contains(&session) && !raw.contains(&op_session));
    }

    #[tokio::test]
    async fn without_sso_the_auth_routes_say_so_and_cookies_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _) = router_with_sso(
            CentralConfig {
                state_dir: dir.path().to_path_buf(),
                token: Some("fleet-token".into()),
                reconcile_interval: None,
                ha: None,
            },
            None,
        );
        let r = send(
            &app,
            request("GET", "/auth/config")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(json_of(r).await["enabled"], false);
        let r = send(
            &app,
            request("GET", "/auth/login")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = send(
            &app,
            request("GET", "/fleet/nodes")
                .header(header::COOKIE, format!("{SESSION_COOKIE}=fvs_whatever"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
}
