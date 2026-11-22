// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0

//! Image usage for `fluxctl mac`: when each image was last used by `mac run` (`last-used.json` in the store root), the size and
//! last-used columns of `mac images --usage`, and `mac image prune --keep-last N`. Pure parts are unit tested on temporary stores.

use crate::mac_store::{Entry, Store};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;

const FILE: &str = "last-used.json";

/// `name:tag` to unix seconds of the last `mac run`. A missing or unreadable file is an empty map: usage is advisory.
pub fn load(store: &Store) -> BTreeMap<String, u64> {
    std::fs::read(store.root().join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Record that `reference` was just used. Failures are ignored (a read-only store must not stop a run).
pub fn touch(store: &Store, reference: &str, now: u64) {
    let Ok(key) = crate::mac_store::normalize(reference) else {
        return;
    };
    let mut m = load(store);
    m.insert(key, now);
    let tmp = store.root().join(".last-used.json.tmp");
    if let Ok(b) = serde_json::to_vec_pretty(&m)
        && std::fs::write(&tmp, b).is_ok()
    {
        let _ = std::fs::rename(&tmp, store.root().join(FILE));
    }
}

/// The repository part of `name:tag`: everything before a tag after the last `/`.
pub fn repo_of(name: &str) -> &str {
    let slash = name.rfind('/').map_or(0, |i| i + 1);
    match name[slash..].rfind(':') {
        Some(i) => &name[..slash + i],
        None => name,
    }
}

/// From `(name, last-used-or-added)` pairs, the names beyond the `keep` most recent of each repository.
pub fn select_beyond(items: &[(String, u64)], keep: usize) -> Vec<String> {
    let mut by_repo: BTreeMap<&str, Vec<&(String, u64)>> = BTreeMap::new();
    for it in items {
        by_repo.entry(repo_of(&it.0)).or_default().push(it);
    }
    let mut out = Vec::new();
    for (_, mut v) in by_repo {
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out.extend(v.into_iter().skip(keep).map(|x| x.0.clone()));
    }
    out.sort();
    out
}

/// What `mac image prune --keep-last N` untags.
pub fn beyond_keep_last(store: &Store, keep: usize) -> Result<Vec<String>> {
    ensure!(keep >= 1, "--keep-last must be at least 1");
    let used = load(store);
    let items: Vec<(String, u64)> = store
        .list()?
        .into_iter()
        .filter(|(n, _)| !crate::mac_cache::is_cache_ref(n))
        .map(|(n, e)| {
            let t = used.get(&n).copied().unwrap_or(e.added_unix);
            (n, t)
        })
        .collect();
    Ok(select_beyond(&items, keep))
}

fn ago(now: u64, t: u64) -> String {
    if t == 0 {
        return "never".into();
    }
    let s = now.saturating_sub(t);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// `mac images --usage`: size on disk (allocated, because `disk.raw` is sparse), apparent size and last use.
pub fn render_usage(store: &Store, rows: &[(String, Entry)], now: u64) -> String {
    if rows.is_empty() {
        return "no macOS images\n".into();
    }
    let used = load(store);
    let w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(9).max(9);
    let mut out = format!(
        "{:<w$}  {:>10}  {:>10}  {:<10}  ADDED\n",
        "REFERENCE", "ALLOCATED", "APPARENT", "LAST USED"
    );
    for (n, e) in rows {
        let (apparent, alloc) = crate::mac_image_ops::dir_usage(&store.root().join(&e.dir));
        let last = used.get(n).copied().unwrap_or(0);
        out.push_str(&format!(
            "{n:<w$}  {:>10}  {:>10}  {:<10}  {}\n",
            crate::mac_cmd::human(alloc),
            crate::mac_cmd::human(apparent),
            ago(now, last),
            ago(now, e.added_unix)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(n: &str, t: u64) -> (String, u64) {
        (n.to_string(), t)
    }

    #[test]
    fn repo_strips_only_a_tag() {
        assert_eq!(repo_of("macos-xcode:15"), "macos-xcode");
        assert_eq!(repo_of("localhost:5000/macos:15"), "localhost:5000/macos");
        assert_eq!(repo_of("localhost:5000/macos"), "localhost:5000/macos");
    }

    #[test]
    fn keep_last_keeps_the_newest_per_repository() {
        let items = vec![
            pair("a:1", 10),
            pair("a:2", 30),
            pair("a:3", 20),
            pair("b:1", 5),
        ];
        assert_eq!(select_beyond(&items, 1), ["a:1", "a:3"]);
        assert_eq!(select_beyond(&items, 2), ["a:1"]);
        assert!(select_beyond(&items, 3).is_empty());
    }

    #[test]
    fn ties_break_by_name_so_the_choice_is_stable() {
        let items = vec![pair("a:2", 1), pair("a:1", 1)];
        assert_eq!(select_beyond(&items, 1), ["a:2"]);
    }

    #[test]
    fn touch_round_trips_and_a_broken_file_is_empty() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path()).unwrap();
        assert!(load(&store).is_empty());
        touch(&store, "img", 42);
        assert_eq!(load(&store).get("img:latest"), Some(&42));
        std::fs::write(d.path().join(FILE), "{nope").unwrap();
        assert!(load(&store).is_empty());
    }

    #[test]
    fn last_use_beats_the_added_time_in_prune_selection() {
        let d = tempfile::tempdir().unwrap();
        let store = Store::open(d.path()).unwrap();
        for (n, added) in [("a:1", 100u64), ("a:2", 200)] {
            let src = d.path().join(format!("src-{}", &n[2..]));
            std::fs::create_dir_all(&src).unwrap();
            for f in crate::mac_store::REQUIRED_FILES {
                std::fs::write(src.join(f), n).unwrap();
            }
            store
                .add(
                    n,
                    Entry {
                        dir: format!("src-{}", &n[2..]),
                        digest: String::new(),
                        source: "local".into(),
                        added_unix: added,
                    },
                )
                .unwrap();
        }
        assert_eq!(beyond_keep_last(&store, 1).unwrap(), ["a:1"]);
        touch(&store, "a:1", 999);
        assert_eq!(beyond_keep_last(&store, 1).unwrap(), ["a:2"]);
        assert!(beyond_keep_last(&store, 0).is_err());
    }

    #[test]
    fn ago_reads_naturally() {
        assert_eq!(ago(100, 0), "never");
        assert_eq!(ago(1000, 1000), "just now");
        assert_eq!(ago(1000 + 7200, 1000), "2h ago");
        assert_eq!(ago(1000 + 3 * 86400, 1000), "3d ago");
    }
}
