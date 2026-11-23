// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Named volumes for `fluxctl mac`: a directory on this Mac that is shared into a guest over virtiofs, kept after the instance is
//! gone. `-v NAME:/path[:ro]` on `mac run` / `mac create` uses (and creates) the volume `NAME`; `mac volume create|ls|inspect|rm|prune`
//! manage them.
//!
//! Layout: `~/.fluxvm/mac-volumes/NAME/volume.json` (metadata) and `NAME/data/` (what the guest sees). `FLUXVM_MAC_VOLUMES` moves the
//! root. A volume is "in use" while any instance, running or not, has its `data/` directory among its shared folders, as Docker
//! does for containers. The guest side is the same virtiofs mount as `-v ./dir:/path`, so what is and is not verified on a real macOS
//! guest is the same: the share path under `/Volumes/My Shared Files` is unverified.

use crate::mac_errors::coded;
use crate::mac_inspect::{human_bytes, status};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const META: &str = "volume.json";
const DATA: &str = "data";

pub fn default_root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("FLUXVM_MAC_VOLUMES").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME").context("HOME is not set (or set FLUXVM_MAC_VOLUMES)")?;
    Ok(PathBuf::from(home).join(".fluxvm").join("mac-volumes"))
}

/// Letters, digits, `_`, `.` and `-`, starting with a letter or digit, at most 64 characters. A name like that in `-v NAME:/path` is a
/// volume; anything with a `/`, `.` or `~` at the front is a path.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.starts_with(|c: char| c.is_ascii_alphanumeric())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Meta {
    pub name: String,
    pub created: u64,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub meta: Meta,
    pub data: PathBuf,
}

pub struct VolumeStore {
    root: PathBuf,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Allocated bytes under `p` (symlinks are counted as themselves, never followed).
pub fn tree_alloc(p: &Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    let mut n = m.blocks() * 512;
    if m.is_dir()
        && let Ok(rd) = std::fs::read_dir(p)
    {
        for e in rd.flatten() {
            n += tree_alloc(&e.path());
        }
    }
    n
}

impl VolumeStore {
    /// Does not create the root: a Mac that never used a volume gets no directory from `ls`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn open_default() -> Result<Self> {
        Ok(Self::at(default_root()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn check_name(name: &str) -> Result<()> {
        if valid_name(name) {
            return Ok(());
        }
        Err(coded(
            format!(
                "{name:?} is not a volume name. Use letters, digits, `_`, `.` and `-` (up to 64, starting with a letter or digit)"
            ),
            "MAC_VOLUME_NAME",
        ))
    }

    pub fn data_path(&self, name: &str) -> PathBuf {
        self.root.join(name).join(DATA)
    }

    /// Create the volume, or return the existing one (Docker's `volume create` is idempotent too). The bool is true when it is new.
    pub fn create(&self, name: &str, labels: BTreeMap<String, String>) -> Result<(Volume, bool)> {
        Self::check_name(name)?;
        if let Some(v) = self.get(name)? {
            return Ok((v, false));
        }
        let data = self.data_path(name);
        std::fs::create_dir_all(&data).map_err(|e| {
            coded(
                format!(
                    "cannot create the volume directory {}: {e}. Check that the disk has room and the folder is writable, or point FLUXVM_MAC_VOLUMES at another folder",
                    data.display()
                ),
                "MAC_VOLUME_IO",
            )
        })?;
        let meta = Meta {
            name: name.into(),
            created: now(),
            labels,
        };
        std::fs::write(
            self.root.join(name).join(META),
            serde_json::to_vec_pretty(&meta)?,
        )
        .with_context(|| format!("writing the metadata of volume {name}"))?;
        Ok((Volume { meta, data }, true))
    }

    pub fn get(&self, name: &str) -> Result<Option<Volume>> {
        Self::check_name(name)?;
        let file = self.root.join(name).join(META);
        let Ok(bytes) = std::fs::read(&file) else {
            return Ok(None);
        };
        let meta: Meta = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "{} is not valid JSON; delete the folder {} to forget the volume",
                file.display(),
                self.root.join(name).display()
            )
        })?;
        Ok(Some(Volume {
            meta,
            data: self.data_path(name),
        }))
    }

    pub fn require(&self, name: &str) -> Result<Volume> {
        self.get(name)?.ok_or_else(|| {
            coded(
                format!("no volume named {name:?}. See `fluxctl mac volume ls`, or make it with `fluxctl mac volume create {name}`"),
                "MAC_VOLUME_NOT_FOUND",
            )
        })
    }

    pub fn list(&self) -> Result<Vec<Volume>> {
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let Some(n) = e.file_name().to_str().map(String::from) else {
                continue;
            };
            if valid_name(&n)
                && let Some(v) = self.get(&n)?
            {
                out.push(v);
            }
        }
        out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
        Ok(out)
    }

    /// Delete the volume and its data. Returns the bytes it held. The caller has checked that no instance uses it.
    pub fn remove(&self, name: &str) -> Result<u64> {
        let v = self.require(name)?;
        let dir = self.root.join(name);
        ensure!(
            dir.parent() == Some(self.root.as_path()) && v.data.starts_with(&dir),
            "refusing to delete {} (not a volume folder)",
            dir.display()
        );
        let bytes = tree_alloc(&dir);
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("deleting the volume folder {}", dir.display()))?;
        Ok(bytes)
    }
}

/// `volume name -> instances (in the order given) that share its data directory`. Stopped instances count: they would mount it again.
pub fn users(instances: &[&Value], volumes: &[Volume]) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for v in volumes {
        let mut names = Vec::new();
        for i in instances {
            let uses = i["request"]["shared_folders"].as_array().is_some_and(|a| {
                a.iter().any(|f| {
                    f["host_path"]
                        .as_str()
                        .is_some_and(|p| Path::new(p) == v.data)
                })
            });
            if uses {
                names.push(i["name"].as_str().unwrap_or("?").to_string());
            }
        }
        out.insert(v.meta.name.clone(), names);
    }
    out
}

/// Refuse to delete a volume an instance uses, saying which.
pub fn check_unused(name: &str, used_by: &[String]) -> Result<()> {
    if used_by.is_empty() {
        return Ok(());
    }
    Err(coded(
        format!(
            "volume {name} is still used by {}. Remove that instance first (`fluxctl mac rm {}`), then retry",
            used_by.join(", "),
            used_by[0]
        ),
        "MAC_VOLUME_IN_USE",
    ))
}

/// The status of the instances that hold a volume, e.g. `web (running)`.
pub fn used_by_text(instances: &[&Value], names: &[String]) -> String {
    names
        .iter()
        .map(|n| {
            let st = instances
                .iter()
                .find(|i| i["name"].as_str() == Some(n))
                .map_or("?", |i| status(i));
            format!("{n} ({st})")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn inspect_json(v: &Volume, used_by: &[String]) -> Value {
    json!({
        "name": v.meta.name,
        "driver": "local",
        "mountpoint": v.data,
        "created_at": v.meta.created,
        "labels": v.meta.labels,
        "scope": "local",
        "used_by": used_by,
        "size_bytes": tree_alloc(&v.data),
    })
}

pub fn render_ls(vols: &[Volume], used: &BTreeMap<String, Vec<String>>) -> String {
    let mut out = format!("{:<28}  {:>10}  IN USE BY\n", "NAME", "SIZE");
    for v in vols {
        let by = used.get(&v.meta.name).cloned().unwrap_or_default();
        out.push_str(&format!(
            "{:<28}  {:>10}  {}\n",
            v.meta.name,
            human_bytes(tree_alloc(&v.data)),
            if by.is_empty() {
                "-".into()
            } else {
                by.join(",")
            }
        ));
    }
    out
}

/// A `-v` host part that is a volume name: the guest path and mode follow. `parse_mount` does the rest, with a placeholder host.
/// The volume is created when it is new (Docker does the same). Anything that is not a bare name is a path and goes to
/// [`crate::mac_instance::parse_mount`] unchanged.
pub fn parse_mount_named(
    s: &str,
    cwd: &Path,
    home: Option<&Path>,
    store: &VolumeStore,
) -> Result<crate::mac_instance::Mount> {
    if let Some((name, rest)) = s.split_once(':')
        && valid_name(name)
    {
        let mut m = crate::mac_instance::parse_mount(&format!("/volume:{rest}"), cwd, home)?;
        let (v, _) = store.create(name, BTreeMap::new())?;
        m.host = v.data;
        return Ok(m);
    }
    crate::mac_instance::parse_mount(s, cwd, home)
}

/// The `-v` parser for `mac run` and `mac create`: names become volumes.
pub fn parse_mount_default(
    s: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<crate::mac_instance::Mount> {
    // A path form never touches the volume store, so a Mac without HOME-based state still parses `-v ./src:/src`.
    match s.split_once(':') {
        Some((name, _)) if valid_name(name) => {
            parse_mount_named(s, cwd, home, &VolumeStore::open_default()?)
        }
        _ => crate::mac_instance::parse_mount(s, cwd, home),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, VolumeStore) {
        let d = tempfile::tempdir().unwrap();
        let s = VolumeStore::at(d.path().join("vols"));
        (d, s)
    }

    #[test]
    fn names_are_checked_with_a_plain_message() {
        assert!(valid_name("data-1.x_y"));
        for bad in ["", ".hidden", "a/b", "~x", "-x", "a b", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
        let (_d, s) = store();
        let e = s.create("a/b", BTreeMap::new()).unwrap_err().to_string();
        assert!(
            e.contains("not a volume name") && e.contains("MAC_VOLUME_NAME"),
            "{e}"
        );
    }

    #[test]
    fn create_is_idempotent_and_ls_is_sorted() {
        let (_d, s) = store();
        assert!(s.list().unwrap().is_empty());
        assert!(!s.root().exists(), "ls must not create the root");
        let (v, new) = s.create("zeta", BTreeMap::new()).unwrap();
        assert!(new && v.data.is_dir());
        let (_, again) = s.create("zeta", BTreeMap::new()).unwrap();
        assert!(!again);
        s.create("alpha", BTreeMap::from([("k".into(), "v".into())]))
            .unwrap();
        let names: Vec<_> = s.list().unwrap().into_iter().map(|v| v.meta.name).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert_eq!(s.require("alpha").unwrap().meta.labels["k"], "v");
    }

    #[test]
    fn remove_deletes_data_and_reports_missing_volumes() {
        let (_d, s) = store();
        let (v, _) = s.create("db", BTreeMap::new()).unwrap();
        std::fs::write(v.data.join("f"), vec![1u8; 10_000]).unwrap();
        assert!(s.remove("db").unwrap() >= 10_000);
        assert!(s.get("db").unwrap().is_none());
        let e = s.remove("db").unwrap_err().to_string();
        assert!(
            e.contains("no volume named") && e.contains("MAC_VOLUME_NOT_FOUND"),
            "{e}"
        );
    }

    fn vm(name: &str, st: &str, host: &Path) -> Value {
        json!({"name": name, "status": st,
               "request": {"shared_folders": [{"host_path": host, "guest_path": "/d"}]}})
    }

    #[test]
    fn usage_counts_stopped_instances_and_blocks_removal() {
        let (_d, s) = store();
        let (a, _) = s.create("a", BTreeMap::new()).unwrap();
        s.create("b", BTreeMap::new()).unwrap();
        let web = vm("web", "stopped", &a.data);
        let other = json!({"name": "o", "status": "running", "request": {}});
        let inst = [&web, &other];
        let u = users(&inst, &s.list().unwrap());
        assert_eq!(u["a"], ["web"]);
        assert!(u["b"].is_empty());
        let e = check_unused("a", &u["a"]).unwrap_err().to_string();
        assert!(
            e.contains("used by web")
                && e.contains("mac rm web")
                && e.contains("MAC_VOLUME_IN_USE"),
            "{e}"
        );
        assert!(check_unused("b", &u["b"]).is_ok());
        assert_eq!(used_by_text(&inst, &u["a"]), "web (stopped)");
    }

    #[test]
    fn dash_v_with_a_name_makes_the_volume_and_a_path_is_untouched() {
        let (_d, s) = store();
        let cwd = Path::new("/work");
        let m = parse_mount_named("cache:/var/cache:ro", cwd, None, &s).unwrap();
        assert_eq!(m.host, s.data_path("cache"));
        assert_eq!(m.guest, "/var/cache");
        assert!(m.read_only && m.host.is_dir());
        let p = parse_mount_named("./src:/src", cwd, None, &s).unwrap();
        assert_eq!(p.host, PathBuf::from("/work/./src"));
        assert!(s.get("src").unwrap().is_none());
        assert!(parse_mount_named("cache:rel", cwd, None, &s).is_err());
    }

    #[test]
    fn ls_and_inspect_show_size_and_users() {
        let (_d, s) = store();
        let (v, _) = s.create("db", BTreeMap::new()).unwrap();
        std::fs::write(v.data.join("f"), vec![0u8; 50_000]).unwrap();
        let used = BTreeMap::from([("db".to_string(), vec!["web".to_string()])]);
        let t = render_ls(&s.list().unwrap(), &used);
        assert!(
            t.contains("db") && t.contains("web") && t.contains("KiB"),
            "{t}"
        );
        let j = inspect_json(&v, &["web".into()]);
        assert_eq!(j["driver"], "local");
        assert!(j["size_bytes"].as_u64().unwrap() >= 50_000);
    }
}
