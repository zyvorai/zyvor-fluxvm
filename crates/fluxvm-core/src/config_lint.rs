// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Static checks of a FluxVM host config file, for `fluxctl config check`.
//!
//! [`Config::load`] is lenient on purpose: unknown keys are ignored and every field has a default. That also means a
//! misspelled or misplaced key (`[tls] trust_proxy_header = true`, `[sandbox.procbox] ...` keys written under
//! `[sandbox]`) silently leaves the default in place, and combinations that can never work (`tls.cert` without
//! `tls.key`, `auth.require` without a token) only fail at serve time or, worse, quietly turn a protection off.
//! This module reports those, each with the key it concerns and what to change. It never changes the config.
//!
//! Severity: `Error` = the daemon will not do what the file says (or will not start); `Warning` = legal but
//! probably unintended or weakening a protection; `Info` = how an unusual setting behaves.
//!
//! Checks that look at the host (does this file exist) go through [`LintEnv::exists`], so they can be switched off
//! (`fluxctl config check --offline`, for a config written on another machine) and tested without a filesystem.
//! Secrets (token values, credential-vault authorization strings, inline registry passwords) are never echoed.

use crate::config::{Config, DataplaneMode, Role, is_loopback_listen};
use base64::Engine;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub severity: Severity,
    /// Dotted key the finding is about (`tls.cert`, `auth.tokens[1].token`), or `(file)` for a syntax error.
    pub key: String,
    pub message: String,
    /// What to change, when there is one obvious change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// What the checks may ask about the machine they run on.
pub struct LintEnv<'a> {
    /// `None` skips every "does this path exist" check.
    pub exists: Option<&'a dyn Fn(&Path) -> bool>,
}

impl LintEnv<'_> {
    /// No host access: the file-existence checks are skipped.
    pub const OFFLINE: LintEnv<'static> = LintEnv { exists: None };

    fn missing(&self, p: &Path) -> bool {
        self.exists.is_some_and(|f| !f(p))
    }
}

struct Out(Vec<Finding>);

impl Out {
    fn add(
        &mut self,
        severity: Severity,
        key: &str,
        message: impl Into<String>,
        fix: Option<&str>,
    ) {
        self.0.push(Finding {
            severity,
            key: key.to_owned(),
            message: message.into(),
            fix: fix.map(str::to_owned),
        });
    }
    fn err(&mut self, key: &str, message: impl Into<String>, fix: &str) {
        self.add(Severity::Error, key, message, Some(fix));
    }
    fn warn(&mut self, key: &str, message: impl Into<String>, fix: &str) {
        self.add(Severity::Warning, key, message, Some(fix));
    }
    fn info(&mut self, key: &str, message: impl Into<String>) {
        self.add(Severity::Info, key, message, None);
    }
}

/// Checks the text of a config file: syntax, type errors, unknown keys, then every semantic check on the result.
/// Findings are sorted most severe first, then by key.
pub fn lint_str(raw: &str, env: &LintEnv<'_>) -> Vec<Finding> {
    let mut out = Out(Vec::new());
    let value: toml::Value = match toml::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            out.err(
                "(file)",
                format!("not valid TOML: {}", one_line(&e.to_string())),
                "fix the syntax at the line shown",
            );
            return out.0;
        }
    };
    let cfg: Config = match value.clone().try_into() {
        Ok(c) => c,
        Err(e) => {
            out.err(
                "(file)",
                format!("the daemon cannot load this: {}", one_line(&e.to_string())),
                "correct the value type or name shown",
            );
            return out.0;
        }
    };
    for (path, hint) in unknown_keys(&value, &cfg) {
        let (msg, fix) = match hint {
            Some(h) => (
                format!("unknown key, ignored by the daemon (did you mean `{h}`?)"),
                Some(format!("rename it to `{h}`")),
            ),
            None => (
                "unknown key, ignored by the daemon".to_owned(),
                Some("remove it".to_owned()),
            ),
        };
        out.add(Severity::Warning, &path, msg, fix.as_deref());
    }
    let mut all = out.0;
    all.extend(lint_config(&cfg, env));
    sort(&mut all);
    all
}

/// Semantic checks of a loaded config.
pub fn lint_config(cfg: &Config, env: &LintEnv<'_>) -> Vec<Finding> {
    let mut o = Out(Vec::new());
    listen(cfg, &mut o);
    tls(cfg, env, &mut o);
    auth(cfg, env, &mut o);
    policy(cfg, &mut o);
    sandbox(cfg, &mut o);
    procbox(cfg, &mut o);
    dataplane(cfg, &mut o);
    jailer(cfg, env, &mut o);
    catalog(cfg, env, &mut o);
    misc(cfg, env, &mut o);
    let mut v = o.0;
    sort(&mut v);
    v
}

fn sort(v: &mut [Finding]) {
    v.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.key.cmp(&b.key)));
}

fn one_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn valid_host_port(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0)
}

fn listen(cfg: &Config, o: &mut Out) {
    if !valid_host_port(&cfg.listen) {
        o.err(
            "listen",
            format!("{:?} is not host:port", cfg.listen),
            "use for example \"127.0.0.1:7788\" or \"0.0.0.0:7788\"",
        );
    }
    for (key, v) in [
        (
            "sandbox.egress_proxy_listen",
            &cfg.sandbox.egress_proxy_listen,
        ),
        (
            "sandbox.egress_transparent_listen",
            &cfg.sandbox.egress_transparent_listen,
        ),
    ] {
        if v.is_empty() {
            continue;
        }
        if !valid_host_port(v) {
            o.err(
                key,
                format!("{v:?} is not host:port"),
                "use host:port, or empty to turn it off",
            );
        } else if !is_loopback_listen(v) {
            o.warn(
                key,
                format!("{v} listens beyond loopback: any host that can reach it can use the egress proxy"),
                "bind 127.0.0.1 (or the sandbox bridge address) unless that is intended",
            );
        }
    }
}

fn tls(cfg: &Config, env: &LintEnv<'_>, o: &mut Out) {
    let t = &cfg.tls;
    match (&t.cert, &t.key) {
        (Some(_), None) => o.err(
            "tls.key",
            "tls.cert is set without tls.key: TLS stays off and the API serves plain HTTP",
            "set tls.key",
        ),
        (None, Some(_)) => o.err(
            "tls.cert",
            "tls.key is set without tls.cert: TLS stays off and the API serves plain HTTP",
            "set tls.cert",
        ),
        _ => {}
    }
    if t.client_ca.is_some() && !t.enabled() {
        o.err(
            "tls.client_ca",
            "tls.client_ca is set but TLS is off (needs tls.cert and tls.key): client certificates are NOT required",
            "set tls.cert and tls.key, or remove tls.client_ca",
        );
    }
    for (key, p) in [
        ("tls.cert", &t.cert),
        ("tls.key", &t.key),
        ("tls.client_ca", &t.client_ca),
    ] {
        if let Some(p) = p
            && env.missing(p)
        {
            o.err(
                key,
                format!("{} does not exist", p.display()),
                "fix the path or create the file",
            );
        }
    }
    if t.trust_proxy_headers {
        if t.mtls_enabled() {
            o.info("tls.trust_proxy_headers", "ignored for connections this process terminates mTLS on: their identity always comes from the certificate");
        } else if !is_loopback_listen(&cfg.listen) {
            o.warn(
                "tls.trust_proxy_headers",
                "X-Client-Cert-* headers are trusted while the API listens beyond loopback: any client that can reach it directly can claim any role or tenant",
                "listen on 127.0.0.1 behind the proxy, or firewall the port so only the proxy connects",
            );
        }
    }
}

fn auth(cfg: &Config, env: &LintEnv<'_>, o: &mut Out) {
    let a = &cfg.auth;
    if a.require && !a.has_credentials() {
        o.err(
            "auth.require",
            "auth.require is true but there are no [[auth.tokens]] and no OIDC: every request is refused with 401",
            "add a [[auth.tokens]] entry or set oidc_issuer and oidc_audience",
        );
    }
    if !is_loopback_listen(&cfg.listen) && !a.has_credentials() && !a.require {
        o.warn(
            "auth",
            "listening beyond loopback with no tokens or OIDC: mutating routes are refused (fail closed) and the API is unusable",
            "add a [[auth.tokens]] entry",
        );
    }
    if !is_loopback_listen(&cfg.listen) && a.has_credentials() && !cfg.tls.enabled() {
        o.warn(
            "tls",
            "bearer tokens are sent over plain HTTP on a non-loopback listener",
            "set tls.cert and tls.key, or terminate TLS in front and keep listen on loopback",
        );
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    let mut names: HashMap<&str, usize> = HashMap::new();
    for (i, t) in a.tokens.iter().enumerate() {
        let key = format!("auth.tokens[{i}].token");
        if t.token.is_empty() {
            o.err(
                &key,
                "empty token value",
                "generate one, for example `openssl rand -hex 32`",
            );
        } else {
            if t.token.len() < 16 {
                o.warn(
                    &key,
                    "token is shorter than 16 characters and easy to guess",
                    "use `openssl rand -hex 32`",
                );
            }
            if let Some(first) = seen.insert(t.token.as_str(), i) {
                o.err(
                    &key,
                    format!("same value as auth.tokens[{first}]: only one role can win"),
                    "give each token its own value",
                );
            }
        }
        if let Some(n) = t.name.as_deref().filter(|n| !n.is_empty())
            && let Some(first) = names.insert(n, i)
        {
            o.warn(&format!("auth.tokens[{i}].name"), format!("name {n:?} is already used by auth.tokens[{first}]; audit logs and MFA rules cannot tell them apart"), "use unique names");
        }
    }
    if !a.tokens.is_empty() && !a.oidc_enabled() && !a.tokens.iter().any(|t| t.role == Role::Admin)
    {
        o.warn(
            "auth.tokens",
            "no token has role = \"admin\": nobody can create or change VMs",
            "give one token role = \"admin\"",
        );
    }
    let (iss, aud) = (
        a.oidc_issuer.as_deref().is_some_and(|s| !s.is_empty()),
        a.oidc_audience.as_deref().is_some_and(|s| !s.is_empty()),
    );
    if iss != aud {
        o.err(
            if iss {
                "auth.oidc_audience"
            } else {
                "auth.oidc_issuer"
            },
            "OIDC needs both oidc_issuer and oidc_audience; with one missing it is silently off",
            "set both or neither",
        );
    }
    if let Some(i) = a.oidc_issuer.as_deref()
        && !i.is_empty()
        && !i.starts_with("https://")
    {
        o.warn(
            "auth.oidc_issuer",
            format!(
                "issuer {i:?} is not https: discovery and keys can be tampered with in transit"
            ),
            "use the https issuer URL",
        );
    }
    match (a.rate_limit_rps, a.rate_limit_burst) {
        (Some(_), None) => o.err(
            "auth.rate_limit_burst",
            "rate_limit_rps without rate_limit_burst",
            "set both",
        ),
        (None, Some(_)) => o.err(
            "auth.rate_limit_rps",
            "rate_limit_burst without rate_limit_rps",
            "set both",
        ),
        (Some(r), Some(b)) if !(r > 0.0 && r.is_finite()) || b == 0 => {
            o.err(
                "auth.rate_limit_rps",
                "rate limit values must be positive (rate_limit_rps > 0, rate_limit_burst >= 1)",
                "use positive values or remove both",
            );
        }
        _ => {}
    }
    for (i, m) in a.mfa.iter().enumerate() {
        let key = format!("auth.mfa[{i}]");
        if !a
            .tokens
            .iter()
            .any(|t| t.name.as_deref() == Some(m.token.as_str()))
        {
            o.err(
                &format!("{key}.token"),
                format!(
                    "no [[auth.tokens]] has name = {:?}: this MFA rule never applies",
                    m.token
                ),
                "match a token's name",
            );
        }
        if m.totp_secret_file.is_empty() {
            o.err(
                &format!("{key}.totp_secret_file"),
                "empty path",
                "point at the file holding the base32 secret",
            );
        } else if env.missing(Path::new(&m.totp_secret_file)) {
            o.err(
                &format!("{key}.totp_secret_file"),
                format!("{} does not exist", m.totp_secret_file),
                "fix the path",
            );
        }
    }
    let tenants: BTreeSet<&str> = cfg
        .policy
        .tenants
        .iter()
        .map(|t| t.tenant.as_str())
        .collect();
    if !tenants.is_empty() {
        for (i, t) in a.tokens.iter().enumerate() {
            if let Some(tn) = t.tenant.as_deref()
                && !tenants.contains(tn)
            {
                o.warn(&format!("auth.tokens[{i}].tenant"), format!("tenant {tn:?} has no [[policy.tenants]] entry: its VMs get no aggregate caps"), "add a [[policy.tenants]] entry or accept it");
            }
        }
    }
}

fn policy(cfg: &Config, o: &mut Out) {
    let p = &cfg.policy;
    for (key, v) in [
        ("policy.max_vcpus", p.max_vcpus.map(u64::from)),
        ("policy.max_memory_mib", p.max_memory_mib),
        ("policy.max_disk_gib", p.max_disk_gib),
        ("policy.max_ttl_seconds", p.max_ttl_seconds),
    ] {
        if v == Some(0) {
            o.warn(
                key,
                "0 refuses every VM create",
                "remove the key for no limit, or set a positive cap",
            );
        }
    }
    if p.allowed_backends.as_ref().is_some_and(Vec::is_empty) {
        o.warn(
            "policy.allowed_backends",
            "an empty list refuses every backend; omit the key to allow all",
            "remove it or list backends",
        );
    }
    if p.allowed_network_modes.as_ref().is_some_and(Vec::is_empty) {
        o.warn(
            "policy.allowed_network_modes",
            "an empty list refuses every network mode; omit the key to allow all",
            "remove it or list modes",
        );
    }
    if p.allowed_image_dirs.as_ref().is_some_and(Vec::is_empty) {
        o.warn(
            "policy.allowed_image_dirs",
            "an empty list refuses every image path; omit the key to allow all",
            "remove it or list directories",
        );
    }
    let mut seen = BTreeSet::new();
    for (i, t) in p.tenants.iter().enumerate() {
        if !seen.insert(t.tenant.as_str()) {
            o.err(
                &format!("policy.tenants[{i}].tenant"),
                format!("tenant {:?} is listed twice", t.tenant),
                "merge the entries",
            );
        }
        if t.max_vcpus_total == Some(0)
            || t.max_memory_mib_total == Some(0)
            || t.max_vms_total == Some(0)
        {
            o.warn(
                &format!("policy.tenants[{i}]"),
                format!("a 0 cap blocks every VM of tenant {:?}", t.tenant),
                "remove the cap or set a positive one",
            );
        }
    }
    if let (Some(host), Some(vm)) = (p.max_memory_mib_host, p.max_memory_mib)
        && vm > host
    {
        o.warn(
            "policy.max_memory_mib",
            format!("per-VM cap {vm} MiB is above the host cap {host} MiB"),
            "lower max_memory_mib or raise max_memory_mib_host",
        );
    }
    if let (Some(host), Some(vm)) = (p.max_vcpus_host, p.max_vcpus)
        && u32::from(vm) > host
    {
        o.warn(
            "policy.max_vcpus",
            format!("per-VM cap {vm} is above the host cap {host}"),
            "lower max_vcpus or raise max_vcpus_host",
        );
    }
    if p.default_cpu_quota_percent == Some(0) {
        o.err(
            "policy.default_cpu_quota_percent",
            "0 percent of a CPU would starve every VM",
            "use 1..100 or remove it",
        );
    }
}

fn sandbox(cfg: &Config, o: &mut Out) {
    let s = &cfg.sandbox;
    if s.autopause_idle_secs > 0 && s.autopause_scan_secs == 0 {
        o.err("sandbox.autopause_scan_secs", "autopause_idle_secs is set but the scan interval is 0: idle sandboxes are never paused", "set a scan interval, for example 10");
    }
    if s.idle_balloon_secs > 0 && !(1..=90).contains(&s.idle_balloon_percent) {
        o.err(
            "sandbox.idle_balloon_percent",
            format!("{} is outside 1-90", s.idle_balloon_percent),
            "use 1..90",
        );
    }
    if s.idle_balloon_secs > 0
        && s.autopause_idle_secs > 0
        && s.idle_balloon_secs >= s.autopause_idle_secs
    {
        o.warn("sandbox.idle_balloon_secs", "should be shorter than autopause_idle_secs, or the balloon never inflates before the sandbox pauses", "lower idle_balloon_secs");
    }
    if s.hibernate_idle_secs > 0
        && s.autopause_idle_secs > 0
        && s.hibernate_idle_secs <= s.autopause_idle_secs
    {
        o.warn(
            "sandbox.hibernate_idle_secs",
            "should be longer than autopause_idle_secs, or hibernation pre-empts pausing",
            "raise hibernate_idle_secs",
        );
    }
    let proxy_on = !s.egress_proxy_listen.is_empty() || !s.egress_transparent_listen.is_empty();
    if s.egress_tls_intercept && !proxy_on {
        o.warn("sandbox.egress_tls_intercept", "TLS interception needs egress_proxy_listen or egress_transparent_listen; with neither it does nothing", "set a proxy listener");
    }
    if !s.egress_transparent_listen.is_empty() && !s.egress_tls_intercept {
        o.info("sandbox.egress_transparent_listen", "HTTPS through the transparent listener is not inspected without egress_tls_intercept = true");
    }
    if s.egress_tls_intercept && s.egress_tls_allow_private {
        o.warn("sandbox.egress_tls_allow_private", "intercepted tunnels may reach loopback, private and link-local addresses, including cloud metadata", "leave it off unless the guest must reach internal services");
    }
    if !s.egress_http_rules.is_empty() && !proxy_on {
        o.warn(
            "sandbox.egress_http_rules",
            "HTTP rules are only enforced by the egress proxy, and no proxy listener is set",
            "set egress_proxy_listen",
        );
    }
    for (i, r) in s.egress_http_rules.iter().enumerate() {
        let verb = r.split_whitespace().next().unwrap_or("");
        if !matches!(verb.to_ascii_lowercase().as_str(), "allow" | "deny") {
            o.err(
                &format!("sandbox.egress_http_rules[{i}]"),
                format!("rule {r:?} must start with allow or deny"),
                "write `allow GET host/path` or `deny * host/path`",
            );
        }
    }
    if !s.credential_vault.is_empty() && !proxy_on {
        o.warn("sandbox.credential_vault", "credentials are injected by the egress proxy, and no proxy listener is set: they are never used", "set egress_proxy_listen");
    }
    for (i, c) in s.credential_vault.iter().enumerate() {
        if c.host.trim().is_empty() {
            o.err(
                &format!("sandbox.credential_vault[{i}].host"),
                "empty host",
                "name the upstream host",
            );
        }
        if c.authorization.trim().is_empty() {
            o.err(
                &format!("sandbox.credential_vault[{i}].authorization"),
                "empty authorization",
                "set the Authorization header value, or remove the entry",
            );
        }
    }
    for p in &s.egress_tls_ports {
        if *p == 0 {
            o.err(
                "sandbox.egress_tls_ports",
                "port 0 is not valid",
                "use 1..65535",
            );
        }
    }
}

fn procbox(cfg: &Config, o: &mut Out) {
    let p = &cfg.sandbox.procbox;
    if !matches!(p.isolation.as_str(), "off" | "auto" | "strict") {
        o.err(
            "sandbox.procbox.isolation",
            format!("{:?} is not off, auto or strict", p.isolation),
            "use \"off\", \"auto\" or \"strict\"",
        );
    }
    if p.default_timeout_secs > p.max_timeout_secs {
        o.err(
            "sandbox.procbox.default_timeout_secs",
            "above max_timeout_secs: every default request is refused",
            "lower the default or raise the cap",
        );
    }
    if p.default_memory_mib > p.max_memory_mib {
        o.err(
            "sandbox.procbox.default_memory_mib",
            "above max_memory_mib: every default request is refused",
            "lower the default or raise the cap",
        );
    }
    if p.uid_base.checked_add(p.uid_count).is_none() {
        o.err(
            "sandbox.procbox.uid_base",
            "uid_base + uid_count overflows a 32-bit uid",
            "lower uid_base or uid_count",
        );
    }
    if p.enabled {
        if p.uid_count > 0 && p.uid_base < 1000 {
            o.warn(
                "sandbox.procbox.uid_base",
                "the uid pool overlaps system or login accounts (below 1000)",
                "pick a range no real account uses, for example 200000",
            );
        }
        if p.best_effort {
            o.warn(
                "sandbox.procbox.best_effort",
                "fails open: a kernel that cannot enforce a protection runs the command without it",
                "leave it false in production",
            );
        }
        if p.allow_root && p.uid_count == 0 {
            o.warn(
                "sandbox.procbox.allow_root",
                "commands run as root when the uid pool is disabled",
                "set uid_count > 0 and allow_root = false",
            );
        }
        if p.isolation == "off" {
            o.info(
                "sandbox.procbox.isolation",
                "namespace isolation is off: only Landlock and seccomp confine commands",
            );
        }
    }
}

fn dataplane(cfg: &Config, o: &mut Out) {
    let d = &cfg.sandbox.dataplane;
    let base = "sandbox.dataplane";
    if d.mode == DataplaneMode::Legacy
        && (d.xdp.enabled || d.service.xdp_acceleration || d.service.cgroup_connect)
    {
        o.warn(
            &format!("{base}.mode"),
            "xdp and service-fabric options need mode = \"ebpf\"; with legacy they are ignored",
            "set mode = \"ebpf\" or turn the options off",
        );
    }
    if d.xdp.enabled && d.xdp.interface.as_deref().is_none_or(str::is_empty) {
        o.err(
            &format!("{base}.xdp.interface"),
            "xdp.enabled without an interface",
            "set xdp.interface to the uplink",
        );
    }
    if !d.xdp.block_cidrs.is_empty() && !d.xdp.enabled {
        o.warn(
            &format!("{base}.xdp.block_cidrs"),
            "block_cidrs only apply when xdp.enabled = true",
            "enable xdp or remove the list",
        );
    }
    if d.service.pressure_soft_percent >= d.service.pressure_hard_percent {
        o.err(
            &format!("{base}.service.pressure_soft_percent"),
            "soft threshold must be below the hard one",
            "lower soft or raise hard",
        );
    }
    if d.service.pressure_hard_percent > 100 {
        o.err(
            &format!("{base}.service.pressure_hard_percent"),
            "above 100 percent",
            "use 1..100",
        );
    }
    if d.service.edt_enabled && d.service.edt_interfaces.is_empty() {
        o.warn(
            &format!("{base}.service.edt_interfaces"),
            "edt_enabled with no interfaces does nothing",
            "list the interfaces",
        );
    }
    for (key, list) in [
        ("allow_cidrs", &d.allow_cidrs),
        ("xdp.block_cidrs", &d.xdp.block_cidrs),
    ] {
        for (i, c) in list.iter().enumerate() {
            if !valid_cidr(c) {
                o.err(
                    &format!("{base}.{key}[{i}]"),
                    format!("{c:?} is not a CIDR (address/prefix)"),
                    "write for example 10.0.0.0/8 or 2001:db8::/32",
                );
            }
        }
    }
    for (i, r) in d.allow_ports.iter().enumerate() {
        if !valid_port_rule(r) {
            o.err(
                &format!("{base}.allow_ports[{i}]"),
                format!("{r:?} is not proto/PORT"),
                "write for example tcp/443 or udp/53 (protocols: tcp, udp, sctp, icmp, icmp6)",
            );
        }
    }
    if d.max_egress_mbps == Some(0) || d.max_egress_pps == Some(0) {
        o.warn(
            &format!("{base}.max_egress_mbps"),
            "a ceiling of 0 is not a limit the dataplane accepts; omit the key for no ceiling",
            "remove it or use a positive value",
        );
    }
    if !matches!(
        d.qemu_egress_mode.as_str(),
        "loopback" | "allowlist" | "off" | "default" | ""
    ) {
        o.info(
            &format!("{base}.qemu_egress_mode"),
            format!(
                "{:?} is not one of the documented modes (loopback, allowlist)",
                d.qemu_egress_mode
            ),
        );
    }
}

fn valid_cidr(s: &str) -> bool {
    let Some((ip, prefix)) = s.trim().split_once('/') else {
        return false;
    };
    let (Ok(ip), Ok(prefix)) = (ip.parse::<std::net::IpAddr>(), prefix.parse::<u8>()) else {
        return false;
    };
    prefix <= if ip.is_ipv4() { 32 } else { 128 }
}

fn valid_port_rule(s: &str) -> bool {
    let Some((proto, port)) = s.split_once('/') else {
        return false;
    };
    let Ok(port) = port.trim().parse::<u16>() else {
        return false;
    };
    match proto.trim().to_ascii_lowercase().as_str() {
        "tcp" | "udp" | "sctp" => port != 0,
        "icmp" | "icmp6" | "icmpv6" => true,
        _ => false,
    }
}

fn jailer(cfg: &Config, env: &LintEnv<'_>, o: &mut Out) {
    let j = &cfg.jailer;
    if cfg.jailer_required() && !j.enabled {
        o.err("jailer.enabled", "the jailer is required (jailer.enforce, or auth.require on a non-loopback listen) but jailer.enabled is false: every Firecracker launch is refused", "set jailer.enabled = true, or stop requiring it");
    }
    if j.uid_range_start.is_some() != j.uid_range_len.is_some() {
        o.err(
            "jailer.uid_range_start",
            "uid_range_start and uid_range_len must be set together",
            "set both or neither",
        );
    }
    if j.uid_range_len == Some(0) {
        o.err(
            "jailer.uid_range_len",
            "0 leaves no uid for any tenant",
            "use a positive length",
        );
    }
    if j.enabled && (j.uid == 0 || j.gid == 0) && j.uid_range_start.is_none() {
        o.warn(
            "jailer.uid",
            "the jailer drops to uid/gid 0: that is root, not a drop",
            "use an unprivileged uid and gid",
        );
    }
    if j.enabled && env.missing(&j.chroot_base_dir) {
        o.warn(
            "jailer.chroot_base_dir",
            format!("{} does not exist", j.chroot_base_dir.display()),
            "create it",
        );
    }
}

fn catalog(cfg: &Config, env: &LintEnv<'_>, o: &mut Out) {
    let c = &cfg.catalog;
    if let Some(p) = &c.path
        && env.missing(p)
    {
        o.err(
            "catalog.path",
            format!("{} does not exist", p.display()),
            "fix the path",
        );
    }
    let mut names = BTreeSet::new();
    let mut keys = BTreeSet::new();
    for (i, s) in c.trusted_signers.iter().enumerate() {
        let key = format!("catalog.trusted_signers[{i}]");
        match base64::engine::general_purpose::STANDARD.decode(s.public_key.trim()) {
            Ok(b) if b.len() == 32 => {}
            Ok(b) => o.err(
                &format!("{key}.public_key"),
                format!("decodes to {} bytes; an Ed25519 public key is 32", b.len()),
                "paste the base64 of the 32-byte public key",
            ),
            Err(_) => o.err(
                &format!("{key}.public_key"),
                "not valid base64",
                "paste the base64 of the 32-byte public key",
            ),
        }
        if !keys.insert(s.public_key.trim().to_owned()) {
            o.warn(
                &format!("{key}.public_key"),
                "same key listed twice",
                "remove the duplicate",
            );
        }
        if !names.insert(s.name.clone()) {
            o.warn(
                &format!("{key}.name"),
                format!(
                    "name {:?} is used more than once; signed_by cannot tell them apart",
                    s.name
                ),
                "use unique names",
            );
        }
    }
    if !c.trusted_signers.is_empty() && c.path.is_none() {
        o.info("catalog.trusted_signers", "signers are set but catalog.path is not: the catalog is off, so they are not consulted");
    }
}

fn misc(cfg: &Config, env: &LintEnv<'_>, o: &mut Out) {
    for (key, p) in [("state_dir", &cfg.state_dir), ("run_dir", &cfg.run_dir)] {
        if !p.is_absolute() {
            o.warn(
                key,
                format!(
                    "{} is relative: it resolves against the daemon's working directory",
                    p.display()
                ),
                "use an absolute path",
            );
        }
    }
    let optional = [
        ("firecracker_kernel", &cfg.firecracker_kernel),
        ("fluxvm_kernel", &cfg.fluxvm_kernel),
        ("cloud_hypervisor_firmware", &cfg.cloud_hypervisor_firmware),
        ("qemu_ovmf_code", &cfg.qemu_ovmf_code),
        ("qemu_ovmf_vars_template", &cfg.qemu_ovmf_vars_template),
        ("virtio_win_dir", &cfg.virtio_win_dir),
    ];
    for (key, p) in optional {
        if let Some(p) = p
            && env.missing(p)
        {
            o.warn(
                key,
                format!("{} does not exist", p.display()),
                "fix the path",
            );
        }
    }
    for (i, c) in cfg.apple.oci_registry_credentials.iter().enumerate() {
        let key = format!("apple.oci_registry_credentials[{i}]");
        if c.registry.is_empty() {
            o.err(
                &format!("{key}.registry"),
                "empty registry",
                "name the registry host, for example ghcr.io",
            );
        }
        if !c.password.is_empty() && c.keychain_service.is_some() {
            o.warn(
                &format!("{key}.password"),
                "both an inline password and keychain_service are set",
                "keep only keychain_service",
            );
        } else if !c.password.is_empty() {
            o.warn(
                &format!("{key}.password"),
                "inline registry password in the config file",
                "store it in the Keychain and set keychain_service",
            );
        } else if c.keychain_service.is_none() {
            o.err(
                &format!("{key}.password"),
                "neither password nor keychain_service is set: pulls will be unauthenticated",
                "set one",
            );
        }
    }
}

/// Keys present in `raw` that the daemon ignores, each with a suggestion when a similar key exists in the same table
/// or the same key name exists in another table (a misplaced key). Detected by loading and re-serialising: a key the
/// round trip drops was not read.
pub fn unknown_keys(raw: &toml::Value, cfg: &Config) -> Vec<(String, Option<String>)> {
    let (Ok(inp), Ok(out)) = (serde_json::to_value(raw), serde_json::to_value(cfg)) else {
        return Vec::new();
    };
    let mut leaves: BTreeMap<String, Vec<String>> = BTreeMap::new();
    collect_names(&out, "", &mut leaves);
    let mut found = Vec::new();
    walk(&inp, &out, "", &leaves, &mut found);
    found
}

fn collect_names(v: &serde_json::Value, path: &str, into: &mut BTreeMap<String, Vec<String>>) {
    match v {
        serde_json::Value::Object(m) => {
            for (k, c) in m {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                into.entry(k.clone()).or_default().push(p.clone());
                collect_names(c, &p, into);
            }
        }
        serde_json::Value::Array(a) => {
            for c in a {
                collect_names(c, &format!("{path}[]"), into);
            }
        }
        _ => {}
    }
}

fn walk(
    inp: &serde_json::Value,
    out: &serde_json::Value,
    path: &str,
    leaves: &BTreeMap<String, Vec<String>>,
    found: &mut Vec<(String, Option<String>)>,
) {
    match (inp, out) {
        (serde_json::Value::Object(i), serde_json::Value::Object(o)) => {
            for (k, v) in i {
                let p = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                match o.get(k) {
                    Some(ov) => walk(v, ov, &p, leaves, found),
                    None => {
                        let siblings: Vec<&String> = o.keys().collect();
                        found.push((p.clone(), suggest(k, path, &siblings, leaves)));
                    }
                }
            }
        }
        (serde_json::Value::Array(i), serde_json::Value::Array(o)) => {
            for (n, (iv, ov)) in i.iter().zip(o).enumerate() {
                walk(iv, ov, &format!("{path}[{n}]"), leaves, found);
            }
        }
        _ => {}
    }
}

fn suggest(
    key: &str,
    parent: &str,
    siblings: &[&String],
    leaves: &BTreeMap<String, Vec<String>>,
) -> Option<String> {
    let join = |k: &str| {
        if parent.is_empty() {
            k.to_owned()
        } else {
            format!("{parent}.{k}")
        }
    };
    if let Some(best) = siblings
        .iter()
        .map(|s| (edit_distance(key, s), s))
        .filter(|(d, s)| *d <= 2.max(s.len() / 4))
        .min_by_key(|(d, _)| *d)
    {
        return Some(join(best.1));
    }
    let elsewhere = leaves.get(key)?;
    (elsewhere.len() == 1).then(|| elsewhere[0].replace("[]", "[0]"))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lint(raw: &str) -> Vec<Finding> {
        lint_str(raw, &LintEnv::OFFLINE)
    }
    fn has(f: &[Finding], sev: Severity, key: &str, needle: &str) -> bool {
        f.iter()
            .any(|f| f.severity == sev && f.key == key && f.message.contains(needle))
    }
    fn any_error(f: &[Finding]) -> bool {
        f.iter().any(|f| f.severity == Severity::Error)
    }

    #[test]
    fn empty_and_default_config_are_clean() {
        assert!(lint("").is_empty(), "{:?}", lint(""));
    }

    #[test]
    fn syntax_and_type_errors_are_reported_with_position() {
        let f = lint("listen = \n");
        assert_eq!(f.len(), 1);
        assert!(has(&f, Severity::Error, "(file)", "not valid TOML"));
        let f = lint("reaper_interval_secs = \"soon\"\n");
        assert!(has(&f, Severity::Error, "(file)", "cannot load"), "{f:?}");
    }

    #[test]
    fn unknown_key_gets_a_did_you_mean() {
        let f = lint("[tls]\ntrust_proxy_header = true\n");
        assert!(
            has(
                &f,
                Severity::Warning,
                "tls.trust_proxy_header",
                "did you mean `tls.trust_proxy_headers`"
            ),
            "{f:?}"
        );
        assert_eq!(
            f[0].fix.as_deref(),
            Some("rename it to `tls.trust_proxy_headers`")
        );
    }

    #[test]
    fn misplaced_key_points_at_its_real_table() {
        let f = lint("[sandbox]\nallow_net = true\n");
        assert!(
            has(
                &f,
                Severity::Warning,
                "sandbox.allow_net",
                "`sandbox.procbox.allow_net`"
            ),
            "{f:?}"
        );
    }

    #[test]
    fn unknown_top_level_and_array_item_keys() {
        let f = lint(
            "liste = \"127.0.0.1:1\"\n[[auth.tokens]]\ntoken = \"0123456789abcdef0123\"\nrole = \"admin\"\nrol = \"x\"\n",
        );
        assert!(
            has(&f, Severity::Warning, "liste", "did you mean `listen`"),
            "{f:?}"
        );
        assert!(
            has(&f, Severity::Warning, "auth.tokens[0].rol", "unknown key"),
            "{f:?}"
        );
    }

    #[test]
    fn known_option_keys_are_not_flagged() {
        let f = lint(
            "firecracker_kernel = \"/k\"\n[auth]\nrate_limit_rps = 5.0\nrate_limit_burst = 10\n[policy]\nmax_vcpus = 4\n",
        );
        assert!(
            !f.iter().any(|f| f.message.contains("unknown key")),
            "{f:?}"
        );
    }

    #[test]
    fn tls_half_configured_is_an_error() {
        let f = lint("[tls]\ncert = \"/c\"\n");
        assert!(has(&f, Severity::Error, "tls.key", "plain HTTP"));
        let f = lint("[tls]\nclient_ca = \"/ca\"\n");
        assert!(has(&f, Severity::Error, "tls.client_ca", "NOT required"));
        let f = lint("[tls]\ncert = \"/c\"\nkey = \"/k\"\nclient_ca = \"/ca\"\n");
        assert!(!any_error(&f));
    }

    #[test]
    fn tls_files_are_checked_only_when_the_host_is_consulted() {
        let raw = "[tls]\ncert = \"/nope/c\"\nkey = \"/nope/k\"\n";
        assert!(!any_error(&lint(raw)));
        let none = |_: &Path| false;
        let f = lint_str(
            raw,
            &LintEnv {
                exists: Some(&none),
            },
        );
        assert!(has(&f, Severity::Error, "tls.cert", "does not exist"));
        let all = |_: &Path| true;
        assert!(!any_error(&lint_str(raw, &LintEnv { exists: Some(&all) })));
    }

    #[test]
    fn trust_proxy_headers_beyond_loopback_is_flagged() {
        let f = lint("listen = \"0.0.0.0:7788\"\n[tls]\ntrust_proxy_headers = true\n");
        assert!(has(
            &f,
            Severity::Warning,
            "tls.trust_proxy_headers",
            "claim any role"
        ));
        let f = lint("[tls]\ntrust_proxy_headers = true\n");
        assert!(!f.iter().any(|f| f.key == "tls.trust_proxy_headers"));
        let f = lint(
            "listen = \"0.0.0.0:7788\"\n[tls]\ncert=\"/c\"\nkey=\"/k\"\nclient_ca=\"/ca\"\ntrust_proxy_headers = true\n",
        );
        assert!(has(
            &f,
            Severity::Info,
            "tls.trust_proxy_headers",
            "ignored"
        ));
    }

    #[test]
    fn auth_require_without_credentials() {
        let f = lint("[auth]\nrequire = true\n");
        assert!(has(&f, Severity::Error, "auth.require", "401"));
        let f = lint(
            "[auth]\nrequire = true\n[[auth.tokens]]\ntoken = \"0123456789abcdef0123\"\nrole = \"admin\"\n",
        );
        assert!(!any_error(&f), "{f:?}");
    }

    #[test]
    fn non_loopback_without_tokens_and_with_cleartext_tokens() {
        let f = lint("listen = \"0.0.0.0:7788\"\n");
        assert!(has(&f, Severity::Warning, "auth", "fail closed"));
        let f = lint(
            "listen = \"0.0.0.0:7788\"\n[[auth.tokens]]\ntoken = \"0123456789abcdef0123\"\nrole = \"admin\"\n",
        );
        assert!(has(&f, Severity::Warning, "tls", "plain HTTP"));
    }

    #[test]
    fn token_hygiene_never_echoes_the_secret() {
        let f = lint(
            "[[auth.tokens]]\ntoken = \"hunter2\"\nrole = \"admin\"\n[[auth.tokens]]\ntoken = \"hunter2\"\nrole = \"read-only\"\n[[auth.tokens]]\ntoken = \"\"\nrole = \"admin\"\n",
        );
        assert!(has(
            &f,
            Severity::Warning,
            "auth.tokens[0].token",
            "shorter than 16"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "auth.tokens[1].token",
            "same value as auth.tokens[0]"
        ));
        assert!(has(&f, Severity::Error, "auth.tokens[2].token", "empty"));
        let dump = format!("{f:?}");
        assert!(!dump.contains("hunter2"));
    }

    #[test]
    fn no_admin_token_and_oidc_and_rate_limit() {
        let f = lint("[[auth.tokens]]\ntoken = \"0123456789abcdef0123\"\nrole = \"read-only\"\n");
        assert!(has(
            &f,
            Severity::Warning,
            "auth.tokens",
            "nobody can create"
        ));
        let f = lint("[auth]\noidc_issuer = \"http://idp\"\n");
        assert!(has(
            &f,
            Severity::Error,
            "auth.oidc_audience",
            "silently off"
        ));
        assert!(has(&f, Severity::Warning, "auth.oidc_issuer", "not https"));
        let f = lint("[auth]\nrate_limit_rps = 5.0\n");
        assert!(has(&f, Severity::Error, "auth.rate_limit_burst", "without"));
        let f = lint("[auth]\nrate_limit_rps = 0.0\nrate_limit_burst = 3\n");
        assert!(has(&f, Severity::Error, "auth.rate_limit_rps", "positive"));
    }

    #[test]
    fn mfa_rule_must_match_a_named_token() {
        let f = lint(
            "[[auth.tokens]]\ntoken = \"0123456789abcdef0123\"\nrole = \"admin\"\nname = \"ops\"\n[[auth.mfa]]\ntoken = \"opz\"\ntotp_secret_file = \"/s\"\n",
        );
        assert!(has(
            &f,
            Severity::Error,
            "auth.mfa[0].token",
            "never applies"
        ));
        let f = lint(
            "[[auth.tokens]]\ntoken = \"0123456789abcdef0123\"\nrole = \"admin\"\nname = \"ops\"\n[[auth.mfa]]\ntoken = \"ops\"\ntotp_secret_file = \"/s\"\n",
        );
        assert!(!any_error(&f), "{f:?}");
    }

    #[test]
    fn listen_must_be_host_port() {
        assert!(has(
            &lint("listen = \"localhost\"\n"),
            Severity::Error,
            "listen",
            "host:port"
        ));
        assert!(has(
            &lint("listen = \"0.0.0.0:99999\"\n"),
            Severity::Error,
            "listen",
            "host:port"
        ));
        assert!(!any_error(&lint("listen = \"[::1]:7788\"\n")));
    }

    #[test]
    fn policy_dead_ends() {
        let f = lint(
            "[policy]\nmax_vcpus = 0\nallowed_backends = []\n[[policy.tenants]]\ntenant = \"a\"\n[[policy.tenants]]\ntenant = \"a\"\nmax_vms_total = 0\n",
        );
        assert!(has(
            &f,
            Severity::Warning,
            "policy.max_vcpus",
            "refuses every VM"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "policy.allowed_backends",
            "empty list"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "policy.tenants[1].tenant",
            "listed twice"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "policy.tenants[1]",
            "blocks every VM"
        ));
        let f = lint("[policy]\nmax_memory_mib = 9000\nmax_memory_mib_host = 8000\n");
        assert!(has(
            &f,
            Severity::Warning,
            "policy.max_memory_mib",
            "above the host cap"
        ));
    }

    #[test]
    fn token_tenant_without_policy_entry() {
        let f = lint(
            "[[auth.tokens]]\ntoken = \"0123456789abcdef0123\"\nrole = \"admin\"\ntenant = \"acme\"\n[[policy.tenants]]\ntenant = \"other\"\n",
        );
        assert!(has(
            &f,
            Severity::Warning,
            "auth.tokens[0].tenant",
            "no aggregate caps"
        ));
    }

    #[test]
    fn sandbox_timing_and_egress() {
        let f = lint(
            "[sandbox]\nautopause_idle_secs = 60\nautopause_scan_secs = 0\nidle_balloon_secs = 90\nidle_balloon_percent = 95\nhibernate_idle_secs = 30\n",
        );
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.autopause_scan_secs",
            "never paused"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.idle_balloon_percent",
            "outside 1-90"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.idle_balloon_secs",
            "shorter"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.hibernate_idle_secs",
            "longer"
        ));
        let f = lint(
            "[sandbox]\negress_tls_intercept = true\negress_http_rules = [\"permit GET x/*\", \"deny * x/admin\"]\negress_proxy_listen = \"0.0.0.0:3128\"\negress_tls_allow_private = true\n",
        );
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.egress_http_rules[0]",
            "allow or deny"
        ));
        assert!(!f.iter().any(|f| f.key == "sandbox.egress_http_rules[1]"));
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.egress_proxy_listen",
            "beyond loopback"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.egress_tls_allow_private",
            "metadata"
        ));
        let f = lint("[sandbox]\negress_tls_intercept = true\n");
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.egress_tls_intercept",
            "does nothing"
        ));
    }

    #[test]
    fn credential_vault_is_checked_without_echoing_the_header() {
        let f = lint(
            "[sandbox]\negress_proxy_listen = \"127.0.0.1:3128\"\n[[sandbox.credential_vault]]\nhost = \"\"\nauthorization = \"Bearer SECRET-VALUE\"\n[[sandbox.credential_vault]]\nhost = \"api.x\"\nauthorization = \" \"\n",
        );
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.credential_vault[0].host",
            "empty"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.credential_vault[1].authorization",
            "empty"
        ));
        assert!(!format!("{f:?}").contains("SECRET-VALUE"));
        let f =
            lint("[[sandbox.credential_vault]]\nhost = \"api.x\"\nauthorization = \"Bearer z\"\n");
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.credential_vault",
            "never used"
        ));
    }

    #[test]
    fn procbox_caps_and_fail_open() {
        let f = lint(
            "[sandbox.procbox]\nenabled = true\nbest_effort = true\nisolation = \"strictly\"\ndefault_timeout_secs = 600\nmax_timeout_secs = 300\ndefault_memory_mib = 4096\nuid_base = 500\nallow_root = true\nuid_count = 0\n",
        );
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.procbox.isolation",
            "not off, auto or strict"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.procbox.default_timeout_secs",
            "above max"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.procbox.default_memory_mib",
            "above max"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.procbox.best_effort",
            "fails open"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.procbox.allow_root",
            "as root"
        ));
        let f = lint("[sandbox.procbox]\nuid_base = 4294967295\nuid_count = 10\n");
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.procbox.uid_base",
            "overflows"
        ));
        let f = lint("[sandbox.procbox]\nuid_base = 500\nuid_count = 10\nenabled = true\n");
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.procbox.uid_base",
            "below 1000"
        ));
        // disabled procbox: only hard contradictions are reported
        assert!(
            !lint("[sandbox.procbox]\nbest_effort = true\n")
                .iter()
                .any(|f| f.key.ends_with("best_effort"))
        );
    }

    #[test]
    fn dataplane_contradictions() {
        let f = lint(
            "[sandbox.dataplane]\nmode = \"legacy\"\n[sandbox.dataplane.xdp]\nenabled = true\nblock_cidrs = [\"10.0.0.0/8\"]\n",
        );
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.dataplane.mode",
            "ignored"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.dataplane.xdp.interface",
            "without an interface"
        ));
        let f = lint(
            "[sandbox.dataplane.service]\npressure_soft_percent = 95\npressure_hard_percent = 90\nedt_enabled = true\n",
        );
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.dataplane.service.pressure_soft_percent",
            "below"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.dataplane.service.edt_interfaces",
            "does nothing"
        ));
        let f = lint("[sandbox.dataplane]\nblock_cidrs = []\n");
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.dataplane.block_cidrs",
            "did you mean"
        ));
    }

    #[test]
    fn dataplane_lists_are_parsed() {
        let f = lint(
            "[sandbox.dataplane]\nallow_cidrs = [\"10.0.0.0\", \"10.0.0.0/8\", \"::/129\"]\nallow_ports = [\"tcp/443\", \"http/80\", \"tcp/0\", \"icmp/0\"]\nmax_egress_mbps = 0\n",
        );
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.dataplane.allow_cidrs[0]",
            "not a CIDR"
        ));
        assert!(
            !f.iter()
                .any(|f| f.key == "sandbox.dataplane.allow_cidrs[1]")
        );
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.dataplane.allow_cidrs[2]",
            "not a CIDR"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.dataplane.allow_ports[1]",
            "proto/PORT"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "sandbox.dataplane.allow_ports[2]",
            "proto/PORT"
        ));
        assert!(
            !f.iter().any(|f| f.key == "sandbox.dataplane.allow_ports[0]"
                || f.key == "sandbox.dataplane.allow_ports[3]")
        );
        assert!(has(
            &f,
            Severity::Warning,
            "sandbox.dataplane.max_egress_mbps",
            "0"
        ));
    }

    #[test]
    fn jailer_rules() {
        let f = lint("[jailer]\nenforce = true\n");
        assert!(has(&f, Severity::Error, "jailer.enabled", "refused"));
        let f = lint("listen = \"0.0.0.0:7788\"\n[auth]\nrequire = true\n");
        assert!(has(&f, Severity::Error, "jailer.enabled", "refused"));
        let f = lint("[jailer]\nenabled = true\nuid_range_start = 2000\nuid = 0\n");
        assert!(has(
            &f,
            Severity::Error,
            "jailer.uid_range_start",
            "together"
        ));
        let f = lint("[jailer]\nenabled = true\nuid = 0\n");
        assert!(has(&f, Severity::Warning, "jailer.uid", "root"));
    }

    #[test]
    fn catalog_signers() {
        let good = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let short = base64::engine::general_purpose::STANDARD.encode([7u8; 16]);
        let raw = format!(
            "[catalog]\npath = \"/c.json\"\n[[catalog.trusted_signers]]\nname = \"a\"\npublic_key = \"{good}\"\n[[catalog.trusted_signers]]\nname = \"a\"\npublic_key = \"{short}\"\n[[catalog.trusted_signers]]\nname = \"c\"\npublic_key = \"!!!\"\n[[catalog.trusted_signers]]\nname = \"d\"\npublic_key = \"{good}\"\n"
        );
        let f = lint(&raw);
        assert!(has(
            &f,
            Severity::Error,
            "catalog.trusted_signers[1].public_key",
            "16 bytes"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "catalog.trusted_signers[2].public_key",
            "base64"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "catalog.trusted_signers[3].public_key",
            "twice"
        ));
        assert!(has(
            &f,
            Severity::Warning,
            "catalog.trusted_signers[1].name",
            "more than once"
        ));
        assert!(
            !f.iter()
                .any(|f| f.key == "catalog.trusted_signers[0].public_key")
        );
    }

    #[test]
    fn misc_paths_and_registry_credentials() {
        let f = lint(
            "state_dir = \"var/fluxvm\"\n[[apple.oci_registry_credentials]]\nregistry = \"ghcr.io\"\nusername = \"u\"\npassword = \"pw-secret\"\n[[apple.oci_registry_credentials]]\nregistry = \"\"\nusername = \"u\"\n",
        );
        assert!(has(&f, Severity::Warning, "state_dir", "relative"));
        assert!(has(
            &f,
            Severity::Warning,
            "apple.oci_registry_credentials[0].password",
            "inline"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "apple.oci_registry_credentials[1].registry",
            "empty"
        ));
        assert!(has(
            &f,
            Severity::Error,
            "apple.oci_registry_credentials[1].password",
            "unauthenticated"
        ));
        assert!(!format!("{f:?}").contains("pw-secret"));
    }

    #[test]
    fn findings_are_sorted_most_severe_first() {
        let f = lint("state_dir = \"rel\"\n[tls]\ncert = \"/c\"\n");
        assert_eq!(f[0].severity, Severity::Error);
        assert!(f.windows(2).all(|w| w[0].severity >= w[1].severity));
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "ab"), 2);
        assert_eq!(edit_distance("same", "same"), 0);
    }
}
