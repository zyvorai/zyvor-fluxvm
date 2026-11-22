// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac cp`: copy files between this Mac and a running instance, both directions, with `scp -r` over the same SSH
//! identity handling as `mac exec`. `NAME:/path` is the guest side, as in `docker cp`.
//!
//! Not verified against a real macOS guest: the argument lists are checked as data only.

use anyhow::{Result, bail, ensure};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Local(PathBuf),
    Guest { name: String, path: String },
}

/// `NAME:/path` is a guest path when NAME has no `/` and does not start with `.` or `~` (so `./a:b` stays a local file).
pub fn parse_endpoint(s: &str) -> Result<Endpoint> {
    ensure!(!s.is_empty(), "an empty path");
    if let Some((name, path)) = s.split_once(':')
        && !name.is_empty()
        && !name.contains('/')
        && !name.starts_with(['.', '~'])
    {
        ensure!(
            !path.is_empty(),
            "{s:?}: the path after the colon is empty (NAME:/path)"
        );
        return Ok(Endpoint::Guest {
            name: name.to_string(),
            path: path.to_string(),
        });
    }
    Ok(Endpoint::Local(PathBuf::from(s)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub name: String,
    pub guest_path: String,
    pub local: PathBuf,
    /// Mac to guest.
    pub upload: bool,
}

pub fn plan(src: &str, dst: &str) -> Result<Transfer> {
    match (parse_endpoint(src)?, parse_endpoint(dst)?) {
        (Endpoint::Local(l), Endpoint::Guest { name, path }) => Ok(Transfer {
            name,
            guest_path: path,
            local: l,
            upload: true,
        }),
        (Endpoint::Guest { name, path }, Endpoint::Local(l)) => Ok(Transfer {
            name,
            guest_path: path,
            local: l,
            upload: false,
        }),
        (Endpoint::Local(_), Endpoint::Local(_)) => {
            bail!("one side must be NAME:/path in an instance (see `fluxctl mac cp --help`)")
        }
        (Endpoint::Guest { .. }, Endpoint::Guest { .. }) => {
            bail!("copying between two instances is not supported: copy to this Mac first")
        }
    }
}

/// The `scp` arguments (without the program name). `identity` is the generated key, when there is one.
pub fn scp_args(
    t: &Transfer,
    user: &str,
    ip: &str,
    identity: Option<&std::path::Path>,
) -> Vec<String> {
    let mut a: Vec<String> = [
        "-r",
        "-o",
        "StrictHostKeyChecking=no",
        "-o",
        "UserKnownHostsFile=/dev/null",
        "-o",
        "LogLevel=ERROR",
    ]
    .map(String::from)
    .to_vec();
    if let Some(k) = identity {
        a.push("-i".into());
        a.push(k.display().to_string());
    }
    let remote = format!("{user}@{ip}:{}", t.guest_path);
    let local = t.local.display().to_string();
    if t.upload {
        a.push(local);
        a.push(remote);
    } else {
        a.push(remote);
        a.push(local);
    }
    a
}

/// Run the copy. The upload source must exist; a download creates nothing before `scp` succeeds.
pub async fn copy(t: &Transfer, user: &str, ip: &str) -> Result<()> {
    use anyhow::Context;
    if t.upload {
        ensure!(
            t.local.exists(),
            "{} does not exist on this Mac",
            t.local.display()
        );
    }
    let identity = crate::run::generated_identity();
    let status = tokio::process::Command::new("scp")
        .args(scp_args(t, user, ip, identity.as_deref()))
        .status()
        .await
        .context("running scp (is the OpenSSH client installed?)")?;
    ensure!(status.success(), "scp exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(
            parse_endpoint("web:/tmp/a").unwrap(),
            Endpoint::Guest {
                name: "web".into(),
                path: "/tmp/a".into()
            }
        );
        assert_eq!(
            parse_endpoint("web:rel/dir").unwrap(),
            Endpoint::Guest {
                name: "web".into(),
                path: "rel/dir".into()
            }
        );
        for local in ["./a:b", "/abs/a:b", "~/x:y", "plain", "dir/with:colon"] {
            assert!(
                matches!(parse_endpoint(local).unwrap(), Endpoint::Local(_)),
                "{local}"
            );
        }
        assert!(parse_endpoint("web:").is_err());
        assert!(parse_endpoint("").is_err());
    }

    #[test]
    fn direction_is_decided_by_the_guest_side() {
        let up = plan("./site", "web:/Library/WebServer").unwrap();
        assert!(up.upload);
        assert_eq!(
            (up.name.as_str(), up.guest_path.as_str()),
            ("web", "/Library/WebServer")
        );
        let down = plan("web:/var/log/system.log", "./out").unwrap();
        assert!(!down.upload);
        assert_eq!(down.local, PathBuf::from("./out"));
        assert!(plan("a", "b").unwrap_err().to_string().contains("one side"));
        assert!(
            plan("a:/x", "b:/y")
                .unwrap_err()
                .to_string()
                .contains("two instances")
        );
    }

    #[test]
    fn scp_argument_order_follows_the_direction() {
        let up = plan("./f", "web:/tmp/f").unwrap();
        let a = scp_args(&up, "admin", "192.168.64.5", None);
        assert_eq!(&a[a.len() - 2..], ["./f", "admin@192.168.64.5:/tmp/f"]);
        assert!(
            a.contains(&"-r".to_string()) && a.contains(&"StrictHostKeyChecking=no".to_string())
        );
        let down = plan("web:/tmp/f", "./f").unwrap();
        let k = PathBuf::from("/k/key");
        let a = scp_args(&down, "ci", "10.0.0.2", Some(&k));
        assert_eq!(&a[a.len() - 2..], ["ci@10.0.0.2:/tmp/f", "./f"]);
        let i = a.iter().position(|x| x == "-i").unwrap();
        assert_eq!(a[i + 1], "/k/key");
    }

    #[test]
    fn an_upload_of_a_missing_file_fails_before_scp() {
        let t = plan("/nonexistent-xyz/f", "web:/tmp").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let e = rt
            .block_on(copy(&t, "admin", "1.2.3.4"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not exist on this Mac"), "{e}");
    }
}
