// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl self-test` and `POST /v1/self-test`: a quick end-to-end smoke check of this install.
//!
//! Each probe is real but cheap (a socket ping, a one-byte file write, a `launchctl print`) and reports PASS, WARN, FAIL
//! or SKIP (not applicable on this machine) with a plain-English "what to do". Failures carry a code `fluxctl explain`
//! knows (see [`crate::health::CODES`]). The CLI and the daemon share this module; they differ only in the
//! [`DaemonProbe`] they hand in, because the CLI has to ask the daemon over HTTP and the daemon already knows.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Version of the JSON document. Adding fields does not bump it.
pub const SCHEMA: u32 = 1;
const LOW_DISK_GIB: u64 = 20;
const CRITICAL_DISK_GIB: u64 = 5;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Not applicable here (for example the Mac probes on Linux).
    Skip,
    Pass,
    Warn,
    Fail,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Skip => "SKIP",
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Stable id: `daemon`, `docker_socket`, `vm_backend`, `cache_dir`, `disk_space`, `scheduler`, `supervisor`.
    pub id: String,
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// Plain English: the next thing to do. Present for WARN and FAIL.
    pub what_to_do: Option<String>,
    /// A code `fluxctl explain` knows.
    pub explain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub ts: u64,
    /// The worst status of any check (SKIP counts as nothing).
    pub status: Status,
    pub passed: u32,
    pub warned: u32,
    pub failed: u32,
    pub skipped: u32,
    pub headline: String,
    pub checks: Vec<Check>,
}

impl Report {
    /// Exit code: 0 unless something failed. Warnings do not fail the run.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.failed > 0)
    }
}

/// What the caller already knows about the daemon. The CLI asks it over HTTP; the daemon answers for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonProbe {
    /// `Ok(())` when the daemon answered its health endpoint, `Err(why)` when not.
    pub reachable: Result<(), String>,
    /// The scheduler API (`GET /v1/vms`): `Ok(number of VMs)` or `Err(why)`. `None` when the daemon is down.
    pub scheduler: Option<Result<usize, String>>,
}

#[derive(Debug, Clone)]
pub struct Inputs {
    pub state_dir: PathBuf,
    /// The Docker-compatible socket the daemon serves; `None` when it is switched off in the config.
    pub docker_sock: Option<PathBuf>,
    /// The macOS LaunchAgent label to look for (the supervisor).
    pub agent_label: String,
    pub daemon: DaemonProbe,
}

fn check(id: &str, name: &str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        id: id.into(),
        name: name.into(),
        status,
        detail: detail.into(),
        what_to_do: None,
        explain: None,
    }
}

fn with_fix(mut c: Check, code: &str, what: impl Into<String>) -> Check {
    c.explain = Some(code.into());
    c.what_to_do = Some(what.into());
    c
}

/// Run every probe. Each is bounded by a few seconds; none changes anything except a throw-away file that is removed.
pub fn run(inp: &Inputs, now: u64) -> Report {
    let checks = vec![
        daemon_check(&inp.daemon),
        scheduler_check(&inp.daemon),
        docker_check(inp.docker_sock.as_deref()),
        vm_backend_check(),
        cache_check(&inp.state_dir),
        disk_check(&inp.state_dir),
        supervisor_check(&inp.agent_label),
    ];
    summarize(checks, now)
}

pub fn summarize(checks: Vec<Check>, now: u64) -> Report {
    let n = |s: Status| checks.iter().filter(|c| c.status == s).count() as u32;
    let (passed, warned, failed, skipped) = (
        n(Status::Pass),
        n(Status::Warn),
        n(Status::Fail),
        n(Status::Skip),
    );
    let status = if failed > 0 {
        Status::Fail
    } else if warned > 0 {
        Status::Warn
    } else {
        Status::Pass
    };
    let headline = if failed > 0 {
        format!(
            "{failed} check{} failed: see \"what to do\" below.",
            if failed == 1 { "" } else { "s" }
        )
    } else if warned > 0 {
        format!(
            "Works, with {warned} warning{}.",
            if warned == 1 { "" } else { "s" }
        )
    } else {
        format!("All {passed} checks passed.")
    };
    Report {
        schema: SCHEMA,
        ts: now,
        status,
        passed,
        warned,
        failed,
        skipped,
        headline,
        checks,
    }
}

// ---- probes -----------------------------------------------------------------------------------------------------------

pub fn daemon_check(d: &DaemonProbe) -> Check {
    match &d.reachable {
        Ok(()) => check(
            "daemon",
            "FluxVM daemon",
            Status::Pass,
            "answers its health check",
        ),
        Err(why) => with_fix(
            check(
                "daemon",
                "FluxVM daemon",
                Status::Fail,
                format!("not answering ({why})"),
            ),
            "DAEMON_DOWN",
            "Start it with `fluxctl fix` (macOS) or `fluxctl serve`; on Linux `sudo systemctl start fluxvm`.",
        ),
    }
}

pub fn scheduler_check(d: &DaemonProbe) -> Check {
    match &d.scheduler {
        Some(Ok(n)) => check(
            "scheduler",
            "Scheduler API",
            Status::Pass,
            format!("lists {n} VM{}", if *n == 1 { "" } else { "s" }),
        ),
        Some(Err(why)) => with_fix(
            check(
                "scheduler",
                "Scheduler API",
                Status::Fail,
                format!("the daemon is up but the VM list failed ({why})"),
            ),
            "SCHEDULER_NOT_ANSWERING",
            "Check the daemon log (`fluxctl support-bundle` collects it), then restart the daemon. If it says unauthorized, pass --server-token.",
        ),
        None => check(
            "scheduler",
            "Scheduler API",
            Status::Skip,
            "not checked because the daemon is down",
        ),
    }
}

pub fn docker_check(sock: Option<&Path>) -> Check {
    let name = "Docker socket";
    let Some(p) = sock else {
        return check(
            "docker_socket",
            name,
            Status::Skip,
            "the Docker socket is switched off in the config",
        );
    };
    if !p.exists() {
        return with_fix(
            check(
                "docker_socket",
                name,
                Status::Warn,
                format!("{} does not exist", p.display()),
            ),
            "DOCKER_SOCKET_DOWN",
            "Start the daemon (the socket appears when it runs). If you turned it off on purpose, ignore this.",
        );
    }
    match docker_probe(p) {
        Ok(v) => check(
            "docker_socket",
            name,
            Status::Pass,
            format!("/_ping is OK, API version {v}"),
        ),
        Err(why) => with_fix(
            check(
                "docker_socket",
                name,
                Status::Fail,
                format!("{} did not answer: {why}", p.display()),
            ),
            "DOCKER_SOCKET_DOWN",
            "The socket file is stale or the daemon is stuck: restart the daemon (`fluxctl fix`), then run this again.",
        ),
    }
}

fn http_get(sock: &Path, path: &str) -> Result<(u16, String), String> {
    let mut s =
        std::os::unix::net::UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(SOCKET_TIMEOUT)).ok();
    s.set_write_timeout(Some(SOCKET_TIMEOUT)).ok();
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: docker\r\n\r\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::new();
    // A timeout after some bytes is still an answer; only an empty reply is an error.
    let _ = s.take(1 << 20).read_to_end(&mut buf);
    parse_http(&String::from_utf8_lossy(&buf))
}

/// Status code and body of an HTTP/1.x response.
pub fn parse_http(text: &str) -> Result<(u16, String), String> {
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text, ""));
    let code = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "no HTTP answer".to_owned())?;
    Ok((code, body.to_owned()))
}

/// `/_ping` must say OK and `/version` must answer; returns the API version.
fn docker_probe(sock: &Path) -> Result<String, String> {
    let (code, body) = http_get(sock, "/_ping")?;
    if code != 200 || body.trim() != "OK" {
        return Err(format!("/_ping answered {code}"));
    }
    let (code, body) = http_get(sock, "/version")?;
    if code != 200 {
        return Err(format!("/version answered {code}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| "/version is not JSON".to_owned())?;
    Ok(v["ApiVersion"].as_str().unwrap_or("unknown").to_owned())
}

pub fn vm_backend_check() -> Check {
    let name = "VM backend";
    if cfg!(target_os = "macos") {
        if !cfg!(target_arch = "aarch64") {
            return with_fix(
                check(
                    "vm_backend",
                    name,
                    Status::Fail,
                    "this Mac is not Apple silicon",
                ),
                "UNSUPPORTED_HOST",
                "macOS virtual machines need an Apple silicon Mac (M1 or newer). Use a different machine.",
            );
        }
        let out = std::process::Command::new("sysctl")
            .args(["-n", "kern.hv_support"])
            .output();
        return match out {
            Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "1" => check(
                "vm_backend",
                name,
                Status::Pass,
                "Apple Virtualization is available",
            ),
            Ok(_) => with_fix(
                check(
                    "vm_backend",
                    name,
                    Status::Fail,
                    "the hypervisor is not available",
                ),
                "UNSUPPORTED_HOST",
                "This Mac cannot run virtual machines (it may itself be a VM without nested virtualization). Use a physical Mac.",
            ),
            Err(e) => check(
                "vm_backend",
                name,
                Status::Warn,
                format!("could not ask the system ({e})"),
            ),
        };
    }
    if cfg!(target_os = "linux") {
        return match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
        {
            Ok(_) => check("vm_backend", name, Status::Pass, "/dev/kvm is usable"),
            Err(e) => with_fix(
                check("vm_backend", name, Status::Fail, format!("/dev/kvm: {e}")),
                "NO_KVM",
                "Turn on virtualization in the BIOS and add yourself to the kvm group (`sudo usermod -aG kvm $USER`, then log in again).",
            ),
        };
    }
    check(
        "vm_backend",
        name,
        Status::Skip,
        "no VM backend on this operating system",
    )
}

/// The state folder (and the image cache inside it) must accept a write.
pub fn cache_check(state_dir: &Path) -> Check {
    let name = "Image and registry cache";
    if !state_dir.exists() {
        return with_fix(
            check(
                "cache_dir",
                name,
                Status::Fail,
                format!("{} does not exist", state_dir.display()),
            ),
            "STORE_PROBLEM",
            format!(
                "Create the folder with `mkdir -p {0}` (use sudo and then `sudo chown $USER {0}` if it is under /var), or on a Mac run `fluxctl fix`.",
                state_dir.display()
            ),
        );
    }
    let dir = ["images", "registry", "cache"]
        .iter()
        .map(|d| state_dir.join(d))
        .find(|p| p.is_dir())
        .unwrap_or_else(|| state_dir.to_owned());
    let probe = dir.join(format!(".self-test-{}", std::process::id()));
    match std::fs::write(&probe, b"x") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            check(
                "cache_dir",
                name,
                Status::Pass,
                format!("{} is writable", dir.display()),
            )
        }
        Err(e) => with_fix(
            check(
                "cache_dir",
                name,
                Status::Fail,
                format!("cannot write to {}: {e}", dir.display()),
            ),
            "STORE_PROBLEM",
            format!(
                "Fix the owner or permissions of {} (for example `sudo chown -R $USER {}`).",
                dir.display(),
                dir.display()
            ),
        ),
    }
}

pub fn disk_check(state_dir: &Path) -> Check {
    disk_judge(crate::health::free_gib(state_dir))
}

pub fn disk_judge(free: Option<u64>) -> Check {
    let name = "Disk space";
    match free {
        None => check(
            "disk_space",
            name,
            Status::Warn,
            "could not read the free space",
        ),
        Some(g) if g < CRITICAL_DISK_GIB => with_fix(
            check(
                "disk_space",
                name,
                Status::Fail,
                format!("only {g} GiB free"),
            ),
            "LOW_DISK",
            "Free space now: delete stopped VMs and old images (`fluxctl list`, `fluxctl delete ID`), or move the state folder to a bigger disk.",
        ),
        Some(g) if g < LOW_DISK_GIB => with_fix(
            check("disk_space", name, Status::Warn, format!("{g} GiB free")),
            "LOW_DISK",
            "Getting low: remove VMs and images you no longer use before creating new ones.",
        ),
        Some(g) => check("disk_space", name, Status::Pass, format!("{g} GiB free")),
    }
}

/// The macOS LaunchAgent that keeps the daemon running.
pub fn supervisor_check(label: &str) -> Check {
    let name = "Supervisor (LaunchAgent)";
    if !cfg!(target_os = "macos") {
        return check(
            "supervisor",
            name,
            Status::Skip,
            "the LaunchAgent is a macOS feature",
        );
    }
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let out = std::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .output();
    let (loaded, text) = match out {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
        ),
        Err(e) => {
            return check(
                "supervisor",
                name,
                Status::Warn,
                format!("could not run launchctl ({e})"),
            );
        }
    };
    let plist = std::env::var_os("HOME")
        .map(|h| {
            PathBuf::from(h)
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"))
        })
        .is_some_and(|p| p.exists());
    supervisor_judge(loaded, &text, plist)
}

/// Pure part of [`supervisor_check`]: `loaded` is whether `launchctl print` succeeded, `text` its output.
pub fn supervisor_judge(loaded: bool, text: &str, plist_exists: bool) -> Check {
    let name = "Supervisor (LaunchAgent)";
    if !loaded {
        return if plist_exists {
            with_fix(
                check("supervisor", name, Status::Fail, "installed but not loaded"),
                "AGENT_NOT_LOADED",
                "Run `fluxctl fix`, which loads it.",
            )
        } else {
            with_fix(
                check(
                    "supervisor",
                    name,
                    Status::Warn,
                    "not installed, so the daemon will not restart after a reboot or crash",
                ),
                "AGENT_NOT_LOADED",
                "Run `fluxctl service install` to keep the daemon running.",
            )
        };
    }
    let state = text
        .lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("state =")
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into());
    if state == "running" {
        check("supervisor", name, Status::Pass, "loaded and running")
    } else {
        with_fix(
            check(
                "supervisor",
                name,
                Status::Warn,
                format!("loaded but {state}"),
            ),
            "AGENT_NOT_LOADED",
            "Run `fluxctl fix` to start it; if it keeps stopping, read the log with `fluxctl service status`.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dp(ok: bool) -> DaemonProbe {
        DaemonProbe {
            reachable: if ok { Ok(()) } else { Err("refused".into()) },
            scheduler: if ok { Some(Ok(2)) } else { None },
        }
    }

    fn serve(dir: &Path, ping: &'static str, version: &'static str) -> PathBuf {
        let p = dir.join("docker.sock");
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut buf = [0u8; 512];
                let mut n = 0;
                while !buf[..n].windows(4).any(|w| w == b"\r\n\r\n") && n < buf.len() {
                    match s.read(&mut buf[n..]) {
                        Ok(0) | Err(_) => break,
                        Ok(k) => n += k,
                    }
                }
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let (code, body) = if req.starts_with("GET /_ping") {
                    (ping, "OK")
                } else {
                    (version, "{\"ApiVersion\":\"1.43\"}")
                };
                let _ = write!(
                    s,
                    "HTTP/1.0 {code}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        p
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fluxvm-selftest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn http_parsing() {
        assert_eq!(
            parse_http("HTTP/1.1 200 OK\r\nA: b\r\n\r\nOK").unwrap(),
            (200, "OK".into())
        );
        assert!(parse_http("").is_err());
    }

    #[test]
    fn docker_socket_pass_fail_warn_skip() {
        let d = tmp("docker");
        let ok = serve(&d, "200 OK", "200 OK");
        let c = docker_check(Some(&ok));
        assert_eq!(c.status, Status::Pass, "{c:?}");
        assert!(c.detail.contains("1.43"));
        let d2 = tmp("docker2");
        let bad = serve(&d2, "500 Internal Server Error", "200 OK");
        let c = docker_check(Some(&bad));
        assert_eq!(
            (c.status, c.explain.as_deref()),
            (Status::Fail, Some("DOCKER_SOCKET_DOWN"))
        );
        let c = docker_check(Some(&d.join("nope.sock")));
        assert_eq!(c.status, Status::Warn);
        assert!(c.what_to_do.is_some());
        assert_eq!(docker_check(None).status, Status::Skip);
        // a regular file that is not a socket is a failure, not a hang
        let f = d.join("file.sock");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(docker_check(Some(&f)).status, Status::Fail);
    }

    #[test]
    fn cache_dir_writable_missing_and_readonly() {
        let d = tmp("cache");
        assert_eq!(cache_check(&d).status, Status::Pass);
        assert!(
            std::fs::read_dir(&d).unwrap().next().is_none(),
            "the probe file is removed"
        );
        let c = cache_check(&d.join("missing"));
        assert_eq!(
            (c.status, c.explain.as_deref()),
            (Status::Fail, Some("STORE_PROBLEM"))
        );
        // root ignores permission bits, so only check when the write really is refused
        use std::os::unix::fs::PermissionsExt;
        let ro = tmp("ro");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(ro.join("x"), b"x").is_err() {
            assert_eq!(cache_check(&ro).status, Status::Fail);
        }
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn disk_thresholds() {
        assert_eq!(disk_judge(Some(2)).status, Status::Fail);
        assert_eq!(disk_judge(Some(10)).status, Status::Warn);
        assert_eq!(disk_judge(Some(100)).status, Status::Pass);
        assert_eq!(disk_judge(None).status, Status::Warn);
    }

    #[test]
    fn supervisor_states() {
        let run = "dev.zyvor.fluxvm = {\n\tstate = running\n\tpid = 4\n}";
        assert_eq!(supervisor_judge(true, run, true).status, Status::Pass);
        assert_eq!(
            supervisor_judge(true, "\tstate = waiting", true).status,
            Status::Warn
        );
        assert_eq!(supervisor_judge(false, "", true).status, Status::Fail);
        let c = supervisor_judge(false, "", false);
        assert_eq!(c.status, Status::Warn);
        assert!(c.what_to_do.unwrap().contains("service install"));
    }

    #[test]
    fn daemon_and_scheduler() {
        assert_eq!(daemon_check(&dp(true)).status, Status::Pass);
        assert_eq!(scheduler_check(&dp(true)).status, Status::Pass);
        assert_eq!(daemon_check(&dp(false)).status, Status::Fail);
        assert_eq!(scheduler_check(&dp(false)).status, Status::Skip);
        let bad = DaemonProbe {
            reachable: Ok(()),
            scheduler: Some(Err("401".into())),
        };
        assert_eq!(
            scheduler_check(&bad).explain.as_deref(),
            Some("SCHEDULER_NOT_ANSWERING")
        );
    }

    #[test]
    fn every_warning_and_failure_says_what_to_do_and_explains() {
        let d = tmp("all");
        let inp = Inputs {
            state_dir: d.join("missing"),
            docker_sock: Some(d.join("nope.sock")),
            agent_label: "dev.zyvor.fluxvm.selftest-none".into(),
            daemon: dp(false),
        };
        let r = run(&inp, 5);
        assert_eq!(r.checks.len(), 7);
        assert_eq!(r.status, Status::Fail);
        assert_eq!(r.exit_code(), 1);
        for c in &r.checks {
            if matches!(c.status, Status::Warn | Status::Fail) {
                assert!(
                    c.what_to_do.as_deref().is_some_and(|w| !w.is_empty()),
                    "{c:?}"
                );
                let code = c.explain.as_deref().unwrap();
                assert!(
                    crate::health::code(code).is_some(),
                    "{code} is not in CODES"
                );
            }
        }
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["schema"], 1);
        assert_eq!(j["checks"][0]["status"], "fail");
        let back: Report = serde_json::from_value(j).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn summary_and_exit_code() {
        let ok = summarize(
            vec![
                check("a", "A", Status::Pass, ""),
                check("b", "B", Status::Skip, ""),
            ],
            1,
        );
        assert_eq!(
            (ok.status, ok.exit_code(), ok.skipped),
            (Status::Pass, 0, 1)
        );
        let warn = summarize(vec![check("a", "A", Status::Warn, "")], 1);
        assert_eq!((warn.status, warn.exit_code()), (Status::Warn, 0));
        assert_eq!(
            summarize(vec![check("a", "A", Status::Fail, "")], 1).exit_code(),
            1
        );
    }
}
