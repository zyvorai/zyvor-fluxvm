// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Local store of macOS guest templates for `fluxctl mac` (docs/macos-containers-design.md, phase 1).
//!
//! A template is a directory (`disk.raw`, `hardware.bin`, `auxiliary.bin`), the same layout `vm-image push` and `pull` use.
//! The store maps `name:tag` to such a directory through one small `index.json`. Several tags may share a directory; the
//! directory is deleted only when the last tag that points at it goes.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Files a macOS template must have for `run` to clone it.
pub const REQUIRED_FILES: [&str; 3] = ["disk.raw", "hardware.bin", "auxiliary.bin"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// Directory name under the store root (the manifest digest hex, or a local id for `import`).
    pub dir: String,
    /// `sha256:…` when the template came from a registry; empty for a local import.
    #[serde(default)]
    pub digest: String,
    /// Where it came from: a registry reference or `local`.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub added_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    images: BTreeMap<String, Entry>,
}

pub struct Store {
    root: PathBuf,
}

/// `name` → `name:latest`; a digest reference (`@sha256:…`) and an explicit tag are left alone.
pub fn normalize(reference: &str) -> Result<String> {
    let r = reference.trim();
    ensure!(!r.is_empty(), "an image reference is empty");
    ensure!(
        !r.contains(char::is_whitespace) && !r.contains(".."),
        "{r:?} is not a valid image reference"
    );
    if r.contains('@') {
        return Ok(r.to_string());
    }
    // A colon after the last slash is a tag; one before it is a registry port (`localhost:5000/x`).
    let last = r.rsplit('/').next().unwrap_or(r);
    if last.contains(':') {
        let (_, tag) = last.rsplit_once(':').unwrap();
        ensure!(!tag.is_empty(), "{r:?} has an empty tag");
        Ok(r.to_string())
    } else {
        Ok(format!("{r}:latest"))
    }
}

pub fn default_root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_STORE").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").context("HOME is not set (or set FLUXVM_MAC_STORE)")?;
    Ok(PathBuf::from(home).join(".fluxvm").join("mac-images"))
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    fn load(&self) -> Result<Index> {
        match std::fs::read(self.index_path()) {
            Ok(b) => serde_json::from_slice(&b)
                .with_context(|| format!("{} is corrupt", self.index_path().display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e).context("reading the image index"),
        }
    }

    fn save(&self, idx: &Index) -> Result<()> {
        let tmp = self.root.join(".index.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(idx)?)?;
        std::fs::rename(&tmp, self.index_path())?; // atomic: a crash leaves the old index
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<(String, Entry)>> {
        Ok(self.load()?.images.into_iter().collect())
    }

    /// The template directory for a reference, checked to hold the files a clone needs.
    pub fn resolve(&self, reference: &str) -> Result<PathBuf> {
        let key = normalize(reference)?;
        let idx = self.load()?;
        let Some(e) = idx.images.get(&key) else {
            bail!(
                "no image {key} in {} (fluxctl mac images)",
                self.root.display()
            );
        };
        let dir = self.root.join(&e.dir);
        for f in REQUIRED_FILES {
            ensure!(
                dir.join(f).is_file(),
                "image {key} is damaged: {} is missing",
                dir.join(f).display()
            );
        }
        Ok(dir)
    }

    /// Register `dir` (already inside the store root, named `entry.dir`) under `reference`.
    pub fn add(&self, reference: &str, entry: Entry) -> Result<String> {
        let key = normalize(reference)?;
        ensure!(
            !entry.dir.contains('/') && !entry.dir.starts_with('.'),
            "bad store directory name {:?}",
            entry.dir
        );
        let mut idx = self.load()?;
        let mut entry = entry;
        if entry.added_unix == 0 {
            entry.added_unix = now_unix();
        }
        let replaced = idx.images.insert(key.clone(), entry);
        self.save(&idx)?;
        if let Some(old) = replaced {
            self.gc_dir(&old.dir)?;
        }
        Ok(key)
    }

    /// `docker tag`: a second name for an existing image (same directory, no copy).
    pub fn tag(&self, source: &str, target: &str) -> Result<String> {
        let skey = normalize(source)?;
        let tkey = normalize(target)?;
        let mut idx = self.load()?;
        let Some(e) = idx.images.get(&skey).cloned() else {
            bail!("no image {skey} to tag");
        };
        let replaced = idx.images.insert(tkey.clone(), e);
        self.save(&idx)?;
        if let Some(old) = replaced {
            self.gc_dir(&old.dir)?;
        }
        Ok(tkey)
    }

    /// `docker rmi`: drop a tag; the directory goes with the last tag that used it. Returns whether the files were deleted.
    pub fn remove(&self, reference: &str) -> Result<bool> {
        let key = normalize(reference)?;
        let mut idx = self.load()?;
        let Some(e) = idx.images.remove(&key) else {
            bail!("no image {key}");
        };
        self.save(&idx)?;
        self.gc_dir(&e.dir)
    }

    fn gc_dir(&self, dir: &str) -> Result<bool> {
        if self.load()?.images.values().any(|e| e.dir == dir) {
            return Ok(false);
        }
        let p = self.root.join(dir);
        if p.is_dir() {
            std::fs::remove_dir_all(&p).with_context(|| format!("deleting {}", p.display()))?;
        }
        Ok(true)
    }

    /// A fresh scratch directory inside the store (same volume as the final home, so the move is a rename).
    pub fn scratch(&self, label: &str) -> PathBuf {
        self.root
            .join(format!(".{label}-{}-{}", std::process::id(), now_unix()))
    }

    /// Adopt a finished scratch directory as `dir_name`, or discard it when that directory already exists.
    pub fn adopt(&self, scratch: &Path, dir_name: &str) -> Result<()> {
        let dest = self.root.join(dir_name);
        if dest.exists() {
            std::fs::remove_dir_all(scratch).ok();
            return Ok(());
        }
        std::fs::rename(scratch, &dest)
            .with_context(|| format!("moving {} into the store", scratch.display()))
    }
}

/// `sha256:abcd…` → the directory name `abcd…` (first 32 hex characters keep paths short and collisions impossible in practice).
pub fn dir_for_digest(digest: &str) -> Result<String> {
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    ensure!(
        hex.len() >= 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "{digest:?} is not a sha256 digest"
    );
    Ok(hex[..32].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_template(store: &Store, dir: &str) {
        let p = store.root().join(dir);
        std::fs::create_dir_all(&p).unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(p.join(f), b"x").unwrap();
        }
    }

    fn entry(dir: &str) -> Entry {
        Entry {
            dir: dir.into(),
            digest: String::new(),
            source: "local".into(),
            added_unix: 0,
        }
    }

    #[test]
    fn references_get_latest_and_keep_ports_digests_and_tags() {
        assert_eq!(normalize("macos-xcode").unwrap(), "macos-xcode:latest");
        assert_eq!(normalize("macos-xcode:15").unwrap(), "macos-xcode:15");
        assert_eq!(
            normalize("localhost:5000/macos").unwrap(),
            "localhost:5000/macos:latest"
        );
        assert_eq!(
            normalize("localhost:5000/macos:2").unwrap(),
            "localhost:5000/macos:2"
        );
        assert_eq!(
            normalize("ghcr.io/a/b@sha256:ab").unwrap(),
            "ghcr.io/a/b@sha256:ab"
        );
        assert!(normalize("").is_err());
        assert!(normalize("a b").is_err());
        assert!(normalize("../x").is_err());
        assert!(normalize("x:").is_err());
    }

    #[test]
    fn add_resolve_and_a_damaged_image_is_reported() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        make_template(&s, "aa");
        assert_eq!(s.add("base", entry("aa")).unwrap(), "base:latest");
        assert_eq!(s.resolve("base").unwrap(), t.path().join("aa"));
        std::fs::remove_file(t.path().join("aa/auxiliary.bin")).unwrap();
        let e = s.resolve("base:latest").unwrap_err().to_string();
        assert!(e.contains("damaged") && e.contains("auxiliary.bin"), "{e}");
        assert!(
            s.resolve("nope")
                .unwrap_err()
                .to_string()
                .contains("no image nope:latest")
        );
    }

    #[test]
    fn a_shared_directory_survives_until_its_last_tag_goes() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        make_template(&s, "aa");
        s.add("base:15", entry("aa")).unwrap();
        s.tag("base:15", "ci:stable").unwrap();
        assert!(!s.remove("base:15").unwrap(), "ci:stable still uses it");
        assert!(t.path().join("aa").is_dir());
        assert!(s.remove("ci:stable").unwrap());
        assert!(!t.path().join("aa").exists());
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn retagging_over_a_name_frees_the_old_directory() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        make_template(&s, "aa");
        make_template(&s, "bb");
        s.add("img", entry("aa")).unwrap();
        s.add("img", entry("bb")).unwrap();
        assert!(
            !t.path().join("aa").exists(),
            "the replaced template is deleted"
        );
        assert!(t.path().join("bb").is_dir());
        assert!(s.tag("missing", "x").is_err());
    }

    #[test]
    fn a_path_in_the_directory_name_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        assert!(s.add("x", entry("../etc")).is_err());
        assert!(s.add("x", entry(".hidden")).is_err());
    }

    #[test]
    fn adopt_moves_a_scratch_dir_or_discards_a_duplicate() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        let a = s.scratch("pull");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("disk.raw"), b"1").unwrap();
        s.adopt(&a, "abc").unwrap();
        assert!(t.path().join("abc/disk.raw").is_file() && !a.exists());
        let b = t.path().join(".dup");
        std::fs::create_dir_all(&b).unwrap();
        s.adopt(&b, "abc").unwrap();
        assert!(
            !b.exists(),
            "an identical pull is dropped, the stored copy stays"
        );
    }

    #[test]
    fn digest_directory_names() {
        let d = "sha256:".to_string() + &"AB".repeat(32);
        assert_eq!(dir_for_digest(&d).unwrap(), "ab".repeat(16));
        assert!(dir_for_digest("sha256:zz").is_err());
        assert!(dir_for_digest("sha256:ab").is_err());
    }

    #[test]
    fn a_corrupt_index_is_an_error_not_an_empty_store() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::open(t.path()).unwrap();
        std::fs::write(t.path().join("index.json"), b"{nope").unwrap();
        assert!(s.list().unwrap_err().to_string().contains("corrupt"));
    }
}
