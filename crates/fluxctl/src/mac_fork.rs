// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! `fluxctl mac fork NAME -n N`: N new instances from a stopped instance's disk.
//!
//! A fork is a cold boot of a copy-on-write clone of the source's disk. Nothing of a running VM is carried over (a saved running
//! state cannot be restored on macOS; see docs/macos-containers.md). It is built from the two things `mac commit` and `mac run` already
//! do: the stopped instance's files are cloned into one hidden store image (an APFS clone, instant on the same volume), and each fork
//! is created from that image through the same daemon call `mac run` uses. Forks are created stopped unless `--start`.
//!
//! Not run against a real daemon or macOS VM: the tests use a loopback fake daemon.

use crate::mac_cmd::{INSTANCE_SELECTOR, build_create_body, delete_instance, find_instance};
use crate::mac_inspect::{self as view, IMAGE_LABEL};
use crate::mac_instance::explain;
use crate::mac_store::{self, Store};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::PathBuf;

/// The label on every fork: the name of the instance it was cloned from.
pub const FORKED_LABEL: &str = "fluxvm.mac.forked-from";
/// Apple's licence limit (the daemon enforces it too).
const MAX_MACOS: usize = 2;
const MAX_FORKS: usize = 64;

#[derive(clap::Args, Debug)]
#[command(
    after_help = "Examples:\n  fluxctl mac stop base && fluxctl mac fork base -n 3\n  fluxctl mac fork base -n 2 --name-prefix ci --start\n\nA fork is a COLD BOOT of a copy-on-write clone of the stopped instance's disk (instant, and it takes almost no space until the copies diverge). A running VM's memory is never carried over: macOS cannot restore a saved running state, so nothing is resumed. Each fork gets its own name, its own VM record from the daemon, and the label fluxvm.mac.forked-from=NAME. Settings inside the guest (its hostname, ssh host keys, accounts) are the source's: change them in the guest if the forks must differ.\n\nForks are created stopped. A Mac runs at most 2 macOS VMs, so --start needs free slots for all of them; otherwise stop one (`fluxctl mac stop NAME`) or spread the work over a fleet (`fluxctl mac-fleet run`). Needs the daemon on this Mac. If it fails: fluxctl explain MAC_FORK_SOURCE_RUNNING"
)]
pub struct ForkArgs {
    /// The stopped instance to clone (`fluxctl mac ps -a`).
    pub name: String,
    /// How many forks to create.
    #[arg(short = 'n', long, default_value_t = 1)]
    pub count: usize,
    /// Forks are named PREFIX-1, PREFIX-2, ... (default: NAME-fork).
    #[arg(long, value_name = "PREFIX")]
    pub name_prefix: Option<String>,
    /// Boot the forks after creating them (needs a free macOS slot for each).
    #[arg(long)]
    pub start: bool,
    #[arg(long)]
    pub json: bool,
}

/// `PREFIX-1 ... PREFIX-N`.
pub fn plan_names(source: &str, prefix: Option<&str>, n: usize) -> Result<Vec<String>> {
    ensure!(
        (1..=MAX_FORKS).contains(&n),
        "-n must be between 1 and {MAX_FORKS} (a Mac runs 2 macOS VMs at once; forks stay stopped until started)"
    );
    let prefix = prefix.map(str::trim).unwrap_or("");
    let prefix = if prefix.is_empty() {
        format!("{source}-fork")
    } else {
        prefix.to_string()
    };
    ensure!(
        !prefix.contains(char::is_whitespace) && !prefix.contains('/'),
        "{prefix:?} is not a usable name prefix (no spaces or slashes)"
    );
    Ok((1..=n).map(|i| format!("{prefix}-{i}")).collect())
}

/// Instances that hold a macOS slot.
pub fn slots_held(items: &[Value]) -> usize {
    items
        .iter()
        .filter(|v| matches!(view::status(v), "running" | "starting" | "paused"))
        .count()
}

/// `--start` needs a slot per fork.
pub fn check_slots(held: usize, n: usize) -> Result<()> {
    ensure!(
        held + n <= MAX_MACOS,
        "MAC_FORK_SLOTS: --start would run {n} more macOS VM(s) while {held} already run, and a Mac runs at most {MAX_MACOS}. Nothing was created. Stop one (`fluxctl mac stop NAME`), fork without --start and start them one at a time, or spread the work over a fleet (`fluxctl mac-fleet run`). More: fluxctl explain MAC_FORK_SLOTS"
    );
    Ok(())
}

/// The labels PATCH of one fork.
pub fn fork_labels(image: &str, source: &str) -> Value {
    let mut p = crate::mac_cmd::instance_labels(image);
    p["labels"][FORKED_LABEL] = json!(source);
    p
}

async fn list_instances(r: &crate::remote::Remote) -> Result<Vec<Value>> {
    let v = r
        .call(
            reqwest::Method::GET,
            &format!(
                "/v1/vms?label={}",
                crate::remote::encode_query(INSTANCE_SELECTOR)
            ),
            None,
        )
        .await?;
    Ok(crate::mac_cmd::filter_instances(&v["items"])
        .into_iter()
        .cloned()
        .collect())
}

pub async fn run(r: &crate::remote::Remote, a: ForkArgs) -> Result<()> {
    let store = Store::open(mac_store::default_root()?)?;
    let out = fork(r, &store, &a).await?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        for f in out["forks"].as_array().into_iter().flatten() {
            println!("{}", f["name"].as_str().unwrap_or("?"));
        }
        eprintln!(
            "{} created {} from {} (a cold boot of its disk clone). Start with `fluxctl mac start NAME`.",
            out["forks"].as_array().map_or(0, Vec::len),
            if a.start { "and started" } else { "stopped" },
            a.name
        );
    }
    Ok(())
}

/// Create the forks; returns `{source, image, forks: [{name, id, started}]}`.
pub async fn fork(r: &crate::remote::Remote, store: &Store, a: &ForkArgs) -> Result<Value> {
    let names = plan_names(&a.name, a.name_prefix.as_deref(), a.count)?;
    let src = find_instance(r, &a.name).await?;
    ensure!(
        view::status(&src) == "stopped",
        "MAC_FORK_SOURCE_RUNNING: {} is {}, and a running disk is not consistent to clone. Stop it first with `fluxctl mac stop {}`, then run the fork again (a fork is a cold boot of the stopped disk). More: fluxctl explain MAC_FORK_SOURCE_RUNNING",
        a.name,
        view::status(&src),
        a.name
    );
    let existing = list_instances(r).await?;
    for n in &names {
        ensure!(
            !existing.iter().any(|v| v["name"].as_str() == Some(n)),
            "an instance named {n:?} already exists: pick another --name-prefix, or remove it with `fluxctl mac rm {n}`"
        );
    }
    if a.start {
        check_slots(slots_held(&existing), names.len())?;
    }
    let disk = PathBuf::from(src["disk"].as_str().context("the VM record has no disk")?);
    let ws = PathBuf::from(
        src["workspace"]
            .as_str()
            .context("the VM record has no workspace")?,
    );
    ensure!(
        disk.is_file(),
        "{} is not readable here: `mac fork` must run on the Mac whose daemon owns {}",
        disk.display(),
        a.name
    );
    // One hidden image, cloned from the source (what `mac commit` does), shared by every fork.
    let key = format!(
        "forked-{}-{}:latest",
        a.name,
        &crate::mac_cmd::short_id()[..6]
    );
    mac_store::normalize(&key)?;
    crate::mac_cmd::store_instance_files(store, &key, &disk, &ws)?;
    let image_disk = store.resolve(&key)?.join("disk.raw");
    let mut forks = Vec::new();
    let mut failure = None;
    for name in &names {
        match create_one(r, &image_disk, &key, &src, name, a.start).await {
            Ok(f) => forks.push(f),
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if let Some(e) = failure {
        if forks.is_empty() {
            store.remove(&key).ok();
            return Err(e);
        }
        let made: Vec<&str> = forks.iter().filter_map(|f| f["name"].as_str()).collect();
        bail!(
            "{e:#}\nalready created and left in place: {}. Remove them with `fluxctl mac rm NAME` if you do not want them",
            made.join(", ")
        );
    }
    Ok(json!({"source": a.name, "image": key, "started": a.start, "forks": forks}))
}

async fn create_one(
    r: &crate::remote::Remote,
    image_disk: &std::path::Path,
    key: &str,
    src: &Value,
    name: &str,
    start: bool,
) -> Result<Value> {
    use reqwest::Method;
    let mut body = build_create_body(name, image_disk, &[])?;
    // Same size as the source, where the daemon recorded it.
    for k in ["vcpus", "memory_mib"] {
        if let Some(v) = src["request"].get(k).filter(|v| v.is_number()) {
            body[k] = v.clone();
        }
    }
    let vm = r
        .call(Method::POST, "/v1/vms", Some(body))
        .await
        .map_err(|e| explain(e, &format!("create {name}")))?;
    let id = vm["id"]
        .as_str()
        .context("the daemon returned no id")?
        .to_string();
    let rest = async {
        r.call(
            Method::PATCH,
            &format!("/v1/vms/{id}"),
            Some(fork_labels(key, src["name"].as_str().unwrap_or(""))),
        )
        .await?;
        if start {
            r.call(Method::POST, &format!("/v1/vms/{id}/start"), None)
                .await
                .map_err(|e| explain(e, &format!("start {name}")))?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = rest {
        delete_instance(r, &id).await.ok();
        return Err(e);
    }
    Ok(json!({"name": name, "id": id, "started": start, IMAGE_LABEL: key}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn names_prefix_and_limits() {
        assert_eq!(
            plan_names("base", None, 2).unwrap(),
            ["base-fork-1", "base-fork-2"]
        );
        assert_eq!(plan_names("base", Some("ci"), 1).unwrap(), ["ci-1"]);
        assert_eq!(plan_names("b", Some("  "), 1).unwrap(), ["b-fork-1"]);
        assert!(plan_names("b", None, 0).is_err() && plan_names("b", None, 65).is_err());
        assert!(
            plan_names("b", Some("a b"), 1).is_err() && plan_names("b", Some("a/b"), 1).is_err()
        );
    }

    #[test]
    fn slot_check_counts_running_paused_and_starting() {
        let items: Vec<Value> = ["running", "stopped", "paused", "starting", "failed"]
            .iter()
            .map(|s| json!({"status": s}))
            .collect();
        assert_eq!(slots_held(&items), 3);
        assert!(check_slots(0, 2).is_ok() && check_slots(1, 1).is_ok());
        let e = check_slots(1, 2).unwrap_err().to_string();
        assert!(
            e.contains("MAC_FORK_SLOTS") && e.contains("fleet") && e.contains("stop"),
            "{e}"
        );
    }

    #[test]
    fn labels_carry_the_source() {
        let l = fork_labels("forked-a-1:latest", "a");
        assert_eq!(l["labels"][FORKED_LABEL], "a");
        assert_eq!(l["labels"]["fluxvm.mac"], "instance");
        assert_eq!(l["labels"][IMAGE_LABEL], "forked-a-1:latest");
    }

    /// A loopback daemon: lists instances from `vms`, records each mutating request, and makes new VMs with sequential ids.
    struct Fake {
        addr: String,
        log: Arc<Mutex<Vec<String>>>,
        vms: Arc<Mutex<Vec<Value>>>,
    }

    async fn fake_daemon(vms: Vec<Value>, start_status: u16) -> Fake {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let log = Arc::new(Mutex::new(Vec::new()));
        let vms = Arc::new(Mutex::new(vms));
        let (log2, vms2) = (log.clone(), vms.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    break;
                };
                let (log, vms) = (log2.clone(), vms2.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head, body) = loop {
                        let n = c.read(&mut chunk).await.unwrap_or(0);
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some((h, b)) = text.split_once("\r\n\r\n") {
                            let want = h
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            if b.len() >= want || n == 0 {
                                break (h.to_string(), b.to_string());
                            }
                        } else if n == 0 {
                            return;
                        }
                    };
                    let mut it = head.lines().next().unwrap_or("").split_whitespace();
                    let (method, path) = (
                        it.next().unwrap_or("").to_string(),
                        it.next().unwrap_or("").to_string(),
                    );
                    let (status, out) = if method == "GET" && path.starts_with("/v1/vms") {
                        let all = vms.lock().unwrap().clone();
                        let items: Vec<Value> = match path.split("name=").nth(1) {
                            Some(n) => {
                                let n = n.split('&').next().unwrap_or("");
                                all.into_iter().filter(|v| v["name"] == n).collect()
                            }
                            None => all,
                        };
                        (200, json!({"items": items}))
                    } else {
                        log.lock().unwrap().push(format!("{method} {path} {body}"));
                        if method == "POST" && path == "/v1/vms" {
                            let b: Value = serde_json::from_str(&body).unwrap_or_default();
                            let id = format!("id-{}", vms.lock().unwrap().len() + 1);
                            vms.lock().unwrap().push(json!({"id": id, "name": b["name"], "status": "stopped", "labels": {"fluxvm.mac": "instance"}}));
                            (200, json!({"id": id}))
                        } else if path.ends_with("/start") && start_status != 200 {
                            (
                                start_status,
                                json!({"error": "2 macOS guests are already running"}),
                            )
                        } else {
                            (200, json!({}))
                        }
                    };
                    let text = match status {
                        200 => "OK",
                        _ => "Conflict",
                    };
                    let body = out.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status} {text}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    c.write_all(resp.as_bytes()).await.ok();
                });
            }
        });
        Fake { addr, log, vms }
    }

    fn source(dir: &std::path::Path, status: &str) -> Value {
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        for f in ["disk.raw", "hardware.bin", "auxiliary.bin"] {
            std::fs::write(ws.join(f), f).unwrap();
        }
        json!({"id": "id-0", "name": "base", "status": status, "disk": ws.join("disk.raw"), "workspace": ws,
               "labels": {"fluxvm.mac": "instance", "fluxvm.mac.image": "x:1"},
               "request": {"vcpus": 4, "memory_mib": 8192}})
    }

    fn args(n: usize, start: bool) -> ForkArgs {
        ForkArgs {
            name: "base".into(),
            count: n,
            name_prefix: None,
            start,
            json: true,
        }
    }

    #[tokio::test]
    async fn forks_are_created_stopped_with_labels_and_the_sources_size() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake_daemon(vec![source(dir.path(), "stopped")], 200).await;
        let store = Store::open(dir.path().join("store")).unwrap();
        let r = crate::remote::Remote::new(&f.addr, None);
        let out = fork(&r, &store, &args(2, false)).await.unwrap();
        let names: Vec<_> = out["forks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["base-fork-1", "base-fork-2"]);
        let log = f.log.lock().unwrap().join("\n");
        assert_eq!(log.matches("POST /v1/vms {").count(), 2, "{log}");
        assert!(!log.contains("/start"), "{log}");
        assert!(
            log.contains("\"vcpus\":4") && log.contains("\"memory_mib\":8192"),
            "{log}"
        );
        assert!(log.contains("fluxvm.mac.forked-from\":\"base"), "{log}");
        // The hidden image exists and holds the three files (clones, not the source's own files).
        let key = out["image"].as_str().unwrap();
        let d = store.resolve(key).unwrap();
        for file in ["disk.raw", "hardware.bin", "auxiliary.bin"] {
            assert!(d.join(file).is_file());
        }
        assert_eq!(f.vms.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_running_source_is_refused_before_anything_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake_daemon(vec![source(dir.path(), "running")], 200).await;
        let store = Store::open(dir.path().join("store")).unwrap();
        let r = crate::remote::Remote::new(&f.addr, None);
        let e = fork(&r, &store, &args(1, false))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("MAC_FORK_SOURCE_RUNNING") && e.contains("fluxctl mac stop base"),
            "{e}"
        );
        assert!(f.log.lock().unwrap().is_empty());
        assert!(store.list().unwrap().is_empty());
    }

    #[tokio::test]
    async fn start_beyond_the_slot_limit_is_a_plain_error_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut running = json!({"id": "id-9", "name": "other", "status": "running", "labels": {"fluxvm.mac": "instance"}});
        running["request"] = json!({});
        let f = fake_daemon(vec![source(dir.path(), "stopped"), running], 200).await;
        let store = Store::open(dir.path().join("store")).unwrap();
        let r = crate::remote::Remote::new(&f.addr, None);
        let e = fork(&r, &store, &args(2, true))
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("MAC_FORK_SLOTS") && e.contains("fleet"), "{e}");
        assert!(f.log.lock().unwrap().is_empty());
        // One fork fits.
        assert!(fork(&r, &store, &args(1, true)).await.is_ok());
        assert!(f.log.lock().unwrap().iter().any(|l| l.contains("/start")));
    }

    #[tokio::test]
    async fn a_daemon_refusal_to_start_is_explained_and_the_fork_removed() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake_daemon(vec![source(dir.path(), "stopped")], 409).await;
        let store = Store::open(dir.path().join("store")).unwrap();
        let r = crate::remote::Remote::new(&f.addr, None);
        let e = fork(&r, &store, &args(1, true))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("cannot start base-fork-1") && e.contains("fluxctl mac stop"),
            "{e}"
        );
        assert!(
            f.log
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.starts_with("DELETE"))
        );
        assert!(
            store.list().unwrap().is_empty(),
            "the hidden image goes when no fork was made"
        );
    }

    #[tokio::test]
    async fn existing_name_missing_instance_and_json_shape() {
        let dir = tempfile::tempdir().unwrap();
        let taken = json!({"id": "id-5", "name": "base-fork-1", "status": "stopped", "labels": {"fluxvm.mac": "instance"}});
        let f = fake_daemon(vec![source(dir.path(), "stopped"), taken], 200).await;
        let store = Store::open(dir.path().join("store")).unwrap();
        let r = crate::remote::Remote::new(&f.addr, None);
        let e = fork(&r, &store, &args(1, false))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("already exists") && e.contains("--name-prefix"),
            "{e}"
        );
        let mut a = args(1, false);
        a.name = "nope".into();
        let e = fork(&r, &store, &a).await.unwrap_err().to_string();
        assert!(e.contains("no instance named"), "{e}");
    }
}
