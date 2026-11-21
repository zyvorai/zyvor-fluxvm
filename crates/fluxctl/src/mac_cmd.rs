// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! `fluxctl mac`: the image half of the Docker-style workflow for macOS guests (docs/macos-containers-design.md, phase 1).
//! Client-side only: it talks to a registry and the local store, never the daemon.

use crate::mac_store::{self, Entry, REQUIRED_FILES, Store};
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use fluxvm_image as image;
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum MacCommand {
    /// List the local macOS images (`name:tag`, digest, size on disk).
    Images {
        #[arg(long)]
        json: bool,
    },
    /// Pull `registry/repo:tag` into the local store (credentials: FLUXVM_REGISTRY_USER / FLUXVM_REGISTRY_PASSWORD).
    Pull { reference: String },
    /// Push a local image to a registry. REMOTE defaults to REFERENCE when that already names a registry.
    /// Apple's licence terms limit where macOS images may be stored: use a private registry.
    Push {
        reference: String,
        remote: Option<String>,
    },
    /// Give an image a second name (no copy).
    Tag { source: String, target: String },
    /// Remove a name; the files go with the last name that uses them.
    Rmi { reference: String },
    /// Add a template directory (disk.raw, hardware.bin, auxiliary.bin) to the store as `name:tag`; an APFS clone, so it is instant
    /// on the same volume.
    Import { dir: PathBuf, reference: String },
}

pub async fn run(command: MacCommand) -> Result<()> {
    let store = Store::open(mac_store::default_root()?)?;
    match command {
        MacCommand::Images { json } => {
            let rows = store.list()?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!(
                        rows.iter()
                            .map(|(n, e)| serde_json::json!({
                                "reference": n, "digest": e.digest, "source": e.source,
                                "size_bytes": dir_size(&store.root().join(&e.dir)),
                            }))
                            .collect::<Vec<_>>()
                    ))?
                );
            } else {
                print!("{}", render_images(&store, &rows));
            }
        }
        MacCommand::Pull { reference } => {
            let key = mac_store::normalize(&reference)?;
            let scratch = store.scratch("pull");
            let report = image::vm_artifact::pull(&key, &scratch, creds())
                .await
                .with_context(|| format!("pulling {key}"))?;
            let adopted = check_template(&scratch)
                .and_then(|()| mac_store::dir_for_digest(&report.manifest_digest))
                .and_then(|dir| store.adopt(&scratch, &dir).map(|()| dir));
            let dir = match adopted {
                Ok(d) => d,
                Err(e) => {
                    std::fs::remove_dir_all(&scratch).ok();
                    return Err(e);
                }
            };
            store.add(
                &key,
                Entry {
                    dir,
                    digest: report.manifest_digest.clone(),
                    source: key.clone(),
                    added_unix: 0,
                },
            )?;
            println!("pulled {key} ({})", report.manifest_digest);
        }
        MacCommand::Push { reference, remote } => {
            let dir = store.resolve(&reference)?;
            let target = remote_reference(&reference, remote.as_deref())?;
            let opts = image::vm_artifact::PushOptions {
                chunk_size: 0,
                exclude: Vec::new(),
                os: Some("macos".into()),
                name: Some(reference.clone()),
                annotations: Default::default(),
            };
            let out = image::vm_artifact::push(&dir, &target, creds(), &opts)
                .await
                .with_context(|| format!("pushing {} to {target}", dir.display()))?;
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        MacCommand::Tag { source, target } => println!("tagged {}", store.tag(&source, &target)?),
        MacCommand::Rmi { reference } => {
            let deleted = store.remove(&reference)?;
            println!(
                "untagged {reference}{}",
                if deleted {
                    "; files deleted"
                } else {
                    "; files kept (another name uses them)"
                }
            );
        }
        MacCommand::Import { dir, reference } => {
            check_template(&dir)?;
            let key = mac_store::normalize(&reference)?;
            let name = format!("local-{}", short_id());
            let scratch = store.scratch("import");
            std::fs::create_dir_all(&scratch)?;
            for f in REQUIRED_FILES {
                if let Err(e) = clone_file(&dir.join(f), &scratch.join(f)) {
                    std::fs::remove_dir_all(&scratch).ok();
                    return Err(e);
                }
            }
            store.adopt(&scratch, &name)?;
            store.add(
                &key,
                Entry {
                    dir: name,
                    digest: String::new(),
                    source: "local".into(),
                    added_unix: 0,
                },
            )?;
            println!("imported {} as {key}", dir.display());
        }
    }
    Ok(())
}

fn creds() -> Option<image::oci_registry::Credentials> {
    let username = std::env::var("FLUXVM_REGISTRY_USER").ok()?;
    let password = std::env::var("FLUXVM_REGISTRY_PASSWORD").unwrap_or_default();
    Some(image::oci_registry::Credentials { username, password })
}

fn check_template(dir: &Path) -> Result<()> {
    for f in REQUIRED_FILES {
        ensure!(
            dir.join(f).is_file(),
            "{} is not a macOS template: {f} is missing (needs {})",
            dir.display(),
            REQUIRED_FILES.join(", ")
        );
    }
    Ok(())
}

/// APFS copy-on-write clone where available (`cp -c`), else a plain copy.
fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    let cloned = std::process::Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dst)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !cloned {
        std::fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    }
    Ok(())
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// Whether the first path component names a registry host (`ghcr.io`, `localhost:5000`), as Docker decides it.
fn names_a_registry(reference: &str) -> bool {
    match reference.split_once('/') {
        Some((host, _)) => host.contains('.') || host.contains(':') || host == "localhost",
        None => false,
    }
}

fn remote_reference(local: &str, explicit: Option<&str>) -> Result<String> {
    match explicit {
        Some(r) => {
            ensure!(
                names_a_registry(r),
                "{r:?} does not name a registry (e.g. ghcr.io/acme/macos-xcode:15)"
            );
            mac_store::normalize(r)
        }
        None if names_a_registry(local) => mac_store::normalize(local),
        None => bail!(
            "{local:?} is a local name: pass a registry reference, e.g. `fluxctl mac push {local} ghcr.io/acme/{local}`"
        ),
    }
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn human(bytes: u64) -> String {
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= G {
        format!("{:.1} GiB", b / G)
    } else {
        format!("{:.0} MiB", b / M)
    }
}

fn render_images(store: &Store, rows: &[(String, Entry)]) -> String {
    if rows.is_empty() {
        return "no macOS images (fluxctl mac pull REF, or fluxctl mac import DIR NAME)\n".into();
    }
    let w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(9).max(9);
    let mut out = format!(
        "{:<w$}  {:<14}  {:>9}  SOURCE\n",
        "REFERENCE", "DIGEST", "SIZE"
    );
    for (name, e) in rows {
        let digest = if e.digest.is_empty() {
            "-".to_string()
        } else {
            e.digest
                .trim_start_matches("sha256:")
                .chars()
                .take(12)
                .collect()
        };
        out.push_str(&format!(
            "{name:<w$}  {digest:<14}  {:>9}  {}\n",
            human(dir_size(&store.root().join(&e.dir))),
            e.source
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_hosts_are_told_from_local_names() {
        assert!(names_a_registry("ghcr.io/acme/mac:15"));
        assert!(names_a_registry("localhost:5000/mac"));
        assert!(names_a_registry("localhost/mac"));
        assert!(!names_a_registry("macos-xcode:15"));
        assert!(!names_a_registry("acme/mac"));
    }

    #[test]
    fn push_targets() {
        assert_eq!(
            remote_reference("ghcr.io/a/m", None).unwrap(),
            "ghcr.io/a/m:latest"
        );
        assert_eq!(
            remote_reference("m:15", Some("ghcr.io/a/m:15")).unwrap(),
            "ghcr.io/a/m:15"
        );
        let e = remote_reference("m:15", None).unwrap_err().to_string();
        assert!(
            e.contains("local name") && e.contains("ghcr.io/acme/m:15"),
            "{e}"
        );
        assert!(remote_reference("m", Some("acme/m")).is_err());
    }

    #[test]
    fn a_template_missing_a_file_is_named() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("disk.raw"), b"x").unwrap();
        let e = check_template(t.path()).unwrap_err().to_string();
        assert!(e.contains("hardware.bin is missing"), "{e}");
    }

    #[test]
    fn import_images_and_rmi_round_trip_through_the_store() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(src.path().join(f), vec![7u8; 2048]).unwrap();
        }
        // SAFETY of env use: this is the only test in the crate that sets FLUXVM_MAC_STORE.
        unsafe { std::env::set_var("FLUXVM_MAC_STORE", root.path()) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(run(MacCommand::Import {
            dir: src.path().into(),
            reference: "base:15".into(),
        }))
        .unwrap();
        rt.block_on(run(MacCommand::Tag {
            source: "base:15".into(),
            target: "ci".into(),
        }))
        .unwrap();
        let store = Store::open(root.path()).unwrap();
        let rows = store.list().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["base:15", "ci:latest"]
        );
        let text = render_images(&store, &rows);
        assert!(
            text.contains("REFERENCE") && text.contains("base:15") && text.contains("local"),
            "{text}"
        );
        rt.block_on(run(MacCommand::Rmi {
            reference: "base:15".into(),
        }))
        .unwrap();
        assert!(store.resolve("ci").is_ok(), "the other name still resolves");
        rt.block_on(run(MacCommand::Rmi {
            reference: "ci".into(),
        }))
        .unwrap();
        assert!(store.list().unwrap().is_empty());
        let e = rt
            .block_on(run(MacCommand::Import {
                dir: root.path().into(),
                reference: "x".into(),
            }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a macOS template"), "{e}");
    }
}
