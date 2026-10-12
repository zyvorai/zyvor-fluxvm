// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Self-service sign-up and Stripe payments for `fluxvm-agent central`.
//!
//! | Route | Who | What |
//! |---|---|---|
//! | `GET /signup/config` | anyone | whether sign-up is open, and what it asks for |
//! | `POST /signup` | anyone | `{tenant_id, display_name, email}`: with email confirmation (the default) mails a link and answers `202`; without it, creates the tenant and answers its token, once |
//! | `GET/POST /signup/verify` | anyone with the mailed token | the confirmation page and the step that creates the tenant; the token is in the answer, once |
//! | `POST /stripe/webhook` | Stripe (signed) | invoice paid / payment failed, payment method saved |
//! | `GET /fleet/billing/invoices` | tenant, operator | invoices with their status and pay links |
//! | `POST /fleet/billing/invoices?month=YYYY-MM` | operator | turn each billed tenant's statement into a Stripe invoice |
//! | `POST /fleet/billing/payment-method` | tenant | a Stripe Checkout URL (setup mode) to save a card |
//! | `PUT /fleet/tenants/{id}/billing` | operator | turn billing on (creates the Stripe customer) or off |
//!
//! Sign-up is off until the operator turns `signup_enabled` on in the policy, and needs central to run with `--token`
//! (tenants need it). It is rate limited per client address and overall. Every Stripe call goes through
//! [`crate::stripe::Client`] to the configured base URL, with an idempotency key per tenant, month and step, and the
//! invoice records in the tenancy state make a second run for the same month a no-op.

use super::*;
use crate::stripe::{self as st, InvoiceRecord, StripeConfig, kv};
use axum::extract::{ConnectInfo, Query};
use axum::http::{HeaderMap, Method};
use chrono::Datelike;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration as StdDuration, Instant};

/// Routes that need no token: the auth middleware lets these through untouched.
pub(super) const PUBLIC_PATHS: [&str; 4] = [
    "/signup",
    "/signup/config",
    "/signup/verify",
    "/stripe/webhook",
];

/// Sign-ups one client address may make in an hour.
const SIGNUPS_PER_ADDRESS_PER_HOUR: usize = 5;
/// Sign-ups the whole central takes in an hour, whatever the address.
const SIGNUPS_PER_HOUR: usize = 100;
/// Client addresses the rate limiter remembers at once.
const MAX_TRACKED_ADDRESSES: usize = 10_000;
const MAX_SIGNUP_BODY: usize = 16 * 1024;
const MAX_WEBHOOK_BODY: usize = 512 * 1024;
/// Webhook event ids remembered, to drop Stripe's redeliveries.
const SEEN_EVENTS: usize = 2_000;
/// Days a tenant has to pay an invoice sent without a saved payment method.
const DAYS_UNTIL_DUE: u32 = 30;
/// Tenant ids nobody may sign up as.
const RESERVED_IDS: &[&str] = &[
    "admin",
    "administrator",
    "operator",
    "root",
    "system",
    "fleet",
    "billing",
    "support",
    "stripe",
    "signup",
    "console",
    "api",
    "www",
    "help",
    "security",
];

// ---- configuration ------------------------------------------------------------------------------------------------

static BOOT: std::sync::OnceLock<StripeConfig> = std::sync::OnceLock::new();

/// Sets the Stripe configuration central starts with (`fluxvm-agent central --stripe-secret-key-file …`). Call it
/// before [`super::router`]; without it, payments are off and sign-up cannot require a payment method.
pub fn configure_stripe(cfg: StripeConfig) {
    let _ = BOOT.set(cfg);
}

pub(super) fn boot_config() -> Option<StripeConfig> {
    BOOT.get().cloned()
}

/// Where sign-up confirmation mail goes: the operator's mail webhook. Central does not speak SMTP; it POSTs JSON
/// (`{type, to, subject, text, link, ...}`, signed with `secret` as `X-FluxVM-Signature` when set) to this URL, and
/// whatever receives it sends the mail. The URL `log` writes the mail to central's log instead (labs).
#[derive(Debug, Clone)]
pub struct MailConfig {
    pub url: String,
    pub secret: Option<String>,
    /// This central's URL as browsers reach it: the base of the confirmation link.
    pub public_url: String,
}

static BOOT_MAIL: std::sync::OnceLock<MailConfig> = std::sync::OnceLock::new();

/// Sets the sign-up mail webhook central starts with (`--signup-mail-webhook`). Call it before [`super::router`].
pub fn configure_mail(cfg: MailConfig) {
    let _ = BOOT_MAIL.set(cfg);
}

pub(super) fn boot_mail() -> Option<MailConfig> {
    BOOT_MAIL.get().cloned()
}

/// Sign-up and Stripe state that lives only in memory.
#[derive(Default)]
pub(super) struct Runtime {
    stripe: Option<StripeConfig>,
    mail: Option<MailConfig>,
    limiter: std::sync::Mutex<Limiter>,
    seen_events: std::sync::Mutex<VecDeque<String>>,
    /// One invoice run at a time (the operator's and the monthly job's).
    run_lock: tokio::sync::Mutex<()>,
}

impl Runtime {
    pub(super) fn new(stripe: Option<StripeConfig>) -> Self {
        Runtime {
            stripe,
            ..Default::default()
        }
    }

    pub(super) fn with_mail(mut self, mail: Option<MailConfig>) -> Self {
        self.mail = mail;
        self
    }
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn err(status: StatusCode, msg: impl Into<String>) -> AppError {
    AppError(status, msg.into())
}

fn admin_only(caller: &Caller) -> Result<(), AppError> {
    match caller {
        Caller::Admin => Ok(()),
        Caller::Tenant(t) => Err(err(
            StatusCode::FORBIDDEN,
            format!("tenant '{t}' may not call this route"),
        )),
    }
}

fn stripe_cfg(fleet: &Fleet) -> Result<&StripeConfig, AppError> {
    fleet.signup.stripe.as_ref().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "payments are not set up: start central with --stripe-secret-key-file (see docs/mac-cloud-billing.md)",
        )
    })
}

fn client<'a>(fleet: &'a Fleet, cfg: &'a StripeConfig) -> st::Client<'a> {
    st::Client {
        http: &fleet.http,
        cfg,
    }
}

fn stripe_failed(e: st::StripeError) -> AppError {
    tracing::warn!(error = %e, "Stripe call failed");
    err(StatusCode::BAD_GATEWAY, e.to_string())
}

// ---- rate limits --------------------------------------------------------------------------------------------------

#[derive(Default)]
struct Limiter {
    by_address: HashMap<IpAddr, VecDeque<Instant>>,
    all: VecDeque<Instant>,
}

fn prune(q: &mut VecDeque<Instant>, now: Instant) {
    let hour = StdDuration::from_secs(3600);
    while q
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= hour)
    {
        q.pop_front();
    }
}

impl Limiter {
    /// Counts one sign-up from `addr` (unknown when central cannot tell), or says why not.
    fn allow(&mut self, addr: Option<IpAddr>, now: Instant) -> Result<(), &'static str> {
        prune(&mut self.all, now);
        if self.all.len() >= SIGNUPS_PER_HOUR {
            return Err("too many sign-ups right now; try again in an hour");
        }
        if let Some(ip) = addr {
            if !self.by_address.contains_key(&ip) && self.by_address.len() >= MAX_TRACKED_ADDRESSES
            {
                self.by_address.retain(|_, q| {
                    prune(q, now);
                    !q.is_empty()
                });
                if self.by_address.len() >= MAX_TRACKED_ADDRESSES {
                    return Err("too many sign-ups right now; try again in an hour");
                }
            }
            let q = self.by_address.entry(ip).or_default();
            prune(q, now);
            if q.len() >= SIGNUPS_PER_ADDRESS_PER_HOUR {
                return Err("too many sign-ups from your network; try again in an hour");
            }
            q.push_back(now);
        }
        self.all.push_back(now);
        Ok(())
    }
}

/// The address a request came from: the peer, or, behind a reverse proxy on this machine, the address it appended
/// to `X-Forwarded-For`. IPv6 addresses count per /64, which is what one customer usually gets.
pub(super) fn client_addr(req: &Request) -> Option<IpAddr> {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())?;
    Some(client_ip(peer, req.headers()))
}

/// [`client_addr`] from the peer address and the request headers (for handlers that extract those).
fn client_ip(peer: IpAddr, headers: &axum::http::HeaderMap) -> IpAddr {
    let ip = if peer.is_loopback() {
        headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse::<IpAddr>().ok())
            .unwrap_or(peer)
    } else {
        peer
    };
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

// ---- validation ---------------------------------------------------------------------------------------------------

/// Checks an email's shape and lowercases its domain. Not a deliverability check: the payment provider and SSO
/// verify the address.
pub(super) fn normalize_email(raw: &str) -> Result<String, &'static str> {
    const BAD: &str = "enter an email address like you@example.com";
    let e = raw.trim();
    if e.len() > 254 {
        return Err("that email address is too long");
    }
    let (local, domain) = e.rsplit_once('@').ok_or(BAD)?;
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+/=?^_`{|}~.-".contains(&b));
    let domain = domain.to_ascii_lowercase();
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && labels
            .last()
            .is_some_and(|t| t.len() >= 2 && t.bytes().all(|b| b.is_ascii_alphabetic()));
    if !local_ok || !domain_ok {
        return Err(BAD);
    }
    Ok(format!("{local}@{domain}"))
}

/// Whether `email`'s domain is one of `allowed` (exactly, ignoring case); an empty list allows any.
fn domain_allowed(email: &str, allowed: &[String]) -> bool {
    let domain = email.rsplit_once('@').map_or("", |(_, d)| d);
    allowed.is_empty()
        || allowed.iter().any(|a| {
            a.trim()
                .trim_start_matches('@')
                .eq_ignore_ascii_case(domain)
        })
}

fn clean_display_name(raw: Option<String>) -> Result<Option<String>, &'static str> {
    let Some(s) = raw else { return Ok(None) };
    let s = s.trim();
    if s.chars().count() > 80 {
        return Err("the name is at most 80 characters");
    }
    if s.chars().any(char::is_control) {
        return Err("the name cannot contain control characters");
    }
    Ok((!s.is_empty()).then(|| s.to_owned()))
}

/// Whether sign-up is open: the operator turned it on, tenants are possible (a fleet token), and a required payment
/// method can be collected.
fn signup_open(fleet: &Fleet, p: &crate::tenancy::Policy) -> bool {
    p.signup_enabled
        && fleet.token.is_some()
        && (!p.signup_require_payment_method || fleet.signup.stripe.is_some())
        // Confirming the address needs a way to send the link.
        && (!p.signup_require_email_verification || fleet.signup.mail.is_some())
}

// ---- routes -------------------------------------------------------------------------------------------------------

pub(super) fn routes() -> Router<Fleet> {
    Router::new()
        .route("/signup", post(signup))
        .route("/signup/config", get(signup_config))
        .route("/signup/verify", get(verify_page).post(verify_signup))
        .route("/stripe/webhook", post(stripe_webhook))
        .route(
            "/fleet/billing/invoices",
            get(list_invoices).post(run_invoices),
        )
        .route("/fleet/billing/payment-method", post(payment_method))
        .route(
            "/fleet/tenants/{id}/billing",
            axum::routing::put(put_tenant_billing),
        )
}

/// Refuses billable creates (`POST /fleet/vms`, `POST /fleet/leases`, and a clone, a volume or its resize, a warm pool or
/// a change to one, which are billed too) for a tenant that signed up when a payment method was required and has not
/// added one yet. Everything else, including adding the payment method, still works.
pub(super) async fn payment_gate(
    fleet: &Fleet,
    tenant: &str,
    method: &Method,
    path: &str,
) -> Option<Response> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let billable = match seg.as_slice() {
        ["fleet", "vms"]
        | ["fleet", "leases"]
        | ["fleet", "vms", _, _, "clone"]
        // Bringing a stopped VM back, or rolling one back, bills again as much as creating one.
        | ["fleet", "vms", _, _, "start" | "restart" | "resume"]
        | ["fleet", "vms", _, _, "snapshots", _, "restore"]
        | ["fleet", "volumes"]
        | ["fleet", "volumes", _, "resize"]
        | ["fleet", "pools"] => *method == Method::POST,
        // Growing a warm pool boots (and bills) more VMs.
        ["fleet", "pools", _] => *method == Method::PATCH,
        _ => false,
    };
    if !billable {
        return None;
    }
    let pending = fleet
        .tenancy
        .lock()
        .await
        .tenants
        .get(tenant)
        .is_some_and(|t| t.payment_method_pending);
    pending.then(|| {
        (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({"error": "add a payment method first: Billing, Add payment method in the console, or `fluxctl fleet payment-method`"})),
        )
            .into_response()
    })
}

/// `GET /signup/config`: what the sign-in page needs to offer sign-up.
async fn signup_config(State(fleet): State<Fleet>) -> Json<Value> {
    let t = fleet.tenancy.lock().await;
    let p = &t.policy;
    Json(json!({
        "open": signup_open(&fleet, p),
        "require_payment_method": p.signup_require_payment_method,
        "email_confirmation": p.signup_require_email_verification,
        "payments": fleet.signup.stripe.is_some(),
        "allowed_email_domains": p.signup_allowed_email_domains,
    }))
}

#[derive(Debug, Deserialize)]
struct SignupBody {
    tenant_id: String,
    #[serde(default)]
    display_name: Option<String>,
    email: String,
}

/// `POST /signup`: creates a tenant with the policy's default quota and answers with its token, once.
async fn signup(
    State(fleet): State<Fleet>,
    req: Request,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let addr = client_addr(&req);
    let bytes = axum::body::to_bytes(req.into_body(), MAX_SIGNUP_BODY)
        .await
        .map_err(|_| err(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"))?;
    let b: SignupBody = serde_json::from_slice(&bytes).map_err(|_| {
        err(
            StatusCode::BAD_REQUEST,
            "send JSON with tenant_id, display_name and email",
        )
    })?;
    let policy = fleet.tenancy.lock().await.policy.clone();
    if !signup_open(&fleet, &policy) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "sign-up is closed here; ask the operator for an account",
        ));
    }
    let id = b.tenant_id.trim().to_owned();
    let email = normalize_email(&b.email).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let display_name =
        clean_display_name(b.display_name).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    if !domain_allowed(&email, &policy.signup_allowed_email_domains) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "sign-up here is for email addresses at your organization's domain",
        ));
    }
    if RESERVED_IDS.contains(&id.as_str()) {
        return Err(err(
            StatusCode::CONFLICT,
            format!("'{id}' is reserved; pick another account ID"),
        ));
    }
    lock(&fleet.signup.limiter)
        .allow(addr, Instant::now())
        .map_err(|e| err(StatusCode::TOO_MANY_REQUESTS, e))?;
    if policy.signup_require_email_verification {
        return start_verification(&fleet, id, email, display_name).await;
    }
    // Email confirmation is turned off (a lab): the account exists at once, with an address nobody confirmed.
    let (tenant, token) =
        create_signed_up_tenant(&fleet, &policy, &id, &email, display_name, false).await?;
    finish_signup(&fleet, tenant, token).await
}

/// Creates the tenant of a sign-up with the policy's default quota and returns it with its token. `verified` says the
/// address was confirmed by a mailed link.
async fn create_signed_up_tenant(
    fleet: &Fleet,
    policy: &crate::tenancy::Policy,
    id: &str,
    email: &str,
    display_name: Option<String>,
    verified: bool,
) -> Result<(crate::tenancy::Tenant, String), AppError> {
    let mut t = fleet.tenancy.lock().await;
    if t.tenants.values().any(|x| {
        x.owner_email
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case(email))
    }) {
        return Err(err(
            StatusCode::CONFLICT,
            "that email already has an account; sign in with its token, or ask the operator",
        ));
    }
    let (_, token) = t
        .create_tenant(
            id,
            display_name,
            policy.signup_default_quota.clone(),
            Utc::now(),
        )
        .map_err(tenancy_error)?;
    let rec = t
        .tenants
        .get_mut(id)
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "tenant vanished"))?;
    rec.owner_email = Some(email.to_owned());
    rec.owner_email_verified = verified;
    rec.self_service = true;
    rec.billing_enabled = fleet.signup.stripe.is_some();
    rec.payment_method_pending = policy.signup_require_payment_method;
    // An address that was not confirmed is not made a member now (anyone could sign up with someone else's address and
    // appear in their SSO sign-in as a team they own). The first SSO sign-in with that verified email makes them the
    // owner instead ([`TenancyState::adopt_signup_owner`]), and the sign-up token is rotated then.
    let tenant = t
        .tenants
        .get(id)
        .cloned()
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "tenant vanished"))?;
    fleet.persist_tenancy(&t);
    Ok((tenant, token))
}

/// The audit entry, the Stripe customer and the answer of a sign-up that created its tenant.
async fn finish_signup(
    fleet: &Fleet,
    tenant: crate::tenancy::Tenant,
    token: String,
) -> Result<(StatusCode, Json<Value>), AppError> {
    tracing::info!(tenant = %tenant.id, verified = tenant.owner_email_verified, "tenant signed up");
    fleet.audit.record(audit::event(
        &Caller::Tenant(tenant.id.clone()),
        "POST".into(),
        "/signup".into(),
        Some(tenant.id.clone()),
        StatusCode::CREATED,
        None,
    ));
    // The customer is made now so the first invoice has somewhere to go; if Stripe is down it is made later, on the
    // first invoice or payment method.
    let mut public = tenant.public_json();
    if fleet.signup.stripe.is_some() {
        match ensure_customer(fleet, &tenant.id).await {
            Ok(c) => public["stripe_customer_id"] = json!(c),
            Err(e) => {
                tracing::warn!(tenant = %tenant.id, error = %e.1, "creating the Stripe customer at sign-up")
            }
        }
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "tenant": public,
            "token": token,
            "payment_method_required": tenant.payment_method_pending,
            "note": "store this token now; central keeps only its hash and cannot show it again",
        })),
    ))
}

// ---- email confirmation -------------------------------------------------------------------------------------------

/// Records a pending sign-up and mails its link. The answer is the same whether or not the address already has an
/// account, so the form cannot be used to find out who is registered; a tenant ID that is taken is said plainly (an
/// ID is not a secret and the person needs to pick another).
async fn start_verification(
    fleet: &Fleet,
    id: String,
    email: String,
    display_name: Option<String>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let Some(mail) = fleet.signup.mail.clone() else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "sign-up needs email confirmation, but this central has no mail webhook configured",
        ));
    };
    enum Next {
        Link(String),
        Exists,
        Nothing,
    }
    let now = Utc::now();
    let next = {
        let mut t = fleet.tenancy.lock().await;
        t.prune_pending_signups(now);
        let has_account = t.tenants.values().any(|x| {
            x.owner_email
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case(&email))
        });
        if has_account {
            Next::Exists
        } else if t.tenants.contains_key(&id) {
            return Err(err(
                StatusCode::CONFLICT,
                format!("'{id}' is taken; pick another account ID"),
            ));
        } else {
            match t.add_pending_signup(&id, &email, display_name, now) {
                Some(token) => {
                    fleet.persist_tenancy(&t);
                    Next::Link(token)
                }
                None => Next::Nothing,
            }
        }
    };
    // Mailed in the background, so how long this answer takes does not say which case it was.
    let f = fleet.clone();
    tokio::spawn(async move {
        match next {
            Next::Link(token) => {
                let link = format!(
                    "{}/signup/verify?token={token}",
                    mail.public_url.trim_end_matches('/')
                );
                send_mail(
                    &f,
                    &mail,
                    json!({
                        "type": "signup.verify",
                        "to": email,
                        "tenant_id": id,
                        "link": link,
                        "expires_in_hours": crate::tenancy::SIGNUP_LINK_HOURS,
                        "subject": "Confirm your FluxVM Cloud account",
                        "text": format!(
                            "Someone asked for the FluxVM Cloud account '{id}' with this address.\n\nConfirm it, and get \
                             its access token, within {} hours:\n{link}\n\nIf that was not you, ignore this message; \
                             nothing was created.",
                            crate::tenancy::SIGNUP_LINK_HOURS
                        ),
                    }),
                )
                .await;
            }
            Next::Exists => {
                send_mail(
                    &f,
                    &mail,
                    json!({
                        "type": "signup.exists",
                        "to": email,
                        "console": format!("{}/console", mail.public_url.trim_end_matches('/')),
                        "subject": "You already have a FluxVM Cloud account",
                        "text": "Someone tried to sign up with this address, which already has a FluxVM Cloud \
                                 account. Sign in with its access token or your identity provider; if it was not you, \
                                 ignore this message.",
                    }),
                )
                .await;
            }
            Next::Nothing => {}
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "email_confirmation": true,
            "message": "If that address can be used, a confirmation link has been sent. It is valid for 24 hours; the \
                        account is created, and its token shown, when you confirm.",
        })),
    ))
}

/// Delivers one mail through the operator's webhook (or logs it, in `log` mode). Failures are logged, never shown:
/// the person is told the same thing either way.
async fn send_mail(fleet: &Fleet, mail: &MailConfig, payload: Value) {
    if mail.url == "log" {
        // Written straight to stderr: central's default log filter shows errors only, and this mode exists so a person
        // can copy the link out of the terminal.
        eprintln!(
            "sign-up mail (log mode): to={} type={} link={}",
            payload["to"].as_str().unwrap_or(""),
            payload["type"].as_str().unwrap_or(""),
            payload["link"].as_str().unwrap_or("")
        );
        return;
    }
    let Ok(body) = serde_json::to_vec(&payload) else {
        return;
    };
    let mut req = fleet
        .http
        .post(&mail.url)
        .header("content-type", "application/json")
        .header("user-agent", "fluxvm-central")
        .timeout(StdDuration::from_secs(10));
    if let Some(secret) = &mail.secret {
        req = req.header(
            "x-fluxvm-signature",
            super::notify::signature(secret, &body),
        );
    }
    match req.body(body).send().await {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            tracing::warn!(status = %r.status(), "the sign-up mail webhook refused the message")
        }
        Err(e) => tracing::warn!(error = %e, "the sign-up mail webhook could not be reached"),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// A small standalone page (no scripts, no external loads) for the confirmation steps.
fn confirm_page(status: StatusCode, title: &str, body_html: &str) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
         content=\"width=device-width,initial-scale=1\"><title>{t}</title><style>\
         :root{{color-scheme:light dark}}body{{font:16px/1.5 -apple-system,\"SF Pro Text\",system-ui,sans-serif;\
         margin:0;display:grid;place-items:center;min-height:100vh;background:Canvas;color:CanvasText}}\
         main{{max-width:34rem;padding:2rem}}h1{{font-size:1.5rem;margin:0 0 1rem}}\
         code{{display:block;padding:.75rem;border-radius:8px;background:rgba(127,127,127,.15);overflow-wrap:anywhere;\
         font:13px/1.4 ui-monospace,\"SF Mono\",Menlo,monospace}}button{{font:inherit;padding:.6rem 1.2rem;\
         border-radius:999px;border:0;background:#0a84ff;color:#fff;cursor:pointer}}\
         .note{{opacity:.7;font-size:.9rem}}</style></head><body><main><h1>{t}</h1>{b}</main></body></html>",
        t = esc(title),
        b = body_html
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::REFERRER_POLICY, "no-referrer"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
            ),
        ],
        html,
    )
        .into_response()
}

#[derive(Debug, Default, Deserialize)]
struct VerifyQuery {
    #[serde(default)]
    token: String,
}

/// `GET /signup/verify?token=…`: the page the mailed link opens. It only *shows* the sign-up; the account is created
/// by the `POST` its button sends, so a mail scanner that fetches every link cannot use one up.
async fn verify_page(State(fleet): State<Fleet>, Query(q): Query<VerifyQuery>) -> Response {
    let now = Utc::now();
    let pending = {
        let t = fleet.tenancy.lock().await;
        t.pending_signup(&q.token, now).cloned()
    };
    let Some(p) = pending else {
        return confirm_page(
            StatusCode::BAD_REQUEST,
            "This link is not valid",
            "<p>The confirmation link has expired or was already used. Start the sign-up again to get a new one.</p>",
        );
    };
    confirm_page(
        StatusCode::OK,
        "Confirm your account",
        &format!(
            "<p>Create the FluxVM Cloud account <strong>{id}</strong> for <strong>{email}</strong>?</p>\
             <form method=\"post\" action=\"/signup/verify\"><input type=\"hidden\" name=\"token\" value=\"{tok}\">\
             <button type=\"submit\">Confirm and create the account</button></form>\
             <p class=\"note\">The access token is shown once, on the next page.</p>",
            id = esc(&p.tenant_id),
            email = esc(&p.email),
            tok = esc(q.token.trim()),
        ),
    )
}

/// `POST /signup/verify` (`{"token": "…"}` as JSON, or the form of the confirmation page): consumes the link, creates
/// the tenant with its address marked confirmed, and answers with its token, once.
async fn verify_signup(State(fleet): State<Fleet>, req: Request) -> Response {
    let json_client = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let fail = |status: StatusCode, msg: &str| -> Response {
        if json_client {
            (status, Json(json!({"error": msg}))).into_response()
        } else {
            confirm_page(status, "This did not work", &format!("<p>{}</p>", esc(msg)))
        }
    };
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), MAX_SIGNUP_BODY).await else {
        return fail(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    let token = if json_client {
        serde_json::from_slice::<VerifyQuery>(&bytes)
            .map(|b| b.token)
            .unwrap_or_default()
    } else {
        reqwest::Url::parse(&format!("http://x/?{}", String::from_utf8_lossy(&bytes)))
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "token")
                    .map(|(_, v)| v.into_owned())
            })
            .unwrap_or_default()
    };
    let policy = fleet.tenancy.lock().await.policy.clone();
    let taken = {
        let mut t = fleet.tenancy.lock().await;
        t.take_pending_signup(&token, Utc::now())
    };
    let Some(p) = taken else {
        return fail(
            StatusCode::BAD_REQUEST,
            "this confirmation link has expired or was already used; sign up again",
        );
    };
    match create_signed_up_tenant(
        &fleet,
        &policy,
        &p.tenant_id,
        &p.email,
        p.display_name.clone(),
        true,
    )
    .await
    {
        Ok((tenant, token)) => match finish_signup(&fleet, tenant.clone(), token.clone()).await {
            Ok((status, Json(body))) if json_client => (status, Json(body)).into_response(),
            Ok(_) => confirm_page(
                StatusCode::CREATED,
                "Your account is ready",
                &format!(
                    "<p>Account <strong>{id}</strong> exists. This is its access token, shown only once:</p>\
                     <code>{tok}</code>\
                     <p>Store it now (a password manager, or your CI secrets), then use it with <code>fluxctl</code> \
                     (<code>FLUXVM_AGENT_TOKEN</code>) or sign in to the <a href=\"/console\">console</a>.</p>\
                     {pay}",
                    id = esc(&tenant.id),
                    tok = esc(&token),
                    pay = if tenant.payment_method_pending {
                        "<p>Add a payment method before you lease a Mac or launch a VM.</p>"
                    } else {
                        ""
                    },
                ),
            ),
            Err(e) => fail(e.0, &e.1),
        },
        Err(e) => fail(e.0, &e.1),
    }
}

/// The tenant's Stripe customer, created on first use.
async fn ensure_customer(fleet: &Fleet, id: &str) -> Result<String, AppError> {
    let cfg = stripe_cfg(fleet)?;
    let (existing, name, email, created) = {
        let t = fleet.tenancy.lock().await;
        let rec = t
            .tenants
            .get(id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no tenant '{id}'")))?;
        (
            rec.stripe_customer_id.clone(),
            rec.display_name.clone(),
            rec.owner_email.clone(),
            rec.created_at,
        )
    };
    if let Some(c) = existing {
        return Ok(c);
    }
    let mut form = vec![
        kv("name", name.unwrap_or_else(|| id.to_owned())),
        kv("description", format!("FluxVM tenant {id}")),
        kv("metadata[fluxvm_tenant]", id),
    ];
    if let Some(e) = email {
        form.push(kv("email", e));
    }
    let v = client(fleet, cfg)
        .post(
            "/customers",
            &form,
            Some(st::customer_idempotency_key(id, created).as_str()),
        )
        .await
        .map_err(stripe_failed)?;
    let customer = st::object_id(&v).ok_or_else(|| {
        err(
            StatusCode::BAD_GATEWAY,
            "Stripe answered without a customer id",
        )
    })?;
    let mut t = fleet.tenancy.lock().await;
    if let Some(rec) = t.tenants.get_mut(id)
        && rec.created_at == created
    {
        // Two calls racing both got the same customer (same idempotency key); the first one stored wins.
        let stored = rec.stripe_customer_id.get_or_insert(customer).clone();
        fleet.persist_tenancy(&t);
        return Ok(stored);
    }
    Err(err(StatusCode::NOT_FOUND, format!("no tenant '{id}'")))
}

// ---- operator: billing per tenant ---------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TenantBillingBody {
    /// Send this tenant monthly invoices through Stripe.
    #[serde(default)]
    enabled: Option<bool>,
    /// `false` lets a tenant that signed up without a payment method lease and launch anyway.
    #[serde(default)]
    payment_method_required: Option<bool>,
}

/// `PUT /fleet/tenants/{id}/billing` (operator).
async fn put_tenant_billing(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Json(b): Json<TenantBillingBody>,
) -> Result<Json<Value>, AppError> {
    admin_only(&caller)?;
    if !fleet.tenancy.lock().await.tenants.contains_key(&id) {
        return Err(err(StatusCode::NOT_FOUND, format!("no tenant '{id}'")));
    }
    if b.enabled == Some(true) {
        ensure_customer(&fleet, &id).await?;
    }
    let mut t = fleet.tenancy.lock().await;
    let rec = t
        .tenants
        .get_mut(&id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no tenant '{id}'")))?;
    if let Some(on) = b.enabled {
        rec.billing_enabled = on;
    }
    if let Some(req) = b.payment_method_required {
        rec.payment_method_pending = req && !rec.has_payment_method;
    }
    let mut out = rec.public_json();
    if let (Some(cfg), Some(c)) = (&fleet.signup.stripe, &rec.stripe_customer_id) {
        out["stripe_customer_url"] = json!(cfg.dashboard_customer_url(c));
    }
    fleet.persist_tenancy(&t);
    Ok(Json(out))
}

// ---- tenant: payment method ---------------------------------------------------------------------------------------

/// The console's base URL for Checkout's return links: `--public-url`, else this request's host.
fn console_base(cfg: &StripeConfig, headers: &HeaderMap) -> Result<String, AppError> {
    if let Some(u) = &cfg.public_url {
        return Ok(u.trim_end_matches('/').to_owned());
    }
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| {
            !h.is_empty()
                && h.len() <= 255
                && h.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
        })
        .ok_or_else(|| {
            err(
                StatusCode::CONFLICT,
                "start central with --public-url so Stripe can send people back to the console",
            )
        })?;
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|p| p.eq_ignore_ascii_case("https"));
    Ok(format!("{}://{host}", if https { "https" } else { "http" }))
}

/// `POST /fleet/billing/payment-method` (tenant): a Stripe Checkout page, in setup mode, to save a payment method.
async fn payment_method(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let Caller::Tenant(id) = &caller else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "the operator token is not a tenant; a tenant adds its own payment method",
        ));
    };
    let cfg = stripe_cfg(&fleet)?;
    let base = console_base(cfg, &headers)?;
    let customer = ensure_customer(&fleet, id).await?;
    let currency = fleet
        .tenancy
        .lock()
        .await
        .prices
        .currency
        .to_ascii_lowercase();
    let form = vec![
        kv("mode", "setup"),
        kv("customer", customer),
        kv("currency", currency),
        kv(
            "success_url",
            format!("{base}/console?payment_method=added#/billing"),
        ),
        kv(
            "cancel_url",
            format!("{base}/console?payment_method=cancelled#/billing"),
        ),
        kv("client_reference_id", id.as_str()),
        kv("metadata[fluxvm_tenant]", id.as_str()),
        kv("setup_intent_data[metadata][fluxvm_tenant]", id.as_str()),
    ];
    let v = client(&fleet, cfg)
        .post("/checkout/sessions", &form, None)
        .await
        .map_err(stripe_failed)?;
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .filter(|u| u.starts_with("https://"))
        .ok_or_else(|| {
            err(
                StatusCode::BAD_GATEWAY,
                "Stripe answered without a Checkout URL",
            )
        })?;
    Ok(Json(json!({ "url": url, "id": st::object_id(&v) })))
}

// ---- invoices -----------------------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct InvoiceQuery {
    #[serde(default)]
    month: Option<String>,
    /// Operator only.
    #[serde(default)]
    tenant: Option<String>,
}

/// `GET /fleet/billing/invoices[?month=&tenant=]`: a tenant's own invoices, or (operator) everyone's.
async fn list_invoices(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<InvoiceQuery>,
) -> Result<Json<Value>, AppError> {
    let t = fleet.tenancy.lock().await;
    let who = caller.tenant().map(str::to_owned).or(q.tenant.clone());
    let mut recs: Vec<&InvoiceRecord> = t
        .invoices
        .iter()
        .filter(|r| who.as_deref().is_none_or(|w| r.tenant == w))
        .filter(|r| q.month.as_deref().is_none_or(|m| r.month == m))
        .collect();
    recs.sort_by(|a, b| b.month.cmp(&a.month).then(a.tenant.cmp(&b.tenant)));
    let items: Vec<Value> = recs.iter().map(|r| r.public_json()).collect();
    let stripe = fleet.signup.stripe.as_ref();
    let billing = match caller.tenant() {
        Some(me) => {
            let rec = t.tenants.get(me);
            json!({
                "payments": stripe.is_some(),
                "enabled": rec.is_some_and(|r| r.billing_enabled),
                "has_payment_method": rec.is_some_and(|r| r.has_payment_method),
                "payment_method_pending": rec.is_some_and(|r| r.payment_method_pending),
            })
        }
        None => json!({
            "payments": stripe.is_some(),
            "mode": stripe.map(|c| c.mode()),
            "webhooks": stripe.is_some_and(|c| c.webhook_secret.is_some()),
            "dashboard": stripe.map(|c| c.dashboard_customer_url("").trim_end_matches("customers/").to_owned()),
            "auto_invoice": t.policy.billing_auto_invoice,
            "suspend_unpaid": t.policy.billing_suspend_unpaid,
            "suspend_after_days": t.policy.billing_suspend_after_days,
        }),
    };
    Ok(Json(json!({ "items": items, "billing": billing })))
}

/// `POST /fleet/billing/invoices?month=YYYY-MM[&tenant=T]` (operator): invoices a finished month.
async fn run_invoices(
    State(fleet): State<Fleet>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<InvoiceQuery>,
) -> Result<Json<Value>, AppError> {
    admin_only(&caller)?;
    stripe_cfg(&fleet)?;
    let month = q.month.as_deref().ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "say which month to invoice, e.g. ?month=2026-09",
        )
    })?;
    let (_, to) = crate::billing::parse_month(month).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("month must look like 2026-09, not '{month}'"),
        )
    })?;
    if to > Utc::now() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{month} has not ended yet: invoices are made for finished months"),
        ));
    }
    if let Some(one) = &q.tenant
        && !fleet.tenancy.lock().await.tenants.contains_key(one)
    {
        return Err(err(StatusCode::NOT_FOUND, format!("no tenant '{one}'")));
    }
    Ok(Json(invoice_run(&fleet, month, q.tenant.as_deref()).await))
}

/// Invoices `month` for every tenant with billing on (or just `only`). Safe to run again: a month already invoiced is
/// left alone, and a run that stopped half-way picks up where it stopped.
pub(super) async fn invoice_run(fleet: &Fleet, month: &str, only: Option<&str>) -> Value {
    let _one_at_a_time = fleet.signup.run_lock.lock().await;
    let ids: Vec<(String, bool)> = {
        let t = fleet.tenancy.lock().await;
        t.tenants
            .values()
            .filter(|r| only.is_none_or(|o| r.id == o))
            .map(|r| (r.id.clone(), r.billing_enabled))
            .collect()
    };
    let mut results = Vec::new();
    for (id, enabled) in ids {
        if !enabled {
            if only.is_some() {
                results.push(json!({"tenant": id, "outcome": "skipped", "reason": "billing is off for this tenant"}));
            }
            continue;
        }
        results.push(invoice_tenant(fleet, &id, month).await);
    }
    let count = |o: &str| results.iter().filter(|r| r["outcome"] == o).count();
    json!({
        "month": month,
        "created": count("created"),
        "existing": count("exists"),
        "skipped": count("skipped"),
        "failed": count("failed"),
        "results": results,
    })
}

/// Stores `rec` (one per tenant and month). A status Stripe already moved past (paid by a webhook while the run was
/// finishing) is kept.
async fn store_record(fleet: &Fleet, mut rec: InvoiceRecord) {
    let mut t = fleet.tenancy.lock().await;
    match t
        .invoices
        .iter_mut()
        .find(|r| r.tenant == rec.tenant && r.month == rec.month)
    {
        Some(old) => {
            if matches!(old.status.as_str(), "paid" | "void" | "uncollectible")
                && matches!(rec.status.as_str(), "draft" | "open")
            {
                rec.status = old.status.clone();
                rec.paid_at = old.paid_at;
                rec.payment_failed = old.payment_failed;
            }
            *old = rec;
        }
        None => t.invoices.push(rec),
    }
    fleet.persist_tenancy(&t);
}

async fn invoice_tenant(fleet: &Fleet, id: &str, month: &str) -> Value {
    let failed = |reason: String| json!({"tenant": id, "outcome": "failed", "reason": reason});
    let Some(cfg) = fleet.signup.stripe.as_ref() else {
        return failed("payments are not set up".into());
    };
    let Some((from, to)) = crate::billing::parse_month(month) else {
        return failed(format!("bad month '{month}'"));
    };
    let now = Utc::now();
    let (created, has_pm, statement, existing) = {
        let t = fleet.tenancy.lock().await;
        let Some(rec) = t.tenants.get(id) else {
            return failed("the tenant was deleted".into());
        };
        (
            rec.created_at,
            rec.has_payment_method,
            crate::billing::statement(&t, id, from, to, now),
            t.invoices
                .iter()
                .find(|r| r.tenant == id && r.month == month)
                .cloned(),
        )
    };
    if let Some(r) = &existing
        && r.status != "draft"
    {
        return json!({"tenant": id, "outcome": "exists", "invoice": r.public_json()});
    }
    let currency = statement.currency.to_ascii_lowercase();
    let lines: Vec<(String, i64)> = statement
        .lines
        .iter()
        .map(|l| {
            (
                format!("{} ({} {})", l.description, l.quantity, l.unit),
                st::micros_to_minor(l.amount_micros, &currency),
            )
        })
        .filter(|(_, minor)| *minor > 0)
        .collect();
    if lines.is_empty() && existing.is_none() {
        return json!({"tenant": id, "outcome": "skipped", "reason": format!("nothing to bill in {month}")});
    }
    let total: i64 = lines.iter().map(|(_, m)| m).sum();
    let customer = match ensure_customer(fleet, id).await {
        Ok(c) => c,
        Err(e) => return failed(e.1),
    };
    let stripe = client(fleet, cfg);
    let key = |step: &str| st::invoice_idempotency_key(id, created, month, step);
    let resumed = existing.is_some();
    let mut rec = match existing {
        Some(r) => r,
        None => {
            let label = from.format("%B %Y").to_string();
            let mut form = vec![
                kv("customer", customer.as_str()),
                kv("currency", currency.as_str()),
                // Off until the items are on and it is finalized: a draft left by a crash must not be auto-finalized
                // by Stripe half-filled.
                kv("auto_advance", "false"),
                kv("pending_invoice_items_behavior", "exclude"),
                kv("description", format!("FluxVM Cloud, {label}")),
                kv("metadata[fluxvm_tenant]", id),
                kv("metadata[fluxvm_month]", month),
            ];
            if has_pm {
                form.push(kv("collection_method", "charge_automatically"));
            } else {
                form.push(kv("collection_method", "send_invoice"));
                form.push(kv("days_until_due", DAYS_UNTIL_DUE.to_string()));
            }
            let v = match stripe
                .post("/invoices", &form, Some(key("invoice").as_str()))
                .await
            {
                Ok(v) => v,
                Err(e) => return failed(e.to_string()),
            };
            let Some(inv) = st::object_id(&v) else {
                return failed("Stripe answered without an invoice id".into());
            };
            let r = InvoiceRecord {
                tenant: id.to_owned(),
                month: month.to_owned(),
                stripe_invoice_id: Some(inv),
                status: "draft".into(),
                payment_failed: false,
                currency: currency.clone(),
                amount_minor: total,
                number: None,
                hosted_invoice_url: None,
                items_added: false,
                created_at: now,
                finalized_at: None,
                paid_at: None,
                updated_at: now,
                last_error: None,
            };
            store_record(fleet, r.clone()).await;
            r
        }
    };
    let Some(inv) = rec.stripe_invoice_id.clone() else {
        return failed("the invoice record has no Stripe id".into());
    };
    if !rec.items_added {
        // Stripe forgets idempotency keys after 24 hours, so a draft resumed later is asked which lines it already
        // has: adding them again would bill them twice.
        let mut on_invoice: HashSet<String> = HashSet::new();
        if resumed {
            match stripe
                .get(&format!("/invoiceitems?invoice={inv}&limit=100"))
                .await
            {
                Ok(v) => on_invoice.extend(
                    v.get("data")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|it| {
                            it.pointer("/metadata/fluxvm_line").and_then(Value::as_str)
                        })
                        .map(str::to_owned),
                ),
                Err(e) => {
                    rec.last_error = Some(e.to_string());
                    rec.updated_at = Utc::now();
                    store_record(fleet, rec).await;
                    return failed(e.to_string());
                }
            }
        }
        for (i, (desc, minor)) in lines.iter().enumerate() {
            if on_invoice.contains(&i.to_string()) {
                continue;
            }
            let form = vec![
                kv("customer", customer.as_str()),
                kv("invoice", inv.as_str()),
                kv("amount", minor.to_string()),
                kv("currency", currency.as_str()),
                kv("description", desc.as_str()),
                kv("metadata[fluxvm_tenant]", id),
                kv("metadata[fluxvm_month]", month),
                kv("metadata[fluxvm_line]", i.to_string()),
            ];
            if let Err(e) = stripe
                .post(
                    "/invoiceitems",
                    &form,
                    Some(key(&format!("item-{i}")).as_str()),
                )
                .await
            {
                rec.last_error = Some(e.to_string());
                rec.updated_at = Utc::now();
                store_record(fleet, rec).await;
                return failed(e.to_string());
            }
        }
        rec.items_added = true;
        rec.amount_minor = total;
        store_record(fleet, rec.clone()).await;
    }
    match stripe
        .post(
            &format!("/invoices/{inv}/finalize"),
            &[kv("auto_advance", "true")],
            Some(key("finalize").as_str()),
        )
        .await
    {
        Ok(v) => {
            rec.apply_stripe(&v, Utc::now());
            rec.last_error = None;
            store_record(fleet, rec.clone()).await;
            tracing::info!(tenant = %id, month, invoice = %inv, "invoice sent");
            json!({"tenant": id, "outcome": "created", "invoice": rec.public_json()})
        }
        Err(e) => {
            rec.last_error = Some(e.to_string());
            rec.updated_at = Utc::now();
            store_record(fleet, rec).await;
            failed(e.to_string())
        }
    }
}

// ---- the Stripe webhook -------------------------------------------------------------------------------------------

/// `POST /stripe/webhook`: Stripe's events, checked against the endpoint's signing secret.
async fn stripe_webhook(State(fleet): State<Fleet>, req: Request) -> Response {
    let Some(secret) = fleet
        .signup
        .stripe
        .as_ref()
        .and_then(|c| c.webhook_secret.clone())
    else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "Stripe webhooks are not set up on this central (--stripe-webhook-secret-file)",
        )
        .into_response();
    };
    let sig = req
        .headers()
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let Ok(body) = axum::body::to_bytes(req.into_body(), MAX_WEBHOOK_BODY).await else {
        return err(StatusCode::PAYLOAD_TOO_LARGE, "event too large").into_response();
    };
    if let Err(e) = st::verify_webhook(&secret, &sig, &body, Utc::now()) {
        tracing::warn!(reason = e, "refused a Stripe webhook");
        return err(StatusCode::BAD_REQUEST, e).into_response();
    }
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return err(StatusCode::BAD_REQUEST, "the event is not JSON").into_response();
    };
    let event_id = event
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if !event_id.is_empty() && lock(&fleet.signup.seen_events).contains(&event_id) {
        return Json(json!({"received": true, "duplicate": true})).into_response();
    }
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let obj = event
        .pointer("/data/object")
        .cloned()
        .unwrap_or(Value::Null);
    let handled = match kind {
        "invoice.paid"
        | "invoice.payment_succeeded"
        | "invoice.finalized"
        | "invoice.updated"
        | "invoice.voided"
        | "invoice.marked_uncollectible" => on_invoice(&fleet, &obj, false).await,
        "invoice.payment_failed" => on_invoice(&fleet, &obj, true).await,
        "checkout.session.completed" => on_checkout(&fleet, &obj).await,
        _ => Ok(()),
    };
    if let Err(e) = handled {
        // Not remembered as seen: Stripe sends it again later.
        tracing::warn!(event = %event_id, kind, error = %e.1, "handling a Stripe webhook");
        return e.into_response();
    }
    if !event_id.is_empty() {
        let mut seen = lock(&fleet.signup.seen_events);
        if seen.len() >= SEEN_EVENTS {
            seen.pop_front();
        }
        seen.push_back(event_id);
    }
    Json(json!({"received": true})).into_response()
}

/// `2026-09` as `September 2026`.
fn month_name(month: &str) -> String {
    crate::billing::parse_month(month).map_or_else(
        || month.to_owned(),
        |(from, _)| from.format("%B %Y").to_string(),
    )
}

/// A tenant's unpaid invoices older than the policy allows.
fn overdue(t: &TenancyState, tenant: &str, now: DateTime<Utc>) -> bool {
    let days = i64::from(t.policy.billing_suspend_after_days);
    t.invoices.iter().any(|r| {
        r.tenant == tenant
            && r.unpaid()
            && r.finalized_at.is_some_and(|f| (now - f).num_days() >= days)
    })
}

async fn on_invoice(fleet: &Fleet, obj: &Value, payment_failed: bool) -> Result<(), AppError> {
    let Some(inv) = st::object_id(obj) else {
        return Ok(());
    };
    let now = Utc::now();
    let mut news: Vec<(&'static str, String, String, String)> = Vec::new();
    {
        let mut t = fleet.tenancy.lock().await;
        let Some(rec) = t
            .invoices
            .iter_mut()
            .find(|r| r.stripe_invoice_id.as_deref() == Some(inv.as_str()))
        else {
            // Not one of central's invoices (the account may bill other things too).
            return Ok(());
        };
        rec.apply_stripe(obj, now);
        let (tenant, month, paid) = (rec.tenant.clone(), rec.month.clone(), rec.status == "paid");
        if payment_failed && rec.unpaid() && !rec.payment_failed {
            rec.payment_failed = true;
            let amount = st::minor_to_amount(rec.amount_minor, &rec.currency);
            news.push((
                "billing.payment_failed",
                tenant.clone(),
                format!("Payment failed for the {} invoice", month_name(&month)),
                format!(
                    "{amount:.2} {} is still due. Pay it from Billing, or add another payment method.",
                    rec.currency.to_ascii_uppercase()
                ),
            ));
        }
        if paid
            && !overdue(&t, &tenant, now)
            && let Some(rec) = t.tenants.get_mut(&tenant)
            && rec.suspended_for_nonpayment
        {
            rec.suspended_for_nonpayment = false;
            rec.disabled = false;
            tracing::info!(tenant = %tenant, "invoice paid: tenant enabled again");
        }
        fleet.persist_tenancy(&t);
    }
    for (kind, tenant, title, detail) in news {
        notify::emit(
            fleet,
            kind,
            Some(tenant),
            title,
            Some(detail),
            Some("#/billing".into()),
        )
        .await;
    }
    Ok(())
}

/// A Checkout session in setup mode finished: make the saved method the customer's default for invoices, and let the
/// tenant lease and launch.
async fn on_checkout(fleet: &Fleet, obj: &Value) -> Result<(), AppError> {
    if obj.get("mode").and_then(Value::as_str) != Some("setup") {
        return Ok(());
    }
    let str_of = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_owned);
    let Some(tenant) = obj
        .pointer("/metadata/fluxvm_tenant")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| str_of("client_reference_id"))
    else {
        return Ok(());
    };
    let Some(customer) = str_of("customer").filter(|c| st::valid_object_id(c)) else {
        return Ok(());
    };
    let ours = fleet
        .tenancy
        .lock()
        .await
        .tenants
        .get(&tenant)
        .is_some_and(|t| t.stripe_customer_id.as_deref() == Some(customer.as_str()));
    if !ours {
        return Ok(());
    }
    let cfg = stripe_cfg(fleet)?;
    if let Some(si) = str_of("setup_intent").filter(|s| st::valid_object_id(s)) {
        let intent = client(fleet, cfg)
            .get(&format!("/setup_intents/{si}"))
            .await
            .map_err(stripe_failed)?;
        let pm = intent
            .get("payment_method")
            .and_then(|p| p.as_str().map(str::to_owned).or_else(|| st::object_id(p)))
            .filter(|p| st::valid_object_id(p));
        if let Some(pm) = pm {
            client(fleet, cfg)
                .post(
                    &format!("/customers/{customer}"),
                    &[kv("invoice_settings[default_payment_method]", pm)],
                    None,
                )
                .await
                .map_err(stripe_failed)?;
        }
    }
    let mut t = fleet.tenancy.lock().await;
    if let Some(rec) = t.tenants.get_mut(&tenant) {
        rec.has_payment_method = true;
        rec.payment_method_pending = false;
        tracing::info!(tenant = %tenant, "payment method saved");
    }
    fleet.persist_tenancy(&t);
    Ok(())
}

// ---- the monthly job ----------------------------------------------------------------------------------------------

/// Once an hour: on the 1st, invoice last month when the policy says so; suspend tenants with overdue invoices when
/// the policy says so. Both are off by default.
pub(super) fn spawn_billing_job(fleet: Fleet) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(StdDuration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if fleet.leading() {
                billing_tick(&fleet, Utc::now()).await;
            }
        }
    });
}

async fn billing_tick(fleet: &Fleet, now: DateTime<Utc>) {
    let policy = fleet.tenancy.lock().await.policy.clone();
    if policy.billing_auto_invoice && fleet.signup.stripe.is_some() && now.day() == 1 {
        let (last_month, _) = crate::billing::month_of(now - chrono::Duration::days(1));
        let month = last_month.format("%Y-%m").to_string();
        let report = invoice_run(fleet, &month, None).await;
        tracing::info!(%report, "monthly invoice run");
    }
    if policy.billing_suspend_unpaid {
        suspend_overdue(fleet, now).await;
    }
}

/// Disables tenants with an invoice unpaid past the policy's days.
async fn suspend_overdue(fleet: &Fleet, now: DateTime<Utc>) -> Vec<String> {
    let suspended: Vec<String> = {
        let mut t = fleet.tenancy.lock().await;
        let ids: Vec<String> = t
            .tenants
            .values()
            .filter(|r| !r.disabled && r.billing_enabled)
            .map(|r| r.id.clone())
            .filter(|id| overdue(&t, id, now))
            .collect();
        for id in &ids {
            if let Some(rec) = t.tenants.get_mut(id) {
                rec.disabled = true;
                rec.suspended_for_nonpayment = true;
            }
        }
        if !ids.is_empty() {
            fleet.persist_tenancy(&t);
        }
        ids
    };
    for id in &suspended {
        tracing::warn!(tenant = %id, "tenant suspended for an unpaid invoice");
        notify::emit(
            fleet,
            "billing.suspended",
            Some(id.clone()),
            format!("{id} was suspended for an unpaid invoice"),
            Some("It is enabled again as soon as the invoice is paid.".into()),
            Some("#/billing".into()),
        )
        .await;
    }
    suspended
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use chrono::TimeZone;
    use tower::ServiceExt;

    #[test]
    fn emails_are_checked_and_their_domain_lowercased() {
        assert_eq!(
            normalize_email(" Ops.Team+cloud@Acme.IO ").unwrap(),
            "Ops.Team+cloud@acme.io"
        );
        for bad in [
            "",
            "acme.io",
            "@acme.io",
            "ops@",
            "ops@acme",
            "ops@acme.i",
            "ops@-acme.io",
            "ops@acme..io",
            "o..ps@acme.io",
            ".ops@acme.io",
            "ops @acme.io",
            "a@b@acme.io",
            "ops@ac\nme.io",
            "ops@acme.123",
            "<script>@acme.io",
        ] {
            assert!(normalize_email(bad).is_err(), "{bad:?} passed");
        }
        assert!(normalize_email(&format!("{}@acme.io", "a".repeat(65))).is_err());
        assert!(domain_allowed("a@acme.io", &[]));
        assert!(domain_allowed("a@acme.io", &["ACME.io".into()]));
        assert!(domain_allowed("a@acme.io", &["@acme.io".into()]));
        assert!(!domain_allowed("a@evil-acme.io", &["acme.io".into()]));
        assert!(!domain_allowed("a@sub.acme.io", &["acme.io".into()]));
        assert_eq!(
            clean_display_name(Some("  Acme ".into()))
                .unwrap()
                .as_deref(),
            Some("Acme")
        );
        assert_eq!(clean_display_name(Some("   ".into())).unwrap(), None);
        assert!(clean_display_name(Some("a\u{7}b".into())).is_err());
        assert!(clean_display_name(Some("x".repeat(81))).is_err());
    }

    #[test]
    fn sign_ups_are_limited_per_address_and_overall() {
        let mut l = Limiter::default();
        let now = Instant::now();
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let b: IpAddr = "198.51.100.9".parse().unwrap();
        for _ in 0..SIGNUPS_PER_ADDRESS_PER_HOUR {
            assert!(l.allow(Some(a), now).is_ok());
        }
        assert!(l.allow(Some(a), now).is_err());
        assert!(
            l.allow(Some(b), now).is_ok(),
            "another address is not limited by the first"
        );
        // An hour later the address may sign up again.
        assert!(l.allow(Some(a), now + StdDuration::from_secs(3601)).is_ok());
        // The overall cap holds whatever the address.
        let mut l = Limiter::default();
        for i in 0..SIGNUPS_PER_HOUR {
            let ip = IpAddr::V4(std::net::Ipv4Addr::new(
                10,
                0,
                (i / 250) as u8,
                (i % 250) as u8,
            ));
            assert!(l.allow(Some(ip), now).is_ok());
        }
        assert!(l.allow(Some(b), now).is_err());
        assert!(l.allow(None, now).is_err());
    }

    #[test]
    fn client_addresses_group_ipv6_by_64_and_trust_only_a_local_proxy() {
        let req = |peer: &str, xff: Option<&str>| {
            let mut r = Request::builder().uri("/signup");
            if let Some(x) = xff {
                r = r.header("x-forwarded-for", x);
            }
            let mut r = r.body(Body::empty()).unwrap();
            r.extensions_mut()
                .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
            r
        };
        assert_eq!(
            client_addr(&req("203.0.113.7:5000", Some("1.2.3.4"))),
            Some("203.0.113.7".parse().unwrap()),
            "a remote peer cannot claim another address"
        );
        assert_eq!(
            client_addr(&req("127.0.0.1:5000", Some("9.9.9.9, 1.2.3.4"))),
            Some("1.2.3.4".parse().unwrap())
        );
        assert_eq!(
            client_addr(&req("[2001:db8:1:2:3:4:5:6]:5000", None)),
            Some("2001:db8:1:2::".parse().unwrap())
        );
        let bare = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(client_addr(&bare), None);
    }

    // ---- end to end, against a mock Stripe ------------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct Call {
        method: String,
        path: String,
        idempotency: Option<String>,
        auth: String,
        body: String,
    }

    type Calls = Arc<std::sync::Mutex<Vec<Call>>>;

    async fn mock_stripe(
        State(calls): State<Calls>,
        method: Method,
        uri: axum::http::Uri,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let h = |k: &str| {
            headers
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        calls.lock().unwrap().push(Call {
            method: method.to_string(),
            path: uri.path().to_owned(),
            idempotency: h("idempotency-key"),
            auth: h("authorization").unwrap_or_default(),
            body,
        });
        let ok = |v: Value| Json(v).into_response();
        match (method.as_str(), uri.path()) {
            ("POST", "/v1/customers") => ok(json!({"id": "cus_mock", "object": "customer"})),
            ("POST", "/v1/customers/cus_mock") => ok(json!({"id": "cus_mock"})),
            ("POST", "/v1/checkout/sessions") => {
                ok(json!({"id": "cs_test_1", "url": "https://checkout.stripe.com/c/pay/cs_test_1"}))
            }
            ("GET", "/v1/setup_intents/seti_1") => {
                ok(json!({"id": "seti_1", "payment_method": "pm_1"}))
            }
            ("POST", "/v1/invoices") => ok(json!({"id": "in_mock", "status": "draft"})),
            ("POST", "/v1/invoiceitems") => ok(json!({"id": "ii_1"})),
            ("GET", "/v1/invoiceitems") => ok(json!({"object": "list", "data": [
                {"id": "ii_1", "metadata": {"fluxvm_line": "0"}}
            ]})),
            ("POST", "/v1/invoices/in_mock/finalize") => ok(json!({
                "id": "in_mock", "status": "open", "number": "FLUX-0001", "total": 4320,
                "hosted_invoice_url": "https://invoice.stripe.com/i/in_mock"
            })),
            _ => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": {"message": "no such route in the mock"}})),
            )
                .into_response(),
        }
    }

    const SECRET: &str = "whsec_mock";

    async fn setup() -> (Fleet, Router, Calls) {
        setup_with(None).await
    }

    /// Where the tests' sign-up mail goes: every JSON body POSTed to it, in order.
    async fn mail_sink() -> (String, Arc<std::sync::Mutex<Vec<(Value, Option<String>)>>>) {
        let got: Arc<std::sync::Mutex<Vec<(Value, Option<String>)>>> = Arc::default();
        let g = got.clone();
        let app = Router::new().route(
            "/",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let g = g.clone();
                async move {
                    let sig = headers
                        .get("x-fluxvm-signature")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    g.lock()
                        .unwrap()
                        .push((serde_json::from_slice(&body).unwrap_or(Value::Null), sig));
                    StatusCode::OK
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, got)
    }

    async fn setup_with(mail: Option<MailConfig>) -> (Fleet, Router, Calls) {
        let calls: Calls = Arc::default();
        let mock = Router::new()
            .fallback(mock_stripe)
            .with_state(calls.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, mock).await.unwrap() });
        let fleet = Fleet {
            nodes: Arc::default(),
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            persist_path: None,
            lock_path: None,
            token: Some("op".into()),
            tenancy: Arc::default(),
            tenancy_path: None,
            ha: None,
            audit: Arc::new(super::super::audit::AuditLog::memory()),
            notify: Arc::default(),
            metrics: Arc::default(),
            tunnels: Arc::default(),
            sso: Arc::default(),
            signup: Arc::new(
                Runtime::new(Some(StripeConfig {
                    secret_key: "sk_test_mock".into(),
                    webhook_secret: Some(SECRET.into()),
                    api_base: base,
                    public_url: Some("https://cloud.example.com/".into()),
                }))
                .with_mail(mail),
            ),
        };
        let app = routes()
            .route("/fleet/tenants/me", get(super::super::cloud::tenant_me))
            .route("/fleet/leases", post(|| async { "leased" }))
            .layer(middleware::from_fn_with_state(
                fleet.clone(),
                auth_middleware,
            ))
            .with_state(fleet.clone());
        (fleet, app, calls)
    }

    async fn send(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut r = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            r = r.header("authorization", format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => r
                .header("content-type", "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => r.body(Body::empty()).unwrap(),
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn webhook(app: &Router, event: Value, secret: &str) -> StatusCode {
        let body = event.to_string();
        let t = Utc::now().timestamp();
        let sig = st::webhook_signature(secret, t, body.as_bytes());
        let req = Request::builder()
            .method("POST")
            .uri("/stripe/webhook")
            .header("stripe-signature", format!("t={t},v1={sig}"))
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    /// Every route that starts or restores billable VM time is held back for a tenant that still owes a payment method,
    /// and nothing else is.
    /// Email confirmation: nothing exists until the mailed link is confirmed, the link works once, and a sign-up
    /// that could not be told apart from a taken address gets the same answer.
    #[tokio::test]
    async fn the_account_exists_only_after_its_link_is_confirmed() {
        let (url, mail) = mail_sink().await;
        let (fleet, app, _) = setup_with(Some(MailConfig {
            url,
            secret: Some("mail-secret".into()),
            public_url: "https://cloud.example.com".into(),
        }))
        .await;
        fleet.tenancy.lock().await.policy.signup_enabled = true;
        let (s, cfg) = send(&app, "GET", "/signup/config", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            (cfg["open"].clone(), cfg["email_confirmation"].clone()),
            (json!(true), json!(true))
        );

        let body = json!({"tenant_id": "acme", "email": "Ops@Acme.io"});
        let (s, v) = send(&app, "POST", "/signup", None, Some(body.clone())).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        assert!(v.get("token").is_none(), "no token before the link: {v}");
        // The mail goes out in the background.
        for _ in 0..50 {
            if !mail.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        let (payload, sig) = mail.lock().unwrap()[0].clone();
        assert_eq!(payload["type"], "signup.verify");
        assert_eq!(payload["to"], "Ops@acme.io");
        assert!(
            sig.unwrap().starts_with("sha256="),
            "the mail webhook is signed"
        );
        let link = payload["link"].as_str().unwrap().to_owned();
        assert!(
            link.starts_with("https://cloud.example.com/signup/verify?token=fvv_"),
            "{link}"
        );
        let token = link.split("token=").nth(1).unwrap().to_owned();
        // Nothing exists yet, and the address is not an account.
        assert!(fleet.tenancy.lock().await.tenants.is_empty());
        assert_eq!(fleet.tenancy.lock().await.pending_signups.len(), 1);

        // A second request for the same address inside the resend window sends nothing more.
        let (s, _) = send(&app, "POST", "/signup", None, Some(body)).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        tokio::time::sleep(StdDuration::from_millis(100)).await;
        assert_eq!(mail.lock().unwrap().len(), 1);

        // The link's page shows the sign-up without using it up; a wrong token is refused.
        let page = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/signup/verify?token={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let html = String::from_utf8(
            axum::body::to_bytes(page.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("acme") && html.contains("Ops@acme.io") && !html.contains("fvt_"));
        assert!(
            fleet.tenancy.lock().await.tenants.is_empty(),
            "viewing the link creates nothing"
        );
        let (s, _) = send(
            &app,
            "POST",
            "/signup/verify",
            None,
            Some(json!({"token": "fvv_nope"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // Confirming creates the tenant, with its address marked confirmed, and answers with the token once.
        let (s, v) = send(
            &app,
            "POST",
            "/signup/verify",
            None,
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let tok = v["token"].as_str().unwrap().to_owned();
        assert!(tok.starts_with("fvt_"));
        {
            let t = fleet.tenancy.lock().await;
            let acme = &t.tenants["acme"];
            assert!(acme.owner_email_verified && acme.self_service);
            assert_eq!(acme.owner_email.as_deref(), Some("Ops@acme.io"));
            assert!(t.pending_signups.is_empty());
        }
        let (s, me) = send(&app, "GET", "/fleet/tenants/me", Some(&tok), None).await;
        assert_eq!((s, me["id"].clone()), (StatusCode::OK, json!("acme")));
        // The link works once.
        let (s, _) = send(
            &app,
            "POST",
            "/signup/verify",
            None,
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // The same address again: the same answer, and a different mail (so the form leaks nothing).
        let (s, _) = send(
            &app,
            "POST",
            "/signup",
            None,
            Some(json!({"tenant_id": "acme2", "email": "ops@acme.io"})),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED);
        for _ in 0..50 {
            if mail.lock().unwrap().len() >= 2 {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        assert_eq!(mail.lock().unwrap()[1].0["type"], "signup.exists");
        assert!(fleet.tenancy.lock().await.pending_signups.is_empty());
        // A taken tenant ID is said plainly: an ID is not a secret and the person must pick another.
        let (s, _) = send(
            &app,
            "POST",
            "/signup",
            None,
            Some(json!({"tenant_id": "acme", "email": "other@acme.io"})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
    }

    /// Sign-up cannot be opened while confirmation is required and central has no way to send the link; a link that
    /// expired is gone.
    #[tokio::test]
    async fn sign_up_stays_closed_without_a_mail_path_and_links_expire() {
        let (fleet, app, _) = setup().await;
        fleet.tenancy.lock().await.policy.signup_enabled = true;
        let (_, cfg) = send(&app, "GET", "/signup/config", None, None).await;
        assert_eq!(cfg["open"], json!(false));
        let (s, _) = send(
            &app,
            "POST",
            "/signup",
            None,
            Some(json!({"tenant_id": "acme", "email": "ops@acme.io"})),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // An expired pending sign-up is not valid and is dropped.
        let past = Utc::now() - chrono::Duration::hours(25);
        let token = {
            let mut t = fleet.tenancy.lock().await;
            t.add_pending_signup("late", "late@acme.io", None, past)
                .unwrap()
        };
        let (s, _) = send(
            &app,
            "POST",
            "/signup/verify",
            None,
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(fleet.tenancy.lock().await.tenants.is_empty());
    }

    #[tokio::test]
    async fn the_payment_gate_covers_start_restart_resume_and_snapshot_restore() {
        use axum::http::Method;
        let (fleet, _app, _calls) = setup().await;
        {
            let mut t = fleet.tenancy.lock().await;
            t.create_tenant("acme", None, Default::default(), Utc::now())
                .unwrap();
            t.tenants.get_mut("acme").unwrap().payment_method_pending = true;
        }
        let held = [
            (Method::POST, "/fleet/vms"),
            (Method::POST, "/fleet/leases"),
            (Method::POST, "/fleet/vms/mac-1/abc/clone"),
            (Method::POST, "/fleet/vms/mac-1/abc/start"),
            (Method::POST, "/fleet/vms/mac-1/abc/restart"),
            (Method::POST, "/fleet/vms/mac-1/abc/resume"),
            (Method::POST, "/fleet/vms/mac-1/abc/snapshots/clean/restore"),
        ];
        for (m, p) in &held {
            assert!(
                payment_gate(&fleet, "acme", m, p).await.is_some(),
                "{m} {p} should be held"
            );
        }
        let free = [
            (Method::GET, "/fleet/vms"),
            (Method::POST, "/fleet/vms/mac-1/abc/stop"),
            (Method::POST, "/fleet/vms/mac-1/abc/pause"),
            (Method::DELETE, "/fleet/vms/mac-1/abc"),
            (Method::GET, "/fleet/vms/mac-1/abc/snapshots"),
            (Method::POST, "/fleet/billing/payment-method"),
        ];
        for (m, p) in &free {
            assert!(
                payment_gate(&fleet, "acme", m, p).await.is_none(),
                "{m} {p} should pass"
            );
        }
        // Once the payment method exists, nothing is held.
        fleet
            .tenancy
            .lock()
            .await
            .tenants
            .get_mut("acme")
            .unwrap()
            .payment_method_pending = false;
        for (m, p) in &held {
            assert!(payment_gate(&fleet, "acme", m, p).await.is_none());
        }
    }

    #[tokio::test]
    async fn sign_up_pay_and_invoice_end_to_end() {
        let (fleet, app, calls) = setup().await;

        // Closed by default.
        let (s, cfg) = send(&app, "GET", "/signup/config", None, None).await;
        assert_eq!((s, cfg["open"].clone()), (StatusCode::OK, json!(false)));
        let body = json!({"tenant_id": "acme", "display_name": "Acme", "email": "Ops@Acme.IO"});
        let (s, _) = send(&app, "POST", "/signup", None, Some(body.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        {
            let mut t = fleet.tenancy.lock().await;
            t.policy.signup_enabled = true;
            t.policy.signup_require_email_verification = false;
            t.policy.signup_require_payment_method = true;
        }
        let (_, cfg) = send(&app, "GET", "/signup/config", None, None).await;
        assert_eq!(cfg["open"], true);
        assert_eq!(cfg["require_payment_method"], true);

        // Bad input is refused before anything is created.
        for (bad, why) in [
            (json!({"tenant_id": "acme", "email": "nope"}), "email"),
            (json!({"tenant_id": "Bad Id", "email": "a@acme.io"}), "id"),
            (
                json!({"tenant_id": "admin", "email": "a@acme.io"}),
                "reserved",
            ),
        ] {
            let (s, v) = send(&app, "POST", "/signup", None, Some(bad)).await;
            assert!(s.is_client_error(), "{why}: {s} {v}");
        }
        assert!(fleet.tenancy.lock().await.tenants.is_empty());

        // Sign up: a tenant with the default quota, the token once, a Stripe customer.
        let (s, v) = send(&app, "POST", "/signup", None, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let token = v["token"].as_str().unwrap().to_owned();
        assert_eq!(v["payment_method_required"], true);
        assert_eq!(v["tenant"]["owner_email"], "Ops@acme.io");
        assert_eq!(v["tenant"]["stripe_customer_id"], "cus_mock");
        {
            let t = fleet.tenancy.lock().await;
            let rec = &t.tenants["acme"];
            assert_eq!(rec.quota, t.policy.signup_default_quota);
            assert!(rec.self_service && rec.billing_enabled && rec.payment_method_pending);
            assert!(
                !format!("{rec:?}").contains(&token),
                "only the hash is stored"
            );
        }
        let customer_call = calls.lock().unwrap()[0].clone();
        assert_eq!(customer_call.path, "/v1/customers");
        assert_eq!(customer_call.auth, "Bearer sk_test_mock");
        assert!(
            customer_call
                .body
                .contains("metadata%5Bfluxvm_tenant%5D=acme")
        );
        assert!(
            customer_call
                .idempotency
                .unwrap()
                .starts_with("fluxvm-customer-acme-")
        );
        // The same id or the same email again: refused.
        let (s, _) = send(
            &app,
            "POST",
            "/signup",
            None,
            Some(json!({"tenant_id": "acme", "email": "x@acme.io"})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, _) = send(
            &app,
            "POST",
            "/signup",
            None,
            Some(json!({"tenant_id": "acme2", "email": "ops@ACME.io"})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);

        // The new token works, but leasing waits for a payment method.
        let (s, me) = send(&app, "GET", "/fleet/tenants/me", Some(&token), None).await;
        assert_eq!((s, me["id"].clone()), (StatusCode::OK, json!("acme")));
        let (s, _) = send(&app, "POST", "/fleet/leases", Some(&token), Some(json!({}))).await;
        assert_eq!(s, StatusCode::PAYMENT_REQUIRED);

        // Checkout in setup mode, back to the console.
        let (s, v) = send(
            &app,
            "POST",
            "/fleet/billing/payment-method",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["url"], "https://checkout.stripe.com/c/pay/cs_test_1");
        let session = calls.lock().unwrap().last().unwrap().clone();
        assert_eq!(session.path, "/v1/checkout/sessions");
        assert!(session.body.contains("mode=setup") && session.body.contains("customer=cus_mock"));
        assert!(session.body.contains(
            "success_url=https%3A%2F%2Fcloud.example.com%2Fconsole%3Fpayment_method%3Dadded%23%2Fbilling"
        ));

        // A forged webhook does nothing; the signed one saves the method.
        let done = json!({"id": "evt_1", "type": "checkout.session.completed", "data": {"object": {
            "id": "cs_test_1", "mode": "setup", "customer": "cus_mock", "setup_intent": "seti_1",
            "metadata": {"fluxvm_tenant": "acme"}}}});
        assert_eq!(
            webhook(&app, done.clone(), "whsec_wrong").await,
            StatusCode::BAD_REQUEST
        );
        assert!(fleet.tenancy.lock().await.tenants["acme"].payment_method_pending);
        assert_eq!(webhook(&app, done.clone(), SECRET).await, StatusCode::OK);
        {
            let t = fleet.tenancy.lock().await;
            assert!(
                t.tenants["acme"].has_payment_method && !t.tenants["acme"].payment_method_pending
            );
        }
        let n = calls.lock().unwrap().len();
        let set_default = calls.lock().unwrap()[n - 1].clone();
        assert_eq!(set_default.path, "/v1/customers/cus_mock");
        assert!(set_default.body.contains("default_payment_method%5D=pm_1"));
        // Stripe resends: nothing is called again.
        assert_eq!(webhook(&app, done, SECRET).await, StatusCode::OK);
        assert_eq!(calls.lock().unwrap().len(), n);
        let (s, _) = send(&app, "POST", "/fleet/leases", Some(&token), Some(json!({}))).await;
        assert_eq!(s, StatusCode::OK);

        // September: a 24-hour lease at 1.80 an hour is 43.20 USD.
        {
            let mut t = fleet.tenancy.lock().await;
            let start = Utc.with_ymd_and_hms(2026, 9, 3, 0, 0, 0).unwrap();
            t.leases.push(crate::tenancy::Lease {
                id: "l1".into(),
                tenant: "acme".into(),
                node: "studio-1".into(),
                started_at: start,
                min_end: start + chrono::Duration::hours(24),
                ends_at: None,
                released_at: Some(start + chrono::Duration::hours(10)),
                state: crate::tenancy::LeaseState::Released,
                rate_micros: Some(1_800_000),
                price_class: Some("Mac Studio".into()),
            });
        }
        // Tenants cannot run invoices; an unfinished month is refused.
        let (s, _) = send(
            &app,
            "POST",
            "/fleet/billing/invoices?month=2026-09",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let next = Utc::now().format("%Y-%m").to_string();
        let (s, _) = send(
            &app,
            "POST",
            &format!("/fleet/billing/invoices?month={next}"),
            Some("op"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);

        let before = calls.lock().unwrap().len();
        let (s, v) = send(
            &app,
            "POST",
            "/fleet/billing/invoices?month=2026-09",
            Some("op"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["created"], 1, "{v}");
        let run: Vec<Call> = calls.lock().unwrap()[before..].to_vec();
        let paths: Vec<&str> = run.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/v1/invoices",
                "/v1/invoiceitems",
                "/v1/invoices/in_mock/finalize"
            ]
        );
        assert!(
            run[0]
                .body
                .contains("collection_method=charge_automatically")
        );
        assert!(
            run[0]
                .body
                .contains("pending_invoice_items_behavior=exclude")
        );
        assert!(run[1].body.contains("amount=4320") && run[1].body.contains("currency=usd"));
        assert!(run.iter().all(|c| {
            c.idempotency
                .as_deref()
                .is_some_and(|k| k.contains("-2026-09-"))
        }));
        assert!(run[1].idempotency.as_deref().unwrap().ends_with("-item-0"));
        let inv = &v["results"][0]["invoice"];
        assert_eq!(inv["status"], "open");
        assert_eq!(inv["amount"], 43.2);
        assert_eq!(
            inv["hosted_invoice_url"],
            "https://invoice.stripe.com/i/in_mock"
        );

        // Running it again bills nobody twice and calls Stripe no more.
        let (_, v) = send(
            &app,
            "POST",
            "/fleet/billing/invoices?month=2026-09",
            Some("op"),
            None,
        )
        .await;
        assert_eq!(
            (v["created"].clone(), v["existing"].clone()),
            (json!(0), json!(1))
        );
        assert_eq!(calls.lock().unwrap().len(), before + 3);

        // The payment fails: a notification for the tenant. Then it is paid.
        let failed = json!({"id": "evt_2", "type": "invoice.payment_failed", "data": {"object": {
            "id": "in_mock", "status": "open"}}});
        assert_eq!(webhook(&app, failed, SECRET).await, StatusCode::OK);
        let ntf = fleet.notify.list(Some("acme"), 10);
        assert!(
            ntf.iter().any(|n| n.kind == "billing.payment_failed"),
            "{ntf:?}"
        );
        let (_, list) = send(&app, "GET", "/fleet/billing/invoices", Some(&token), None).await;
        assert_eq!(list["items"][0]["payment_failed"], true);
        let paid = json!({"id": "evt_3", "type": "invoice.paid", "data": {"object": {
            "id": "in_mock", "status": "paid"}}});
        assert_eq!(webhook(&app, paid, SECRET).await, StatusCode::OK);
        let (_, list) = send(&app, "GET", "/fleet/billing/invoices", Some(&token), None).await;
        assert_eq!(list["items"][0]["status"], "paid");
        assert_eq!(list["items"][0]["payment_failed"], false);
        assert_eq!(list["billing"]["has_payment_method"], true);

        // Another tenant sees none of it.
        let other = {
            let mut t = fleet.tenancy.lock().await;
            t.create_tenant("globex", None, Default::default(), Utc::now())
                .unwrap()
                .1
        };
        let (_, list) = send(
            &app,
            "GET",
            "/fleet/billing/invoices?tenant=acme",
            Some(&other),
            None,
        )
        .await;
        assert_eq!(list["items"], json!([]));
        let (s, _) = send(
            &app,
            "PUT",
            "/fleet/tenants/acme/billing",
            Some(&other),
            Some(json!({"enabled": false})),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (_, all) = send(&app, "GET", "/fleet/billing/invoices", Some("op"), None).await;
        assert_eq!(all["items"].as_array().unwrap().len(), 1);
        assert_eq!(all["billing"]["mode"], "test");
    }

    #[tokio::test]
    async fn sign_ups_from_one_address_are_rate_limited() {
        let (fleet, app, _) = setup().await;
        {
            let mut t = fleet.tenancy.lock().await;
            t.policy.signup_enabled = true;
            t.policy.signup_require_email_verification = false;
        }
        let mut last = StatusCode::OK;
        for i in 0..=SIGNUPS_PER_ADDRESS_PER_HOUR {
            let mut req = Request::builder()
                .method("POST")
                .uri("/signup")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"tenant_id": format!("t{i}"), "email": format!("u{i}@example.com")})
                        .to_string(),
                ))
                .unwrap();
            req.extensions_mut().insert(ConnectInfo(
                "203.0.113.5:4000".parse::<SocketAddr>().unwrap(),
            ));
            last = app.clone().oneshot(req).await.unwrap().status();
            if i < SIGNUPS_PER_ADDRESS_PER_HOUR {
                assert_eq!(last, StatusCode::CREATED);
            }
        }
        assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn unpaid_invoices_suspend_only_when_the_policy_says_so() {
        let (fleet, app, _) = setup().await;
        let now = Utc::now();
        let token = {
            let mut t = fleet.tenancy.lock().await;
            let tok = t
                .create_tenant("acme", None, Default::default(), now)
                .unwrap()
                .1;
            t.tenants.get_mut("acme").unwrap().billing_enabled = true;
            t.invoices.push(InvoiceRecord {
                tenant: "acme".into(),
                month: "2026-08".into(),
                stripe_invoice_id: Some("in_old".into()),
                status: "open".into(),
                payment_failed: true,
                currency: "usd".into(),
                amount_minor: 100,
                number: None,
                hosted_invoice_url: None,
                items_added: true,
                created_at: now - chrono::Duration::days(40),
                finalized_at: Some(now - chrono::Duration::days(40)),
                paid_at: None,
                updated_at: now,
                last_error: None,
            });
            tok
        };
        billing_tick(&fleet, now).await;
        assert!(
            !fleet.tenancy.lock().await.tenants["acme"].disabled,
            "off by default"
        );
        fleet.tenancy.lock().await.policy.billing_suspend_unpaid = true;
        billing_tick(&fleet, now).await;
        assert!(fleet.tenancy.lock().await.tenants["acme"].disabled);
        let (s, _) = send(&app, "GET", "/fleet/tenants/me", Some(&token), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let paid = json!({"id": "evt_9", "type": "invoice.paid", "data": {"object": {"id": "in_old", "status": "paid"}}});
        assert_eq!(webhook(&app, paid, SECRET).await, StatusCode::OK);
        let t = fleet.tenancy.lock().await;
        assert!(!t.tenants["acme"].disabled && !t.tenants["acme"].suspended_for_nonpayment);
    }

    /// A run that stopped after adding some lines, resumed after Stripe forgot its idempotency keys: the lines already
    /// on the draft are not added again.
    #[tokio::test]
    async fn a_resumed_draft_does_not_bill_a_line_twice() {
        let (fleet, _, calls) = setup().await;
        let now = Utc::now();
        {
            let mut t = fleet.tenancy.lock().await;
            let created = Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
            t.create_tenant("acme", None, crate::tenancy::Quota::default(), created)
                .unwrap();
            let rec = t.tenants.get_mut("acme").unwrap();
            rec.billing_enabled = true;
            rec.stripe_customer_id = Some("cus_mock".into());
            let start = Utc.with_ymd_and_hms(2026, 9, 3, 0, 0, 0).unwrap();
            t.leases.push(crate::tenancy::Lease {
                id: "l1".into(),
                tenant: "acme".into(),
                node: "studio-1".into(),
                started_at: start,
                min_end: start + chrono::Duration::hours(24),
                ends_at: None,
                released_at: Some(start + chrono::Duration::hours(10)),
                state: crate::tenancy::LeaseState::Released,
                rate_micros: Some(1_800_000),
                price_class: Some("Mac Studio".into()),
            });
            t.invoices.push(InvoiceRecord {
                tenant: "acme".into(),
                month: "2026-09".into(),
                stripe_invoice_id: Some("in_mock".into()),
                status: "draft".into(),
                payment_failed: false,
                currency: "usd".into(),
                amount_minor: 4320,
                number: None,
                hosted_invoice_url: None,
                items_added: false,
                created_at: now,
                finalized_at: None,
                paid_at: None,
                updated_at: now,
                last_error: None,
            });
        }
        let v = invoice_run(&fleet, "2026-09", None).await;
        assert_eq!(v["created"], 1, "{v}");
        let paths: Vec<(String, String)> = calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| (c.method.clone(), c.path.clone()))
            .collect();
        assert_eq!(
            paths,
            [
                ("GET".to_owned(), "/v1/invoiceitems".to_owned()),
                (
                    "POST".to_owned(),
                    "/v1/invoices/in_mock/finalize".to_owned()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn without_stripe_payments_are_refused_clearly() {
        let (mut fleet, _, _) = setup().await;
        fleet.signup = Arc::default();
        {
            let mut t = fleet.tenancy.lock().await;
            t.policy.signup_enabled = true;
            t.policy.signup_require_email_verification = false;
            t.policy.signup_require_payment_method = true;
        }
        // A required payment method that cannot be collected keeps sign-up closed.
        let p = fleet.tenancy.lock().await.policy.clone();
        assert!(!signup_open(&fleet, &p));
        let e = stripe_cfg(&fleet).unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert!(e.1.contains("--stripe-secret-key-file"));
        // Without a fleet token, never.
        fleet.token = None;
        fleet
            .tenancy
            .lock()
            .await
            .policy
            .signup_require_payment_method = false;
        let p = fleet.tenancy.lock().await.policy.clone();
        assert!(!signup_open(&fleet, &p));
    }
}
