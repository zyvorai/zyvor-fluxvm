// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Enforced `HEALTHCHECK` for `fluxctl mac` instances. The Macfile records a healthcheck in the image config; this module runs it:
//! over SSH, inside the guest, from the Mac that drives the instance (`mac health`, `mac run --wait-healthy`, compose
//! `depends_on: condition: service_healthy`, `mac supervise`).
//!
//! The state lives in VM labels, so it survives this process and shows in `mac inspect`:
//! `fluxvm.mac.health` (`starting`, `healthy`, `unhealthy`), `fluxvm.mac.health.streak` (consecutive failures) and
//! `fluxvm.mac.health.checked` (unix seconds of the last probe). The state machine follows Docker: an instance is `starting` until
//! the first success, `healthy` after one, and `unhealthy` once `retries` consecutive probes fail.
//!
//! The state machine and the wait loop are pure or generic over fakes and unit tested. The SSH probe has never run against a real VM.

use crate::mac_config::Healthcheck;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const HEALTH_LABEL: &str = "fluxvm.mac.health";
pub const HEALTH_STREAK_LABEL: &str = "fluxvm.mac.health.streak";
pub const HEALTH_AT_LABEL: &str = "fluxvm.mac.health.checked";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Starting,
    Healthy,
    Unhealthy,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Healthy => "healthy",
            Status::Unhealthy => "unhealthy",
        }
    }
    fn parse(s: &str) -> Option<Status> {
        match s {
            "starting" => Some(Status::Starting),
            "healthy" => Some(Status::Healthy),
            "unhealthy" => Some(Status::Unhealthy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub status: Status,
    /// Consecutive failed probes.
    pub streak: u32,
    pub checked_unix: u64,
}

/// The recorded state of a VM record, if a probe ever ran.
pub fn from_labels(vm: &Value) -> Option<Health> {
    let l = &vm["labels"];
    Some(Health {
        status: Status::parse(l[HEALTH_LABEL].as_str()?)?,
        streak: l[HEALTH_STREAK_LABEL]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        checked_unix: l[HEALTH_AT_LABEL]
            .as_str()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    })
}

/// The state after one probe. A failure keeps `starting`/`healthy` until `retries` in a row have failed.
pub fn next(prev: Option<&Health>, hc: &Healthcheck, ok: bool, now: u64) -> Health {
    if ok {
        return Health {
            status: Status::Healthy,
            streak: 0,
            checked_unix: now,
        };
    }
    let (status, streak) = match prev {
        Some(p) => (p.status, p.streak + 1),
        None => (Status::Starting, 1),
    };
    let status = if streak >= hc.retries.max(1) {
        Status::Unhealthy
    } else {
        status
    };
    Health {
        status,
        streak,
        checked_unix: now,
    }
}

/// Is a probe due? Always when none ran yet; else after the healthcheck's interval.
pub fn due(prev: Option<&Health>, hc: &Healthcheck, now: u64) -> bool {
    prev.is_none_or(|p| now >= p.checked_unix + hc.interval_secs)
}

/// The PATCH body that records `h`.
pub fn labels_patch(h: &Health) -> Value {
    json!({"labels": {
        HEALTH_LABEL: h.status.as_str(),
        HEALTH_STREAK_LABEL: h.streak.to_string(),
        HEALTH_AT_LABEL: h.checked_unix.to_string(),
    }})
}

/// What `mac inspect` shows under `state.health`; `null` when the image has no healthcheck and nothing ran.
pub fn inspect_value(vm: &Value) -> Value {
    match from_labels(vm) {
        Some(h) => {
            json!({"status": h.status.as_str(), "failing_streak": h.streak, "last_check_unix": h.checked_unix})
        }
        None => Value::Null,
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Runs the healthcheck command in the guest. The real one is `ssh`; tests use a fake.
pub(crate) trait Prober {
    /// True when the command exited 0 within `timeout`.
    async fn probe(&mut self, ip: &str, user: &str, cmd: &str, timeout: Duration) -> bool;
}

pub(crate) struct SshProber;

impl Prober for SshProber {
    async fn probe(&mut self, ip: &str, user: &str, cmd: &str, timeout: Duration) -> bool {
        let mut c = tokio::process::Command::new("ssh");
        c.args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
        ]);
        if let Some(k) = crate::run::generated_identity() {
            c.arg("-i").arg(k);
        }
        c.arg(format!("{user}@{ip}"))
            .arg(cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // kill_on_drop: a probe that outlives its timeout is killed when the future is dropped.
        match tokio::time::timeout(timeout, c.status()).await {
            Ok(Ok(s)) => s.success(),
            _ => false,
        }
    }
}

/// Probe a VM once if it is running with an address. `None` when it cannot be probed (not running, no address).
pub(crate) async fn probe_vm<P: Prober>(
    p: &mut P,
    vm: &Value,
    hc: &Healthcheck,
    user: &str,
    now: u64,
) -> Option<Health> {
    if vm["status"].as_str() != Some("running") {
        return None;
    }
    let ip = vm["guest_ip"].as_str().filter(|s| !s.is_empty())?;
    let ok = p
        .probe(
            ip,
            user,
            &hc.cmd.to_shell(),
            Duration::from_secs(hc.timeout_secs.max(1)),
        )
        .await;
    Some(next(from_labels(vm).as_ref(), hc, ok, now))
}

/// The healthcheck of the image an instance was made from, when the image is still in the store.
pub fn healthcheck_of(vm: &Value) -> Option<Healthcheck> {
    let image = vm["labels"][crate::mac_inspect::IMAGE_LABEL].as_str()?;
    let store = crate::mac_store::Store::open(crate::mac_store::default_root().ok()?).ok()?;
    crate::mac_config::load(&store.resolve(image).ok()?)
        .ok()?
        .healthcheck
}

/// The SSH user for probes: the image's `USER`, else the build default.
pub fn user_of(vm: &Value) -> String {
    let image = vm["labels"][crate::mac_inspect::IMAGE_LABEL].as_str();
    image
        .and_then(|i| {
            let store =
                crate::mac_store::Store::open(crate::mac_store::default_root().ok()?).ok()?;
            crate::mac_config::load(&store.resolve(i).ok()?).ok()?.user
        })
        .unwrap_or_else(|| crate::mac_build_exec::DEFAULT_USER.to_string())
}

/// One round of the wait loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Healthy,
    Unhealthy,
    /// Not yet; the text says what is missing.
    Pending(String),
    /// The instance will not become healthy (stopped, failed, deleted).
    Gone(String),
}

pub(crate) trait Source {
    /// Probe if due, record, and report the state.
    async fn poll(&mut self) -> Result<Poll>;
}

/// Block until healthy. Fails on `unhealthy` (the retries are used up), on a stopped instance, and on timeout.
pub(crate) async fn wait_healthy<S: Source>(
    s: &mut S,
    name: &str,
    timeout: Option<Duration>,
    poll: Duration,
) -> Result<Duration> {
    let start = Instant::now();
    loop {
        let waited = start.elapsed();
        match s.poll().await? {
            Poll::Healthy => return Ok(waited),
            Poll::Unhealthy => bail!(
                "{name} is unhealthy: its HEALTHCHECK failed in a row. Look with `fluxctl mac exec {name} -- ...` or `fluxctl mac logs {name}`"
            ),
            Poll::Gone(why) => bail!("{name} will not become healthy: {why}"),
            Poll::Pending(why) => {
                if let Some(t) = timeout
                    && waited >= t
                {
                    bail!(
                        "{name} was not healthy after {}s: {why} (raise --health-timeout)",
                        t.as_secs()
                    );
                }
            }
        }
        tokio::time::sleep(poll).await;
    }
}

/// The real source: the daemon for the record and labels, [`SshProber`] for the probe.
pub(crate) struct DaemonSource<'a> {
    pub r: &'a crate::remote::Remote,
    pub id: String,
    pub hc: Healthcheck,
    pub user: String,
}

impl Source for DaemonSource<'_> {
    async fn poll(&mut self) -> Result<Poll> {
        let vm = self
            .r
            .call(reqwest::Method::GET, &format!("/v1/vms/{}", self.id), None)
            .await
            .map_err(|e| crate::mac_instance::explain(e, "read the instance"))?;
        let st = crate::mac_inspect::status(&vm).to_string();
        if matches!(st.as_str(), "stopped" | "failed") {
            return Ok(Poll::Gone(format!("it is {st}")));
        }
        let now = now_unix();
        let prev = from_labels(&vm);
        if !due(prev.as_ref(), &self.hc, now) {
            return Ok(Poll::Pending("waiting for the next probe".into()));
        }
        let Some(h) = probe_vm(&mut SshProber, &vm, &self.hc, &self.user, now).await else {
            return Ok(Poll::Pending(format!("the VM is {st} with no address yet")));
        };
        self.r
            .call(
                reqwest::Method::PATCH,
                &format!("/v1/vms/{}", self.id),
                Some(labels_patch(&h)),
            )
            .await?;
        Ok(match h.status {
            Status::Healthy => Poll::Healthy,
            Status::Unhealthy => Poll::Unhealthy,
            Status::Starting => Poll::Pending(format!("{} probe(s) failed so far", h.streak)),
        })
    }
}

/// Wait for the named instance to be healthy. The image must have a `HEALTHCHECK`.
pub async fn wait_instance_healthy(
    r: &crate::remote::Remote,
    name: &str,
    timeout_secs: u64,
) -> Result<Duration> {
    let vm = crate::mac_cmd::find_instance(r, name).await?;
    let Some(hc) = healthcheck_of(&vm) else {
        bail!(
            "{name}: its image has no HEALTHCHECK (or is no longer in the store), so there is nothing to wait for. Add `HEALTHCHECK CMD ...` to the Macfile"
        );
    };
    let mut src = DaemonSource {
        r,
        id: vm["id"].as_str().unwrap_or_default().to_string(),
        user: user_of(&vm),
        // Probe at least every 2 s while waiting: the interval of a long-running check is too slow for a start-up gate.
        hc: Healthcheck {
            interval_secs: hc.interval_secs.min(2),
            ..hc
        },
    };
    wait_healthy(
        &mut src,
        name,
        (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs)),
        Duration::from_secs(2),
    )
    .await
}

/// `mac health NAME [--json]`: probe once now, record it, print it. Exit 1 unless healthy.
pub async fn health_once(r: &crate::remote::Remote, name: &str, json_out: bool) -> Result<()> {
    let vm = crate::mac_cmd::find_instance(r, name).await?;
    let Some(hc) = healthcheck_of(&vm) else {
        bail!("{name}: its image has no HEALTHCHECK (or is no longer in the store)");
    };
    let id = vm["id"].as_str().unwrap_or_default();
    let Some(h) = probe_vm(&mut SshProber, &vm, &hc, &user_of(&vm), now_unix()).await else {
        bail!("{name} is not running with an address: nothing to probe");
    };
    r.call(
        reqwest::Method::PATCH,
        &format!("/v1/vms/{id}"),
        Some(labels_patch(&h)),
    )
    .await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "name": name, "status": h.status.as_str(), "failing_streak": h.streak,
            }))?
        );
    } else {
        println!(
            "{name}: {} (failing streak {})",
            h.status.as_str(),
            h.streak
        );
    }
    if h.status != Status::Healthy {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_config::Cmd;
    use std::collections::VecDeque;

    fn hc(retries: u32) -> Healthcheck {
        Healthcheck {
            cmd: Cmd::Shell("curl -f localhost".into()),
            interval_secs: 30,
            timeout_secs: 5,
            retries,
        }
    }

    #[test]
    fn starting_until_the_first_success_then_unhealthy_after_retries() {
        let h = hc(3);
        let a = next(None, &h, false, 10);
        assert_eq!((a.status, a.streak), (Status::Starting, 1));
        let b = next(Some(&a), &h, false, 20);
        assert_eq!((b.status, b.streak), (Status::Starting, 2));
        let c = next(Some(&b), &h, false, 30);
        assert_eq!((c.status, c.streak), (Status::Unhealthy, 3));
        let d = next(Some(&c), &h, true, 40);
        assert_eq!((d.status, d.streak), (Status::Healthy, 0));
    }

    #[test]
    fn a_healthy_instance_survives_fewer_failures_than_retries() {
        let h = hc(3);
        let ok = next(None, &h, true, 1);
        let f = next(Some(&ok), &h, false, 2);
        assert_eq!((f.status, f.streak), (Status::Healthy, 1));
        let f2 = next(Some(&f), &h, false, 3);
        let f3 = next(Some(&f2), &h, false, 4);
        assert_eq!(f3.status, Status::Unhealthy);
    }

    #[test]
    fn zero_retries_counts_as_one() {
        assert_eq!(next(None, &hc(0), false, 1).status, Status::Unhealthy);
    }

    #[test]
    fn labels_round_trip_and_due_follows_the_interval() {
        let h = next(None, &hc(3), false, 100);
        let patch = labels_patch(&h);
        let vm = json!({"labels": patch["labels"]});
        assert_eq!(from_labels(&vm), Some(h.clone()));
        assert!(!due(Some(&h), &hc(3), 129));
        assert!(due(Some(&h), &hc(3), 130));
        assert!(due(None, &hc(3), 0));
        assert_eq!(inspect_value(&vm)["status"], "starting");
        assert_eq!(inspect_value(&json!({"labels": {}})), Value::Null);
    }

    struct Fake(VecDeque<bool>);
    impl Prober for Fake {
        async fn probe(&mut self, _: &str, _: &str, _: &str, _: Duration) -> bool {
            self.0.pop_front().unwrap_or(false)
        }
    }

    #[tokio::test]
    async fn probe_vm_needs_a_running_vm_with_an_address() {
        let mut f = Fake(VecDeque::from([true]));
        let stopped = json!({"status": "stopped", "guest_ip": "1.2.3.4"});
        assert!(
            probe_vm(&mut f, &stopped, &hc(1), "admin", 1)
                .await
                .is_none()
        );
        let noip = json!({"status": "running"});
        assert!(probe_vm(&mut f, &noip, &hc(1), "admin", 1).await.is_none());
        let up = json!({"status": "running", "guest_ip": "1.2.3.4"});
        let h = probe_vm(&mut f, &up, &hc(1), "admin", 9).await.unwrap();
        assert_eq!((h.status, h.checked_unix), (Status::Healthy, 9));
    }

    struct Script(VecDeque<Poll>);
    impl Source for Script {
        async fn poll(&mut self) -> Result<Poll> {
            Ok(self.0.pop_front().unwrap_or(Poll::Pending("end".into())))
        }
    }

    #[tokio::test]
    async fn wait_returns_when_healthy() {
        let mut s = Script(VecDeque::from([
            Poll::Pending("x".into()),
            Poll::Pending("y".into()),
            Poll::Healthy,
        ]));
        wait_healthy(&mut s, "db", None, Duration::from_millis(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn wait_fails_on_unhealthy_gone_and_timeout() {
        let mut s = Script(VecDeque::from([Poll::Unhealthy]));
        let e = wait_healthy(&mut s, "db", None, Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("db is unhealthy"), "{e}");
        let mut s = Script(VecDeque::from([Poll::Gone("it is stopped".into())]));
        let e = wait_healthy(&mut s, "db", None, Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("it is stopped"), "{e}");
        let mut s = Script(VecDeque::new());
        let e = wait_healthy(&mut s, "db", Some(Duration::ZERO), Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not healthy after 0s"), "{e}");
    }
}
