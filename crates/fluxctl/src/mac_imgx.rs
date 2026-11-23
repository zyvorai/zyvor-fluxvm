// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Docker image and instance parity that was still missing from `fluxctl mac`:
//!
//! - `mac export NAME --output FILE`: a stopped instance's disk as a `mac save` archive (so `mac import FILE` and `mac load` read it);
//! - a `FILE.sha256` checksum next to every `mac save`/`mac export` archive, checked by `mac load` / `mac import` when it is there;
//! - `mac manifest inspect REF`: what a registry holds for a reference (digest, platforms, layers), like `docker manifest inspect`;
//! - `mac stats --format`: the Go-template subset of `mac inspect --format` over the stats rows.
//!
//! The other commands in the request (`tag`, `history`, `save`, `load`, `wait`, `port`, `commit`, `image prune`, `search`, `stats`)
//! already existed. Unit-tested with fakes; not run against a real macOS VM or registry.

use crate::mac_errors::coded;
use crate::mac_image_ops as ops;
use crate::mac_store::Store;
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Subcommand, Debug)]
pub enum MacImgx {
    /// Write a STOPPED instance's disk to a tar file that `mac import` / `mac load` read back (`mac export NAME --output FILE`), with a
    /// `FILE.sha256` checksum beside it. Needs the daemon on this Mac. A sparse disk is archived at its full size: expect tens of GiB.
    #[command(
        after_help = "Example:\n  fluxctl mac stop build-1 && fluxctl mac export build-1 --output build-1.tar\n  fluxctl mac import build-1.tar build-1:copy   # on any Mac"
    )]
    Export {
        name: String,
        #[arg(long, value_name = "FILE")]
        output: PathBuf,
    },
    /// What a registry holds for an image reference, like `docker manifest inspect`.
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum ManifestCommand {
    /// Digest, platforms (for an index), config and layers of REFERENCE. Uses your `mac login` credentials.
    Inspect {
        reference: String,
        #[arg(long)]
        json: bool,
    },
}

impl MacImgx {
    /// `manifest inspect` talks to the registry only; `export` reads the daemon's instance record.
    pub fn is_client_side(&self) -> bool {
        matches!(self, Self::Manifest { .. })
    }
}

/// `FILE.sha256`: `HEXDIGEST  FILE-NAME`, the format `shasum -a 256 -c` reads.
pub fn sidecar_path(archive: &Path) -> PathBuf {
    let mut s = archive.as_os_str().to_owned();
    s.push(".sha256");
    PathBuf::from(s)
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Write the checksum file beside `archive`; returns the digest.
pub fn write_sidecar(archive: &Path) -> Result<String> {
    let digest = sha256_file(archive)?;
    let name = archive
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    std::fs::write(sidecar_path(archive), format!("{digest}  {name}\n"))
        .context("writing the .sha256 file")?;
    Ok(digest)
}

/// Check `archive` against its `.sha256` file. `Ok(None)` when there is no checksum file (older archives still load);
/// `Ok(Some(digest))` when it matches; a coded error when it does not.
pub fn verify_sidecar(archive: &Path) -> Result<Option<String>> {
    let side = sidecar_path(archive);
    if !side.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&side).context("reading the .sha256 file")?;
    let want = text.split_whitespace().next().unwrap_or("").to_lowercase();
    let got = sha256_file(archive)?;
    if want != got {
        return Err(coded(
            format!(
                "{} does not match its checksum file {}: the copy is damaged or was changed",
                archive.display(),
                side.display()
            ),
            "MAC_ARCHIVE_CHECKSUM",
        ));
    }
    Ok(Some(got))
}

/// Export an instance's disk files (already located) as an archive: stored under a throwaway name, saved, then forgotten.
pub fn export_files(store: &Store, name: &str, disk: &Path, ws: &Path, out: &Path) -> Result<()> {
    let tmp = format!(
        "fluxvm-export-{}:{}",
        name_slug(name),
        crate::mac_cmd::short_id()
    );
    crate::mac_cmd::store_instance_files(store, &tmp, disk, ws)?;
    let r = ops::save(store, &tmp, out);
    store.remove(&tmp).ok();
    r
}

fn name_slug(n: &str) -> String {
    n.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Why an instance cannot be exported yet; `None` when it can.
pub fn export_refusal(name: &str, status: &str) -> Option<anyhow::Error> {
    if matches!(status, "stopped" | "failed") {
        return None;
    }
    Some(coded(
        format!("{name} is {status}: a running disk is not consistent, so it cannot be exported"),
        "MAC_EXPORT_RUNNING",
    ))
}

pub async fn run_daemon(r: &crate::remote::Remote, cmd: MacImgx) -> Result<()> {
    match cmd {
        MacImgx::Export { name, output } => {
            let vm = crate::mac_cmd::find_instance(r, &name).await?;
            let status = crate::mac_inspect::status(&vm);
            if let Some(e) = export_refusal(&name, status) {
                return Err(e);
            }
            let disk = PathBuf::from(vm["disk"].as_str().context("the VM record has no disk")?);
            let ws = PathBuf::from(
                vm["workspace"]
                    .as_str()
                    .context("the VM record has no workspace")?,
            );
            if !disk.is_file() {
                bail!(
                    "{} is not readable here: `mac export` needs the daemon on this Mac",
                    disk.display()
                );
            }
            let store = crate::mac_store::Store::open(crate::mac_store::default_root()?)?;
            export_files(&store, &name, &disk, &ws, &output)?;
            let digest = write_sidecar(&output)?;
            println!(
                "exported {name} to {} (sha256 {digest}, also in {})",
                output.display(),
                sidecar_path(&output).display()
            );
            Ok(())
        }
        MacImgx::Manifest { .. } => bail!("not a daemon command"),
    }
}

pub async fn run_client(cmd: MacImgx) -> Result<()> {
    match cmd {
        MacImgx::Manifest {
            command: ManifestCommand::Inspect { reference, json },
        } => {
            let rep = fluxvm_image::oci_manage::inspect(&reference, crate::mac_cmd::creds(&reference))
                .await
                .map_err(|e| manifest_error(&reference, &e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                print!("{}", fluxvm_image::oci_manage::render_inspect(&rep));
            }
            Ok(())
        }
        MacImgx::Export { .. } => {
            bail!("`mac export` needs the daemon: pass --server (or select a context)")
        }
    }
}

fn manifest_error(reference: &str, e: &anyhow::Error) -> anyhow::Error {
    coded(
        format!("could not read the manifest of {reference}: {e:#}"),
        "MAC_MANIFEST_UNREADABLE",
    )
}

/// One `stats` record for `--format`: the daemon's numbers plus ready-made text fields.
pub fn stats_doc(name: &str, m: &Value) -> Value {
    let mem = m["memory_usage_bytes"].as_u64().unwrap_or(0);
    let rd = m["disk_read_bytes"].as_u64().unwrap_or(0);
    let wr = m["disk_write_bytes"].as_u64().unwrap_or(0);
    let cpu = m["cpu_usage_percent"].as_f64().unwrap_or(0.0);
    json!({
        "Name": name,
        "CPUPerc": format!("{cpu:.1}%"),
        "CPUPercent": cpu,
        "MemUsage": crate::mac_inspect::human_bytes(mem),
        "MemBytes": mem,
        "DiskRead": crate::mac_inspect::human_bytes(rd),
        "DiskWrite": crate::mac_inspect::human_bytes(wr),
        "DiskReadBytes": rd,
        "DiskWriteBytes": wr,
    })
}

/// `mac stats --format`: one line per instance, or a table when the format starts with `table`.
pub fn render_stats(docs: &[Value], fmt: &str) -> Result<String> {
    crate::mac_tmpl::render_rows(
        docs,
        fmt,
        "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.DiskRead}}\t{{.DiskWrite}}",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_store::REQUIRED_FILES;

    fn fixture() -> (tempfile::TempDir, Store, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path().join("store")).unwrap();
        let ws = root.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let disk = ws.join("disk.raw");
        std::fs::write(&disk, vec![7u8; 4096]).unwrap();
        std::fs::write(ws.join("hardware.bin"), b"hw").unwrap();
        std::fs::write(ws.join("auxiliary.bin"), b"aux").unwrap();
        (root, store, disk, ws)
    }

    #[test]
    fn export_then_import_round_trips_into_another_store() {
        let (root, store, disk, ws) = fixture();
        let out = root.path().join("vm.tar");
        export_files(&store, "Build 1", &disk, &ws, &out).unwrap();
        assert!(out.is_file());
        assert!(
            store.list().unwrap().is_empty(),
            "the throwaway name is forgotten"
        );
        write_sidecar(&out).unwrap();
        assert!(verify_sidecar(&out).unwrap().is_some());
        let other = Store::open(root.path().join("store2")).unwrap();
        let key = ops::load(&other, &out, Some("copy:1")).unwrap();
        assert_eq!(key, "copy:1");
        let dir = other.resolve("copy:1").unwrap();
        for f in REQUIRED_FILES {
            assert!(dir.join(f).is_file(), "{f}");
        }
        assert_eq!(
            std::fs::read(dir.join("disk.raw")).unwrap(),
            vec![7u8; 4096]
        );
    }

    #[test]
    fn checksum_catches_a_changed_archive_and_allows_a_missing_sidecar() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.tar");
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(verify_sidecar(&f).unwrap(), None);
        let digest = write_sidecar(&f).unwrap();
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let text = std::fs::read_to_string(sidecar_path(&f)).unwrap();
        assert!(text.ends_with("  a.tar\n"), "{text}");
        assert_eq!(
            verify_sidecar(&f).unwrap().as_deref(),
            Some(digest.as_str())
        );
        std::fs::write(&f, b"hellp").unwrap();
        let e = verify_sidecar(&f).unwrap_err().to_string();
        assert!(
            e.contains("does not match") && e.contains("explain MAC_ARCHIVE_CHECKSUM"),
            "{e}"
        );
    }

    #[test]
    fn running_instances_are_refused_with_a_next_step() {
        assert!(export_refusal("a", "stopped").is_none());
        let e = export_refusal("a", "running").unwrap().to_string();
        assert!(e.contains("explain MAC_EXPORT_RUNNING"), "{e}");
    }

    #[test]
    fn stats_format_renders_lines_and_tables() {
        let m = json!({"cpu_usage_percent": 12.34, "memory_usage_bytes": 2147483648u64,
                       "disk_read_bytes": 0, "disk_write_bytes": 1024});
        let docs = vec![stats_doc("web", &m)];
        assert_eq!(
            render_stats(&docs, "{{.Name}} {{.CPUPerc}} {{.MemUsage}}").unwrap(),
            "web 12.3% 2.0 GiB\n"
        );
        let t = render_stats(&docs, "table").unwrap();
        assert!(t.lines().count() == 2 && t.contains("web"), "{t}");
        assert!(render_stats(&docs, "{{if .Name}}x{{end}}").is_err());
    }

    #[test]
    fn parses_the_new_commands() {
        use clap::Parser;
        #[derive(Parser)]
        struct P {
            #[command(subcommand)]
            c: MacImgx,
        }
        let p = P::try_parse_from(["x", "export", "vm", "--output", "o.tar"]).unwrap();
        assert!(!p.c.is_client_side());
        let p = P::try_parse_from(["x", "manifest", "inspect", "r/x:1", "--json"]).unwrap();
        assert!(p.c.is_client_side());
    }

    #[test]
    fn manifest_errors_name_the_code() {
        let e = manifest_error("r/x:1", &anyhow::anyhow!("401")).to_string();
        assert!(
            e.contains("r/x:1") && e.contains("explain MAC_MANIFEST_UNREADABLE"),
            "{e}"
        );
    }
}
