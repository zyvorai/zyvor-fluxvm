// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Outbound ICMP echo proxy. The guest's echo request is re-sent from an unprivileged ICMP datagram socket
//! (`socket(AF_INET, SOCK_DGRAM, IPPROTO_ICMP)`, no root on macOS; on Linux it needs `net.ipv4.ping_group_range`
//! to include the gateway's group). One socket per (destination, echo identifier) keeps replies apart. The kernel
//! may rewrite the identifier on the wire, so replies are matched by sequence number against the outstanding
//! requests of the flow and the guest's original identifier is restored.
//!
//! When the socket cannot be created the request is denied with reason `icmp-socket-unavailable: <errno text>`
//! and nothing is sent: the guest's ping times out like it would behind a firewall that drops ICMP.

use crate::wire;
use crate::{Direction, Engine, Ev, FlowKind, Proto};
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::task::AbortHandle;

/// (destination, guest echo identifier)
pub(crate) type IcmpKey = (Ipv4Addr, u16);

const IDLE: Duration = Duration::from_secs(30);
const MAX_PENDING: usize = 64;

pub(crate) struct IcmpFlow {
    sock: Arc<std::net::UdpSocket>,
    task: AbortHandle,
    last: Instant,
    pending: VecDeque<u16>,
    up: u64,
    down: u64,
}

impl Drop for IcmpFlow {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An unprivileged ICMP echo socket connected to `dst`.
fn open_socket(dst: Ipv4Addr) -> std::io::Result<std::net::UdpSocket> {
    // SAFETY: plain socket(2); the result is checked and immediately owned.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh, valid, exclusively owned descriptor.
    let s = std::net::UdpSocket::from(unsafe { OwnedFd::from_raw_fd(fd) });
    s.set_nonblocking(true)?;
    s.connect(SocketAddr::new(dst.into(), 0))?;
    Ok(s)
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

impl Engine {
    fn icmp_token(&mut self, now: Instant) -> bool {
        let rate = f64::from(self.cfg.icmp_rate_per_sec);
        if rate == 0.0 {
            return true; // limiter disabled
        }
        let (tokens, last) = &mut self.icmp_bucket;
        let cap = rate * 2.0;
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * rate).min(cap);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// A guest echo request (`p` is the ICMP message) towards a non-gateway IPv4 address.
    pub(crate) fn icmp_request(&mut self, dst: Ipv4Addr, p: &[u8], now: Instant) {
        let (id, seq) = (be16(&p[4..6]), be16(&p[6..8]));
        let guest = self.guest_addr(id);
        let rip = dst.into();
        if !self.cfg.icmp_proxy {
            return self.deny_from(Proto::Icmp, guest, rip, 0, "icmp-disabled");
        }
        if let crate::Decision::Deny(why) = self.cfg.egress.check_addr(dst, &self.learned, now) {
            return self.deny_from(Proto::Icmp, guest, rip, 0, why);
        }
        let key = (dst, id);
        if !self.icmp.contains_key(&key) {
            if self.icmp.len() >= self.cfg.max_icmp_flows {
                return self.deny_from(Proto::Icmp, guest, rip, 0, "flow-limit");
            }
            if !self.icmp_token(now) {
                return self.deny_from(Proto::Icmp, guest, rip, 0, "rate-limit");
            }
            let sock = match open_socket(dst) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "usernet: ICMP datagram socket unavailable; echo requests are dropped");
                    return self.deny_from(
                        Proto::Icmp,
                        guest,
                        rip,
                        0,
                        &format!("icmp-socket-unavailable: {e}"),
                    );
                }
            };
            let recv = match sock.try_clone().and_then(UdpSocket::from_std) {
                Ok(r) => r,
                Err(e) => {
                    return self.deny_from(Proto::Icmp, guest, rip, 0, &format!("socket: {e}"));
                }
            };
            let tx = self.ev_tx.clone();
            let task = tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                while let Ok(n) = recv.recv(&mut buf).await {
                    if tx
                        .send(Ev::IcmpReply {
                            key,
                            data: buf[..n].to_vec(),
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .abort_handle();
            let e = self.log(
                FlowKind::Open,
                Proto::Icmp,
                Direction::Outbound,
                guest,
                SocketAddr::new(rip, 0),
            );
            self.publish(e);
            self.icmp.insert(
                key,
                IcmpFlow {
                    sock: Arc::new(sock),
                    task,
                    last: now,
                    pending: VecDeque::new(),
                    up: 0,
                    down: 0,
                },
            );
        } else if !self.icmp_token(now) {
            return self.deny_from(Proto::Icmp, guest, rip, 0, "rate-limit");
        }
        let Some(f) = self.icmp.get_mut(&key) else {
            return;
        };
        f.last = now;
        f.up += p.len().saturating_sub(8) as u64;
        if f.pending.len() >= MAX_PENDING {
            f.pending.pop_front();
        }
        f.pending.push_back(seq);
        let _ = f.sock.send(p);
    }

    pub(crate) fn on_icmp_reply(&mut self, key: IcmpKey, data: &[u8], now: Instant) {
        let guest_ip = self.cfg.guest_ip;
        let Some(f) = self.icmp.get_mut(&key) else {
            return;
        };
        let mut p = data;
        // macOS delivers the IP header with datagram ICMP sockets, Linux does not.
        if p.len() >= 20 && p[0] >> 4 == 4 {
            let ihl = usize::from(p[0] & 0xf) * 4;
            if p.len() >= ihl {
                p = &p[ihl..];
            }
        }
        if p.len() < 8 || p[0] != 0 || p[1] != 0 {
            return;
        }
        let seq = be16(&p[6..8]);
        let Some(pos) = f.pending.iter().position(|s| *s == seq) else {
            return;
        };
        f.pending.remove(pos);
        f.last = now;
        f.down += p.len() as u64 - 8;
        let mut r = p.to_vec();
        r[2] = 0;
        r[3] = 0;
        r[4..6].copy_from_slice(&key.1.to_be_bytes());
        let c = wire::checksum(&r);
        r[2..4].copy_from_slice(&c.to_be_bytes());
        self.send_ip(key.0.into(), guest_ip.into(), wire::PROTO_ICMP, &r, false);
    }

    pub(crate) fn icmp_tick(&mut self, now: Instant) {
        let expired: Vec<IcmpKey> = self
            .icmp
            .iter()
            .filter(|(_, f)| now.duration_since(f.last) > IDLE)
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            if let Some(f) = self.icmp.remove(&k) {
                let mut e = self.log(
                    FlowKind::Close,
                    Proto::Icmp,
                    Direction::Outbound,
                    self.guest_addr(k.1),
                    SocketAddr::new(k.0.into(), 0),
                );
                e.bytes_tx = f.up;
                e.bytes_rx = f.down;
                self.publish(e);
            }
        }
    }
}
