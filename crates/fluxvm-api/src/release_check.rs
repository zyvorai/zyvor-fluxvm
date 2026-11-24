// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! "Is there a newer release I can install?" for `fluxctl upgrade --check` and `GET /v1/upgrade/check` (the Studio Home
//! card). One model, two readers.
//!
//! The releases come from the GitHub releases API of the default release page (or from `$FLUXVM_RELEASES_API`, which may
//! be a `file://` JSON file: that is how the tests and an offline mirror feed it). A release is installable when it is
//! published (not a draft or pre-release), its tag is `vX.Y.Z` or `mac-vX.Y.Z`, it has a `.tar.gz` package for this
//! platform, and a `.sha256` file for that package. Anything else is listed with the reason it was passed over, so
//! "no release" is never a silent answer. Read-only: nothing is downloaded or written here.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

pub const DEFAULT_RELEASES_PAGE: &str = "https://github.com/zyvorai/zyvor-fluxvm/releases";

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    #[serde(default)]
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    UpToDate,
    UpdateAvailable,
    /// Releases exist (or not) but none has a package and checksum for this platform.
    NoInstallableRelease,
    /// The release list could not be read at all.
    Unreachable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seen {
    pub tag: String,
    pub installable: bool,
    /// Why it was passed over, or "ok".
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseStatus {
    pub schema: u32,
    pub state: State,
    pub current: String,
    pub latest: Option<String>,
    pub tag: Option<String>,
    pub update_available: bool,
    pub source: String,
    pub package: Option<String>,
    pub checksum: Option<String>,
    pub platform: Option<String>,
    pub looked_for: String,
    /// The newest releases the server listed, newest first, at most five.
    pub newest: Vec<Seen>,
    /// One line, for a terminal or a card.
    pub line: String,
    /// What the person who publishes releases has to do; absent when nothing is missing.
    pub maintainer_next: Option<String>,
    /// What the person running fluxctl can do now.
    pub user_next: String,
}

/// `Greater` when `a` is newer than `b`; a pre-release is older than its release.
pub fn compare(a: &str, b: &str) -> Ordering {
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
    let (na, pa) = parts(a);
    let (nb, pb) = parts(b);
    na.cmp(&nb).then(pb.cmp(&pa))
}

pub fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
}

/// `v0.4.22` or `mac-v0.1.0` to `0.4.22` / `0.1.0`; anything else is not a release tag of ours.
pub fn tag_version(tag: &str) -> Option<String> {
    let v = tag
        .strip_prefix("mac-v")
        .or_else(|| tag.strip_prefix('v'))?;
    valid_version(v).then(|| v.to_owned())
}

/// The releases-API address for a GitHub releases page, or None for a page that is not GitHub (a mirror).
pub fn api_url_for(page: &str) -> Option<String> {
    let rest = page
        .trim_end_matches('/')
        .strip_prefix("https://github.com/")?;
    let path = rest.strip_suffix("/releases")?;
    (path.split('/').count() == 2 && !path.contains(".."))
        .then(|| format!("https://api.github.com/repos/{path}/releases?per_page=30"))
}

/// One release against this platform: the package and checksum addresses, or the reason it cannot be installed.
fn judge(r: &Release, platform: Option<&str>) -> Result<(String, String, String), String> {
    if r.draft {
        return Err("a draft".into());
    }
    if r.prerelease {
        return Err("a pre-release".into());
    }
    let Some(version) = tag_version(&r.tag_name) else {
        return Err("the tag is not vX.Y.Z or mac-vX.Y.Z".into());
    };
    let Some(platform) = platform else {
        return Err("no package is built for this operating system".into());
    };
    let Some(pkg) = r
        .assets
        .iter()
        .find(|a| a.name.ends_with(".tar.gz") && a.name.contains(platform))
    else {
        return Err(format!("no .tar.gz package for {platform}"));
    };
    let sum_name = format!("{}.sha256", pkg.name);
    let Some(sum) = r.assets.iter().find(|a| a.name == sum_name) else {
        return Err(format!("{} has no {sum_name} checksum file", pkg.name));
    };
    if pkg.browser_download_url.is_empty() || sum.browser_download_url.is_empty() {
        return Err("the assets have no download address".into());
    }
    Ok((
        version,
        pkg.browser_download_url.clone(),
        sum.browser_download_url.clone(),
    ))
}

fn looked_for(platform: Option<&str>) -> String {
    format!(
        "a published release tagged vX.Y.Z or mac-vX.Y.Z with a {} package (fluxvm-X.Y.Z-{}.tar.gz) and its .sha256 file",
        platform.unwrap_or("<your platform>"),
        platform.unwrap_or("<platform>")
    )
}

const MAINTAINER: &str = "Publish a vX.Y.Z release that has a fluxvm-X.Y.Z-<platform>.tar.gz package and a matching .tar.gz.sha256 asset for each platform (the release-macos and release-linux workflows upload both).";
const USER: &str = "Nothing to install. To get the newest build now, re-run the installer (scripts/install.sh) or build from source (cargo build --release -p fluxctl).";

fn base(current: &str, source: &str, platform: Option<&str>) -> ReleaseStatus {
    ReleaseStatus {
        schema: 1,
        state: State::NoInstallableRelease,
        current: current.into(),
        latest: None,
        tag: None,
        update_available: false,
        source: source.into(),
        package: None,
        checksum: None,
        platform: platform.map(str::to_owned),
        looked_for: looked_for(platform),
        newest: vec![],
        line: String::new(),
        maintainer_next: None,
        user_next: USER.into(),
    }
}

/// Decide from a release list. `platform` is the package tag of this machine (`macos-arm64`), None if unsupported.
pub fn assess(
    releases: &[Release],
    current: &str,
    platform: Option<&str>,
    source: &str,
) -> ReleaseStatus {
    let mut s = base(current, source, platform);
    let mut best: Option<(String, String, String, String)> = None;
    for r in releases {
        match judge(r, platform) {
            Ok((v, pkg, sum)) => {
                if s.newest.len() < 5 {
                    s.newest.push(Seen {
                        tag: r.tag_name.clone(),
                        installable: true,
                        note: "ok".into(),
                    });
                }
                if best.as_ref().is_none_or(|(bv, ..)| compare(&v, bv).is_gt()) {
                    best = Some((v, r.tag_name.clone(), pkg, sum));
                }
            }
            Err(why) => {
                if s.newest.len() < 5 {
                    s.newest.push(Seen {
                        tag: r.tag_name.clone(),
                        installable: false,
                        note: why,
                    });
                }
            }
        }
    }
    let Some((v, tag, pkg, sum)) = best else {
        let listed = if s.newest.is_empty() {
            "the server lists no releases at all".to_owned()
        } else {
            format!(
                "newest releases: {}",
                s.newest
                    .iter()
                    .map(|n| format!("{} ({})", n.tag, n.note))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        s.line = format!(
            "fluxctl {current}: no installable release exists yet, so there is nothing newer to install ({listed})."
        );
        s.maintainer_next = Some(MAINTAINER.into());
        return s;
    };
    s.latest = Some(v.clone());
    s.tag = Some(tag.clone());
    if compare(&v, current).is_gt() {
        s.state = State::UpdateAvailable;
        s.update_available = true;
        s.package = Some(pkg);
        s.checksum = Some(sum);
        s.line = format!(
            "fluxctl {current}: update available, {v} ({tag}). Run `fluxctl upgrade` to install it."
        );
        s.user_next = "Run `fluxctl upgrade` (it asks first, verifies the SHA-256, and keeps the old binary as fluxctl.previous).".into();
    } else {
        s.state = State::UpToDate;
        s.line = format!("fluxctl {current} is up to date (newest installable release: {tag}).");
        s.user_next = "Nothing to do.".into();
    }
    s
}

/// The list could not be read: still a status, with the next step, never a crash.
pub fn unreachable(
    current: &str,
    source: &str,
    platform: Option<&str>,
    why: &str,
) -> ReleaseStatus {
    let mut s = base(current, source, platform);
    s.state = State::Unreachable;
    s.line = format!("fluxctl {current}: could not read the release list ({why}); not checked.");
    s.user_next = "Check the network and run `fluxctl upgrade --check` again, or point it at a mirror with `fluxctl config set cli.upgrade_url URL`.".into();
    s
}

/// The several-line explanation of a "no installable release" or "unreachable" answer.
pub fn explain(s: &ReleaseStatus) -> Vec<String> {
    let mut o = vec![
        s.line.clone(),
        format!("Looked at: {}", s.source),
        format!("Looked for: {}", s.looked_for),
    ];
    if s.state == State::NoInstallableRelease {
        if s.newest.is_empty() {
            o.push("Newest releases: none listed.".into());
        } else {
            o.push("Newest releases:".into());
            for n in &s.newest {
                o.push(format!(
                    "  {}  {}",
                    n.tag,
                    if n.installable {
                        "installable".to_owned()
                    } else {
                        format!("skipped: {}", n.note)
                    }
                ));
            }
        }
    }
    if let Some(m) = &s.maintainer_next {
        o.push(format!("For the maintainer: {m}"));
    }
    o.push(format!("For you: {}", s.user_next));
    o
}

/// Reads the release list: a `file://` JSON file or an https address. Err is a short reason.
pub async fn fetch_releases(url: &str) -> Result<Vec<Release>, String> {
    let bytes = if let Some(p) = url.strip_prefix("file://") {
        std::fs::read(p).map_err(|e| format!("{p}: {e}"))?
    } else {
        let c = reqwest::Client::builder()
            .user_agent(concat!("fluxctl/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| e.to_string())?;
        let r = c
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("the server answered {}", r.status()));
        }
        r.bytes().await.map_err(|e| e.to_string())?.to_vec()
    };
    serde_json::from_slice(&bytes).map_err(|e| format!("not a release list: {e}"))
}

/// The address the release list is read from for this page, honouring `$FLUXVM_RELEASES_API`.
pub fn list_url(page: &str) -> Option<String> {
    std::env::var("FLUXVM_RELEASES_API")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| api_url_for(page))
}

/// Package tag of this machine, the same names `scripts/install.sh` and the release workflows use.
pub fn platform_tag(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("macos-arm64"),
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        _ => None,
    }
}

/// The whole check for one release page.
pub async fn check(page: &str, current: &str) -> ReleaseStatus {
    let platform = platform_tag(std::env::consts::OS, std::env::consts::ARCH);
    let Some(url) = list_url(page) else {
        return unreachable(
            current,
            page,
            platform,
            "this release server has no release list to read",
        );
    };
    match fetch_releases(&url).await {
        Ok(r) => assess(&r, current, platform, page),
        Err(e) => unreachable(current, page, platform, &e),
    }
}

/// `GET /v1/upgrade/check`: the same status `fluxctl upgrade --check` prints. Read-only; it asks the release list and
/// installs nothing. The version is the daemon's own.
pub(crate) fn routes() -> axum::Router<std::sync::Arc<fluxvm_scheduler::VmManager>> {
    axum::Router::new().route("/v1/upgrade/check", axum::routing::get(upgrade_check))
}

async fn upgrade_check() -> axum::Json<ReleaseStatus> {
    axum::Json(check(DEFAULT_RELEASES_PAGE, env!("CARGO_PKG_VERSION")).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(tag: &str, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: assets
                .iter()
                .map(|n| Asset {
                    name: (*n).into(),
                    browser_download_url: format!("https://dl/{tag}/{n}"),
                })
                .collect(),
        }
    }
    #[test]
    fn the_route_is_registered() {
        let _ = routes();
    }

    const P: Option<&str> = Some("macos-arm64");

    #[test]
    fn tags_and_pages_are_read_strictly() {
        assert_eq!(tag_version("v0.4.22").as_deref(), Some("0.4.22"));
        assert_eq!(tag_version("mac-v0.1.0").as_deref(), Some("0.1.0"));
        assert_eq!(tag_version("nightly"), None);
        assert_eq!(tag_version("v"), None);
        assert_eq!(
            api_url_for("https://github.com/zyvorai/zyvor-fluxvm/releases/").as_deref(),
            Some("https://api.github.com/repos/zyvorai/zyvor-fluxvm/releases?per_page=30")
        );
        assert_eq!(api_url_for("https://mirror.example/releases"), None);
    }

    #[test]
    fn only_a_mac_tag_with_no_package_is_explained_not_accepted() {
        let rs = [rel("mac-v0.1.0", &["notes.txt"])];
        let s = assess(&rs, "0.4.21", P, "src");
        assert_eq!(s.state, State::NoInstallableRelease);
        assert!(!s.update_available && s.maintainer_next.is_some());
        assert!(
            s.line.contains("mac-v0.1.0") && s.line.contains("no .tar.gz package for macos-arm64"),
            "{}",
            s.line
        );
        let text = explain(&s).join("\n");
        assert!(
            text.contains("Looked for:")
                && text.contains("For the maintainer:")
                && text.contains("build from source"),
            "{text}"
        );
    }

    #[test]
    fn a_package_without_a_checksum_is_not_installable() {
        let rs = [rel("v0.5.0", &["fluxvm-0.5.0-macos-arm64.tar.gz"])];
        let s = assess(&rs, "0.4.21", P, "src");
        assert_eq!(s.state, State::NoInstallableRelease);
        assert!(
            s.line
                .contains("has no fluxvm-0.5.0-macos-arm64.tar.gz.sha256"),
            "{}",
            s.line
        );
    }

    #[test]
    fn a_mac_tag_is_accepted_when_package_and_checksum_exist() {
        let rs = [rel(
            "mac-v0.9.0",
            &[
                "fluxvm-0.9.0-macos-arm64.tar.gz",
                "fluxvm-0.9.0-macos-arm64.tar.gz.sha256",
            ],
        )];
        let s = assess(&rs, "0.4.21", P, "src");
        assert_eq!(s.state, State::UpdateAvailable);
        assert_eq!(s.tag.as_deref(), Some("mac-v0.9.0"));
        assert!(
            s.package.unwrap().ends_with("macos-arm64.tar.gz")
                && s.checksum.unwrap().ends_with(".sha256")
        );
        assert!(s.maintainer_next.is_none());
    }

    #[test]
    fn newest_installable_wins_and_an_equal_one_is_up_to_date_and_drafts_are_skipped() {
        let both = |v: &str| {
            [
                format!("fluxvm-{v}-macos-arm64.tar.gz"),
                format!("fluxvm-{v}-macos-arm64.tar.gz.sha256"),
            ]
        };
        let mk = |t: &str, v: &str| {
            let a = both(v);
            rel(t, &[a[0].as_str(), a[1].as_str()])
        };
        let mut draft = mk("v9.9.9", "9.9.9");
        draft.draft = true;
        let rs = [draft, mk("v0.4.20", "0.4.20"), mk("v0.4.21", "0.4.21")];
        let s = assess(&rs, "0.4.21", P, "src");
        assert_eq!(s.state, State::UpToDate);
        assert_eq!(s.tag.as_deref(), Some("v0.4.21"));
        assert!(s.line.contains("up to date"));
        assert_eq!(s.newest[0].note, "a draft");
    }

    #[test]
    fn no_releases_and_an_unsupported_platform_both_say_so() {
        let s = assess(&[], "0.4.21", P, "src");
        assert!(s.line.contains("lists no releases"), "{}", s.line);
        let s = assess(&[rel("v1.0.0", &[])], "0.4.21", None, "src");
        assert!(
            s.line
                .contains("no package is built for this operating system"),
            "{}",
            s.line
        );
    }

    #[tokio::test]
    async fn a_fake_release_source_is_read_from_a_file_and_a_bad_one_is_unreachable() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("r.json");
        std::fs::write(&f, r#"[{"tag_name":"mac-v0.1.0","assets":[]}]"#).unwrap();
        let rs = fetch_releases(&format!("file://{}", f.display()))
            .await
            .unwrap();
        assert_eq!(rs.len(), 1);
        assert!(
            fetch_releases(&format!("file://{}/nope", d.path().display()))
                .await
                .is_err()
        );
        std::fs::write(&f, "not json").unwrap();
        let e = fetch_releases(&format!("file://{}", f.display()))
            .await
            .unwrap_err();
        assert!(e.contains("not a release list"), "{e}");
        let u = unreachable("0.4.21", "src", P, &e);
        assert_eq!(u.state, State::Unreachable);
        assert!(explain(&u).join("\n").contains("For you:"));
    }
}
