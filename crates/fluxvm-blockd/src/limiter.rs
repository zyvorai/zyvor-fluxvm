// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Token-bucket bandwidth and IOPS limiter. Requests incur "debt" and are
//! delayed until it is repaid, so a single large request is never rejected.

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct State {
    bytes: f64,
    ops: f64,
    last: Instant,
}

pub struct Limiter {
    bps: Option<f64>,
    iops: Option<f64>,
    burst_bytes: f64,
    burst_ops: f64,
    state: Mutex<State>,
}

impl Limiter {
    /// `None` disables that dimension. Burst capacity is 1/4 second of rate.
    pub fn new(bytes_per_sec: Option<u64>, iops: Option<u64>) -> Self {
        let bps = bytes_per_sec.map(|v| v.max(1) as f64);
        let io = iops.map(|v| v.max(1) as f64);
        let burst_bytes = bps.map_or(0.0, |v| (v / 4.0).max(1.0));
        let burst_ops = io.map_or(0.0, |v| (v / 4.0).max(1.0));
        Self {
            bps,
            iops: io,
            burst_bytes,
            burst_ops,
            state: Mutex::new(State {
                bytes: burst_bytes,
                ops: burst_ops,
                last: Instant::now(),
            }),
        }
    }

    /// The bucket's reference instant, for deterministic tests.
    pub fn epoch(&self) -> Instant {
        self.state.lock().unwrap().last
    }

    /// Account one operation of `bytes` at time `now`; returns how long the
    /// caller must wait before proceeding.
    pub fn reserve(&self, now: Instant, bytes: u64) -> Duration {
        let mut s = self.state.lock().unwrap();
        let dt = now.saturating_duration_since(s.last).as_secs_f64();
        if now > s.last {
            s.last = now;
        }
        let mut wait: f64 = 0.0;
        if let Some(r) = self.bps {
            s.bytes = (s.bytes + dt * r).min(self.burst_bytes) - bytes as f64;
            if s.bytes < 0.0 {
                wait = wait.max(-s.bytes / r);
            }
        }
        if let Some(r) = self.iops {
            s.ops = (s.ops + dt * r).min(self.burst_ops) - 1.0;
            if s.ops < 0.0 {
                wait = wait.max(-s.ops / r);
            }
        }
        Duration::from_secs_f64(wait)
    }

    pub async fn acquire(&self, bytes: u64) {
        let w = self.reserve(Instant::now(), bytes);
        if !w.is_zero() {
            tokio::time::sleep(w).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iops_burst_then_throttle() {
        let l = Limiter::new(None, Some(100)); // burst 25
        let t0 = l.epoch();
        for _ in 0..25 {
            assert!(l.reserve(t0, 0).is_zero());
        }
        let w = l.reserve(t0, 0);
        assert!(
            w >= Duration::from_millis(9) && w <= Duration::from_millis(11),
            "{w:?}"
        );
        // 1 second later the bucket is refilled (capped at burst).
        assert!(l.reserve(t0 + Duration::from_secs(1), 0).is_zero());
    }

    #[test]
    fn bandwidth_debt() {
        let l = Limiter::new(Some(1_000_000), None); // burst 250k
        let t0 = l.epoch();
        assert!(l.reserve(t0, 250_000).is_zero());
        let w = l.reserve(t0, 500_000); // 500k debt at 1MB/s
        assert!(
            w >= Duration::from_millis(490) && w <= Duration::from_millis(510),
            "{w:?}"
        );
    }

    #[tokio::test]
    async fn acquire_really_sleeps() {
        let l = Limiter::new(None, Some(20)); // burst 5
        let start = Instant::now();
        for _ in 0..15 {
            l.acquire(0).await;
        }
        // 10 ops past the burst at 20/s => ~500ms
        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "{:?}",
            start.elapsed()
        );
    }
}
