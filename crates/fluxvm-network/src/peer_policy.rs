// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Per-sandbox HTTP method/host/path rules for the egress proxy.
//!
//! `sandbox.egress_http_rules` is one rule set for the whole host. An agent session wants its own:
//! "this sandbox may `GET` the repository host but not `POST` to it". A sandbox's rules are keyed
//! by the guest address its traffic arrives from (the same attribution the credential-grant store
//! uses) and are applied **in addition to** the host-wide rules: a request must pass both, so a
//! session policy can only narrow what the host allows.
//!
//! A sandbox with no rules is not in the store and is not affected. Rules use the grammar in
//! `docs/http-acl.md`; a malformed rule is an error, never silently dropped.

use crate::http_acl::{HttpAcl, Verdict};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};

/// Most rules one sandbox may carry.
pub const MAX_RULES: usize = 128;

#[derive(Default)]
struct Inner {
    by_sandbox: HashMap<String, Arc<HttpAcl>>,
    sources: HashMap<IpAddr, String>,
}

#[derive(Default)]
pub struct PeerPolicyStore {
    inner: Mutex<Inner>,
}

impl PeerPolicyStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Installs (or replaces) a sandbox's rules, bound to its guest address when known. An empty
    /// rule list removes the entry. Refuses an address already bound to another sandbox.
    pub fn set(
        &self,
        sandbox_id: &str,
        rules: &[String],
        source_ip: Option<IpAddr>,
    ) -> Result<(), String> {
        if rules.is_empty() {
            self.remove(sandbox_id);
            return Ok(());
        }
        if rules.len() > MAX_RULES {
            return Err(format!("at most {MAX_RULES} http rules per sandbox"));
        }
        let acl = HttpAcl::parse(rules)?;
        let mut inner = self.lock();
        if let Some(ip) = source_ip
            && inner.sources.get(&ip).is_some_and(|s| s != sandbox_id)
        {
            return Err(format!("address {ip} is bound to another sandbox's policy"));
        }
        inner.sources.retain(|_, s| s != sandbox_id);
        if let Some(ip) = source_ip {
            inner.sources.insert(ip, sandbox_id.to_string());
        }
        inner
            .by_sandbox
            .insert(sandbox_id.to_string(), Arc::new(acl));
        Ok(())
    }

    /// Moves a sandbox's binding to its current address (after a restore or a move). Returns
    /// whether the sandbox has rules. Refuses when the new address belongs to another sandbox; the
    /// old binding is dropped either way, so the old address never resolves to this sandbox.
    pub fn rebind(&self, sandbox_id: &str, source_ip: Option<IpAddr>) -> Result<bool, String> {
        let mut inner = self.lock();
        inner.sources.retain(|_, s| s != sandbox_id);
        let has = inner.by_sandbox.contains_key(sandbox_id);
        if let Some(ip) = source_ip {
            if inner.sources.get(&ip).is_some_and(|s| s != sandbox_id) {
                return Err(format!("address {ip} is bound to another sandbox's policy"));
            }
            if has {
                inner.sources.insert(ip, sandbox_id.to_string());
            }
        }
        Ok(has)
    }

    pub fn remove(&self, sandbox_id: &str) {
        let mut inner = self.lock();
        inner.by_sandbox.remove(sandbox_id);
        inner.sources.retain(|_, s| s != sandbox_id);
    }

    /// Whether the sandbox has rules, and whether they are bound to an address.
    pub fn status(&self, sandbox_id: &str) -> (bool, bool) {
        let inner = self.lock();
        (
            inner.by_sandbox.contains_key(sandbox_id),
            inner.sources.values().any(|s| s == sandbox_id),
        )
    }

    fn acl_for(&self, peer: Option<IpAddr>) -> Option<Arc<HttpAcl>> {
        let inner = self.lock();
        let sandbox = inner.sources.get(&peer?)?;
        inner.by_sandbox.get(sandbox).cloned()
    }

    /// Judges one request from `peer`. `Allow` when the peer has no rules.
    pub fn check(&self, peer: Option<IpAddr>, method: &str, host: &str, path: &str) -> Verdict {
        match self.acl_for(peer) {
            Some(acl) => acl.check(method, host, path),
            None => Verdict::Allow,
        }
    }

    /// Pre-TLS judgement of a tunnel to `host` the proxy will intercept.
    pub fn check_tunnel(&self, peer: Option<IpAddr>, host: &str) -> Verdict {
        match self.acl_for(peer) {
            Some(acl) => acl.check_intercepted_connect(host),
            None => Verdict::Allow,
        }
    }
}

/// The store the daemon and its egress proxy share.
pub fn global() -> &'static PeerPolicyStore {
    static STORE: OnceLock<PeerPolicyStore> = OnceLock::new();
    STORE.get_or_init(PeerPolicyStore::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    fn rules(r: &[&str]) -> Vec<String> {
        r.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_peer_without_rules_is_unaffected() {
        let s = PeerPolicyStore::new();
        assert!(s.check(ip("10.0.0.5"), "POST", "x.example", "/").is_allow());
        s.set("a", &rules(&["allow GET git.example/*"]), ip("10.0.0.5"))
            .unwrap();
        // Another address, and an unknown one, are not this sandbox.
        assert!(s.check(ip("10.0.0.6"), "POST", "x.example", "/").is_allow());
        assert!(s.check(None, "POST", "x.example", "/").is_allow());
    }

    #[test]
    fn rules_narrow_by_method_host_and_path() {
        let s = PeerPolicyStore::new();
        s.set(
            "a",
            &rules(&["allow GET git.example/*", "deny * */admin/*"]),
            ip("10.0.0.5"),
        )
        .unwrap();
        let p = ip("10.0.0.5");
        assert!(
            s.check(p, "GET", "git.example", "/repo.git/info/refs")
                .is_allow()
        );
        assert!(
            !s.check(p, "POST", "git.example", "/repo.git/git-receive-pack")
                .is_allow()
        );
        assert!(!s.check(p, "GET", "other.example", "/").is_allow());
        assert!(!s.check(p, "GET", "git.example", "/x/../admin/y").is_allow());
        // A tunnel is judged by host only; method and path are checked inside it.
        assert!(s.check_tunnel(p, "git.example").is_allow());
        assert!(!s.check_tunnel(p, "other.example").is_allow());
    }

    #[test]
    fn bad_rules_are_refused_and_change_nothing() {
        let s = PeerPolicyStore::new();
        s.set("a", &rules(&["allow GET git.example/*"]), ip("10.0.0.5"))
            .unwrap();
        assert!(s.set("a", &rules(&["allow GET"]), ip("10.0.0.5")).is_err());
        assert!(
            !s.check(ip("10.0.0.5"), "POST", "git.example", "/")
                .is_allow()
        );
        let many: Vec<String> = (0..=MAX_RULES)
            .map(|i| format!("deny * h{i}.example"))
            .collect();
        assert!(s.set("b", &many, None).is_err());
    }

    #[test]
    fn an_address_belongs_to_one_sandbox() {
        let s = PeerPolicyStore::new();
        s.set("a", &rules(&["allow GET a.example/*"]), ip("10.0.0.5"))
            .unwrap();
        assert!(
            s.set("b", &rules(&["allow GET b.example/*"]), ip("10.0.0.5"))
                .is_err()
        );
        // Nothing of b was installed.
        assert_eq!(s.status("b"), (false, false));
        assert!(!s.check(ip("10.0.0.5"), "GET", "b.example", "/").is_allow());
    }

    #[test]
    fn rebind_moves_the_binding_and_refuses_a_taken_address() {
        let s = PeerPolicyStore::new();
        s.set("a", &rules(&["allow GET a.example/*"]), ip("10.0.0.5"))
            .unwrap();
        s.set("b", &rules(&["allow GET b.example/*"]), ip("10.0.0.6"))
            .unwrap();
        assert_eq!(s.rebind("a", ip("10.0.0.7")), Ok(true));
        // The old address no longer filters as a.
        assert!(s.check(ip("10.0.0.5"), "POST", "z.example", "/").is_allow());
        assert!(!s.check(ip("10.0.0.7"), "GET", "z.example", "/").is_allow());
        // b's address cannot be taken by a.
        assert!(s.rebind("a", ip("10.0.0.6")).is_err());
        assert_eq!(s.status("a"), (true, false), "policy kept, unbound");
        assert!(!s.check(ip("10.0.0.6"), "GET", "a.example", "/").is_allow());
        // A sandbox with no policy rebinds as a no-op.
        assert_eq!(s.rebind("none", ip("10.0.0.9")), Ok(false));
    }

    #[test]
    fn remove_and_empty_rules_clear_the_sandbox() {
        let s = PeerPolicyStore::new();
        s.set("a", &rules(&["deny * x.example"]), ip("10.0.0.5"))
            .unwrap();
        assert!(!s.check(ip("10.0.0.5"), "GET", "x.example", "/").is_allow());
        s.set("a", &[], ip("10.0.0.5")).unwrap();
        assert_eq!(s.status("a"), (false, false));
        assert!(s.check(ip("10.0.0.5"), "GET", "x.example", "/").is_allow());
    }
}
