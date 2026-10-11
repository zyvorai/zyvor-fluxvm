// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `apple.usernet`: the guest's only NIC is a file-handle attachment whose peer is a supervised `fluxvm-usernet`
//! process (userspace NAT, egress/DNS policy, port forwards).
//!
//! The daemon creates the NIC socketpair itself and both children inherit their end at a fixed descriptor number: the
//! runner gets [`RUNNER_NIC_FD`] (and builds a `VZFileHandleNetworkDeviceAttachment` on it), the gateway gets
//! [`GATEWAY_NIC_FD`] (and, when observability is on, [`GATEWAY_TAP_FD`]). The daemon keeps the gateway's end, so a
//! gateway that dies is restarted on the same socket while the runner lives.
//!
//! Observability: the gateway copies every frame to a second datagram socket (`--tap-fd`); a thread here feeds those
//! real frames to the process-wide `fluxvm_netobs` tap, tagged with the VM's identity, so flows, DNS and health show up in
//! `/v1/netobs/*` like any other guest traffic. (Its stdout flow-event JSON lines are only logged.)
//!
//! UNVERIFIED on hardware: no VM has been booted through this path.

use anyhow::{Context, Result, bail};
use fluxvm_core::model::{
    AppleGuest, AppleSpec, AppleUsernet, CreateVmRequest, NetworkSpec,
};
use fluxvm_usernet::{Cidr, PortForward, Proto, UserNetConfig};
use std::net::IpAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Descriptor numbers in the child processes.
pub const RUNNER_NIC_FD: i32 = 3;
pub const GATEWAY_NIC_FD: RawFd = 3;
pub const GATEWAY_TAP_FD: RawFd = 4;

const MAX_ENTRIES: usize = 64;

fn valid_domain(d: &str) -> bool {
    let host = d.strip_prefix("*.").unwrap_or(d);
    !host.is_empty()
        && !host.starts_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Admission checks for `apple.usernet` (the daemon repeats nothing at launch beyond [`to_config`]).
pub fn validate(u: &AppleUsernet, req: &CreateVmRequest, apple: &AppleSpec) -> Result<()> {
    if apple.guest_os != AppleGuest::Linux {
        bail!("apple.usernet is for Linux guests");
    }
    if matches!(req.network, NetworkSpec::None) {
        bail!("apple.usernet needs a network card; network.mode = \"none\" has none");
    }
    if apple.bridge_interface.is_some() {
        bail!("apple.usernet and apple.bridge_interface are mutually exclusive");
    }
    if apple.vmnet.is_some() {
        bail!("apple.usernet and apple.vmnet are mutually exclusive");
    }
    if let NetworkSpec::User { forwards } = &req.network
        && !forwards.is_empty()
    {
        bail!("with apple.usernet use apple.usernet.port_forwards, not network.forwards");
    }
    for (what, n) in [
        ("allow_cidrs", u.allow_cidrs.len()),
        ("deny_cidrs", u.deny_cidrs.len()),
        ("allow_ports", u.allow_ports.len()),
        ("deny_ports", u.deny_ports.len()),
        ("dns_allow", u.dns_allow.len()),
        ("dns_deny", u.dns_deny.len()),
        ("port_forwards", u.port_forwards.len()),
    ] {
        if n > MAX_ENTRIES {
            bail!("apple.usernet.{what} has more than {MAX_ENTRIES} entries");
        }
    }
    for c in u.allow_cidrs.iter().chain(&u.deny_cidrs) {
        c.parse::<Cidr>()
            .map_err(|e| anyhow::anyhow!("apple.usernet CIDR {e}"))?;
    }
    for p in u.allow_ports.iter().chain(&u.deny_ports) {
        if *p == 0 {
            bail!("apple.usernet ports must be 1..=65535");
        }
    }
    for d in u.dns_allow.iter().chain(&u.dns_deny) {
        if !valid_domain(d) {
            bail!("apple.usernet DNS entry {d:?} is not a host name or *.suffix");
        }
    }
    if let Some(mtu) = u.mtu
        && !(576..=9000).contains(&mtu)
    {
        bail!("apple.usernet.mtu must be 576..=9000");
    }
    let mut seen = std::collections::HashSet::new();
    for f in &u.port_forwards {
        let proto = f.protocol.to_ascii_lowercase();
        if proto != "tcp" && proto != "udp" {
            bail!("apple.usernet forward protocol must be tcp or udp");
        }
        if f.guest_port == 0 {
            bail!("apple.usernet forward guest_port must be 1..=65535");
        }
        if f.host_port != 0 && f.host_port < 1024 {
            bail!(
                "apple.usernet forward host_port {} is privileged; use 1024..=65535 (or 0 for an ephemeral port)",
                f.host_port
            );
        }
        let addr: IpAddr = match &f.host_addr {
            Some(a) => a
                .parse()
                .map_err(|_| anyhow::anyhow!("apple.usernet forward host_addr {a:?} is not an IP address"))?,
            None => IpAddr::from([127, 0, 0, 1]),
        };
        if f.host_port != 0 && !seen.insert((proto, addr, f.host_port)) {
            bail!(
                "apple.usernet forwards host port {} more than once",
                f.host_port
            );
        }
    }
    Ok(())
}

/// Maps the request's spec onto the gateway's config. `guest_mac` pins the NIC's MAC so the gateway never learns another.
pub fn to_config(u: &AppleUsernet, guest_mac: Option<[u8; 6]>) -> Result<UserNetConfig> {
    let mut c = UserNetConfig::default();
    let cidrs = |v: &[String]| -> Result<Vec<Cidr>> {
        v.iter()
            .map(|s| s.parse::<Cidr>().map_err(|e| anyhow::anyhow!("CIDR {e}")))
            .collect()
    };
    c.egress.allow_cidrs = cidrs(&u.allow_cidrs)?;
    c.egress.deny_cidrs = cidrs(&u.deny_cidrs)?;
    c.egress.allow_ports = u.allow_ports.clone();
    c.egress.deny_ports = u.deny_ports.clone();
    c.egress.allow_domains = u.dns_allow.clone();
    c.egress.deny_domains = u.dns_deny.clone();
    c.egress.allow_private = u.allow_private;
    if let Some(mtu) = u.mtu {
        c.mtu = mtu;
    }
    c.guest_mac = guest_mac;
    for f in &u.port_forwards {
        c.forwards.push(PortForward {
            proto: if f.protocol.eq_ignore_ascii_case("udp") {
                Proto::Udp
            } else {
                Proto::Tcp
            },
            host_addr: match &f.host_addr {
                Some(a) => a.parse().context("forward host_addr")?,
                None => IpAddr::from([127, 0, 0, 1]),
            },
            host_port: f.host_port,
            guest_port: f.guest_port,
        });
    }
    c.validate().map_err(|e| anyhow::anyhow!("apple.usernet: {e}"))?;
    Ok(c)
}

/// `aa:bb:cc:dd:ee:ff` -> bytes.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut n = 0;
    for part in s.split(':') {
        if n >= 6 || part.len() != 2 {
            return None;
        }
        out[n] = u8::from_str_radix(part, 16).ok()?;
        n += 1;
    }
    (n == 6).then_some(out)
}

pub fn find_usernet() -> Result<PathBuf> {
    let mut tried = Vec::new();
    if let Some(p) = std::env::var_os("FLUXVM_USERNET") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        tried.push(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for d in [Some(dir), dir.parent()].into_iter().flatten() {
            let p = d.join("fluxvm-usernet");
            if p.is_file() {
                return Ok(p);
            }
            tried.push(p);
        }
    }
    bail!(
        "fluxvm-usernet not found (tried {}); install it next to fluxctl or set FLUXVM_USERNET",
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn set_cloexec(fd: RawFd) {
    // SAFETY: plain fcntl on a descriptor this process owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

fn pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a valid two-element out array.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("socketpair");
    }
    for fd in fds {
        set_cloexec(fd);
    }
    // SAFETY: both descriptors were just created and are owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn set_bufs(fd: RawFd) {
    // Apple's guidance for file-handle networking: a receive buffer several times the send buffer. Best effort.
    for (opt, v) in [(libc::SO_SNDBUF, 1 << 20), (libc::SO_RCVBUF, 4 << 20)] {
        let v: libc::c_int = v;
        // SAFETY: valid fd and a correctly sized int option.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw const v).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

/// Makes `src` available as descriptor `target` in a child (to be called from `pre_exec`; async-signal-safe).
///
/// # Safety
/// Only for use inside a `pre_exec` closure.
pub unsafe fn install_fd(src: RawFd, target: RawFd) -> std::io::Result<()> {
    // SAFETY: dup2/fcntl are async-signal-safe.
    unsafe {
        if src == target {
            let flags = libc::fcntl(src, libc::F_GETFD);
            if flags < 0 || libc::fcntl(src, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        } else if libc::dup2(src, target) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The gateway side of one VM's usernet NIC.
pub struct Gateway {
    /// The guest's end; inherited by the runner as [`RUNNER_NIC_FD`].
    guest_end: OwnedFd,
    gw_end: OwnedFd,
    /// Gateway's frame tap end, and the thread-side reader (only when netobs is on).
    tap: Option<(OwnedFd, UnixDatagram)>,
    config_path: PathBuf,
    vm_id: uuid::Uuid,
    log_path: PathBuf,
    bin: PathBuf,
    mac: [u8; 6],
}

impl Gateway {
    /// Creates the sockets and writes `<workspace>/usernet.json`. Nothing is spawned yet.
    pub fn prepare(
        u: &AppleUsernet,
        vm_id: uuid::Uuid,
        workspace: &Path,
        mac: &str,
    ) -> Result<Self> {
        let mac = parse_mac(mac).with_context(|| format!("apple.usernet: bad guest MAC {mac:?}"))?;
        let bin = find_usernet()?;
        let cfg = to_config(u, Some(mac))?;
        let config_path = workspace.join("usernet.json");
        std::fs::write(&config_path, serde_json::to_vec_pretty(&cfg)?)
            .with_context(|| format!("writing {}", config_path.display()))?;
        let (guest_end, gw_end) = pair()?;
        set_bufs(guest_end.as_raw_fd());
        set_bufs(gw_end.as_raw_fd());
        let tap = if fluxvm_netobs::global_tap().is_some() {
            let (tx, rx) = pair()?;
            set_bufs(rx.as_raw_fd());
            Some((tx, UnixDatagram::from(rx)))
        } else {
            None
        };
        Ok(Self {
            guest_end,
            gw_end,
            tap,
            config_path,
            vm_id,
            log_path: workspace.join("usernet.log"),
            bin,
            mac,
        })
    }

    /// The descriptor the runner must inherit as [`RUNNER_NIC_FD`].
    pub fn runner_fd(&self) -> RawFd {
        self.guest_end.as_raw_fd()
    }

    /// Runs the gateway until `runner_pid` is gone or `stop` fires/closes, restarting it if it dies in between.
    pub fn supervise(mut self, runner_pid: u32, mut stop: tokio::sync::watch::Receiver<bool>) {
        let alive = Arc::new(AtomicBool::new(true));
        if let Some((_, rx)) = self.tap.as_mut() {
            let rx = rx.try_clone();
            if let (Ok(rx), Some(tap)) = (rx, fluxvm_netobs::global_tap()) {
                let id = fluxvm_netobs::Identity::new(
                    &self.vm_id.to_string(),
                    "usernet",
                    "eth0",
                    self.mac,
                );
                spawn_tap_reader(rx, tap, id, alive.clone());
            }
        }
        tokio::spawn(async move {
            let mut quick_failures = 0u32;
            loop {
                let started = tokio::time::Instant::now();
                let mut child = match self.spawn_child(runner_pid) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(vm = %self.vm_id, error = %e, "starting fluxvm-usernet failed");
                        break;
                    }
                };
                let exited = tokio::select! {
                    r = child.wait() => Some(r),
                    _ = stop.changed() => None,
                };
                match exited {
                    None => {
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        break;
                    }
                    Some(r) => {
                        if !pid_alive(runner_pid) || *stop.borrow() {
                            break;
                        }
                        tracing::warn!(vm = %self.vm_id, status = ?r.ok(), "fluxvm-usernet exited; restarting");
                        if started.elapsed() < Duration::from_secs(5) {
                            quick_failures += 1;
                            if quick_failures >= 5 {
                                tracing::error!(
                                    vm = %self.vm_id,
                                    "fluxvm-usernet keeps failing (see usernet.log); the guest has no network"
                                );
                                break;
                            }
                        } else {
                            quick_failures = 0;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
            alive.store(false, Ordering::Relaxed);
        });
    }

    fn spawn_child(&self, runner_pid: u32) -> Result<tokio::process::Child> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            .context("opening usernet.log")?;
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("--fd")
            .arg(GATEWAY_NIC_FD.to_string())
            .arg("--config")
            .arg(&self.config_path)
            .arg("--watch-pid")
            .arg(runner_pid.to_string());
        let nic = self.gw_end.as_raw_fd();
        let tap = self.tap.as_ref().map(|(tx, _)| tx.as_raw_fd());
        if tap.is_some() {
            cmd.arg("--tap-fd").arg(GATEWAY_TAP_FD.to_string());
        }
        // SAFETY: only async-signal-safe calls (dup2/fcntl) run between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                install_fd(nic, GATEWAY_NIC_FD)?;
                if let Some(t) = tap {
                    install_fd(t, GATEWAY_TAP_FD)?;
                }
                Ok(())
            });
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0);
        cmd.spawn()
            .with_context(|| format!("starting {}", self.bin.display()))
    }
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes.
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn spawn_tap_reader(
    rx: UnixDatagram,
    tap: fluxvm_netobs::NetTap,
    id: Arc<fluxvm_netobs::Identity>,
    alive: Arc<AtomicBool>,
) {
    let _ = rx.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = std::thread::Builder::new()
        .name("fluxvm-usernet-tap".into())
        .spawn(move || {
            let mut buf = vec![0u8; 65_536 + 1];
            while alive.load(Ordering::Relaxed) {
                match rx.recv(&mut buf) {
                    Ok(0) => {}
                    Ok(n) => {
                        let dir = if buf[0] == fluxvm_usernet::TAP_TO_GUEST {
                            fluxvm_netobs::Direction::ToGuest
                        } else {
                            fluxvm_netobs::Direction::FromGuest
                        };
                        tap.tap_as(&id, dir, fluxvm_netobs::Verdict::Allowed, &buf[1..n]);
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => break,
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_core::model::AppleUsernetForward;

    fn req(apple: serde_json::Value, network: serde_json::Value) -> CreateVmRequest {
        serde_json::from_value(serde_json::json!({
            "name": "t", "image": "/tmp/i", "backend": "vz",
            "vcpus": 1, "memory_mib": 512,
            "network": network, "apple": apple,
        }))
        .unwrap()
    }

    fn check(apple: serde_json::Value, network: serde_json::Value) -> Result<()> {
        let r = req(apple, network);
        let a = r.apple.clone().unwrap();
        validate(a.usernet.as_ref().unwrap(), &r, &a)
    }

    fn user() -> serde_json::Value {
        serde_json::json!({"mode": "user"})
    }

    #[test]
    fn accepts_a_policy_with_forwards() {
        check(
            serde_json::json!({"usernet": {
                "allow_cidrs": ["93.184.216.0/24", "1.1.1.1"],
                "deny_ports": [25],
                "dns_allow": ["example.com", "*.example.org"],
                "port_forwards": [
                    {"host_port": 8080, "guest_port": 80},
                    {"protocol": "udp", "host_addr": "127.0.0.1", "host_port": 0, "guest_port": 53}
                ],
                "mtu": 1400
            }}),
            user(),
        )
        .unwrap();
    }

    #[test]
    fn rejects_other_network_modes_and_macos() {
        let u = serde_json::json!({"usernet": {}});
        let with = |k: &str, v: serde_json::Value| {
            let mut m = u.clone();
            m[k] = v;
            m
        };
        assert!(check(u.clone(), serde_json::json!({"mode": "none"})).is_err());
        assert!(check(with("bridge_interface", "en0".into()), user()).is_err());
        assert!(
            check(
                with(
                    "vmnet",
                    serde_json::json!({"mode":"host-only","subnet":"192.168.105.0","mask":"255.255.255.0"})
                ),
                user()
            )
            .is_err()
        );
        assert!(check(with("guest_os", "macos".into()), user()).is_err());
        assert!(
            check(
                u,
                serde_json::json!({"mode":"user","forwards":[{"host_port":2222,"guest_port":22}]})
            )
            .is_err(),
            "network.forwards are for NAT"
        );
    }

    #[test]
    fn rejects_bad_ports_cidrs_domains_and_mtu() {
        for bad in [
            serde_json::json!({"allow_cidrs": ["10.0.0.0/33"]}),
            serde_json::json!({"deny_cidrs": ["nope"]}),
            serde_json::json!({"allow_ports": [0]}),
            serde_json::json!({"dns_allow": ["bad name"]}),
            serde_json::json!({"mtu": 100}),
            serde_json::json!({"mtu": 9001}),
            serde_json::json!({"port_forwards": [{"host_port": 80, "guest_port": 80}]}),
            serde_json::json!({"port_forwards": [{"host_port": 2000, "guest_port": 0}]}),
            serde_json::json!({"port_forwards": [{"protocol": "icmp", "host_port": 2000, "guest_port": 1}]}),
            serde_json::json!({"port_forwards": [{"host_addr": "x", "host_port": 2000, "guest_port": 1}]}),
            serde_json::json!({"port_forwards": [
                {"host_port": 2000, "guest_port": 1}, {"host_port": 2000, "guest_port": 2}]}),
        ] {
            assert!(
                check(serde_json::json!({"usernet": bad.clone()}), user()).is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn maps_onto_the_gateway_config() {
        let u = AppleUsernet {
            allow_cidrs: vec!["10.1.0.0/16".into()],
            deny_ports: vec![25],
            dns_deny: vec!["ads.example".into()],
            allow_private: true,
            mtu: Some(1400),
            port_forwards: vec![AppleUsernetForward {
                protocol: "udp".into(),
                host_addr: None,
                host_port: 5353,
                guest_port: 53,
            }],
            ..Default::default()
        };
        let c = to_config(&u, parse_mac("02:00:00:00:00:09")).unwrap();
        assert_eq!(c.mtu, 1400);
        assert_eq!(c.guest_mac, Some([2, 0, 0, 0, 0, 9]));
        assert!(c.egress.allow_private);
        assert_eq!(c.egress.allow_cidrs.len(), 1);
        assert_eq!(c.egress.deny_ports, vec![25]);
        assert_eq!(c.egress.deny_domains, vec!["ads.example".to_string()]);
        assert_eq!(c.forwards[0].proto, Proto::Udp);
        assert_eq!(c.forwards[0].host_addr, IpAddr::from([127, 0, 0, 1]));
    }

    #[test]
    fn mac_parsing() {
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:ff"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(parse_mac("aa:bb:cc:dd:ee"), None);
        assert_eq!(parse_mac("aa:bb:cc:dd:ee:gg"), None);
    }
}
