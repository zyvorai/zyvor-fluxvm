// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac wait-ready NAME [--timeout S] [--check CMD]`: block until an instance is up, so a caller does not poll SSH
//! itself. "Up" means the VM is `running`, has an address, and answers on TCP port 22; with `--check CMD`, `CMD` must also
//! exit 0 in the guest over SSH (for example `xcodebuild -version`, or `test -d "/Volumes/My Shared Files"`).
//!
//! The wait loop is generic over a [`Probe`] so it is unit tested with a fake clock-free probe. The real probe talks to the
//! daemon and `ssh`; it has not been run against a real VM.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// What the wait loop asks the outside world.
pub(crate) trait Probe {
    /// The instance's current VM record.
    async fn vm(&mut self) -> Result<Value>;
    /// Does TCP port 22 at `ip` accept a connection?
    async fn ssh_open(&mut self, ip: &str) -> bool;
    /// Does `--check` exit 0 in the guest?
    async fn check(&mut self, ip: &str, cmd: &str) -> bool;
}

#[derive(Debug, Clone)]
pub struct ReadyOpts {
    /// Give up after this long; `None` waits forever.
    pub timeout: Option<Duration>,
    pub poll: Duration,
    pub check: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ready {
    pub ip: String,
    pub waited: Duration,
}

/// What the loop is still waiting for, for the timeout message.
fn blocker(status: &str, ip: Option<&str>, ssh: Option<bool>, check: Option<bool>) -> String {
    match (status, ip, ssh, check) {
        (s, _, _, _) if s != "running" => format!("the VM is {s}"),
        (_, None, _, _) => "the VM is running but has no address yet".into(),
        (_, Some(ip), Some(false), _) => format!("{ip} does not answer on port 22 yet"),
        (_, Some(ip), _, Some(false)) => {
            format!("{ip} answers on SSH but --check does not pass yet")
        }
        _ => "not ready".into(),
    }
}

pub(crate) async fn wait_ready<P: Probe>(p: &mut P, name: &str, o: &ReadyOpts) -> Result<Ready> {
    let start = Instant::now();
    loop {
        let vm = p.vm().await?;
        let status = crate::mac_inspect::status(&vm).to_string();
        match status.as_str() {
            "stopped" | "failed" => bail!(
                "{name} is {status}: it will not become ready. Start it with `fluxctl mac start {name}`{}",
                vm["error"]
                    .as_str()
                    .map(|e| format!(" (error: {e})"))
                    .unwrap_or_default()
            ),
            "paused" => bail!("{name} is paused: `fluxctl mac unpause {name}` first"),
            _ => {}
        }
        let ip = vm["guest_ip"].as_str().filter(|s| !s.is_empty());
        let (mut ssh, mut chk) = (None, None);
        if status == "running"
            && let Some(ip) = ip
        {
            ssh = Some(p.ssh_open(ip).await);
            if ssh == Some(true) {
                match &o.check {
                    None => chk = Some(true),
                    Some(c) => chk = Some(p.check(ip, c).await),
                }
            }
            if chk == Some(true) {
                return Ok(Ready {
                    ip: ip.to_string(),
                    waited: start.elapsed(),
                });
            }
        }
        if let Some(t) = o.timeout
            && start.elapsed() >= t
        {
            bail!(
                "{name} was not ready after {}s: {} (a first boot of a clone can take minutes; raise --timeout)",
                t.as_secs(),
                blocker(&status, ip, ssh, chk)
            );
        }
        tokio::time::sleep(o.poll).await;
    }
}

/// The document `mac wait-ready --json` prints.
pub fn ready_document(name: &str, id: &str, r: &Ready) -> Value {
    json!({
        "schema_version": crate::mac_json::SCHEMA_VERSION,
        "name": name,
        "id": id,
        "ready": true,
        "ip": r.ip,
        "waited_secs": r.waited.as_secs(),
    })
}

/// The real probe: the daemon for state, a TCP connect for port 22, `ssh` for `--check`.
pub(crate) struct DaemonProbe<'a> {
    pub r: &'a crate::remote::Remote,
    pub id: String,
    pub user: String,
}

impl Probe for DaemonProbe<'_> {
    async fn vm(&mut self) -> Result<Value> {
        self.r
            .call(reqwest::Method::GET, &format!("/v1/vms/{}", self.id), None)
            .await
            .map_err(|e| crate::mac_instance::explain(e, "read the instance"))
    }

    async fn ssh_open(&mut self, ip: &str) -> bool {
        tokio::time::timeout(
            Duration::from_secs(3),
            tokio::net::TcpStream::connect((ip, 22)),
        )
        .await
        .is_ok_and(|c| c.is_ok())
    }

    async fn check(&mut self, ip: &str, cmd: &str) -> bool {
        // Options must precede `user@host`: anything after it is the remote command.
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
        c.arg(format!("{}@{ip}", self.user))
            .arg(cmd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        c.status().await.is_ok_and(|s| s.success())
    }
}

/// `fluxctl mac wait-ready`.
pub async fn run(
    r: &crate::remote::Remote,
    name: &str,
    timeout: u64,
    user: &str,
    check: Option<String>,
    json_out: bool,
) -> Result<()> {
    let vm = crate::mac_cmd::find_instance(r, name).await?;
    let id = vm["id"].as_str().unwrap_or_default().to_string();
    let mut probe = DaemonProbe {
        r,
        id: id.clone(),
        user: user.to_string(),
    };
    let opts = ReadyOpts {
        timeout: (timeout > 0).then(|| Duration::from_secs(timeout)),
        poll: Duration::from_secs(2),
        check,
    };
    let ready = wait_ready(&mut probe, name, &opts).await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&ready_document(name, &id, &ready))?
        );
    } else {
        println!(
            "ready: {name} {} (after {}s)",
            ready.ip,
            ready.waited.as_secs()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Plays back a list of VM records; then repeats the last one.
    struct Fake {
        vms: VecDeque<Value>,
        last: Value,
        ssh: VecDeque<bool>,
        checks: VecDeque<bool>,
        vm_calls: usize,
        check_cmds: Vec<String>,
    }

    impl Fake {
        fn new(vms: Vec<Value>, ssh: Vec<bool>, checks: Vec<bool>) -> Self {
            Self {
                vms: vms.into(),
                last: Value::Null,
                ssh: ssh.into(),
                checks: checks.into(),
                vm_calls: 0,
                check_cmds: vec![],
            }
        }
    }

    impl Probe for Fake {
        async fn vm(&mut self) -> Result<Value> {
            self.vm_calls += 1;
            if let Some(v) = self.vms.pop_front() {
                self.last = v;
            }
            Ok(self.last.clone())
        }
        async fn ssh_open(&mut self, _: &str) -> bool {
            self.ssh.pop_front().unwrap_or(true)
        }
        async fn check(&mut self, _: &str, cmd: &str) -> bool {
            self.check_cmds.push(cmd.into());
            self.checks.pop_front().unwrap_or(true)
        }
    }

    fn vm(status: &str, ip: Option<&str>) -> Value {
        json!({"status": status, "guest_ip": ip})
    }

    fn opts(check: Option<&str>, timeout_ms: Option<u64>) -> ReadyOpts {
        ReadyOpts {
            timeout: timeout_ms.map(Duration::from_millis),
            poll: Duration::from_millis(1),
            check: check.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn waits_through_creating_no_address_and_closed_port() {
        let mut f = Fake::new(
            vec![
                vm("creating", None),
                vm("running", None),
                vm("running", Some("10.0.0.5")),
                vm("running", Some("10.0.0.5")),
            ],
            vec![false, true],
            vec![],
        );
        let r = wait_ready(&mut f, "a", &opts(None, Some(5000)))
            .await
            .unwrap();
        assert_eq!(r.ip, "10.0.0.5");
        assert_eq!(f.vm_calls, 4);
        assert!(f.check_cmds.is_empty(), "no --check, no ssh command");
    }

    #[tokio::test]
    async fn check_must_pass_too() {
        let mut f = Fake::new(
            vec![vm("running", Some("10.0.0.5"))],
            vec![],
            vec![false, false, true],
        );
        let r = wait_ready(&mut f, "a", &opts(Some("xcodebuild -version"), Some(5000)))
            .await
            .unwrap();
        assert_eq!(r.ip, "10.0.0.5");
        assert_eq!(f.check_cmds, vec!["xcodebuild -version"; 3]);
    }

    #[tokio::test]
    async fn a_stopped_or_failed_vm_fails_at_once() {
        for st in ["stopped", "failed"] {
            let mut f = Fake::new(vec![vm(st, None)], vec![], vec![]);
            let e = wait_ready(&mut f, "a", &opts(None, None))
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains(st) && e.contains("mac start a"), "{e}");
            assert_eq!(f.vm_calls, 1);
        }
        let mut f = Fake::new(vec![vm("paused", Some("1.1.1.1"))], vec![], vec![]);
        assert!(
            wait_ready(&mut f, "a", &opts(None, None))
                .await
                .unwrap_err()
                .to_string()
                .contains("unpause")
        );
    }

    #[tokio::test]
    async fn timeout_names_what_is_missing() {
        let cases = [
            (vm("creating", None), vec![], "the VM is creating"),
            (vm("running", None), vec![], "no address"),
            (
                vm("running", Some("10.0.0.5")),
                vec![false; 1000],
                "port 22",
            ),
        ];
        for (v, ssh, want) in cases {
            let mut f = Fake::new(vec![v], ssh, vec![]);
            let e = wait_ready(&mut f, "a", &opts(None, Some(20)))
                .await
                .unwrap_err()
                .to_string();
            assert!(e.contains("not ready after") && e.contains(want), "{e}");
        }
        let mut f = Fake::new(
            vec![vm("running", Some("10.0.0.5"))],
            vec![],
            vec![false; 1000],
        );
        let e = wait_ready(&mut f, "a", &opts(Some("false"), Some(20)))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("--check does not pass"), "{e}");
    }

    #[tokio::test]
    async fn a_daemon_error_propagates() {
        struct Broken;
        impl Probe for Broken {
            async fn vm(&mut self) -> Result<Value> {
                bail!("GET /v1/vms/x: 404 Not Found")
            }
            async fn ssh_open(&mut self, _: &str) -> bool {
                true
            }
            async fn check(&mut self, _: &str, _: &str) -> bool {
                true
            }
        }
        assert!(
            wait_ready(&mut Broken, "a", &opts(None, None))
                .await
                .unwrap_err()
                .to_string()
                .contains("404")
        );
    }

    #[test]
    fn ready_document_shape() {
        let d = ready_document(
            "a",
            "id1",
            &Ready {
                ip: "1.2.3.4".into(),
                waited: Duration::from_secs(61),
            },
        );
        assert_eq!(
            d,
            json!({"schema_version": 1, "name": "a", "id": "id1", "ready": true, "ip": "1.2.3.4", "waited_secs": 61})
        );
    }
}
