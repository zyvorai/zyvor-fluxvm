// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! The VM backend as the Docker and Kubernetes workflows see it, and the network spec each backend needs.
//!
//! * `Vz` (macOS, Apple Virtualization): the guest gets an address the host can reach, so the request carries no forward;
//!   it also offers a private switch between VMs and Rosetta.
//! * `Qemu` (Linux): user-mode networking gives the guest no address the host can reach. The control path is an SSH forward
//!   that listens on the host's loopback only, on a free high port. The port is part of the VM request, so the daemon's
//!   record holds it and `VmApi::guest_ip` reads it back as `127.0.0.1:<port>` (see `run::loopback_ssh_endpoint`). Every
//!   later ssh, the API tunnel and the Docker socket forward go through that one endpoint.
//!
//! No private VM-to-VM network exists on `Qemu`, so multi-node clusters and Rosetta are `Vz`-only and say so.

use anyhow::{Result, bail};
use serde_json::{Value, json};

/// Host ports tried for the SSH forward (shared with `fluxctl stack`, which hashes the VM name into the same range).
const PORT_LO: u16 = 22000;
const PORT_COUNT: u16 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Vz,
    Qemu,
}

impl Backend {
    /// The backend this host runs VMs with.
    pub fn host() -> Self {
        if cfg!(target_os = "macos") { Backend::Vz } else { Backend::Qemu }
    }

    pub fn name(self) -> &'static str {
        match self {
            Backend::Vz => "vz (Apple Virtualization)",
            Backend::Qemu => "qemu (Linux)",
        }
    }

    /// The CPU architecture of the guests: always arm64 under `Vz`; the host's under `Qemu` (KVM).
    pub fn guest_arch(self) -> &'static str {
        match self {
            Backend::Vz => "aarch64",
            Backend::Qemu => std::env::consts::ARCH,
        }
    }

    /// The name the k3s release gives the binary for this architecture, and its checksum file.
    pub fn k3s_binary(self) -> &'static str {
        if self.guest_arch() == "aarch64" { "k3s-arm64" } else { "k3s" }
    }

    pub fn k3s_sums_file(self) -> &'static str {
        if self.guest_arch() == "aarch64" { "sha256sum-arm64.txt" } else { "sha256sum-amd64.txt" }
    }

    /// A private network that joins several VMs (workers, extra servers).
    pub fn has_private_network(self) -> bool {
        self == Backend::Vz
    }

    pub fn has_rosetta(self) -> bool {
        self == Backend::Vz
    }

    pub fn require_private_network(self, what: &str) -> Result<()> {
        if !self.has_private_network() {
            bail!(
                "{what} needs the private VM-to-VM network, which only the macOS (vz) backend has; this host uses {}. \
                 Use a single server here",
                self.name()
            );
        }
        Ok(())
    }

    pub fn require_rosetta(self) -> Result<()> {
        if !self.has_rosetta() {
            bail!("--rosetta is for macOS guests on Apple silicon (vz); this host uses {}", self.name());
        }
        Ok(())
    }
}

/// The first port at or after the VM name's stable slot (so a rerun computes the same spec) for which `free` says yes.
pub fn pick_ssh_port(vm_name: &str, free: &dyn Fn(u16) -> bool) -> Option<u16> {
    let start = crate::stack::ssh_forward_port(vm_name) - PORT_LO;
    (0..PORT_COUNT).map(|i| PORT_LO + (start + i) % PORT_COUNT).find(|p| free(*p))
}

/// True when nothing listens on 127.0.0.1:`port` now.
pub fn loopback_port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Reads a port back out of a VM request, the other half of [`network_spec`].
pub fn recorded_ssh_port(request: &Value) -> Option<u16> {
    crate::run::loopback_ssh_endpoint(request)?.rsplit(':').next()?.parse().ok()
}

/// The `network` member of a VM request. `ssh_port` is only used under `Qemu`.
pub fn network_spec(backend: Backend, mut forwards: Vec<Value>, ssh_port: u16) -> Value {
    if backend == Backend::Qemu {
        forwards.push(json!({"host_port": ssh_port, "guest_port": 22, "host_addr": "127.0.0.1"}));
    }
    json!({"mode": "user", "forwards": forwards})
}

/// [`network_spec`] for this host, choosing a free port for a new VM.
pub fn host_network_spec(vm_name: &str, forwards: Vec<Value>) -> Value {
    let backend = Backend::host();
    let port = if backend == Backend::Qemu {
        pick_ssh_port(vm_name, &loopback_port_free).unwrap_or_else(|| crate::stack::ssh_forward_port(vm_name))
    } else {
        0
    };
    network_spec(backend, forwards, port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vz_needs_no_forward() {
        let n = network_spec(Backend::Vz, vec![], 0);
        assert_eq!(n, json!({"mode": "user", "forwards": []}));
    }

    #[test]
    fn qemu_gets_a_loopback_only_ssh_forward_the_endpoint_finder_reads_back() {
        let n = network_spec(Backend::Qemu, vec![json!({"host_port": 8080, "guest_port": 80})], 22123);
        assert_eq!(n["forwards"][1], json!({"host_port": 22123, "guest_port": 22, "host_addr": "127.0.0.1"}));
        assert_eq!(n["forwards"][0]["guest_port"], 80);
        let req = json!({"network": n});
        assert_eq!(crate::run::loopback_ssh_endpoint(&req).as_deref(), Some("127.0.0.1:22123"));
        assert_eq!(recorded_ssh_port(&req), Some(22123));
        assert_eq!(recorded_ssh_port(&json!({"network": network_spec(Backend::Vz, vec![], 0)})), None);
    }

    #[test]
    fn port_selection_is_stable_in_range_and_skips_taken_ports() {
        let first = pick_ssh_port("k3s-dev", &|_| true).unwrap();
        assert!((22000..23000).contains(&first));
        assert_eq!(first, crate::stack::ssh_forward_port("k3s-dev"));
        assert_eq!(pick_ssh_port("k3s-dev", &|_| true), Some(first));
        let second = pick_ssh_port("k3s-dev", &|p| p != first).unwrap();
        assert_ne!(second, first);
        assert!((22000..23000).contains(&second));
        let last = pick_ssh_port("k3s-dev", &|p| p == 22000 || p == 22999);
        assert!(matches!(last, Some(22000) | Some(22999)));
        assert_eq!(pick_ssh_port("k3s-dev", &|_| false), None);
    }

    #[test]
    fn a_listening_port_is_not_free() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let p = l.local_addr().unwrap().port();
        assert!(!loopback_port_free(p));
        drop(l);
        assert!(loopback_port_free(p));
    }

    #[test]
    fn linux_artifacts_follow_the_guest_architecture() {
        assert_eq!(Backend::Vz.k3s_binary(), "k3s-arm64");
        assert_eq!(Backend::Vz.k3s_sums_file(), "sha256sum-arm64.txt");
        assert_eq!(Backend::Vz.guest_arch(), "aarch64");
        assert!(Backend::Vz.require_private_network("workers").is_ok());
        assert!(Backend::Vz.require_rosetta().is_ok());
        let e = Backend::Qemu.require_private_network("--servers 3").unwrap_err().to_string();
        assert!(e.contains("--servers 3") && e.contains("vz"), "{e}");
        assert!(Backend::Qemu.require_rosetta().unwrap_err().to_string().contains("--rosetta"));
    }
}
