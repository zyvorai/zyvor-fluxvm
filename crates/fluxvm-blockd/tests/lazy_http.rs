// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Lazy HTTP base: caching, bounded parallelism, failure surfacing as NBD EIO
//! and whole-image sha256 verification, against an in-process range server.

use fluxvm_blockd::cow::overlay_info;
use fluxvm_blockd::nbd::*;
use fluxvm_blockd::seams::LazySource;
use fluxvm_blockd::{
    BlockBackend, CowBackend, CreateOptions, Export, LazyConfig, LazyHttp, OpenOptions, Server,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const MODE_OK: u8 = 0;
const MODE_TRUNCATE: u8 = 1; // advertise full length, send half, close
const MODE_NO_RANGE: u8 = 2; // ignore Range, answer 200

struct HttpSrv {
    addr: SocketAddr,
    mode: Arc<AtomicU8>,
    requests: Arc<AtomicUsize>,
    max_concurrent: Arc<AtomicUsize>,
}

fn http_server(data: Arc<Vec<u8>>, delay: Duration) -> HttpSrv {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let mode = Arc::new(AtomicU8::new(MODE_OK));
    let requests = Arc::new(AtomicUsize::new(0));
    let cur = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    let (m, r, c, x) = (mode.clone(), requests.clone(), cur, max.clone());
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(mut s) = conn else { continue };
            let (data, m, r, c, x) = (data.clone(), m.clone(), r.clone(), c.clone(), x.clone());
            std::thread::spawn(move || {
                let mut req = Vec::new();
                let mut b = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => req.extend_from_slice(&b[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).to_lowercase();
                r.fetch_add(1, Ordering::SeqCst);
                let now = c.fetch_add(1, Ordering::SeqCst) + 1;
                x.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(delay);
                let total = data.len() as u64;
                let range = text
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|v| {
                        let (a, b) = v.trim().split_once('-')?;
                        Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
                    });
                let mode = m.load(Ordering::SeqCst);
                match (range, mode) {
                    (_, MODE_NO_RANGE) | (None, _) => {
                        let h = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                        );
                        let _ = s.write_all(h.as_bytes());
                        let _ = s.write_all(&data);
                    }
                    (Some((a, e)), _) => {
                        let e = e.min(total - 1);
                        let body = &data[a as usize..=e as usize];
                        let h = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{e}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = s.write_all(h.as_bytes());
                        if mode == MODE_TRUNCATE && a != 0 {
                            let _ = s.write_all(&body[..body.len() / 2]);
                        } else {
                            let _ = s.write_all(body);
                        }
                    }
                }
                let _ = s.flush();
                c.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    HttpSrv {
        addr,
        mode,
        requests,
        max_concurrent: max,
    }
}

fn image(len: usize) -> Arc<Vec<u8>> {
    Arc::new(
        (0..len as u64)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect(),
    )
}

fn sha(d: &[u8]) -> [u8; 32] {
    Sha256::digest(d).into()
}

fn url(s: &HttpSrv) -> String {
    format!("http://{}/img", s.addr)
}

fn cfg(parallel: usize) -> LazyConfig {
    LazyConfig {
        parallel,
        timeout: Duration::from_secs(5),
        retries: 0,
        ..Default::default()
    }
}

#[test]
fn read_through_cache_persists_and_is_not_refetched() {
    let data = image(5 * (1 << 20) + 12_345);
    let srv = http_server(data.clone(), Duration::ZERO);
    let d = tempfile::tempdir().unwrap();
    let cache = d.path().join("c");
    let l = LazyHttp::open(&url(&srv), &cache, cfg(2)).unwrap();
    assert_eq!(l.size(), data.len() as u64);
    assert_eq!(l.total_chunks(), 6);
    let mut b = vec![0u8; 3000];
    l.read_at((1 << 20) - 1500, &mut b).unwrap(); // straddles chunks 0/1
    assert_eq!(b, data[(1 << 20) - 1500..(1 << 20) + 1500]);
    assert_eq!(l.cached_chunks(), 2);
    let n = srv.requests.load(Ordering::SeqCst);
    l.read_at((1 << 20) - 1500, &mut b).unwrap();
    assert_eq!(srv.requests.load(Ordering::SeqCst), n, "served from cache");
    // Tail chunk (short) is exact.
    let mut t = vec![0u8; 12_345];
    l.read_at(5 << 20, &mut t).unwrap();
    assert_eq!(t, data[5 << 20..]);
    assert!(l.write_at(0, b"x").is_err());
    drop(l);
    // Restart with the server unreachable: cached chunks still work.
    let l2 = LazyHttp::open(&url(&srv), &cache, cfg(2)).unwrap();
    assert_eq!(l2.cached_chunks(), 3);
    let mut b2 = vec![0u8; 3000];
    let n = srv.requests.load(Ordering::SeqCst);
    l2.read_at((1 << 20) - 1500, &mut b2).unwrap();
    assert_eq!(b2, b);
    assert_eq!(srv.requests.load(Ordering::SeqCst), n);
}

#[test]
fn prefetch_parallelism_is_bounded_and_completes() {
    let data = image(8 << 20);
    let srv = http_server(data.clone(), Duration::from_millis(80));
    let d = tempfile::tempdir().unwrap();
    let l = LazyHttp::open(&url(&srv), &d.path().join("c"), cfg(2)).unwrap();
    let probe_reqs = srv.requests.load(Ordering::SeqCst);
    l.prefetch(0, data.len() as u64);
    let t = Instant::now();
    while !l.is_complete() {
        assert!(t.elapsed() < Duration::from_secs(10), "prefetch stalled");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(srv.requests.load(Ordering::SeqCst) - probe_reqs, 8);
    let m = srv.max_concurrent.load(Ordering::SeqCst);
    assert!(
        m == 2,
        "max concurrent fetches {m}, wanted exactly the configured 2"
    );
    let mut all = vec![0u8; data.len()];
    l.read_at(0, &mut all).unwrap();
    assert_eq!(all, *data);
}

#[test]
fn sha256_verified_once_fully_cached_and_mismatch_is_sticky() {
    let data = image(3 << 20);
    let srv = http_server(data.clone(), Duration::ZERO);
    let d = tempfile::tempdir().unwrap();
    let good = LazyConfig {
        sha256: Some(sha(&data)),
        ..cfg(3)
    };
    let l = LazyHttp::open(&url(&srv), &d.path().join("good"), good).unwrap();
    assert_eq!(l.verified(), None);
    let mut b = vec![0u8; 100];
    l.read_at(0, &mut b).unwrap();
    assert_eq!(l.verified(), None, "not complete yet");
    let mut all = vec![0u8; data.len()];
    l.read_at(0, &mut all).unwrap();
    assert!(l.is_complete());
    assert_eq!(l.verified(), Some(true));

    let bad = LazyConfig {
        sha256: Some([0xAB; 32]),
        ..cfg(3)
    };
    let l = LazyHttp::open(&url(&srv), &d.path().join("bad"), bad).unwrap();
    let e = l.read_at(0, &mut all).unwrap_err();
    assert!(e.to_string().contains("sha256"), "{e}");
    assert_eq!(l.verified(), Some(false));
    // Sticky: even cached ranges are refused afterwards.
    assert!(l.read_at(0, &mut b).is_err());
}

#[test]
fn server_without_range_support_is_rejected() {
    let data = image(1 << 20);
    let srv = http_server(data, Duration::ZERO);
    srv.mode.store(MODE_NO_RANGE, Ordering::SeqCst);
    let d = tempfile::tempdir().unwrap();
    let e = LazyHttp::open(&url(&srv), &d.path().join("c"), cfg(1))
        .err()
        .unwrap();
    assert!(e.to_string().contains("range"), "{e}");
}

#[test]
fn overlay_over_lazy_source_header_and_reopen() {
    let data = image(3 << 20);
    let srv = http_server(data.clone(), Duration::ZERO);
    let d = tempfile::tempdir().unwrap();
    let ov = d.path().join("o.ovl");
    let lz = LazyConfig {
        sha256: Some(sha(&data)),
        ..cfg(2)
    };
    let o = CowBackend::create_on_lazy(&ov, &url(&srv), &CreateOptions::default(), &lz).unwrap();
    assert_eq!(o.size(), data.len() as u64);
    o.write_at(70_000, b"patched").unwrap();
    let mut b = vec![0u8; 20];
    o.read_at(69_995, &mut b).unwrap();
    assert_eq!(&b[..5], &data[69_995..70_000]);
    assert_eq!(&b[5..12], b"patched");
    o.prefetch(0, data.len() as u64);
    o.flush().unwrap();
    drop(o);
    assert_eq!(
        overlay_info(&ov).unwrap(),
        (false, 2),
        "parent kind is lazy http"
    );
    // Reopen: sha256 comes from the header, URL too.
    let o = CowBackend::open_with(
        &ov,
        &OpenOptions {
            lazy: cfg(2),
            ..Default::default()
        },
    )
    .unwrap();
    let mut b2 = vec![0u8; 20];
    o.read_at(69_995, &mut b2).unwrap();
    assert_eq!(b, b2);
    // Overlay writes never reach the remote or the cache.
    let mut tail = vec![0u8; 100];
    o.read_at(data.len() as u64 - 100, &mut tail).unwrap();
    assert_eq!(tail, data[data.len() - 100..]);
}

#[tokio::test]
async fn mid_stream_failure_surfaces_as_eio_not_hang() {
    let data = image(4 << 20);
    let srv = http_server(data.clone(), Duration::ZERO);
    let d = tempfile::tempdir().unwrap();
    let ov = d.path().join("o.ovl");
    let u = url(&srv);
    let ovc = ov.clone();
    let o = tokio::task::spawn_blocking(move || {
        let lz = LazyConfig {
            timeout: Duration::from_secs(3),
            retries: 1,
            parallel: 2,
            ..Default::default()
        };
        CowBackend::create_on_lazy(&ovc, &u, &CreateOptions::default(), &lz).unwrap()
    })
    .await
    .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(Server::new(vec![Export::new("z", o.clone())]).serve(l));

    let mut s = TcpStream::connect(addr).await.unwrap();
    s.read_u64().await.unwrap();
    s.read_u64().await.unwrap();
    s.read_u16().await.unwrap();
    s.write_u32(3).await.unwrap();
    let name = b"z";
    s.write_u64(0x49484156454f5054).await.unwrap();
    s.write_u32(OPT_GO).await.unwrap();
    s.write_u32(4 + 1 + 2).await.unwrap();
    s.write_u32(1).await.unwrap();
    s.write_all(name).await.unwrap();
    s.write_u16(0).await.unwrap();
    loop {
        s.read_u64().await.unwrap();
        s.read_u32().await.unwrap();
        let rep = s.read_u32().await.unwrap();
        let n = s.read_u32().await.unwrap();
        let mut p = vec![0u8; n as usize];
        s.read_exact(&mut p).await.unwrap();
        if rep == REP_ACK {
            break;
        }
    }
    async fn read(s: &mut TcpStream, h: u64, off: u64, len: u32) -> Result<Vec<u8>, u32> {
        s.write_u32(0x25609513).await.unwrap();
        s.write_u16(0).await.unwrap();
        s.write_u16(CMD_READ).await.unwrap();
        s.write_u64(h).await.unwrap();
        s.write_u64(off).await.unwrap();
        s.write_u32(len).await.unwrap();
        assert_eq!(s.read_u32().await.unwrap(), 0x67446698);
        let err = s.read_u32().await.unwrap();
        assert_eq!(s.read_u64().await.unwrap(), h);
        if err != 0 {
            return Err(err);
        }
        let mut v = vec![0u8; len as usize];
        s.read_exact(&mut v).await.unwrap();
        Ok(v)
    }
    // Chunk 0 was cached by the probe path? No: only size was learned. Fail
    // the stream for any non-first range.
    srv.mode.store(MODE_TRUNCATE, Ordering::SeqCst);
    let t = Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(15), read(&mut s, 1, 2 << 20, 4096))
        .await
        .expect("read must not hang");
    assert_eq!(r.unwrap_err(), EIO);
    assert!(t.elapsed() < Duration::from_secs(10));
    // The failure is not sticky: with the server healthy again it works.
    srv.mode.store(MODE_OK, Ordering::SeqCst);
    let got = read(&mut s, 2, 2 << 20, 4096).await.unwrap();
    assert_eq!(got, data[2 << 20..(2 << 20) + 4096]);
}
