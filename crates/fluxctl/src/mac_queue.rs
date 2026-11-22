// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac run --queue [--queue-timeout SECS]`: wait for a free macOS slot instead of failing with 409.
//!
//! A Mac runs at most 2 macOS VMs. Without `--queue`, `mac run` fails at once when both are taken; CI wants backpressure.
//! With it, the client polls the daemon: while 2 `fluxctl mac` instances are active it only waits (no clone is made), and
//! otherwise it tries to start; a 409 that names the guest limit (another macOS VM took the slot first, or a VM this client
//! cannot see) sends it back to waiting. Anything else is a real error and ends the run.
//!
//! Limits, stated plainly: the queue lives in each client process, so waiters are served in whatever order their polls land,
//! not first-come-first-served; and it cannot see macOS VMs that `fluxctl mac` did not create except through the 409.
//! Ctrl-C while waiting just ends the process: nothing was created.

use anyhow::{Result, bail};
use std::time::{Duration, Instant};

#[derive(clap::Args, Debug, Default, Clone)]
pub struct QueueOpts {
    /// If both macOS VM slots are taken, wait for one instead of failing with 409. Not first-come-first-served across
    /// several waiting clients.
    #[arg(long)]
    pub queue: bool,
    /// With --queue: give up after this many seconds (0 = wait forever).
    #[arg(
        long = "queue-timeout",
        value_name = "SECS",
        default_value_t = 3600,
        requires = "queue"
    )]
    pub queue_timeout: u64,
}

/// How many macOS slots the client can see in use.
pub(crate) trait Slots {
    async fn active(&mut self) -> Result<usize>;
}

/// The daemon's 409 for "this Mac already runs 2 macOS guests", before or after `explain` rewrote it.
pub fn is_slot_conflict(e: &anyhow::Error) -> bool {
    let t = format!("{e:#}");
    (t.contains(": 409 ") && t.contains("macOS guests")) || t.contains("already runs 2 macOS VMs")
}

/// Run `attempt` once a slot looks free; retry while it fails with a slot conflict.
pub(crate) async fn when_slot_free<S, F, Fut, T>(
    slots: &mut S,
    mut attempt: F,
    timeout: Option<Duration>,
    poll: Duration,
    notice: &mut dyn FnMut(&str),
) -> Result<T>
where
    S: Slots,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let start = Instant::now();
    let mut told = false;
    loop {
        // A listing error is not a reason to refuse: try, and let the daemon's own answer decide.
        let busy = match slots.active().await {
            Ok(n) => n >= crate::mac_doctor::MAX_SLOTS,
            Err(_) => false,
        };
        if !busy {
            match attempt().await {
                Ok(v) => return Ok(v),
                Err(e) if is_slot_conflict(&e) => {}
                Err(e) => return Err(e),
            }
        }
        if let Some(t) = timeout
            && start.elapsed() >= t
        {
            bail!(
                "no macOS slot became free within {}s (this Mac runs at most 2 macOS VMs; see `fluxctl mac ps`)",
                t.as_secs()
            );
        }
        if !told {
            notice("both macOS VM slots are taken; waiting for one (Ctrl-C to give up)");
            told = true;
        }
        tokio::time::sleep(poll).await;
    }
}

/// The real slot counter: active `fluxctl mac` instances on the daemon.
pub(crate) struct DaemonSlots<'a>(pub &'a crate::remote::Remote);

impl Slots for DaemonSlots<'_> {
    async fn active(&mut self) -> Result<usize> {
        let v = self
            .0
            .call(
                reqwest::Method::GET,
                &format!(
                    "/v1/vms?label={}",
                    crate::remote::encode_query(crate::mac_cmd::INSTANCE_SELECTOR)
                ),
                None,
            )
            .await?;
        Ok(crate::mac_cmd::filter_instances(&v["items"])
            .into_iter()
            .filter(|vm| crate::mac_doctor::is_active(vm))
            .count())
    }
}

/// Wrap `attempt` (the whole `mac run`) in the queue when `--queue` was given.
pub async fn run_queued<F, Fut, T>(
    r: &crate::remote::Remote,
    q: &QueueOpts,
    attempt: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = attempt;
    if !q.queue {
        return attempt().await;
    }
    when_slot_free(
        &mut DaemonSlots(r),
        attempt,
        (q.queue_timeout > 0).then(|| Duration::from_secs(q.queue_timeout)),
        Duration::from_secs(5),
        &mut |m| eprintln!("{m}"),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fake(VecDeque<Result<usize>>);
    impl Slots for Fake {
        async fn active(&mut self) -> Result<usize> {
            self.0.pop_front().unwrap_or(Ok(0))
        }
    }

    const MS: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn runs_at_once_when_a_slot_is_free() {
        let mut calls = 0;
        let mut notes = vec![];
        let out = when_slot_free(
            &mut Fake([Ok(1)].into()),
            || {
                calls += 1;
                async { Ok::<_, anyhow::Error>(7) }
            },
            None,
            MS,
            &mut |m| notes.push(m.to_string()),
        )
        .await
        .unwrap();
        assert_eq!((out, calls), (7, 1));
        assert!(notes.is_empty());
    }

    #[tokio::test]
    async fn waits_without_attempting_while_both_slots_are_busy() {
        let attempts = std::cell::Cell::new(0);
        let mut notes = vec![];
        let out = when_slot_free(
            &mut Fake([Ok(2), Ok(2), Ok(2), Ok(1)].into()),
            || {
                attempts.set(attempts.get() + 1);
                async { Ok::<_, anyhow::Error>("up") }
            },
            Some(Duration::from_secs(5)),
            MS,
            &mut |m| notes.push(m.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(out, "up");
        assert_eq!(attempts.get(), 1, "no clone while the slots were full");
        assert_eq!(notes.len(), 1, "told once");
        assert!(notes[0].contains("waiting"));
    }

    #[tokio::test]
    async fn a_slot_conflict_goes_back_to_waiting() {
        let attempts = std::cell::Cell::new(0);
        let out = when_slot_free(
            &mut Fake(VecDeque::new()),
            || {
                attempts.set(attempts.get() + 1);
                let n = attempts.get();
                async move {
                    if n < 3 {
                        Err(anyhow::anyhow!(
                            "POST /v1/vms/x/start: 409 Conflict: this Mac already runs 2 macOS guests"
                        ))
                    } else {
                        Ok(n)
                    }
                }
            },
            Some(Duration::from_secs(5)),
            MS,
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!(out, 3);
    }

    #[tokio::test]
    async fn other_errors_end_the_run() {
        let e = when_slot_free(
            &mut Fake(VecDeque::new()),
            || async { Err::<(), _>(anyhow::anyhow!("cannot create x: 503 locked")) },
            None,
            MS,
            &mut |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("503"));
    }

    #[tokio::test]
    async fn times_out_naming_the_limit() {
        let e = when_slot_free(
            &mut Fake((0..1000).map(|_| Ok(2)).collect()),
            || async { Ok::<(), anyhow::Error>(()) },
            Some(Duration::from_millis(20)),
            MS,
            &mut |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("no macOS slot became free") && e.contains("at most 2"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn a_listing_error_still_tries() {
        let out = when_slot_free(
            &mut Fake([Err(anyhow::anyhow!("boom"))].into()),
            || async { Ok::<_, anyhow::Error>(1) },
            None,
            MS,
            &mut |_| {},
        )
        .await
        .unwrap();
        assert_eq!(out, 1);
    }

    #[test]
    fn conflict_detection_covers_raw_and_explained_text() {
        assert!(is_slot_conflict(&anyhow::anyhow!(
            "POST /v1/vms/x/start: 409 Conflict: already runs 2 macOS guests"
        )));
        assert!(is_slot_conflict(&anyhow::anyhow!(
            "cannot start a: this Mac already runs 2 macOS VMs (Apple's licence limit)"
        )));
        assert!(!is_slot_conflict(&anyhow::anyhow!(
            "POST /v1/vms/x/start: 409 Conflict: start it first"
        )));
        assert!(!is_slot_conflict(&anyhow::anyhow!("503 locked")));
    }

    #[test]
    fn flags_parse_and_queue_timeout_needs_queue() {
        use clap::Parser;
        #[derive(Parser)]
        struct C {
            #[command(flatten)]
            q: QueueOpts,
        }
        let c = C::try_parse_from(["x", "--queue", "--queue-timeout", "30"]).unwrap();
        assert!(c.q.queue);
        assert_eq!(c.q.queue_timeout, 30);
        assert!(!C::try_parse_from(["x"]).unwrap().q.queue);
        assert!(C::try_parse_from(["x", "--queue-timeout", "30"]).is_err());
    }
}
