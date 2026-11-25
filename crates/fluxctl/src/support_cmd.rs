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
    /// Print the result as JSON: the path, the size, the files inside and what was skipped.
    #[arg(long)]
    pub json: bool,
    /// Only show which files the bundle would hold (and what is skipped); write nothing.
    #[arg(long, alias = "dry-run")]
    pub list: bool,
    /// Write it without asking. By default a terminal is asked "Write it? [Y/n]" after the list of files; scripts are not asked.
    #[arg(long, short = 'y')]
    pub yes: bool,
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
    let files = fluxvm_core::support_bundle::list_archive(&built.bytes)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Show exactly what goes in before anything is written.
    if !a.json {
        print!("{}", render_contents(&files, &built.skipped, &out));
    }
    if a.list {
        if a.json {
            print!("{}", render_list_json(&files, &built.skipped, &out));
        } else {
            println!("Nothing was written (--list). Run `fluxctl support-bundle` to write it.");
        }
        return Ok(0);
    }
    if !a.yes && !a.json && stdin_and_stdout_are_terminals() && !confirm_write(&out) {
        println!("Nothing was written.");
        return Ok(0);
    }
    write_private(&out, &built.bytes)?;
    print!(
        "{}",
        render(&out, built.bytes.len(), &built.skipped, &files, a.json)
    );
    Ok(0)
}

fn stdin_and_stdout_are_terminals() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Asks once; anything but n/no writes it.
fn confirm_write(out: &Path) -> bool {
    print!("Write it to {}? [Y/n] ", out.display());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    !matches!(line.trim().to_ascii_lowercase().as_str(), "n" | "no")
}

/// The list shown before writing: every file with its size, what is skipped, and that nothing is sent.
pub fn render_contents(
    files: &[(String, u64)],
    skipped: &[(String, String)],
    out: &Path,
) -> String {
    let mut s = format!("The support bundle will hold {} files:\n", files.len());
    for (n, b) in files {
        s.push_str(&format!("  {n}  ({b} bytes)\n"));
    }
    if !skipped.is_empty() {
        s.push_str("Not included:\n");
        for (w, why) in skipped {
            s.push_str(&format!("  {w}: {why}\n"));
        }
    }
    s.push_str(&format!(
        "It is written only to {} (readable by you only). Nothing is uploaded or sent anywhere.\n",
        out.display()
    ));
    s
}

fn render_list_json(files: &[(String, u64)], skipped: &[(String, String)], out: &Path) -> String {
    let v: Value = json!({
        "written": false,
        "would_write_to": out.display().to_string(),
        "files": files.iter().map(|(n, b)| json!({"name": n, "bytes": b})).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|(w, why)| json!({"what": w, "why": why})).collect::<Vec<_>>(),
        "sent_anywhere": false,
        "note": REVIEW_NOTE,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

/// What a crash prints after the panic message: the one command that collects what a maintainer needs.
pub fn crash_hint() -> String {
    "\nfluxctl hit an internal error (a bug). To collect what is needed to fix it, run:\n  fluxctl support-bundle\nIt lists the files first, removes secrets, writes one .tar.gz on this machine and sends nothing anywhere. Attach it to a bug report at https://github.com/zyvorai/zyvor-fluxvm/issues if you wish.\n".to_owned()
}

/// Keeps the normal panic report and adds [`crash_hint`] once per process (a daemon can see many task panics).
pub fn install_crash_hook() {
    static SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        if !SHOWN.swap(true, std::sync::atomic::Ordering::SeqCst) {
            eprintln!("{}", crash_hint());
        }
    }));
}

/// What is printed when the bundle is written.
pub fn render(
    path: &Path,
    bytes: usize,
    skipped: &[(String, String)],
    files: &[(String, u64)],
    json: bool,
) -> String {
    if json {
        let v: Value = json!({
            "path": path.display().to_string(),
            "bytes": bytes,
            "files": files.iter().map(|(n, b)| json!({"name": n, "bytes": b})).collect::<Vec<_>>(),
            "sent_anywhere": false,
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
        let s = render(Path::new("/h/b.tar.gz"), 2049, &skipped, &[], false);
        assert!(s.contains("/h/b.tar.gz") && s.contains("3 KiB"));
        assert!(s.contains("instances.json: daemon down"));
        assert!(
            s.contains("Review before sharing; secrets are removed but check for anything private")
        );
        let all = render(Path::new("/h/b.tar.gz"), 10, &[], &[], false);
        assert!(all.contains("Everything was collected"));
        let j: Value = serde_json::from_str(&render(
            Path::new("/h/b"),
            10,
            &skipped,
            &[("a".to_owned(), 1)],
            true,
        ))
        .unwrap();
        assert_eq!(j["skipped"][0]["what"], "instances.json");
        assert_eq!(j["note"], REVIEW_NOTE);
        assert_eq!(j["files"][0]["name"], "a");
        assert_eq!(j["sent_anywhere"], false);
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
            list: false,
            yes: true,
        };
        assert_eq!(run(&a, &env, &cfg).await.unwrap(), 0);
        let bytes = std::fs::read(&out).unwrap();
        assert!(fluxvm_core::support_bundle::verify_archive(&bytes).unwrap() >= 4);
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

    #[test]
    fn the_list_before_writing_names_every_file_and_says_nothing_is_sent() {
        let files = vec![
            ("MANIFEST.txt".to_owned(), 120),
            ("host.txt".to_owned(), 900),
        ];
        let skipped = vec![("clusters.json".to_owned(), "daemon down".to_owned())];
        let s = render_contents(&files, &skipped, Path::new("/h/b.tar.gz"));
        assert!(s.contains("2 files") && s.contains("MANIFEST.txt  (120 bytes)"));
        assert!(s.contains("clusters.json: daemon down"));
        assert!(s.contains("/h/b.tar.gz") && s.contains("Nothing is uploaded or sent anywhere"));
    }

    #[test]
    fn the_crash_hint_names_the_command_and_promises_no_upload() {
        let h = crash_hint();
        assert!(h.contains("fluxctl support-bundle") && h.contains("sends nothing anywhere"));
    }

    #[tokio::test]
    async fn list_writes_nothing_and_the_bundle_has_no_secret_patterns() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            listen: "127.0.0.1:1".into(),
            state_dir: d.path().join("state"),
            ..Config::default()
        };
        cfg.auth.tokens.push(fluxvm_core::config::ApiToken {
            token: "tok_live_0123456789abcdefSECRET".into(),
            role: fluxvm_core::config::Role::Admin,
            name: None,
            tenant: None,
        });
        let env = Env {
            listen: cfg.listen.clone(),
            state_dir: cfg.state_dir.clone(),
            remote: crate::remote::Remote::new(&cfg.listen, None),
        };
        let out = d.path().join("b.tar.gz");
        let mut a = SupportBundleArgs {
            out: Some(out.clone()),
            log_lines: Some(20),
            json: true,
            list: true,
            yes: true,
        };
        assert_eq!(run(&a, &env, &cfg).await.unwrap(), 0);
        assert!(!out.exists(), "--list must not write the bundle");
        a.list = false;
        assert_eq!(run(&a, &env, &cfg).await.unwrap(), 0);
        let bytes = std::fs::read(&out).unwrap();
        let listed: Vec<String> = fluxvm_core::support_bundle::list_archive(&bytes)
            .unwrap()
            .into_iter()
            .map(|f| f.0)
            .collect();
        for want in ["README.txt", "MANIFEST.txt", "versions.json", "host.txt"] {
            assert!(listed.iter().any(|n| n == want), "{want} not in {listed:?}");
        }
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            let mut text = String::new();
            std::io::Read::read_to_string(&mut e, &mut text).unwrap();
            assert!(!text.contains("SECRET"), "token in {:?}", e.path().unwrap());
            assert!(
                fluxvm_core::support_bundle::find_leaks(&text).is_empty(),
                "secret pattern in {:?}",
                e.path().unwrap()
            );
        }
    }
}
