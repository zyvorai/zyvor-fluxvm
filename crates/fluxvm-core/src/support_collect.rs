// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Gathers what goes into a support bundle (see `support_bundle` for the redaction and the archive).
//!
//! The same [`build`] runs in the CLI (`fluxctl support-bundle`) and in the daemon (`POST /v1/support-bundle`); each
//! passes in what only it can see (the daemon's own VM list, or the answers an HTTP call got) as `Result`s, so a part
//! that is missing becomes a line in the manifest saying what was skipped and what to do, never a failure of the whole
//! bundle. Everything is read-only. Child processes (`journalctl`, `ip`, `df`, `uname`) get a deadline.

use crate::config::Config;
use crate::support_bundle::{Bundle, BundleError, last_lines, project_vm};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Lines kept from each log when the caller gives none.
pub const DEFAULT_LOG_LINES: usize = 1000;
/// The smallest and largest number of log lines anyone can ask for.
pub const MIN_LOG_LINES: usize = 10;
pub const MAX_LOG_LINES: usize = 10_000;
/// At most this many VM console logs go in, newest first, and each is cut to [`VM_LOG_LINES_MAX`] lines.
pub const MAX_VM_LOGS: usize = 20;
pub const VM_LOG_LINES_MAX: usize = 500;
/// A log file is read from its last bytes only.
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Keeps a requested line count inside the allowed range (`0` means "the default").
pub fn clamp_lines(n: usize) -> usize {
    if n == 0 {
        DEFAULT_LOG_LINES
    } else {
        n.clamp(MIN_LOG_LINES, MAX_LOG_LINES)
    }
}

/// Everything a bundle is made from. Anything that could not be had is an `Err` with a plain-English reason.
pub struct Inputs<'a> {
    pub cfg: &'a Config,
    /// `"fluxctl"` or `"daemon"`: who built this bundle.
    pub source: &'static str,
    pub fluxctl_version: String,
    /// The daemon's version, or why it is not known (usually: it is not running).
    pub daemon_version: Result<String, String>,
    /// The `fluxctl cluster status --json` document.
    pub health: Result<Value, String>,
    /// The daemon's VM records (full; only a safe projection is written).
    pub vms: Result<Vec<Value>, String>,
    /// The daemon's cluster list (`GET /v1/kube`).
    pub clusters: Result<Value, String>,
    pub log_lines: usize,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// `fluxvm-support-20261122-101500.tar.gz`.
pub fn default_file_name(now: chrono::DateTime<chrono::Utc>) -> String {
    format!("fluxvm-support-{}.tar.gz", now.format("%Y%m%d-%H%M%S"))
}

/// The folder name inside the archive for a file called `file_name`.
pub fn root_for(file_name: &str) -> String {
    let base = file_name
        .trim_end_matches(".gz")
        .trim_end_matches(".tar")
        .trim_end_matches(".tgz");
    let base = if base.is_empty() {
        "fluxvm-support"
    } else {
        base
    };
    crate::support_bundle::safe_entry_name(base)
}

/// Runs a program with a deadline and returns its stdout. `Err` says why in words.
pub fn run_bounded(program: &str, args: &[&str], secs: u64) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run `{program}`: {e}"))?;
    fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                let _ = r.take(16 * 1024 * 1024).read_to_end(&mut buf);
            }
            buf
        })
    }
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`{program}` did not finish within {secs}s and was stopped"
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("`{program}`: {e}")),
        }
    };
    let out = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if status.success() {
        Ok(out)
    } else {
        let e = err.trim();
        Err(format!(
            "`{program}` exited with {}{}",
            status
                .code()
                .map_or("a signal".to_owned(), |c| c.to_string()),
            if e.is_empty() {
                String::new()
            } else {
                format!(": {}", last_lines(e, 3).trim_end())
            }
        ))
    }
}

/// The last `n` lines of a file, reading only its last few MiB.
pub fn tail_file(path: &Path, n: usize) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    // When we started mid-file the first line is cut: drop it.
    let text = if start > 0 {
        text.split_once('\n').map_or("", |x| x.1)
    } else {
        &text
    };
    Ok(last_lines(text, n))
}

fn read_first(paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|p| std::fs::read_to_string(p).ok())
}

fn os_release_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|l| {
        l.strip_prefix(key)
            .and_then(|r| r.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_owned())
    })
}

/// `MemTotal`/`MemAvailable` in MiB from `/proc/meminfo` text.
pub fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let get = |k: &str| {
        text.lines().find_map(|l| {
            let r = l.strip_prefix(k)?.strip_prefix(':')?;
            r.split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|kb| kb / 1024)
        })
    };
    (get("MemTotal"), get("MemAvailable"))
}

fn versions_doc(i: &Inputs) -> Value {
    let os_release = read_first(&["/etc/os-release", "/usr/lib/os-release"]).unwrap_or_default();
    let uname = run_bounded("uname", &["-srm"], 5)
        .ok()
        .map(|s| s.trim().to_owned());
    let mac = if cfg!(target_os = "macos") {
        run_bounded("sw_vers", &["-productVersion"], 5)
            .ok()
            .map(|s| s.trim().to_owned())
    } else {
        None
    };
    let (daemon, daemon_note) = match &i.daemon_version {
        Ok(v) => (Some(v.clone()), None),
        Err(why) => (None, Some(why.clone())),
    };
    json!({
        "generated_by": i.source,
        "generated_at": i.now.to_rfc3339(),
        "fluxctl": i.fluxctl_version,
        "daemon": daemon,
        "daemon_note": daemon_note,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "os_pretty_name": os_release_field(&os_release, "PRETTY_NAME"),
        "macos_version": mac,
        "kernel": uname,
        "hostname": crate::health::hostname(),
        "listen": i.cfg.listen,
    })
}

fn host_facts(cfg: &Config) -> String {
    let mut s = String::new();
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());
    s.push_str(&format!("cpus: {cpus}\n"));
    match read_first(&["/proc/meminfo"]) {
        Some(m) => {
            let (t, a) = parse_meminfo(&m);
            s.push_str(&format!(
                "memory_total_mib: {}\nmemory_available_mib: {}\n",
                t.map_or("unknown".into(), |v| v.to_string()),
                a.map_or("unknown".into(), |v| v.to_string())
            ));
        }
        None => match run_bounded("sysctl", &["-n", "hw.memsize"], 5) {
            Ok(b) => {
                let mib = b.trim().parse::<u64>().map(|v| v / 1024 / 1024).ok();
                s.push_str(&format!(
                    "memory_total_mib: {}\n",
                    mib.map_or("unknown".into(), |v| v.to_string())
                ));
            }
            Err(e) => s.push_str(&format!("memory: not read ({e})\n")),
        },
    }
    s.push_str(&format!("state_dir: {}\n", cfg.state_dir.display()));
    s.push_str(&format!(
        "state_dir_free_gib: {}\n",
        crate::health::free_gib(&cfg.state_dir).map_or("unknown".into(), |v| v.to_string())
    ));
    match run_bounded("df", &["-h", &cfg.state_dir.to_string_lossy()], 8) {
        Ok(d) => s.push_str(&format!("\n$ df -h {}\n{d}", cfg.state_dir.display())),
        Err(e) => s.push_str(&format!("\ndf: not read ({e})\n")),
    }
    let net = run_bounded("ip", &["-brief", "address"], 8)
        .or_else(|_| run_bounded("ifconfig", &["-a"], 8))
        .or_else(|_| {
            // No `ip` or `ifconfig`: list the interfaces the kernel has.
            std::fs::read_dir("/sys/class/net")
                .map(|d| {
                    d.flatten()
                        .map(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            let st = std::fs::read_to_string(e.path().join("operstate"))
                                .unwrap_or_default();
                            format!("{n} {}\n", st.trim())
                        })
                        .collect::<String>()
                })
                .map_err(|e| {
                    format!("no `ip`, no `ifconfig` and /sys/class/net is unreadable: {e}")
                })
        });
    match net {
        Ok(n) => s.push_str(&format!("\nnetwork interfaces:\n{n}")),
        Err(e) => s.push_str(&format!("\nnetwork: not read ({e})\n")),
    }
    s
}

/// Files the daemon may write its log to, per platform.
fn daemon_log_candidates() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/var/log/fluxvm.log"),
        PathBuf::from("/opt/homebrew/var/log/fluxvm.log"),
        PathBuf::from("/usr/local/var/log/fluxvm.log"),
    ];
    if let Some(h) = std::env::var_os("HOME") {
        v.push(Path::new(&h).join("Library/Logs/fluxvm.log"));
    }
    v
}

fn daemon_log(n: usize) -> Result<String, String> {
    if cfg!(target_os = "linux")
        && let Ok(j) = run_bounded(
            "journalctl",
            &[
                "-u",
                "fluxvm",
                "-n",
                &n.to_string(),
                "--no-pager",
                "-o",
                "short-iso",
            ],
            15,
        )
        && !j.trim().is_empty()
        && !j.contains("No entries")
    {
        return Ok(j);
    }
    for p in daemon_log_candidates() {
        if let Ok(t) = tail_file(&p, n) {
            return Ok(format!("# from {}\n{t}", p.display()));
        }
    }
    Err("no daemon log found: `journalctl -u fluxvm` had nothing this user may read and none of /var/log/fluxvm.log, the Homebrew log or ~/Library/Logs/fluxvm.log exists. On Linux run `sudo journalctl -u fluxvm -n 500 > fluxvm.log` and attach that file.".to_owned())
}

/// Console logs of VMs: `(label, path)`, newest first.
fn vm_log_paths(i: &Inputs) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf, std::time::SystemTime)> = Vec::new();
    let mtime = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    };
    match &i.vms {
        Ok(vms) => {
            for vm in vms {
                let (Some(name), Some(p)) = (vm["name"].as_str(), vm["log_path"].as_str()) else {
                    continue;
                };
                let id = vm["id"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(8)
                    .collect::<String>();
                let p = PathBuf::from(p);
                let t = mtime(&p);
                v.push((format!("{name}-{id}"), p, t));
            }
        }
        Err(_) => {
            // The daemon is down: look for `console.log` files under the state directory, two levels deep.
            let mut stack = vec![(i.cfg.state_dir.clone(), 0u8)];
            while let Some((d, depth)) = stack.pop() {
                let Ok(rd) = std::fs::read_dir(&d) else {
                    continue;
                };
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() && depth < 2 {
                        stack.push((p, depth + 1));
                    } else if p.file_name().is_some_and(|n| n == "console.log") {
                        let label = p
                            .parent()
                            .and_then(|x| x.file_name())
                            .map_or("vm".to_owned(), |n| n.to_string_lossy().into_owned());
                        let t = mtime(&p);
                        v.push((label, p, t));
                    }
                }
            }
        }
    }
    v.retain(|(_, p, _)| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("console.log"))
    });
    v.sort_by_key(|a| std::cmp::Reverse(a.2));
    v.into_iter()
        .take(MAX_VM_LOGS)
        .map(|(l, p, _)| (l, p))
        .collect()
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// A finished bundle: the `.tar.gz` bytes and what was left out, with the reason for each.
pub struct Built {
    pub bytes: Vec<u8>,
    pub skipped: Vec<(String, String)>,
}

/// Builds the archive. Fails only if the final secret scan fails (nothing is returned then) or the archive cannot be made;
/// a source that could not be read is recorded as skipped.
pub fn build(i: &Inputs) -> Result<Built, BundleError> {
    let n = clamp_lines(i.log_lines);
    let mut b = Bundle::new();
    let daemon_down = "the FluxVM daemon is not answering, so its own data was not collected. Start it (`fluxctl fix`, or `fluxctl explain DAEMON_DOWN`) and run `fluxctl support-bundle` again for a fuller bundle";

    b.add_text("versions.json", &pretty(&versions_doc(i)));
    match &i.health {
        Ok(h) => b.add_text("cluster-health.json", &pretty(h)),
        Err(e) => b.skip("cluster-health.json", e),
    }
    match &i.vms {
        Ok(vms) => b.add_text(
            "instances.json",
            &pretty(&json!({"items": vms.iter().map(project_vm).collect::<Vec<_>>()})),
        ),
        Err(e) => b.skip("instances.json", &format!("{e}; {daemon_down}")),
    }
    match &i.clusters {
        Ok(c) => b.add_text("clusters.json", &pretty(c)),
        Err(e) => b.skip("clusters.json", &format!("{e}; {daemon_down}")),
    }
    match serde_json::to_value(i.cfg) {
        Ok(c) => b.add_text("config-effective.json", &pretty(&c)),
        Err(e) => b.skip(
            "config-effective.json",
            &format!("the configuration could not be written out: {e}"),
        ),
    }
    b.add_text("host.txt", &host_facts(i.cfg));
    match daemon_log(n) {
        Ok(t) => b.add_text("logs/daemon.log", &last_lines(&t, n)),
        Err(e) => b.skip("logs/daemon.log", &e),
    }
    let vm_n = n.min(VM_LOG_LINES_MAX);
    let paths = vm_log_paths(i);
    if paths.is_empty() {
        b.skip(
            "logs/vm-*.console.log",
            "no VM console logs were found (no VMs yet, or they live on another machine); nothing to do unless you expected one",
        );
    }
    for (label, p) in paths {
        let name = format!("logs/vm-{label}.console.log");
        match tail_file(&p, vm_n) {
            Ok(t) => b.add_text(&name, &t),
            Err(e) => b.skip(&name, &format!("{} could not be read: {e}. If the VM runs on another machine, run `fluxctl support-bundle` there", p.display())),
        }
    }
    let generated = i.now.to_rfc3339();
    let root = root_for(&default_file_name(i.now));
    let skipped = b.skipped().to_vec();
    let bytes = b.finish(
        &root,
        &generated,
        u64::try_from(i.now.timestamp()).unwrap_or(0),
    )?;
    Ok(Built { bytes, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(bytes: &[u8]) -> Vec<(String, String)> {
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        ar.entries()
            .unwrap()
            .map(|e| {
                let mut e = e.unwrap();
                let n = e.path().unwrap().to_string_lossy().into_owned();
                let mut s = String::new();
                e.read_to_string(&mut s).unwrap();
                (n, s)
            })
            .collect()
    }

    fn inputs(cfg: &Config) -> Inputs<'_> {
        Inputs {
            cfg,
            source: "fluxctl",
            fluxctl_version: "0.4.21".into(),
            daemon_version: Ok("0.4.21".into()),
            health: Ok(json!({"schema": 1, "light": "ok", "headline": "All good"})),
            vms: Ok(vec![]),
            clusters: Ok(json!({"items": []})),
            log_lines: 50,
            now: chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn line_counts_are_bounded() {
        assert_eq!(clamp_lines(0), DEFAULT_LOG_LINES);
        assert_eq!(clamp_lines(1), MIN_LOG_LINES);
        assert_eq!(clamp_lines(500), 500);
        assert_eq!(clamp_lines(usize::MAX), MAX_LOG_LINES);
    }

    #[test]
    fn the_file_name_has_a_timestamp_and_the_folder_matches() {
        let t = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let n = default_file_name(t);
        assert!(n.starts_with("fluxvm-support-") && n.ends_with(".tar.gz"));
        assert_eq!(root_for(&n), n.trim_end_matches(".tar.gz"));
        assert_eq!(root_for(""), "fluxvm-support");
    }

    #[test]
    fn meminfo_is_parsed() {
        let m = "MemTotal:       16384000 kB\nMemFree:  100 kB\nMemAvailable:    8192000 kB\n";
        assert_eq!(parse_meminfo(m), (Some(16000), Some(8000)));
        assert_eq!(parse_meminfo("nothing"), (None, None));
    }

    #[test]
    fn tail_file_keeps_only_the_last_lines_and_a_missing_file_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.log");
        std::fs::write(&p, "1\n2\n3\n4\n5\n").unwrap();
        assert_eq!(tail_file(&p, 2).unwrap(), "4\n5\n");
        assert!(tail_file(&d.path().join("nope"), 2).is_err());
    }

    #[test]
    fn tail_file_of_a_huge_file_reads_only_the_end() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("big.log");
        let mut s = String::new();
        for i in 0..400_000 {
            s.push_str(&format!("line number {i:08} padding padding padding\n"));
        }
        std::fs::write(&p, &s).unwrap();
        assert!(s.len() as u64 > TAIL_BYTES);
        let t = tail_file(&p, 3).unwrap();
        assert_eq!(t.lines().count(), 3);
        assert!(t.contains("line number 00399999"));
    }

    #[test]
    fn a_program_that_hangs_is_stopped() {
        let t = Instant::now();
        let r = run_bounded("sleep", &["30"], 1);
        assert!(r.unwrap_err().contains("did not finish"));
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_or_failing_program_is_a_plain_error() {
        assert!(
            run_bounded("definitely-not-a-program-xyz", &[], 2)
                .unwrap_err()
                .contains("could not run")
        );
        assert!(
            run_bounded("sh", &["-c", "echo boom >&2; exit 3"], 5)
                .unwrap_err()
                .contains("boom")
        );
        assert_eq!(
            run_bounded("sh", &["-c", "echo hi"], 5).unwrap().trim(),
            "hi"
        );
    }

    #[test]
    fn a_bundle_with_the_daemon_up_has_every_section() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("console.log");
        std::fs::write(&log, "boot ok\npassword=hunter2hunter2\nlogin: root\n").unwrap();
        let cfg = Config {
            state_dir: d.path().to_path_buf(),
            ..Config::default()
        };
        let mut i = inputs(&cfg);
        i.vms = Ok(vec![json!({
            "id": "0123456789abcdef", "name": "web", "status": "running", "log_path": log,
            "request": {"vcpus": 2, "memory_mib": 1024, "env": {"TOKEN": "zzzz"}}
        })]);
        let out = files(&build(&i).unwrap().bytes);
        let names: Vec<&str> = out.iter().map(|f| f.0.as_str()).collect();
        for want in [
            "versions.json",
            "cluster-health.json",
            "instances.json",
            "clusters.json",
            "config-effective.json",
            "host.txt",
            "logs/vm-web-01234567.console.log",
        ] {
            assert!(
                names.iter().any(|n| n.ends_with(want)),
                "missing {want}: {names:?}"
            );
        }
        let vm_log = &out.iter().find(|f| f.0.ends_with("console.log")).unwrap().1;
        assert!(vm_log.contains("boot ok") && !vm_log.contains("hunter2hunter2"));
        let inst = &out
            .iter()
            .find(|f| f.0.ends_with("instances.json"))
            .unwrap()
            .1;
        assert!(!inst.contains("zzzz") && inst.contains("\"vcpus\": 2"));
        let v: Value = serde_json::from_str(
            &out.iter()
                .find(|f| f.0.ends_with("versions.json"))
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(v["fluxctl"], "0.4.21");
        assert!(v["arch"].is_string() && v["os"].is_string());
    }

    #[test]
    fn a_bundle_with_the_daemon_down_still_works_and_says_what_it_skipped() {
        let d = tempfile::tempdir().unwrap();
        let vmdir = d.path().join("vms").join("abc");
        std::fs::create_dir_all(&vmdir).unwrap();
        std::fs::write(vmdir.join("console.log"), "kernel panic\n").unwrap();
        let cfg = Config {
            state_dir: d.path().to_path_buf(),
            ..Config::default()
        };
        let mut i = inputs(&cfg);
        i.daemon_version = Err("the daemon does not answer".into());
        i.vms = Err("could not reach the daemon at 127.0.0.1:7788".into());
        i.clusters = Err("could not reach the daemon at 127.0.0.1:7788".into());
        let out = files(&build(&i).unwrap().bytes);
        let manifest = &out
            .iter()
            .find(|f| f.0.ends_with("MANIFEST.txt"))
            .unwrap()
            .1;
        assert!(manifest.contains("instances.json: could not reach the daemon"));
        assert!(manifest.contains("fluxctl explain DAEMON_DOWN"));
        assert!(manifest.contains("clusters.json"));
        assert!(!out.iter().any(|f| f.0.ends_with("instances.json")));
        // The console log under the state dir is still found.
        assert!(
            out.iter()
                .any(|f| f.0.ends_with("logs/vm-abc.console.log") && f.1.contains("kernel panic"))
        );
        let v = &out
            .iter()
            .find(|f| f.0.ends_with("versions.json"))
            .unwrap()
            .1;
        assert!(v.contains("the daemon does not answer"));
    }

    #[test]
    fn the_effective_config_has_its_tokens_removed() {
        let mut cfg = Config::default();
        cfg.auth
            .tokens
            .push(fluxvm_test_token("tok_live_0123456789abcdef"));
        let out = files(&build(&inputs(&cfg)).unwrap().bytes);
        for (n, t) in &out {
            assert!(!t.contains("tok_live_0123456789abcdef"), "{n}");
        }
        let c = &out
            .iter()
            .find(|f| f.0.ends_with("config-effective.json"))
            .unwrap()
            .1;
        assert!(c.contains("[REDACTED]"));
    }

    fn fluxvm_test_token(t: &str) -> crate::config::ApiToken {
        crate::config::ApiToken {
            token: t.into(),
            role: crate::config::Role::Admin,
            name: None,
            tenant: None,
        }
    }

    #[test]
    fn a_secret_that_only_a_collector_could_leak_is_still_caught() {
        // A health document with a join token in a message, as a buggy check might print.
        let cfg = Config::default();
        let mut i = inputs(&cfg);
        i.health = Ok(
            json!({"headline": "join with K10a1b2c3d4e5f60718293a4b5c6d7e8f9012345678901234567890abcdef12345678::server:7f3a9b2c1d"}),
        );
        let out = files(&build(&i).unwrap().bytes);
        assert!(out.iter().all(|f| !f.1.contains("7f3a9b2c1d")));
    }

    #[test]
    fn only_console_logs_are_read_for_vms() {
        let d = tempfile::tempdir().unwrap();
        let secret_file = d.path().join("id_rsa");
        std::fs::write(
            &secret_file,
            "-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----",
        )
        .unwrap();
        let cfg = Config {
            state_dir: d.path().to_path_buf(),
            ..Config::default()
        };
        let mut i = inputs(&cfg);
        i.vms = Ok(vec![
            json!({"id": "1", "name": "x", "log_path": secret_file}),
        ]);
        assert!(vm_log_paths(&i).is_empty());
    }
}
