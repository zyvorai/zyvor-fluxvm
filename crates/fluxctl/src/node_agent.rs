// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl node-agent install | uninstall | status`: run `fluxvm-agent node` (the fleet heartbeat that registers this Mac
//! with a central registry, or with Kairon's tooling) as its own LaunchAgent, next to the daemon's (`fluxctl service`).
//!
//! Same reasoning as the daemon: a LaunchAgent in the logged-in user's session, not a LaunchDaemon (see
//! deploy/launchd-notes.md). launchd has no start ordering, which is fine: the agent keeps heartbeating and tolerates a
//! daemon that is not up yet. The bearer token is never written into the plist: pass `--token-file`.

use crate::docker_host as dh;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const LABEL: &str = "dev.zyvor.fluxvm-agent";

#[derive(clap::Subcommand)]
pub enum NodeAgentCommand {
    /// Write ~/Library/LaunchAgents/dev.zyvor.fluxvm-agent.plist for `fluxvm-agent node` and load it.
    Install(NodeAgentArgs),
    /// Unload and delete it.
    Uninstall,
    /// Whether it is installed and loaded, its pid and last exit code, as JSON.
    Status,
}

#[derive(clap::Args, Clone, Debug)]
pub struct NodeAgentArgs {
    /// This node's name in the fleet (lowercase letters, digits, dashes and dots).
    #[arg(long)]
    pub name: String,
    /// Base URL of the central registry (`fluxvm-agent central`); a comma-separated list for HA.
    #[arg(long)]
    pub central: String,
    /// How the agent reaches the local daemon.
    #[arg(long, default_value = "http://127.0.0.1:7788")]
    pub fluxvm_url: String,
    /// How the registry reaches this Mac's daemon, when it is not on this Mac. The daemon must then listen on a reachable
    /// address with auth on.
    #[arg(long)]
    pub advertise_url: Option<String>,
    /// File holding the bearer or join token (`fvjt_...`). Never put the token itself on the command line: it would end
    /// up in the plist.
    #[arg(long)]
    pub token_file: Option<PathBuf>,
    /// Extra label as `key=value`, repeatable. Overrides an automatic label with the same key.
    #[arg(long = "label", value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    /// Do not add the automatic labels (`kairon.zyvor.dev/backend.vz`, `.../chip`, `.../memory-gib`).
    #[arg(long)]
    pub no_auto_labels: bool,
    /// Do not write this Mac's overlay.json from the fleet's allocation.
    #[arg(long)]
    pub no_overlay_sync: bool,
    /// Where the node keeps its id and credential. Default: ~/Library/Application Support/FluxVM/agent.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
}

fn parse_label(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_owned(), v.to_owned())),
        _ => Err(format!("expected key=value, got {s:?}")),
    }
}

impl NodeAgentArgs {
    pub fn validate(&self) -> Result<()> {
        let ok_name = !self.name.is_empty()
            && self.name.len() <= 63
            && !self.name.starts_with(['-', '.'])
            && self
                .name
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.');
        if !ok_name {
            bail!("--name must be lowercase letters, digits, dashes and dots (at most 63)");
        }
        for u in self.central.split(',') {
            if !(u.starts_with("http://") || u.starts_with("https://")) {
                bail!("--central must be an http(s):// URL (or a comma-separated list), got {u:?}");
            }
        }
        if !(self.fluxvm_url.starts_with("http://") || self.fluxvm_url.starts_with("https://")) {
            bail!("--fluxvm-url must be an http(s):// URL");
        }
        if let Some(t) = &self.token_file
            && !t.is_file()
        {
            bail!("--token-file {} does not exist", t.display());
        }
        Ok(())
    }
}

/// The labels every Mac node reports: the backend, the chip (`Apple M4 Max` becomes `m4-max`) and the memory in GiB.
/// These are the conventions of deploy/kairon-node-mac.md.
pub fn auto_labels(brand: Option<&str>, mem_bytes: Option<u64>) -> Vec<(String, String)> {
    let mut v = vec![("kairon.zyvor.dev/backend.vz".to_owned(), "true".to_owned())];
    if let Some(b) = brand {
        let chip: String = b
            .trim()
            .trim_start_matches("Apple ")
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join("-");
        if !chip.is_empty() {
            v.push(("kairon.zyvor.dev/chip".to_owned(), chip));
        }
    }
    if let Some(m) = mem_bytes.filter(|m| *m > 0) {
        v.push((
            "kairon.zyvor.dev/memory-gib".to_owned(),
            (m / (1 << 30)).to_string(),
        ));
    }
    v
}

/// `auto` first, then `user`; a user label replaces an automatic one with the same key.
pub fn merge_labels(
    auto: Vec<(String, String)>,
    user: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = auto
        .into_iter()
        .filter(|(k, _)| !user.iter().any(|(uk, _)| uk == k))
        .collect();
    out.extend(user.iter().cloned());
    out
}

/// The command line of the LaunchAgent: `fluxvm-agent node ...`.
pub fn render_args(
    agent: &Path,
    a: &NodeAgentArgs,
    labels: &[(String, String)],
    state_dir: &Path,
) -> Vec<String> {
    let mut v = vec![
        agent.display().to_string(),
        "node".into(),
        "--name".into(),
        a.name.clone(),
        "--central".into(),
        a.central.clone(),
        "--fluxvm-url".into(),
        a.fluxvm_url.clone(),
        "--state-dir".into(),
        state_dir.display().to_string(),
    ];
    if let Some(u) = &a.advertise_url {
        v.extend(["--advertise-url".into(), u.clone()]);
    }
    if let Some(t) = &a.token_file {
        v.extend(["--token-file".into(), t.display().to_string()]);
    }
    for (k, val) in labels {
        v.extend(["--label".into(), format!("{k}={val}")]);
    }
    if a.no_overlay_sync {
        v.push("--no-overlay-sync".into());
    }
    v
}

fn paths(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (
        home.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
        home.join("Library/Logs/fluxvm-agent.log"),
        home.join("Library/Application Support/FluxVM/agent"),
    )
}

/// `fluxvm-agent` next to this `fluxctl` (the package puts them side by side), by the version-free Homebrew path.
fn agent_binary() -> Result<PathBuf> {
    let exe = crate::stable_exe::stable_exe().context("finding fluxctl's own path")?;
    let agent = exe.with_file_name("fluxvm-agent");
    if !agent.is_file() {
        bail!(
            "{} not found next to fluxctl: this install does not include the node agent (the macOS package ships it from the release after 0.4.21); build it with `cargo build --release -p fluxvm-agent` and put it beside fluxctl",
            agent.display()
        );
    }
    Ok(agent)
}

#[cfg(target_os = "macos")]
fn sysctl(key: &str) -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", key])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn host_labels() -> Vec<(String, String)> {
    #[cfg(target_os = "macos")]
    {
        let brand = sysctl("machdep.cpu.brand_string");
        let mem = sysctl("hw.memsize").and_then(|m| m.parse::<u64>().ok());
        auto_labels(brand.as_deref(), mem)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Vec::new()
    }
}

pub fn install(a: &NodeAgentArgs) -> Result<PathBuf> {
    a.validate()?;
    let home = dh::home()?;
    let (plist, log, default_state) = paths(&home);
    let state_dir = a.state_dir.clone().unwrap_or(default_state);
    let agent = agent_binary()?;
    let auto = if a.no_auto_labels {
        Vec::new()
    } else {
        host_labels()
    };
    let labels = merge_labels(auto, &a.labels);
    let args = render_args(&agent, a, &labels, &state_dir);
    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::write(
        &plist,
        dh::render_plist_with(LABEL, &args, &log, &[], &dh::PlistOpts::default()),
    )?;
    dh::launchctl_load(LABEL, &plist)?;
    Ok(plist)
}

pub fn uninstall() -> Result<bool> {
    let (plist, _, _) = paths(&dh::home()?);
    dh::launchctl_unload(LABEL, &plist)
}

pub fn status() -> Result<serde_json::Value> {
    let (plist, log, _) = paths(&dh::home()?);
    crate::launch_agent::status_of(LABEL, &plist, &log)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> NodeAgentArgs {
        NodeAgentArgs {
            name: "studio-1".into(),
            central: "https://fleet.example:7790".into(),
            fluxvm_url: "http://127.0.0.1:7788".into(),
            advertise_url: Some("http://studio-1.local:7788".into()),
            token_file: Some("/Users/me/.config/fluxvm/agent.token".into()),
            labels: vec![],
            no_auto_labels: false,
            no_overlay_sync: true,
            state_dir: None,
        }
    }

    #[test]
    fn the_chip_and_memory_become_labels() {
        let l = auto_labels(Some("Apple M4 Max"), Some(64 << 30));
        assert!(l.contains(&("kairon.zyvor.dev/backend.vz".into(), "true".into())));
        assert!(l.contains(&("kairon.zyvor.dev/chip".into(), "m4-max".into())));
        assert!(l.contains(&("kairon.zyvor.dev/memory-gib".into(), "64".into())));
        // an unreadable host still reports the backend, and nothing invented
        assert_eq!(auto_labels(None, None).len(), 1);
        assert_eq!(auto_labels(Some("  "), Some(0)).len(), 1);
    }

    #[test]
    fn a_user_label_replaces_the_automatic_one() {
        let auto = auto_labels(Some("Apple M2"), Some(16 << 30));
        let user = vec![("kairon.zyvor.dev/chip".to_owned(), "custom".to_owned())];
        let m = merge_labels(auto, &user);
        assert_eq!(
            m.iter()
                .filter(|(k, _)| k == "kairon.zyvor.dev/chip")
                .count(),
            1
        );
        assert!(m.contains(&("kairon.zyvor.dev/chip".into(), "custom".into())));
        assert!(m.contains(&("kairon.zyvor.dev/memory-gib".into(), "16".into())));
    }

    #[test]
    fn the_command_line_carries_the_token_file_and_never_a_token() {
        let labels = vec![("zone".to_owned(), "lab".to_owned())];
        let v = render_args(
            Path::new("/opt/homebrew/bin/fluxvm-agent"),
            &args(),
            &labels,
            Path::new("/s"),
        );
        assert_eq!(&v[..2], ["/opt/homebrew/bin/fluxvm-agent", "node"]);
        let joined = v.join(" ");
        assert!(joined.contains("--token-file /Users/me/.config/fluxvm/agent.token"));
        assert!(!joined.contains("--token "));
        assert!(joined.contains("--advertise-url http://studio-1.local:7788"));
        assert!(joined.contains("--label zone=lab") && joined.contains("--no-overlay-sync"));
        assert!(joined.contains("--state-dir /s"));
    }

    #[test]
    fn bad_names_and_urls_are_refused() {
        let mut a = args();
        a.token_file = None;
        assert!(a.validate().is_ok());
        for bad in ["", "Studio", "-x", "a b", ".x"] {
            a.name = bad.into();
            assert!(a.validate().is_err(), "{bad:?}");
        }
        a.name = "ok".into();
        a.central = "fleet.example".into();
        assert!(a.validate().is_err());
        a.central = "https://a:1,http://b:2".into();
        assert!(a.validate().is_ok());
        a.token_file = Some("/definitely/not/here".into());
        assert!(
            a.validate()
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
    }

    #[test]
    fn the_plist_uses_its_own_label_and_log() {
        let v = render_args(Path::new("/a"), &args(), &[], Path::new("/s"));
        let p = dh::render_plist_with(
            LABEL,
            &v,
            Path::new("/l/fluxvm-agent.log"),
            &[],
            &dh::PlistOpts::default(),
        );
        assert!(p.contains("<key>Label</key><string>dev.zyvor.fluxvm-agent</string>"));
        assert!(p.contains("<string>node</string>") && p.contains("/l/fluxvm-agent.log"));
    }
}
