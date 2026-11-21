// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! A launchd LaunchAgent that keeps the API tunnel (or, for a multi-server cluster, the failover forwarder) of one cluster
//! up: `fluxctl kube apple tunnel NAME --foreground`, started at login and restarted whenever it exits (the guests'
//! addresses are looked up again on each start). A LaunchAgent, not a daemon: the Virtualization framework and the user's ssh
//! identity belong to the logged-in session. Nothing secret is written into the plist.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub fn label(cluster: &str) -> String {
    format!("dev.zyvor.fluxvm.k3s-tunnel.{cluster}")
}

pub fn plist_path(home: &Path, cluster: &str) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{}.plist", label(cluster)))
}

pub fn log_path(home: &Path, cluster: &str) -> PathBuf {
    home.join("Library/Logs")
        .join(format!("fluxvm-k3s-tunnel-{cluster}.log"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The plist. `state_root` is passed on when the journals live somewhere other than the default.
pub fn render_plist(exe: &Path, cluster: &str, log: &Path, state_root: Option<&Path>) -> String {
    let args: String = [
        exe.display().to_string(),
        "kube".into(),
        "apple".into(),
        "tunnel".into(),
        cluster.to_owned(),
        "--foreground".into(),
    ]
    .iter()
    .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
    .collect();
    let env = state_root
        .map(|r| {
            format!(
                "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>FLUXVM_APPLE_K3S_HOME</key><string>{}</string>\n  </dict>\n",
                xml_escape(&r.display().to_string())
            )
        })
        .unwrap_or_default();
    let log = xml_escape(&log.display().to_string());
    let label = label(cluster);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
{env}  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

/// How `launchctl` is run, so installing can be tested without launchd.
pub type Launchctl<'a> = &'a dyn Fn(&[&str]) -> Result<std::process::Output>;

pub fn real_launchctl(args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("running /bin/launchctl (launchd is macOS only)")
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

/// Writes the plist (replacing an older one) and (re)loads it.
pub fn install(
    home: &Path,
    exe: &Path,
    cluster: &str,
    state_root: Option<&Path>,
    launchctl: Launchctl,
) -> Result<PathBuf> {
    let plist = plist_path(home, cluster);
    let log = log_path(home, cluster);
    for dir in [plist.parent(), log.parent()].into_iter().flatten() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&plist, render_plist(exe, cluster, &log, state_root))
        .with_context(|| format!("writing {}", plist.display()))?;
    let target = format!("{}/{}", domain(), label(cluster));
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

/// Unloads and deletes the agent. True when there was one.
pub fn uninstall(home: &Path, cluster: &str, launchctl: Launchctl) -> Result<bool> {
    let _ = launchctl(&["bootout", &format!("{}/{}", domain(), label(cluster))]);
    match std::fs::remove_file(plist_path(home, cluster)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("removing the LaunchAgent"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    fn ok() -> std::process::Output {
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: vec![],
            stderr: vec![],
        }
    }

    #[test]
    fn plist_runs_the_foreground_tunnel_and_keeps_it_alive_without_secrets() {
        let p = render_plist(
            Path::new("/opt/homebrew/bin/fluxctl"),
            "dev",
            Path::new("/Users/u/Library/Logs/x.log"),
            None,
        );
        assert!(p.contains("dev.zyvor.fluxvm.k3s-tunnel.dev"));
        for a in ["kube", "apple", "tunnel", "dev", "--foreground"] {
            assert!(p.contains(&format!("<string>{a}</string>")), "{a}");
        }
        assert!(p.contains("<key>KeepAlive</key><true/>") && p.contains("<key>RunAtLoad</key><true/>"));
        assert!(!p.to_lowercase().contains("token") && !p.contains("EnvironmentVariables"));
        let q = render_plist(Path::new("/f"), "dev", Path::new("/l"), Some(Path::new("/a&b")));
        assert!(q.contains("FLUXVM_APPLE_K3S_HOME") && q.contains("/a&amp;b"));
    }

    #[test]
    fn install_unloads_the_old_agent_then_loads_the_new_one_and_uninstall_removes_it() {
        let d = tempfile::tempdir().unwrap();
        let calls: RefCell<Vec<String>> = RefCell::default();
        let run = |a: &[&str]| -> Result<std::process::Output> {
            calls.borrow_mut().push(a[0].to_owned());
            Ok(ok())
        };
        let plist = install(d.path(), Path::new("/f"), "dev", None, &run).unwrap();
        assert!(plist.is_file());
        assert_eq!(*calls.borrow(), ["bootout", "bootstrap"]);
        assert!(uninstall(d.path(), "dev", &run).unwrap());
        assert!(!plist.exists());
        assert!(!uninstall(d.path(), "dev", &run).unwrap());
    }

    #[test]
    fn a_failing_bootstrap_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let run = |a: &[&str]| -> Result<std::process::Output> {
            let mut o = ok();
            if a[0] == "bootstrap" {
                o.status = std::process::ExitStatus::from_raw(1 << 8);
                o.stderr = b"Bootstrap failed: 5".to_vec();
            }
            Ok(o)
        };
        let e = install(d.path(), Path::new("/f"), "dev", None, &run).unwrap_err();
        assert!(e.to_string().contains("Bootstrap failed"));
    }
}
