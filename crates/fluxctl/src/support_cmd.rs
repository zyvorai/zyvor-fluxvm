// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl support-bundle`: one `.tar.gz` with what a maintainer needs, secrets removed (see
//! `fluxvm_core::support_bundle`). Works with the daemon down: each part it cannot get is listed as skipped, with the
//! reason and the next step. The Studio Home page's "Download support bundle" asks the daemon for the same archive.

use crate::cluster_cmd::{self, Env};
use crate::mac_fleet::Conn;
use anyhow::{Result, bail};
use fluxvm_core::config::Config;
use fluxvm_core::health;
use fluxvm_core::support_bundle::{BundleError, REVIEW_NOTE};
use fluxvm_core::support_collect::{self, Inputs};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(clap::Args, Clone)]
pub struct SupportBundleArgs {
    /// Where to write the bundle. Default: ~/fluxvm-support-<timestamp>.tar.gz (readable by you only).
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// How many lines of each log to keep (10 to 10000; default 1000; VM console logs are cut to 500).
    #[arg(long, value_name = "N")]
    pub log_lines: Option<usize>,
    /// Print the result as JSON: the path, the size and what was skipped.
    #[arg(long)]
    pub json: bool,
}

/// The default output path: `~/fluxvm-support-<timestamp>.tar.gz`, or the current folder with no home.
pub fn default_path(home: Option<&Path>, now: chrono::DateTime<chrono::Utc>) -> PathBuf {
    let name = support_collect::default_file_name(now);
    match home {
        Some(h) => h.join(name),
        None => PathBuf::from(name),
    }
}

/// Writes the bytes to a new file that only the owner can read. Refuses to replace an existing file.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::AlreadyExists {
                "that file already exists and is not replaced; pick another name with --out"
            } else {
                "check the folder exists and you may write there, or choose another place with --out"
            };
            anyhow::anyhow!(
                "could not create {}: {e}. {hint}. See `fluxctl explain SUPPORT_BUNDLE_WRITE_FAILED`",
                path.display()
            )
        })?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(|e| {
            // Do not leave half a bundle behind.
            let _ = std::fs::remove_file(path);
            anyhow::anyhow!(
                "could not write {}: {e}. The disk may be full. See `fluxctl explain SUPPORT_BUNDLE_WRITE_FAILED`",
                path.display()
            )
        })
}

fn unreachable_reason(listen: &str, e: &anyhow::Error) -> String {
    format!(
        "could not ask the daemon at {listen} ({e:#}); it is probably not running (`fluxctl explain DAEMON_DOWN`)"
    )
}

pub async fn run(a: &SupportBundleArgs, env: &Env, cfg: &Config) -> Result<i32> {
    let now = chrono::Utc::now();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let out = a
        .out
        .clone()
        .unwrap_or_else(|| default_path(home.as_deref(), now));
    if out.exists() {
        bail!(
            "{} already exists and is not replaced. Pick another name with --out. See `fluxctl explain SUPPORT_BUNDLE_WRITE_FAILED`",
            out.display()
        );
    }

    // The same data as `fluxctl cluster status --json`; it works with the daemon down.
    let conn = Conn {
        identity: None,
        known_hosts: None,
        accept_new_hostkey: false,
        jobs: 1,
        connect_timeout: 10,
        dry_run: false,
        json: false,
        step_timeout: 60,
    };
    let (health, daemon_version) = match cluster_cmd::gather(env, None, false, &conn).await {
        Ok(nodes) => {
            let up = nodes.first().and_then(|n| n.daemon_up) == Some(true);
            let ver = nodes.first().and_then(|n| n.version.clone());
            let h = health::assess_with(
                &nodes,
                cluster_cmd::gateway_items_now().await,
                now.timestamp().max(0) as u64,
            );
            let dv = match (up, ver) {
                (true, Some(v)) => Ok(v),
                (true, None) => Err("the daemon answered but did not say its version".to_owned()),
                (false, _) => {
                    Err("the daemon is not answering (`fluxctl explain DAEMON_DOWN`)".to_owned())
                }
            };
            (serde_json::to_value(&h).map_err(|e| e.to_string()), dv)
        }
        Err(e) => (
            Err(format!(
                "the health report could not be built: {e:#}. Run `fluxctl cluster status` to see why"
            )),
            Err("not checked because the health report failed".to_owned()),
        ),
    };

    let deadline = Duration::from_secs(15);
    let vms = match tokio::time::timeout(deadline, env.remote.list_vms_all(None, true)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(unreachable_reason(&env.listen, &e)),
        Err(_) => Err(format!(
            "the daemon at {} did not answer within 15 seconds",
            env.listen
        )),
    };
    let clusters = match tokio::time::timeout(
        deadline,
        env.remote.call(reqwest::Method::GET, "/v1/kube", None),
    )
    .await
    {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(unreachable_reason(&env.listen, &e)),
        Err(_) => Err(format!(
            "the daemon at {} did not answer within 15 seconds",
            env.listen
        )),
    };

    let cfg = cfg.clone();
    let lines = a.log_lines.unwrap_or(0);
    let built = tokio::task::spawn_blocking(move || {
        support_collect::build(&Inputs {
            cfg: &cfg,
            source: "fluxctl",
            fluxctl_version: env!("CARGO_PKG_VERSION").to_owned(),
            daemon_version,
            health,
            vms,
            clusters,
            log_lines: lines,
            now,
        })
    })
    .await?;
    let built = match built {
        Ok(b) => b,
        Err(e @ BundleError::SecretLeft { .. }) => {
            eprintln!("{e}");
            return Ok(1);
        }
        Err(e) => bail!("{e}"),
    };
    write_private(&out, &built.bytes)?;
    print!(
        "{}",
        render(&out, built.bytes.len(), &built.skipped, a.json)
    );
    Ok(0)
}

/// What is printed when the bundle is written.
pub fn render(path: &Path, bytes: usize, skipped: &[(String, String)], json: bool) -> String {
    if json {
        let v: Value = json!({
            "path": path.display().to_string(),
            "bytes": bytes,
            "skipped": skipped.iter().map(|(w, why)| json!({"what": w, "why": why})).collect::<Vec<_>>(),
            "note": REVIEW_NOTE,
        });
        return serde_json::to_string_pretty(&v).unwrap_or_default() + "\n";
    }
    let mut s = format!(
        "Support bundle written: {} ({} KiB)\n",
        path.display(),
        bytes.div_ceil(1024)
    );
    if skipped.is_empty() {
        s.push_str("Everything was collected.\n");
    } else {
        s.push_str("Some parts were skipped (the bundle is still useful):\n");
        for (what, why) in skipped {
            s.push_str(&format!("  - {what}: {why}\n"));
        }
    }
    s.push_str(&format!("{REVIEW_NOTE}\n"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_path_is_in_home_with_a_timestamp() {
        let t = chrono::DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let p = default_path(Some(Path::new("/home/a")), t);
        assert!(p.starts_with("/home/a"));
        assert!(
            p.to_string_lossy().contains("fluxvm-support-")
                && p.to_string_lossy().ends_with(".tar.gz")
        );
        assert!(!default_path(None, t).is_absolute());
    }

    #[test]
    fn the_file_is_private_and_never_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("b.tar.gz");
        write_private(&p, b"data").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let e = write_private(&p, b"other").unwrap_err().to_string();
        assert!(e.contains("already exists") && e.contains("SUPPORT_BUNDLE_WRITE_FAILED"));
        assert_eq!(std::fs::read(&p).unwrap(), b"data");
    }

    #[test]
    fn a_folder_that_does_not_exist_says_what_to_do() {
        let d = tempfile::tempdir().unwrap();
        let e = write_private(&d.path().join("no/such/dir/b.tgz"), b"x")
            .unwrap_err()
            .to_string();
        assert!(e.contains("--out") && e.contains("SUPPORT_BUNDLE_WRITE_FAILED"));
    }

    #[test]
    fn the_output_names_the_path_what_was_skipped_and_the_review_note() {
        let skipped = vec![("instances.json".to_owned(), "daemon down".to_owned())];
        let s = render(Path::new("/h/b.tar.gz"), 2049, &skipped, false);
        assert!(s.contains("/h/b.tar.gz") && s.contains("3 KiB"));
        assert!(s.contains("instances.json: daemon down"));
        assert!(
            s.contains("Review before sharing; secrets are removed but check for anything private")
        );
        let all = render(Path::new("/h/b.tar.gz"), 10, &[], false);
        assert!(all.contains("Everything was collected"));
        let j: Value =
            serde_json::from_str(&render(Path::new("/h/b"), 10, &skipped, true)).unwrap();
        assert_eq!(j["skipped"][0]["what"], "instances.json");
        assert_eq!(j["note"], REVIEW_NOTE);
    }

    #[tokio::test]
    async fn with_no_daemon_it_still_writes_a_bundle_and_lists_what_it_skipped() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config {
            // Nothing listens on this port.
            listen: "127.0.0.1:1".into(),
            state_dir: d.path().join("state"),
            ..Config::default()
        };
        let env = Env {
            listen: cfg.listen.clone(),
            state_dir: cfg.state_dir.clone(),
            remote: crate::remote::Remote::new(&cfg.listen, None),
        };
        let out = d.path().join("b.tar.gz");
        let a = SupportBundleArgs {
            out: Some(out.clone()),
            log_lines: Some(20),
            json: false,
        };
        assert_eq!(run(&a, &env, &cfg).await.unwrap(), 0);
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(
            fluxvm_core::support_bundle::verify_archive(&bytes).unwrap() >= 4,
            true
        );
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
        let mut manifest = String::new();
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            if e.path().unwrap().ends_with("MANIFEST.txt") {
                std::io::Read::read_to_string(&mut e, &mut manifest).unwrap();
            }
        }
        assert!(manifest.contains("instances.json"), "{manifest}");
        assert!(manifest.contains("DAEMON_DOWN"), "{manifest}");
        // A second run to the same path refuses instead of overwriting.
        assert!(
            run(&a, &env, &cfg)
                .await
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
    }
}
