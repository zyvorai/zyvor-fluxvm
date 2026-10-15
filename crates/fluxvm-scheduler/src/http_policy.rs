// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Per-sandbox HTTP method/host/path rules, enforced by the host's egress proxy
//! (`fluxvm_network::peer_policy`). The proxy attributes traffic by guest address, so a sandbox
//! can have rules only where its traffic reaches that proxy with its own address.

use crate::{VmManager, procbox_sandbox};
use anyhow::{Result, bail};
use fluxvm_core::model::BackendKind;
use fluxvm_network::peer_policy;
use std::net::IpAddr;
use uuid::Uuid;

/// Why per-sandbox HTTP rules cannot be enforced for a sandbox, or `None` when they can.
pub(crate) fn unsupported_reason(
    backend: BackendKind,
    procbox: bool,
    proxy_listen: &str,
    guest_ip: Option<IpAddr>,
) -> Option<String> {
    if procbox {
        return Some("procbox sandboxes share the host network namespace's own policy".into());
    }
    if backend == BackendKind::Vz && guest_ip.is_none() {
        // A vz guest's address is its alias (see `fluxvm_core::peer_alias`), which exists only when its
        // runner relays through the daemon's proxy: no network card, a host allow-list, and a loopback
        // `sandbox.egress_proxy_listen`.
        return Some(
            "a vz sandbox's method/path rules need the daemon's egress proxy: set \
             sandbox.egress_proxy_listen to a 127.0.0.1 address and give the session a host allow-list \
             (a guest with its own network card, or the runner's name-only proxy, cannot be filtered)"
                .into(),
        );
    }
    if proxy_listen.trim().is_empty() {
        return Some(
            "the host's egress proxy is not enabled (sandbox.egress_proxy_listen is empty)".into(),
        );
    }
    if guest_ip.is_none() {
        return Some(
            "the sandbox's guest address is unknown, so its traffic cannot be attributed".into(),
        );
    }
    None
}

impl VmManager {
    /// The address the proxy sees this sandbox's traffic from: the guest's own, or, for a vz guest relayed
    /// through the runner, its alias.
    pub(crate) fn guest_addr(vm: &fluxvm_core::model::VmRecord) -> Option<IpAddr> {
        if vm.backend == BackendKind::Vz {
            return fluxvm_core::peer_alias::ip_of(vm.id);
        }
        vm.guest_ip.as_deref().and_then(|s| s.parse().ok())
    }

    /// Why this sandbox cannot have per-sandbox HTTP rules (`None`: it can).
    pub async fn http_policy_unsupported(&self, id: Uuid) -> Result<Option<String>> {
        let vm = self.get(id).await?;
        let procbox = procbox_sandbox::load_spec(&vm)?.is_some();
        Ok(unsupported_reason(
            vm.backend,
            procbox,
            &self.cfg.sandbox.egress_proxy_listen,
            Self::guest_addr(&vm),
        ))
    }

    /// Installs the sandbox's HTTP rules in the egress proxy, bound to its guest address. Fails
    /// (installing nothing) when the sandbox cannot be filtered or a rule is malformed.
    pub async fn set_http_policy(&self, id: Uuid, rules: &[String]) -> Result<()> {
        if let Some(why) = self.http_policy_unsupported(id).await? {
            bail!("per-sandbox HTTP rules cannot be enforced: {why}");
        }
        let vm = self.get(id).await?;
        peer_policy::global()
            .set(&id.to_string(), rules, Self::guest_addr(&vm))
            .map_err(anyhow::Error::msg)
    }

    /// After a restore or a move the guest address may have changed. Re-binds the rules to it.
    /// `Err` when the new address is bound to another sandbox's rules: the rules stay
    /// installed but unbound, and the caller must not treat the sandbox as filtered.
    pub async fn rebind_http_policy(&self, id: Uuid) -> Result<bool> {
        let vm = self.get(id).await?;
        peer_policy::global()
            .rebind(&id.to_string(), Self::guest_addr(&vm))
            .map_err(anyhow::Error::msg)
    }

    /// Whether the sandbox has rules and they are bound to an address.
    pub fn http_policy_status(&self, id: Uuid) -> (bool, bool) {
        peer_policy::global().status(&id.to_string())
    }

    pub fn clear_http_policy(&self, id: Uuid) {
        peer_policy::global().remove(&id.to_string());
        fluxvm_core::peer_alias::remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_proxied_attributable_sandbox_can_be_filtered() {
        let ip = Some("10.0.0.5".parse().unwrap());
        assert_eq!(
            unsupported_reason(BackendKind::Qemu, false, "127.0.0.1:18080", ip),
            None
        );
        // A vz guest with an alias (relayed through its runner) can be filtered.
        assert_eq!(
            unsupported_reason(BackendKind::Vz, false, "127.0.0.1:18080", ip),
            None
        );
        for (b, pb, listen, ip) in [
            (BackendKind::Vz, false, "127.0.0.1:18080", None),
            (BackendKind::Qemu, true, "127.0.0.1:18080", ip),
            (BackendKind::Qemu, false, "", ip),
            (BackendKind::Qemu, false, "127.0.0.1:18080", None),
        ] {
            assert!(
                unsupported_reason(b, pb, listen, ip).is_some(),
                "{b:?} {pb} {listen:?} {ip:?}"
            );
        }
    }
}
