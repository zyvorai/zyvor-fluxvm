// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac host`: the Mac that runs FluxVM, looked at as a server. Read-only `status`, `headless-check` and `rosetta`;
//! a `keepawake` LaunchAgent that holds a `caffeinate` assertion; and `tune`, which prints recommended power settings and
//! changes them only with `--apply --only NAME`.
//!
//! Everything that reads the host goes through [`Sh`], so the parsers and the decisions are tested on captured command output
//! (`mac_host_fixtures/`) and the apply path on a fake that records what it was asked to run. Nothing here is run with
//! `sudo` unless the user passes `--apply`, and then `sudo` prompts for itself. See docs/apple-host-research.md and
//! docs/macos-host.md. Not run against a Mac you do not own: the commands used are documented `pmset`, `fdesetup`, `defaults`,
//! `ioreg`, `system_profiler`, `df` and `launchctl` calls.

use crate::vz_doctor::{self, Check, Status};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The LaunchAgent that holds the sleep assertion.
pub const KEEPAWAKE_LABEL: &str = "dev.zyvor.fluxvm.keepawake";
/// A marker file of an installed Rosetta (inferred: it is what the Rosetta installer writes, not an Apple API).
pub const ROSETTA_MARKER: &str = "/Library/Apple/usr/libexec/oah/libRosettaRuntime";
/// Free space on the data volume below which a Mac is nearly out of room for VM disks and snapshots.
const LOW_DISK_GB: f64 = 20.0;

#[derive(Subcommand, Debug)]
pub enum HostCommand {
    /// Power and sleep settings, display state, auto-login, FileVault, free disk and thermal state, read without sudo.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Keep the Mac awake for the daemon with a `caffeinate` LaunchAgent (`install`, `uninstall`, `status`; default `status`).
    Keepawake {
        #[command(subcommand)]
        action: Option<KeepAwakeAction>,
    },
    /// Is there an unlocked login session? Restoring saved VM state needs one. Exits 1 on a failure; each finding says how to fix it.
    HeadlessCheck {
        #[arg(long)]
        json: bool,
    },
    /// Print recommended power settings for a server Mac. Changes nothing unless `--apply` and `--only NAME` (repeatable) are given;
    /// then runs exactly those `sudo pmset -a ...` commands, shown first.
    #[command(
        after_help = "Examples:\n  fluxctl mac host tune\n  fluxctl mac host tune --apply --only sleep --only autorestart\n\nNames: sleep, displaysleep, disksleep, autorestart, womp, lowpowermode. Auto-login, the screen lock and FileVault are never changed."
    )]
    Tune {
        /// Run the commands named by `--only` (asks `sudo` for a password itself).
        #[arg(long)]
        apply: bool,
        /// A setting to change. Required with `--apply`; there is no "all".
        #[arg(long = "only", value_name = "NAME")]
        only: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Is Rosetta for Linux guests available? Also prints how a Linux guest uses it.
    Rosetta {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum KeepAwakeAction {
    /// Write the LaunchAgent and load it. `caffeinate -i -m -s` (idle, disk and system sleep; `-s` counts on AC power only).
    Install {
        /// Also keep the display awake (`-d`).
        #[arg(long)]
        display: bool,
        /// Print the plist and the launchctl commands; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Unload the agent and delete its plist.
    Uninstall,
    /// Whether the agent is installed and loaded.
    Status,
}

// ------------------------------------------------------------------ host access

/// What the module needs from the machine. The real one runs commands; tests answer from fixtures.
pub trait Sh {
    /// stdout of a command that exits 0, else `None` (also when it cannot be run).
    fn read(&self, program: &str, args: &[&str]) -> Option<String>;
    /// Run a command with the terminal attached (so `sudo` can prompt); true when it exits 0.
    fn run(&self, argv: &[String]) -> Result<bool>;
    fn exists(&self, path: &str) -> bool;
}

pub struct RealSh;

impl Sh for RealSh {
    fn read(&self, program: &str, args: &[&str]) -> Option<String> {
        let o = Command::new(program).args(args).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }
    fn run(&self, argv: &[String]) -> Result<bool> {
        let (prog, args) = argv.split_first().context("empty command")?;
        Ok(Command::new(prog)
            .args(args)
            .status()
            .with_context(|| format!("running {prog}"))?
            .success())
    }
    fn exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }
}

// ------------------------------------------------------------------ parsers

/// `pmset -g`: the "Currently in use" settings and who is holding sleep off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pmset {
    /// Lower-cased setting name to its value (`sleep` -> `0`).
    pub values: BTreeMap<String, String>,
    /// Names from `(sleep prevented by a, b)` notes.
    pub sleep_blockers: Vec<String>,
}

impl Pmset {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }
    fn num(&self, key: &str) -> Option<u32> {
        self.get(key)?.parse().ok()
    }
}

pub fn parse_pmset_g(text: &str) -> Pmset {
    let mut p = Pmset::default();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.ends_with(':') || t.starts_with("Note:") {
            continue;
        }
        let (main, note) = match t.split_once(" (") {
            Some((m, n)) => (m.trim(), Some(n.trim_end_matches(')'))),
            None => (t, None),
        };
        if let Some(n) = note
            && let Some((_, who)) = n.split_once("prevented by ")
        {
            for w in who.split(',').map(str::trim).filter(|w| !w.is_empty()) {
                if !p.sleep_blockers.iter().any(|b| b == w) {
                    p.sleep_blockers.push(w.to_owned());
                }
            }
        }
        // `key value`, where the key may hold spaces (`Sleep On Power Button 1`).
        if let Some((key, value)) = main.rsplit_once(char::is_whitespace) {
            let key = key.trim();
            if !key.is_empty() {
                p.values.insert(key.to_lowercase(), value.to_owned());
            }
        }
    }
    p
}

/// `pmset -g therm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thermal {
    Nominal,
    /// `CPU_Speed_Limit` below 100.
    Throttled(u32),
    Unknown,
}

pub fn parse_pmset_therm(text: &str) -> Thermal {
    let limit = |key: &str| {
        text.lines().find_map(|l| {
            l.trim()
                .strip_prefix(key)?
                .trim_start()
                .strip_prefix('=')?
                .trim()
                .parse::<u32>()
                .ok()
        })
    };
    match limit("CPU_Speed_Limit") {
        Some(n) if n < 100 => Thermal::Throttled(n),
        Some(_) => Thermal::Nominal,
        None if text.contains("No thermal warning level has been recorded")
            && text.contains("No performance warning level has been recorded") =>
        {
            Thermal::Nominal
        }
        None => Thermal::Unknown,
    }
}

/// One line of `pmset -g assertions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assertion {
    pub pid: u32,
    pub process: String,
    pub kind: String,
    pub name: String,
}

/// `pid 51214(caffeinate): [0x..] 00:00:21 PreventUserIdleSystemSleep named: "caffeinate command-line tool"`.
pub fn parse_assertions(text: &str) -> Vec<Assertion> {
    text.lines()
        .filter_map(|l| {
            let rest = l.trim().strip_prefix("pid ")?;
            let (pid, rest) = rest.split_once('(')?;
            let (process, rest) = rest.split_once("):")?;
            let rest = rest.split_once(']')?.1.trim();
            let (_time, rest) = rest.split_once(' ')?;
            let (kind, rest) = rest.trim().split_once(" named:")?;
            Some(Assertion {
                pid: pid.trim().parse().ok()?,
                process: process.to_owned(),
                kind: kind.trim().to_owned(),
                name: rest.trim().trim_matches('"').to_owned(),
            })
        })
        .collect()
}

/// Assertions that keep the system from sleeping, whoever owns them.
pub fn sleep_assertions(a: &[Assertion]) -> Vec<&Assertion> {
    a.iter()
        .filter(|a| {
            matches!(
                a.kind.as_str(),
                "PreventSystemSleep" | "PreventUserIdleSystemSleep"
            )
        })
        .collect()
}

/// `fdesetup status`: `FileVault is On.` / `FileVault is Off.`
pub fn parse_filevault(text: &str) -> Option<bool> {
    let t = text.trim();
    if t.starts_with("FileVault is On") {
        Some(true)
    } else if t.starts_with("FileVault is Off") {
        Some(false)
    } else {
        None
    }
}

/// `defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser` (it exits non-zero when the key is absent).
pub fn parse_autologin(text: Option<&str>) -> Option<String> {
    let t = text?.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// The console session, from `ioreg -n Root -d1 -r`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Console {
    /// `IOConsoleUsers` is empty or absent: the login window is showing.
    pub login_window: bool,
    pub user: Option<String>,
    pub on_console: Option<bool>,
    /// `CGSSessionScreenIsLocked`.
    pub screen_locked: Option<bool>,
    /// The separate top-level `IOConsoleLocked`. On the machine the fixture came from the two disagreed (see the research doc),
    /// so it is reported but not used to decide.
    pub io_console_locked: Option<bool>,
}

fn kv<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"=");
    let rest = &text[text.find(&needle)? + needle.len()..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(rest[..end].trim().trim_matches('"'))
}

fn yes_no(v: Option<&str>) -> Option<bool> {
    match v? {
        "Yes" => Some(true),
        "No" => Some(false),
        _ => None,
    }
}

pub fn parse_console(ioreg: &str) -> Console {
    let io_console_locked = ioreg.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix("\"IOConsoleLocked\"")?;
        yes_no(Some(rest.trim_start().strip_prefix('=')?.trim()))
    });
    let users = ioreg
        .lines()
        .find(|l| l.contains("\"IOConsoleUsers\""))
        .map(|l| l.split_once('=').map_or("", |x| x.1).trim());
    let Some(users) = users.filter(|u| u.contains('{')) else {
        return Console {
            login_window: true,
            io_console_locked,
            ..Console::default()
        };
    };
    Console {
        login_window: false,
        user: kv(users, "kCGSSessionUserNameKey").map(str::to_owned),
        on_console: yes_no(kv(users, "kCGSSessionOnConsoleKey")),
        screen_locked: yes_no(kv(users, "CGSSessionScreenIsLocked")),
        io_console_locked,
    }
}

/// `defaults read /Library/Preferences/com.apple.SoftwareUpdate AutomaticallyInstallMacOSUpdates`: `1`/`0`.
pub fn parse_auto_updates(text: &str) -> Option<bool> {
    match text.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// One display from `system_profiler SPDisplaysDataType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub resolution: String,
    pub online: bool,
}

pub fn parse_displays(text: &str) -> Vec<Display> {
    let indent = |l: &str| l.len() - l.trim_start().len();
    let mut out: Vec<Display> = Vec::new();
    let mut base: Option<usize> = None;
    for l in text.lines() {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if t == "Displays:" {
            base = Some(indent(l));
            continue;
        }
        let Some(b) = base else { continue };
        if indent(l) <= b {
            base = None;
            continue;
        }
        if indent(l) == b + 2 && t.ends_with(':') {
            out.push(Display {
                resolution: String::new(),
                online: false,
            });
        } else if let Some(d) = out.last_mut() {
            if let Some(r) = t.strip_prefix("Resolution:") {
                d.resolution = r.trim().to_owned();
            } else if let Some(o) = t.strip_prefix("Online:") {
                d.online = o.trim() == "Yes";
            }
        }
    }
    out
}

/// `df -k PATH`: bytes available.
pub fn parse_df_available(text: &str) -> Option<u64> {
    let line = text.lines().rfind(|l| !l.trim().is_empty())?;
    let kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb * 1024)
}

/// The quoted power source of `pmset -g batt` (`AC Power`).
pub fn parse_power_source(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let (_, rest) = first.split_once('\'')?;
    Some(rest.split_once('\'')?.0.to_owned())
}

// ------------------------------------------------------------------ facts

#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    pub os_version: Option<String>,
    pub arch: String,
    pub current_user: String,
    pub pmset: Option<Pmset>,
    pub power_source: Option<String>,
    pub assertions: Vec<Assertion>,
    pub thermal: Option<Thermal>,
    pub low_power_mode: Option<bool>,
    pub console: Option<Console>,
    pub autologin: Option<String>,
    pub filevault: Option<bool>,
    pub displays: Option<Vec<Display>>,
    pub free_bytes: Option<u64>,
    pub keepawake_loaded: bool,
    /// `AutomaticallyInstallMacOSUpdates`: on means macOS may install an update and reboot unattended.
    pub auto_macos_updates: Option<bool>,
}

pub fn collect(sh: &dyn Sh, _uid: u32, user: &str, keepawake_loaded: bool) -> HostFacts {
    let pmset = sh.read("pmset", &["-g"]).map(|t| parse_pmset_g(&t));
    let low_power_mode = pmset
        .as_ref()
        .and_then(|p| p.num("lowpowermode"))
        .map(|n| n == 1);
    HostFacts {
        os_version: sh
            .read("/usr/bin/sw_vers", &["-productVersion"])
            .map(|s| s.trim().to_owned()),
        arch: std::env::consts::ARCH.to_owned(),
        current_user: user.to_owned(),
        pmset,
        power_source: sh
            .read("pmset", &["-g", "batt"])
            .and_then(|t| parse_power_source(&t)),
        assertions: sh
            .read("pmset", &["-g", "assertions"])
            .map(|t| parse_assertions(&t))
            .unwrap_or_default(),
        thermal: sh
            .read("pmset", &["-g", "therm"])
            .map(|t| parse_pmset_therm(&t)),
        low_power_mode,
        console: sh
            .read("ioreg", &["-n", "Root", "-d1", "-r"])
            .map(|t| parse_console(&t)),
        autologin: parse_autologin(
            sh.read(
                "defaults",
                &[
                    "read",
                    "/Library/Preferences/com.apple.loginwindow",
                    "autoLoginUser",
                ],
            )
            .as_deref(),
        ),
        filevault: sh
            .read("fdesetup", &["status"])
            .and_then(|t| parse_filevault(&t)),
        displays: sh
            .read("system_profiler", &["SPDisplaysDataType"])
            .map(|t| parse_displays(&t)),
        free_bytes: sh
            .read("df", &["-k", "/System/Volumes/Data"])
            .or_else(|| sh.read("df", &["-k", "/"]))
            .and_then(|t| parse_df_available(&t)),
        keepawake_loaded,
        // The key is absent until someone changes the setting, which is how `defaults` reports the default: unknown, and
        // not treated as on.
        auto_macos_updates: sh
            .read(
                "defaults",
                &[
                    "read",
                    "/Library/Preferences/com.apple.SoftwareUpdate",
                    "AutomaticallyInstallMacOSUpdates",
                ],
            )
            .and_then(|t| parse_auto_updates(&t)),
    }
}

// ------------------------------------------------------------------ status

fn thermal_text(t: &Option<Thermal>) -> String {
    match t {
        Some(Thermal::Nominal) => "nominal (no throttling recorded)".into(),
        Some(Thermal::Throttled(n)) => format!("throttled: CPU speed limit {n}%"),
        _ => "unknown".into(),
    }
}

fn gib(b: u64) -> String {
    format!("{:.1} GiB", b as f64 / (1u64 << 30) as f64)
}

pub fn status_json(f: &HostFacts) -> Value {
    let get = |k: &str| f.pmset.as_ref().and_then(|p| p.get(k)).map(str::to_owned);
    json!({
        "os_version": f.os_version,
        "arch": f.arch,
        "power": {
            "source": f.power_source,
            "sleep_minutes": get("sleep"),
            "displaysleep_minutes": get("displaysleep"),
            "disksleep_minutes": get("disksleep"),
            "autorestart": get("autorestart"),
            "wake_on_network": get("womp"),
            "low_power_mode": f.low_power_mode,
            "sleep_held_off_by": f.pmset.as_ref().map(|p| p.sleep_blockers.clone()).unwrap_or_default(),
        },
        "sleep_assertions": sleep_assertions(&f.assertions).iter().map(|a| json!({
            "pid": a.pid, "process": a.process, "kind": a.kind, "name": a.name,
        })).collect::<Vec<_>>(),
        "keepawake_agent_loaded": f.keepawake_loaded,
        "session": f.console.as_ref().map(|c| json!({
            "login_window": c.login_window,
            "console_user": c.user,
            "screen_locked": c.screen_locked,
            "io_console_locked": c.io_console_locked,
        })),
        "auto_login_user": f.autologin,
        "filevault": f.filevault,
        "displays": f.displays.as_ref().map(|d| d.iter().map(|d| json!({
            "resolution": d.resolution, "online": d.online,
        })).collect::<Vec<_>>()),
        "free_bytes": f.free_bytes,
        "auto_install_macos_updates": f.auto_macos_updates,
        "thermal": thermal_text(&f.thermal),
    })
}

pub fn render_status(f: &HostFacts) -> String {
    let get = |k: &str| {
        f.pmset
            .as_ref()
            .and_then(|p| p.get(k))
            .map_or("?".to_owned(), str::to_owned)
    };
    let mins = |k: &str| match get(k).as_str() {
        "0" => "never".to_owned(),
        "?" => "?".to_owned(),
        v => format!("{v} min"),
    };
    let onoff = |v: Option<bool>| match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    let mut s = String::new();
    let mut row = |k: &str, v: String| s.push_str(&format!("{k:<26} {v}\n"));
    row(
        "macOS",
        format!("{} ({})", f.os_version.as_deref().unwrap_or("?"), f.arch),
    );
    row("power source", f.power_source.clone().unwrap_or("?".into()));
    row("system sleep", mins("sleep"));
    row("display sleep", mins("displaysleep"));
    row("disk sleep", mins("disksleep"));
    row(
        "restart after power loss",
        onoff(get("autorestart").parse::<u8>().ok().map(|n| n == 1)).into(),
    );
    row(
        "wake on network",
        onoff(get("womp").parse::<u8>().ok().map(|n| n == 1)).into(),
    );
    row("low power mode", onoff(f.low_power_mode).into());
    let holders: Vec<String> = sleep_assertions(&f.assertions)
        .iter()
        .map(|a| a.process.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    row(
        "sleep held off by",
        if holders.is_empty() {
            "nothing".into()
        } else {
            holders.join(", ")
        },
    );
    row(
        "keepawake agent",
        if f.keepawake_loaded {
            "loaded"
        } else {
            "not loaded"
        }
        .into(),
    );
    row(
        "console session",
        match &f.console {
            None => "unknown".into(),
            Some(c) if c.login_window => "login window (nobody logged in)".into(),
            Some(c) => format!(
                "{} ({})",
                c.user.as_deref().unwrap_or("?"),
                match c.screen_locked {
                    Some(true) => "screen locked",
                    Some(false) => "unlocked",
                    None => "lock state unknown",
                }
            ),
        },
    );
    row(
        "automatic login",
        f.autologin.clone().unwrap_or_else(|| "off".into()),
    );
    row(
        "FileVault",
        match f.filevault {
            Some(true) => "on",
            Some(false) => "off",
            None => "unknown",
        }
        .into(),
    );
    row(
        "displays",
        match &f.displays {
            None => "unknown".into(),
            Some(d) if d.is_empty() => "none attached".into(),
            Some(d) => d
                .iter()
                .map(|d| {
                    format!(
                        "{}{}",
                        d.resolution,
                        if d.online { "" } else { " (offline)" }
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        },
    );
    row(
        "free disk (data volume)",
        f.free_bytes.map_or("?".into(), gib),
    );
    row("thermal", thermal_text(&f.thermal));
    row(
        "automatic macOS updates",
        match f.auto_macos_updates {
            Some(true) => {
                "ON: an update can reboot this Mac unattended and invalidate saved VM state".into()
            }
            Some(false) => "off".into(),
            None => "not set (macOS default)".into(),
        },
    );
    s
}

// ------------------------------------------------------------------ headless-check

fn chk(name: &'static str, status: Status, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
        fix: fix.map(str::to_owned),
    }
}

/// The unlocked-session requirement and what keeps a headless Mac reachable. Pure.
pub fn headless_checks(f: &HostFacts) -> Vec<Check> {
    let mut out = Vec::new();
    match &f.console {
        None => out.push(chk(
            "console session",
            Status::Warn,
            "could not read the console session (ioreg)",
            None,
        )),
        Some(c) if c.login_window => out.push(chk(
            "console session",
            Status::Fail,
            "nobody is logged in at the console: the login window is showing, so a saved VM state cannot be restored",
            Some(
                "turn on automatic login for the user that runs FluxVM (System Settings > Users & Groups > Automatic login), or log in once over Screen Sharing",
            ),
        )),
        Some(c) => {
            out.push(chk(
                "console session",
                Status::Ok,
                format!("{} is logged in", c.user.as_deref().unwrap_or("a user")),
                None,
            ));
            match c.screen_locked {
                Some(true) => out.push(chk(
                    "screen lock",
                    Status::Fail,
                    "the session's screen is locked: Virtualization.framework restores saved state only in an unlocked session",
                    Some(
                        "unlock it (Screen Sharing), then System Settings > Lock Screen: 'Require password after screen saver begins or display is turned off' = Never; set the screen saver and display-off timers to Never",
                    ),
                )),
                Some(false) => out.push(chk("screen lock", Status::Ok, "unlocked", None)),
                None => out.push(chk(
                    "screen lock",
                    Status::Warn,
                    "lock state not reported",
                    None,
                )),
            }
            if !f.current_user.is_empty()
                && let Some(u) = &c.user
                && *u != f.current_user
            {
                out.push(chk(
                    "session owner",
                    Status::Warn,
                    format!(
                        "{u} is on the console but this is {}: the daemon's LaunchAgent runs in its own user's session",
                        f.current_user
                    ),
                    Some("run fluxctl as the console user, or log that user in (fast user switching leaves the other session in the background)"),
                ));
            }
        }
    }
    match (&f.autologin, f.filevault) {
        (Some(u), _) => out.push(chk(
            "automatic login",
            Status::Ok,
            format!("on for {u}: a reboot returns to a session"),
            None,
        )),
        (None, Some(true)) => out.push(chk(
            "automatic login",
            Status::Warn,
            "off, and FileVault is on: macOS does not offer automatic login with FileVault, so after a reboot someone must unlock the disk and log in",
            Some(
                "accept that and avoid reboots, or turn FileVault off (System Settings > Privacy & Security > FileVault; decrypts the disk) and then enable automatic login",
            ),
        )),
        (None, _) => out.push(chk(
            "automatic login",
            Status::Warn,
            "off: after a reboot or power cut the Mac waits at the login window",
            Some("System Settings > Users & Groups > Automatic login (the user that runs FluxVM)"),
        )),
    }
    if f.filevault == Some(true) {
        out.push(chk(
            "FileVault",
            Status::Warn,
            "on: an unattended reboot stops at the pre-boot unlock",
            Some("`sudo fdesetup authrestart` can bring a planned restart back without typing (it needs the user's password; check `man fdesetup` for your macOS)"),
        ));
    }
    let sleep_zero = f.pmset.as_ref().and_then(|p| p.num("sleep")) == Some(0);
    let held = !sleep_assertions(&f.assertions)
        .iter()
        .all(|a| a.process == "powerd");
    if sleep_zero || f.keepawake_loaded {
        out.push(chk(
            "idle sleep",
            Status::Ok,
            if f.keepawake_loaded {
                "held off by the keepawake agent".to_owned()
            } else {
                "system sleep is set to never".to_owned()
            },
            None,
        ));
    } else if held {
        out.push(chk(
            "idle sleep",
            Status::Warn,
            "sleep is on and only a transient assertion (a running tool) is holding it off",
            Some("fluxctl mac host keepawake install"),
        ));
    } else {
        out.push(chk(
            "idle sleep",
            Status::Fail,
            "the Mac will sleep when idle: the API and every VM stop being reachable",
            Some("fluxctl mac host keepawake install   (or: sudo pmset -a sleep 0; see `fluxctl mac host tune`)"),
        ));
    }
    match &f.displays {
        Some(d) if d.iter().any(|d| d.online) => {
            out.push(chk("display", Status::Ok, "a display is online", None))
        }
        Some(_) => out.push(chk(
            "display",
            Status::Warn,
            "no display is online: Screen Sharing falls back to a small default resolution and some Macs behave differently without a display",
            Some("attach an HDMI/DisplayPort dummy plug (display emulator) to the headless Mac"),
        )),
        None => {}
    }
    if f.auto_macos_updates == Some(true) {
        out.push(chk(
            "automatic macOS updates",
            Status::Warn,
            "on: macOS can download, install and reboot into a new build on its own; saved VM state does not survive a macOS update and the Mac may wait at the login window",
            Some("System Settings > General > Software Update > (i) > turn off 'Install macOS updates', and update on your schedule; then re-take snapshots"),
        ));
    }
    if let Some(b) = f.free_bytes {
        let gb = b as f64 / 1e9;
        if gb < LOW_DISK_GB {
            out.push(chk(
                "free disk",
                Status::Warn,
                format!("{} free on the data volume", gib(b)),
                Some("`fluxctl mac images` and `fluxctl image prune` show what can go; macOS guests need about 55 GB"),
            ));
        }
    }
    out
}

// ------------------------------------------------------------------ tune

/// One recommended `pmset -a` setting.
pub struct Recommendation {
    pub name: &'static str,
    pub value: &'static str,
    pub why: &'static str,
}

pub const RECOMMENDATIONS: [Recommendation; 6] = [
    Recommendation {
        name: "sleep",
        value: "0",
        why: "never sleep when idle: the daemon, SSH and every VM stay reachable",
    },
    Recommendation {
        name: "displaysleep",
        value: "0",
        why: "a sleeping display can lock the session, and a saved VM state restores only in an unlocked one",
    },
    Recommendation {
        name: "disksleep",
        value: "0",
        why: "never spin down storage (a no-op on an SSD)",
    },
    Recommendation {
        name: "autorestart",
        value: "1",
        why: "power back on after a power cut",
    },
    Recommendation {
        name: "womp",
        value: "1",
        why: "wake for network access (Wake on LAN) if it does sleep",
    },
    Recommendation {
        name: "lowpowermode",
        value: "0",
        why: "Low Power Mode throttles the CPU",
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: &'static str,
    pub current: Option<String>,
    pub wanted: &'static str,
    pub why: &'static str,
}

impl Change {
    pub fn command(&self) -> Vec<String> {
        ["sudo", "pmset", "-a", self.name, self.wanted]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }
}

/// The recommendations that differ from what `pmset -g` reports. A setting the Mac does not list (`lowpowermode` on a
/// desktop that lacks it, or an older macOS) is left out: it cannot be set there either.
pub fn plan_changes(p: &Pmset) -> Vec<Change> {
    RECOMMENDATIONS
        .iter()
        .filter_map(|r| {
            let cur = p.get(r.name)?;
            (cur != r.value).then(|| Change {
                name: r.name,
                current: Some(cur.to_owned()),
                wanted: r.value,
                why: r.why,
            })
        })
        .collect()
}

/// The names `--only` may take; anything else is refused before a command runs.
pub fn select_changes(plan: &[Change], only: &[String]) -> Result<Vec<Change>> {
    ensure!(
        !only.is_empty(),
        "--apply needs an explicit list: pass --only NAME for each setting to change (names: {})",
        RECOMMENDATIONS
            .iter()
            .map(|r| r.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut out = Vec::new();
    for n in only {
        ensure!(
            RECOMMENDATIONS.iter().any(|r| r.name == n),
            "unknown setting `{n}` (names: {})",
            RECOMMENDATIONS
                .iter()
                .map(|r| r.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        match plan.iter().find(|c| c.name == n) {
            Some(c) if !out.contains(c) => out.push(c.clone()),
            Some(_) => {}
            None => eprintln!("{n}: already as recommended (or not offered by this Mac); skipped"),
        }
    }
    Ok(out)
}

pub fn render_tune(plan: &[Change], p: &Pmset) -> String {
    let mut s = String::from(
        "Recommended power settings for a server Mac (nothing is changed without --apply --only NAME):\n\n",
    );
    for r in &RECOMMENDATIONS {
        let cur = p.get(r.name).unwrap_or("not offered");
        let mark = if cur == r.value {
            "ok "
        } else if p.get(r.name).is_none() {
            "n/a"
        } else {
            "set"
        };
        s.push_str(&format!(
            "  [{mark}] {:<13} now {:<11} want {:<2} {}\n",
            r.name, cur, r.value, r.why
        ));
    }
    if plan.is_empty() {
        s.push_str("\nNothing to change.\n");
    } else {
        s.push_str("\nTo apply (each needs sudo):\n");
        for c in plan {
            s.push_str(&format!("  {}\n", c.command().join(" ")));
        }
        s.push_str("or: fluxctl mac host tune --apply");
        for c in plan {
            s.push_str(&format!(" --only {}", c.name));
        }
        s.push('\n');
    }
    s.push_str(
        "\nNot changed by fluxctl, because they are security settings or need a password dialog:\n  \
         automatic login   System Settings > Users & Groups (not available with FileVault on)\n  \
         screen lock       System Settings > Lock Screen: require password after display off = Never\n  \
         FileVault         System Settings > Privacy & Security\n  \
         keep awake        `fluxctl mac host keepawake install` (no sudo, a LaunchAgent)\n",
    );
    s
}

/// Runs exactly the selected commands, stopping at the first failure. Returns what was run.
pub fn apply(sh: &dyn Sh, chosen: &[Change]) -> Result<Vec<Vec<String>>> {
    let mut ran = Vec::new();
    for c in chosen {
        let cmd = c.command();
        println!("running: {}", cmd.join(" "));
        if !sh.run(&cmd)? {
            bail!(
                "`{}` failed; stopped (nothing after it was run)",
                cmd.join(" ")
            );
        }
        ran.push(cmd);
    }
    Ok(ran)
}

// ------------------------------------------------------------------ keepawake

/// The LaunchAgent plist. `caffeinate` holds the assertions for as long as launchd keeps it running.
pub fn render_keepawake_plist(display: bool) -> String {
    let mut args = vec!["/usr/bin/caffeinate", "-i", "-m", "-s"];
    if display {
        args.push("-d");
    }
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{a}</string>\n"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{KEEPAWAKE_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
</dict>
</plist>
"#
    )
}

pub fn keepawake_plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{KEEPAWAKE_LABEL}.plist"))
}

/// `bootout` (ignored if not loaded) then `bootstrap`, for the plist.
pub fn keepawake_load_commands(uid: u32, plist: &Path) -> Vec<Vec<String>> {
    let domain = format!("gui/{uid}");
    vec![
        vec![
            "/bin/launchctl".into(),
            "bootout".into(),
            format!("{domain}/{KEEPAWAKE_LABEL}"),
        ],
        vec![
            "/bin/launchctl".into(),
            "bootstrap".into(),
            domain,
            plist.display().to_string(),
        ],
    ]
}

pub fn keepawake_install(
    sh: &dyn Sh,
    home: &Path,
    uid: u32,
    display: bool,
    dry_run: bool,
) -> Result<String> {
    let plist_path = keepawake_plist_path(home);
    let plist = render_keepawake_plist(display);
    let cmds = keepawake_load_commands(uid, &plist_path);
    if dry_run {
        let mut s = format!(
            "would write {}:\n{plist}\nthen run:\n",
            plist_path.display()
        );
        for c in &cmds {
            s.push_str(&format!("  {}\n", c.join(" ")));
        }
        return Ok(s);
    }
    if let Some(dir) = plist_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&plist_path, plist)
        .with_context(|| format!("writing {}", plist_path.display()))?;
    // An agent that was not loaded makes `bootout` fail: that is fine.
    let _ = sh.run(&cmds[0]);
    if !sh.run(&cmds[1])? {
        bail!("`{}` failed", cmds[1].join(" "));
    }
    Ok(format!(
        "keepawake agent installed and loaded ({})\nremove it with `fluxctl mac host keepawake uninstall`\n",
        plist_path.display()
    ))
}

pub fn keepawake_uninstall(sh: &dyn Sh, home: &Path, uid: u32) -> Result<String> {
    let plist = keepawake_plist_path(home);
    let _ = sh.run(&[
        "/bin/launchctl".into(),
        "bootout".into(),
        format!("gui/{uid}/{KEEPAWAKE_LABEL}"),
    ]);
    match std::fs::remove_file(&plist) {
        Ok(()) => Ok(format!("removed {}\n", plist.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok("the keepawake agent was not installed\n".into())
        }
        Err(e) => Err(e).with_context(|| format!("removing {}", plist.display())),
    }
}

// ------------------------------------------------------------------ rosetta

#[derive(Debug, Clone, Default)]
pub struct RosettaFacts {
    pub arm64: bool,
    /// `installed`, `not-installed` or `not-supported` from the signed runner; `None` when it could not be asked.
    pub runner: Option<String>,
    pub marker: bool,
}

pub fn rosetta_facts(sh: &dyn Sh) -> RosettaFacts {
    RosettaFacts {
        arm64: matches!(std::env::consts::ARCH, "aarch64" | "arm64"),
        runner: fluxvm_apple::host_capabilities()
            .ok()
            .map(|c| c.rosetta)
            .filter(|r| !r.is_empty()),
        marker: sh.exists(ROSETTA_MARKER),
    }
}

pub fn rosetta_checks(f: &RosettaFacts) -> Vec<Check> {
    const INSTALL: &str = "fluxvm-vz-runner install-rosetta   (or: softwareupdate --install-rosetta --agree-to-license)";
    if !f.arm64 {
        return vec![chk(
            "Rosetta",
            Status::Warn,
            "not an Apple silicon Mac: Rosetta for Linux guests does not apply",
            None,
        )];
    }
    vec![match (f.runner.as_deref(), f.marker) {
        (Some("installed"), true) => chk(
            "Rosetta",
            Status::Ok,
            "installed (runner reports it and the runtime file exists)",
            None,
        ),
        (Some("installed"), false) => chk(
            "Rosetta",
            Status::Warn,
            "the runner reports it installed, but the runtime file is missing; on some macOS 27 builds the availability value has been reported as unreliable. Boot a guest with `rosetta: true` to be sure",
            Some(INSTALL),
        ),
        (Some("not-installed"), _) => chk(
            "Rosetta",
            Status::Warn,
            "not installed: guests cannot run x86_64 binaries or linux/amd64 images",
            Some(INSTALL),
        ),
        (Some("not-supported"), _) => chk(
            "Rosetta",
            Status::Warn,
            "the runner says Rosetta for Linux is not supported on this Mac",
            None,
        ),
        (_, true) => chk(
            "Rosetta",
            Status::Ok,
            "runtime file present (the runner could not be asked)",
            None,
        ),
        (_, false) => chk(
            "Rosetta",
            Status::Warn,
            "no runtime file and the runner could not be asked: probably not installed",
            Some(INSTALL),
        ),
    }]
}

/// What a Linux guest does with Rosetta. The same mount and registration `kube up --rosetta` writes into its guest.
pub fn rosetta_guidance() -> String {
    format!(
        "Using Rosetta in a Linux guest:\n\
         \x20 1. Create the VM with `rosetta: true` (and optionally `rosetta_cache`); the runner shares it as virtiofs tag `rosetta`.\n\
         \x20 2. In the guest: mkdir -p /mnt/rosetta && mount -t virtiofs rosetta /mnt/rosetta\n\
         \x20 3. Register it for x86-64 ELF files (binfmt_misc):\n\
         \x20      mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc   # if not mounted\n\
         \x20      printf '%s' '{}' > /proc/sys/fs/binfmt_misc/register\n\
         \x20 4. Run an x86-64 binary; or pull a linux/amd64 image with `--platform linux/amd64`.\n\
         `fluxctl kube up --rosetta` does steps 2 and 3 at every boot. Rosetta is for Linux guests only: macOS guests have their own.\n\
         It does not provide AVX, so some binaries fail; keep databases native.\n",
        crate::kube_host::rosetta_binfmt()
    )
}

// ------------------------------------------------------------------ commands

pub(crate) fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

pub(crate) fn user() -> String {
    std::env::var("USER").unwrap_or_default()
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn require_macos() -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "`fluxctl mac host` reads macOS settings (pmset, launchd): run it on the Mac"
    );
    Ok(())
}

pub(crate) fn keepawake_loaded() -> bool {
    home()
        .ok()
        .and_then(|h| {
            crate::launch_agent::status_of(
                KEEPAWAKE_LABEL,
                &keepawake_plist_path(&h),
                &h.join("Library/Logs/fluxvm-keepawake.log"),
            )
            .ok()
        })
        .is_some_and(|v| v["loaded"] == true)
}

fn print_checks(checks: &[Check], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&vz_doctor::render_json(checks))?
        );
    } else {
        print!("{}", vz_doctor::render_text(checks));
    }
    if vz_doctor::worst(checks) == Status::Fail {
        std::process::exit(1);
    }
    Ok(())
}

pub fn run(command: HostCommand) -> Result<()> {
    require_macos()?;
    let sh = RealSh;
    match command {
        HostCommand::Status { json } => {
            let f = collect(&sh, uid(), &user(), keepawake_loaded());
            if json {
                println!("{}", serde_json::to_string_pretty(&status_json(&f))?);
            } else {
                print!("{}", render_status(&f));
            }
        }
        HostCommand::HeadlessCheck { json } => {
            let f = collect(&sh, uid(), &user(), keepawake_loaded());
            print_checks(&headless_checks(&f), json)?;
        }
        HostCommand::Keepawake { action } => match action.unwrap_or(KeepAwakeAction::Status) {
            KeepAwakeAction::Install { display, dry_run } => {
                print!(
                    "{}",
                    keepawake_install(&sh, &home()?, uid(), display, dry_run)?
                )
            }
            KeepAwakeAction::Uninstall => print!("{}", keepawake_uninstall(&sh, &home()?, uid())?),
            KeepAwakeAction::Status => {
                let h = home()?;
                let v = crate::launch_agent::status_of(
                    KEEPAWAKE_LABEL,
                    &keepawake_plist_path(&h),
                    &h.join("Library/Logs/fluxvm-keepawake.log"),
                )?;
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
        },
        HostCommand::Tune {
            apply: do_apply,
            only,
            json,
        } => {
            let p = sh
                .read("pmset", &["-g"])
                .map(|t| parse_pmset_g(&t))
                .context("could not run `pmset -g`")?;
            let plan = plan_changes(&p);
            if do_apply {
                let chosen = select_changes(&plan, &only)?;
                let ran = apply(&sh, &chosen)?;
                let after = sh
                    .read("pmset", &["-g"])
                    .map(|t| parse_pmset_g(&t))
                    .unwrap_or_default();
                println!("applied {} change(s). Now:", ran.len());
                for c in &chosen {
                    println!("  {:<13} {}", c.name, after.get(c.name).unwrap_or("?"));
                }
            } else {
                ensure!(
                    only.is_empty(),
                    "--only needs --apply (without it nothing is changed)"
                );
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "changes": plan.iter().map(|c| json!({
                                "name": c.name, "current": c.current, "wanted": c.wanted,
                                "why": c.why, "command": c.command(),
                            })).collect::<Vec<_>>(),
                        }))?
                    );
                } else {
                    print!("{}", render_tune(&plan, &p));
                }
            }
        }
        HostCommand::Rosetta { json } => {
            let checks = rosetta_checks(&rosetta_facts(&sh));
            print_checks(&checks, json)?;
            if !json {
                print!("\n{}", rosetta_guidance());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const PMSET_G: &str = include_str!("mac_host_fixtures/pmset_g.txt");
    const THERM: &str = include_str!("mac_host_fixtures/pmset_therm.txt");
    const ASSERT: &str = include_str!("mac_host_fixtures/pmset_assertions.txt");
    const BATT: &str = include_str!("mac_host_fixtures/pmset_batt.txt");
    const FDE: &str = include_str!("mac_host_fixtures/fdesetup_on.txt");
    const IOREG: &str = include_str!("mac_host_fixtures/ioreg_console.txt");
    const DISPLAYS: &str = include_str!("mac_host_fixtures/displays.txt");
    const DF: &str = include_str!("mac_host_fixtures/df.txt");

    /// Answers from fixtures and records what `run` was asked; never touches the machine.
    #[derive(Default)]
    struct Fake {
        out: Vec<(&'static str, Vec<&'static str>, &'static str)>,
        ran: RefCell<Vec<Vec<String>>>,
        fail_on: Option<&'static str>,
        marker: bool,
    }
    impl Sh for Fake {
        fn read(&self, program: &str, args: &[&str]) -> Option<String> {
            self.out
                .iter()
                .find(|(p, a, _)| *p == program && a.as_slice() == args)
                .map(|(_, _, o)| (*o).to_owned())
        }
        fn run(&self, argv: &[String]) -> Result<bool> {
            self.ran.borrow_mut().push(argv.to_vec());
            Ok(self.fail_on.is_none_or(|f| !argv.join(" ").contains(f)))
        }
        fn exists(&self, _: &str) -> bool {
            self.marker
        }
    }

    fn fixture_fake() -> Fake {
        Fake {
            out: vec![
                ("pmset", vec!["-g"], PMSET_G),
                ("pmset", vec!["-g", "batt"], BATT),
                ("pmset", vec!["-g", "assertions"], ASSERT),
                ("pmset", vec!["-g", "therm"], THERM),
                ("ioreg", vec!["-n", "Root", "-d1", "-r"], IOREG),
                ("fdesetup", vec!["status"], FDE),
                ("system_profiler", vec!["SPDisplaysDataType"], DISPLAYS),
                ("df", vec!["-k", "/System/Volumes/Data"], DF),
                ("/usr/bin/sw_vers", vec!["-productVersion"], "27.3\n"),
            ],
            ..Fake::default()
        }
    }

    #[test]
    fn pending_automatic_updates_warn() {
        assert_eq!(parse_auto_updates("1\n"), Some(true));
        assert_eq!(parse_auto_updates("0"), Some(false));
        assert_eq!(
            parse_auto_updates("The domain/default pair does not exist"),
            None
        );
        let mut f = HostFacts::default();
        assert!(
            headless_checks(&f)
                .iter()
                .all(|c| c.name != "automatic macOS updates")
        );
        f.auto_macos_updates = Some(true);
        let c = headless_checks(&f);
        let w = c
            .iter()
            .find(|c| c.name == "automatic macOS updates")
            .unwrap();
        assert_eq!(w.status, Status::Warn);
        assert!(render_status(&f).contains("ON:"));
        assert_eq!(status_json(&f)["auto_install_macos_updates"], true);
    }

    #[test]
    fn pmset_g_fixture_is_parsed() {
        let p = parse_pmset_g(PMSET_G);
        assert_eq!(p.get("sleep"), Some("0"));
        assert_eq!(p.get("disksleep"), Some("10"));
        assert_eq!(p.get("womp"), Some("1"));
        assert_eq!(p.get("lowpowermode"), Some("0"));
        assert_eq!(p.get("sleep on power button"), Some("1"));
        assert_eq!(p.sleep_blockers, ["screensharingd", "powerd", "caffeinate"]);
    }

    #[test]
    fn thermal_fixture_and_a_throttled_sample() {
        assert_eq!(parse_pmset_therm(THERM), Thermal::Nominal);
        // The throttled shape is the one kube_policy already parses; it is not a captured output.
        let hot = "CPU_Scheduler_Limit = 100\nCPU_Available_CPUs = 8\nCPU_Speed_Limit = 70\n";
        assert_eq!(parse_pmset_therm(hot), Thermal::Throttled(70));
        assert_eq!(
            parse_pmset_therm("CPU_Speed_Limit = 100\n"),
            Thermal::Nominal
        );
        assert_eq!(parse_pmset_therm(""), Thermal::Unknown);
    }

    #[test]
    fn assertions_fixture_finds_caffeinate_and_system_sleep_holders() {
        let a = parse_assertions(ASSERT);
        assert!(a.iter().any(|x| x.process == "caffeinate"
            && x.kind == "PreventUserIdleSystemSleep"
            && x.name == "caffeinate command-line tool"));
        assert!(
            a.iter()
                .any(|x| x.process == "screensharingd" && x.kind == "PreventSystemSleep")
        );
        assert_eq!(a.iter().find(|x| x.process == "powerd").unwrap().pid, 594);
        assert!(!sleep_assertions(&a).is_empty());
        // Header and detail lines are not assertions.
        assert!(a.iter().all(|x| x.pid > 0));
    }

    #[test]
    fn small_command_outputs() {
        assert_eq!(parse_filevault(FDE), Some(true));
        assert_eq!(parse_filevault("FileVault is Off.\n"), Some(false));
        assert_eq!(parse_filevault("weird"), None);
        assert_eq!(parse_autologin(None), None);
        assert_eq!(parse_autologin(Some("admin\n")), Some("admin".into()));
        assert_eq!(parse_power_source(BATT).as_deref(), Some("AC Power"));
        assert_eq!(parse_df_available(DF), Some(34_973_776 * 1024));
    }

    #[test]
    fn console_fixture_reports_session_and_lock() {
        let c = parse_console(IOREG);
        assert!(!c.login_window);
        assert_eq!(c.user.as_deref(), Some("admin"));
        assert_eq!(c.on_console, Some(true));
        assert_eq!(c.screen_locked, Some(true));
        // The two lock signals disagreed on the captured machine; the module decides on the session's one.
        assert_eq!(c.io_console_locked, Some(false));
        let at_login = parse_console("      \"IOConsoleLocked\" = No\n");
        assert!(at_login.login_window && at_login.user.is_none());
        let empty = parse_console("  \"IOConsoleUsers\" = ()\n");
        assert!(empty.login_window);
    }

    #[test]
    fn displays_fixture_and_none_attached() {
        let d = parse_displays(DISPLAYS);
        assert_eq!(d.len(), 1);
        assert!(d[0].online);
        assert!(d[0].resolution.starts_with("1920 x 1080"));
        let headless = "Graphics/Displays:\n\n    Apple M4:\n\n      Type: GPU\n      Total Number of Cores: 10\n";
        assert!(parse_displays(headless).is_empty());
    }

    #[test]
    fn status_renders_every_fact_from_the_fixtures() {
        let f = collect(&fixture_fake(), 501, "admin", false);
        let t = render_status(&f);
        for want in [
            "27.3",
            "AC Power",
            "never",
            "10 min",
            "restart after power loss",
            "yes",
            "caffeinate, powerd, screensharingd",
            "admin (screen locked)",
            "off",
            "on",
            "1920 x 1080",
            "33.4 GiB",
            "nominal",
        ] {
            assert!(t.contains(want), "missing `{want}` in:\n{t}");
        }
        let j = status_json(&f);
        assert_eq!(j["filevault"], true);
        assert_eq!(j["session"]["screen_locked"], true);
        assert_eq!(j["power"]["sleep_minutes"], "0");
    }

    #[test]
    fn headless_check_fails_on_a_locked_session_with_a_fix() {
        let f = collect(&fixture_fake(), 501, "admin", false);
        let checks = headless_checks(&f);
        let lock = checks.iter().find(|c| c.name == "screen lock").unwrap();
        assert_eq!(lock.status, Status::Fail);
        assert!(lock.fix.as_ref().unwrap().contains("Lock Screen"));
        let auto = checks.iter().find(|c| c.name == "automatic login").unwrap();
        assert_eq!(auto.status, Status::Warn);
        assert!(auto.detail.contains("FileVault"));
        assert_eq!(
            checks
                .iter()
                .find(|c| c.name == "idle sleep")
                .unwrap()
                .status,
            Status::Ok
        );
        assert_eq!(vz_doctor::worst(&checks), Status::Fail);
    }

    #[test]
    fn headless_check_login_window_unlocked_and_sleepy_cases() {
        let mut f = HostFacts {
            console: Some(parse_console("")),
            ..HostFacts::default()
        };
        let c = headless_checks(&f);
        assert_eq!(c[0].status, Status::Fail);
        assert!(c[0].fix.as_ref().unwrap().contains("Automatic login"));
        // Unlocked, auto-login on, sleep enabled, nothing holding it: the sleep is the failure.
        f.console = Some(Console {
            user: Some("admin".into()),
            screen_locked: Some(false),
            ..Console::default()
        });
        f.current_user = "admin".into();
        f.autologin = Some("admin".into());
        f.filevault = Some(false);
        f.displays = Some(vec![]);
        f.pmset = Some(parse_pmset_g(" sleep 1\n"));
        let c = headless_checks(&f);
        let sleep = c.iter().find(|c| c.name == "idle sleep").unwrap();
        assert_eq!(sleep.status, Status::Fail);
        assert!(sleep.fix.as_ref().unwrap().contains("keepawake install"));
        assert_eq!(
            c.iter().find(|c| c.name == "display").unwrap().status,
            Status::Warn
        );
        assert!(c.iter().all(|c| c.name != "session owner"));
        // The keepawake agent satisfies it; a different console user is flagged.
        f.keepawake_loaded = true;
        f.current_user = "other".into();
        let c = headless_checks(&f);
        assert_eq!(
            c.iter().find(|c| c.name == "idle sleep").unwrap().status,
            Status::Ok
        );
        assert_eq!(
            c.iter().find(|c| c.name == "session owner").unwrap().status,
            Status::Warn
        );
    }

    #[test]
    fn tune_plans_only_what_differs_and_only_for_offered_settings() {
        let p = parse_pmset_g(PMSET_G);
        let plan = plan_changes(&p);
        let names: Vec<_> = plan.iter().map(|c| c.name).collect();
        assert_eq!(names, ["disksleep", "autorestart"]);
        assert_eq!(plan[0].command(), ["sudo", "pmset", "-a", "disksleep", "0"]);
        let text = render_tune(&plan, &p);
        assert!(text.contains("--only disksleep --only autorestart"));
        assert!(text.contains("Not changed by fluxctl"));
        // A setting the Mac does not list is not planned.
        assert!(plan_changes(&parse_pmset_g(" sleep 0\n")).is_empty());
    }

    #[test]
    fn apply_needs_an_explicit_known_list_and_runs_exactly_it() {
        let plan = plan_changes(&parse_pmset_g(PMSET_G));
        assert!(
            select_changes(&plan, &[])
                .unwrap_err()
                .to_string()
                .contains("--only")
        );
        assert!(
            select_changes(&plan, &["all".into()])
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
        let chosen = select_changes(&plan, &["autorestart".into(), "sleep".into()]).unwrap();
        // `sleep` is already 0: skipped, not run.
        assert_eq!(chosen.len(), 1);
        let fake = Fake::default();
        let ran = apply(&fake, &chosen).unwrap();
        assert_eq!(ran, [["sudo", "pmset", "-a", "autorestart", "1"]]);
        assert_eq!(fake.ran.borrow().len(), 1);
        // A failure stops the list.
        let both = select_changes(&plan, &["disksleep".into(), "autorestart".into()]).unwrap();
        let failing = Fake {
            fail_on: Some("disksleep"),
            ..Fake::default()
        };
        assert!(apply(&failing, &both).is_err());
        assert_eq!(failing.ran.borrow().len(), 1);
    }

    #[test]
    fn keepawake_plist_and_commands() {
        let p = render_keepawake_plist(false);
        assert!(p.contains("<string>/usr/bin/caffeinate</string>\n    <string>-i</string>\n    <string>-m</string>\n    <string>-s</string>\n  </array>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains(&format!("<string>{KEEPAWAKE_LABEL}</string>")));
        assert!(render_keepawake_plist(true).contains("<string>-d</string>"));
        assert_eq!(
            keepawake_plist_path(Path::new("/Users/me")),
            Path::new("/Users/me/Library/LaunchAgents/dev.zyvor.fluxvm.keepawake.plist")
        );
        let c = keepawake_load_commands(501, Path::new("/p.plist"));
        assert_eq!(
            c[0],
            [
                "/bin/launchctl",
                "bootout",
                "gui/501/dev.zyvor.fluxvm.keepawake"
            ]
        );
        assert_eq!(c[1], ["/bin/launchctl", "bootstrap", "gui/501", "/p.plist"]);
    }

    #[test]
    fn keepawake_install_and_uninstall_use_a_temp_home_and_a_fake_launchctl() {
        let home = tempfile::tempdir().unwrap();
        let fake = Fake::default();
        let dry = keepawake_install(&fake, home.path(), 501, false, true).unwrap();
        assert!(dry.contains("would write") && fake.ran.borrow().is_empty());
        assert!(!keepawake_plist_path(home.path()).exists());
        keepawake_install(&fake, home.path(), 501, true, false).unwrap();
        let written = std::fs::read_to_string(keepawake_plist_path(home.path())).unwrap();
        assert!(written.contains("-d"));
        assert_eq!(fake.ran.borrow().len(), 2);
        assert!(fake.ran.borrow()[1].contains(&"bootstrap".to_owned()));
        let out = keepawake_uninstall(&fake, home.path(), 501).unwrap();
        assert!(out.contains("removed"));
        assert!(!keepawake_plist_path(home.path()).exists());
        assert!(
            keepawake_uninstall(&fake, home.path(), 501)
                .unwrap()
                .contains("not installed")
        );
        // A failing bootstrap is an error.
        let bad = Fake {
            fail_on: Some("bootstrap"),
            ..Fake::default()
        };
        assert!(keepawake_install(&bad, home.path(), 501, false, false).is_err());
    }

    #[test]
    fn rosetta_states() {
        let mk = |runner: Option<&str>, marker| RosettaFacts {
            arm64: true,
            runner: runner.map(str::to_owned),
            marker,
        };
        let st = |f: RosettaFacts| rosetta_checks(&f).remove(0);
        assert_eq!(st(mk(Some("installed"), true)).status, Status::Ok);
        let odd = st(mk(Some("installed"), false));
        assert_eq!(odd.status, Status::Warn);
        assert!(odd.detail.contains("unreliable"));
        let missing = st(mk(Some("not-installed"), false));
        assert!(missing.fix.unwrap().contains("install-rosetta"));
        assert_eq!(st(mk(Some("not-supported"), false)).status, Status::Warn);
        assert_eq!(st(mk(None, true)).status, Status::Ok);
        assert_eq!(st(mk(None, false)).status, Status::Warn);
        let intel = RosettaFacts {
            arm64: false,
            ..RosettaFacts::default()
        };
        assert!(st(intel).detail.contains("Apple silicon"));
    }

    #[test]
    fn rosetta_guidance_carries_the_binfmt_line_the_kube_guest_uses() {
        let g = rosetta_guidance();
        assert!(g.contains("mount -t virtiofs rosetta /mnt/rosetta"));
        assert!(g.contains(&crate::kube_host::rosetta_binfmt()));
        assert!(g.contains("rosetta: true"));
    }

    #[test]
    fn command_group_parses() {
        use clap::Parser;
        #[derive(Parser)]
        struct C {
            #[command(subcommand)]
            c: HostCommand,
        }
        assert!(matches!(
            C::parse_from(["x", "keepawake"]).c,
            HostCommand::Keepawake { action: None }
        ));
        let t = C::parse_from(["x", "tune", "--apply", "--only", "sleep", "--only", "womp"]);
        assert!(matches!(t.c, HostCommand::Tune { apply: true, ref only, .. } if only.len() == 2));
        assert!(matches!(
            C::parse_from(["x", "headless-check", "--json"]).c,
            HostCommand::HeadlessCheck { json: true }
        ));
    }
}
