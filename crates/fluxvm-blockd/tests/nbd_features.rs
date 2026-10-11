// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Structured replies, parallel in-flight requests, flush barrier, resize
//! advertisement and the CLI subcommands, against an in-test NBD client.

use fluxvm_blockd::nbd::*;
use fluxvm_blockd::{BlockBackend, CowBackend, Export, RawFile, Server};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const IHAVEOPT: u64 = 0x49484156454f5054;
const REP_MAGIC: u64 = 0x0003e889045565a9;

struct Client {
    s: TcpStream,
    next: u64,
}

enum Reply {
    Simple {
        err: u32,
    },
    /// Structured: chunks as (offset, data), plus error if any.
    Structured {
        chunks: Vec<(u64, Vec<u8>)>,
        err: Option<u32>,
        frames: usize,
    },
}

impl Client {
    async fn connect(addr: std::net::SocketAddr, structured: bool) -> Client {
        let mut s = TcpStream::connect(addr).await.unwrap();
        assert_eq!(s.read_u64().await.unwrap(), 0x4e42444d41474943);
        assert_eq!(s.read_u64().await.unwrap(), IHAVEOPT);
        let _ = s.read_u16().await.unwrap();
        s.write_u32(3).await.unwrap();
        let mut c = Client { s, next: 1 };
        if structured {
            let r = c.opt(OPT_STRUCTURED_REPLY, &[]).await;
            assert_eq!(r, REP_ACK);
        }
        c
    }

    async fn opt(&mut self, opt: u32, data: &[u8]) -> u32 {
        self.s.write_u64(IHAVEOPT).await.unwrap();
        self.s.write_u32(opt).await.unwrap();
        self.s.write_u32(data.len() as u32).await.unwrap();
        self.s.write_all(data).await.unwrap();
        assert_eq!(self.s.read_u64().await.unwrap(), REP_MAGIC);
        assert_eq!(self.s.read_u32().await.unwrap(), opt);
        let rep = self.s.read_u32().await.unwrap();
        let n = self.s.read_u32().await.unwrap();
        let mut p = vec![0u8; n as usize];
        self.s.read_exact(&mut p).await.unwrap();
        rep
    }

    async fn go(&mut self, name: &str) -> (u64, u16) {
        let mut d = (name.len() as u32).to_be_bytes().to_vec();
        d.extend_from_slice(name.as_bytes());
        d.extend_from_slice(&0u16.to_be_bytes());
        self.s.write_u64(IHAVEOPT).await.unwrap();
        self.s.write_u32(OPT_GO).await.unwrap();
        self.s.write_u32(d.len() as u32).await.unwrap();
        self.s.write_all(&d).await.unwrap();
        let mut info = None;
        loop {
            assert_eq!(self.s.read_u64().await.unwrap(), REP_MAGIC);
            let _ = self.s.read_u32().await.unwrap();
            let rep = self.s.read_u32().await.unwrap();
            let n = self.s.read_u32().await.unwrap();
            let mut p = vec![0u8; n as usize];
            self.s.read_exact(&mut p).await.unwrap();
            match rep {
                REP_INFO if u16::from_be_bytes([p[0], p[1]]) == INFO_EXPORT => {
                    info = Some((
                        u64::from_be_bytes(p[2..10].try_into().unwrap()),
                        u16::from_be_bytes(p[10..12].try_into().unwrap()),
                    ));
                }
                REP_INFO => {}
                REP_ACK => return info.unwrap(),
                other => panic!("go failed: {other:#x}"),
            }
        }
    }

    async fn send(&mut self, flags: u16, cmd: u16, off: u64, len: u32, data: &[u8]) -> u64 {
        let h = self.next;
        self.next += 1;
        let mut b = Vec::new();
        b.extend_from_slice(&0x25609513u32.to_be_bytes());
        b.extend_from_slice(&flags.to_be_bytes());
        b.extend_from_slice(&cmd.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&off.to_be_bytes());
        b.extend_from_slice(&len.to_be_bytes());
        b.extend_from_slice(data);
        self.s.write_all(&b).await.unwrap();
        h
    }

    /// Read one complete reply (all structured chunks up to DONE).
    async fn recv(&mut self) -> (u64, Reply) {
        let magic = self.s.read_u32().await.unwrap();
        if magic == 0x67446698 {
            let err = self.s.read_u32().await.unwrap();
            let h = self.s.read_u64().await.unwrap();
            // Data only follows successful simple READs; callers using
            // `recv_read_simple` handle that. Here: no data.
            return (h, Reply::Simple { err });
        }
        assert_eq!(magic, STRUCTURED_REPLY_MAGIC);
        let (mut chunks, mut err, mut frames) = (Vec::new(), None, 0);
        let mut handle;
        loop {
            let flags = self.s.read_u16().await.unwrap();
            let ty = self.s.read_u16().await.unwrap();
            handle = self.s.read_u64().await.unwrap();
            let len = self.s.read_u32().await.unwrap() as usize;
            let mut p = vec![0u8; len];
            self.s.read_exact(&mut p).await.unwrap();
            frames += 1;
            match ty {
                REPLY_TYPE_OFFSET_DATA => chunks.push((
                    u64::from_be_bytes(p[0..8].try_into().unwrap()),
                    p[8..].to_vec(),
                )),
                REPLY_TYPE_ERROR => err = Some(u32::from_be_bytes(p[0..4].try_into().unwrap())),
                REPLY_TYPE_NONE => {}
                t => panic!("unexpected chunk type {t}"),
            }
            if flags & REPLY_FLAG_DONE != 0 {
                return (
                    handle,
                    Reply::Structured {
                        chunks,
                        err,
                        frames,
                    },
                );
            }
            // Next chunk begins with a fresh magic.
            assert_eq!(self.s.read_u32().await.unwrap(), STRUCTURED_REPLY_MAGIC);
        }
    }

    async fn simple_ok(&mut self, cmd: u16, flags: u16, off: u64, len: u32, data: &[u8]) -> u32 {
        let h = self.send(flags, cmd, off, len, data).await;
        match self.recv().await {
            (rh, Reply::Simple { err, .. }) => {
                assert_eq!(rh, h);
                err
            }
            _ => panic!("expected simple reply"),
        }
    }

    async fn read_simple(&mut self, off: u64, len: u32) -> Vec<u8> {
        let h = self.send(0, CMD_READ, off, len, &[]).await;
        let magic = self.s.read_u32().await.unwrap();
        assert_eq!(magic, 0x67446698);
        assert_eq!(self.s.read_u32().await.unwrap(), 0);
        assert_eq!(self.s.read_u64().await.unwrap(), h);
        let mut v = vec![0u8; len as usize];
        self.s.read_exact(&mut v).await.unwrap();
        v
    }
}

async fn start(exports: Vec<Export>) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(Server::new(exports).serve(l));
    addr
}

fn raw(dir: &std::path::Path, name: &str, size: usize) -> Arc<RawFile> {
    let p = dir.join(format!("{name}.img"));
    std::fs::write(&p, vec![0u8; size]).unwrap();
    Arc::new(RawFile::open(&p, false).unwrap())
}

#[tokio::test]
async fn structured_read_chunks_dont_fragment_and_errors() {
    let d = tempfile::tempdir().unwrap();
    let addr = start(vec![Export::new("s", raw(d.path(), "s", 8 << 20))]).await;
    let mut c = Client::connect(addr, true).await;
    let (size, flags) = c.go("s").await;
    assert_eq!(size, 8 << 20);
    assert!(flags & FLAG_SEND_DF != 0);

    let data: Vec<u8> = (0..(3 << 20) + 17u32)
        .map(|i| ((i * 31) >> 3) as u8)
        .collect();
    assert_eq!(
        c.simple_ok(CMD_WRITE, 0, 4096, data.len() as u32, &data)
            .await,
        0
    );

    // Fragmented: >1 MiB split into several OFFSET_DATA chunks, in order.
    let h = c.send(0, CMD_READ, 4096, data.len() as u32, &[]).await;
    let (rh, r) = c.recv().await;
    assert_eq!(rh, h);
    let Reply::Structured {
        chunks,
        err,
        frames,
    } = r
    else {
        panic!()
    };
    assert!(err.is_none());
    assert!(frames >= 4, "{frames}");
    let mut got = Vec::new();
    for (o, d) in &chunks {
        assert_eq!(*o, 4096 + got.len() as u64);
        got.extend_from_slice(d);
    }
    assert_eq!(got, data);

    // DF: exactly one chunk.
    let h = c
        .send(CMDF_DF, CMD_READ, 4096, data.len() as u32, &[])
        .await;
    let (rh, r) = c.recv().await;
    assert_eq!(rh, h);
    let Reply::Structured { chunks, frames, .. } = r else {
        panic!()
    };
    assert_eq!(
        (frames, chunks.len(), chunks[0].1.len()),
        (1, 1, data.len())
    );

    // Error is a structured ERROR chunk.
    let h = c.send(0, CMD_READ, 8 << 20, 1, &[]).await;
    let (rh, r) = c.recv().await;
    assert_eq!(rh, h);
    let Reply::Structured { err, chunks, .. } = r else {
        panic!()
    };
    assert_eq!(err, Some(EINVAL));
    assert!(chunks.is_empty());

    // Non-read commands still use simple replies; connection stays healthy.
    assert_eq!(c.simple_ok(CMD_FLUSH, 0, 0, 0, &[]).await, 0);
    assert_eq!(c.simple_ok(CMD_TRIM, 0, 0, 4096, &[]).await, 0);
}

#[tokio::test]
async fn clients_without_structured_negotiation_get_simple_replies() {
    let d = tempfile::tempdir().unwrap();
    let addr = start(vec![Export::new("s", raw(d.path(), "s", 1 << 20))]).await;
    let mut c = Client::connect(addr, false).await;
    c.go("s").await;
    assert_eq!(c.simple_ok(CMD_WRITE, 0, 0, 4, b"abcd").await, 0);
    assert_eq!(c.read_simple(0, 4).await, b"abcd");
    // DF without structured replies is invalid.
    assert_eq!(c.simple_ok(CMD_READ, CMDF_DF, 0, 4, &[]).await, EINVAL);
}

/// Backend that records concurrency and ordering.
struct Probe {
    inner: RawFile,
    delay: Duration,
    cur: AtomicUsize,
    max: AtomicUsize,
    writes_done: AtomicUsize,
    flushes: Mutex<Vec<usize>>, // writes_done observed at each flush
}

impl Probe {
    fn enter(&self) {
        let c = self.cur.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(c, Ordering::SeqCst);
        std::thread::sleep(self.delay);
    }
    fn leave(&self) {
        self.cur.fetch_sub(1, Ordering::SeqCst);
    }
}

impl BlockBackend for Probe {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        self.enter();
        let r = self.inner.read_at(off, buf);
        self.leave();
        r
    }
    fn write_at(&self, off: u64, data: &[u8]) -> io::Result<()> {
        self.enter();
        let r = self.inner.write_at(off, data);
        self.writes_done.fetch_add(1, Ordering::SeqCst);
        self.leave();
        r
    }
    fn flush(&self) -> io::Result<()> {
        self.flushes
            .lock()
            .unwrap()
            .push(self.writes_done.load(Ordering::SeqCst));
        Ok(())
    }
    fn write_zeroes(&self, off: u64, len: u64) -> io::Result<()> {
        self.inner.write_zeroes(off, len)
    }
}

fn probe(d: &std::path::Path, delay_ms: u64) -> Arc<Probe> {
    Arc::new(Probe {
        inner: Arc::try_unwrap(raw(d, "p", 1 << 20)).ok().unwrap(),
        delay: Duration::from_millis(delay_ms),
        cur: 0.into(),
        max: 0.into(),
        writes_done: 0.into(),
        flushes: Mutex::new(vec![]),
    })
}

#[tokio::test]
async fn requests_run_in_parallel_and_replies_match_by_handle() {
    let d = tempfile::tempdir().unwrap();
    let p = probe(d.path(), 150);
    let addr = start(vec![Export::new("p", p.clone())]).await;
    let mut c = Client::connect(addr, false).await;
    c.go("p").await;
    let t = Instant::now();
    let mut hs = Vec::new();
    for i in 0..8u64 {
        hs.push(c.send(0, CMD_READ, i * 512, 512, &[]).await);
    }
    let mut seen = HashMap::new();
    for _ in 0..8 {
        assert_eq!(c.s.read_u32().await.unwrap(), 0x67446698);
        assert_eq!(c.s.read_u32().await.unwrap(), 0);
        let h = c.s.read_u64().await.unwrap();
        let mut b = vec![0u8; 512];
        c.s.read_exact(&mut b).await.unwrap();
        seen.insert(h, b);
    }
    for h in hs {
        assert!(seen.contains_key(&h));
    }
    let el = t.elapsed();
    assert!(
        el < Duration::from_millis(8 * 150 - 200),
        "not parallel: {el:?}"
    );
    assert!(
        p.max.load(Ordering::SeqCst) >= 4,
        "max concurrency {}",
        p.max.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn flush_is_a_barrier_and_fua_flushes() {
    let d = tempfile::tempdir().unwrap();
    let p = probe(d.path(), 100);
    let addr = start(vec![Export::new("p", p.clone())]).await;
    let mut c = Client::connect(addr, false).await;
    c.go("p").await;
    let mut write_handles = Vec::new();
    for i in 0..4u64 {
        write_handles.push(c.send(0, CMD_WRITE, i * 8, 8, &[i as u8; 8]).await);
    }
    let fh = c.send(0, CMD_FLUSH, 0, 0, &[]).await;
    // A read issued after the flush must observe all four writes.
    let rh = c.send(0, CMD_READ, 0, 32, &[]).await;
    let mut order = Vec::new();
    let mut read_data = None;
    for _ in 0..6 {
        assert_eq!(c.s.read_u32().await.unwrap(), 0x67446698);
        assert_eq!(c.s.read_u32().await.unwrap(), 0);
        let h = c.s.read_u64().await.unwrap();
        if h == rh {
            let mut b = vec![0u8; 32];
            c.s.read_exact(&mut b).await.unwrap();
            read_data = Some(b);
        }
        order.push(h);
    }
    let fpos = order.iter().position(|h| *h == fh).unwrap();
    for w in &write_handles {
        assert!(
            order.iter().position(|h| h == w).unwrap() < fpos,
            "write after flush: {order:?}"
        );
    }
    assert_eq!(
        order.iter().position(|h| *h == rh).unwrap(),
        5,
        "read must follow flush"
    );
    assert_eq!(
        p.flushes.lock().unwrap().clone(),
        vec![4],
        "flush saw all 4 writes"
    );
    let rd = read_data.unwrap();
    assert_eq!(&rd[0..8], &[0u8; 8]);
    assert_eq!(&rd[24..32], &[3u8; 8]);

    // FUA write flushes the backend before replying.
    let before = p.flushes.lock().unwrap().len();
    assert_eq!(c.simple_ok(CMD_WRITE, CMDF_FUA, 100, 4, b"fua!").await, 0);
    assert_eq!(p.flushes.lock().unwrap().len(), before + 1);
}

#[tokio::test]
async fn grown_overlay_is_advertised_on_next_connect() {
    let d = tempfile::tempdir().unwrap();
    let base = d.path().join("base.img");
    std::fs::write(&base, vec![7u8; 1 << 20]).unwrap();
    let ov = CowBackend::create(&d.path().join("o.ovl"), &base, None).unwrap();
    let addr = start(vec![Export::new("o", ov.clone())]).await;
    let mut c1 = Client::connect(addr, false).await;
    assert_eq!(c1.go("o").await.0, 1 << 20);
    ov.grow(4 << 20).unwrap();
    let mut c2 = Client::connect(addr, false).await;
    assert_eq!(c2.go("o").await.0, 4 << 20);
    assert_eq!(c2.simple_ok(CMD_WRITE, 0, (4 << 20) - 2, 2, b"ok").await, 0);
    assert_eq!(c2.read_simple((4 << 20) - 2, 2).await, b"ok");
    assert_eq!(c2.read_simple(1 << 20, 4).await, [0; 4]);
    // The old connection keeps its negotiated size.
    assert_eq!(c1.simple_ok(CMD_READ, 0, 2 << 20, 1, &[]).await, EINVAL);
}

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_fluxvm-blockd"))
}

#[test]
fn cli_resize_rebase_branch_encrypted() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    std::fs::create_dir(p.join("a")).unwrap();
    std::fs::write(p.join("a/base.img"), vec![3u8; 1 << 20]).unwrap();
    let ov = p.join("a/o.ovl");
    let key = p.join("key");
    std::fs::write(&key, "cd".repeat(32)).unwrap(); // 64 hex digits
    let k = fluxvm_blockd::XtsCipher::from_file(&key).unwrap();
    let cipher: Arc<dyn fluxvm_blockd::seams::BlockCipher> = Arc::new(k);
    {
        let o = CowBackend::create_opts(
            &ov,
            &p.join("a/base.img"),
            &fluxvm_blockd::CreateOptions {
                size: None,
                cipher: Some(cipher.clone()),
            },
        )
        .unwrap();
        o.write_at(5, b"cli").unwrap();
        o.flush().unwrap();
    }
    let out = bin()
        .args(["resize", "--overlay", ov.to_str().unwrap(), "--size", "2T"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("rewritten"));
    let out = bin()
        .args(["resize", "--overlay", ov.to_str().unwrap(), "--size", "1M"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "shrink must fail");

    // Move the base next to a new location and rebase.
    std::fs::create_dir(p.join("b")).unwrap();
    std::fs::rename(p.join("a/base.img"), p.join("b/base.img")).unwrap();
    let out = bin()
        .args(["rebase", "--overlay", ov.to_str().unwrap(), "--parent"])
        .arg(p.join("b/base.img"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Branch needs the key for an encrypted parent and passes it on.
    let child = p.join("a/c.ovl");
    let out = bin()
        .args([
            "branch",
            "--from",
            ov.to_str().unwrap(),
            "--to",
            child.to_str().unwrap(),
        ])
        .env_remove("FLUXVM_BLOCKD_KEY")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!child.exists());
    let out = bin()
        .args([
            "branch",
            "--from",
            ov.to_str().unwrap(),
            "--to",
            child.to_str().unwrap(),
        ])
        .args(["--key-file", key.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let c = CowBackend::open_with(
        &child,
        &fluxvm_blockd::OpenOptions {
            key: Some(cipher),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(c.is_encrypted());
    assert_eq!(c.size(), 2u64 << 40);
    let mut b = [0u8; 5];
    c.read_at(4, &mut b).unwrap();
    assert_eq!(&b, &[3, b'c', b'l', b'i', 3]);
}
