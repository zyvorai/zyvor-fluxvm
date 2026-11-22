// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac-fleet`: how commands reach a Mac. The system `ssh` binary, always with `BatchMode=yes` (never a prompt,
//! so a missing key fails fast instead of hanging a parallel run) and `StrictHostKeyChecking=yes` (the host key must
//! already be in known_hosts) unless the operator passes `--accept-new-hostkey`. The command is one argv word; a secret
//! only ever travels on stdin.

use crate::mac_fleet_remote::{Secret, sh_quote};
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use std::process::Command;

/// One Mac: where to connect and what the fleet knows about it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Target {
    /// The fleet node name (lowercase letters, digits, dashes, dots).
    pub name: String,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<PathBuf>,
    pub k3s: bool,
    pub labels: Vec<String>,
}

impl Target {
    /// `[user@]host[:port]`. The name defaults to the host's first label, lowercased.
    pub fn parse(s: &str) -> Result<Self> {
        let (user, rest) = match s.split_once('@') {
            Some((u, r)) => (Some(u.to_owned()), r),
            None => (None, s),
        };
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (
                h,
                Some(
                    p.parse::<u16>()
                        .with_context(|| format!("bad port in {s:?}"))?,
                ),
            ),
            _ => (rest, None),
        };
        if let Some(u) = &user
            && !valid_ssh_word(u)
        {
            bail!("bad user in {s:?}");
        }
        if !valid_host(host) {
            bail!("bad host in {s:?}: use a name or IPv4 address, optionally user@host:port");
        }
        let first = if host
            .bytes()
            .all(|c| (b'0'..=b'9').contains(&c) || c == b'.')
        {
            host
        } else {
            host.split('.').next().unwrap_or(host)
        };
        Ok(Self {
            name: sanitize_name(first),
            host: host.to_owned(),
            user,
            port,
            ..Self::default()
        })
    }

    /// `user@host` or `host`, as ssh takes it.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// What the operator typed, for tables and errors.
    pub fn label(&self) -> String {
        let mut s = self.destination();
        if let Some(p) = self.port {
            s.push_str(&format!(":{p}"));
        }
        s
    }
}

fn valid_ssh_word(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 64
        && !u.starts_with('-')
        && u.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

/// A host name or IPv4 address; never something ssh could read as an option.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with(['-', '.'])
        && h.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
}

pub fn sanitize_name(s: &str) -> String {
    let n: String = s
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let n = n.trim_matches(['-', '.']).to_owned();
    n.chars().take(63).collect()
}

#[derive(Debug, Clone, Default)]
pub struct SshOpts {
    /// Used for targets without their own identity.
    pub identity: Option<PathBuf>,
    pub known_hosts: Option<PathBuf>,
    /// `--accept-new-hostkey`: trust a host key the first time it is seen. Off by default.
    pub accept_new: bool,
    pub connect_timeout_secs: u32,
}

/// The full `ssh` argument vector for one remote command.
pub fn ssh_args(t: &Target, o: &SshOpts, command: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-T".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!(
            "StrictHostKeyChecking={}",
            if o.accept_new { "accept-new" } else { "yes" }
        ),
        "-o".into(),
        format!("ConnectTimeout={}", o.connect_timeout_secs.max(1)),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    if let Some(k) = &o.known_hosts {
        a.push("-o".into());
        a.push(format!("UserKnownHostsFile={}", k.display()));
    }
    if let Some(i) = t.identity.as_ref().or(o.identity.as_ref()) {
        a.push("-i".into());
        a.push(i.display().to_string());
        a.push("-o".into());
        a.push("IdentitiesOnly=yes".into());
    }
    if let Some(p) = t.port {
        a.push("-p".into());
        a.push(p.to_string());
    }
    a.push("--".into());
    a.push(t.destination());
    a.push(command.to_owned());
    a
}

/// `ssh ...` as one copy-pasteable line, for `--dry-run`.
pub fn display_command(args: &[String]) -> String {
    let mut s = String::from("ssh");
    for a in args {
        s.push(' ');
        if a.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_=.,:/@+".contains(&c))
        {
            s.push_str(a);
        } else {
            s.push_str(&sh_quote(a));
        }
    }
    s
}

#[derive(Debug, Clone, Default)]
pub struct ExecOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOut {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    /// ssh itself failed (could not connect, authenticate or verify the host key), not the remote command.
    pub fn ssh_failed(&self) -> bool {
        self.code == 255
    }
}

pub trait Transport: Send + Sync {
    /// Run `command` on `t`. `stdin`, if any, is written to the remote command's stdin; it is never part of argv.
    fn run(&self, t: &Target, command: &str, stdin: Option<&Secret>) -> Result<ExecOut>;
    /// Like `run`, but the command is killed after `timeout` and reported as exit 124. Transports that cannot time out
    /// (the test fake, the dry-run printer) ignore it.
    fn run_timed(
        &self,
        t: &Target,
        command: &str,
        stdin: Option<&Secret>,
        timeout: Option<std::time::Duration>,
    ) -> Result<ExecOut> {
        let _ = timeout;
        self.run(t, command, stdin)
    }
    /// The dry-run printer, when this is one.
    fn as_dry(&self) -> Option<&DryTransport> {
        None
    }
}

pub struct SshTransport {
    pub opts: SshOpts,
    pub binary: String,
    /// Deadline for every ssh call (each step of a plan is one call). `None`: no deadline.
    pub step_timeout: Option<std::time::Duration>,
}

impl SshTransport {
    pub fn new(opts: SshOpts) -> Self {
        Self {
            opts,
            binary: std::env::var("FLUXVM_SSH").unwrap_or_else(|_| "ssh".into()),
            step_timeout: None,
        }
    }
}

impl Transport for SshTransport {
    fn run(&self, t: &Target, command: &str, stdin: Option<&Secret>) -> Result<ExecOut> {
        self.run_timed(t, command, stdin, self.step_timeout)
    }
    fn run_timed(
        &self,
        t: &Target,
        command: &str,
        stdin: Option<&Secret>,
        timeout: Option<std::time::Duration>,
    ) -> Result<ExecOut> {
        let mut c = Command::new(&self.binary);
        c.args(ssh_args(t, &self.opts, command));
        let what = format!("{} (is OpenSSH installed?)", self.binary);
        crate::mac_fleet_timed::run_with_timeout(c, stdin, timeout, &what)
    }
}

/// `--dry-run`: prints each ssh command line and pretends it succeeded with no output.
pub struct DryTransport {
    pub opts: SshOpts,
    pub lines: std::sync::Mutex<Vec<String>>,
}

impl DryTransport {
    pub fn new(opts: SshOpts) -> Self {
        Self {
            opts,
            lines: Default::default(),
        }
    }
}

impl Transport for DryTransport {
    fn as_dry(&self) -> Option<&DryTransport> {
        Some(self)
    }
    fn run(&self, t: &Target, command: &str, stdin: Option<&Secret>) -> Result<ExecOut> {
        let mut line = display_command(&ssh_args(t, &self.opts, command));
        if let Some(s) = stdin {
            line.push_str(&format!(
                "   # stdin: the join token ({} bytes, not shown)",
                s.len()
            ));
        }
        self.lines.lock().unwrap().push(line);
        Ok(ExecOut::default())
    }
}

/// What a failed ssh (exit 255) most likely means, with the fix.
pub fn explain_ssh_failure(t: &Target, stderr: &str) -> String {
    let h = t.label();
    let s = stderr.to_ascii_lowercase();
    let hint = if s.contains("remote host identification has changed") {
        format!(
            "the host key of {h} changed (a reinstalled Mac, or something in the middle): verify it, then `ssh-keygen -R {}` and connect once by hand",
            t.host
        )
    } else if s.contains("host key verification failed") || s.contains("no matching host key") {
        format!(
            "the host key of {h} is not trusted yet: check its fingerprint on the Mac (`ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`), compare with `ssh-keyscan -t ed25519 {}`, then add it to ~/.ssh/known_hosts, or pass --accept-new-hostkey if you accept trusting it on first use",
            t.host
        )
    } else if s.contains("permission denied") {
        format!(
            "{h} refused the key (BatchMode never asks for a password): `ssh-copy-id {}` once, or pass --identity",
            t.destination()
        )
    } else if s.contains("connection refused") {
        format!(
            "{h} refused the connection: turn on Remote Login (System Settings > General > Sharing > Remote Login, or `sudo systemsetup -setremotelogin on` on the Mac)"
        )
    } else if s.contains("timed out") || s.contains("no route") || s.contains("could not resolve") {
        format!(
            "{h} is not reachable: check that it is on, awake (`caffeinate`, Energy settings) and on this network"
        )
    } else {
        "ssh failed".to_owned()
    };
    hint
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    pub struct Call {
        pub host: String,
        pub command: String,
        pub stdin: Option<Vec<u8>>,
    }

    type Responder = Box<dyn Fn(&Target, &str) -> ExecOut + Send + Sync>;

    /// Records every call and answers through a closure.
    pub struct FakeTransport {
        pub calls: Mutex<Vec<Call>>,
        respond: Responder,
    }

    impl FakeTransport {
        pub fn new(f: impl Fn(&Target, &str) -> ExecOut + Send + Sync + 'static) -> Self {
            Self {
                calls: Default::default(),
                respond: Box::new(f),
            }
        }
        pub fn calls_for(&self, host: &str) -> Vec<Call> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.host == host)
                .cloned()
                .collect()
        }
    }

    impl Transport for FakeTransport {
        fn run(&self, t: &Target, command: &str, stdin: Option<&Secret>) -> Result<ExecOut> {
            self.calls.lock().unwrap().push(Call {
                host: t.host.clone(),
                command: command.to_owned(),
                stdin: stdin.map(|s| s.expose().to_vec()),
            });
            Ok((self.respond)(t, command))
        }
    }

    pub fn ok(stdout: &str) -> ExecOut {
        ExecOut {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn failed(code: i32, stderr: &str) -> ExecOut {
        ExecOut {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_and_reject_option_injection() {
        let t = Target::parse("admin@Mini-1.local:2222").unwrap();
        assert_eq!(
            (t.user.as_deref(), t.host.as_str(), t.port, t.name.as_str()),
            (Some("admin"), "Mini-1.local", Some(2222), "mini-1")
        );
        assert_eq!(Target::parse("10.0.0.5").unwrap().name, "10-0-0-5");
        for bad in [
            "-oProxyCommand=x",
            "a@-host",
            "ho st",
            "-x@h",
            "h:notaport",
            "",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn golden_ssh_command_line() {
        let mut t = Target::parse("admin@mini-1.local:2222").unwrap();
        t.identity = Some("/k/id".into());
        let o = SshOpts {
            known_hosts: Some("/k/known".into()),
            accept_new: false,
            connect_timeout_secs: 10,
            ..Default::default()
        };
        let line = display_command(&ssh_args(&t, &o, "/bin/sh -c 'echo hi'"));
        assert_eq!(
            line,
            "ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 \
             -o ServerAliveCountMax=3 -o UserKnownHostsFile=/k/known -i /k/id -o IdentitiesOnly=yes -p 2222 -- \
             admin@mini-1.local '/bin/sh -c '\\''echo hi'\\'''"
        );
    }

    #[test]
    fn host_keys_are_pinned_unless_asked() {
        let t = Target::parse("m").unwrap();
        let strict = ssh_args(&t, &SshOpts::default(), "true").join(" ");
        assert!(strict.contains("StrictHostKeyChecking=yes") && strict.contains("BatchMode=yes"));
        let loose = ssh_args(
            &t,
            &SshOpts {
                accept_new: true,
                ..Default::default()
            },
            "true",
        )
        .join(" ");
        assert!(loose.contains("StrictHostKeyChecking=accept-new"));
        assert!(!loose.contains("StrictHostKeyChecking=no"));
    }

    #[test]
    fn ssh_failures_come_with_a_fix() {
        let t = Target::parse("admin@m.local").unwrap();
        assert!(
            explain_ssh_failure(&t, "Host key verification failed.")
                .contains("--accept-new-hostkey")
        );
        assert!(
            explain_ssh_failure(&t, "Permission denied (publickey).")
                .contains("ssh-copy-id admin@m.local")
        );
        assert!(
            explain_ssh_failure(&t, "connect to host m.local port 22: Connection refused")
                .contains("Remote Login")
        );
        assert!(
            explain_ssh_failure(&t, "REMOTE HOST IDENTIFICATION HAS CHANGED!")
                .contains("ssh-keygen -R m.local")
        );
    }
}
