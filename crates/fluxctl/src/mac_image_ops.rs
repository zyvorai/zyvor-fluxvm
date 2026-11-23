// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Image-side commands for `fluxctl mac` beyond pull/push/tag: `image inspect`, `history`, `save` / `load`, `commit`,
//! `image prune`, `system df` / `system prune`. Works on the local store (`mac_store`) and on instance records from the daemon.
//!
//! Sizes are the bytes actually allocated on disk (`st_blocks`), not the apparent size: a template's `disk.raw` is sparse.
//! APFS clones share blocks, so the totals can overstate what deleting would free.
//!
//! `save` and `load` shell out to `tar`. A sparse `disk.raw` is archived at its apparent size, so an archive of a 64 GiB disk is
//! 64 GiB. Not verified with a real template: the unit tests use small files.

use crate::mac_config::{self, HistoryEntry, ImageConfig};
use crate::mac_inspect::{IMAGE_LABEL, human_bytes, status};
use crate::mac_store::{self, Entry, REQUIRED_FILES, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "manifest.json";

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `(apparent, allocated)` bytes of the regular files directly inside `dir`.
pub fn dir_usage(dir: &Path) -> (u64, u64) {
    let mut apparent = 0;
    let mut alloc = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for m in rd
            .flatten()
            .filter_map(|e| e.metadata().ok())
            .filter(|m| m.is_file())
        {
            apparent += m.len();
            alloc += m.blocks() * 512;
        }
    }
    (apparent, alloc)
}

/// Allocated bytes of one file (0 when it is not reachable from this machine).
pub fn file_alloc(p: &Path) -> u64 {
    std::fs::metadata(p).map_or(0, |m| m.blocks() * 512)
}

fn find_entry(store: &Store, reference: &str) -> Result<(String, Entry)> {
    let key = mac_store::normalize(reference)?;
    let entry = store
        .list()?
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, e)| e);
    match entry {
        Some(e) => Ok((key, e)),
        None => bail!("no image {key} (fluxctl mac images)"),
    }
}

/// `mac image inspect REF`: the store entry, its files, sizes and the Macfile-derived config.
pub fn image_inspect(store: &Store, reference: &str) -> Result<Value> {
    let (key, e) = find_entry(store, reference)?;
    let dir = store.resolve(&key)?;
    let files: Vec<Value> = REQUIRED_FILES
        .iter()
        .map(|f| {
            let m = std::fs::metadata(dir.join(f));
            json!({
                "name": f,
                "size_bytes": m.as_ref().map_or(0, |m| m.len()),
                "allocated_bytes": m.as_ref().map_or(0, |m| m.blocks() * 512),
            })
        })
        .collect();
    let (apparent, alloc) = dir_usage(&dir);
    let cfg = mac_config::load(&dir)?;
    let names: Vec<String> = store
        .list()?
        .into_iter()
        .filter(|(_, o)| o.dir == e.dir)
        .map(|(k, _)| k)
        .collect();
    Ok(json!({
        "reference": key,
        "also_tagged": names.into_iter().filter(|n| *n != key).collect::<Vec<_>>(),
        "directory": dir,
        "digest": e.digest,
        "source": e.source,
        "added_unix": e.added_unix,
        "size_bytes": apparent,
        "allocated_bytes": alloc,
        "files": files,
        "config": serde_json::to_value(&cfg)?,
    }))
}

/// `mac history REF`: one row per recorded step, newest first. An image without a recorded build shows where it came from.
pub fn history(store: &Store, reference: &str) -> Result<Vec<HistoryEntry>> {
    let (_, e) = find_entry(store, reference)?;
    let dir = store.resolve(reference)?;
    let mut h = mac_config::load(&dir)?.history;
    if h.is_empty() {
        h.push(HistoryEntry {
            created_unix: e.added_unix,
            created_by: match e.source.as_str() {
                "local" => "mac import".to_string(),
                "build" => "mac build (no step record)".to_string(),
                s => format!("mac pull {s}"),
            },
            comment: String::new(),
        });
    }
    h.reverse();
    Ok(h)
}

pub fn render_history(rows: &[HistoryEntry]) -> String {
    let mut out = format!("{:<20}  CREATED BY\n", "CREATED");
    for r in rows {
        let when = if r.created_unix == 0 {
            "-".to_string()
        } else {
            chrono::DateTime::from_timestamp(r.created_unix as i64, 0)
                .map_or("-".into(), |t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        };
        let c = if r.comment.is_empty() {
            String::new()
        } else {
            format!("   # {}", r.comment)
        };
        out.push_str(&format!("{when:<20}  {}{c}\n", r.created_by));
    }
    out
}

/// Stamp a build's history with the time it finished and write its config next to the template.
pub fn write_build_config(store: &Store, key: &str, mut cfg: ImageConfig) -> Result<()> {
    let now = now_unix();
    for h in &mut cfg.history {
        h.created_unix = now;
    }
    mac_config::save(&store.resolve(key)?, &cfg)
}

/// The config a committed instance gets: the image it came from, plus a history line.
pub fn commit_config(
    base: Option<ImageConfig>,
    instance: &str,
    message: &str,
    author: &str,
    now: u64,
) -> ImageConfig {
    let mut c = base.unwrap_or_default();
    c.history.push(HistoryEntry {
        created_unix: now,
        created_by: format!(
            "mac commit {instance}{}",
            if author.is_empty() {
                String::new()
            } else {
                format!(" (by {author})")
            }
        ),
        comment: message.to_string(),
    });
    c
}

/// Directories in the store that no name uses (a crashed pull, a removed index entry), and stale scratch directories
/// (`.build-…`, older than `scratch_age_secs`).
pub fn orphans(store: &Store, scratch_age_secs: u64) -> Result<Vec<PathBuf>> {
    let used: BTreeSet<String> = store.list()?.into_iter().map(|(_, e)| e.dir).collect();
    let mut out = Vec::new();
    for e in std::fs::read_dir(store.root())
        .with_context(|| format!("reading {}", store.root().display()))?
        .flatten()
    {
        let Ok(ft) = e.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|d| d.as_secs() >= scratch_age_secs);
            if old {
                out.push(e.path());
            }
        } else if !used.contains(&name) {
            out.push(e.path());
        }
    }
    out.sort();
    Ok(out)
}

fn tree_alloc(p: &Path) -> u64 {
    dir_usage(p).1
}

/// Delete the given orphan directories (never anything outside the store root). Returns `(count, bytes freed)`.
pub fn remove_orphans(store: &Store, dirs: &[PathBuf]) -> Result<(usize, u64)> {
    let mut bytes = 0;
    for d in dirs {
        ensure!(
            d.parent() == Some(store.root()),
            "refusing to delete {} (not directly inside the store)",
            d.display()
        );
        bytes += tree_alloc(d);
        std::fs::remove_dir_all(d).with_context(|| format!("deleting {}", d.display()))?;
    }
    Ok((dirs.len(), bytes))
}

/// Image names that no instance uses (`--all`). `used` is the `fluxvm.mac.image` label of every instance, as `run` wrote it.
pub fn unused_images(store: &Store, used: &[String]) -> Result<Vec<String>> {
    let used: BTreeSet<String> = used
        .iter()
        .filter_map(|u| mac_store::normalize(u).ok())
        .collect();
    Ok(store
        .list()?
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| !used.contains(k))
        .collect())
}

#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Df {
    pub image_names: usize,
    pub images_active: usize,
    pub image_bytes: u64,
    pub image_reclaimable: u64,
    pub orphan_dirs: usize,
    pub orphan_bytes: u64,
    pub instances: usize,
    pub instances_running: usize,
    pub instance_bytes: u64,
    /// Disks of stopped or failed instances.
    pub instance_reclaimable: u64,
}

/// `used`/`instances` come from the daemon; sizes of instance disks count only when the disk is readable from here.
pub fn compute_df(store: &Store, instances: &[&Value]) -> Result<Df> {
    let used: Vec<String> = instances
        .iter()
        .filter_map(|v| v["labels"][IMAGE_LABEL].as_str().map(String::from))
        .collect();
    let used_keys: BTreeSet<String> = used
        .iter()
        .filter_map(|u| mac_store::normalize(u).ok())
        .collect();
    let rows = store.list()?;
    let mut df = Df {
        image_names: rows.len(),
        ..Df::default()
    };
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut active_dirs: BTreeSet<&str> = BTreeSet::new();
    for (k, e) in &rows {
        if used_keys.contains(k) {
            active_dirs.insert(&e.dir);
            df.images_active += 1;
        }
    }
    for (_, e) in &rows {
        if seen.insert(&e.dir) {
            let a = tree_alloc(&store.root().join(&e.dir));
            df.image_bytes += a;
            if !active_dirs.contains(e.dir.as_str()) {
                df.image_reclaimable += a;
            }
        }
    }
    let orph = orphans(store, 3600)?;
    df.orphan_dirs = orph.len();
    df.orphan_bytes = orph.iter().map(|p| tree_alloc(p)).sum();
    df.instances = instances.len();
    for v in instances {
        let a = v["disk"].as_str().map_or(0, |p| file_alloc(Path::new(p)));
        df.instance_bytes += a;
        if status(v) == "running" || status(v) == "paused" {
            df.instances_running += 1;
        } else {
            df.instance_reclaimable += a;
        }
    }
    Ok(df)
}

pub fn render_df(d: &Df) -> String {
    let mut out = format!(
        "{:<12}  {:>5}  {:>6}  {:>10}  RECLAIMABLE\n",
        "TYPE", "TOTAL", "ACTIVE", "SIZE"
    );
    out.push_str(&format!(
        "{:<12}  {:>5}  {:>6}  {:>10}  {}\n",
        "Images",
        d.image_names,
        d.images_active,
        human_bytes(d.image_bytes),
        human_bytes(d.image_reclaimable)
    ));
    out.push_str(&format!(
        "{:<12}  {:>5}  {:>6}  {:>10}  {}\n",
        "Instances",
        d.instances,
        d.instances_running,
        human_bytes(d.instance_bytes),
        human_bytes(d.instance_reclaimable)
    ));
    if d.orphan_dirs > 0 {
        out.push_str(&format!(
            "{} unreferenced store director{} ({}): `fluxctl mac image prune`\n",
            d.orphan_dirs,
            if d.orphan_dirs == 1 { "y" } else { "ies" },
            human_bytes(d.orphan_bytes)
        ));
    }
    out.push_str("Sizes are allocated bytes. APFS clones share blocks, so deleting may free less than shown. Instance sizes count only disks this Mac can read.\n");
    out
}

/// Which instances `mac prune` removes: stopped or failed ones; with `orphaned`, also any non-running instance whose image
/// name is no longer in the store. Never a running or paused one.
pub fn prunable_instances<'a>(
    instances: &[&'a Value],
    image_names: &BTreeSet<String>,
    orphaned: bool,
) -> Vec<&'a Value> {
    instances
        .iter()
        .copied()
        .filter(|v| {
            let st = status(v);
            if st == "running" || st == "paused" || st == "creating" {
                return false;
            }
            if st == "stopped" || st == "failed" {
                return true;
            }
            orphaned
                && v["labels"][IMAGE_LABEL]
                    .as_str()
                    .and_then(|i| mac_store::normalize(i).ok())
                    .is_some_and(|k| !image_names.contains(&k))
        })
        .collect()
}

/// A stopped instance that is also an orphan stays in the list above; this is the extra set `--orphaned` adds for display.
#[cfg(test)]
pub fn is_orphan(vm: &Value, image_names: &BTreeSet<String>) -> bool {
    vm["labels"][IMAGE_LABEL]
        .as_str()
        .and_then(|i| mac_store::normalize(i).ok())
        .is_some_and(|k| !image_names.contains(&k))
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Manifest {
    reference: String,
    format: u32,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    source: String,
}

/// The entries `tar -tf` listed must be exactly the template files, an optional `config.json` and `manifest.json`, with no
/// paths that escape the extraction directory.
pub fn check_archive_entries(entries: &[String]) -> Result<()> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for raw in entries {
        let e = raw.trim_start_matches("./").trim_end_matches('/');
        if e.is_empty() {
            continue;
        }
        ensure!(
            !e.starts_with('/') && !e.split('/').any(|p| p == ".."),
            "the archive has an unsafe path {raw:?}"
        );
        let ok = REQUIRED_FILES.contains(&e) || e == MANIFEST || e == mac_config::CONFIG_FILE;
        ensure!(
            ok,
            "the archive has an unexpected entry {raw:?}: not a `fluxctl mac save` file"
        );
        seen.insert(e);
    }
    ensure!(
        seen.contains(MANIFEST),
        "the archive has no {MANIFEST}: not a `fluxctl mac save` file"
    );
    for f in REQUIRED_FILES {
        ensure!(seen.contains(f), "the archive is missing {f}");
    }
    Ok(())
}

fn run_tar(args: &[&std::ffi::OsStr]) -> Result<std::process::Output> {
    let out = std::process::Command::new("tar")
        .args(args)
        .output()
        .context("running tar")?;
    ensure!(
        out.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out)
}

/// `mac save REF --output FILE`: a tar of the template's files, its config and a manifest naming the reference.
pub fn save(store: &Store, reference: &str, out: &Path) -> Result<()> {
    let (key, e) = find_entry(store, reference)?;
    let dir = store.resolve(&key)?;
    let scratch = store.scratch("save");
    std::fs::create_dir_all(&scratch)?;
    let result = (|| {
        std::fs::write(
            scratch.join(MANIFEST),
            serde_json::to_vec_pretty(&Manifest {
                reference: key.clone(),
                format: 1,
                digest: e.digest.clone(),
                source: e.source.clone(),
            })?,
        )?;
        let mut files: Vec<&str> = REQUIRED_FILES.to_vec();
        if dir.join(mac_config::CONFIG_FILE).is_file() {
            files.push(mac_config::CONFIG_FILE);
        }
        let mut args: Vec<&std::ffi::OsStr> = vec![
            "-cf".as_ref(),
            out.as_os_str(),
            "-C".as_ref(),
            dir.as_os_str(),
        ];
        args.extend(files.iter().map(|f| std::ffi::OsStr::new(*f)));
        args.extend(["-C".as_ref(), scratch.as_os_str(), MANIFEST.as_ref()]);
        run_tar(&args)?;
        crate::mac_imgx::write_sidecar(out).map(|_| ())
    })();
    std::fs::remove_dir_all(&scratch).ok();
    if result.is_err() {
        std::fs::remove_file(out).ok();
        std::fs::remove_file(crate::mac_imgx::sidecar_path(out)).ok();
    }
    result
}

/// `mac load -i FILE [REF]`: unpack a `save` archive into the store. Returns the reference it was stored under.
pub fn load(store: &Store, archive: &Path, as_ref: Option<&str>) -> Result<String> {
    ensure!(archive.is_file(), "{} is not a file", archive.display());
    crate::mac_imgx::verify_sidecar(archive)?;
    let listing = run_tar(&["-tf".as_ref(), archive.as_os_str()])?;
    let entries: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    check_archive_entries(&entries)?;
    let scratch = store.scratch("load");
    std::fs::create_dir_all(&scratch)?;
    let result = (|| -> Result<String> {
        run_tar(&[
            "-xf".as_ref(),
            archive.as_os_str(),
            "-C".as_ref(),
            scratch.as_os_str(),
        ])?;
        let m: Manifest = serde_json::from_slice(
            &std::fs::read(scratch.join(MANIFEST)).context("reading the manifest")?,
        )
        .context("the archive's manifest is corrupt")?;
        ensure!(m.format == 1, "unsupported archive format {}", m.format);
        crate::mac_cmd::check_template(&scratch)?;
        std::fs::remove_file(scratch.join(MANIFEST)).ok();
        Ok(match as_ref {
            Some(r) => mac_store::normalize(r)?,
            None => mac_store::normalize(&m.reference)?,
        })
    })();
    let key = match result {
        Ok(k) => k,
        Err(e) => {
            std::fs::remove_dir_all(&scratch).ok();
            return Err(e);
        }
    };
    let dir_name = format!("local-{}", crate::mac_cmd::short_id());
    store.adopt(&scratch, &dir_name)?;
    store.add(
        &key,
        Entry {
            dir: dir_name,
            digest: String::new(),
            source: "load".into(),
            added_unix: 0,
        },
    )?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(names: &[&str]) -> (tempfile::TempDir, Store) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        for (i, n) in names.iter().enumerate() {
            let dir = format!("local-{i:012}");
            let p = root.path().join(&dir);
            std::fs::create_dir_all(&p).unwrap();
            for f in REQUIRED_FILES {
                std::fs::write(p.join(f), vec![i as u8 + 1; 8192]).unwrap();
            }
            store
                .add(
                    n,
                    Entry {
                        dir,
                        digest: String::new(),
                        source: "local".into(),
                        added_unix: 100,
                    },
                )
                .unwrap();
        }
        (root, store)
    }

    #[test]
    fn inspect_reports_files_sizes_config_and_other_tags() {
        let (_r, s) = store_with(&["base:15"]);
        s.tag("base:15", "ci").unwrap();
        let mut cfg = ImageConfig::default();
        cfg.set_env("A", "1");
        mac_config::save(&s.resolve("base:15").unwrap(), &cfg).unwrap();
        let v = image_inspect(&s, "base:15").unwrap();
        assert_eq!(v["reference"], "base:15");
        assert_eq!(v["also_tagged"], json!(["ci:latest"]));
        assert!(v["size_bytes"].as_u64().unwrap() >= 3 * 8192, "{v}");
        assert_eq!(v["files"].as_array().unwrap().len(), 3);
        assert_eq!(v["config"]["env"][0][0], "A");
        assert_eq!(v["source"], "local");
        assert!(
            image_inspect(&s, "nope")
                .unwrap_err()
                .to_string()
                .contains("no image nope:latest")
        );
    }

    #[test]
    fn history_falls_back_to_the_source_and_lists_newest_first() {
        let (_r, s) = store_with(&["a:1"]);
        let h = history(&s, "a:1").unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].created_by, "mac import");
        let cfg = ImageConfig {
            history: vec![
                HistoryEntry {
                    created_unix: 10,
                    created_by: "RUN one".into(),
                    comment: String::new(),
                },
                HistoryEntry {
                    created_unix: 20,
                    created_by: "RUN two".into(),
                    comment: "note".into(),
                },
            ],
            ..Default::default()
        };
        mac_config::save(&s.resolve("a:1").unwrap(), &cfg).unwrap();
        let h = history(&s, "a:1").unwrap();
        assert_eq!(h[0].created_by, "RUN two");
        let t = render_history(&h);
        assert!(
            t.contains("RUN two   # note") && t.contains("1970-01-01"),
            "{t}"
        );
        assert!(t.find("RUN two").unwrap() < t.find("RUN one").unwrap());
    }

    #[test]
    fn write_build_config_stamps_the_history() {
        let (_r, s) = store_with(&["a:1"]);
        let mut cfg = ImageConfig::default();
        cfg.history.push(HistoryEntry {
            created_unix: 0,
            created_by: "RUN x".into(),
            comment: String::new(),
        });
        write_build_config(&s, "a:1", cfg).unwrap();
        let back = mac_config::load(&s.resolve("a:1").unwrap()).unwrap();
        assert!(back.history[0].created_unix > 1_700_000_000);
    }

    #[test]
    fn commit_config_extends_the_base_history() {
        let mut base = ImageConfig::default();
        base.set_env("K", "v");
        base.history.push(HistoryEntry {
            created_unix: 1,
            created_by: "RUN a".into(),
            comment: String::new(),
        });
        let c = commit_config(Some(base), "web", "tweaked", "me", 99);
        assert_eq!(c.env.len(), 1);
        assert_eq!(c.history.len(), 2);
        assert_eq!(c.history[1].created_by, "mac commit web (by me)");
        assert_eq!(c.history[1].comment, "tweaked");
        let c = commit_config(None, "web", "", "", 5);
        assert_eq!(c.history[0].created_by, "mac commit web");
    }

    #[test]
    fn orphans_are_found_and_only_store_children_are_deleted() {
        let (r, s) = store_with(&["a:1"]);
        std::fs::create_dir_all(r.path().join("deadbeef")).unwrap();
        std::fs::write(r.path().join("deadbeef/disk.raw"), vec![1u8; 4096]).unwrap();
        std::fs::create_dir_all(r.path().join(".build-1-2")).unwrap();
        // a fresh scratch dir is in use by someone: kept; with age 0 it counts as stale.
        assert_eq!(orphans(&s, 3600).unwrap(), vec![r.path().join("deadbeef")]);
        let all = orphans(&s, 0).unwrap();
        assert_eq!(all.len(), 2);
        let (n, bytes) = remove_orphans(&s, &all).unwrap();
        assert_eq!(n, 2);
        assert!(bytes > 0);
        assert!(!r.path().join("deadbeef").exists());
        assert!(s.resolve("a:1").is_ok(), "named images stay");
        let e = remove_orphans(&s, &[PathBuf::from("/tmp")])
            .unwrap_err()
            .to_string();
        assert!(e.contains("refusing"), "{e}");
    }

    #[test]
    fn unused_images_ignore_those_an_instance_uses() {
        let (_r, s) = store_with(&["a:1", "b:1", "c"]);
        let u = unused_images(&s, &["a:1".into(), "c".into()]).unwrap();
        assert_eq!(u, ["b:1"]);
    }

    fn inst(name: &str, st: &str, image: &str, disk: &str) -> Value {
        json!({"name": name, "status": st, "disk": disk, "labels": {"fluxvm.mac": "instance", "fluxvm.mac.image": image}})
    }

    #[test]
    fn df_counts_images_instances_and_reclaimable() {
        let (r, s) = store_with(&["a:1", "b:1"]);
        s.tag("a:1", "a2").unwrap();
        std::fs::create_dir_all(r.path().join("orphan")).unwrap();
        std::fs::write(r.path().join("orphan/x"), vec![1u8; 8192]).unwrap();
        let disk_run = r.path().join("d1");
        let disk_stop = r.path().join("d2");
        std::fs::write(&disk_run, vec![1u8; 8192]).unwrap();
        std::fs::write(&disk_stop, vec![1u8; 8192]).unwrap();
        let i1 = inst("x", "running", "a:1", disk_run.to_str().unwrap());
        let i2 = inst("y", "stopped", "a:1", disk_stop.to_str().unwrap());
        let i3 = inst("z", "stopped", "gone:1", "/nonexistent/disk");
        let d = compute_df(&s, &[&i1, &i2, &i3]).unwrap();
        assert_eq!(d.image_names, 3);
        assert_eq!(
            d.images_active, 1,
            "a:1 is used; a2 and b:1 are not by name"
        );
        assert_eq!(d.instances, 3);
        assert_eq!(d.instances_running, 1);
        assert!(
            d.image_bytes > 0 && d.image_reclaimable > 0 && d.image_reclaimable < d.image_bytes
        );
        assert!(d.instance_reclaimable > 0 && d.instance_reclaimable < d.instance_bytes);
        assert_eq!(d.orphan_dirs, 1);
        let t = render_df(&d);
        assert!(
            t.contains("Images")
                && t.contains("Instances")
                && t.contains("1 unreferenced store directory"),
            "{t}"
        );
    }

    #[test]
    fn prunable_instances_never_include_live_ones() {
        let names: BTreeSet<String> = ["a:1".to_string()].into();
        let run = inst("r", "running", "gone:1", "");
        let pau = inst("p", "paused", "gone:1", "");
        let stop = inst("s", "stopped", "a:1", "");
        let fail = inst("f", "failed", "a:1", "");
        let all = [&run, &pau, &stop, &fail];
        let p = prunable_instances(&all, &names, false);
        assert_eq!(
            p.iter()
                .map(|v| v["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["s", "f"]
        );
        let p = prunable_instances(&all, &names, true);
        assert_eq!(p.len(), 2, "running and paused stay even when orphaned");
        assert!(is_orphan(&run, &names) && !is_orphan(&stop, &names));
    }

    #[test]
    fn archive_listings_are_checked() {
        let ok: Vec<String> = [
            "disk.raw",
            "./hardware.bin",
            "auxiliary.bin",
            "config.json",
            "manifest.json",
        ]
        .map(String::from)
        .to_vec();
        check_archive_entries(&ok).unwrap();
        let mut bad = ok.clone();
        bad.push("../evil".into());
        assert!(
            check_archive_entries(&bad)
                .unwrap_err()
                .to_string()
                .contains("unsafe")
        );
        let mut abs = ok.clone();
        abs.push("/etc/passwd".into());
        assert!(check_archive_entries(&abs).is_err());
        let mut extra = ok.clone();
        extra.push("README".into());
        assert!(
            check_archive_entries(&extra)
                .unwrap_err()
                .to_string()
                .contains("unexpected")
        );
        assert!(
            check_archive_entries(&ok[..4])
                .unwrap_err()
                .to_string()
                .contains("manifest.json")
        );
        let missing: Vec<String> = ["disk.raw", "manifest.json"].map(String::from).to_vec();
        assert!(
            check_archive_entries(&missing)
                .unwrap_err()
                .to_string()
                .contains("missing hardware.bin")
        );
    }

    #[test]
    fn save_then_load_round_trips_into_another_store() {
        let (_r, s) = store_with(&["base:15"]);
        let cfg = ImageConfig {
            workdir: Some("/opt".into()),
            ..Default::default()
        };
        mac_config::save(&s.resolve("base:15").unwrap(), &cfg).unwrap();
        let out = tempfile::tempdir().unwrap();
        let tar = out.path().join("base.tar");
        save(&s, "base:15", &tar).unwrap();
        assert!(tar.is_file());

        let (_r2, s2) = store_with(&[]);
        let key = load(&s2, &tar, None).unwrap();
        assert_eq!(key, "base:15");
        let dir = s2.resolve("base:15").unwrap();
        for f in REQUIRED_FILES {
            assert_eq!(std::fs::read(dir.join(f)).unwrap().len(), 8192);
        }
        assert_eq!(
            mac_config::load(&dir).unwrap().workdir.as_deref(),
            Some("/opt")
        );
        assert!(!dir.join(MANIFEST).exists());
        // loading under another name
        let key = load(&s2, &tar, Some("renamed:2")).unwrap();
        assert_eq!(key, "renamed:2");
        assert_eq!(s2.list().unwrap().len(), 2);
    }

    #[test]
    fn load_rejects_a_foreign_tar_and_leaves_nothing_behind() {
        let (r, s) = store_with(&[]);
        let w = tempfile::tempdir().unwrap();
        std::fs::write(w.path().join("hello.txt"), "x").unwrap();
        let tar = w.path().join("x.tar");
        run_tar(&[
            "-cf".as_ref(),
            tar.as_os_str(),
            "-C".as_ref(),
            w.path().as_os_str(),
            "hello.txt".as_ref(),
        ])
        .unwrap();
        let e = load(&s, &tar, None).unwrap_err().to_string();
        assert!(e.contains("unexpected entry"), "{e}");
        assert_eq!(
            std::fs::read_dir(r.path())
                .unwrap()
                .flatten()
                .filter(|e| e.path().is_dir())
                .count(),
            0
        );
        assert!(load(&s, &w.path().join("missing.tar"), None).is_err());
    }
}
