// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl upgrade`: find the latest release, show what would change, and replace this binary only after its SHA-256
//! matches. It never runs by itself, never replaces anything without a "yes" (or `--yes`), and refuses when there is no
//! checksum to check.
//!
//! Where releases come from (the same convention as `scripts/install.sh`): `<base>/latest` redirects to
//! `<base>/tag/vX.Y.Z`, and the package is `<base>/download/vX.Y.Z/fluxvm-X.Y.Z-<platform>.tar.gz` with a `.sha256` next to it.
//! The base is, in order: `--url`, `$FLUXVM_RELEASE_BASE`, the config setting `cli.upgrade_url`, then the public GitHub
//! releases page of FluxVM. A `file://` base (a folder) works with `--to VERSION`.
//!
//! What the check proves: the file is the one the checksum names. The checksum comes from the same server as the file,
//! so it protects against a damaged or truncated download, not against a server that is itself compromised. The signed
//! chain (Ed25519 channel, manifest, package) is `scripts/install.sh --mirror URL --pubkey FILE`; this command does not
//! pretend to be that. The previous binary is kept as `fluxctl.previous` next to it, for going back.
//!
//! Not run against a real published release from this code's tests: they use a folder as the release server.

use crate::cli_errors::coded;
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_BASE: &str = "https://github.com/zyvorai/zyvor-fluxvm/releases";

#[derive(clap::Args, Debug, Clone)]
pub struct UpgradeArgs {
    /// Only look: say whether a newer release exists and what would change. Changes nothing.
    #[arg(long)]
    pub check: bool,
    /// A specific version instead of the latest (also lets you go back).
    #[arg(long = "to", value_name = "VERSION")]
    pub to: Option<String>,
    /// Release server (default: $FLUXVM_RELEASE_BASE, then the `cli.upgrade_url` setting, then the public releases page).
    #[arg(long)]
    pub url: Option<String>,
    /// Do not ask before replacing the binary.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Machine-readable output for `--check`.
    #[arg(long)]
    pub json: bool,
}

pub fn platform_tag(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("macos-arm64"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        _ => None,
    }
}

/// `https://…/releases/tag/v0.4.22` (or a bare tag) to `0.4.22`.
pub fn version_from_url(url: &str) -> Option<String> {
    let last = url.trim_end_matches('/').rsplit('/').next()?;
    let v = last.strip_prefix('v').unwrap_or(last);
    valid_version(v).then(|| v.to_owned())
}

pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn parts(v: &str) -> ([u64; 3], bool) {
    let (core, pre) = match v.split_once('-') {
        Some((c, _)) => (c, true),
        None => (v, false),
    };
    let mut n = [0u64; 3];
    for (i, p) in core.split('.').take(3).enumerate() {
        n[i] = p.parse().unwrap_or(0);
    }
    (n, pre)
}

/// `Greater` when `a` is newer than `b`; a pre-release is older than its release.
pub fn compare(a: &str, b: &str) -> std::cmp::Ordering {
    let (na, pa) = parts(a);
    let (nb, pb) = parts(b);
    na.cmp(&nb).then(pb.cmp(&pa))
}

/// The SHA-256 a checksum file names for `asset`: the first word of a bare file, or of the line that names the asset.
pub fn expected_sum(text: &str, asset: &str) -> Option<String> {
    let line = text
        .lines()
        .find(|l| {
            l.split_whitespace()
                .nth(1)
                .is_some_and(|f| f.trim_start_matches('*').ends_with(asset))
        })
        .or_else(|| text.lines().find(|l| l.split_whitespace().count() == 1))
        .or_else(|| text.lines().next())?;
    let w = line.split_whitespace().next()?.to_ascii_lowercase();
    (w.len() == 64 && w.chars().all(|c| c.is_ascii_hexdigit())).then_some(w)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Ok when the checksum file holds a SHA-256 and it matches; the two refusals carry their explain codes.
pub fn verify(bytes: &[u8], sum_text: Option<&str>, asset: &str) -> Result<String> {
    let Some(text) = sum_text else {
        return Err(coded(
            format!("{asset} has no checksum file, so it was not installed"),
            "UPGRADE_UNVERIFIED",
        ));
    };
    let Some(want) = expected_sum(text, asset) else {
        return Err(coded(
            format!(
                "the checksum file for {asset} does not hold a SHA-256, so it was not installed"
            ),
            "UPGRADE_UNVERIFIED",
        ));
    };
    let got = sha256_hex(bytes);
    if got != want {
        return Err(coded(
            format!(
                "checksum mismatch for {asset}: expected {want}, got {got}. Nothing was changed"
            ),
            "UPGRADE_CHECKSUM_MISMATCH",
        ));
    }
    Ok(got)
}

async fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("fluxctl/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(600))
        .build()?)
}

/// `Ok(None)` for "not there" (404 or a missing file), `Err` for a failure to ask.
async fn fetch(url: &str) -> Result<Option<Vec<u8>>> {
    if let Some(p) = url.strip_prefix("file://") {
        return match std::fs::read(p) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        };
    }
    let r = client().await?.get(url).send().await?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(r.error_for_status()?.bytes().await?.to_vec()))
}

async fn latest(base: &str) -> Result<String> {
    let no_channel = |why: String| {
        coded(
            format!(
                "could not find the latest release at {base}: {why}\nThis build looks for releases at {DEFAULT_BASE} unless told otherwise; \
                 point it at your own with `fluxctl config set cli.upgrade_url URL`, `--url`, or $FLUXVM_RELEASE_BASE"
            ),
            "UPGRADE_NO_CHANNEL",
        )
    };
    if base.starts_with("file://") {
        return Err(no_channel(
            "a folder has no \"latest\"; pass --to VERSION".into(),
        ));
    }
    let r = client()
        .await?
        .head(format!("{base}/latest"))
        .send()
        .await
        .map_err(|e| no_channel(e.to_string()))?;
    if !r.status().is_success() {
        return Err(no_channel(format!(
            "the server answered {} (is a release published?)",
            r.status()
        )));
    }
    version_from_url(r.url().as_str())
        .ok_or_else(|| no_channel(format!("{} is not a release address", r.url())))
}

fn find_binary(dir: &Path) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_binary(&p) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == "fluxctl") {
            return Some(p);
        }
    }
    None
}

/// Unpacks the package into `into` and returns the `fluxctl` inside it.
fn unpack(archive: &Path, into: &Path) -> Result<PathBuf> {
    let st = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .status()
        .context("needs `tar` to unpack the package")?;
    if !st.success() {
        bail!(
            "the package could not be unpacked (it passed its checksum, so it is not a tar.gz at all)"
        );
    }
    find_binary(into).context("the package holds no `fluxctl` binary")
}

/// Puts `new` in place of `exe`, keeping the old one as `<exe>.previous`; undoes itself if the last step fails.
pub fn replace(exe: &Path, new: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let not_writable = |e: &dyn std::fmt::Display| {
        coded(
            format!("cannot replace {}: {e}", exe.display()),
            "UPGRADE_NOT_WRITABLE",
        )
    };
    let dir = exe.parent().context("the binary has no folder")?;
    let staged = dir.join(format!(".fluxctl.new-{}", std::process::id()));
    std::fs::copy(new, &staged).map_err(|e| not_writable(&e))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| not_writable(&e))?;
    let previous = exe.with_file_name("fluxctl.previous");
    if let Err(e) = std::fs::rename(exe, &previous) {
        let _ = std::fs::remove_file(&staged);
        return Err(not_writable(&e));
    }
    if let Err(e) = std::fs::rename(&staged, exe) {
        let _ = std::fs::rename(&previous, exe);
        let _ = std::fs::remove_file(&staged);
        return Err(not_writable(&e));
    }
    Ok(previous)
}

fn managed(exe: &Path) -> bool {
    let s = exe.to_string_lossy();
    s.contains("/Cellar/") || s.contains("/homebrew/Cellar/")
}

fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut l = String::new();
    std::io::stdin().lock().read_line(&mut l).is_ok()
        && matches!(l.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub async fn run(a: &UpgradeArgs, config: Option<&Path>) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let (base, from) = if let Some(u) = &a.url {
        (u.clone(), "--url")
    } else if let Ok(u) = std::env::var("FLUXVM_RELEASE_BASE") {
        (u, "$FLUXVM_RELEASE_BASE")
    } else if let Some(u) = crate::user_config::text_setting(config, "upgrade_url") {
        (u, "cli.upgrade_url")
    } else {
        (DEFAULT_BASE.to_owned(), "the built-in default")
    };
    let base = base.trim_end_matches('/').to_owned();
    let explicit =
        a.to.as_deref()
            .map(|v| v.trim_start_matches('v').to_owned());
    if let Some(v) = &explicit
        && !valid_version(v)
    {
        bail!("{v:?} is not a version (for example 0.4.22)");
    }
    let target = match &explicit {
        Some(v) => v.clone(),
        None => latest(&base).await?,
    };
    let newer = compare(&target, current).is_gt();
    let same = target == current;
    let platform = platform_tag(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
        coded(
            format!(
                "no release package is published for {} {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
            "UPGRADE_NO_CHANNEL",
        )
    })?;
    let asset = format!("fluxvm-{target}-{platform}.tar.gz");
    let asset_url = format!("{base}/download/v{target}/{asset}");
    let exe = std::env::current_exe()?
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("fluxctl"));

    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "current": current, "latest": target, "update_available": newer, "source": base, "source_from": from,
                "package": asset_url, "checksum": format!("{asset_url}.sha256"), "binary": exe,
            }))?
        );
        if a.check {
            return Ok(());
        }
    } else {
        println!("release source: {base} ({from})");
        println!("this fluxctl:   {current}  ({})", exe.display());
        println!(
            "{}:  {target}",
            if explicit.is_some() {
                "requested     "
            } else {
                "latest        "
            }
        );
    }
    if same || (explicit.is_none() && !newer) {
        if !a.json {
            println!("Already up to date.");
        }
        return Ok(());
    }
    if !a.json {
        println!();
        println!(
            "{} fluxctl {current} -> {target}",
            if newer { "Upgrade" } else { "Change" }
        );
        println!("  download   {asset_url}");
        println!("  verify     SHA-256 from {asset_url}.sha256 (refuses if missing or different)");
        println!(
            "  replace    {} (the old one is kept as fluxctl.previous)",
            exe.display()
        );
        println!("  notes      {base}/tag/v{target}");
        println!("  This does not touch the daemon, your config or your VMs.");
    }
    if a.check {
        println!("\nNothing changed (--check). Run `fluxctl upgrade` to do it.");
        return Ok(());
    }
    if managed(&exe) {
        return Err(coded(
            format!(
                "{} belongs to Homebrew, which should upgrade it: `brew upgrade fluxctl`",
                exe.display()
            ),
            "UPGRADE_NOT_WRITABLE",
        ));
    }
    if !a.yes && !confirm("Replace the binary?") {
        bail!("not replaced: it asks first and nothing said yes (pass --yes in a script)");
    }
    let tmp = std::env::temp_dir().join(format!("pcli-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let result = async {
        eprintln!("downloading {asset} ...");
        let bytes = fetch(&asset_url)
            .await
            .map_err(|e| coded(format!("could not download {asset_url}: {e}"), "UPGRADE_NO_CHANNEL"))?
            .ok_or_else(|| coded(format!("{asset_url} does not exist (no package for {platform} in v{target}?)"), "UPGRADE_NO_CHANNEL"))?;
        let sums = fetch(&format!("{asset_url}.sha256"))
            .await
            .map_err(|e| coded(format!("could not download the checksum: {e}"), "UPGRADE_NO_CHANNEL"))?
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        let sum = verify(&bytes, sums.as_deref(), &asset)?;
        eprintln!("checksum ok ({sum})");
        let tgz = tmp.join(&asset);
        std::fs::write(&tgz, &bytes)?;
        let out = tmp.join("x");
        std::fs::create_dir_all(&out)?;
        let new = unpack(&tgz, &out)?;
        let said = std::process::Command::new(&new).arg("--version").output();
        let ok = said.is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&target));
        if !ok {
            bail!("the new binary did not report version {target} when run, so it was not installed");
        }
        let prev = replace(&exe, &new)?;
        println!("Upgraded {current} -> {target}. The old binary is {}.", prev.display());
        println!("If the daemon is running, restart it to use the new version (`fluxctl service restart`).");
        anyhow::Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn versions_come_from_release_addresses() {
        assert_eq!(
            version_from_url("https://x/releases/tag/v0.4.22").as_deref(),
            Some("0.4.22")
        );
        assert_eq!(version_from_url("https://x/releases/latest"), None);
        assert_eq!(
            version_from_url("https://x/releases/tag/0.5.0/").as_deref(),
            Some("0.5.0")
        );
        assert!(!valid_version("../x") && !valid_version("") && !valid_version("v1"));
    }

    #[test]
    fn newer_means_newer_and_a_prerelease_is_older() {
        assert_eq!(compare("0.4.22", "0.4.21"), Greater);
        assert_eq!(compare("0.10.0", "0.9.9"), Greater);
        assert_eq!(compare("0.4.21", "0.4.21"), Equal);
        assert_eq!(compare("0.4.21-rc1", "0.4.21"), Less);
    }

    #[test]
    fn platforms_map_to_package_names() {
        assert_eq!(platform_tag("macos", "aarch64"), Some("macos-arm64"));
        assert_eq!(platform_tag("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(platform_tag("windows", "x86_64"), None);
    }

    #[test]
    fn the_checksum_is_found_in_either_file_shape() {
        let h = sha256_hex(b"abc");
        assert_eq!(
            expected_sum(&format!("{h}\n"), "a.tar.gz").as_deref(),
            Some(h.as_str())
        );
        assert_eq!(
            expected_sum(
                &format!("{h}  a.tar.gz\n{}  b.tar.gz\n", "0".repeat(64)),
                "a.tar.gz"
            )
            .as_deref(),
            Some(h.as_str())
        );
        assert_eq!(expected_sum("not a sum", "a"), None);
    }

    #[test]
    fn it_refuses_without_a_checksum_or_on_a_mismatch() {
        let h = sha256_hex(b"pkg");
        assert_eq!(verify(b"pkg", Some(&h), "a").unwrap(), h);
        let e = verify(b"pkg", None, "a").unwrap_err().to_string();
        assert!(e.contains("UPGRADE_UNVERIFIED"), "{e}");
        let e = verify(b"pkg", Some("garbage"), "a")
            .unwrap_err()
            .to_string();
        assert!(e.contains("UPGRADE_UNVERIFIED"), "{e}");
        let e = verify(b"other", Some(&h), "a").unwrap_err().to_string();
        assert!(e.contains("UPGRADE_CHECKSUM_MISMATCH"), "{e}");
    }

    #[test]
    fn replace_keeps_the_old_binary_and_swaps_the_new_one_in() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("fluxctl");
        let new = d.path().join("new");
        std::fs::write(&exe, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        let prev = replace(&exe, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(prev).unwrap(), "old");
        assert_eq!(
            std::fs::metadata(&exe).unwrap().permissions().mode() & 0o111,
            0o111
        );
        assert!(std::fs::read_dir(d.path()).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".fluxctl.new")
        }));
    }

    #[test]
    fn homebrew_installs_are_left_to_brew() {
        assert!(managed(Path::new(
            "/opt/homebrew/Cellar/fluxctl/0.4.21/bin/fluxctl"
        )));
        assert!(!managed(Path::new("/usr/local/bin/fluxctl")));
    }

    #[tokio::test]
    async fn a_folder_serves_as_a_release_server_and_a_missing_file_is_none() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a"), "x").unwrap();
        let url = |n: &str| format!("file://{}/{n}", d.path().display());
        assert_eq!(fetch(&url("a")).await.unwrap(), Some(b"x".to_vec()));
        assert_eq!(fetch(&url("b")).await.unwrap(), None);
        let e = latest(&format!("file://{}", d.path().display()))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("UPGRADE_NO_CHANNEL") && e.contains("cli.upgrade_url"),
            "{e}"
        );
    }
}
