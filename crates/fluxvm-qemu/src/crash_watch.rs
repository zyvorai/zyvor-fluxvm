// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! Guest crash detection for `on_crash`: the QEMU arguments (a pvpanic device,
//! an i6300esb watchdog, the `-action` policy and a dedicated event monitor),
//! the parser for the QMP events they produce, and a watcher that reads them.
//!
//! The main QMP socket serves one client at a time and is busy with control
//! commands, so crash events come over a second, control-mode monitor
//! (`<run_dir>/qmp/<id>.evt.sock`). Without `on_crash` none of this is added to
//! the command line.
//!
//! Needs QEMU 6.0 or newer for `-action`. The guest needs a pvpanic driver
//! (`CONFIG_PVPANIC`, in Linux 5.x+ distro kernels) and a running watchdog
//! daemon (the `i6300esb` driver plus `watchdog`/systemd `RuntimeWatchdogSec`).

use anyhow::{Context, Result};
use fluxvm_core::config::Config;
use fluxvm_core::lifecycle::OnCrash;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Event monitor socket, next to the QMP socket.
pub fn event_socket_path(cfg: &Config, id: uuid::Uuid) -> PathBuf {
    cfg.run_dir.join("qmp").join(format!("{id}.evt.sock"))
}

/// `-action panic=...,watchdog=...` for a crash policy.
fn actions(on_crash: OnCrash) -> (&'static str, &'static str) {
    match on_crash {
        // QEMU exits and the supervisor restarts; a watchdog resets in place.
        OnCrash::Restart => ("shutdown", "reset"),
        OnCrash::Poweroff => ("shutdown", "shutdown"),
        OnCrash::Pause | OnCrash::CoredumpIfExisting => ("pause", "pause"),
    }
}

/// The extra QEMU arguments for `on_crash`; empty when unset.
pub fn args(cfg: &Config, id: uuid::Uuid, on_crash: Option<OnCrash>) -> Vec<String> {
    let Some(on_crash) = on_crash else {
        return Vec::new();
    };
    let (panic, watchdog) = actions(on_crash);
    let sock = event_socket_path(cfg, id);
    vec![
        "-device".into(),
        "pvpanic".into(),
        "-device".into(),
        "i6300esb,id=watchdog0".into(),
        "-action".into(),
        format!("panic={panic}"),
        "-action".into(),
        format!("watchdog={watchdog}"),
        "-chardev".into(),
        format!("socket,id=evt0,path={},server=on,wait=off", sock.display()),
        "-mon".into(),
        "chardev=evt0,mode=control".into(),
    ]
}

/// A crash-related QMP event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashEvent {
    /// `GUEST_PANICKED`; `action` is what QEMU did (`pause`, `poweroff`, ...).
    Panic { action: String },
    /// `WATCHDOG`; `action` is `reset`, `shutdown`, `pause`, ...
    Watchdog { action: String },
    /// `SHUTDOWN` the guest asked for (not a host or QEMU-initiated one).
    GuestShutdown { reason: String },
}

impl CrashEvent {
    /// The event name recorded in `events.jsonl`.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Panic { .. } => "vm.guest-panic",
            Self::Watchdog { .. } => "vm.watchdog",
            Self::GuestShutdown { .. } => "vm.guest-shutdown",
        }
    }

    pub fn action(&self) -> &str {
        match self {
            Self::Panic { action } | Self::Watchdog { action } => action,
            Self::GuestShutdown { reason } => reason,
        }
    }

    fn is_crash(&self) -> bool {
        !matches!(self, Self::GuestShutdown { .. })
    }
}

/// Parses one QMP line. Anything that is not a crash event (replies, other
/// events, junk, a host-initiated SHUTDOWN) gives `None`.
pub fn parse_event(line: &str) -> Option<CrashEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let data = v.get("data");
    let text = |k: &str| {
        data.and_then(|d| d.get(k))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match v.get("event")?.as_str()? {
        "GUEST_PANICKED" => Some(CrashEvent::Panic {
            action: text("action"),
        }),
        "WATCHDOG" => Some(CrashEvent::Watchdog {
            action: text("action"),
        }),
        "SHUTDOWN" if data.and_then(|d| d.get("guest")).and_then(Value::as_bool) == Some(true) => {
            Some(CrashEvent::GuestShutdown {
                reason: text("reason"),
            })
        }
        _ => None,
    }
}

/// The `dump-guest-memory` command for `<workspace>/dumps/<stamp>.elf`.
pub fn dump_command(dump_path: &Path) -> Value {
    serde_json::json!({
        "execute": "dump-guest-memory",
        "arguments": {
            // The `pause` action already stopped the guest.
            "paused": false,
            "protocol": format!("file:{}", dump_path.display()),
            "format": "elf",
        }
    })
}

/// Where a coredump for an event at `stamp` goes, if dumping applies: the
/// policy is `coredump-if-existing`, the event is a crash, and the workspace's
/// `dumps` directory already exists.
pub fn dump_target(
    on_crash: OnCrash,
    event: &CrashEvent,
    workspace: &Path,
    stamp: &str,
    dumps_dir_exists: bool,
) -> Option<PathBuf> {
    (on_crash == OnCrash::CoredumpIfExisting && event.is_crash() && dumps_dir_exists)
        .then(|| workspace.join("dumps").join(format!("{stamp}.elf")))
}

/// Reads the event monitor until QEMU closes it, calling `on_event` for each
/// crash event. With `coredump-if-existing` it also asks QEMU for the guest
/// core. Returns when the connection ends.
pub async fn watch(
    sock: &Path,
    on_crash: OnCrash,
    workspace: &Path,
    mut on_event: impl FnMut(&CrashEvent),
) -> Result<()> {
    let stream = UnixStream::connect(sock)
        .await
        .with_context(|| format!("connecting {}", sock.display()))?;
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    // The greeting, then capabilities, which the monitor requires first.
    lines.next_line().await?;
    w.write_all(b"{\"execute\":\"qmp_capabilities\"}\n").await?;
    while let Some(line) = lines.next_line().await? {
        let Some(ev) = parse_event(&line) else {
            continue;
        };
        on_event(&ev);
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let exists = workspace.join("dumps").is_dir();
        if let Some(path) = dump_target(on_crash, &ev, workspace, &stamp, exists) {
            let mut cmd = dump_command(&path).to_string();
            cmd.push('\n');
            w.write_all(cmd.as_bytes()).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            run_dir: "/run/fx".into(),
            ..Config::default()
        }
    }

    fn id() -> uuid::Uuid {
        uuid::Uuid::nil()
    }

    #[test]
    fn no_policy_adds_nothing() {
        assert!(args(&cfg(), id(), None).is_empty());
    }

    #[test]
    fn restart_args() {
        let a = args(&cfg(), id(), Some(OnCrash::Restart));
        assert!(a.windows(2).any(|w| w == ["-device", "pvpanic"]));
        assert!(
            a.windows(2)
                .any(|w| w == ["-device", "i6300esb,id=watchdog0"])
        );
        assert!(a.windows(2).any(|w| w == ["-action", "panic=shutdown"]));
        assert!(a.windows(2).any(|w| w == ["-action", "watchdog=reset"]));
        assert!(
            a.windows(2)
                .any(|w| w == ["-mon", "chardev=evt0,mode=control"])
        );
        let ch = a.iter().find(|s| s.starts_with("socket,id=evt0")).unwrap();
        assert_eq!(
            ch,
            &format!(
                "socket,id=evt0,path=/run/fx/qmp/{}.evt.sock,server=on,wait=off",
                id()
            )
        );
    }

    #[test]
    fn each_policy_maps_to_actions() {
        let want = [
            (OnCrash::Restart, "shutdown", "reset"),
            (OnCrash::Poweroff, "shutdown", "shutdown"),
            (OnCrash::Pause, "pause", "pause"),
            (OnCrash::CoredumpIfExisting, "pause", "pause"),
        ];
        for (p, panic, wd) in want {
            let a = args(&cfg(), id(), Some(p));
            assert!(a.contains(&format!("panic={panic}")), "{p:?}");
            assert!(a.contains(&format!("watchdog={wd}")), "{p:?}");
        }
    }

    #[test]
    fn parses_panic_watchdog_and_guest_shutdown() {
        let p = r#"{"timestamp":{"seconds":1,"microseconds":2},"event":"GUEST_PANICKED","data":{"action":"pause","info":{"type":"hyper-v"}}}"#;
        assert_eq!(
            parse_event(p),
            Some(CrashEvent::Panic {
                action: "pause".into()
            })
        );
        let w = r#"{"event":"WATCHDOG","data":{"action":"reset"},"timestamp":{"seconds":1,"microseconds":2}}"#;
        assert_eq!(
            parse_event(w),
            Some(CrashEvent::Watchdog {
                action: "reset".into()
            })
        );
        let s = r#"{"event":"SHUTDOWN","data":{"guest":true,"reason":"guest-shutdown"}}"#;
        assert_eq!(
            parse_event(s),
            Some(CrashEvent::GuestShutdown {
                reason: "guest-shutdown".into()
            })
        );
    }

    #[test]
    fn ignores_everything_else() {
        for l in [
            r#"{"event":"SHUTDOWN","data":{"guest":false,"reason":"host-qmp-quit"}}"#,
            r#"{"event":"RESET","data":{"guest":true}}"#,
            r#"{"return":{}}"#,
            r#"{"QMP":{"version":{}}}"#,
            "not json",
            "",
            r#"{"event":"SHUTDOWN"}"#,
        ] {
            assert_eq!(parse_event(l), None, "{l:?}");
        }
        // A panic with no data still parses, with an empty action.
        assert_eq!(
            parse_event(r#"{"event":"GUEST_PANICKED"}"#),
            Some(CrashEvent::Panic {
                action: String::new()
            })
        );
    }

    #[test]
    fn event_names() {
        assert_eq!(
            CrashEvent::Panic { action: "x".into() }.event_name(),
            "vm.guest-panic"
        );
        assert_eq!(
            CrashEvent::Watchdog { action: "x".into() }.event_name(),
            "vm.watchdog"
        );
    }

    #[test]
    fn coredump_only_when_asked_and_dir_exists() {
        let ws = Path::new("/ws");
        let panic = CrashEvent::Panic {
            action: "pause".into(),
        };
        let t = dump_target(OnCrash::CoredumpIfExisting, &panic, ws, "S", true);
        assert_eq!(t, Some(PathBuf::from("/ws/dumps/S.elf")));
        assert_eq!(
            dump_target(OnCrash::CoredumpIfExisting, &panic, ws, "S", false),
            None
        );
        assert_eq!(dump_target(OnCrash::Pause, &panic, ws, "S", true), None);
        let down = CrashEvent::GuestShutdown {
            reason: "guest-shutdown".into(),
        };
        assert_eq!(
            dump_target(OnCrash::CoredumpIfExisting, &down, ws, "S", true),
            None
        );
    }

    #[test]
    fn dump_command_shape() {
        let c = dump_command(Path::new("/ws/dumps/S.elf"));
        assert_eq!(c["execute"], "dump-guest-memory");
        assert_eq!(c["arguments"]["protocol"], "file:/ws/dumps/S.elf");
        assert_eq!(c["arguments"]["paused"], false);
    }

    #[tokio::test]
    async fn watch_reads_events_from_a_fake_monitor() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("e.sock");
        let ws = dir.path().to_path_buf();
        std::fs::create_dir(ws.join("dumps")).unwrap();
        let l = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.write_all(b"{\"QMP\":{}}\n").await.unwrap();
            let mut buf = vec![0u8; 64];
            let n = s.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("qmp_capabilities"));
            s.write_all(b"{\"return\":{}}\n{\"event\":\"RESET\",\"data\":{}}\n")
                .await
                .unwrap();
            s.write_all(b"{\"event\":\"GUEST_PANICKED\",\"data\":{\"action\":\"pause\"}}\n")
                .await
                .unwrap();
            let mut rest = String::new();
            let mut tmp = [0u8; 512];
            while !rest.contains("dump-guest-memory") {
                let n = s.read(&mut tmp).await.unwrap();
                assert!(n > 0, "closed before dump command");
                rest.push_str(&String::from_utf8_lossy(&tmp[..n]));
            }
            rest
        });
        let w = watch(&sock, OnCrash::CoredumpIfExisting, &ws, |_| {});
        // The server keeps the socket open after sending, so stop the watcher
        // once the dump command arrived.
        tokio::select! {
            _ = w => panic!("watcher returned early"),
            r = server => assert!(r.unwrap().contains("/dumps/")),
        }
    }
}
