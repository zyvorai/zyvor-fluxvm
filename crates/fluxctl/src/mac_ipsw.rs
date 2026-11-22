// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `FROM IPSW latest|PATH`: validation and the unattended-provisioning plan. `mac build` does NOT run an IPSW install yet; this module
//! checks the source, prints what the install would do in order, and says which parts the daemon already supports (`apple.install`,
//! `provision_*`, the first-boot share, `fluxctl vz ipsw`) and which need a Mac to verify. See docs/macos-containers.md.

use anyhow::{Result, bail, ensure};
use std::path::Path;

/// Smallest disk a macOS install accepts, and the default (`fluxvm_apple::macos_install`).
pub const MIN_DISK_GIB: u64 = 40;
pub const DEFAULT_DISK_GIB: u64 = 64;

/// Syntax only (no filesystem): an absolute path to a `.ipsw` file. The parser uses this so `--dry-run` works on any machine.
pub fn check_path_syntax(p: &str) -> Result<(), String> {
    if p.starts_with("http://") || p.starts_with("https://") {
        return Err("URLs are not fetched: download the IPSW first".into());
    }
    if !p.starts_with('/') {
        return Err("the path must be absolute".into());
    }
    if !p.to_ascii_lowercase().ends_with(".ipsw") {
        return Err("the file must end in .ipsw".into());
    }
    Ok(())
}

/// The path exists on this Mac and looks like a restore image: a regular file that is a zip (`PK`), as every IPSW is.
pub fn check_file(p: &Path) -> Result<()> {
    use std::io::Read;
    let meta = std::fs::metadata(p).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
    ensure!(meta.is_file(), "{} is not a file", p.display());
    ensure!(
        meta.len() > 1 << 30,
        "{} is only {} bytes: an IPSW is several GB (a partial download?)",
        p.display(),
        meta.len()
    );
    let mut magic = [0u8; 2];
    std::fs::File::open(p)?.read_exact(&mut magic)?;
    ensure!(
        &magic == b"PK",
        "{} is not a zip archive, so it is not an IPSW",
        p.display()
    );
    Ok(())
}

/// How far the existing code gets with a phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Implemented in the daemon and run on hardware at least once (docs/macos.md).
    Daemon,
    /// Implemented as a script or option, exercised only in part.
    Partial,
    /// Not implemented for `mac build`: someone has to wire it and run it on a Mac.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub what: String,
    pub support: Support,
    pub how: &'static str,
}

/// The unattended install, in order.
pub fn phases(source: &str, disk_gib: u64, user: &str) -> Vec<Phase> {
    let p = |what: String, support, how| Phase { what, support, how };
    vec![
        p(
            if source == "latest" {
                "resolve and cache the newest restore image this Mac supports (about 26.6 GB)".into()
            } else {
                format!("use the restore image {source}")
            },
            Support::Daemon,
            "`fluxctl vz ipsw --download` (latest); a local path is used as is",
        ),
        p(
            format!("create a sparse {disk_gib} GiB disk (at least {MIN_DISK_GIB})"),
            Support::Daemon,
            "apple.install in POST /v1/vms (create_install_disk)",
        ),
        p(
            "run the installer: writes hardware.bin, identity.bin and auxiliary.bin beside the disk".into(),
            Support::Daemon,
            "fluxvm-vz-runner install; up to 3 hours",
        ),
        p(
            format!("provision the admin account {user} with no one at the console (account, password file, auto-login, Remote Login)"),
            Support::Daemon,
            "provision_username / provision_password_file / provision_auto_login / provision_remote_login (macOS 27 hosts); older hosts: apple.firstboot",
        ),
        p(
            "first boot, then wait for SSH".into(),
            Support::Daemon,
            "same wait as `mac run`",
        ),
        p(
            "authorize the daemon's SSH key and install the guest agent without a token".into(),
            Support::Partial,
            "scripts/macos-firstboot-helper.sh and crates/fluxvm-guest-agent/macos/install.sh",
        ),
        p(
            "disable sleep and the screen lock".into(),
            Support::Partial,
            "the template script in docs/macos.md (\"As an agent session\")",
        ),
        p(
            "shut down and store the disk, hardware model and auxiliary storage as the image".into(),
            Support::Daemon,
            "the same store step as `mac build`",
        ),
        p(
            "run the Macfile steps (RUN, COPY, ...) in the fresh guest".into(),
            Support::Missing,
            "`mac build` would continue from step 2 once the install is a build step",
        ),
    ]
}

pub fn render(source: &str, disk_gib: u64, user: &str) -> String {
    let mut out = format!("unattended install from IPSW {source}\n");
    for (i, ph) in phases(source, disk_gib, user).iter().enumerate() {
        let tag = match ph.support {
            Support::Daemon => "have",
            Support::Partial => "partial",
            Support::Missing => "MISSING",
        };
        out.push_str(&format!(
            "{}. [{tag}] {}\n     {}\n",
            i + 1,
            ph.what,
            ph.how
        ));
    }
    out.push_str(&format!(
        "needs: an Apple-silicon Mac with an unlocked login session, about {} GB free (the image, then the guest disk), and a hardware run of the whole sequence.\n",
        26 + disk_gib
    ));
    out
}

/// `mac ipsw [SOURCE]`: validate the source and print the plan.
pub fn run(source: &str, disk_gib: u64, user: &str) -> Result<()> {
    ensure!(
        disk_gib >= MIN_DISK_GIB,
        "--disk-gib must be at least {MIN_DISK_GIB}"
    );
    ensure!(
        !user.is_empty()
            && user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "--user must be a simple account name"
    );
    if source != "latest" {
        if let Err(m) = check_path_syntax(source) {
            bail!("IPSW source {source:?}: {m} (or use `latest`)");
        }
        check_file(Path::new(source))?;
    }
    print!("{}", render(source, disk_gib, user));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_syntax() {
        assert!(check_path_syntax("/Users/me/UniversalMac.ipsw").is_ok());
        assert!(check_path_syntax("/x/A.IPSW").is_ok());
        for bad in ["rel/a.ipsw", "/x/a.zip", "https://a/b.ipsw", "15.2"] {
            assert!(check_path_syntax(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_small_or_non_zip_file_is_not_an_ipsw() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.ipsw");
        std::fs::write(&p, b"PKtiny").unwrap();
        assert!(
            check_file(&p)
                .unwrap_err()
                .to_string()
                .contains("several GB")
        );
        assert!(check_file(&d.path().join("nope.ipsw")).is_err());
        assert!(check_file(d.path()).is_err());
        let f = std::fs::File::create(&p).unwrap();
        f.set_len((1 << 30) + 10).unwrap();
        assert!(
            check_file(&p)
                .unwrap_err()
                .to_string()
                .contains("not a zip")
        );
    }

    #[test]
    fn a_sparse_zip_looking_file_passes() {
        use std::io::Write;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.ipsw");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(b"PK").unwrap();
        f.set_len((1 << 30) + 10).unwrap();
        check_file(&p).unwrap();
    }

    #[test]
    fn the_plan_is_ordered_and_names_what_is_missing() {
        let ph = phases("latest", 64, "admin");
        assert!(ph[0].what.contains("newest restore image"));
        assert!(ph[1].what.contains("64 GiB"));
        assert!(ph[2].what.contains("installer"));
        assert_eq!(ph.last().unwrap().support, Support::Missing);
        let t = render("/a/b.ipsw", 80, "ci");
        assert!(
            t.contains("1. [have] use the restore image /a/b.ipsw"),
            "{t}"
        );
        assert!(t.contains("[MISSING]") && t.contains("106 GB"), "{t}");
    }

    #[test]
    fn run_validates_before_printing() {
        assert!(run("latest", 10, "admin").is_err());
        assert!(run("latest", 64, "a b").is_err());
        assert!(run("relative.ipsw", 64, "admin").is_err());
    }
}
