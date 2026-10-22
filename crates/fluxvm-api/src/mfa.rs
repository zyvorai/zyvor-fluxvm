// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Step-up TOTP (RFC 6238) for static API tokens named in `[[auth.mfa]]`.
//!
//! A code is read from `X-Fluxvm-Totp`. Each time-step is accepted once per
//! token (replay protection), ±1 step of clock skew is tolerated, and repeated
//! failures lock the token out briefly.

use fluxvm_core::config::{AuthConfig, MfaRule, MfaScope};
use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

pub const HEADER: &str = "x-fluxvm-totp";
const STEP_SECS: u64 = 30;
const SKEW_STEPS: i64 = 1;
const MAX_FAILURES: u32 = 5;
const LOCKOUT_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No rule applies to this caller/request.
    NotRequired,
    Ok,
    Missing,
    Failed,
    LockedOut,
}

impl Outcome {
    pub fn label(self) -> &'static str {
        match self {
            Outcome::NotRequired => "none",
            Outcome::Ok => "ok",
            Outcome::Missing => "missing",
            Outcome::Failed => "failed",
            Outcome::LockedOut => "locked",
        }
    }
}

#[derive(Default)]
struct TokenState {
    last_step: Option<u64>,
    failures: u32,
    window_start: u64,
}

fn state() -> &'static Mutex<HashMap<String, TokenState>> {
    static S: OnceLock<Mutex<HashMap<String, TokenState>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Guest command execution and interactive shells: `/agent`, `/qga/exec`
/// (VMs and sandboxes) and agent-session `/exec`, `/pty-ticket`, `/pty`.
fn is_exec_path(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    let vm = p.starts_with("/v1/vms/") || p.starts_with("/v1/sandboxes/");
    let session = p.starts_with("/v1/agents/sessions/");
    (vm && (p.ends_with("/agent") || p.ends_with("/qga/exec") || p.ends_with("/exec")))
        || (session && (p.ends_with("/exec") || p.ends_with("/pty-ticket") || p.ends_with("/pty")))
}

fn scope_applies(scope: MfaScope, method: &str, path: &str) -> bool {
    // `GET .../pty` upgrades to an interactive shell, so it is exec despite
    // being a GET.
    let is_pty = path.trim_end_matches('/').ends_with("/pty");
    if (method == "GET" || method == "HEAD") && !is_pty {
        return false;
    }
    match scope {
        MfaScope::Exec => is_exec_path(path),
        MfaScope::Write => true,
    }
}

/// Decodes RFC 4648 base32 (case-insensitive, padding/whitespace ignored).
pub fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut nbits = 0u32;
    let mut out = Vec::new();
    for c in input.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u32 - 'A' as u32,
            c @ '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        bits = (bits << 5) | v;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
            bits &= (1 << nbits) - 1;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// HOTP value (RFC 4226) for `counter`, 6 digits.
pub fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac =
        <Hmac<Sha1> as KeyInit>::new_from_slice(secret).expect("HMAC takes any key length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
    bin % 1_000_000
}

/// The time-step (counter) at which `code` is valid within skew, if any.
fn matching_step(secret: &[u8], code: u32, now: u64) -> Option<u64> {
    let cur = (now / STEP_SECS) as i64;
    (-SKEW_STEPS..=SKEW_STEPS)
        .map(|d| cur + d)
        .filter(|s| *s >= 0)
        .map(|s| s as u64)
        .find(|s| hotp(secret, *s) == code)
}

fn read_secret(rule: &MfaRule) -> Option<Vec<u8>> {
    let raw = std::fs::read_to_string(&rule.totp_secret_file).ok()?;
    base32_decode(raw.trim())
}

/// Checks `code` for `rule`, applying replay protection and lockout.
fn verify(rule: &MfaRule, code: Option<&str>, now: u64) -> Outcome {
    let mut map = state().lock().unwrap_or_else(|e| e.into_inner());
    let st = map.entry(rule.token.clone()).or_default();
    if now.saturating_sub(st.window_start) >= LOCKOUT_SECS {
        st.failures = 0;
        st.window_start = now;
    }
    if st.failures >= MAX_FAILURES {
        return Outcome::LockedOut;
    }
    let Some(code) = code.map(str::trim).filter(|c| !c.is_empty()) else {
        return Outcome::Missing;
    };
    let ok = (|| {
        let code: u32 = code.parse().ok().filter(|_| code.len() == 6)?;
        // An unreadable secret file fails closed.
        let secret = read_secret(rule)?;
        let step = matching_step(&secret, code, now)?;
        if st.last_step.is_some_and(|last| step <= last) {
            return None; // replay or older step
        }
        Some(step)
    })();
    match ok {
        Some(step) => {
            st.last_step = Some(step);
            st.failures = 0;
            Outcome::Ok
        }
        None => {
            st.failures += 1;
            Outcome::Failed
        }
    }
}

/// Entry point for the auth middleware.
pub fn enforce(
    auth: &AuthConfig,
    token_name: Option<&str>,
    method: &str,
    path: &str,
    code: Option<&str>,
) -> Outcome {
    let Some(rule) = token_name.and_then(|n| auth.mfa_rule(n)) else {
        return Outcome::NotRequired;
    };
    if !scope_applies(rule.require, method, path) {
        return Outcome::NotRequired;
    }
    verify(rule, code, now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6238 appendix B, SHA-1 secret "12345678901234567890", 8 digits there;
    // the low 6 digits must match.
    #[test]
    fn hotp_matches_rfc_6238_vectors() {
        let secret = b"12345678901234567890";
        assert_eq!(hotp(secret, 59 / 30), 287082); // 94287082
        assert_eq!(hotp(secret, 1111111109 / 30), 81804); // 07081804
        assert_eq!(hotp(secret, 2000000000 / 30), 279037); // 69279037
    }

    #[test]
    fn base32_round_trip_known_value() {
        // "12345678901234567890" in base32
        let d = base32_decode("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        assert_eq!(d, b"12345678901234567890");
        assert!(base32_decode("not base32!").is_none());
        assert!(base32_decode("").is_none());
    }

    fn rule(name: &str, scope: MfaScope) -> (MfaRule, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("totp");
        std::fs::write(&f, "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ\n").unwrap();
        (
            MfaRule {
                token: name.into(),
                totp_secret_file: f.to_string_lossy().into(),
                require: scope,
            },
            dir,
        )
    }

    const SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn code_accepted_once_then_replay_rejected() {
        let (r, _d) = rule("t-replay", MfaScope::Exec);
        let now = 1_000_000_020;
        let code = format!("{:06}", hotp(SECRET, now / 30));
        assert_eq!(verify(&r, Some(&code), now), Outcome::Ok);
        assert_eq!(verify(&r, Some(&code), now), Outcome::Failed);
    }

    #[test]
    fn skew_of_one_step_ok_two_steps_rejected() {
        let (r, _d) = rule("t-skew", MfaScope::Exec);
        let now = 2_000_000_010;
        let late = format!("{:06}", hotp(SECRET, now / 30 - 1));
        assert_eq!(verify(&r, Some(&late), now), Outcome::Ok);
        let (r2, _d2) = rule("t-skew2", MfaScope::Exec);
        let far = format!("{:06}", hotp(SECRET, now / 30 - 2));
        assert_eq!(verify(&r2, Some(&far), now), Outcome::Failed);
    }

    #[test]
    fn missing_code_does_not_count_as_failure_but_wrong_codes_lock_out() {
        let (r, _d) = rule("t-lock", MfaScope::Exec);
        let now = 3_000_000_000;
        assert_eq!(verify(&r, None, now), Outcome::Missing);
        for _ in 0..MAX_FAILURES {
            assert_eq!(verify(&r, Some("000000x"), now), Outcome::Failed);
        }
        let good = format!("{:06}", hotp(SECRET, now / 30));
        assert_eq!(verify(&r, Some(&good), now), Outcome::LockedOut);
        // lockout expires; a code for the new time is accepted
        let later = now + LOCKOUT_SECS;
        let fresh = format!("{:06}", hotp(SECRET, later / 30));
        assert_eq!(verify(&r, Some(&fresh), later), Outcome::Ok);
    }

    #[test]
    fn unreadable_secret_fails_closed() {
        let r = MfaRule {
            token: "t-nofile".into(),
            totp_secret_file: "/nonexistent/totp".into(),
            require: MfaScope::Exec,
        };
        assert_eq!(verify(&r, Some("123456"), 4_000_000_000), Outcome::Failed);
    }

    #[test]
    fn scope_matching() {
        assert!(scope_applies(MfaScope::Exec, "POST", "/v1/vms/abc/agent"));
        assert!(scope_applies(
            MfaScope::Exec,
            "POST",
            "/v1/vms/abc/qga/exec"
        ));
        assert!(!scope_applies(MfaScope::Exec, "POST", "/v1/vms/abc/pause"));
        assert!(!scope_applies(MfaScope::Exec, "GET", "/v1/vms/abc/agent"));
        assert!(scope_applies(MfaScope::Write, "DELETE", "/v1/vms/abc"));
        assert!(!scope_applies(MfaScope::Write, "GET", "/v1/vms"));
        assert!(scope_applies(
            MfaScope::Exec,
            "POST",
            "/v1/agents/sessions/s1/exec"
        ));
        assert!(scope_applies(
            MfaScope::Exec,
            "GET",
            "/v1/agents/sessions/s1/pty"
        ));
        assert!(scope_applies(
            MfaScope::Exec,
            "POST",
            "/v1/agents/sessions/s1/pty-ticket"
        ));
    }

    #[test]
    fn enforce_ignores_unlisted_tokens() {
        let auth = AuthConfig::default();
        assert_eq!(
            enforce(&auth, Some("ci"), "POST", "/v1/vms/x/agent", None),
            Outcome::NotRequired
        );
    }
}
