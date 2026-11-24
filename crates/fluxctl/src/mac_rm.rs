// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac rm [-f] [-v] NAME...`: Docker-style removal of several instances at once.

use crate::mac_errors::coded;
use anyhow::Result;
use std::collections::BTreeMap;

/// What `-v` does with the volumes the removed instances mounted: `(delete, keep)`. A volume is deleted only when every
/// instance that mounts it is being removed AND it was created by one of those very instances. `creators` maps a volume to
/// the unique token of the instance whose `-v` made it, and `tokens` maps an instance name to its own token, so a newer
/// instance that reuses an old name does not own the old instance's volume. Everything else is kept, because it may hold
/// data the user made on purpose.
pub fn split_volumes(
    users: &BTreeMap<String, Vec<String>>,
    creators: &BTreeMap<String, String>,
    tokens: &BTreeMap<String, String>,
    removed: &[String],
) -> (Vec<String>, Vec<String>) {
    let (mut del, mut keep) = (Vec::new(), Vec::new());
    for (v, u) in users {
        if u.is_empty() || !u.iter().any(|n| removed.contains(n)) {
            continue;
        }
        let mine = u.iter().all(|n| removed.contains(n))
            && creators
                .get(v)
                .is_some_and(|c| u.iter().any(|n| tokens.get(n) == Some(c)));
        if mine {
            del.push(v.clone());
        } else {
            keep.push(v.clone());
        }
    }
    (del, keep)
}

/// The error when some instances could not be removed: names them and says what to do next.
pub fn failure(failed: &[String]) -> anyhow::Error {
    coded(
        format!(
            "could not remove: {}. Look at the lines above for each reason; see what is left with `fluxctl mac ps -a`, then retry",
            failed.join(", ")
        ),
        "MAC_RM_FAILED",
    )
}

pub async fn run(
    r: &crate::remote::Remote,
    names: Vec<String>,
    force: bool,
    volumes: bool,
) -> Result<()> {
    let mut seen: Vec<String> = Vec::new();
    for n in names {
        if !seen.contains(&n) {
            seen.push(n);
        }
    }
    let mut failed = Vec::new();
    let mut removed = Vec::new();
    // Volumes are worked out before anything is deleted, while the instances still list their mounts.
    let mut tokens: BTreeMap<String, String> = BTreeMap::new();
    let before = if volumes {
        let items = crate::mac_more::instances(r).await?;
        for i in &items {
            if let (Some(n), Some(t)) = (
                i["name"].as_str(),
                i["labels"][crate::mac_volume::INSTANCE_ID_LABEL].as_str(),
            ) {
                tokens.insert(n.to_string(), t.to_string());
            }
        }
        let refs: Vec<&serde_json::Value> = items.iter().collect();
        let store = crate::mac_volume::VolumeStore::open_default()?;
        Some(crate::mac_volume::users(&refs, &store.list()?))
    } else {
        None
    };
    for n in &seen {
        let res = async {
            let vm = match crate::mac_cmd::find_instance(r, n).await {
                Ok(vm) => vm,
                // `-f` ignores a name that is already gone, like `docker rm -f`.
                Err(_) if force => return Ok(false),
                Err(e) => return Err(e),
            };
            crate::mac_cmd::delete_instance(r, vm["id"].as_str().unwrap_or_default()).await?;
            Ok::<bool, anyhow::Error>(true)
        }
        .await;
        match res {
            Ok(true) => {
                println!("removed {n}");
                removed.push(n.clone());
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("{n}: {e:#}");
                failed.push(n.clone());
            }
        }
    }
    if let Some(users) = before {
        // Only volumes whose every user was removed in this very command (never one another instance still holds).
        let store = crate::mac_volume::VolumeStore::open_default()?;
        let creators: BTreeMap<String, String> = store
            .list()?
            .into_iter()
            .filter_map(|v| {
                let c = v.meta.labels.get(crate::mac_volume::CREATED_BY)?.clone();
                Some((v.meta.name, c))
            })
            .collect();
        let (del, keep) = split_volumes(&users, &creators, &tokens, &removed);
        if !keep.is_empty() {
            println!(
                "kept volume{} {}: not created by the instance{} removed here, or still used by another. Delete with `fluxctl mac volume rm NAME`",
                if keep.len() == 1 { "" } else { "s" },
                keep.join(", "),
                if removed.len() == 1 { "" } else { "s" }
            );
        }
        for v in del {
            match store.remove(&v) {
                Ok(_) => println!("removed volume {v}"),
                Err(e) => {
                    eprintln!("volume {v}: {e:#}");
                    failed.push(format!("volume {v}"));
                }
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failure(&failed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users(rows: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        rows.iter()
            .map(|(v, u)| (v.to_string(), u.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    fn map(rows: &[(&str, &str)]) -> BTreeMap<String, String> {
        rows.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn only_volumes_the_removed_instance_created_go() {
        let u = users(&[
            ("mine", &["a"]),
            ("theirs", &["a"]),
            ("shared", &["a", "b"]),
            ("unused", &[]),
            ("other", &["b"]),
        ]);
        let cr = map(&[("mine", "ta"), ("shared", "ta"), ("other", "tb")]);
        let tok = map(&[("a", "ta"), ("b", "tb")]);
        let (del, keep) = split_volumes(&u, &cr, &tok, &["a".to_string()]);
        assert_eq!(del, ["mine"]);
        assert_eq!(keep, ["shared", "theirs"]);
        let (del, keep) = split_volumes(&u, &cr, &tok, &["a".to_string(), "b".to_string()]);
        assert_eq!(del, ["mine", "other", "shared"]);
        assert_eq!(keep, ["theirs"]);
    }

    #[test]
    fn a_new_instance_reusing_a_name_does_not_own_the_old_volume() {
        // Instance `a` (token t1) made `data` and was removed WITHOUT -v; a new `a` (token t2) mounts it.
        let u = users(&[("data", &["a"])]);
        let cr = map(&[("data", "t1")]);
        let tok = map(&[("a", "t2")]);
        let (del, keep) = split_volumes(&u, &cr, &tok, &["a".to_string()]);
        assert!(del.is_empty(), "{del:?}");
        assert_eq!(keep, ["data"]);
        // An instance with no token at all (made before this label existed) owns nothing.
        let (del, keep) = split_volumes(&u, &cr, &BTreeMap::new(), &["a".to_string()]);
        assert!(del.is_empty() && keep == ["data"]);
    }

    #[test]
    fn failure_names_the_instances_and_the_next_step() {
        let m = format!("{:#}", failure(&["a".into(), "b".into()]));
        assert!(m.contains("a, b") && m.contains("mac ps -a") && m.contains("MAC_RM_FAILED"));
        assert_eq!(
            crate::mac_errors::hint_for(&m).map(|c| c.code),
            Some("MAC_RM_FAILED")
        );
    }
}
