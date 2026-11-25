// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac system df` and `mac system prune [--volumes] [--all] [--dry-run]`: what the Mac's disk holds for FluxVM, what a prune
//! would remove and how much room that gives back. The Mac's internal disk is often nearly full and a macOS image is tens of GiB,
//! so every plan says how much is free now and how much would be free after.
//!
//! Sizes are allocated bytes. Instance disks and cached images are APFS clones that share blocks with their parent, so the
//! "would free" figure is an upper bound: the disk may gain less. That is printed next to the number, not hidden.

use crate::mac_image_ops::{self as ops, Df, file_alloc};
use crate::mac_inspect::human_bytes;
use crate::mac_store::Store;
use crate::mac_volume::{Volume, tree_alloc, users};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Below this much free space the report says the Mac is nearly full.
pub const LOW_FREE_BYTES: u64 = 30 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disk {
    pub free: u64,
    pub total: u64,
}

/// `df -Pk PATH` output: total and available bytes.
pub fn parse_df_bytes(text: &str) -> Option<Disk> {
    let f: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    let total: u64 = f.get(1)?.parse().ok()?;
    let avail: u64 = f.get(3)?.parse().ok()?;
    Some(Disk {
        free: avail * 1024,
        total: total * 1024,
    })
}

/// Free and total bytes of the volume that holds `path` (or its nearest existing parent).
pub fn disk_of(path: &Path) -> Option<Disk> {
    let mut p = path;
    while !p.exists() {
        p = p.parent()?;
    }
    let out = std::process::Command::new("df")
        .args(["-Pk"])
        .arg(p)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_df_bytes(&String::from_utf8_lossy(&out.stdout)))
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub instances: Vec<Item>,
    pub images: Vec<Item>,
    pub store_dirs: Vec<(PathBuf, u64)>,
    pub volumes: Vec<Item>,
    /// Unused volumes left alone because `--volumes` was not given.
    pub kept_volumes: Vec<Item>,
}

impl Plan {
    pub fn total(&self) -> u64 {
        self.instances.iter().map(|i| i.bytes).sum::<u64>()
            + self.images.iter().map(|i| i.bytes).sum::<u64>()
            + self.store_dirs.iter().map(|d| d.1).sum::<u64>()
            + self.volumes.iter().map(|i| i.bytes).sum::<u64>()
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
            && self.images.is_empty()
            && self.store_dirs.is_empty()
            && self.volumes.is_empty()
    }
}

/// Decide what a prune removes. `instances` are the daemon's records. Stopped and failed instances always go; images that no
/// instance uses go with `all`; unreferenced store directories always go; volumes that no remaining instance uses go with `volumes`.
pub fn plan(
    store: &Store,
    vols: &[Volume],
    instances: &[&Value],
    all: bool,
    volumes: bool,
) -> Result<Plan> {
    let names: BTreeSet<String> = store.list()?.into_iter().map(|(k, _)| k).collect();
    let victims = ops::prunable_instances(instances, &names, true);
    let mut p = Plan::default();
    for v in &victims {
        p.instances.push(Item {
            name: v["name"].as_str().unwrap_or("?").into(),
            bytes: v["disk"].as_str().map_or(0, |d| file_alloc(Path::new(d))),
        });
    }
    if all {
        let used: Vec<String> = instances
            .iter()
            .filter_map(|v| {
                v["labels"][crate::mac_inspect::IMAGE_LABEL]
                    .as_str()
                    .map(String::from)
            })
            .collect();
        let unused = ops::unused_images(store, &used)?;
        let rows = store.list()?;
        // A directory is freed only when every name that points at it goes.
        let mut counted = BTreeSet::new();
        for u in &unused {
            let Some((_, e)) = rows.iter().find(|(k, _)| k == u) else {
                continue;
            };
            let all_gone = rows
                .iter()
                .filter(|(_, x)| x.dir == e.dir)
                .all(|(k, _)| unused.contains(k));
            let bytes = if all_gone && counted.insert(e.dir.clone()) {
                tree_alloc(&store.root().join(&e.dir))
            } else {
                0
            };
            p.images.push(Item {
                name: u.clone(),
                bytes,
            });
        }
    }
    for d in ops::orphans(store, 3600)? {
        let b = tree_alloc(&d);
        p.store_dirs.push((d, b));
    }
    let victim_names: BTreeSet<&str> = victims.iter().filter_map(|v| v["name"].as_str()).collect();
    let remaining: Vec<&Value> = instances
        .iter()
        .copied()
        .filter(|v| !v["name"].as_str().is_some_and(|n| victim_names.contains(n)))
        .collect();
    let held = users(&remaining, vols);
    for v in vols {
        if held.get(&v.meta.name).is_some_and(|u| !u.is_empty()) {
            continue;
        }
        let item = Item {
            name: v.meta.name.clone(),
            bytes: tree_alloc(&v.data),
        };
        if volumes {
            p.volumes.push(item);
        } else {
            p.kept_volumes.push(item);
        }
    }
    Ok(p)
}

fn names(items: &[Item]) -> String {
    let n: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    if n.len() > 5 {
        format!("{}, and {} more", n[..5].join(", "), n.len() - 5)
    } else {
        n.join(", ")
    }
}

fn disk_line(disk: Option<Disk>, after: Option<u64>) -> String {
    let Some(d) = disk else {
        return "Disk: free space could not be read on this machine.\n".into();
    };
    let used_pct = 100 - (d.free.saturating_mul(100) / d.total.max(1)).min(100);
    let mut s = format!(
        "Disk: {} free of {} ({used_pct}% full).",
        human_bytes(d.free),
        human_bytes(d.total)
    );
    if let Some(freed) = after {
        s.push_str(&format!(
            " After this: about {} free.",
            human_bytes(d.free + freed)
        ));
    }
    s.push('\n');
    if d.free < LOW_FREE_BYTES {
        s.push_str(&format!(
            "This Mac is nearly full ({} free). A macOS image is tens of GiB and a new instance clones one, so `mac build` and `mac run` may fail with no space left. Free room first: stop and remove instances you no longer need, then `fluxctl mac system prune --all`.\n",
            human_bytes(d.free)
        ));
    }
    s
}

pub fn render_plan(p: &Plan, disk: Option<Disk>, dry_run: bool) -> String {
    let mut out = String::new();
    out.push_str(if dry_run {
        "Dry run: nothing is removed.\n"
    } else {
        ""
    });
    let mut row = |what: &str, n: usize, bytes: u64, detail: String| {
        if n > 0 {
            out.push_str(&format!(
                "  {:<16} {:>3}  {:>10}  {detail}\n",
                what,
                n,
                human_bytes(bytes)
            ));
        }
    };
    row(
        "instances",
        p.instances.len(),
        p.instances.iter().map(|i| i.bytes).sum(),
        names(&p.instances),
    );
    row(
        "images",
        p.images.len(),
        p.images.iter().map(|i| i.bytes).sum(),
        names(&p.images),
    );
    row(
        "store folders",
        p.store_dirs.len(),
        p.store_dirs.iter().map(|d| d.1).sum(),
        "left by a crashed pull or build".into(),
    );
    row(
        "volumes",
        p.volumes.len(),
        p.volumes.iter().map(|i| i.bytes).sum(),
        format!("{} (their data is deleted)", names(&p.volumes)),
    );
    if p.is_empty() {
        out.push_str("Nothing to remove.\n");
    } else {
        out.push_str(&format!(
            "Would free up to {}. Instance disks and images are APFS clones that share blocks, so the disk may gain less.\n",
            human_bytes(p.total())
        ));
    }
    if !p.kept_volumes.is_empty() {
        out.push_str(&format!(
            "{} unused volume{} ({}) kept: add --volumes to remove {}.\n",
            p.kept_volumes.len(),
            if p.kept_volumes.len() == 1 { "" } else { "s" },
            human_bytes(p.kept_volumes.iter().map(|i| i.bytes).sum()),
            if p.kept_volumes.len() == 1 {
                "it"
            } else {
                "them"
            }
        ));
    }
    out.push_str(&disk_line(disk, (!p.is_empty()).then(|| p.total())));
    out
}

pub fn plan_json(p: &Plan, disk: Option<Disk>, dry_run: bool) -> Value {
    let items = |v: &[Item]| -> Vec<Value> {
        v.iter()
            .map(|i| json!({"name": i.name, "bytes": i.bytes}))
            .collect()
    };
    json!({
        "schema": 1,
        "dry_run": dry_run,
        "instances": items(&p.instances),
        "images": items(&p.images),
        "store_dirs": p.store_dirs.iter().map(|d| json!({"path": d.0, "bytes": d.1})).collect::<Vec<_>>(),
        "volumes": items(&p.volumes),
        "kept_volumes": items(&p.kept_volumes),
        "would_free_bytes": p.total(),
        "disk_free_bytes": disk.map(|d| d.free),
        "disk_total_bytes": disk.map(|d| d.total),
        "estimate": "upper bound: APFS clones share blocks",
    })
}

/// `system df` output with volumes and free space added to the image and instance rows.
pub fn render_df(
    df: &Df,
    vols: &[Volume],
    disk: Option<Disk>,
    unused_vols: &BTreeSet<String>,
) -> String {
    let mut out = ops::render_df(df);
    // `render_df` ends with its footnote; put the volumes row above it.
    let foot = out.rfind("Sizes are allocated bytes").unwrap_or(out.len());
    let total: u64 = vols.iter().map(|v| tree_alloc(&v.data)).sum();
    let reclaim: u64 = vols
        .iter()
        .filter(|v| unused_vols.contains(&v.meta.name))
        .map(|v| tree_alloc(&v.data))
        .sum();
    let row = format!(
        "{:<12}  {:>5}  {:>6}  {:>10}  {}\n",
        "Volumes",
        vols.len(),
        vols.len() - unused_vols.len().min(vols.len()),
        human_bytes(total),
        human_bytes(reclaim)
    );
    out.insert_str(foot, &row);
    out.push_str(&disk_line(disk, None));
    out
}

pub fn df_json(
    df: &Df,
    vols: &[Volume],
    disk: Option<Disk>,
    unused_vols: &BTreeSet<String>,
) -> Result<Value> {
    let mut v = serde_json::to_value(df)?;
    v["volumes"] = json!(vols.len());
    v["volumes_active"] = json!(vols.len() - unused_vols.len().min(vols.len()));
    v["volume_bytes"] = json!(vols.iter().map(|x| tree_alloc(&x.data)).sum::<u64>());
    v["volume_reclaimable"] = json!(
        vols.iter()
            .filter(|x| unused_vols.contains(&x.meta.name))
            .map(|x| tree_alloc(&x.data))
            .sum::<u64>()
    );
    v["disk_free_bytes"] = json!(disk.map(|d| d.free));
    v["disk_total_bytes"] = json!(disk.map(|d| d.total));
    Ok(v)
}

/// Names of volumes no instance (running or not) holds.
pub fn unused_volumes(vols: &[Volume], instances: &[&Value]) -> BTreeSet<String> {
    users(instances, vols)
        .into_iter()
        .filter(|(_, u)| u.is_empty())
        .map(|(n, _)| n)
        .collect()
}

pub async fn run_df(r: &crate::remote::Remote, json_out: bool) -> Result<()> {
    let items = crate::mac_more::instances(r).await?;
    let refs: Vec<&Value> = items.iter().collect();
    let store = Store::open(crate::mac_store::default_root()?)?;
    let df = ops::compute_df(&store, &refs)?;
    let vstore = crate::mac_volume::VolumeStore::open_default()?;
    let vols = vstore.list()?;
    let unused = unused_volumes(&vols, &refs);
    let disk = disk_of(store.root());
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&df_json(&df, &vols, disk, &unused)?)?
        );
    } else {
        print!("{}", render_df(&df, &vols, disk, &unused));
    }
    Ok(())
}

pub async fn run_prune(
    r: &crate::remote::Remote,
    all: bool,
    volumes: bool,
    dry_run: bool,
    force: bool,
    json_out: bool,
) -> Result<()> {
    let items = crate::mac_more::instances(r).await?;
    let refs: Vec<&Value> = items.iter().collect();
    let store = Store::open(crate::mac_store::default_root()?)?;
    let vstore = crate::mac_volume::VolumeStore::open_default()?;
    let vols = vstore.list()?;
    let p = plan(&store, &vols, &refs, all, volumes)?;
    let disk = disk_of(store.root());
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan_json(&p, disk, dry_run))?
        );
        if dry_run || p.is_empty() {
            return Ok(());
        }
    } else {
        print!("{}", render_plan(&p, disk, dry_run));
    }
    if dry_run || p.is_empty() {
        return Ok(());
    }
    let what = format!(
        "This deletes {} instance(s), {} image(s), {} store folder(s) and {} volume(s) (frees up to {})",
        p.instances.len(),
        p.images.len(),
        p.store_dirs.len(),
        p.volumes.len(),
        human_bytes(p.total())
    );
    if !crate::mac_more::confirm(&what, force)? {
        println!("cancelled");
        return Ok(());
    }
    for v in refs.iter().filter(|v| {
        p.instances
            .iter()
            .any(|i| Some(i.name.as_str()) == v["name"].as_str())
    }) {
        crate::mac_cmd::delete_instance(r, v["id"].as_str().unwrap_or_default()).await?;
        println!("removed instance {}", v["name"].as_str().unwrap_or("?"));
    }
    for i in &p.images {
        store.remove(&i.name)?;
    }
    let dirs: Vec<PathBuf> = p.store_dirs.iter().map(|d| d.0.clone()).collect();
    ops::remove_orphans(&store, &dirs)?;
    for v in &p.volumes {
        vstore.remove(&v.name)?;
        println!("removed volume {}", v.name);
    }
    let after = disk_of(store.root());
    match (disk, after) {
        (Some(b), Some(a)) => println!(
            "Done. Free space on this Mac: {} before, {} now ({} gained).",
            human_bytes(b.free),
            human_bytes(a.free),
            human_bytes(a.free.saturating_sub(b.free))
        ),
        _ => println!("Done."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mac_volume::VolumeStore;
    use std::collections::BTreeMap;

    const DF: &str = "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s5 488245288 480000000 8000000 99% /System/Volumes/Data";

    #[test]
    fn df_text_gives_bytes() {
        let d = parse_df_bytes(DF).unwrap();
        assert_eq!(d.free, 8_000_000 * 1024);
        assert_eq!(d.total, 488_245_288 * 1024);
        assert!(parse_df_bytes("").is_none());
    }

    fn fixture() -> (tempfile::TempDir, Store, VolumeStore) {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path().join("img")).unwrap();
        let vols = VolumeStore::at(d.path().join("vols"));
        (d, store, vols)
    }

    fn vm(name: &str, st: &str, disk: Option<&Path>, host: Option<&Path>) -> Value {
        let mut v = json!({"name": name, "id": format!("id-{name}"), "status": st, "labels": {"fluxvm.mac": "instance"}});
        if let Some(d) = disk {
            v["disk"] = json!(d);
        }
        if let Some(h) = host {
            v["request"] = json!({"shared_folders": [{"host_path": h, "guest_path": "/d"}]});
        }
        v
    }

    #[test]
    fn prune_plan_frees_stopped_instances_and_keeps_volumes_without_the_flag() {
        let (d, store, vs) = fixture();
        let disk = d.path().join("stopped.raw");
        std::fs::write(&disk, vec![7u8; 100_000]).unwrap();
        let (held, _) = vs.create("held", BTreeMap::new()).unwrap();
        let (_free, _) = vs.create("free", BTreeMap::new()).unwrap();
        std::fs::write(vs.data_path("free").join("f"), vec![1u8; 20_000]).unwrap();
        let stopped = vm("old", "stopped", Some(&disk), None);
        let running = vm("live", "running", None, Some(&held.data));
        let vols = vs.list().unwrap();
        let p = plan(&store, &vols, &[&stopped, &running], false, false).unwrap();
        assert_eq!(p.instances.len(), 1);
        assert!(p.instances[0].bytes >= 100_000);
        assert!(p.volumes.is_empty());
        assert_eq!(
            p.kept_volumes[0].name, "free",
            "a volume a running instance holds is never listed"
        );
        let t = render_plan(&p, parse_df_bytes(DF), true);
        assert!(
            t.contains("Dry run") && t.contains("old") && t.contains("add --volumes"),
            "{t}"
        );
        assert!(
            t.contains("nearly full") && t.contains("After this: about"),
            "{t}"
        );

        let p = plan(&store, &vols, &[&stopped, &running], false, true).unwrap();
        assert_eq!(p.volumes[0].name, "free");
        assert!(render_plan(&p, None, false).contains("their data is deleted"));
    }

    #[test]
    fn a_volume_held_only_by_a_pruned_instance_becomes_prunable() {
        let (_d, store, vs) = fixture();
        let (v, _) = vs.create("db", BTreeMap::new()).unwrap();
        let old = vm("old", "stopped", None, Some(&v.data));
        let p = plan(&store, &vs.list().unwrap(), &[&old], false, true).unwrap();
        assert_eq!(p.volumes.len(), 1);
        // Without the instance going away (running), it stays.
        let live = vm("old", "running", None, Some(&v.data));
        let p = plan(&store, &vs.list().unwrap(), &[&live], false, true).unwrap();
        assert!(p.volumes.is_empty());
    }

    #[test]
    fn nothing_to_remove_says_so_and_still_reports_the_disk() {
        let t = render_plan(&Plan::default(), parse_df_bytes(DF), true);
        assert!(
            t.contains("Nothing to remove") && t.contains("Disk:"),
            "{t}"
        );
        let j = plan_json(&Plan::default(), parse_df_bytes(DF), true);
        assert_eq!(j["would_free_bytes"], 0);
        assert_eq!(j["dry_run"], true);
    }

    #[test]
    fn df_gets_a_volumes_row_and_the_free_space() {
        let (_d, _s, vs) = fixture();
        vs.create("a", BTreeMap::new()).unwrap();
        let vols = vs.list().unwrap();
        let unused: BTreeSet<String> = ["a".to_string()].into();
        let t = render_df(&Df::default(), &vols, parse_df_bytes(DF), &unused);
        assert!(
            t.contains("Volumes") && t.contains("Disk: 7.6 GiB free"),
            "{t}"
        );
        assert!(t.find("Volumes").unwrap() < t.find("Sizes are allocated").unwrap());
        let j = df_json(&Df::default(), &vols, parse_df_bytes(DF), &unused).unwrap();
        assert_eq!(j["volumes"], 1);
        assert_eq!(j["image_names"], 0, "the old fields stay");
        assert!(j["disk_free_bytes"].as_u64().unwrap() > 0);
    }
}
