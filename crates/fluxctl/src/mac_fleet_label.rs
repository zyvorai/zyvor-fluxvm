// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Labels on the Macs of a fleet. A label is `key=value` on a host entry of the inventory (`labels: [zone=lab]`); the scheduler already
//! places jobs by them (`--host-label`). This adds the missing half:
//!
//! - `fluxctl mac-fleet label -f hosts.yaml` lists every Mac with its labels (`--filter label=zone=lab` narrows it);
//! - `fluxctl mac-fleet label mini-1 zone=lab --rm gpu -f hosts.yaml` sets and removes labels, written back to the inventory;
//! - `--filter` on `mac-fleet status` and `mac-fleet top` keeps the Macs that carry a label (`label_filter`).
//!
//! Editing the inventory sends nothing to any Mac. The file is rewritten as YAML (or JSON for a `.json` file): other keys are kept,
//! comments are not. The result is parsed again before it replaces the file.

use crate::label_filter::{self, Filter};
use crate::mac_errors::coded;
use crate::mac_fleet_inv::Inventory;
use crate::mac_fleet_remote::valid_label;
use crate::mac_fleet_ssh::Target;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What `--filter` takes on a list of Macs besides `label`.
pub const HOST_FILTER_FIELDS: &[&str] = &["name", "host"];

#[derive(clap::Args)]
pub struct LabelArgs {
    /// The Mac to change, by name or host as in the inventory. Leave it out to list every Mac with its labels.
    pub host: Option<String>,
    /// Labels to set, `KEY=VALUE` (a key that already has a value is changed).
    #[arg(value_name = "KEY=VALUE")]
    pub set: Vec<String>,
    /// Remove a label by key. Repeatable. Removing a label the Mac does not have is not an error.
    #[arg(long = "rm", value_name = "KEY")]
    pub rm: Vec<String>,
    /// Inventory file (YAML or JSON).
    #[arg(long, short = 'f', value_name = "FILE", default_value = "hosts")]
    pub inventory: PathBuf,
    /// When listing: keep Macs by `label=KEY`, `label=KEY=VALUE`, `name=PART` or `host=PART`. Repeatable; all must match.
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filter: Vec<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

/// `["zone=lab", "gpu"]` as `{"zone": "lab", "gpu": ""}`: the shape every list and filter reads.
pub fn labels_map(labels: &[String]) -> BTreeMap<String, String> {
    labels
        .iter()
        .map(|l| match l.split_once('=') {
            Some((k, v)) => (k.to_owned(), v.to_owned()),
            None => (l.clone(), String::new()),
        })
        .collect()
}

fn row(t: &Target) -> Value {
    json!({"name": t.name, "host": t.host, "labels": labels_map(&t.labels)})
}

/// Keep the Macs that pass every `--filter`. With no filter, all of them. An error says what was filtered and how to see the labels.
pub fn filter_targets(targets: Vec<Target>, filters: &[String]) -> Result<Vec<Target>> {
    if filters.is_empty() {
        return Ok(targets);
    }
    let fl: Vec<Filter> = label_filter::parse_all(filters, HOST_FILTER_FIELDS)?;
    let total = targets.len();
    let kept: Vec<Target> = targets
        .into_iter()
        .filter(|t| label_filter::matches(&row(t), label_filter::text, &fl))
        .collect();
    if kept.is_empty() {
        anyhow::bail!(
            "no Mac of the {total} matches --filter {}. See the labels each Mac has: fluxctl mac-fleet label -f HOSTS.yaml; set one: fluxctl mac-fleet label HOST KEY=VALUE -f HOSTS.yaml",
            filters.join(" --filter ")
        );
    }
    Ok(kept)
}

fn bad_label(l: &str) -> anyhow::Error {
    coded(
        format!(
            "label {l:?} must be KEY=VALUE, like zone=lab: letters, digits and . _ - / : only, no spaces. Next: fluxctl mac-fleet label HOST zone=lab -f hosts.yaml"
        ),
        "FLEET_LABEL_INVALID",
    )
}

fn key_of(l: &str) -> &str {
    l.split_once('=').map_or(l, |(k, _)| k)
}

/// The inventory text with `set` applied and `rm` removed on the host entry `host` (its name or host). Returns the new text, the
/// host's name and its labels afterwards. `json` writes JSON instead of YAML. Pure.
pub fn edit_labels(
    text: &str,
    host: &str,
    set: &[String],
    rm: &[String],
    json: bool,
) -> Result<(String, String, Vec<String>)> {
    for l in set {
        if !valid_label(l) {
            return Err(bad_label(l));
        }
    }
    let inv = Inventory::parse(text)?;
    let targets = inv.targets()?;
    let Some(idx) = targets
        .iter()
        .position(|t| t.name == host || t.host == host)
    else {
        let names: Vec<_> = targets.iter().map(|t| t.name.as_str()).collect();
        return Err(coded(
            format!(
                "no Mac named {host:?} in the inventory (it has: {}). Next: pick one of those, or add the Mac first: fluxctl mac-fleet add user@host",
                names.join(", ")
            ),
            "FLEET_HOST_UNKNOWN",
        ));
    };
    let name = targets[idx].name.clone();
    let mut touched: Vec<&str> = rm.iter().map(String::as_str).collect();
    touched.extend(set.iter().map(|l| key_of(l)));
    let mut labels: Vec<String> = targets[idx]
        .labels
        .iter()
        .filter(|l| !touched.contains(&key_of(l)))
        .cloned()
        .collect();
    // when a key is given twice, the last one wins
    let mut add: Vec<String> = vec![];
    for l in set.iter().rev() {
        if !add.iter().any(|a| key_of(a) == key_of(l)) {
            add.push(l.clone());
        }
    }
    add.reverse();
    labels.extend(add);

    let mut root: serde_yaml::Value =
        serde_yaml::from_str(text).context("the inventory is not valid YAML")?;
    let entry = root
        .get_mut("hosts")
        .and_then(|h| h.get_mut(idx))
        .and_then(serde_yaml::Value::as_mapping_mut)
        .context("the inventory host entry is not a mapping")?;
    let k = serde_yaml::Value::from("labels");
    if labels.is_empty() {
        entry.remove(&k);
    } else {
        entry.insert(
            k,
            serde_yaml::Value::Sequence(labels.iter().cloned().map(Into::into).collect()),
        );
    }
    let out = if json {
        serde_json::to_string_pretty(&root)? + "\n"
    } else {
        serde_yaml::to_string(&root)?
    };
    Inventory::parse(&out).context("the edited inventory is not valid; nothing was written")?;
    Ok((out, name, labels))
}

fn load(path: &PathBuf) -> Result<(PathBuf, String)> {
    let p = crate::mac_fleet_inv::resolve_path(path).map_err(|e| {
        coded(
            format!(
                "{e}. Next: pass your inventory with -f FILE, or make one: fluxctl mac-fleet inventory import --ssh-config"
            ),
            "FLEET_INVENTORY_MISSING",
        )
    })?;
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok((p, text))
}

fn write_atomic(p: &std::path::Path, text: &str) -> Result<()> {
    let tmp = p.with_extension("labels.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, p).with_context(|| format!("replacing {}", p.display()))?;
    Ok(())
}

fn render_list(targets: &[Target]) -> String {
    let rows: Vec<Vec<String>> = targets
        .iter()
        .map(|t| {
            vec![
                t.name.clone(),
                t.host.clone(),
                if t.labels.is_empty() {
                    "-".into()
                } else {
                    t.labels.join(" ")
                },
            ]
        })
        .collect();
    crate::mac_fleet::render_table(&["MAC", "HOST", "LABELS"], &rows)
}

pub fn label_cmd(a: LabelArgs) -> Result<()> {
    let (path, text) = load(&a.inventory)?;
    let Some(host) = &a.host else {
        if !a.set.is_empty() || !a.rm.is_empty() {
            anyhow::bail!(
                "name the Mac to change. Next: fluxctl mac-fleet label HOST KEY=VALUE -f {}",
                path.display()
            );
        }
        let targets = filter_targets(Inventory::parse(&text)?.targets()?, &a.filter)?;
        if a.json {
            let rows: Vec<Value> = targets.iter().map(row).collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else {
            print!("{}", render_list(&targets));
        }
        return Ok(());
    };
    if a.set.is_empty() && a.rm.is_empty() {
        // `label HOST` shows that one Mac; an unknown one gets the same explained error as a change would.
        let all = Inventory::parse(&text)?.targets()?;
        return match all.iter().find(|t| &t.name == host || &t.host == host) {
            Some(t) if a.json => {
                println!("{}", serde_json::to_string_pretty(&row(t))?);
                Ok(())
            }
            Some(t) => {
                print!("{}", render_list(std::slice::from_ref(t)));
                Ok(())
            }
            None => edit_labels(&text, host, &[], &[], false).map(|_| ()),
        };
    }
    let is_json = path.extension().is_some_and(|e| e == "json");
    let (out, name, labels) = edit_labels(&text, host, &a.set, &a.rm, is_json)?;
    write_atomic(&path, &out)?;
    if a.json {
        println!(
            "{}",
            json!({"name": name, "labels": labels_map(&labels), "inventory": path})
        );
    } else {
        let shown = if labels.is_empty() {
            "none".to_owned()
        } else {
            labels.join(" ")
        };
        println!(
            "{name}: labels are now {shown} (saved in {})",
            path.display()
        );
        println!("Place a job by them: fluxctl mac-fleet run --host-label KEY=VALUE ...");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INV: &str = "user: admin\nhosts:\n  - host: mini-1.local\n    labels: [zone=lab, gpu=no]\n  - host: mini-2.local\n    name: build-2\n";

    fn names(t: &[Target]) -> Vec<&str> {
        t.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn sets_replaces_and_removes_labels_and_keeps_the_rest() {
        let (out, name, labels) = edit_labels(
            INV,
            "mini-1.local",
            &["gpu=yes".into(), "team=ios".into()],
            &["zone".into()],
            false,
        )
        .unwrap();
        assert_eq!(name, "mini-1");
        assert_eq!(labels, ["gpu=yes", "team=ios"]);
        let inv = Inventory::parse(&out).unwrap();
        assert_eq!(inv.user.as_deref(), Some("admin"));
        let t = inv.targets().unwrap();
        assert_eq!(t[0].labels, ["gpu=yes", "team=ios"]);
        assert!(t[1].labels.is_empty() && t[1].name == "build-2");
    }

    #[test]
    fn a_host_is_found_by_name_and_removing_the_last_label_drops_the_key() {
        let (out, _, labels) = edit_labels(
            INV,
            "mini-1",
            &[],
            &["zone".into(), "gpu".into(), "never-had".into()],
            false,
        )
        .unwrap();
        assert!(labels.is_empty() && !out.contains("labels"), "{out}");
        let (out, name, _) = edit_labels(INV, "build-2", &["zone=lab".into()], &[], false).unwrap();
        assert_eq!(name, "build-2");
        assert_eq!(
            Inventory::parse(&out).unwrap().targets().unwrap()[1].labels,
            ["zone=lab"]
        );
    }

    #[test]
    fn the_last_value_wins_and_json_stays_json() {
        let (out, _, l) =
            edit_labels(INV, "build-2", &["a=1".into(), "a=2".into()], &[], true).unwrap();
        assert_eq!(l, ["a=2"]);
        assert!(serde_json::from_str::<Value>(&out).is_ok(), "{out}");
    }

    #[test]
    fn mistakes_say_what_to_do_and_how_to_explain() {
        for bad in ["zone", "a b=c", "=x", "k=$(x)"] {
            let e = edit_labels(INV, "build-2", &[bad.into()], &[], false)
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("FLEET_LABEL_INVALID") && e.contains("Next:"),
                "{bad}: {e}"
            );
        }
        let e = edit_labels(INV, "nope", &["a=b".into()], &[], false)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("FLEET_HOST_UNKNOWN") && e.contains("build-2") && e.contains("Next:"),
            "{e}"
        );
    }

    #[test]
    fn filter_keeps_macs_by_label_and_says_what_to_do_when_none_match() {
        let t = Inventory::parse(INV).unwrap().targets().unwrap();
        let f = |s: &[&str]| {
            filter_targets(
                t.clone(),
                &s.iter().map(|x| x.to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(names(&f(&[]).unwrap()), ["mini-1", "build-2"]);
        assert_eq!(names(&f(&["label=zone"]).unwrap()), ["mini-1"]);
        assert_eq!(
            names(&f(&["label=zone=lab", "label=gpu=no"]).unwrap()),
            ["mini-1"]
        );
        assert_eq!(names(&f(&["name=build"]).unwrap()), ["build-2"]);
        let e = f(&["label=zone=prod"]).unwrap_err().to_string();
        assert!(
            e.contains("no Mac of the 2") && e.contains("fluxctl mac-fleet label"),
            "{e}"
        );
        assert!(
            f(&["colour=red"])
                .unwrap_err()
                .to_string()
                .contains("MAC_FILTER_INVALID")
        );
    }

    #[test]
    fn labels_map_reads_key_value_and_bare_words() {
        let m = labels_map(&["zone=lab".into(), "gpu".into()]);
        assert_eq!(m["zone"], "lab");
        assert_eq!(m["gpu"], "");
    }

    #[test]
    fn the_file_is_edited_in_place_and_a_missing_one_is_explained() {
        let d = std::env::temp_dir().join(format!("fx-label-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("hosts.yaml");
        std::fs::write(&p, INV).unwrap();
        let args = |host: Option<&str>, set: &[&str], rm: &[&str]| LabelArgs {
            host: host.map(Into::into),
            set: set.iter().map(|s| s.to_string()).collect(),
            rm: rm.iter().map(|s| s.to_string()).collect(),
            inventory: p.clone(),
            filter: vec![],
            json: true,
        };
        label_cmd(args(Some("build-2"), &["zone=ci"], &[])).unwrap();
        label_cmd(args(Some("mini-1"), &[], &["gpu"])).unwrap();
        label_cmd(args(None, &[], &[])).unwrap();
        let t = Inventory::load(&p).unwrap().targets().unwrap();
        assert_eq!(t[0].labels, ["zone=lab"]);
        assert_eq!(t[1].labels, ["zone=ci"]);
        assert!(label_cmd(args(None, &["a=b"], &[])).is_err());
        let mut missing = args(None, &[], &[]);
        missing.inventory = d.join("absent");
        let e = label_cmd(missing).unwrap_err().to_string();
        assert!(
            e.contains("FLEET_INVENTORY_MISSING") && e.contains("-f FILE"),
            "{e}"
        );
        std::fs::remove_dir_all(&d).ok();
    }
}
