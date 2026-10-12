// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Stripe for `fluxvm-agent central`: customers, monthly invoices, a Checkout session to save a payment method, and
//! webhook signature checks.
//!
//! Only plain HTTPS with form-encoded bodies (Stripe's own API shape), so no Stripe SDK is needed and every call can be
//! pointed at a local mock through [`StripeConfig::api_base`]. The secret key is sent only in the `Authorization`
//! header and is never logged, returned or written to the tenancy state.
//!
//! Amounts: central meters in micro-units of the price list's currency (see [`crate::billing`]); Stripe wants the
//! smallest unit of the currency (cents for USD, yen for JPY). [`micros_to_minor`] converts, rounding half away from
//! zero and respecting Stripe's zero-decimal and three-decimal currencies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default API endpoint.
pub const DEFAULT_API_BASE: &str = "https://api.stripe.com";
/// How far a webhook's signed timestamp may be from now.
pub const WEBHOOK_TOLERANCE_SECS: i64 = 300;

/// Where and how central talks to Stripe. Built from `--stripe-secret-key-file` / `STRIPE_SECRET_KEY` and friends.
#[derive(Clone)]
pub struct StripeConfig {
    /// `sk_live_…`, `sk_test_…` or a restricted `rk_…` key.
    pub secret_key: String,
    /// `whsec_…`, the signing secret of the webhook endpoint (`POST /stripe/webhook`). Without it, webhooks are refused.
    pub webhook_secret: Option<String>,
    /// `https://api.stripe.com`, or a local mock in tests. No trailing slash, no `/v1`.
    pub api_base: String,
    /// The console's public URL (`https://cloud.example.com`), for Checkout's return links.
    pub public_url: Option<String>,
}

impl std::fmt::Debug for StripeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeConfig")
            .field("secret_key", &"[redacted]")
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("api_base", &self.api_base)
            .field("public_url", &self.public_url)
            .finish()
    }
}

impl StripeConfig {
    /// `test` for a test-mode key, otherwise `live`; picks the dashboard links.
    pub fn mode(&self) -> &'static str {
        if self.secret_key.contains("_test_") {
            "test"
        } else {
            "live"
        }
    }

    /// The Stripe dashboard page of a customer.
    pub fn dashboard_customer_url(&self, customer: &str) -> String {
        let test = if self.mode() == "test" { "test/" } else { "" };
        format!("https://dashboard.stripe.com/{test}customers/{customer}")
    }
}

/// Reads a secret from a file (trimmed), refusing an empty one.
pub fn read_secret_file(path: &std::path::Path) -> Result<String, String> {
    let s =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let s = s.trim().to_owned();
    if s.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    Ok(s)
}

// ---- amounts ------------------------------------------------------------------------------------------------------

/// Currencies Stripe takes in whole units.
const ZERO_DECIMAL: &[&str] = &[
    "BIF", "CLP", "DJF", "GNF", "JPY", "KMF", "KRW", "MGA", "PYG", "RWF", "VND", "VUV", "XAF",
    "XOF", "XPF",
];
/// Currencies with three decimals; Stripe wants their amounts as multiples of 10.
const THREE_DECIMAL: &[&str] = &["BHD", "JOD", "KWD", "OMR", "TND"];
/// Zero-decimal in practice but sent with two decimals, as multiples of 100.
const WHOLE_IN_TWO_DECIMALS: &[&str] = &["ISK", "UGX"];

/// The number of decimals Stripe uses for `currency` (ISO 4217, any case), and the multiple an amount must be of.
pub fn currency_exponent(currency: &str) -> (u32, i64) {
    let c = currency.to_ascii_uppercase();
    if ZERO_DECIMAL.contains(&c.as_str()) {
        (0, 1)
    } else if THREE_DECIMAL.contains(&c.as_str()) {
        (3, 10)
    } else if WHOLE_IN_TWO_DECIMALS.contains(&c.as_str()) {
        (2, 100)
    } else {
        (2, 1)
    }
}

/// `a / b` rounded half away from zero (`b > 0`).
fn div_round(a: i128, b: i128) -> i128 {
    let q = a / b;
    let r = a % b;
    if r.abs() * 2 >= b { q + a.signum() } else { q }
}

/// Micro-units (1e-6) of `currency` to Stripe's smallest unit: `1_234_567` USD micros is `123` cents, the same in
/// JPY is `1` yen.
pub fn micros_to_minor(micros: i64, currency: &str) -> i64 {
    let (exp, multiple) = currency_exponent(currency);
    let per_minor = 10i128.pow(6 - exp);
    let step = per_minor * i128::from(multiple);
    (div_round(i128::from(micros), step) * i128::from(multiple)) as i64
}

/// Stripe's smallest unit back to a decimal amount, for display.
pub fn minor_to_amount(minor: i64, currency: &str) -> f64 {
    let (exp, _) = currency_exponent(currency);
    minor as f64 / 10f64.powi(exp as i32)
}

// ---- idempotency --------------------------------------------------------------------------------------------------

/// The `Idempotency-Key` for one step of one tenant's invoice for one month. The tenant's creation time is part of
/// it, so a tenant deleted and created again under the same id never collides with the old one.
pub fn invoice_idempotency_key(
    tenant: &str,
    tenant_created: DateTime<Utc>,
    month: &str,
    step: &str,
) -> String {
    format!(
        "fluxvm-invoice-{tenant}-{}-{month}-{step}",
        tenant_created.timestamp()
    )
}

/// The `Idempotency-Key` for creating a tenant's customer.
pub fn customer_idempotency_key(tenant: &str, tenant_created: DateTime<Utc>) -> String {
    format!("fluxvm-customer-{tenant}-{}", tenant_created.timestamp())
}

// ---- form encoding ------------------------------------------------------------------------------------------------

fn form_escape(s: &str, out: &mut String) {
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
}

/// `application/x-www-form-urlencoded`, with Stripe's bracketed keys (`metadata[tenant]`) left readable.
pub fn form_encode(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        form_escape(k, &mut out);
        out.push('=');
        form_escape(v, &mut out);
    }
    out
}

/// A form pair.
pub fn kv(k: &str, v: impl Into<String>) -> (String, String) {
    (k.to_owned(), v.into())
}

// ---- webhook signatures -------------------------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The `v1` signature Stripe computes: HMAC-SHA256 of `"{t}.{payload}"` with the endpoint's secret, in hex.
pub fn webhook_signature(secret: &str, t: i64, payload: &[u8]) -> String {
    let mut signed = format!("{t}.").into_bytes();
    signed.extend_from_slice(payload);
    hex(&crate::gha::hmac_sha256(secret.as_bytes(), &signed))
}

/// Checks a `Stripe-Signature` header (`t=…,v1=…[,v1=…]`) against the raw body: one `v1` must match and `t` must be
/// within [`WEBHOOK_TOLERANCE_SECS`] of `now`.
pub fn verify_webhook(
    secret: &str,
    header: &str,
    payload: &[u8],
    now: DateTime<Utc>,
) -> Result<(), &'static str> {
    let mut t: Option<i64> = None;
    let mut sigs: Vec<&str> = Vec::new();
    for part in header.split(',') {
        let Some((k, v)) = part.trim().split_once('=') else {
            continue;
        };
        match k {
            "t" => t = v.parse().ok(),
            "v1" => sigs.push(v),
            _ => {}
        }
    }
    let t = t.ok_or("the Stripe-Signature header has no timestamp")?;
    if sigs.is_empty() {
        return Err("the Stripe-Signature header has no v1 signature");
    }
    // Saturating: `t` comes from the request, and `i64::MIN` must not overflow.
    if now.timestamp().saturating_sub(t).saturating_abs() > WEBHOOK_TOLERANCE_SECS {
        return Err("the webhook's timestamp is too far from now");
    }
    let expected = webhook_signature(secret, t, payload);
    if sigs
        .iter()
        .any(|s| constant_time_eq(s.as_bytes(), expected.as_bytes()))
    {
        Ok(())
    } else {
        Err("the webhook signature does not match")
    }
}

// ---- invoices -----------------------------------------------------------------------------------------------------

/// One tenant's invoice for one month, as central keeps track of it (`fleet-tenancy.json`, `invoices`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvoiceRecord {
    pub tenant: String,
    /// `2026-10`.
    pub month: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe_invoice_id: Option<String>,
    /// Stripe's status: `draft`, `open`, `paid`, `void` or `uncollectible`.
    pub status: String,
    /// The last payment attempt failed (the invoice stays `open` until paid or voided).
    #[serde(default)]
    pub payment_failed: bool,
    /// Lowercase ISO code, as Stripe writes it.
    pub currency: String,
    /// Total in the currency's smallest unit.
    pub amount_minor: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    /// Stripe's page where the tenant pays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_invoice_url: Option<String>,
    /// Every line was added to the draft (a re-run must not add them again).
    #[serde(default)]
    pub items_added: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finalized_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paid_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    /// Why the last step failed, for the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl InvoiceRecord {
    /// The record as the API shows it.
    pub fn public_json(&self) -> Value {
        serde_json::json!({
            "tenant": self.tenant,
            "month": self.month,
            "id": self.stripe_invoice_id,
            "number": self.number,
            "status": self.status,
            "payment_failed": self.payment_failed,
            "currency": self.currency.to_ascii_uppercase(),
            "amount_minor": self.amount_minor,
            "amount": minor_to_amount(self.amount_minor, &self.currency),
            "hosted_invoice_url": self.hosted_invoice_url,
            "created_at": self.created_at,
            "finalized_at": self.finalized_at,
            "paid_at": self.paid_at,
            "last_error": self.last_error,
        })
    }

    /// Open and not paid.
    pub fn unpaid(&self) -> bool {
        self.status == "open"
    }

    /// Takes what Stripe says about the invoice: status, number, link, total.
    pub fn apply_stripe(&mut self, inv: &Value, now: DateTime<Utc>) {
        // Stripe does not order its webhooks: a late `invoice.finalized` (open) must not undo `invoice.paid`.
        let rank = |s: &str| match s {
            "draft" => 0,
            "open" => 1,
            "uncollectible" => 2,
            "paid" | "void" => 3,
            _ => 1,
        };
        if let Some(s) = inv
            .get("status")
            .and_then(Value::as_str)
            .filter(|s| rank(*s) >= rank(&self.status))
        {
            if s == "paid" && self.status != "paid" {
                self.paid_at = Some(now);
                self.payment_failed = false;
            }
            if s != "draft" && self.finalized_at.is_none() {
                self.finalized_at = Some(now);
            }
            self.status = s.to_owned();
        }
        if let Some(n) = inv.get("number").and_then(Value::as_str) {
            self.number = Some(n.to_owned());
        }
        if let Some(u) = inv.get("hosted_invoice_url").and_then(Value::as_str)
            && u.starts_with("https://")
        {
            self.hosted_invoice_url = Some(u.to_owned());
        }
        if let Some(t) = inv.get("total").and_then(Value::as_i64) {
            self.amount_minor = t;
        }
        self.updated_at = now;
    }
}

// ---- the client ---------------------------------------------------------------------------------------------------

/// A failed Stripe call. The message is Stripe's own (`error.message`) or the transport error; never the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for StripeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "Stripe answered {s}: {}", self.message),
            None => write!(f, "could not reach Stripe: {}", self.message),
        }
    }
}

impl std::error::Error for StripeError {}

/// Calls the Stripe API with one config and one HTTP client.
pub struct Client<'a> {
    pub http: &'a reqwest::Client,
    pub cfg: &'a StripeConfig,
}

impl Client<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}/v1{path}", self.cfg.api_base.trim_end_matches('/'))
    }

    async fn finish(resp: Result<reqwest::Response, reqwest::Error>) -> Result<Value, StripeError> {
        let resp = resp.map_err(|e| StripeError {
            status: None,
            // Without the URL: it is harmless, but the message reads better without it.
            message: e.without_url().to_string(),
        })?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("no error message")
                .chars()
                .take(500)
                .collect();
            return Err(StripeError {
                status: Some(status.as_u16()),
                message,
            });
        }
        Ok(body)
    }

    /// `POST /v1{path}` with a form body; `idempotency` makes a retry return the first answer instead of acting again.
    pub async fn post(
        &self,
        path: &str,
        form: &[(String, String)],
        idempotency: Option<&str>,
    ) -> Result<Value, StripeError> {
        let mut req = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.cfg.secret_key)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("user-agent", "fluxvm-central")
            .timeout(std::time::Duration::from_secs(20))
            .body(form_encode(form));
        if let Some(k) = idempotency {
            req = req.header("idempotency-key", k);
        }
        Self::finish(req.send().await).await
    }

    /// `GET /v1{path}`.
    pub async fn get(&self, path: &str) -> Result<Value, StripeError> {
        let req = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.cfg.secret_key)
            .header("user-agent", "fluxvm-central")
            .timeout(std::time::Duration::from_secs(20));
        Self::finish(req.send().await).await
    }
}

/// A Stripe object id from a response (`cus_…`, `in_…`), checked so it can go into a URL path.
pub fn object_id(v: &Value) -> Option<String> {
    v.get("id")
        .and_then(Value::as_str)
        .filter(|s| valid_object_id(s))
        .map(str::to_owned)
}

/// Stripe ids are short and alphanumeric with underscores.
pub fn valid_object_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn micros_become_cents_and_round_half_away_from_zero() {
        assert_eq!(micros_to_minor(55_200_000, "USD"), 5520);
        assert_eq!(micros_to_minor(1_234_567, "usd"), 123);
        assert_eq!(micros_to_minor(1_235_000, "USD"), 124); // half a cent rounds up
        assert_eq!(micros_to_minor(1_234_999, "EUR"), 123);
        assert_eq!(micros_to_minor(4_999, "USD"), 0);
        assert_eq!(micros_to_minor(5_000, "USD"), 1);
        assert_eq!(micros_to_minor(-5_000, "USD"), -1);
        assert_eq!(micros_to_minor(0, "USD"), 0);
    }

    #[test]
    fn zero_decimal_currencies_are_whole_units() {
        // 1,234.5 yen is 1,235 yen; JPY has no cents.
        assert_eq!(micros_to_minor(1_234_500_000, "JPY"), 1235);
        assert_eq!(micros_to_minor(1_234_499_999, "jpy"), 1234);
        assert_eq!(micros_to_minor(700_000, "KRW"), 1);
        assert_eq!(currency_exponent("JPY"), (0, 1));
        assert_eq!(minor_to_amount(1235, "JPY"), 1235.0);
        assert_eq!(minor_to_amount(5520, "USD"), 55.2);
    }

    #[test]
    fn three_decimal_and_special_currencies() {
        // 1.2345 KWD is 1234.5 fils, sent as a multiple of 10: 1230.
        assert_eq!(micros_to_minor(1_234_500, "KWD"), 1230);
        assert_eq!(micros_to_minor(1_235_000, "KWD"), 1240);
        // ISK: two decimals, whole krónur.
        assert_eq!(micros_to_minor(1_499_999, "ISK"), 100);
        assert_eq!(micros_to_minor(1_500_000, "ISK"), 200);
    }

    #[test]
    fn idempotency_keys_are_stable_per_tenant_month_and_step() {
        let t = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        let a = invoice_idempotency_key("acme", t, "2026-10", "invoice");
        assert_eq!(a, invoice_idempotency_key("acme", t, "2026-10", "invoice"));
        assert_eq!(
            a,
            format!("fluxvm-invoice-acme-{}-2026-10-invoice", t.timestamp())
        );
        assert_ne!(a, invoice_idempotency_key("acme", t, "2026-11", "invoice"));
        assert_ne!(
            a,
            invoice_idempotency_key("globex", t, "2026-10", "invoice")
        );
        assert_ne!(a, invoice_idempotency_key("acme", t, "2026-10", "item-0"));
        // The same id, created again later, is a different tenant.
        let later = t + chrono::Duration::days(3);
        assert_ne!(
            a,
            invoice_idempotency_key("acme", later, "2026-10", "invoice")
        );
        assert!(a.len() <= 255);
        assert_ne!(
            customer_idempotency_key("acme", t),
            customer_idempotency_key("acme", later)
        );
    }

    #[test]
    fn forms_encode_like_stripe_expects() {
        let f = form_encode(&[
            kv("metadata[fluxvm_tenant]", "acme"),
            kv("description", "Mac studio-1 lease, 30 h & more"),
            kv("email", "a+b@x.io"),
        ]);
        assert_eq!(
            f,
            "metadata%5Bfluxvm_tenant%5D=acme&description=Mac+studio-1+lease%2C+30+h+%26+more&email=a%2Bb%40x.io"
        );
    }

    #[test]
    fn webhook_signatures_verify_and_reject() {
        let secret = "whsec_test_123";
        let body = br#"{"id":"evt_1","type":"invoice.paid"}"#;
        let now = Utc.with_ymd_and_hms(2026, 10, 12, 12, 0, 0).unwrap();
        let t = now.timestamp() - 10;
        let sig = webhook_signature(secret, t, body);
        let header = format!("t={t},v1={sig}");
        assert_eq!(verify_webhook(secret, &header, body, now), Ok(()));
        // Another v1 (a rolled secret) and a v0 next to the right one are fine.
        let header2 = format!("t={t},v1={},v0=abc,v1={sig}", "0".repeat(64));
        assert_eq!(verify_webhook(secret, &header2, body, now), Ok(()));
        // A changed body, a wrong secret, an old or future timestamp, a missing part: refused.
        assert!(verify_webhook(secret, &header, br#"{"id":"evt_2"}"#, now).is_err());
        assert!(verify_webhook("whsec_other", &header, body, now).is_err());
        let old = now.timestamp() - WEBHOOK_TOLERANCE_SECS - 1;
        let h_old = format!("t={old},v1={}", webhook_signature(secret, old, body));
        assert!(verify_webhook(secret, &h_old, body, now).is_err());
        let future = now.timestamp() + WEBHOOK_TOLERANCE_SECS + 1;
        let h_future = format!("t={future},v1={}", webhook_signature(secret, future, body));
        assert!(verify_webhook(secret, &h_future, body, now).is_err());
        assert!(verify_webhook(secret, &format!("v1={sig}"), body, now).is_err());
        assert!(verify_webhook(secret, &format!("t={t}"), body, now).is_err());
        assert!(verify_webhook(secret, "", body, now).is_err());
        let extreme = format!("t={},v1={sig}", i64::MIN);
        assert!(verify_webhook(secret, &extreme, body, now).is_err());
        // Known vector: HMAC-SHA256("secret", "1.{}").
        assert_eq!(webhook_signature("secret", 1, b"{}").len(), 64);
    }

    #[test]
    fn records_follow_stripe() {
        let now = Utc.with_ymd_and_hms(2026, 11, 1, 1, 0, 0).unwrap();
        let mut r = InvoiceRecord {
            tenant: "acme".into(),
            month: "2026-10".into(),
            stripe_invoice_id: Some("in_1".into()),
            status: "draft".into(),
            payment_failed: true,
            currency: "usd".into(),
            amount_minor: 0,
            number: None,
            hosted_invoice_url: None,
            items_added: true,
            created_at: now,
            finalized_at: None,
            paid_at: None,
            updated_at: now,
            last_error: None,
        };
        r.apply_stripe(
            &serde_json::json!({"status": "open", "number": "ABC-0001", "total": 5520,
                "hosted_invoice_url": "https://invoice.stripe.com/i/x"}),
            now,
        );
        assert_eq!(r.status, "open");
        assert!(r.unpaid() && r.finalized_at.is_some());
        assert_eq!(r.public_json()["amount"], 55.2);
        assert_eq!(r.public_json()["currency"], "USD");
        // A link that is not https is not kept.
        r.apply_stripe(
            &serde_json::json!({"hosted_invoice_url": "javascript:alert(1)"}),
            now,
        );
        assert_eq!(
            r.hosted_invoice_url.as_deref(),
            Some("https://invoice.stripe.com/i/x")
        );
        r.apply_stripe(&serde_json::json!({"status": "paid"}), now);
        assert!(!r.unpaid() && !r.payment_failed && r.paid_at.is_some());
        // A late `invoice.finalized` (webhooks come in any order) does not make it unpaid again.
        r.apply_stripe(&serde_json::json!({"status": "open"}), now);
        assert_eq!(r.status, "paid");
    }

    #[test]
    fn config_never_prints_its_secrets() {
        let c = StripeConfig {
            secret_key: "sk_test_abc".into(),
            webhook_secret: Some("whsec_abc".into()),
            api_base: DEFAULT_API_BASE.into(),
            public_url: None,
        };
        let d = format!("{c:?}");
        assert!(!d.contains("sk_test_abc") && !d.contains("whsec_abc"));
        assert_eq!(c.mode(), "test");
        assert_eq!(
            c.dashboard_customer_url("cus_1"),
            "https://dashboard.stripe.com/test/customers/cus_1"
        );
        assert!(
            valid_object_id("in_1AbC") && !valid_object_id("in_1/../x") && !valid_object_id("")
        );
    }
}
