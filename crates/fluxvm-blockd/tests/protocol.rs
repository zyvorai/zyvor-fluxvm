// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Protocol round-trips against a minimal in-test NBD client.

use fluxvm_blockd::nbd::*;
use fluxvm_blockd::{BlockBackend, CowBackend, Export, Limiter, RawFile, Server};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const IHAVEOPT: u64 = 0x49484156454f5054;
const REP_MAGIC: u64 = 0x0003e889045565a9;

struct Client {
    s: TcpStream,
    next: u64,
}

struct OptReply {
    rep: u32,
    payload: Vec<u8>,
}

impl Client {
    async fn connect(addr: std::net::SocketAddr) -> Client {
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert_eq!(s.read_u64().await.unwrap(), 0x4e42444d41474943);
        assert_eq!(s.read_u64().await.unwrap(), IHAVEOPT);
        let hs = s.read_u16().await.unwrap();
        assert_eq!(hs & 3, 3);
        s.write_u32(3).await.unwrap(); // fixed newstyle + no zeroes
        Client { s, next: 1 }
    }

    async fn opt(&mut self, opt: u32, data: &[u8]) -> Vec<OptReply> {
        self.s.write_u64(IHAVEOPT).await.unwrap();
        self.s.write_u32(opt).await.unwrap();
        self.s.write_u32(data.len() as u32).await.unwrap();
        self.s.write_all(data).await.unwrap();
        let mut out = Vec::new();
        loop {
            assert_eq!(self.s.read_u64().await.unwrap(), REP_MAGIC);
            assert_eq!(self.s.read_u32().await.unwrap(), opt);
            let rep = self.s.read_u32().await.unwrap();
            let n = self.s.read_u32().await.unwrap();
            let mut payload = vec![0u8; n as usize];
            self.s.read_exact(&mut payload).await.unwrap();
            let last = rep != REP_INFO && rep != REP_SERVER;
            out.push(OptReply { rep, payload });
            if last {
                return out;
            }
        }
    }

    /// OPT_GO; returns (size, flags).
    async fn go(&mut self, name: &str) -> Result<(u64, u16), u32> {
        let mut d = (name.len() as u32).to_be_bytes().to_vec();
        d.extend_from_slice(name.as_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        let r = self.opt(OPT_GO, &d).await;
        let last = r.last().unwrap();
        if last.rep != REP_ACK {
            return Err(last.rep);
        }
        let info = &r[0].payload;
        assert_eq!(
            u16::from_be_bytes(info[0..2].try_into().unwrap()),
            INFO_EXPORT
        );
        Ok((
            u64::from_be_bytes(info[2..10].try_into().unwrap()),
            u16::from_be_bytes(info[10..12].try_into().unwrap()),
        ))
    }

    async fn req(&mut self, flags: u16, cmd: u16, off: u64, len: u32, data: &[u8]) -> (u32, u64) {
        let h = self.next;
        self.next += 1;
        self.s.write_u32(0x25609513).await.unwrap();
        self.s.write_u16(flags).await.unwrap();
        self.s.write_u16(cmd).await.unwrap();
        self.s.write_u64(h).await.unwrap();
        self.s.write_u64(off).await.unwrap();
        self.s.write_u32(len).await.unwrap();
        self.s.write_all(data).await.unwrap();
        (0, h)
    }

    async fn reply(&mut self, handle: u64) -> u32 {
        assert_eq!(self.s.read_u32().await.unwrap(), 0x67446698);
        let err = self.s.read_u32().await.unwrap();
        assert_eq!(self.s.read_u64().await.unwrap(), handle);
        err
    }

    async fn read(&mut self, off: u64, len: u32) -> Result<Vec<u8>, u32> {
        let (_, h) = self.req(0, CMD_READ, off, len, &[]).await;
        let e = self.reply(h).await;
        if e != 0 {
            return Err(e);
        }
        let mut v = vec![0u8; len as usize];
        self.s.read_exact(&mut v).await.unwrap();
        Ok(v)
    }

    async fn write(&mut self, off: u64, data: &[u8]) -> u32 {
        let (_, h) = self.req(0, CMD_WRITE, off, data.len() as u32, data).await;
        self.reply(h).await
    }

    async fn simple(&mut self, cmd: u16, off: u64, len: u32) -> u32 {
        let (_, h) = self.req(0, cmd, off, len, &[]).await;
        self.reply(h).await
    }
}

async fn start(exports: Vec<Export>) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(Server::new(exports).serve(l));
    addr
}

fn raw_export(dir: &std::path::Path, name: &str, size: usize, ro: bool) -> Export {
    let p = dir.join(format!("{name}.img"));
    std::fs::write(&p, vec![0u8; size]).unwrap();
    Export::new(name, Arc::new(RawFile::open(&p, ro).unwrap())).read_only(ro)
}

#[tokio::test]
async fn handshake_go_and_rw_roundtrip() {
    let d = tempfile::tempdir().unwrap();
    let addr = start(vec![raw_export(d.path(), "disk0", 1 << 20, false)]).await;
    let mut c = Client::connect(addr).await;
    let (size, flags) = c.go("disk0").await.unwrap();
    assert_eq!(size, 1 << 20);
    assert!(flags & FLAG_SEND_FLUSH != 0 && flags & FLAG_SEND_TRIM != 0);
    assert!(flags & FLAG_READ_ONLY == 0);

    let data: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
    assert_eq!(c.write(4096, &data).await, 0);
    assert_eq!(c.simple(CMD_FLUSH, 0, 0).await, 0);
    assert_eq!(c.read(4096, 5000).await.unwrap(), data);
    assert_eq!(c.simple(CMD_TRIM, 0, 1 << 20).await, 0);
    assert_eq!(c.read(4096, 16).await.unwrap(), vec![0u8; 16]);
    // out of range
    assert_eq!(c.read(1 << 20, 1).await.unwrap_err(), EINVAL);
    assert_eq!(c.write((1 << 20) - 1, &[1, 2]).await, ENOSPC);
    // still healthy after errors
    assert_eq!(c.write(0, b"ok").await, 0);
    let (_, h) = c.req(0, CMD_DISC, 0, 0, &[]).await;
    let _ = h;
}

#[tokio::test]
async fn export_name_option_and_list_and_unknown() {
    let d = tempfile::tempdir().unwrap();
    let addr = start(vec![
        raw_export(d.path(), "a", 8192, false),
        raw_export(d.path(), "b", 16384, false),
    ])
    .await;

    let mut c = Client::connect(addr).await;
    let l = c.opt(OPT_LIST, &[]).await;
    assert_eq!(l.len(), 3);
    assert_eq!(&l[0].payload[4..], b"a");
    assert_eq!(&l[1].payload[4..], b"b");
    assert_eq!(c.go("nope").await.unwrap_err(), REP_ERR_UNKNOWN);
    assert_eq!(c.opt(0x1234, &[]).await[0].rep, REP_ERR_UNSUP);
    // INFO does not enter transmission; GO afterwards does.
    let mut d2 = 1u32.to_be_bytes().to_vec();
    d2.extend_from_slice(b"b");
    d2.extend_from_slice(&0u16.to_be_bytes());
    assert_eq!(c.opt(OPT_INFO, &d2).await.last().unwrap().rep, REP_ACK);
    assert_eq!(c.go("b").await.unwrap().0, 16384);

    // Legacy EXPORT_NAME path (no zeroes was negotiated).
    let mut c2 = Client::connect(addr).await;
    c2.s.write_u64(IHAVEOPT).await.unwrap();
    c2.s.write_u32(OPT_EXPORT_NAME).await.unwrap();
    c2.s.write_u32(1).await.unwrap();
    c2.s.write_all(b"a").await.unwrap();
    assert_eq!(c2.s.read_u64().await.unwrap(), 8192);
    let _flags = c2.s.read_u16().await.unwrap();
    assert_eq!(c2.write(0, b"hi").await, 0);
    assert_eq!(c2.read(0, 2).await.unwrap(), b"hi");
}

#[tokio::test]
async fn read_only_export_rejects_mutation() {
    let d = tempfile::tempdir().unwrap();
    let addr = start(vec![raw_export(d.path(), "ro", 8192, true)]).await;
    let mut c = Client::connect(addr).await;
    let (_, flags) = c.go("ro").await.unwrap();
    assert!(flags & FLAG_READ_ONLY != 0);
    assert_eq!(c.write(0, b"x").await, EPERM);
    assert_eq!(c.simple(CMD_TRIM, 0, 4096).await, EPERM);
    assert_eq!(c.read(0, 4).await.unwrap(), vec![0; 4]);
}

#[tokio::test]
async fn cow_export_over_nbd_with_branch() {
    let d = tempfile::tempdir().unwrap();
    let base = d.path().join("base.img");
    std::fs::write(&base, vec![0xCDu8; 1 << 20]).unwrap();
    let a = CowBackend::create(&d.path().join("a.ovl"), &base, None).unwrap();
    let b = a.branch(&d.path().join("b.ovl")).unwrap();
    let addr = start(vec![
        Export::new("a", a.clone()).read_only(true),
        Export::new("b", b.clone()),
    ])
    .await;

    let mut cb = Client::connect(addr).await;
    cb.go("b").await.unwrap();
    assert_eq!(cb.read(100, 2).await.unwrap(), [0xCD, 0xCD]);
    assert_eq!(cb.write(100, &[1, 2]).await, 0);
    assert_eq!(cb.simple(CMD_FLUSH, 0, 0).await, 0);
    assert_eq!(cb.read(99, 4).await.unwrap(), [0xCD, 1, 2, 0xCD]);

    let mut ca = Client::connect(addr).await;
    ca.go("a").await.unwrap();
    assert_eq!(ca.read(99, 4).await.unwrap(), [0xCD; 4]);
    assert_eq!(ca.write(0, b"x").await, EPERM);
    assert_eq!(b.size(), 1 << 20);
}

/// A frozen overlay (it has a child) cannot take writes; exporting it must say so (read-only flag, EPERM) rather than
/// advertise a writable disk whose writes fail with EIO inside the guest (found booting a real Debian guest).
#[tokio::test]
async fn frozen_overlay_is_exported_read_only_without_asking() {
    let d = tempfile::tempdir().unwrap();
    let base = d.path().join("base.img");
    std::fs::write(&base, vec![0xABu8; 1 << 20]).unwrap();
    let a = CowBackend::create(&d.path().join("a.ovl"), &base, None).unwrap();
    let addr = start(vec![Export::new("a", a.clone())]).await;

    // Writable while it has no children.
    let mut c = Client::connect(addr).await;
    let (_, flags) = c.go("a").await.unwrap();
    assert!(flags & FLAG_READ_ONLY == 0);
    assert_eq!(c.write(0, b"x").await, 0);

    // Branching freezes it: the same export now advertises and enforces read-only.
    let _b = a.branch(&d.path().join("b.ovl")).unwrap();
    assert_eq!(c.write(0, b"y").await, EPERM, "an open connection sees the freeze at once");
    let mut c2 = Client::connect(addr).await;
    let (_, flags) = c2.go("a").await.unwrap();
    assert!(flags & FLAG_READ_ONLY != 0, "new connections get the read-only flag");
    assert_eq!(c2.read(0, 1).await.unwrap(), b"x");
}

#[tokio::test]
async fn limiter_throttles_over_the_wire() {
    let d = tempfile::tempdir().unwrap();
    let e = raw_export(d.path(), "l", 1 << 20, false).limiter(Limiter::new(None, Some(20)));
    let addr = start(vec![e]).await;
    let mut c = Client::connect(addr).await;
    c.go("l").await.unwrap();
    let t = Instant::now();
    for _ in 0..15 {
        c.read(0, 512).await.unwrap();
    }
    assert!(
        t.elapsed() >= Duration::from_millis(400),
        "{:?}",
        t.elapsed()
    );
}

/// Needs `qemu-nbd`/`qemu-img` on PATH; run with `--ignored`. Starts our
/// server and has qemu-img read it as a client.
#[tokio::test]
#[ignore]
async fn interop_qemu_img() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("x.img");
    std::fs::write(&p, vec![0x5Au8; 1 << 20]).unwrap();
    let addr = start(vec![Export::new(
        "x",
        Arc::new(RawFile::open(&p, false).unwrap()),
    )])
    .await;
    let url = format!("nbd://127.0.0.1:{}/x", addr.port());
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("qemu-img")
            .args(["info", &url])
            .output()
    })
    .await
    .unwrap()
    .expect("qemu-img not installed");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
