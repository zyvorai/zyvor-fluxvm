// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl ssh VM --cwd`: start the guest shell (or command) in the guest folder that shows the folder you are in, as
//! `limactl shell` does. It works only inside a folder shared into the VM (`shared_folders`) and says so otherwise,
//! rather than quietly starting somewhere else.

use crate::mac_fleet_remote::sh_quote;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Where a host folder appears in the guest.
#[derive(Debug, PartialEq, Eq)]
pub struct Place {
    pub guest_dir: String,
    /// The guest path of the share that holds it.
    pub share_root: String,
}

/// `(host_path, guest_path)` of the VM's shared folders, from its record.
fn shares_of(vm: &Value) -> Vec<(PathBuf, String)> {
    vm["request"]["shared_folders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| {
            Some((
                PathBuf::from(s["host_path"].as_str()?),
                s["guest_path"].as_str()?.to_owned(),
            ))
        })
        .collect()
}

/// Symlinks resolved on both sides (`/tmp` is `/private/tmp` on a Mac); a path that cannot be resolved is used as written.
fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The guest folder for `cwd`. With nested shares the deepest one wins. `None` when no share holds `cwd`.
pub fn find(shares: &[(PathBuf, String)], cwd: &Path) -> Option<Place> {
    let cwd = real(cwd);
    let mut best: Option<(usize, Place)> = None;
    for (host, guest) in shares {
        let host = real(host);
        let Ok(rest) = cwd.strip_prefix(&host) else {
            continue;
        };
        let depth = host.components().count();
        if best.as_ref().is_some_and(|(d, _)| *d >= depth) {
            continue;
        }
        let Some(rest) = rest.to_str() else {
            continue;
        };
        let root = guest.trim_end_matches('/');
        let guest_dir = match (root, rest) {
            ("", "") => "/".to_owned(),
            (_, "") => root.to_owned(),
            _ => format!("{root}/{rest}"),
        };
        let share_root = if root.is_empty() { "/" } else { root }.to_owned();
        best = Some((
            depth,
            Place {
                guest_dir,
                share_root,
            },
        ));
    }
    best.map(|(_, p)| p)
}

/// The remote shell line: check the share is mounted, enter the folder, then run `command` or a login shell.
fn script(p: &Place, command: &[String]) -> String {
    let dir = sh_quote(&p.guest_dir);
    let root = sh_quote(&p.share_root);
    let unmounted = sh_quote(&format!(
        "fluxctl: --cwd: the share at {} is configured but not mounted in the guest (see `mount` there)",
        p.share_root
    ));
    let missing = sh_quote(&format!(
        "fluxctl: --cwd: cannot enter {} in the guest (the folder is shared, check that it is mounted there; a macOS guest \
         shows shares under /Volumes/My Shared Files)",
        p.guest_dir
    ));
    // An unmounted share leaves an empty directory that `cd` would enter; guests without `mountpoint` skip that check.
    let enter = format!(
        "if command -v mountpoint >/dev/null 2>&1 && ! mountpoint -q {root}; then echo {unmounted} >&2; exit 1; fi; \
         cd {dir} || {{ echo {missing} >&2; exit 1; }}"
    );
    if command.is_empty() {
        format!("{enter}; exec \"${{SHELL:-/bin/sh}}\" -l")
    } else {
        format!("{enter}; {}", command.join(" "))
    }
}

/// What to pass to ssh for `--cwd`, given the VM record and the arguments after the VM on the command line.
pub fn ssh_args(vm: &Value, cwd: &Path, args: &[String]) -> Result<Vec<String>> {
    let command = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        _ => args,
    };
    if command.first().is_some_and(|a| a.starts_with('-')) {
        bail!(
            "with --cwd, what follows the VM is the command to run in the guest, so ssh options such as {:?} cannot be \
             used; drop --cwd to pass options to ssh",
            command[0]
        );
    }
    let name = vm["name"].as_str().unwrap_or("this VM");
    if vm["backend"] == "firecracker" {
        bail!(
            "--cwd is not available for VM {name}: Firecracker has no shared filesystem, its shared folders are copies \
             made when the VM starts, so the guest folder would not be the one you are in. Use a qemu or cloud-hypervisor \
             VM (vz on a Mac), or drop --cwd."
        );
    }
    let shares = shares_of(vm);
    let Some(place) = find(&shares, cwd) else {
        let shared = if shares.is_empty() {
            format!("VM {name} has no shared folders")
        } else {
            format!(
                "VM {name} shares only:\n{}",
                shares
                    .iter()
                    .map(|(h, g)| format!("  {} -> {g}", h.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        bail!(
            "the current folder {} is not shared into VM {name}, so --cwd cannot start there.\n{shared}\n\
             Change to a shared folder, drop --cwd to start in the guest's home, or create a VM that shares this one \
             (`shared_folders` in its spec, or `fluxctl run --keep -v HOST:GUEST`).",
            cwd.display()
        );
    };
    Ok(vec![script(&place, command)])
}

/// `--cwd` maps a folder of this machine, so the daemon given with `--server` must be on it.
pub fn require_local_server(base: &str) -> Result<()> {
    let url = reqwest::Url::parse(base).with_context(|| format!("bad server URL {base:?}"))?;
    let host = url.host_str().unwrap_or_default();
    let local = host == "localhost"
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|a| a.is_loopback());
    if !local {
        bail!(
            "--cwd maps a folder of this machine into the VM, but the daemon is at {base}: shared folders there are folders \
             of that machine. Drop --cwd."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shares(list: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
        list.iter()
            .map(|(h, g)| (PathBuf::from(h), (*g).to_owned()))
            .collect()
    }

    fn place(dir: &str, root: &str) -> Option<Place> {
        Some(Place {
            guest_dir: dir.into(),
            share_root: root.into(),
        })
    }

    #[test]
    fn maps_the_folder_through_the_deepest_share() {
        let s = shares(&[
            ("/nx/me", "/mnt/home/"),
            ("/nx/me/code", "/work"),
            ("/nx/other", "/"),
        ]);
        let at = |p: &str| find(&s, Path::new(p));
        assert_eq!(at("/nx/me"), place("/mnt/home", "/mnt/home"));
        assert_eq!(at("/nx/me/a/b c"), place("/mnt/home/a/b c", "/mnt/home"));
        assert_eq!(at("/nx/me/code/x"), place("/work/x", "/work"));
        assert_eq!(at("/nx/other"), place("/", "/"));
        assert_eq!(at("/nx/other/x"), place("/x", "/"));
        // A name that only starts the same is not inside the share.
        assert_eq!(at("/nx/meow"), None);
        assert_eq!(at("/nx"), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_resolve_on_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let realdir = tmp.path().join("real");
        std::fs::create_dir_all(realdir.join("sub")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&realdir, &link).unwrap();
        let via_link = vec![(link.clone(), "/g".to_owned())];
        assert_eq!(find(&via_link, &realdir.join("sub")), place("/g/sub", "/g"));
        let via_real = vec![(realdir, "/g".to_owned())];
        assert_eq!(find(&via_real, &link.join("sub")), place("/g/sub", "/g"));
    }

    #[test]
    fn says_what_is_shared_when_the_folder_is_not() {
        let vm = json!({"name": "web", "request": {"shared_folders": [{"host_path": "/nx/src", "guest_path": "/src"}]}});
        let err = ssh_args(&vm, Path::new("/nx/docs"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("/nx/docs is not shared into VM web"), "{err}");
        assert!(err.contains("/nx/src -> /src"), "{err}");
        let none = ssh_args(
            &json!({"name": "web", "request": {}}),
            Path::new("/nx/docs"),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(none.contains("has no shared folders"), "{none}");
    }

    #[test]
    fn live_share_backends_only() {
        let vm = |backend: &str| {
            json!({"name": "v", "backend": backend,
                   "request": {"shared_folders": [{"host_path": "/nx/src", "guest_path": "/src"}]}})
        };
        for ok in ["vz", "qemu", "cloud-hypervisor"] {
            ssh_args(&vm(ok), Path::new("/nx/src"), &[]).unwrap();
        }
        let err = ssh_args(&vm("firecracker"), Path::new("/nx/src"), &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("copies"), "{err}");
    }

    #[test]
    fn builds_a_login_shell_or_a_command_in_the_folder() {
        let vm = json!({"name": "web", "request": {"shared_folders": [{"host_path": "/nx/src", "guest_path": "/src"}]}});
        let shell = ssh_args(&vm, Path::new("/nx/src/it's"), &[]).unwrap();
        assert_eq!(shell.len(), 1);
        assert!(shell[0].contains(r"cd '/src/it'\''s' ||"), "{}", shell[0]);
        assert!(shell[0].contains("mountpoint -q '/src'"), "{}", shell[0]);
        assert!(
            shell[0].ends_with("exec \"${SHELL:-/bin/sh}\" -l"),
            "{}",
            shell[0]
        );
        let args: Vec<String> = ["--", "make", "-j4"].map(String::from).to_vec();
        let run = ssh_args(&vm, Path::new("/nx/src"), &args).unwrap();
        assert!(run[0].ends_with("; make -j4"), "{}", run[0]);
        let err = ssh_args(&vm, Path::new("/nx/src"), &["-A".to_owned()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("ssh options"), "{err}");
    }

    /// The script is real shell: run it here with a stand-in `mountpoint`.
    #[cfg(unix)]
    #[test]
    fn the_script_runs_in_the_folder_and_refuses_an_unmounted_share() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let dir = root.join("a b'c");
        std::fs::create_dir(&dir).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let run = |mounted: bool, guest_dir: &Path| {
            let fake = bin.join("mountpoint");
            std::fs::write(&fake, format!("#!/bin/sh\nexit {}\n", u8::from(!mounted))).unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let p = Place {
                guest_dir: guest_dir.display().to_string(),
                share_root: root.display().to_string(),
            };
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script(&p, &["pwd".to_owned()]))
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .output()
                .unwrap()
        };
        let ok = run(true, &dir);
        assert!(ok.status.success(), "{ok:?}");
        assert_eq!(
            String::from_utf8_lossy(&ok.stdout).trim(),
            dir.display().to_string()
        );
        let unmounted = run(false, &dir);
        assert!(!unmounted.status.success());
        assert!(unmounted.stdout.is_empty(), "the command must not run");
        assert!(String::from_utf8_lossy(&unmounted.stderr).contains("not mounted"));
        let missing = run(true, &root.join("nope"));
        assert!(!missing.status.success() && missing.stdout.is_empty());
        assert!(String::from_utf8_lossy(&missing.stderr).contains("cannot enter"));
    }

    #[test]
    fn only_a_daemon_on_this_machine_can_use_cwd() {
        for ok in [
            "http://127.0.0.1:7788",
            "http://localhost:7788",
            "https://[::1]:7788",
        ] {
            require_local_server(ok).unwrap();
        }
        assert!(require_local_server("http://mac.local:7788").is_err());
        assert!(require_local_server("http://192.168.1.5:7788").is_err());
    }
}
