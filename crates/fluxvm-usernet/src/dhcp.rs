// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! A one-lease DHCPv4 server: the guest always gets `guest_ip`.

use crate::config::UserNetConfig;
use crate::wire::Mac;
use std::net::Ipv4Addr;

pub const DISCOVER: u8 = 1;
pub const OFFER: u8 = 2;
pub const REQUEST: u8 = 3;
pub const NAK: u8 = 6;
pub const ACK: u8 = 5;
const MAGIC: [u8; 4] = [99, 130, 83, 99];

pub struct Msg {
    pub xid: [u8; 4],
    pub flags: [u8; 2],
    pub ciaddr: Ipv4Addr,
    pub chaddr: Mac,
    pub msg_type: u8,
    pub requested_ip: Option<Ipv4Addr>,
}

pub fn parse(p: &[u8]) -> Option<Msg> {
    if p.len() < 240 || p[0] != 1 || p[1] != 1 || p[236..240] != MAGIC {
        return None;
    }
    let mut msg_type = 0;
    let mut requested_ip = None;
    let mut o = &p[240..];
    while let Some(&code) = o.first() {
        if code == 255 {
            break;
        }
        if code == 0 {
            o = &o[1..];
            continue;
        }
        let len = usize::from(*o.get(1)?);
        let val = o.get(2..2 + len)?;
        match (code, len) {
            (53, 1) => msg_type = val[0],
            (50, 4) => requested_ip = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
            _ => {}
        }
        o = &o[2 + len..];
    }
    Some(Msg {
        xid: p[4..8].try_into().ok()?,
        flags: p[10..12].try_into().ok()?,
        ciaddr: Ipv4Addr::new(p[12], p[13], p[14], p[15]),
        chaddr: p[28..34].try_into().ok()?,
        msg_type,
        requested_ip,
    })
}

/// Build a BOOTREPLY. `yiaddr` is zero for a NAK.
pub fn reply(cfg: &UserNetConfig, req: &Msg, msg_type: u8) -> Vec<u8> {
    let mut v = vec![0u8; 240];
    v[0] = 2;
    v[1] = 1;
    v[2] = 6;
    v[4..8].copy_from_slice(&req.xid);
    v[10..12].copy_from_slice(&req.flags);
    if msg_type != NAK {
        v[16..20].copy_from_slice(&cfg.guest_ip.octets());
    }
    v[20..24].copy_from_slice(&cfg.gateway_ip.octets()); // siaddr
    v[28..34].copy_from_slice(&req.chaddr);
    v[236..240].copy_from_slice(&MAGIC);
    let mut opt = |code: u8, val: &[u8]| {
        v.push(code);
        v.push(val.len() as u8);
        v.extend_from_slice(val);
    };
    opt(53, &[msg_type]);
    opt(54, &cfg.gateway_ip.octets());
    if msg_type != NAK {
        let lease = cfg.dhcp_lease_secs;
        opt(51, &lease.to_be_bytes());
        opt(58, &(lease / 2).to_be_bytes());
        opt(59, &(lease / 8 * 7).to_be_bytes());
        opt(1, &cfg.netmask().octets());
        opt(3, &cfg.gateway_ip.octets());
        opt(6, &cfg.dns_ip.octets());
        opt(28, &cfg.broadcast().octets());
        opt(26, &cfg.mtu.to_be_bytes());
    }
    v.push(255);
    while v.len() < 300 {
        v.push(0);
    }
    v
}

#[cfg(test)]
pub(crate) fn test_request(mac: Mac, msg_type: u8, xid: [u8; 4], requested: Option<Ipv4Addr>) -> Vec<u8> {
    let mut v = vec![0u8; 240];
    v[0] = 1;
    v[1] = 1;
    v[2] = 6;
    v[4..8].copy_from_slice(&xid);
    v[28..34].copy_from_slice(&mac);
    v[236..240].copy_from_slice(&MAGIC);
    v.extend_from_slice(&[53, 1, msg_type]);
    if let Some(r) = requested {
        v.extend_from_slice(&[50, 4]);
        v.extend_from_slice(&r.octets());
    }
    v.push(255);
    v
}

#[cfg(test)]
pub(crate) fn test_option(reply: &[u8], code: u8) -> Option<Vec<u8>> {
    let mut o = &reply[240..];
    while let Some(&c) = o.first() {
        if c == 255 {
            return None;
        }
        if c == 0 {
            o = &o[1..];
            continue;
        }
        let l = usize::from(o[1]);
        if c == code {
            return Some(o[2..2 + l].to_vec());
        }
        o = &o[2 + l..];
    }
    None
}
