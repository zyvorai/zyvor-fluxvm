// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Gives a sandbox whose traffic has no address of its own (a vz guest with no network card, reaching the
//! host egress proxy through the runner) a stable *alias* address, so the proxy's per-sandbox rules and
//! grants (keyed by address) apply to it.
//!
//! The runner proves who it is with a secret token it sends to the proxy; the guest never sees the token.
//! The alias is in `127.77.0.0/16`, which no guest network uses, and is only honoured for loopback peers.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

/// The header the runner adds to every request it relays to the host egress proxy.
pub const PEER_HEADER: &str = "x-fluxvm-peer";

#[derive(Default)]
struct Registry {
    by_vm: HashMap<Uuid, IpAddr>,
    by_token: HashMap<String, IpAddr>,
    next: u32,
}

fn reg() -> std::sync::MutexGuard<'static, Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Registers `vm` (idempotent for the address; a fresh token every call, replacing the previous one) and
/// returns its alias address and the token its runner must present.
pub fn register(vm: Uuid) -> (IpAddr, String) {
    let mut r = reg();
    let ip = match r.by_vm.get(&vm) {
        Some(ip) => *ip,
        None => {
            // Skip addresses a live sandbox still holds, so two never share one.
            let ip = loop {
                r.next = (r.next % 65_000) + 1;
                let n = r.next;
                let ip = IpAddr::V4(Ipv4Addr::new(127, 77, (n >> 8) as u8, (n & 0xff) as u8));
                if !r.by_vm.values().any(|v| *v == ip) {
                    break ip;
                }
            };
            r.by_vm.insert(vm, ip);
            ip
        }
    };
    r.by_token.retain(|_, v| *v != ip);
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    r.by_token.insert(token.clone(), ip);
    (ip, token)
}

/// The alias address of `vm`, once its runner has been launched with the proxy.
pub fn ip_of(vm: Uuid) -> Option<IpAddr> {
    reg().by_vm.get(&vm).copied()
}

/// The alias a presented token stands for.
pub fn resolve(token: &str) -> Option<IpAddr> {
    reg().by_token.get(token).copied()
}

/// Forgets `vm` (its token stops working).
pub fn remove(vm: Uuid) {
    let mut r = reg();
    if let Some(ip) = r.by_vm.remove(&vm) {
        r.by_token.retain(|_, v| *v != ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vm_keeps_its_address_and_a_new_token_replaces_the_old() {
        let vm = Uuid::new_v4();
        let (ip, t1) = register(vm);
        assert_eq!(ip_of(vm), Some(ip));
        assert_eq!(resolve(&t1), Some(ip));
        let (ip2, t2) = register(vm);
        assert_eq!(ip, ip2);
        assert_eq!(resolve(&t1), None);
        assert_eq!(resolve(&t2), Some(ip));
        assert!(ip.is_loopback());
        remove(vm);
        assert_eq!(resolve(&t2), None);
        assert_eq!(ip_of(vm), None);
    }

    #[test]
    fn two_vms_get_different_addresses() {
        let (a, _) = register(Uuid::new_v4());
        let (b, _) = register(Uuid::new_v4());
        assert_ne!(a, b);
    }
}
