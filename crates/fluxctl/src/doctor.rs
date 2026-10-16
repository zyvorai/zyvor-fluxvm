// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl doctor`: a read-only "is this host ready?" report.
//!
//! The checks are split in two. The `check_*` functions are pure: they take what was observed
//! (an HTTP status, whether a file exists, free bytes) and return a [`Check`], so each rule is
//! unit-tested without a daemon or a particular OS. [`gather`] is the thin layer that probes the
//! real host and feeds them. Every failing or warning check carries the command that fixes it.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Skip,
    Warn,
    Fail,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Skip => "skip",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    /// The command or step that fixes it; present on `Warn` and `Fail`.
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Ok,
            detail: detail.into(),
            fix: None,
        }
    }
    fn skip(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Skip,
            detail: detail.into(),
            fix: None,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            level: Level::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Exit code for a finished report: 0 all good (warnings allowed), 1 at least one failure.
pub fn exit_code(checks: &[Check]) -> i32 {
    if checks.iter().any(|c| c.level == Level::Fail) {
        1
    } else {
        0
    }
}

pub fn to_json(checks: &[Check]) -> Value {
    let worst = checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok);
    json!({
        "ok": worst < Level::Fail,
        "checks": checks.iter().map(|c| json!({
            "name": c.name,
            "level": c.level.label(),
            "detail": c.detail,
            "fix": c.fix,
        })).collect::<Vec<_>>(),
    })
}

pub fn render(checks: &[Check]) -> String {
    let mut out = String::new();
    for c in checks {
        out.push_str(&format!(
            "{:<5} {:<22} {}\n",
            c.level.label(),
            c.name,
            c.detail
        ));
        if let Some(fix) = &c.fix {
            out.push_str(&format!("      {:<22} fix: {fix}\n", ""));
        }
    }
    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    out.push_str(&format!("\n{fails} failed, {warns} warning(s)\n"));
    out
}

// ---- pure checks ---------------------------------------------------------------------------

/// `healthz` / `readyz` results: an HTTP status, or the connection error text.
pub fn check_daemon(
    listen: &str,
    healthz: &Result<u16, String>,
    readyz: &Result<u16, String>,
) -> Check {
    match healthz {
        Err(e) => Check::fail(
            "daemon",
            format!("nothing answers on {listen} ({e})"),
            "fluxctl serve   (or: brew services start zyvorai/tap/fluxvm, fluxctl service install)",
        ),
        Ok(code) if *code != 200 => Check::fail(
            "daemon",
            format!("GET /healthz on {listen} returned {code}"),
            "check the daemon log: ~/Library/Logs/fluxvm.log (macOS) or journalctl -u fluxvm",
        ),
        Ok(_) => match readyz {
            Ok(200) => Check::ok("daemon", format!("{listen}: healthy and ready")),
            Ok(code) => Check::warn(
                "daemon",
                format!("{listen} is alive but /readyz returned {code}"),
                "0.4.4 answered 503 on a Mac: upgrade the package; otherwise read the daemon log",
            ),
            Err(e) => Check::warn(
                "daemon",
                format!("{listen} answered /healthz but /readyz failed ({e})"),
                "retry; if it persists read the daemon log",
            ),
        },
    }
}

/// `present` pairs a binary name with whether it was found.
pub fn check_binaries(
    required: &[(&'static str, bool)],
    optional: &[(&'static str, bool)],
) -> Check {
    let missing: Vec<&str> = required
        .iter()
        .filter(|(_, p)| !p)
        .map(|(n, _)| *n)
        .collect();
    if !missing.is_empty() {
        return Check::fail(
            "binaries",
            format!("not on PATH: {}", missing.join(", ")),
            "install the package (brew install zyvorai/tap/fluxvm, or the install.sh one-liner) and open a new shell",
        );
    }
    let opt: Vec<&str> = optional
        .iter()
        .filter(|(_, p)| !p)
        .map(|(n, _)| *n)
        .collect();
    if opt.is_empty() {
        Check::ok("binaries", "all found")
    } else {
        Check::ok(
            "binaries",
            format!("required found; optional not installed: {}", opt.join(", ")),
        )
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

pub fn check_disk(path: &Path, free_bytes: Option<u64>) -> Check {
    match free_bytes {
        None => Check::skip(
            "disk",
            format!("could not read free space under {}", path.display()),
        ),
        Some(b) if b < GIB => Check::fail(
            "disk",
            format!("{} free under {}", human(b), path.display()),
            "free space, or point state_dir at a larger volume (--state-dir / [state_dir] in fluxvm.toml)",
        ),
        Some(b) if b < 10 * GIB => Check::warn(
            "disk",
            format!(
                "{} free under {}; a Linux image alone is a few GB",
                human(b),
                path.display()
            ),
            "free space, or move state_dir to a larger volume",
        ),
        Some(b) => Check::ok(
            "disk",
            format!("{} free under {}", human(b), path.display()),
        ),
    }
}

fn human(b: u64) -> String {
    format!("{:.1} GiB", b as f64 / GIB as f64)
}

/// Linux: `/dev/kvm` must exist and be usable. `daemon_up` is whether the daemon answered: it usually runs
/// as another user (root), so a `/dev/kvm` this user cannot open is only a warning while it is healthy: the VMs
/// do not run as this user. `None` = the daemon was not checked (`--host-only`): judge `/dev/kvm` alone, as a warning,
/// because a packaged daemon runs as root.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn check_kvm(exists: bool, usable: bool, daemon_up: Option<bool>) -> Check {
    match (exists, usable) {
        (true, true) => Check::ok("kvm", "/dev/kvm is present and usable"),
        (true, false) if daemon_up.is_none() => Check::warn(
            "kvm",
            "/dev/kvm exists but this user cannot open it; the daemon normally runs as root and does not need this",
            "only needed to run VMs as this user: sudo usermod -aG kvm \"$USER\"   then log in again",
        ),
        (true, false) if daemon_up == Some(true) => Check::warn(
            "kvm",
            "/dev/kvm exists but this user cannot open it; the daemon is running, so it runs as another user",
            "only needed to run VMs as this user: sudo usermod -aG kvm \"$USER\"   then log in again",
        ),
        (true, false) => Check::fail(
            "kvm",
            "/dev/kvm exists but this user cannot open it",
            "sudo usermod -aG kvm \"$USER\"   then log in again (or run the daemon as root)",
        ),
        (false, _) => Check::fail(
            "kvm",
            "/dev/kvm is missing",
            "enable virtualization in firmware / the hypervisor, then: sudo modprobe kvm_intel   (or kvm_amd)",
        ),
    }
}

/// macOS: does the runner carry `com.apple.security.virtualization`? `None` = could not tell.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn check_runner_entitlement(runner: Option<&Path>, entitled: Option<bool>) -> Check {
    match (runner, entitled) {
        (None, _) => Check::fail(
            "vz runner",
            "fluxvm-vz-runner was not found next to fluxctl or on PATH",
            "reinstall the package: brew reinstall zyvorai/tap/fluxvm",
        ),
        (Some(p), Some(true)) => Check::ok(
            "vz runner",
            format!("{} has the virtualization entitlement", p.display()),
        ),
        (Some(p), Some(false)) => Check::fail(
            "vz runner",
            format!("{} lacks com.apple.security.virtualization", p.display()),
            "use the packaged build (scripts/package-macos.sh re-signs it); an unsigned runner cannot start VMs",
        ),
        (Some(p), None) => Check::skip(
            "vz runner",
            format!(
                "found {}; could not read its entitlements (codesign)",
                p.display()
            ),
        ),
    }
}

/// One entry from `mcp_install::status` / the LaunchAgent: what points at a path, and whether it exists.
pub struct PathRef {
    pub what: String,
    pub path: PathBuf,
    pub exists: bool,
}

/// A client config or LaunchAgent that names a binary that is gone, typically a Homebrew
/// `Cellar/fluxvm/<old version>/bin/fluxctl` left behind by `brew upgrade`.
pub fn check_stale_paths(refs: &[PathRef]) -> Check {
    let dead: Vec<&PathRef> = refs.iter().filter(|r| !r.exists).collect();
    if refs.is_empty() {
        return Check::skip("client entries", "no MCP client or LaunchAgent entry found");
    }
    if dead.is_empty() {
        return Check::ok(
            "client entries",
            format!(
                "{} entr{} point at existing binaries",
                refs.len(),
                if refs.len() == 1 { "y" } else { "ies" }
            ),
        );
    }
    let list = dead
        .iter()
        .map(|r| format!("{} -> {}", r.what, r.path.display()))
        .collect::<Vec<_>>()
        .join("; ");
    Check::warn(
        "client entries",
        format!("point at a binary that no longer exists: {list}"),
        "`fluxctl doctor --fix` repoints MCP entries (keeping their settings); or re-run `fluxctl mcp install <client>`; for the LaunchAgent, `fluxctl service install`",
    )
}

/// Headroom for a Mac: leave the larger of 3 GiB or a quarter for macOS itself (the same rule the
/// placement code uses). Warn when the machine is small enough that a default VM will not fit.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn check_memory(total_mib: Option<u64>) -> Check {
    let Some(total) = total_mib else {
        return Check::skip("memory", "could not read total memory");
    };
    let reserve = (total / 4).max(3 * 1024);
    let avail = total.saturating_sub(reserve);
    if avail < 2048 {
        Check::warn(
            "memory",
            format!("{total} MiB total leaves about {avail} MiB for VMs after macOS headroom"),
            "close other apps or use a Mac with more memory; give VMs less than the figure above",
        )
    } else {
        Check::ok(
            "memory",
            format!("{total} MiB total, about {avail} MiB available to VMs after headroom"),
        )
    }
}

/// What the console login session looks like to `ioreg`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session {
    Unlocked,
    Locked,
    /// Nobody is logged in at the console.
    NoSession,
    /// `ioreg` gave nothing usable.
    Unknown,
}

/// Parse the `IOConsoleUsers` line of `ioreg -n Root -d1`. `IOConsoleLocked` is not used: it can
/// say `No` while the user's session reports `CGSSessionScreenIsLocked=Yes`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_console_session(ioreg: &str) -> Session {
    if ioreg.trim().is_empty() {
        return Session::Unknown;
    }
    let Some(line) = ioreg.lines().find(|l| l.contains("\"IOConsoleUsers\"")) else {
        return Session::NoSession;
    };
    if !line.contains('{') {
        return Session::NoSession;
    }
    if line.contains("\"CGSSessionScreenIsLocked\"=Yes") {
        Session::Locked
    } else {
        Session::Unlocked
    }
}

/// Snapshot restore and warm slots need a logged-in, unlocked session (`docs/macos-sandboxes.md`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn check_session(s: Session) -> Check {
    const FIX: &str = "unlock the Mac, or give it a dedicated auto-login user with the screen lock off (docs/agent-density.md#unattended-hosts)";
    match s {
        Session::Unlocked => Check::ok(
            "login session",
            "a user is logged in and the screen is unlocked",
        ),
        Session::Locked => Check::warn(
            "login session",
            "the screen is locked: snapshot restore and warm-pool slots will fail until it is unlocked",
            FIX,
        ),
        Session::NoSession => Check::warn(
            "login session",
            "nobody is logged in at the console: snapshot restore and warm-pool slots will fail",
            FIX,
        ),
        Session::Unknown => Check::skip(
            "login session",
            "could not read the console session (ioreg)",
        ),
    }
}

// ---- host probing --------------------------------------------------------------------------

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Free bytes on the filesystem holding `path` (its nearest existing ancestor).
fn free_bytes(path: &Path) -> Option<u64> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `s` is a properly sized, writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

async fn probe(listen: &str, path: &str) -> Result<u16, String> {
    let base = if listen.starts_with("http://") || listen.starts_with("https://") {
        listen.to_owned()
    } else {
        format!("http://{listen}")
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    client
        .get(format!("{}{}", base.trim_end_matches('/'), path))
        .send()
        .await
        .map(|r| r.status().as_u16())
        .map_err(|e| {
            if e.is_connect() {
                "connection refused".to_owned()
            } else if e.is_timeout() {
                "timed out".to_owned()
            } else {
                e.to_string()
            }
        })
}

#[cfg(target_os = "macos")]
fn runner_entitled(runner: &Path) -> Option<bool> {
    let out = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", "-"])
        .arg(runner)
        .output()
        .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Some(text.contains("com.apple.security.virtualization"))
}

#[cfg(target_os = "macos")]
fn console_session() -> Session {
    match std::process::Command::new("ioreg")
        .args(["-n", "Root", "-d1"])
        .output()
    {
        Ok(o) if o.status.success() => parse_console_session(&String::from_utf8_lossy(&o.stdout)),
        _ => Session::Unknown,
    }
}

#[cfg(target_os = "macos")]
fn total_memory_mib() -> Option<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
        .map(|b| b / (1024 * 1024))
}

/// Pull the executable path out of a LaunchAgent plist's first `<string>` after `ProgramArguments`.
fn plist_program(text: &str) -> Option<String> {
    let after = text.split("<key>ProgramArguments</key>").nth(1)?;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    Some(after[start..end].to_owned())
}

fn client_refs() -> Vec<PathRef> {
    let mut refs = Vec::new();
    if let Ok(Value::Array(rows)) = crate::mcp_install::status(false, "fluxvm") {
        for r in rows {
            if r["installed"].as_bool() != Some(true) {
                continue;
            }
            let Some(cmd) = r["command"].as_str() else {
                continue;
            };
            let bin = cmd.split_whitespace().next().unwrap_or(cmd);
            refs.push(PathRef {
                what: format!("MCP {}", r["client"].as_str().unwrap_or("client")),
                path: PathBuf::from(bin),
                exists: Path::new(bin).exists(),
            });
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let plist = crate::launch_agent::plist_path(&home);
        if let Ok(text) = std::fs::read_to_string(&plist) {
            if let Some(bin) = plist_program(&text) {
                refs.push(PathRef {
                    what: "LaunchAgent".into(),
                    path: PathBuf::from(&bin),
                    exists: Path::new(&bin).exists(),
                });
            }
        }
    }
    refs
}

/// Probe this host and return the report. `host_only` skips the daemon check, for a machine FluxVM is not installed on
/// yet: there is no daemon to find, and `/dev/kvm` is judged on its own (the daemon will run as root).
pub async fn gather(listen: &str, state_dir: &Path, host_only: bool) -> Vec<Check> {
    let mut out = Vec::new();

    let healthz = if host_only {
        Ok(200)
    } else {
        probe(listen, "/healthz").await
    };
    if !host_only {
        let readyz = probe(listen, "/readyz").await;
        out.push(check_daemon(listen, &healthz, &readyz));
    }

    #[cfg(target_os = "macos")]
    let (required, optional): (&[&'static str], &[&'static str]) = (
        &["fluxctl", "fluxvm-vz-runner"],
        &["fluxvm-vz-switch", "fluxvm-blockd", "fluxvm-usernet"],
    );
    #[cfg(not(target_os = "macos"))]
    let (required, optional): (&[&'static str], &[&'static str]) = (
        &["fluxctl", "qemu-system-x86_64", "qemu-img", "ip"],
        &[
            "cloud-localds",
            "dnsmasq",
            "cloud-hypervisor",
            "firecracker",
            "bpftool",
        ],
    );
    let req: Vec<_> = required.iter().map(|n| (*n, which(n).is_some())).collect();
    let opt: Vec<_> = optional.iter().map(|n| (*n, which(n).is_some())).collect();
    out.push(check_binaries(&req, &opt));

    #[cfg(target_os = "macos")]
    {
        let runner = which("fluxvm-vz-runner");
        let entitled = runner.as_deref().and_then(runner_entitled);
        out.push(check_runner_entitlement(runner.as_deref(), entitled));
        out.push(check_memory(total_memory_mib()));
        out.push(check_session(console_session()));
    }
    #[cfg(target_os = "linux")]
    {
        let exists = Path::new("/dev/kvm").exists();
        let usable = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok();
        out.push(check_kvm(
            exists,
            usable,
            (!host_only).then_some(matches!(healthz, Ok(200))),
        ));
    }

    out.push(check_disk(state_dir, free_bytes(state_dir)));
    out.push(check_stale_paths(&client_refs()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(c: &Check) -> Level {
        c.level
    }

    #[test]
    fn daemon_down_fails_with_a_start_command() {
        let c = check_daemon(
            "127.0.0.1:7788",
            &Err("connection refused".into()),
            &Err("x".into()),
        );
        assert_eq!(level(&c), Level::Fail);
        assert!(c.fix.unwrap().contains("fluxctl serve"));
    }

    #[test]
    fn daemon_alive_but_not_ready_warns() {
        let c = check_daemon("h", &Ok(200), &Ok(503));
        assert_eq!(level(&c), Level::Warn);
        assert!(c.detail.contains("503"));
        assert_eq!(level(&check_daemon("h", &Ok(200), &Ok(200))), Level::Ok);
    }

    #[test]
    fn missing_required_binary_fails_missing_optional_does_not() {
        let c = check_binaries(&[("fluxctl", true), ("qemu-img", false)], &[]);
        assert_eq!(level(&c), Level::Fail);
        assert!(c.detail.contains("qemu-img"));
        let c = check_binaries(&[("fluxctl", true)], &[("dnsmasq", false)]);
        assert_eq!(level(&c), Level::Ok);
        assert!(c.detail.contains("dnsmasq"));
    }

    #[test]
    fn disk_thresholds() {
        let p = Path::new("/var/lib/fluxvm");
        assert_eq!(level(&check_disk(p, Some(GIB / 2))), Level::Fail);
        assert_eq!(level(&check_disk(p, Some(5 * GIB))), Level::Warn);
        assert_eq!(level(&check_disk(p, Some(50 * GIB))), Level::Ok);
        assert_eq!(level(&check_disk(p, None)), Level::Skip);
    }

    #[test]
    fn an_unchecked_daemon_makes_an_unopenable_kvm_a_warning_not_a_failure() {
        let c = check_kvm(true, false, None);
        assert_eq!(level(&c), Level::Warn);
        assert_eq!(exit_code(&[c]), 0);
        // A missing /dev/kvm is still a failure whatever the daemon state.
        assert_eq!(level(&check_kvm(false, false, None)), Level::Fail);
    }

    #[test]
    fn kvm_states() {
        assert_eq!(level(&check_kvm(true, true, Some(false))), Level::Ok);
        assert!(
            check_kvm(true, false, Some(false))
                .fix
                .unwrap()
                .contains("kvm")
        );
        assert!(
            check_kvm(false, false, Some(false))
                .detail
                .contains("missing")
        );
    }

    #[test]
    fn an_unopenable_kvm_is_only_a_warning_while_the_daemon_is_healthy() {
        // The daemon runs as root, this user is not in the kvm group: a healthy host, not a failure.
        let c = check_kvm(true, false, Some(true));
        assert_eq!(level(&c), Level::Warn);
        assert!(c.detail.contains("another user"));
        assert_eq!(exit_code(&[c]), 0);
        // With no daemon there is nothing proving VMs can run, so it stays a failure.
        assert_eq!(level(&check_kvm(true, false, Some(false))), Level::Fail);
        // A missing /dev/kvm fails regardless of the daemon.
        assert_eq!(level(&check_kvm(false, false, Some(true))), Level::Fail);
    }

    #[test]
    fn runner_entitlement_states() {
        let p = Path::new("/opt/homebrew/bin/fluxvm-vz-runner");
        assert_eq!(
            level(&check_runner_entitlement(Some(p), Some(true))),
            Level::Ok
        );
        assert_eq!(
            level(&check_runner_entitlement(Some(p), Some(false))),
            Level::Fail
        );
        assert_eq!(level(&check_runner_entitlement(Some(p), None)), Level::Skip);
        assert_eq!(level(&check_runner_entitlement(None, None)), Level::Fail);
    }

    #[test]
    fn stale_cellar_path_is_reported_with_the_repair_command() {
        let refs = vec![
            PathRef {
                what: "MCP claude-code".into(),
                path: "/opt/homebrew/Cellar/fluxvm/0.4.4/bin/fluxctl".into(),
                exists: false,
            },
            PathRef {
                what: "LaunchAgent".into(),
                path: "/opt/homebrew/bin/fluxctl".into(),
                exists: true,
            },
        ];
        let c = check_stale_paths(&refs);
        assert_eq!(level(&c), Level::Warn);
        assert!(c.detail.contains("0.4.4"));
        assert!(
            !c.detail.contains("LaunchAgent ->"),
            "only dead entries are listed"
        );
        let fix = c.fix.unwrap();
        assert!(fix.contains("mcp install") && fix.contains("doctor --fix"));
        assert_eq!(level(&check_stale_paths(&[])), Level::Skip);
    }

    #[test]
    fn memory_headroom_matches_the_placement_rule() {
        // 8 GiB: reserve max(3 GiB, 2 GiB) = 3 GiB, 5 GiB left.
        let c = check_memory(Some(8 * 1024));
        assert_eq!(level(&c), Level::Ok);
        assert!(c.detail.contains("5120"));
        // 4 GiB leaves 1 GiB.
        assert_eq!(level(&check_memory(Some(4 * 1024))), Level::Warn);
        assert_eq!(level(&check_memory(None)), Level::Skip);
    }

    #[test]
    fn console_session_is_read_from_the_user_dict_not_ioconsolelocked() {
        let locked = "      \"IOConsoleLocked\" = No\n      \"IOConsoleUsers\" = ({\"kCGSSessionOnConsoleKey\"=Yes,\"CGSSessionScreenIsLocked\"=Yes,\"kCGSSessionUserNameKey\"=\"mini\"})\n";
        assert_eq!(parse_console_session(locked), Session::Locked);
        let open = locked.replace("\"CGSSessionScreenIsLocked\"=Yes,", "");
        assert_eq!(parse_console_session(&open), Session::Unlocked);
        assert_eq!(
            parse_console_session(
                "      \"IOConsoleLocked\" = No\n      \"IOConsoleUsers\" = ()\n"
            ),
            Session::NoSession
        );
        assert_eq!(
            parse_console_session("      \"IOConsoleLocked\" = No\n"),
            Session::NoSession
        );
        assert_eq!(parse_console_session(""), Session::Unknown);
    }

    #[test]
    fn locked_or_absent_session_warns_with_the_unattended_hosts_fix() {
        for s in [Session::Locked, Session::NoSession] {
            let c = check_session(s);
            assert_eq!(level(&c), Level::Warn);
            assert!(c.fix.unwrap().contains("agent-density.md#unattended-hosts"));
        }
        assert_eq!(level(&check_session(Session::Unlocked)), Level::Ok);
        assert_eq!(level(&check_session(Session::Unknown)), Level::Skip);
    }

    #[test]
    fn exit_code_is_nonzero_only_for_failures() {
        let warn = Check::warn("a", "d", "f");
        let fail = Check::fail("b", "d", "f");
        assert_eq!(exit_code(&[warn.clone()]), 0);
        assert_eq!(exit_code(&[warn, fail]), 1);
    }

    #[test]
    fn json_report_carries_level_and_fix() {
        let v = to_json(&[Check::fail("daemon", "down", "fluxctl serve")]);
        assert_eq!(v["ok"], false);
        assert_eq!(v["checks"][0]["level"], "FAIL");
        assert_eq!(v["checks"][0]["fix"], "fluxctl serve");
    }

    #[test]
    fn plist_program_reads_the_first_argument() {
        let t = "<dict><key>Label</key><string>x</string><key>ProgramArguments</key><array><string>/opt/homebrew/bin/fluxctl</string><string>serve</string></array></dict>";
        assert_eq!(
            plist_program(t).as_deref(),
            Some("/opt/homebrew/bin/fluxctl")
        );
        assert_eq!(plist_program("<dict></dict>"), None);
    }

    #[test]
    fn free_bytes_reads_an_existing_directory() {
        let d = tempfile::tempdir().unwrap();
        assert!(free_bytes(&d.path().join("not/created/yet")).is_some());
    }
}
