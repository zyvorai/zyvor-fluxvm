// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The support bundle: one `.tar.gz` a maintainer can read, with the secrets taken out.
//!
//! This file is the safe part, and it does no I/O of its own except building the archive in memory:
//!
//! * [`redact`] is a pure function: text in, text out, every secret pattern replaced by `[REDACTED]`. It is idempotent
//!   (redacting redacted text changes nothing), which is what makes the final scan meaningful.
//! * [`find_leaks`] says which rules still match a piece of text. Nothing legitimate matches after [`redact`].
//! * [`is_excluded_name`] names files that never enter a bundle at all (kubeconfigs, private keys, `.env`, ...).
//! * [`Bundle`] collects redacted text files, builds the archive (mode 0600 entries), then [`verify_archive`] reads the
//!   finished bytes back and scans every file again. If anything still matches, [`Bundle::finish`] returns
//!   [`BundleError::SecretLeft`] and no archive is produced. It fails closed.
//!
//! What gathers the facts lives in `support_collect`; the CLI (`fluxctl support-bundle`) and the daemon
//! (`POST /v1/support-bundle`) both use it, so the two archives are the same.

use regex::{Captures, Regex};
use std::io::Read;
use std::sync::LazyLock;

/// What a removed secret is replaced with.
pub const MARK: &str = "[REDACTED]";
const MARK_KEY: &str = "[REDACTED PRIVATE KEY]";

/// The line printed after the path of a finished bundle.
pub const REVIEW_NOTE: &str =
    "Review before sharing; secrets are removed but check for anything private.";

struct Rule {
    name: &'static str,
    re: Regex,
    /// Replacement for the whole match when the regex has no `v` group.
    marker: &'static str,
}

fn rule(name: &'static str, pattern: &str) -> Rule {
    Rule {
        name,
        re: Regex::new(pattern).unwrap_or_else(|e| panic!("support-bundle rule {name}: {e}")),
        marker: MARK,
    }
}

fn whole(name: &'static str, pattern: &str, marker: &'static str) -> Rule {
    Rule {
        marker,
        ..rule(name, pattern)
    }
}

/// A value after a key: a double-quoted string, a single-quoted string, or a bare word. A bare word may not start with
/// `{` or `[` (a nested object or list), so JSON stays valid.
const VALUE: &str = r#"(?P<v>"[^"\n]*"|'[^'\n]*'|[^\s"'{\[][^\s"']*)"#;

/// Words that make a key a secret: `password`, `x-api-key`, `AWS_SECRET_ACCESS_KEY`, `client-key-data`, ...
const SECRET_WORDS: &str = "password|passwd|passphrase|pwd|secret|token|api[_-]?key|apikey|private[_-]?key|credential|client[_-]?key|key[_-]?data|certificate[_-]?data|access[_-]?key|account[_-]?key|signing[_-]?key|auth[_-]?key|bearer|cookie|session[_-]?id|totp|psk|pre[_-]?shared";

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // Order matters: the first rules remove the largest, most structured secrets.
        whole(
            "private-key-block",
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY[A-Z ]*-----.*?(?:-----END [A-Z0-9 ]*PRIVATE KEY[A-Z ]*-----|\z)",
            MARK_KEY,
        ),
        whole("age-secret-key", r"AGE-SECRET-KEY-1[A-Z0-9]{20,}", MARK),
        rule(
            "http-credential-header",
            r#"(?im)(?:^|[\s{,"'])(?:proxy-authorization|authorization|cookie|set-cookie|x-api-key|x-auth-token|x-amz-security-token|x-fluxvm-totp|x-fluxvm-token|x-vault-token)["']?[ \t]*[:=][ \t]*(?P<v>"[^"\n]*"|'[^'\n]*'|[^\r\n]+)"#,
        ),
        rule(
            "bearer-token",
            r"(?i)\bbearer[ \t]+(?P<v>[A-Za-z0-9._~+/=\-]{8,})",
        ),
        rule(
            "url-password",
            r#"(?P<pre>[A-Za-z][A-Za-z0-9+.\-]*://)[^\s/:@"']*:(?P<v>[^\s/@"']+)@"#,
        ),
        rule(
            "url-query-secret",
            r#"(?i)[?&][a-z0-9_\-]*(?:token|secret|password|passwd|apikey|api_key|api-key|signature|credential|auth|sig|key)[a-z0-9_\-]*=(?P<v>[^&\s"'#]+)"#,
        ),
        rule("docker-auth", r#"(?i)["']auth["']\s*:\s*(?P<v>"[^"\n]*")"#),
        rule(
            "secret-list",
            &format!(
                r#"(?i)["']?[a-z0-9_.\-]*(?:{SECRET_WORDS})[a-z0-9_.\-]*["']?[ \t]*[:=][ \t]*\[(?P<v>"\[REDACTED\]"|[^\]\n{{}}]+)\]"#
            ),
        ),
        rule(
            "secret-assignment",
            &format!(
                r#"(?i)["']?[a-z0-9_.\-]*(?:{SECRET_WORDS})[a-z0-9_.\-]*["']?[ \t]*[:=][ \t]*{VALUE}"#
            ),
        ),
        rule(
            "secret-flag",
            r#"(?i)(?:^|[ \t])--?[a-z0-9\-]*(?:token|password|passwd|secret|api-key|apikey|passphrase|totp|private-key|client-key)[a-z0-9\-]*[ \t]+(?P<v>"[^"\n]*"|'[^'\n]*'|[^\s"'\-][^\s"']*)"#,
        ),
        rule(
            "curl-user",
            r#"(?i)\bcurl\b[^\n]*?[ \t](?:-u|--user)[ \t]+(?P<v>"[^"\n]*"|'[^'\n]*'|[^\s"']+)"#,
        ),
        whole(
            "github-token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
            MARK,
        ),
        whole(
            "aws-access-key-id",
            r"\b(?:AKIA|ASIA|AGPA|AIDA|AROA|ANPA)[0-9A-Z]{16}\b",
            MARK,
        ),
        whole("slack-token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}", MARK),
        whole(
            "api-key-prefix",
            r"\b(?:sk-ant-[A-Za-z0-9_\-]{20,}|sk-[A-Za-z0-9]{32,}|[sr]k_(?:live|test)_[A-Za-z0-9]{16,}|AIza[0-9A-Za-z_\-]{35}|npm_[A-Za-z0-9]{30,}|dckr_pat_[A-Za-z0-9_\-]{20,}|pypi-[A-Za-z0-9_\-]{30,}|glpat-[A-Za-z0-9_\-]{20,}|hf_[A-Za-z0-9]{30,})",
            MARK,
        ),
        whole(
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]*",
            MARK,
        ),
        whole(
            "join-token",
            r"\bK10[0-9a-f]{40,}(?:::[A-Za-z0-9_\-]+:[A-Za-z0-9]+)?",
            MARK,
        ),
        whole("fluxvm-session-token", r"\bfxst_[A-Za-z0-9_\-]{8,}", MARK),
    ]
});

fn is_marker(value: &str, marker: &str) -> bool {
    let v = value.trim_matches(|c| c == '"' || c == '\'');
    v == marker || v == MARK || v == "REDACTED"
}

/// Applies one rule. Returns the new text and how many replacements actually changed something.
fn apply(r: &Rule, text: &str) -> (String, usize) {
    let mut changed = 0usize;
    let out = r.re.replace_all(text, |c: &Captures| {
        let all = c.get(0).expect("group 0");
        match c.name("v") {
            Some(v) => {
                if is_marker(v.as_str(), r.marker) {
                    return all.as_str().to_owned();
                }
                changed += 1;
                // A list is always rewritten as `["[REDACTED]"]`, so a second pass sees the marker.
                let quote = if r.name == "secret-list" {
                    Some('"')
                } else {
                    v.as_str()
                        .chars()
                        .next()
                        .filter(|q| matches!(q, '"' | '\''))
                };
                let repl = match quote {
                    Some(q) => format!("{q}{MARK}{q}"),
                    None => MARK.to_owned(),
                };
                format!(
                    "{}{}{}",
                    &text[all.start()..v.start()],
                    repl,
                    &text[v.end()..all.end()]
                )
            }
            None => {
                changed += 1;
                r.marker.to_owned()
            }
        }
    });
    (out.into_owned(), changed)
}

/// Removes every secret this module knows how to recognise. Pure and idempotent. Also returns how many were removed.
pub fn redact_counted(text: &str) -> (String, usize) {
    let mut cur = text.to_owned();
    let mut total = 0;
    for r in RULES.iter() {
        let (next, n) = apply(r, &cur);
        cur = next;
        total += n;
    }
    (cur, total)
}

/// [`redact_counted`] without the count.
pub fn redact(text: &str) -> String {
    redact_counted(text).0
}

/// The names of the rules that still find a secret in `text`. Empty for anything [`redact`] has produced.
pub fn find_leaks(text: &str) -> Vec<&'static str> {
    RULES
        .iter()
        .filter(|r| apply(r, text).1 > 0)
        .map(|r| r.name)
        .collect()
}

/// Names of every rule, for the docs and for a test that each one is exercised.
pub fn rule_names() -> Vec<&'static str> {
    RULES.iter().map(|r| r.name).collect()
}

/// Files that never enter a bundle, whatever they contain: kubeconfigs, private keys, certificates stores, `.env`,
/// credential files, and anything under `.ssh`, `.kube`, `.gnupg`, `.aws`. `path` may be a bare name or a full path.
pub fn is_excluded_name(path: &str) -> bool {
    let p = path.replace('\\', "/").to_ascii_lowercase();
    let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty()).collect();
    let Some(name) = comps.last().copied() else {
        return false;
    };
    if comps[..comps.len() - 1].iter().any(|c| {
        matches!(
            *c,
            ".ssh" | ".kube" | ".gnupg" | ".aws" | ".docker" | "secrets"
        )
    }) {
        return true;
    }
    const SUFFIXES: [&str; 12] = [
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        ".jks",
        ".keystore",
        ".kdbx",
        ".ovpn",
        ".p8",
        ".token",
        ".gpg",
        ".asc",
    ];
    if SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    const EXACT: [&str; 10] = [
        ".netrc",
        ".npmrc",
        ".pgpass",
        ".htpasswd",
        ".git-credentials",
        "token",
        "secrets",
        "kubeconfig",
        "config.json.bak",
        "authorized_keys",
    ];
    if EXACT.contains(&name) {
        return true;
    }
    name.contains("kubeconfig")
        || name.starts_with(".env")
        || name.starts_with("credentials")
        || name.starts_with("secret")
        || (name.starts_with("id_") && !name.ends_with(".pub"))
        || (name.starts_with("fluxvm-")
            && (name.ends_with(".yaml") || name.ends_with(".yml"))
            && comps.contains(&".kube"))
}

/// Why a bundle could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleError {
    /// A secret pattern was still present in `file` after redaction. Nothing was written. `rules` names the patterns,
    /// never the value.
    SecretLeft {
        file: String,
        rules: Vec<&'static str>,
    },
    /// Building or reading the archive failed.
    Archive(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SecretLeft { file, rules } => write!(
                f,
                "no bundle was written: the final check still found something that looks like a secret in {file} ({}). Nothing left this machine. Run `fluxctl explain SUPPORT_BUNDLE_SECRET_LEFT`, then send the output of `fluxctl cluster status` and `fluxctl doctor` by hand instead.",
                rules.join(", ")
            ),
            Self::Archive(e) => write!(
                f,
                "the support bundle could not be built: {e}. Run `fluxctl explain SUPPORT_BUNDLE_WRITE_FAILED` for what to try."
            ),
        }
    }
}

impl std::error::Error for BundleError {}

/// A folder name safe to use inside an archive: no `..`, no absolute path, no odd characters.
pub fn safe_entry_name(name: &str) -> String {
    let parts: Vec<String> = name
        .replace('\\', "/")
        .split('/')
        .filter(|p| !p.is_empty() && *p != "." && *p != "..")
        .map(|p| {
            p.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        })
        .collect();
    parts.join("/")
}

/// Collects redacted text files and builds the archive.
#[derive(Default)]
pub struct Bundle {
    files: Vec<(String, String, usize)>,
    skipped: Vec<(String, String)>,
}

impl Bundle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a text file, redacted. A name that [`is_excluded_name`] is skipped (and said so in the manifest).
    pub fn add_text(&mut self, name: &str, text: &str) {
        let name = safe_entry_name(name);
        if name.is_empty() || is_excluded_name(&name) {
            self.skip(
                &name,
                "files of this kind can hold credentials and are never included",
            );
            return;
        }
        let (clean, n) = redact_counted(text);
        self.files.push((name, clean, n));
    }

    /// Records something that was not collected, and why. The reason is redacted too.
    pub fn skip(&mut self, what: &str, reason: &str) {
        self.skipped.push((redact(what), redact(reason)));
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn skipped(&self) -> &[(String, String)] {
        &self.skipped
    }

    fn manifest(&self, generated: &str, root: &str) -> String {
        let mut s =
            format!("FluxVM support bundle\ngenerated: {generated}\nfolder: {root}\n\nIncluded:\n");
        for (n, t, r) in &self.files {
            s.push_str(&format!(
                "  {n}  ({} bytes, {r} secret(s) removed)\n",
                t.len()
            ));
        }
        if self.skipped.is_empty() {
            s.push_str("\nSkipped: nothing.\n");
        } else {
            s.push_str("\nSkipped (and why):\n");
            for (w, why) in &self.skipped {
                s.push_str(&format!("  {w}: {why}\n"));
            }
        }
        s
    }

    /// Builds the `.tar.gz` (every entry mode 0600) under the folder `root`, then reads it back and scans it. Returns the
    /// bytes only when the scan is clean.
    pub fn finish(self, root: &str, generated: &str, mtime: u64) -> Result<Vec<u8>, BundleError> {
        let root = safe_entry_name(root);
        let root = if root.is_empty() {
            "fluxvm-support".to_owned()
        } else {
            root
        };
        let manifest = self.manifest(generated, &root);
        let readme = format!(
            "{REVIEW_NOTE}\n\nThis archive was made by `fluxctl support-bundle`. Tokens, passwords, private keys, cookies, authorization headers, join tokens and kubeconfig credentials are removed, and kubeconfigs and key files are never included. MANIFEST.txt lists every file and what was skipped. Open the files and look before you send this to anyone.\n"
        );
        let mut all: Vec<(String, String)> = vec![
            ("README.txt".into(), readme),
            ("MANIFEST.txt".into(), manifest),
        ];
        all.extend(self.files.into_iter().map(|(n, t, _)| (n, t)));
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        for (name, text) in &all {
            let mut h = tar::Header::new_gnu();
            h.set_size(text.len() as u64);
            h.set_mode(0o600);
            h.set_mtime(mtime);
            h.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut h, format!("{root}/{name}"), text.as_bytes())
                .map_err(|e| BundleError::Archive(e.to_string()))?;
        }
        let gz = tar
            .into_inner()
            .map_err(|e| BundleError::Archive(e.to_string()))?;
        let bytes = gz
            .finish()
            .map_err(|e| BundleError::Archive(e.to_string()))?;
        verify_archive(&bytes)?;
        Ok(bytes)
    }
}

/// The final check: reads a finished `.tar.gz`, and fails closed if any file in it still matches a secret pattern, has an
/// excluded name, or cannot be read as text. Returns the number of files.
pub fn verify_archive(bytes: &[u8]) -> Result<usize, BundleError> {
    const CAP: u64 = 512 * 1024 * 1024;
    let gz = flate2::read::GzDecoder::new(bytes);
    let mut ar = tar::Archive::new(gz.take(CAP));
    let entries = ar
        .entries()
        .map_err(|e| BundleError::Archive(e.to_string()))?;
    let mut n = 0;
    for e in entries {
        let mut e = e.map_err(|e| BundleError::Archive(e.to_string()))?;
        let name = e
            .path()
            .map_err(|e| BundleError::Archive(e.to_string()))?
            .to_string_lossy()
            .into_owned();
        if is_excluded_name(&name) {
            return Err(BundleError::SecretLeft {
                file: name,
                rules: vec!["excluded-file-name"],
            });
        }
        let mut raw = Vec::new();
        e.read_to_end(&mut raw)
            .map_err(|e| BundleError::Archive(e.to_string()))?;
        let text = String::from_utf8_lossy(&raw);
        let rules = find_leaks(&text);
        if !rules.is_empty() {
            return Err(BundleError::SecretLeft { file: name, rules });
        }
        n += 1;
    }
    Ok(n)
}

/// Keeps the last `n` lines of `text`.
pub fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    let mut out = lines[start..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// A safe projection of a VM record: identity, state and size, not the request body (which can carry environment
/// variables, cloud-init user data and keys).
pub fn project_vm(vm: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    let mut out = Map::new();
    for k in [
        "id",
        "name",
        "status",
        "backend",
        "created_at",
        "expires_at",
        "error",
        "guest_ip",
        "pid",
        "log_path",
    ] {
        if let Some(v) = vm.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    if let Some(req) = vm.get("request") {
        let mut r = Map::new();
        for k in ["vcpus", "memory_mib", "image", "backend", "disk_gib"] {
            if let Some(v) = req.get(k) {
                r.insert(k.into(), v.clone());
            }
        }
        out.insert("request".into(), Value::Object(r));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Realistic leaked-secret samples. None of these is a live credential.
    const PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACD3nqY1l4Q0o7S9xGZV8Qp2m9m3C7sHn7mH1vWq0rK3dQAAAJh8z1Ycf\n-----END OPENSSH PRIVATE KEY-----";
    const JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4ifQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    const K3S: &str =
        "K10a1b2c3d4e5f60718293a4b5c6d7e8f9012345678901234567890abcdef12345678::server:7f3a9b2c1d";

    fn gone(input: &str, secret: &str) {
        let out = redact(input);
        assert!(!out.contains(secret), "secret survived: {out}");
        assert_eq!(redact(&out), out, "not idempotent: {out}");
        assert!(find_leaks(&out).is_empty(), "scan still finds: {out}");
        assert!(out.contains("[REDACTED"), "no marker in: {out}");
    }

    #[test]
    fn a_private_key_block_is_removed_entirely() {
        let log = format!("loading key\n{PEM}\nkey loaded\n");
        let out = redact(&log);
        assert!(!out.contains("b3BlbnNzaC1rZXk"));
        assert!(!out.contains("BEGIN OPENSSH"));
        assert!(out.contains("loading key") && out.contains("key loaded"));
        assert_eq!(redact(&out), out);
    }

    #[test]
    fn every_pem_flavour_is_removed() {
        for kind in [
            "RSA PRIVATE KEY",
            "EC PRIVATE KEY",
            "PRIVATE KEY",
            "ENCRYPTED PRIVATE KEY",
            "PGP PRIVATE KEY BLOCK",
        ] {
            let s = format!(
                "x\n-----BEGIN {kind}-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END {kind}-----\ny"
            );
            let out = redact(&s);
            assert!(!out.contains("MIIEvQ"), "{kind}: {out}");
        }
    }

    #[test]
    fn a_truncated_private_key_is_removed_to_the_end() {
        let s = "tail of log\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gun\nVTLw7onLRnrq0/IzW7yWR7QkrmBL7jTKEn5u";
        let out = redact(s);
        assert!(!out.contains("MIIEow") && !out.contains("VTLw7"));
    }

    #[test]
    fn a_public_certificate_is_not_a_secret() {
        let s = "-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIEAgAAuTANBg\n-----END CERTIFICATE-----";
        assert_eq!(redact(s), s);
    }

    #[test]
    fn authorization_headers_lose_their_value() {
        gone(
            "Authorization: Bearer abc123def456ghi789jkl",
            "abc123def456ghi789jkl",
        );
        gone(
            "authorization: Basic dXNlcjpwYXNzd29yZA==",
            "dXNlcjpwYXNzd29yZA",
        );
        gone(
            "Proxy-Authorization: Negotiate YIIB8gYGKwYBBQUCoIIB5jCC",
            "YIIB8gYGKwYB",
        );
        gone(
            "> Authorization: token ghp_16C7e42F292c6912E7710c838347Ae178B4a",
            "16C7e42F292c",
        );
        gone(
            r#"{"headers": {"Authorization": "Bearer sk_live_abc"}}"#,
            "sk_live_abc",
        );
    }

    #[test]
    fn cookies_and_api_key_headers_are_removed() {
        gone("Cookie: session=9f8e7d6c5b4a; theme=dark", "9f8e7d6c5b4a");
        gone(
            "Set-Cookie: fluxvm_session=abcdef0123456789; Path=/; HttpOnly",
            "abcdef0123456789",
        );
        gone(
            "X-Api-Key: 0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef",
        );
        gone("X-FluxVM-Totp: 123456", "123456");
    }

    #[test]
    fn a_bearer_token_in_prose_is_removed() {
        gone(
            "curl failed with header 'bearer eyABCDEFGH12345' sent",
            "eyABCDEFGH12345",
        );
    }

    #[test]
    fn passwords_in_urls_are_removed_but_the_user_and_host_stay() {
        let out = redact("pulling https://deploy:hunter2hunter2@registry.example.com/v2/app");
        assert!(!out.contains("hunter2"));
        assert!(out.contains("deploy") && out.contains("registry.example.com"));
        gone("postgres://app:S3cr3t!pw@db.internal:5432/app", "S3cr3t!pw");
        gone("redis://:onlypassword@cache:6379/0", "onlypassword");
        assert_eq!(
            redact("ssh://git@github.com/org/repo.git"),
            "ssh://git@github.com/org/repo.git"
        );
        assert_eq!(
            redact("listening on http://127.0.0.1:7788/healthz"),
            "listening on http://127.0.0.1:7788/healthz"
        );
    }

    #[test]
    fn secrets_in_query_strings_are_removed() {
        gone(
            "GET /v1/events?access_token=abcdef123456&follow=1",
            "abcdef123456",
        );
        gone(
            "GET /download?X-Amz-Signature=deadbeefcafe0123&expires=9",
            "deadbeefcafe0123",
        );
        gone("wss://host/term?token=tok_0123456789", "tok_0123456789");
        let out = redact("GET /v1/vms?all=true&limit=5");
        assert_eq!(out, "GET /v1/vms?all=true&limit=5");
    }

    #[test]
    fn key_value_pairs_in_every_common_syntax_are_removed() {
        gone("password = \"p@ssw0rd-long\"", "p@ssw0rd-long");
        gone("PASSWORD=p@ssw0rd-long", "p@ssw0rd-long");
        gone("db_password: p@ssw0rd-long", "p@ssw0rd-long");
        gone(
            r#"{"password":"p@ssw0rd-long","user":"root"}"#,
            "p@ssw0rd-long",
        );
        gone("FLUXVM_TOKEN=tkn_abcdef0123456789", "tkn_abcdef0123456789");
        gone(
            "export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "wJalrXUtnFEMI",
        );
        gone(
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "wJalrXUtnFEMI",
        );
        gone("client_secret: 'abcdEFGH-1234'", "abcdEFGH-1234");
        gone("api_key=0123456789abcdef", "0123456789abcdef");
        gone(
            "DefaultEndpointsProtocol=https;AccountName=x;AccountKey=Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==;",
            "Eby8vdM02xNOcqFlqUwJ",
        );
    }

    #[test]
    fn json_stays_valid_after_redaction() {
        let s = r#"{"auth":{"tokens":[{"token":"abc-secret-123","role":"admin"}]},"listen":"127.0.0.1:7788","password":"x1y2z3w4"}"#;
        let out = redact(s);
        assert!(!out.contains("abc-secret-123") && !out.contains("x1y2z3w4"));
        let v: serde_json::Value = serde_json::from_str(&out).expect("still JSON");
        assert_eq!(v["listen"], "127.0.0.1:7788");
        assert_eq!(v["auth"]["tokens"][0]["role"], "admin");
    }

    #[test]
    fn secret_lists_are_removed() {
        gone(
            r#"{"api_tokens": ["tok-aaaaaaaa", "tok-bbbbbbbb"]}"#,
            "tok-aaaaaaaa",
        );
        gone("secrets: [alpha-secret-1, beta-secret-2]", "beta-secret-2");
    }

    #[test]
    fn a_kubeconfig_that_leaks_into_a_log_loses_its_credentials() {
        let kc = "apiVersion: v1\nclusters:\n- cluster:\n    certificate-authority-data: LS0tLS1CRUdJTiBDRVJU\n    server: https://10.0.0.5:6443\n  name: fluxvm\nusers:\n- name: admin\n  user:\n    client-certificate-data: LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCk1JSUM\n    client-key-data: LS0tLS1CRUdJTiBSU0EgUFJJVkFURSBLRVktLS0tLQpNSUlFcEFJQkFBS0NBUUVB\n    token: 6b3f1d0e8a7c4b2f9e5d\n";
        let out = redact(kc);
        for s in [
            "LS0tLS1CRUdJTiBSU0EgUFJJVkFURSBLRVktLS0tLQ",
            "LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0",
            "6b3f1d0e8a7c4b2f9e5d",
        ] {
            assert!(!out.contains(s), "{s} survived:\n{out}");
        }
        assert!(out.contains("https://10.0.0.5:6443"));
        assert_eq!(redact(&out), out);
    }

    #[test]
    fn command_line_flags_are_removed() {
        gone(
            "exec: fluxctl --server-token tkn_abcdef0123456789 ps",
            "tkn_abcdef0123456789",
        );
        gone(
            "k3s agent --token K10deadbeef --server https://x:6443",
            "K10deadbeef",
        );
        gone("run --password 'my pass phrase' --name x", "my pass phrase");
        gone("tool --api-key=0123456789abcdef", "0123456789abcdef");
        gone(
            "curl -s -u admin:s3cretpass https://example.com/api",
            "s3cretpass",
        );
        gone(
            "curl --user admin:s3cretpass https://example.com/api",
            "s3cretpass",
        );
        assert_eq!(
            redact("fluxctl ps --all --name web"),
            "fluxctl ps --all --name web"
        );
    }

    #[test]
    fn join_tokens_are_removed_wherever_they_appear() {
        gone(&format!("to join, use {K3S} on the new node"), "7f3a9b2c1d");
        gone(
            "kubeadm join 10.0.0.1:6443 --token abcdef.0123456789abcdef --discovery-token-ca-cert-hash sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "abcdef.0123456789abcdef",
        );
        gone("K3S_TOKEN=whatever-it-is", "whatever-it-is");
        gone("fxst_ab12cd34_9f8e7d6c5b4a3210", "9f8e7d6c5b4a3210");
    }

    #[test]
    fn well_known_token_formats_are_removed() {
        gone(
            "github ghp_16C7e42F292c6912E7710c838347Ae178B4a failed",
            "16C7e42F292c",
        );
        gone(
            "using github_pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz",
            "11ABCDEFG0123456789",
        );
        gone("key AKIAIOSFODNN7EXAMPLE rejected", "AKIAIOSFODNN7EXAMPLE");
        gone("slack xoxb-123456789012-abcdefghijkl", "abcdefghijkl");
        gone(
            "anthropic sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123",
            "abcdefghijklmnopqrstuvwxyz0123",
        );
        gone(
            "openai sk-abcdefghijklmnopqrstuvwxyz0123456789ABCD",
            "abcdefghijklmnopqrstuvwxyz0123456789ABCD",
        );
        // built at run time so no literal Stripe-shaped key sits in the source (GitHub push protection rejects those)
        gone(
            &format!("stripe {}{}", "sk_live_", "4eC39HqLyjWDarjtT1zdp7dc"),
            "4eC39HqLyjWDarjtT1zdp7dc",
        );
        gone(
            "gcp AIzaSyA-1234567890abcdefghijklmnopqrstuv",
            "1234567890abcdefghijklmnopqrstuv",
        );
        gone(
            &format!("session {JWT} expired"),
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        );
        gone(
            "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ",
            "QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ",
        );
        gone("glab glpat-abcdefghij0123456789", "abcdefghij0123456789");
    }

    #[test]
    fn docker_registry_auth_is_removed() {
        gone(
            r#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYXNzd29yZA=="}}}"#,
            "dXNlcjpwYXNzd29yZA",
        );
    }

    #[test]
    fn ordinary_log_lines_are_untouched() {
        for s in [
            "2026-11-22T10:00:01Z INFO vm 3f2a started in 3.2s on tap0",
            "daemon listening on 127.0.0.1:7788, state_dir=/var/lib/fluxvm",
            "qemu-system-x86_64 -m 4096 -smp 2 -drive file=/var/lib/fluxvm/vms/a/disk.qcow2",
            "disk free 212 GiB, memory available 41200 MiB",
            "kernel 6.8.0-45-generic x86_64 Ubuntu 24.04",
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "contact admin@example.com about vm web-1",
        ] {
            assert_eq!(redact(s), s, "changed: {s}");
            assert!(find_leaks(s).is_empty(), "{s}");
        }
    }

    #[test]
    fn redaction_is_idempotent_on_a_pile_of_everything() {
        let all = format!(
            "{PEM}\nAuthorization: Bearer abcdefghijkl\npassword=abc123xyz\nhttps://u:p4ss@h/x?token=zzzzzz1\n{JWT}\n{K3S}\n--token tkn123456\n"
        );
        let once = redact(&all);
        assert_eq!(redact(&once), once);
        assert!(find_leaks(&once).is_empty());
        assert!(find_leaks(&all).len() >= 6);
    }

    #[test]
    fn the_count_matches_what_was_removed() {
        let (out, n) =
            redact_counted("password=abcdef12\nAuthorization: Bearer xxxxxxxxxx\nnothing here");
        assert!(n >= 2, "{n}");
        assert_eq!(redact_counted(&out).1, 0);
    }

    #[test]
    fn find_leaks_names_the_rule_not_the_value() {
        let l = find_leaks("password=supersecretvalue");
        assert!(l.contains(&"secret-assignment"));
        assert!(l.iter().all(|n| !n.contains("supersecret")));
    }

    #[test]
    fn every_rule_is_exercised_by_a_sample() {
        let samples = format!(
            "{PEM}\nAGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQ\nAuthorization: x1y2z3w4v5\nbearer abcdefghij\nhttps://u:pw12345@h/\n/p?api_key=abcd1234\n\"auth\": \"YWJjZGVm\"\ntokens: [aaaaaaaa]\npassword=abcdef12\n run --token abcdef123\ncurl -u a:b https://x\nghp_16C7e42F292c6912E7710c838347Ae178B4a\nAKIAIOSFODNN7EXAMPLE\nxoxb-123456789012-abcdefghijkl\nsk-ant-api03-abcdefghijklmnopqrstuvwxyz0123\n{JWT}\n{K3S}\nfxst_ab12cd34_9f8e7d6c5b4a3210\n"
        );
        let found = find_leaks(&samples);
        for n in rule_names() {
            assert!(found.contains(&n), "rule {n} was not exercised");
        }
    }

    #[test]
    fn excluded_names_cover_credentials_and_keys() {
        for n in [
            "kubeconfig",
            "fluxvm-dev.kubeconfig",
            "admin.kubeconfig.yaml",
            "server.pem",
            "tls.key",
            "id_rsa",
            "id_ed25519",
            ".env",
            ".env.production",
            "credentials",
            "credentials.json",
            "secrets.yaml",
            ".netrc",
            ".npmrc",
            "cert.p12",
            "store.pfx",
            "x/.ssh/config",
            "home/.kube/fluxvm-a.yaml",
            "/root/.aws/config",
            "vault.token",
            "token",
            "authorized_keys",
        ] {
            assert!(is_excluded_name(n), "{n} should be excluded");
        }
        for n in [
            "versions.json",
            "logs/daemon.log",
            "cluster-health.json",
            "id_rsa.pub",
            "config-effective.json",
            "instances.json",
            "host.txt",
        ] {
            assert!(!is_excluded_name(n), "{n} should be allowed");
        }
    }

    #[test]
    fn a_bundle_skips_an_excluded_file_and_says_so() {
        let mut b = Bundle::new();
        b.add_text("home/.kube/fluxvm-a.yaml", "token: abc");
        b.add_text("host.txt", "ok");
        assert_eq!(b.file_count(), 1);
        assert_eq!(b.skipped().len(), 1);
    }

    #[test]
    fn entry_names_cannot_escape_the_folder() {
        assert_eq!(safe_entry_name("../../etc/passwd"), "etc/passwd");
        assert_eq!(safe_entry_name("/abs/path.log"), "abs/path.log");
        assert_eq!(safe_entry_name("logs/a b?.log"), "logs/a_b_.log");
        assert_eq!(safe_entry_name("..\\..\\x"), "x");
    }

    fn read_all(bytes: &[u8]) -> Vec<(String, String, u32)> {
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        ar.entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let name = e.path().unwrap().to_string_lossy().into_owned();
                let mode = e.header().mode().unwrap();
                let mut s = String::new();
                e.read_to_string(&mut s).unwrap();
                (name, s, mode)
            })
            .collect()
    }

    #[test]
    fn a_finished_bundle_is_clean_private_and_lists_what_it_skipped() {
        let mut b = Bundle::new();
        b.add_text(
            "logs/daemon.log",
            &format!("start\npassword=hunter2hunter2\n{JWT}\n"),
        );
        b.add_text("versions.json", "{\"fluxctl\":\"0.4.21\"}");
        b.skip("logs/vm-x.log", "the VM log file is not on this machine");
        let bytes = b
            .finish("fluxvm-support-t", "2026-11-22T10:00:00Z", 1_800_000_000)
            .expect("clean");
        let files = read_all(&bytes);
        let names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        assert!(names.contains(&"fluxvm-support-t/README.txt"));
        assert!(names.contains(&"fluxvm-support-t/MANIFEST.txt"));
        assert!(names.contains(&"fluxvm-support-t/logs/daemon.log"));
        for (n, text, mode) in &files {
            assert_eq!(*mode & 0o777, 0o600, "{n}");
            assert!(
                !text.contains("hunter2hunter2") && !text.contains("SflKxwRJ"),
                "{n}"
            );
        }
        let manifest = &files
            .iter()
            .find(|f| f.0.ends_with("MANIFEST.txt"))
            .unwrap()
            .1;
        assert!(manifest.contains("logs/vm-x.log: the VM log file is not on this machine"));
        assert!(manifest.contains("secret(s) removed"));
        let readme = &files
            .iter()
            .find(|f| f.0.ends_with("README.txt"))
            .unwrap()
            .1;
        assert!(readme.contains(REVIEW_NOTE));
        assert_eq!(verify_archive(&bytes).unwrap(), 4);
    }

    #[test]
    fn the_final_scan_fails_closed_on_a_leaky_archive() {
        // Build an archive by hand that bypasses `redact`, as a bug in a collector would.
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let body = "log line\nauthorization: Bearer 0123456789abcdefghij\n";
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o600);
        tar.append_data(&mut h, "x/logs/daemon.log", body.as_bytes())
            .unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        match verify_archive(&bytes) {
            Err(BundleError::SecretLeft { file, rules }) => {
                assert_eq!(file, "x/logs/daemon.log");
                assert!(!rules.is_empty());
            }
            other => panic!("expected a leak, got {other:?}"),
        }
    }

    #[test]
    fn the_final_scan_rejects_an_excluded_file_name() {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut h = tar::Header::new_gnu();
        h.set_size(2);
        h.set_mode(0o600);
        tar.append_data(&mut h, "x/kubeconfig", &b"ok"[..]).unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        assert!(matches!(
            verify_archive(&bytes),
            Err(BundleError::SecretLeft { .. })
        ));
    }

    #[test]
    fn garbage_is_an_archive_error_not_a_pass() {
        assert!(matches!(
            verify_archive(b"not a gzip"),
            Err(BundleError::Archive(_))
        ));
    }

    #[test]
    fn the_error_messages_say_what_to_do_next() {
        let leak = BundleError::SecretLeft {
            file: "a".into(),
            rules: vec!["jwt"],
        }
        .to_string();
        assert!(
            leak.contains("SUPPORT_BUNDLE_SECRET_LEFT")
                && leak.contains("Nothing left this machine")
        );
        let arch = BundleError::Archive("disk full".into()).to_string();
        assert!(arch.contains("SUPPORT_BUNDLE_WRITE_FAILED") && arch.contains("disk full"));
    }

    #[test]
    fn skip_reasons_are_redacted_too() {
        let mut b = Bundle::new();
        b.skip(
            "daemon",
            "GET failed: Authorization: Bearer abcdefghijklmnop was refused",
        );
        assert!(!b.skipped()[0].1.contains("abcdefghijklmnop"));
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines("a\nb\nc\nd\n", 2), "c\nd\n");
        assert_eq!(last_lines("a\nb", 5), "a\nb\n");
        assert_eq!(last_lines("", 3), "");
    }

    #[test]
    fn a_vm_record_is_projected_without_its_request_body() {
        let vm = serde_json::json!({
            "id": "1", "name": "web", "status": "running", "backend": "qemu", "log_path": "/s/web/console.log",
            "request": {"vcpus": 2, "memory_mib": 2048, "image": "ubuntu", "env": {"DB_PASSWORD": "x"}, "user_data": "#cloud-config\npassword: y"}
        });
        let p = project_vm(&vm);
        assert_eq!(p["request"]["vcpus"], 2);
        assert!(p["request"].get("env").is_none() && p["request"].get("user_data").is_none());
        assert!(!p.to_string().contains("DB_PASSWORD"));
    }
}
