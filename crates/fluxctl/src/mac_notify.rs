// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Tell someone a Mac run ended: a macOS notification (`osascript`) and, when given, a JSON POST to a webhook URL
//! (Slack, Discord and generic receivers all accept the `text` / `content` keys sent). Delivery sits behind [`Notifier`] so tests
//! use a fake. A notification that cannot be delivered never fails the run: [`deliver`] returns the plain-English problems and the
//! caller prints them. The webhook URL is a secret: it is never printed, logged or put in an error message.
//!
//! Also `fluxctl mac notify test` (send one now, to check the setup) and `fluxctl mac notify wait NAME` (notify when a detached
//! instance stops or is removed).

use crate::mac_errors::coded;
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Duration;

/// What happened, in words a person reads in a banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub what: String,
    pub ok: bool,
    pub detail: String,
}

impl Event {
    pub fn title(&self) -> String {
        format!(
            "fluxctl mac: {} {}",
            self.what,
            if self.ok { "succeeded" } else { "failed" }
        )
    }
    pub fn payload(&self) -> Value {
        let line = format!("{}: {}", self.title(), self.detail);
        json!({"text": line, "content": line, "what": self.what, "ok": self.ok, "detail": self.detail})
    }
}

/// Where to send it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Targets {
    pub desktop: bool,
    pub webhook: Option<String>,
}

impl Targets {
    pub fn any(&self) -> bool {
        self.desktop || self.webhook.is_some()
    }
}

pub trait Notifier {
    async fn desktop(&self, title: &str, body: &str) -> Result<()>;
    async fn webhook(&self, url: &str, payload: &Value) -> Result<()>;
}

/// Send to every target; returns what could not be delivered, each with what to do about it.
pub async fn deliver<N: Notifier>(n: &N, t: &Targets, e: &Event) -> Vec<String> {
    let mut problems = Vec::new();
    if t.desktop
        && let Err(err) = n.desktop(&e.title(), &e.detail).await
    {
        problems.push(format!("{err:#}"));
    }
    if let Some(url) = &t.webhook
        && let Err(err) = n.webhook(url, &e.payload()).await
    {
        problems.push(format!("{err:#}"));
    }
    problems
}

/// Print delivery problems; they never change the exit status of the run.
pub fn warn(problems: &[String]) {
    for p in problems {
        eprintln!("warning: {p}");
    }
}

/// The real thing: `osascript` and an HTTP POST.
pub struct Real;

/// The AppleScript reads its text from argv, so a title or body can never be taken for script.
pub fn osascript_args(title: &str, body: &str) -> Vec<String> {
    vec![
        "-e".into(),
        "on run argv".into(),
        "-e".into(),
        "display notification (item 1 of argv) with title (item 2 of argv)".into(),
        "-e".into(),
        "end run".into(),
        body.into(),
        title.into(),
    ]
}

impl Notifier for Real {
    async fn desktop(&self, title: &str, body: &str) -> Result<()> {
        let out = tokio::process::Command::new("osascript")
            .args(osascript_args(title, body))
            .output()
            .await
            .map_err(|e| {
                coded(
                    format!(
                        "could not show a macOS notification: osascript did not start ({e}). Desktop notifications need a Mac with a logged-in desktop session; on anything else use --notify-webhook URL"
                    ),
                    "MAC_NOTIFY_FAILED",
                )
            })?;
        if out.status.success() {
            return Ok(());
        }
        Err(coded(
            format!(
                "macOS refused the notification ({}). Allow notifications for Script Editor / osascript in System Settings > Notifications, and run this from a logged-in desktop session (not a bare ssh login)",
                String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .lines()
                    .next()
                    .unwrap_or("no reason given")
            ),
            "MAC_NOTIFY_FAILED",
        ))
    }

    async fn webhook(&self, url: &str, payload: &Value) -> Result<()> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        match client.post(url).json(payload).send().await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => Err(coded(
                format!(
                    "the webhook answered {}. Check the URL is still valid (receivers revoke them) and re-add it with `fluxctl mac schedule add ... --notify-webhook URL`, or test it with `fluxctl mac notify test --webhook URL`",
                    r.status()
                ),
                "MAC_NOTIFY_FAILED",
            )),
            // The error text of reqwest names the URL; the URL may hold a token.
            Err(e) => Err(coded(
                format!(
                    "could not reach the webhook ({}). Check this Mac's network and the URL, then try `fluxctl mac notify test --webhook URL`",
                    e.without_url()
                ),
                "MAC_NOTIFY_FAILED",
            )),
        }
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum NotifyCmd {
    /// Send a test notification now, to check that banners and the webhook work.
    Test {
        /// POST to this URL too (or set FLUXVM_MAC_NOTIFY_WEBHOOK, which keeps it out of shell history).
        #[arg(long, env = "FLUXVM_MAC_NOTIFY_WEBHOOK", hide_env_values = true)]
        webhook: Option<String>,
        /// Skip the macOS banner (only the webhook).
        #[arg(long)]
        no_desktop: bool,
    },
    /// Wait until the instance stops or is removed, then notify. For detached runs (`mac run -d`).
    Wait {
        name: String,
        #[arg(long, env = "FLUXVM_MAC_NOTIFY_WEBHOOK", hide_env_values = true)]
        webhook: Option<String>,
        #[arg(long)]
        no_desktop: bool,
        /// Give up after this long (`2h`); default is to wait forever.
        #[arg(long)]
        timeout: Option<String>,
    },
}

impl NotifyCmd {
    pub fn is_client_side(&self) -> bool {
        matches!(self, NotifyCmd::Test { .. })
    }
}

fn checked_targets(webhook: Option<String>, no_desktop: bool) -> Result<Targets> {
    if let Some(w) = &webhook {
        fluxvm_core::mac_schedule::check_webhook(w).map_err(|m| coded(m, "MAC_NOTIFY_FAILED"))?;
    }
    Ok(Targets {
        desktop: !no_desktop,
        webhook,
    })
}

pub async fn run_client(c: NotifyCmd) -> Result<()> {
    let NotifyCmd::Test {
        webhook,
        no_desktop,
    } = c
    else {
        anyhow::bail!("internal: not a client-side notify command");
    };
    let t = checked_targets(webhook, no_desktop)?;
    let e = Event {
        what: "test".into(),
        ok: true,
        detail: "If you can read this, notifications work.".into(),
    };
    let problems = deliver(&Real, &t, &e).await;
    if problems.is_empty() {
        println!("sent");
        Ok(())
    } else {
        anyhow::bail!("{}", problems.join("\n"))
    }
}

pub async fn run_daemon(r: &crate::remote::Remote, c: NotifyCmd) -> Result<()> {
    let NotifyCmd::Wait {
        name,
        webhook,
        no_desktop,
        timeout,
    } = c
    else {
        anyhow::bail!("internal: not a daemon notify command");
    };
    let t = checked_targets(webhook, no_desktop)?;
    let limit = timeout
        .as_deref()
        .map(fluxvm_core::mac_schedule::parse_duration)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let start = std::time::Instant::now();
    let (ok, detail) = loop {
        match crate::mac_cmd::find_instance(r, &name).await {
            Err(_) => break (true, format!("{name} is gone (removed or ran with --rm).")),
            Ok(vm) if vm["status"].as_str() == Some("failed") => {
                break (
                    false,
                    format!("{name} failed. See `fluxctl mac inspect {name}`."),
                );
            }
            Ok(vm) if vm["status"].as_str() == Some("stopped") => {
                break (true, format!("{name} stopped."));
            }
            Ok(_) => {}
        }
        if limit.is_some_and(|l| start.elapsed().as_secs() >= l) {
            anyhow::bail!(
                "{name} was still running when the wait ended. Run `fluxctl mac notify wait {name}` again to keep waiting"
            );
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    let e = Event {
        what: name,
        ok,
        detail,
    };
    warn(&deliver(&Real, &t, &e).await);
    Ok(())
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct Fake {
        pub desktop_calls: Mutex<Vec<(String, String)>>,
        pub webhook_calls: Mutex<Vec<(String, Value)>>,
        pub fail_desktop: bool,
        pub fail_webhook: bool,
    }

    impl Notifier for Fake {
        async fn desktop(&self, title: &str, body: &str) -> Result<()> {
            self.desktop_calls
                .lock()
                .unwrap()
                .push((title.into(), body.into()));
            if self.fail_desktop {
                anyhow::bail!("banner refused");
            }
            Ok(())
        }
        async fn webhook(&self, url: &str, payload: &Value) -> Result<()> {
            self.webhook_calls
                .lock()
                .unwrap()
                .push((url.into(), payload.clone()));
            if self.fail_webhook {
                anyhow::bail!("webhook refused");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;

    fn ev(ok: bool) -> Event {
        Event {
            what: "nightly".into(),
            ok,
            detail: "exit 0".into(),
        }
    }

    #[tokio::test]
    async fn delivers_to_every_target() {
        let f = Fake::default();
        let t = Targets {
            desktop: true,
            webhook: Some("https://h.example/x".into()),
        };
        assert!(deliver(&f, &t, &ev(true)).await.is_empty());
        assert_eq!(
            f.desktop_calls.lock().unwrap()[0].0,
            "fluxctl mac: nightly succeeded"
        );
        let w = f.webhook_calls.lock().unwrap();
        assert_eq!(w[0].0, "https://h.example/x");
        assert_eq!(w[0].1["ok"], true);
        assert!(
            w[0].1["text"]
                .as_str()
                .unwrap()
                .contains("nightly succeeded")
        );
    }

    #[tokio::test]
    async fn a_failing_target_is_reported_not_fatal_and_the_other_still_runs() {
        let f = Fake {
            fail_desktop: true,
            ..Default::default()
        };
        let t = Targets {
            desktop: true,
            webhook: Some("https://h.example/x".into()),
        };
        let p = deliver(&f, &t, &ev(false)).await;
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("banner refused"));
        assert_eq!(f.webhook_calls.lock().unwrap().len(), 1);
        assert!(
            f.webhook_calls.lock().unwrap()[0].1["text"]
                .as_str()
                .unwrap()
                .contains("failed")
        );
    }

    #[tokio::test]
    async fn nothing_is_sent_when_no_target_is_set() {
        let f = Fake::default();
        assert!(!Targets::default().any());
        assert!(deliver(&f, &Targets::default(), &ev(true)).await.is_empty());
        assert!(f.desktop_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn osascript_gets_text_as_arguments_never_as_script() {
        let a = osascript_args("t\" & (do shell script \"x\")", "b");
        assert!(!a[..6].iter().any(|s| s.contains("do shell script")));
        assert_eq!(a[6], "b");
        assert!(a[7].contains("do shell script"));
    }

    #[test]
    fn webhook_urls_are_checked_before_use() {
        assert!(checked_targets(Some("not a url".into()), false).is_err());
        assert!(
            !checked_targets(Some("https://h.example/x".into()), true)
                .unwrap()
                .desktop
        );
    }
}
