// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl service install | uninstall | status`: run `fluxctl serve` as a launchd LaunchAgent of the logged-in user.
//! Not a system LaunchDaemon: Virtualization.framework restores saved VM state only in an unlocked login session
//! (see deploy/launchd-notes.md).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "dev.zyvor.fluxvm";

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn log_path(home: &Path) -> PathBuf {
    home.join("Library/Logs/fluxvm.log")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The LaunchAgent: `fluxctl [--config FILE] serve`, started at login and restarted if it exits. No token or secret
/// is written into it; the daemon reads its config file.
pub fn render_plist(fluxctl: &Path, config: Option<&Path>, log: &Path) -> String {
    let mut args = vec![fluxctl.display().to_string()];
    if let Some(c) = config {
        args.push("--config".into());
        args.push(c.display().to_string());
    }
    args.push("serve".into());
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    let log = xml_escape(&log.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("running /bin/launchctl (launchd is macOS only)")
}

/// Whether `exe` is a Homebrew install (a path through `Cellar`, as `current_exe` gives for a brew-installed binary).
pub fn is_homebrew_exe(exe: &Path) -> bool {
    exe.components().any(|c| c.as_os_str() == "Cellar")
}

/// The LaunchAgent plist files Homebrew's `brew services` writes for the `fluxvm` formula (the label changed between
/// Homebrew versions).
fn brew_service_plists(home: &Path) -> [PathBuf; 2] {
    let dir = home.join("Library/LaunchAgents");
    [
        dir.join("sh.brew.fluxvm.plist"),
        dir.join("homebrew.mxcl.fluxvm.plist"),
    ]
}

/// Whether something already accepts connections on `listen` (`host:port`). A `listen` that is not a socket address
/// (a name, a malformed value) is not checked.
pub fn port_in_use(listen: &str) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let Ok(addr) = listen.parse::<SocketAddr>() else {
        return false;
    };
    // An unspecified address (0.0.0.0) is reached through loopback.
    let addr = if addr.ip().is_unspecified() {
        SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), addr.port())
    } else {
        addr
    };
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// Refuses an install that would start a second daemon beside one that already runs, unless `force`:
/// a daemon managed by Homebrew, or anything already listening on the configured address. Both end with the new
/// agent exiting on `Address already in use`, after it has already opened the same state directory.
fn guard(home: &Path, exe: &Path, listen: &str, force: bool, own_agent_loaded: bool) -> Result<()> {
    if force {
        return Ok(());
    }
    if is_homebrew_exe(exe) || brew_service_plists(home).iter().any(|p| p.exists()) {
        bail!(
            "FluxVM is managed by Homebrew on this Mac: use `brew services start zyvorai/tap/fluxvm` (and \
             `brew services stop zyvorai/tap/fluxvm` to stop it). A second LaunchAgent would run `fluxctl serve` \
             on the same address and state directory and exit with `Address already in use`. \
             Pass --force to install it anyway."
        );
    }
    if !own_agent_loaded && port_in_use(listen) {
        bail!(
            "something already listens on {listen}: another FluxVM daemon (`curl http://{listen}/healthz`)? Stop it \
             first, or give this one its own `listen` and `state_dir` in the --config file. A second daemon on the \
             same state directory is not safe. Pass --force to install anyway."
        );
    }
    Ok(())
}

/// Whether this agent is already loaded in launchd (a reinstall replaces it, and its own port is not a conflict).
fn own_agent_loaded() -> bool {
    launchctl(&["print", &format!("{}/{LABEL}", domain())])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Writes the plist (replacing an older one) and (re)loads it. Refuses when a daemon is already managed by Homebrew
/// or listening on `listen`, unless `force`.
pub fn install(config: Option<&Path>, listen: &str, force: bool) -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("`fluxctl service` installs a macOS LaunchAgent; on Linux use a systemd unit");
    }
    let home = home()?;
    let real_exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.canonicalize().ok())
        .unwrap_or_default();
    guard(&home, &real_exe, listen, force, own_agent_loaded())?;
    // The Homebrew `opt` path when there is one, so the agent survives `brew upgrade`.
    let exe = crate::stable_exe::stable_exe().context("locating fluxctl")?;
    let config = match config {
        Some(c) => Some(
            c.canonicalize()
                .with_context(|| format!("config file {}", c.display()))?,
        ),
        None => None,
    };
    let plist = plist_path(&home);
    let log = log_path(&home);
    for dir in [plist.parent(), log.parent()].into_iter().flatten() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&plist, render_plist(&exe, config.as_deref(), &log))
        .with_context(|| format!("writing {}", plist.display()))?;
    let target = format!("{}/{LABEL}", domain());
    // Replacing a loaded agent: unload the old definition first; it is fine if none was loaded.
    let _ = launchctl(&["bootout", &target]);
    let out = launchctl(&["bootstrap", &domain(), &plist.display().to_string()])?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(plist)
}

/// Unloads and deletes the agent. VMs keep running only until the daemon's children exit with it.
pub fn uninstall() -> Result<bool> {
    let plist = plist_path(&home()?);
    let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
    match std::fs::remove_file(&plist) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", plist.display())),
    }
}

/// `{installed, loaded, pid, plist, log}` from the plist file and `launchctl print`.
pub fn status() -> Result<serde_json::Value> {
    let home = home()?;
    let plist = plist_path(&home);
    let out = launchctl(&["print", &format!("{}/{LABEL}", domain())])?;
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |key: &str| {
        text.lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix(key)?
                    .trim()
                    .strip_prefix('=')?
                    .trim()
                    .into()
            })
            .map(str::to_owned)
    };
    Ok(serde_json::json!({
        "installed": plist.is_file(),
        "loaded": out.status.success(),
        "state": field("state"),
        "pid": field("pid").and_then(|p| p.parse::<u32>().ok()),
        "last_exit_code": field("last exit code"),
        "plist": plist,
        "log": log_path(&home),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn homebrew_paths_are_recognised() {
        assert!(is_homebrew_exe(Path::new(
            "/opt/homebrew/Cellar/fluxvm/0.4.4/bin/fluxctl"
        )));
        assert!(is_homebrew_exe(Path::new(
            "/usr/local/Cellar/fluxvm/0.4.4/bin/fluxctl"
        )));
        assert!(!is_homebrew_exe(Path::new("/usr/local/bin/fluxctl")));
        assert!(!is_homebrew_exe(Path::new(
            "/Users/me/src/target/release/fluxctl"
        )));
    }

    #[test]
    fn a_listening_address_is_detected_and_a_free_one_is_not() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = l.local_addr().unwrap().to_string();
        assert!(port_in_use(&busy));
        drop(l);
        assert!(!port_in_use(&busy));
        assert!(!port_in_use("not-an-address"));
    }

    #[test]
    fn the_guard_refuses_homebrew_and_busy_ports_unless_forced() {
        let home = std::env::temp_dir().join(format!("fluxvm-guard-{}", std::process::id()));
        std::fs::create_dir_all(home.join("Library/LaunchAgents")).unwrap();
        let free = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let src = Path::new("/Users/me/src/target/release/fluxctl");
        let brew = Path::new("/opt/homebrew/Cellar/fluxvm/0.4.4/bin/fluxctl");

        // A plain build and a free port: allowed.
        assert!(guard(&home, src, &free, false, false).is_ok());
        // Homebrew's binary: refused, with the brew instruction; --force allows it.
        let e = guard(&home, brew, &free, false, false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("brew services start"), "{e}");
        assert!(guard(&home, brew, &free, true, false).is_ok());
        // A brew service plist alone is enough.
        std::fs::write(home.join("Library/LaunchAgents/sh.brew.fluxvm.plist"), "").unwrap();
        assert!(guard(&home, src, &free, false, false).is_err());
        std::fs::remove_file(home.join("Library/LaunchAgents/sh.brew.fluxvm.plist")).unwrap();
        // A busy port: refused, unless this agent already owns it (a reinstall) or --force.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = l.local_addr().unwrap().to_string();
        let e = guard(&home, src, &busy, false, false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("already listens"), "{e}");
        assert!(guard(&home, src, &busy, false, true).is_ok());
        assert!(guard(&home, src, &busy, true, false).is_ok());
        let _ = std::fs::remove_dir_all(&home);
    }

    use super::*;

    #[test]
    fn the_agent_runs_serve_with_the_config_and_escapes_paths() {
        let p = render_plist(
            Path::new("/opt/homebrew/bin/fluxctl"),
            Some(Path::new("/Users/a&b/fluxvm.toml")),
            Path::new("/Users/a&b/Library/Logs/fluxvm.log"),
        );
        assert!(p.contains("<string>/opt/homebrew/bin/fluxctl</string>\n    <string>--config</string>\n    <string>/Users/a&amp;b/fluxvm.toml</string>\n    <string>serve</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
        assert!(p.contains(&format!("<string>{LABEL}</string>")));
        assert!(!p.contains("a&b"));
        let bare = render_plist(Path::new("/f"), None, Path::new("/l"));
        assert!(!bare.contains("--config"));
        assert_eq!(
            plist_path(Path::new("/Users/me")),
            Path::new("/Users/me/Library/LaunchAgents/dev.zyvor.fluxvm.plist")
        );
    }
}
