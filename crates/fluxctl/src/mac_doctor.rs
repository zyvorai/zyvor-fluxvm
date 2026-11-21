// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac doctor`: a read-only preflight for the macOS-guest workflow. Every limit in docs/macos-containers.md that
//! otherwise fails late (a 409, a 503, a 20 GB copy instead of a clone, a key login that does not work) is checked up front
//! and each non-ok check carries the exact command or setting that fixes it.
//!
//! Facts are gathered from the OS by [`gather`] and judged by the pure [`evaluate`], so every rule is unit tested with
//! made-up facts. The probes themselves (`sw_vers`, `df`, `diskutil`, `ioreg`, `fdesetup`) are macOS tools; their parsing is
//! tested on sample text. Only `ioreg`, `fdesetup`, `df` and `diskutil` parsing was compared with a real Mac's output.

use crate::vz_doctor::{Check, Status};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Apple's limit on concurrent macOS guests per Mac.
pub const MAX_SLOTS: usize = 2;
/// Free GiB on the store volume below which a template (a 20+ GB disk) plus a clone will not fit.
const FAIL_FREE_GB: f64 = 25.0;
const WARN_FREE_GB: f64 = 80.0;

/// The login-session state of the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// A user is on the console and the screen is not locked.
    Unlocked,
    /// A user is on the console and the screen is locked.
    Locked,
    /// Nobody is logged in on the console (the login window).
    NoConsoleUser,
}

/// What the daemon says, when it could be asked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DaemonFacts {
    /// `/healthz` answered.
    pub reachable: bool,
    /// Active (running, paused or starting) `fluxctl mac` instances; `None` when the list could not be read.
    pub active_instances: Option<usize>,
    /// The workspace directory of one instance, to learn which volume clones land on.
    pub workspace: Option<PathBuf>,
}

/// Everything [`evaluate`] needs. `None` means the probe could not run (reported, never guessed).
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub is_macos: bool,
    pub arch: String,
    pub os_version: String,
    pub store: PathBuf,
    /// `FLUXVM_MAC_STORE` is set (so the location was chosen on purpose).
    pub store_from_env: bool,
    /// The store directory exists or could be created and written.
    pub store_writable: Option<Result<(), String>>,
    pub store_free_gb: Option<f64>,
    /// Device id (`st_dev`) of the store, and of the daemon's instance directory.
    pub store_dev: Option<u64>,
    pub instance_dev: Option<u64>,
    /// `Some("apfs")`, `Some("hfs")`, ... for the volume holding the store.
    pub store_fs: Option<String>,
    pub session: Option<Session>,
    /// `None` when no daemon address is configured at all.
    pub daemon: Option<DaemonFacts>,
    pub ssh_keys: Vec<String>,
    pub filevault_on: Option<bool>,
}

fn check(
    name: &'static str,
    status: Status,
    detail: impl Into<String>,
    fix: Option<&str>,
) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
        fix: fix.map(str::to_owned),
    }
}

fn major(version: &str) -> Option<u32> {
    version.split('.').next()?.trim().parse().ok()
}

/// Judges the facts. The order is the order the checks are shown.
pub fn evaluate(f: &Facts) -> Vec<Check> {
    let mut out = Vec::new();
    if !f.is_macos {
        out.push(check(
            "platform",
            Status::Fail,
            "macOS guests run only on a Mac with Apple silicon",
            Some("run `fluxctl mac` on a Mac; to reach a Mac from here use `fluxctl mac fleet` or `--server http://MAC:7788`"),
        ));
        return out;
    }
    if f.arch == "arm64" || f.arch == "aarch64" {
        out.push(check(
            "apple silicon",
            Status::Ok,
            format!("arch {}", f.arch),
            None,
        ));
    } else {
        out.push(check(
            "apple silicon",
            Status::Fail,
            format!("arch {} (Intel Macs cannot run macOS guests here)", f.arch),
            Some("use an Apple silicon Mac"),
        ));
    }
    match major(&f.os_version) {
        Some(m) if m >= 14 => out.push(check(
            "macOS version",
            Status::Ok,
            format!("macOS {}", f.os_version),
            None,
        )),
        Some(_) => out.push(check(
            "macOS version",
            Status::Fail,
            format!("macOS {} is older than 14", f.os_version),
            Some("System Settings > General > Software Update (the vz backend needs macOS 14 or newer)"),
        )),
        None => out.push(check(
            "macOS version",
            Status::Warn,
            format!("could not read the version ({:?})", f.os_version),
            Some("run `sw_vers -productVersion`"),
        )),
    }
    out.push(store_location(f));
    out.extend(disk_checks(f));
    out.push(session_check(f.session));
    out.extend(daemon_checks(f));
    out.push(ssh_check(&f.ssh_keys));
    out.push(match f.filevault_on {
        Some(true) => check(
            "FileVault (host)",
            Status::Warn,
            "FileVault is on: auto-login is unavailable, so after a reboot nobody is logged in and guests cannot start until someone logs in",
            Some("attended Mac: ignore this. Headless Mac: System Settings > Privacy & Security > FileVault > Turn Off, then System Settings > Users & Groups > Automatically log in as"),
        ),
        Some(false) => check("FileVault (host)", Status::Ok, "off", None),
        None => check(
            "FileVault (host)",
            Status::Warn,
            "could not read the state",
            Some("run `fdesetup status`"),
        ),
    });
    out
}

fn store_location(f: &Facts) -> Check {
    let origin = if f.store_from_env {
        "from FLUXVM_MAC_STORE"
    } else {
        "default; set FLUXVM_MAC_STORE to move it"
    };
    match &f.store_writable {
        Some(Ok(())) => check(
            "store location",
            Status::Ok,
            format!("{} ({origin})", f.store.display()),
            None,
        ),
        Some(Err(e)) => check(
            "store location",
            Status::Fail,
            format!("{} is not usable: {e}", f.store.display()),
            Some(
                "fix the permissions, or point FLUXVM_MAC_STORE at a writable directory on an APFS volume",
            ),
        ),
        None => check(
            "store location",
            Status::Warn,
            format!("{} (not probed)", f.store.display()),
            None,
        ),
    }
}

fn disk_checks(f: &Facts) -> Vec<Check> {
    let mut out = Vec::new();
    match f.store_free_gb {
        Some(g) if g < FAIL_FREE_GB => out.push(check(
            "free disk",
            Status::Fail,
            format!("{g:.0} GiB free on the store volume; one template is 20+ GB and a copy needs the same again"),
            Some("free space (`fluxctl mac system df`, `fluxctl mac system prune --all`), or move the store with FLUXVM_MAC_STORE"),
        )),
        Some(g) if g < WARN_FREE_GB => out.push(check(
            "free disk",
            Status::Warn,
            format!("{g:.0} GiB free on the store volume; a few templates and instances will not fit"),
            Some("`fluxctl mac system df` shows what is reclaimable; `fluxctl mac system prune`"),
        )),
        Some(g) => out.push(check("free disk", Status::Ok, format!("{g:.0} GiB free"), None)),
        None => out.push(check(
            "free disk",
            Status::Warn,
            "could not read free space",
            Some("run `df -h` on the store directory"),
        )),
    }
    match f.store_fs.as_deref() {
        Some("apfs") => out.push(check("store filesystem", Status::Ok, "APFS (clones are instant)", None)),
        Some(other) => out.push(check(
            "store filesystem",
            Status::Fail,
            format!("{other}: not APFS, so `import` and every clone is a full 20+ GB copy"),
            Some("move the store to an APFS volume: FLUXVM_MAC_STORE=/Volumes/<apfs-volume>/mac-images"),
        )),
        None => out.push(check(
            "store filesystem",
            Status::Warn,
            "could not tell the filesystem",
            Some("run `diskutil info <store directory>`"),
        )),
    }
    match (f.store_dev, f.instance_dev) {
        (Some(a), Some(b)) if a == b => out.push(check(
            "clone volume",
            Status::Ok,
            "the store and the daemon's instance directory are on one volume (clones are instant)",
            None,
        )),
        (Some(_), Some(_)) => out.push(check(
            "clone volume",
            Status::Fail,
            "the store and the daemon's instance directory are on different volumes: every `mac run` copies the whole disk (20+ GB, minutes)",
            Some("put both on one APFS volume: move the store (FLUXVM_MAC_STORE) or the daemon's FLUXVM_STATE_DIR"),
        )),
        _ => out.push(check(
            "clone volume",
            Status::Warn,
            "could not compare the store's volume with the daemon's instance directory (no instance exists yet, or FLUXVM_STATE_DIR is unset)",
            Some("set FLUXVM_STATE_DIR to the daemon's state directory, or re-run `fluxctl mac doctor` after the first `mac run`"),
        )),
    }
    out
}

fn session_check(s: Option<Session>) -> Check {
    match s {
        Some(Session::Unlocked) => check(
            "login session",
            Status::Ok,
            "a user is logged in and the screen is unlocked",
            None,
        ),
        Some(Session::Locked) => check(
            "login session",
            Status::Fail,
            "the screen is locked: a macOS guest cannot start or restore from a locked session (the daemon answers 503)",
            Some(
                "unlock the Mac. Headless: System Settings > Lock Screen > set \"Require password after screen saver begins or display is turned off\" to Never, and use an auto-login user",
            ),
        ),
        Some(Session::NoConsoleUser) => check(
            "login session",
            Status::Fail,
            "nobody is logged in on the console: macOS guests need a logged-in user session",
            Some(
                "log in at the console, or set System Settings > Users & Groups > Automatically log in as (needs FileVault off)",
            ),
        ),
        None => check(
            "login session",
            Status::Warn,
            "could not read the session state",
            Some("run `ioreg -n Root -d1 | grep CGSSessionScreenIsLocked`"),
        ),
    }
}

fn daemon_checks(f: &Facts) -> Vec<Check> {
    let mut out = Vec::new();
    let Some(d) = &f.daemon else {
        out.push(check(
            "daemon",
            Status::Warn,
            "no daemon address configured (run, ps, build and exec need one; image commands do not)",
            Some("pass --server http://127.0.0.1:7788 (and --server-token), or `fluxctl context add`; start it with `fluxvm serve`"),
        ));
        return out;
    };
    if !d.reachable {
        out.push(check(
            "daemon",
            Status::Fail,
            "the daemon did not answer /healthz",
            Some("start it (`fluxvm serve`, or `brew services start fluxvm`) and check --server / --server-token / FLUXVM_URL"),
        ));
        return out;
    }
    out.push(check("daemon", Status::Ok, "answers /healthz", None));
    match d.active_instances {
        Some(n) if n >= MAX_SLOTS => out.push(check(
            "macOS VM slots",
            Status::Warn,
            format!("{n} of {MAX_SLOTS} in use: `mac run` fails with 409 until one stops"),
            Some("`fluxctl mac ps` then `fluxctl mac rm NAME` (or `mac stop NAME`); or use `mac run --queue` to wait for a slot"),
        )),
        Some(n) => out.push(check(
            "macOS VM slots",
            Status::Ok,
            format!("{n} of {MAX_SLOTS} in use by `fluxctl mac` instances (other macOS VMs on this Mac also count)"),
            None,
        )),
        None => out.push(check(
            "macOS VM slots",
            Status::Warn,
            "could not list instances",
            Some("run `fluxctl mac ps -a`"),
        )),
    }
    out
}

fn ssh_check(keys: &[String]) -> Check {
    if keys.is_empty() {
        check(
            "ssh key",
            Status::Warn,
            "no key in ~/.ssh: `run`, `exec`, `cp` and `build` reach the guest over SSH",
            Some(
                "ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519, and put the .pub in the template's ~/.ssh/authorized_keys before you import it",
            ),
        )
    } else {
        check(
            "ssh key",
            Status::Ok,
            format!(
                "{} (the matching public key must be in the guest's authorized_keys)",
                keys.join(", ")
            ),
            None,
        )
    }
}

/// `mac doctor --json`: the same document as `vz doctor --json`.
pub fn render_json(checks: &[Check]) -> serde_json::Value {
    crate::vz_doctor::render_json(checks)
}

// ---- probes ---------------------------------------------------------------------------------------------------------------

fn cmd_out(program: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(program).args(args).output().ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// The nearest existing ancestor of `p` (the store may not exist yet).
fn existing(p: &Path) -> Option<&Path> {
    let mut d = p;
    while !d.exists() {
        d = d.parent()?;
    }
    Some(d)
}

/// Free GiB and mount point from `df -Pk` output (`df -Pk path`).
pub fn parse_df(text: &str) -> Option<(f64, String)> {
    let line = text.lines().nth(1)?;
    // POSIX columns: filesystem 1024-blocks used available capacity mount (the mount point may hold spaces).
    let rest: Vec<&str> = line.split_whitespace().collect();
    let kb: f64 = rest.get(3)?.parse().ok()?;
    let mount = rest.get(5..)?.join(" ");
    Some((kb / 1024.0 / 1024.0, mount))
}

/// The filesystem name from `diskutil info` text (`Type (Bundle):   apfs`).
pub fn parse_diskutil_fs(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let l = l.trim();
        l.strip_prefix("Type (Bundle):")
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty())
    })
}

/// The session state from `ioreg -n Root -d1` text.
pub fn parse_session(text: &str) -> Session {
    let on_console = text.contains("\"kCGSSessionOnConsoleKey\"=Yes");
    if !text.contains("IOConsoleUsers") || !on_console {
        Session::NoConsoleUser
    } else if text.contains("\"CGSSessionScreenIsLocked\"=Yes") {
        Session::Locked
    } else {
        Session::Unlocked
    }
}

fn dev_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(existing(p)?).ok().map(|m| m.dev())
}

fn probe_store(store: &Path) -> Result<(), String> {
    std::fs::create_dir_all(store).map_err(|e| e.to_string())?;
    let probe = store.join(format!(".doctor-{}", std::process::id()));
    std::fs::write(&probe, b"x").map_err(|e| e.to_string())?;
    std::fs::remove_file(&probe).map_err(|e| e.to_string())
}

/// Reads this machine. `daemon` is supplied by the async caller (it needs the HTTP client).
pub fn gather(store: &Path, daemon: Option<DaemonFacts>) -> Facts {
    let is_macos = cfg!(target_os = "macos");
    let mut f = Facts {
        is_macos,
        arch: std::env::consts::ARCH.to_owned(),
        store: store.to_path_buf(),
        store_from_env: std::env::var_os("FLUXVM_MAC_STORE").is_some_and(|v| !v.is_empty()),
        daemon,
        ..Facts::default()
    };
    if !is_macos {
        return f;
    }
    f.os_version = cmd_out("/usr/bin/sw_vers", &["-productVersion"]).unwrap_or_default();
    f.store_writable = Some(probe_store(store));
    if let Some(d) = existing(store).and_then(|d| d.to_str().map(str::to_owned)) {
        if let Some((gb, mount)) = cmd_out("/bin/df", &["-Pk", &d]).and_then(|t| parse_df(&t)) {
            f.store_free_gb = Some(gb);
            f.store_fs = cmd_out("/usr/sbin/diskutil", &["info", &mount])
                .and_then(|t| parse_diskutil_fs(&t));
        }
    }
    f.store_dev = dev_of(store);
    let state = std::env::var_os("FLUXVM_STATE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| f.daemon.as_ref().and_then(|d| d.workspace.clone()));
    f.instance_dev = state.as_deref().and_then(dev_of);
    f.session = cmd_out("/usr/sbin/ioreg", &["-n", "Root", "-d1"]).map(|t| parse_session(&t));
    f.filevault_on =
        cmd_out("/usr/bin/fdesetup", &["status"]).map(|t| t.contains("FileVault is On"));
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        // A user's own key (the public half), or the one `fluxctl run` generated.
        f.ssh_keys = ["id_ed25519", "id_ecdsa", "id_rsa"]
            .iter()
            .filter(|n| home.join(".ssh").join(format!("{n}.pub")).exists())
            .map(|n| format!("~/.ssh/{n}"))
            .chain(
                home.join(".ssh/fluxvm_ed25519")
                    .exists()
                    .then(|| "~/.ssh/fluxvm_ed25519".to_string()),
            )
            .collect();
    }
    f
}

/// Ask the daemon what the preflight needs. Never fails: an unreachable daemon is a fact.
pub async fn probe_daemon(r: &crate::remote::Remote) -> DaemonFacts {
    let reachable = matches!(r.get_raw("/healthz").await, Ok((st, _)) if st.is_success());
    if !reachable {
        return DaemonFacts::default();
    }
    let list = r
        .call(
            reqwest::Method::GET,
            &format!(
                "/v1/vms?label={}",
                crate::remote::encode_query(crate::mac_cmd::INSTANCE_SELECTOR)
            ),
            None,
        )
        .await
        .ok();
    let items: Vec<serde_json::Value> = list
        .as_ref()
        .map(|v| {
            crate::mac_cmd::filter_instances(&v["items"])
                .into_iter()
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    DaemonFacts {
        reachable,
        active_instances: list
            .is_some()
            .then(|| items.iter().filter(|v| is_active(v)).count()),
        workspace: items
            .iter()
            .find_map(|v| v["workspace"].as_str().map(PathBuf::from)),
    }
}

/// A VM that holds a macOS slot.
pub fn is_active(vm: &serde_json::Value) -> bool {
    matches!(
        crate::mac_inspect::status(vm),
        "running" | "paused" | "creating" | "starting"
    )
}

/// `fluxctl mac doctor`.
pub async fn run(json: bool, remote: Option<crate::remote::Remote>) -> anyhow::Result<()> {
    let store = crate::mac_store::default_root()?;
    let daemon = match &remote {
        Some(r) => Some(probe_daemon(r).await),
        None => None,
    };
    let checks = evaluate(&gather(&store, daemon));
    if json {
        println!("{}", serde_json::to_string_pretty(&render_json(&checks))?);
    } else {
        print!("{}", crate::vz_doctor::render_text(&checks));
    }
    anyhow::ensure!(
        crate::vz_doctor::worst(&checks) != Status::Fail,
        "mac doctor found problems (see the fix lines above)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Facts {
        Facts {
            is_macos: true,
            arch: "arm64".into(),
            os_version: "27.2".into(),
            store: "/Users/me/.fluxvm/mac-images".into(),
            store_from_env: false,
            store_writable: Some(Ok(())),
            store_free_gb: Some(400.0),
            store_dev: Some(16),
            instance_dev: Some(16),
            store_fs: Some("apfs".into()),
            session: Some(Session::Unlocked),
            daemon: Some(DaemonFacts {
                reachable: true,
                active_instances: Some(1),
                workspace: None,
            }),
            ssh_keys: vec!["~/.ssh/id_ed25519".into()],
            filevault_on: Some(false),
        }
    }

    fn find<'a>(c: &'a [Check], name: &str) -> &'a Check {
        c.iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no check {name}"))
    }

    #[test]
    fn healthy_mac_is_all_ok() {
        let c = evaluate(&good());
        for ch in &c {
            assert_eq!(ch.status, Status::Ok, "{}: {}", ch.name, ch.detail);
            assert!(ch.fix.is_none());
        }
        assert_eq!(c.len(), 11);
    }

    #[test]
    fn every_non_ok_check_has_a_fix() {
        let mut f = good();
        f.arch = "x86_64".into();
        f.os_version = "13.1".into();
        f.store_writable = Some(Err("read-only".into()));
        f.store_free_gb = Some(10.0);
        f.store_fs = Some("hfs".into());
        f.instance_dev = Some(99);
        f.session = Some(Session::Locked);
        f.daemon = Some(DaemonFacts::default());
        f.ssh_keys.clear();
        f.filevault_on = Some(true);
        let c = evaluate(&f);
        for ch in c.iter().filter(|c| c.status != Status::Ok) {
            assert!(
                ch.fix.as_deref().is_some_and(|s| !s.is_empty()),
                "{} has no fix",
                ch.name
            );
        }
        for n in [
            "apple silicon",
            "macOS version",
            "store location",
            "free disk",
            "store filesystem",
            "clone volume",
            "login session",
            "daemon",
        ] {
            assert_eq!(find(&c, n).status, Status::Fail, "{n}");
        }
        assert_eq!(find(&c, "ssh key").status, Status::Warn);
        assert_eq!(find(&c, "FileVault (host)").status, Status::Warn);
    }

    #[test]
    fn non_macos_stops_at_one_check() {
        let c = evaluate(&Facts::default());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].status, Status::Fail);
    }

    #[test]
    fn slots_warn_at_two_and_ok_at_one() {
        let mut f = good();
        f.daemon.as_mut().unwrap().active_instances = Some(2);
        let c = evaluate(&f);
        let s = find(&c, "macOS VM slots");
        assert_eq!(s.status, Status::Warn);
        assert!(s.fix.as_deref().unwrap().contains("--queue"));
        f.daemon.as_mut().unwrap().active_instances = Some(0);
        assert_eq!(find(&evaluate(&f), "macOS VM slots").status, Status::Ok);
        f.daemon.as_mut().unwrap().active_instances = None;
        assert_eq!(find(&evaluate(&f), "macOS VM slots").status, Status::Warn);
    }

    #[test]
    fn no_daemon_address_is_a_warning_not_a_failure() {
        let mut f = good();
        f.daemon = None;
        let c = evaluate(&f);
        assert_eq!(find(&c, "daemon").status, Status::Warn);
        assert!(c.iter().all(|c| c.name != "macOS VM slots"));
    }

    #[test]
    fn free_disk_thresholds() {
        let mut f = good();
        for (gb, st) in [
            (24.0, Status::Fail),
            (60.0, Status::Warn),
            (120.0, Status::Ok),
        ] {
            f.store_free_gb = Some(gb);
            assert_eq!(find(&evaluate(&f), "free disk").status, st, "{gb}");
        }
    }

    #[test]
    fn unknown_volumes_warn() {
        let mut f = good();
        f.instance_dev = None;
        f.store_fs = None;
        let c = evaluate(&f);
        assert_eq!(find(&c, "clone volume").status, Status::Warn);
        assert_eq!(find(&c, "store filesystem").status, Status::Warn);
    }

    #[test]
    fn env_store_is_named() {
        let mut f = good();
        f.store_from_env = true;
        assert!(
            find(&evaluate(&f), "store location")
                .detail
                .contains("FLUXVM_MAC_STORE")
        );
    }

    #[test]
    fn session_text_is_parsed() {
        let unlocked = "+-o Root  <class IORegistryEntry>\n    | \"IOConsoleUsers\" = ({\"kCGSSessionUserNameKey\"=\"me\",\"kCGSSessionOnConsoleKey\"=Yes,\"CGSSessionScreenIsLocked\"=No})";
        let locked = unlocked.replace("ScreenIsLocked\"=No", "ScreenIsLocked\"=Yes");
        let off = "+-o Root  <class IORegistryEntry>\n    | \"IOKitBuildVersion\" = \"x\"";
        assert_eq!(parse_session(unlocked), Session::Unlocked);
        assert_eq!(parse_session(&locked), Session::Locked);
        assert_eq!(parse_session(off), Session::NoConsoleUser);
    }

    #[test]
    fn df_and_diskutil_text_are_parsed() {
        let df = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s5 965492904 826453208 76587640 92% /System/Volumes/Data";
        let (gb, mount) = parse_df(df).unwrap();
        assert!((gb - 73.0).abs() < 1.0, "{gb}");
        assert_eq!(mount, "/System/Volumes/Data");
        let spaced = "h\n/dev/disk4s1 100 1 1048576 1% /Volumes/My Disk";
        assert_eq!(parse_df(spaced).unwrap().1, "/Volumes/My Disk");
        assert!(parse_df("header only").is_none());
        let du = "   Volume Name:  Data\n   File System Personality:   APFS\n   Type (Bundle):             apfs\n";
        assert_eq!(parse_diskutil_fs(du).as_deref(), Some("apfs"));
        assert_eq!(parse_diskutil_fs("nothing"), None);
    }

    #[test]
    fn active_states_hold_a_slot() {
        for (st, hold) in [
            ("running", true),
            ("paused", true),
            ("creating", true),
            ("stopped", false),
            ("failed", false),
        ] {
            assert_eq!(is_active(&serde_json::json!({"status": st})), hold, "{st}");
        }
    }

    #[test]
    fn json_has_ok_and_checks() {
        let mut f = good();
        let j = render_json(&evaluate(&f));
        assert_eq!(j["ok"], true);
        f.session = Some(Session::Locked);
        let j = render_json(&evaluate(&f));
        assert_eq!(j["ok"], false);
        assert!(
            j["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["status"] == "fail" && c["fix"].is_string())
        );
    }

    #[test]
    fn store_probe_reports_unwritable_paths() {
        let d = tempfile::tempdir().unwrap();
        assert!(probe_store(&d.path().join("a/b")).is_ok());
        let file = d.path().join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(probe_store(&file.join("sub")).is_err());
    }
}
