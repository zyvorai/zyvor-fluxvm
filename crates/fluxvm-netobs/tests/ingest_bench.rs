// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Micro benchmark for `Observer::ingest` (criterion-free). Ignored by default:
//!
//! ```text
//! cargo test -p fluxvm-netobs --release --test ingest_bench -- --ignored --nocapture
//! ```
//!
//! It reports frames per second for one producer thread and for several producer threads sharing one observer,
//! over a mix of established-TCP data, DNS and UDP flows. The numbers are for comparison between revisions on the
//! same machine, not an SLA. `FLUXVM_NETOBS_BENCH_FRAMES` and `FLUXVM_NETOBS_BENCH_THREADS` override the sizes.

use fluxvm_netobs::packet::build::{self, tcp_v4, udp_v4};
use fluxvm_netobs::packet::{TCP_ACK, TCP_PSH, TCP_SYN};
use fluxvm_netobs::{Direction, FrameMeta, Identity, Observer, ObserverConfig, Verdict};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Instant;

fn corpus(vm: usize) -> Vec<Vec<u8>> {
    let g = Ipv4Addr::new(10, 0, (vm >> 8) as u8, vm as u8);
    let s = Ipv4Addr::new(93, 184, 216, 34);
    let ns = Ipv4Addr::new(10, 0, 0, 1);
    let mut v = Vec::new();
    for p in 0..256u16 {
        let c = (g, 20_000 + p);
        v.push(tcp_v4(c, (s, 443), 1, 0, TCP_SYN, 65535, b""));
        for n in 0..6u32 {
            v.push(tcp_v4(
                c,
                (s, 443),
                2 + n * 100,
                1,
                TCP_ACK | TCP_PSH,
                65535,
                &[0x55u8; 100],
            ));
            v.push(tcp_v4(
                (s, 443),
                c,
                1,
                2 + n * 100 + 100,
                TCP_ACK,
                65535,
                b"",
            ));
        }
    }
    for p in 0..64u16 {
        v.push(udp_v4(
            (g, 30_000 + p),
            (ns, 53),
            &fluxvm_netobs::l7::fixtures::dns_query(p, "bench.example.com", 1),
        ));
        v.push(udp_v4((g, 40_000 + p), (s, 9999), &[1u8; 200]));
    }
    v
}

fn run(threads: usize, frames_per_thread: usize, batch: usize) -> f64 {
    let obs = Arc::new(Observer::new(ObserverConfig::default()));
    let corpora: Vec<Vec<Vec<u8>>> = (0..threads).map(corpus).collect();
    let start = Instant::now();
    std::thread::scope(|sc| {
        for (t, frames) in corpora.iter().enumerate() {
            let obs = obs.clone();
            sc.spawn(move || {
                let id = Identity::new(&format!("vm-{t}"), "bench", "eth0", build::GUEST_MAC);
                let mut meta = FrameMeta {
                    identity: id,
                    ts_us: 1_000_000,
                    direction: Direction::FromGuest,
                    verdict: Verdict::Allowed,
                };
                if batch <= 1 {
                    for i in 0..frames_per_thread {
                        meta.ts_us += 5;
                        obs.ingest(&frames[i % frames.len()], &meta);
                    }
                } else {
                    // One lock acquisition per `batch` frames (what the bridge server does).
                    let mut i = 0;
                    while i < frames_per_thread {
                        let n = batch.min(frames_per_thread - i);
                        let metas: Vec<FrameMeta> = (0..n)
                            .map(|k| FrameMeta {
                                ts_us: meta.ts_us + 5 * (k as u64 + 1),
                                ..meta.clone()
                            })
                            .collect();
                        meta.ts_us += 5 * n as u64;
                        obs.ingest_batch(
                            (0..n).map(|k| (&frames[(i + k) % frames.len()][..], &metas[k])),
                        );
                        i += n;
                    }
                }
            });
        }
    });
    let secs = start.elapsed().as_secs_f64();
    assert_eq!(obs.counters().frames, (threads * frames_per_thread) as u64);
    (threads * frames_per_thread) as f64 / secs
}

#[test]
#[ignore = "benchmark; run with --ignored in release mode"]
fn ingest_frames_per_second() {
    let env = |k: &str, d: usize| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let frames = env("FLUXVM_NETOBS_BENCH_FRAMES", 1_000_000);
    let threads = env("FLUXVM_NETOBS_BENCH_THREADS", 4);
    let single = run(1, frames, 1);
    println!("netobs ingest, 1 producer, per-frame lock : {single:>12.0} frames/s");
    let multi = run(threads, frames, 1);
    println!(
        "netobs ingest, {threads} producers, per-frame lock: {multi:>12.0} frames/s ({:.2}x)",
        multi / single
    );
    let batched = run(threads, frames, 64);
    println!(
        "netobs ingest, {threads} producers, batches of 64: {batched:>12.0} frames/s ({:.2}x)",
        batched / single
    );
}
