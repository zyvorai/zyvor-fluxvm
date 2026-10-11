// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Throughput and loss accounting of the bridge over a real unix socket. Ignored by default:
//!
//! ```text
//! cargo test -p fluxvm-netobs --release --test bridge_throughput -- --ignored --nocapture
//! ```
//!
//! One producer pushes `FLUXVM_NETOBS_BENCH_FRAMES` (default 1,000,000) small frames into a [`BridgeClient`] as
//! fast as it can (far above any guest's rate), a [`BridgeServer`] ingests them into an [`Observer`], and the test
//! reports offered / ingested / dropped. The invariant asserted is `offered == ingested + counted drops`: the
//! bridge may lose observations under overload, but never silently. The observer is set up like the daemon's: it
//! already has an in-process tap registered, so the bridge's own accounting must not depend on being first.
//! `FLUXVM_NETOBS_BENCH_PPS` paces the producer (frames per second, 0 = unpaced).

use fluxvm_netobs::bridge::{BridgeClient, BridgeClientConfig, BridgeServer};
use fluxvm_netobs::packet::build::{self, tcp_v4};
use fluxvm_netobs::packet::{TCP_ACK, TCP_PSH};
use fluxvm_netobs::{Direction, Identity, NetTap, Observer, ObserverConfig, Verdict, now_us};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn env(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

struct Outcome {
    offered: u64,
    ingested: u64,
    client_dropped: u64,
    /// What an operator reading the summary sees as dropped by the bridge.
    reported_dropped: u64,
    secs: f64,
}

fn reported(obs: &Observer) -> u64 {
    obs.summary(now_us()).counters.bridge_dropped
}

fn run(offered: u64, pps: u64, cfg: BridgeClientConfig, payload: usize) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("bridge.sock");
    let obs = Arc::new(Observer::new(ObserverConfig::default()));
    // The daemon registers its in-process tap first; the bridge must still be accounted for.
    let (_local, local_rx) = NetTap::new(16, 2048);
    obs.set_tap_stats(local_rx.stats());
    let server = BridgeServer::bind(&sock).unwrap().spawn(obs.clone());
    let client = BridgeClient::connect(&sock, cfg);
    let id = Identity::new("vm-a", "bench", "private", build::GUEST_MAC);
    let g = Ipv4Addr::new(10, 0, 0, 2);
    let s = Ipv4Addr::new(10, 0, 0, 3);
    let body = vec![0x5au8; payload];
    let frame = tcp_v4((g, 40000), (s, 5201), 1, 1, TCP_ACK | TCP_PSH, 65535, &body);
    let start = Instant::now();
    for i in 0..offered {
        client.tap_as(&id, Direction::FromGuest, Verdict::Allowed, &frame);
        if pps > 0 && i % 64 == 63 {
            let due = Duration::from_secs_f64((i + 1) as f64 / pps as f64);
            while start.elapsed() < due {
                std::hint::spin_loop();
            }
        }
    }
    // Wait until every offered frame is either ingested or reported dropped.
    let drops = || client.stats().total_dropped();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if obs.counters().frames + drops() >= offered {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let secs = start.elapsed().as_secs_f64();
    // Let a final STATS record arrive (the client sends one per second at most).
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && reported(&obs) < drops() {
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = Outcome {
        offered,
        ingested: obs.counters().frames,
        client_dropped: drops(),
        reported_dropped: reported(&obs),
        secs,
    };
    server.shutdown().unwrap();
    out
}

fn report(label: &str, o: &Outcome) {
    println!(
        "{label}: offered {} ingested {} ({:.1}%) client-dropped {} reported-dropped {} in {:.2}s ({:.0} offered/s)",
        o.offered,
        o.ingested,
        100.0 * o.ingested as f64 / o.offered as f64,
        o.client_dropped,
        o.reported_dropped,
        o.secs,
        o.offered as f64 / o.secs
    );
}

#[test]
#[ignore = "benchmark; run with --ignored in release mode"]
fn bridge_accounts_for_every_offered_frame() {
    let frames = env("FLUXVM_NETOBS_BENCH_FRAMES", 1_000_000);
    let pps = env("FLUXVM_NETOBS_BENCH_PPS", 0);
    let o = run(frames, pps, BridgeClientConfig::default(), 100);
    report("small frames, unpaced", &o);
    assert_eq!(
        o.offered,
        o.ingested + o.client_dropped,
        "every frame is either ingested or counted as dropped"
    );
    assert_eq!(
        o.reported_dropped, o.client_dropped,
        "and the observer summary reports exactly those drops"
    );
}

#[test]
#[ignore = "benchmark; run with --ignored in release mode"]
fn bridge_mtu_frames_at_a_guest_like_rate() {
    let frames = env("FLUXVM_NETOBS_BENCH_FRAMES", 1_000_000);
    let pps = env("FLUXVM_NETOBS_BENCH_PPS", 300_000);
    let o = run(frames, pps, BridgeClientConfig::default(), 1400);
    report("MTU frames, paced", &o);
    assert_eq!(o.offered, o.ingested + o.client_dropped);
    assert_eq!(o.reported_dropped, o.client_dropped);
}
