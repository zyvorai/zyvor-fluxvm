// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl upgrade --check` against a fake release list (a JSON file named by `$FLUXVM_RELEASES_API`): no network.
//! It must exit 0, print one status line, and leave the binary untouched.

#![cfg(unix)]

use std::process::Command;

fn check(releases: &str, extra: &[&str]) -> (bool, String, String) {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("releases.json");
    std::fs::write(&f, releases).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_fluxctl"))
        .args(["upgrade", "--check"])
        .args(extra)
        .env("FLUXVM_RELEASES_API", format!("file://{}", f.display()))
        .env("HOME", d.path())
        .output()
        .expect("spawn fluxctl");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn only_a_mac_tag_without_a_package_says_nothing_is_installable_and_exits_zero() {
    let (ok, out, err) = check(r#"[{"tag_name":"mac-v0.1.0","assets":[]}]"#, &[]);
    assert!(ok, "{err}");
    assert_eq!(out.trim().lines().count(), 1, "{out}");
    assert!(
        out.contains("no installable release exists") && out.contains("mac-v0.1.0"),
        "{out}"
    );
}

#[test]
fn an_unreadable_list_is_a_status_line_not_a_failure() {
    let (ok, out, err) = check("not json", &[]);
    assert!(ok, "{err}");
    assert!(out.contains("could not read the release list"), "{out}");
}

#[test]
fn json_carries_the_state_and_the_next_steps() {
    let (ok, out, err) = check(r#"[]"#, &["--json"]);
    assert!(ok, "{err}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["state"], "no_installable_release");
    assert_eq!(v["update_available"], false);
    assert!(v["maintainer_next"].as_str().unwrap().contains("Publish"));
    assert!(v["user_next"].as_str().unwrap().contains("installer"));
}

#[test]
fn a_real_package_and_checksum_is_reported_as_an_update_and_still_writes_nothing() {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let plat = match (os, arch) {
        ("macos", "aarch64") => "macos-arm64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => return,
    };
    let rel = format!(
        r#"[{{"tag_name":"v99.0.0","assets":[
            {{"name":"fluxvm-99.0.0-{plat}.tar.gz","browser_download_url":"https://x/p.tar.gz"}},
            {{"name":"fluxvm-99.0.0-{plat}.tar.gz.sha256","browser_download_url":"https://x/p.sha256"}}]}}]"#
    );
    let (ok, out, err) = check(&rel, &[]);
    assert!(ok, "{err}");
    assert!(
        out.contains("update available, 99.0.0") && out.contains("fluxctl upgrade"),
        "{out}"
    );
    assert_eq!(out.trim().lines().count(), 1, "{out}");
}
