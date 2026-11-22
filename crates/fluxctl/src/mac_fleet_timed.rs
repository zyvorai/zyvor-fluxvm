// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet`: running a child process (ssh, or a local shell) with a deadline, and the `LocalTransport` that lets the
//! fleet scripts run on this Mac. The transport trait stays in `mac_fleet_ssh`; this is the part that can time out.

use crate::mac_fleet_remote::Secret;
use crate::mac_fleet_ssh::{ExecOut, Target, Transport};
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Exit code reported for a command that ran past its deadline (the one `timeout(1)` uses).
pub const TIMED_OUT: i32 = 124;

/// Run `cmd` with `stdin` (never argv) and kill it when `timeout` passes. `None` waits for as long as it takes.
pub fn run_with_timeout(
    mut cmd: Command,
    stdin: Option<&Secret>,
    timeout: Option<Duration>,
    what: &str,
) -> Result<ExecOut> {
    // Own process group, so a timeout can kill the whole tree: a shell that forks (dash does) would otherwise leave a
    // grandchild holding the output pipes open, and the reads below would wait for it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {what}"))?;
    if let (Some(s), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // A command that exits early closes the pipe; its exit code says why.
        let _ = pipe.write_all(s.expose());
    }
    let drain = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            b
        })
    };
    let out = drain(Box::new(child.stdout.take().context("no stdout pipe")?));
    let err = drain(Box::new(child.stderr.take().context("no stderr pipe")?));
    let deadline = timeout.map(|t| Instant::now() + t);
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            timed_out = true;
            #[cfg(unix)]
            if let Ok(pid) = i32::try_from(child.id()) {
                // SAFETY: killpg only sends a signal; the group id is our child's pid (process_group(0) above).
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let mut stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if timed_out {
        stderr.push_str(&format!(
            "\ntimed out after {}s",
            timeout.map(|t| t.as_secs()).unwrap_or(0)
        ));
    }
    Ok(ExecOut {
        code: if timed_out {
            TIMED_OUT
        } else {
            status.and_then(|s| s.code()).unwrap_or(255)
        },
        stdout,
        stderr,
    })
}

/// Runs the fleet scripts on this Mac through `/bin/sh -c` instead of ssh (`mac runner` without `--fleet`).
pub struct LocalTransport {
    pub timeout: Option<Duration>,
}

impl Transport for LocalTransport {
    fn run(&self, t: &Target, command: &str, stdin: Option<&Secret>) -> Result<ExecOut> {
        self.run_timed(t, command, stdin, self.timeout)
    }
    fn run_timed(
        &self,
        _t: &Target,
        command: &str,
        stdin: Option<&Secret>,
        timeout: Option<Duration>,
    ) -> Result<ExecOut> {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(command);
        run_with_timeout(c, stdin, timeout, "/bin/sh")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_command_past_its_deadline_is_killed_and_reported_as_124() {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg("sleep 5");
        let t = Instant::now();
        let o = run_with_timeout(c, None, Some(Duration::from_millis(200)), "sh").unwrap();
        assert!(t.elapsed() < Duration::from_secs(3));
        assert_eq!(o.code, TIMED_OUT);
        assert!(!o.ssh_failed());
        assert!(o.stderr.contains("timed out"), "{}", o.stderr);
    }

    #[test]
    fn a_command_inside_its_deadline_keeps_output_and_status() {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg("echo out; echo err >&2; exit 3");
        let o = run_with_timeout(c, None, Some(Duration::from_secs(10)), "sh").unwrap();
        assert_eq!((o.code, o.stdout.trim(), o.stderr.trim()), (3, "out", "err"));
    }

    #[test]
    fn stdin_reaches_the_command_and_not_its_arguments() {
        let t = LocalTransport { timeout: None };
        let target = Target::parse("localhost").unwrap();
        let s = Secret::new(b"s3cret-value".to_vec());
        let o = t.run(&target, "read -r x; echo got-$x", Some(&s)).unwrap();
        assert_eq!(o.stdout.trim(), "got-s3cret-value");
    }
}
