// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! The path of this `fluxctl` to write into a config that must keep working later (an MCP client's entry, a LaunchAgent,
//! an ssh ProxyCommand).
//!
//! Homebrew installs into a versioned directory (`<prefix>/Cellar/fluxvm/0.4.4/bin/fluxctl`) and keeps a version-free
//! symlink (`<prefix>/opt/fluxvm/bin/fluxctl`) that follows upgrades. `std::env::current_exe` resolves to the versioned
//! one, which stops existing once `brew upgrade` and `brew cleanup` remove the old version.

use std::path::{Path, PathBuf};

/// This executable, canonical, except that a Homebrew Cellar path becomes its `opt/` equivalent when that exists.
pub fn stable_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(brew_opt_path(&exe).unwrap_or(exe))
}

/// `<prefix>/Cellar/<formula>/<version>/<rest>` becomes `<prefix>/opt/<formula>/<rest>`, if that file exists.
fn brew_opt_path(exe: &Path) -> Option<PathBuf> {
    let comps: Vec<_> = exe.components().collect();
    let i = comps.iter().position(|c| c.as_os_str() == "Cellar")?;
    // Cellar, <formula>, <version>, then at least the file itself.
    if comps.len() < i + 4 {
        return None;
    }
    let prefix: PathBuf = comps[..i].iter().collect();
    let rest: PathBuf = comps[i + 3..].iter().collect();
    let opt = prefix.join("opt").join(comps[i + 1].as_os_str()).join(rest);
    opt.exists().then_some(opt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_cellar_path_becomes_the_version_free_opt_path_when_it_exists() {
        let t = tempfile::tempdir().unwrap();
        let keg = t.path().join("Cellar/fluxvm/0.4.4");
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::write(keg.join("bin/fluxctl"), b"").unwrap();
        let exe = keg.join("bin/fluxctl");

        // No opt link yet: keep the real path.
        assert_eq!(brew_opt_path(&exe), None);

        std::fs::create_dir_all(t.path().join("opt")).unwrap();
        std::os::unix::fs::symlink(&keg, t.path().join("opt/fluxvm")).unwrap();
        assert_eq!(
            brew_opt_path(&exe),
            Some(t.path().join("opt/fluxvm/bin/fluxctl"))
        );
    }

    #[test]
    fn other_paths_are_left_alone() {
        assert_eq!(brew_opt_path(Path::new("/usr/local/bin/fluxctl")), None);
        assert_eq!(brew_opt_path(Path::new("/x/target/release/fluxctl")), None);
        // Cellar too close to the end to hold formula, version and file.
        assert_eq!(
            brew_opt_path(Path::new("/opt/homebrew/Cellar/fluxvm")),
            None
        );
    }
}
