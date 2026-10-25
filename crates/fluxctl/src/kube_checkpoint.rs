// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl kube checkpoint | restore | checkpoints | checkpoint-rm`: save a running cluster as it is, and put it back.
//!
//! A checkpoint is a snapshot of the server VM that carries its memory (the vz backend writes the guest's machine state and
//! clones the disk), so restoring it brings back the guest exactly where it was: every object in the cluster, every running
//! pod, the page cache. It costs almost nothing until the disk changes. The snapshot is named `kube-<label>`, which keeps a
//! cluster's checkpoints apart from other snapshots of the same VM.
//!
//! One server only: a cluster with workers keeps its nodes on a private network and in several VMs, which a snapshot of one VM
//! cannot put back consistently, so those are refused with a reason.

use crate::{kube_host, run::VmApi};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::Instant;
use uuid::Uuid;

/// Every checkpoint's snapshot tag starts with this.
const PREFIX: &str = "kube-";

/// A label is what the user types: lowercase letters, digits and dashes, 1 to 40 characters, not starting or ending with a dash.
pub fn label_ok(label: &str) -> bool {
    let b = label.as_bytes();
    !b.is_empty()
        && b.len() <= 40
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

fn check_label(label: &str) -> Result<()> {
    if !label_ok(label) {
        bail!(
            "checkpoint label {label:?} must be 1 to 40 lowercase letters, digits or dashes, not starting or ending with a dash"
        );
    }
    Ok(())
}

pub fn tag_of(label: &str) -> String {
    format!("{PREFIX}{label}")
}

/// The label of a snapshot tag that is a checkpoint.
pub fn label_of(tag: &str) -> Option<&str> {
    tag.strip_prefix(PREFIX).filter(|l| label_ok(l))
}

const WORKERS_REFUSAL: &str = "this cluster has worker nodes. A checkpoint is a snapshot of one VM, and workers are separate VMs on a private network that it cannot put back consistently. Checkpoints work on a cluster made without --workers";

/// The server VM of a single-node cluster, running or not.
async fn server<A: VmApi>(api: &A, o: &kube_host::KubeOptions) -> Result<(Uuid, String)> {
    kube_host::validate(o)?;
    if !kube_host::worker_vms(api, o).await?.is_empty() {
        bail!(WORKERS_REFUSAL);
    }
    match api.find(&o.name).await? {
        Some(vm) => Ok(vm),
        None => bail!(
            "there is no cluster named {:?}. Create it with `fluxctl kube up --name {}`",
            o.name,
            o.name
        ),
    }
}

/// `fluxctl kube checkpoint LABEL`: snapshot the running server, memory included.
pub async fn checkpoint<A: VmApi>(
    api: &A,
    o: &kube_host::KubeOptions,
    label: &str,
) -> Result<String> {
    check_label(label)?;
    let (id, status) = server(api, o).await?;
    if status != "running" {
        bail!(
            "{} is {status}; a checkpoint saves a running cluster. Start it with `fluxctl kube up --name {}`",
            o.name,
            o.name
        );
    }
    let tag = tag_of(label);
    if api.has_snapshot(id, &tag).await? {
        bail!(
            "{} already has a checkpoint {label:?}. Remove it with `fluxctl kube checkpoint-rm {label}` or pick another label",
            o.name
        );
    }
    let started = Instant::now();
    api.snapshot(id, &tag).await?;
    Ok(format!(
        "checkpoint {label:?} of {} saved in {:.1}s",
        o.name,
        started.elapsed().as_secs_f32()
    ))
}

/// `fluxctl kube restore LABEL`: put the cluster back as the checkpoint saved it, then wait until the API answers again.
pub async fn restore<A: VmApi>(api: &A, o: &kube_host::KubeOptions, label: &str) -> Result<String> {
    check_label(label)?;
    let (id, _) = server(api, o).await?;
    let tag = tag_of(label);
    if !api.has_snapshot(id, &tag).await? {
        let have: Vec<String> = checkpoints(api, id)
            .await?
            .iter()
            .filter_map(|c| c["label"].as_str().map(str::to_owned))
            .collect();
        bail!(
            "{} has no checkpoint {label:?}{}",
            o.name,
            if have.is_empty() {
                ". Make one with `fluxctl kube checkpoint LABEL`".to_owned()
            } else {
                format!(" (it has: {})", have.join(", "))
            }
        );
    }
    let started = Instant::now();
    kube_host::stop_forward(o); // the VM restarts, so the old tunnel is dead
    api.restore(id, &tag).await?;
    kube_host::up(api, o).await?; // waits for ssh and a Ready node, and forwards the API port again
    Ok(format!(
        "{} restored to checkpoint {label:?} in {:.1}s",
        o.name,
        started.elapsed().as_secs_f32()
    ))
}

/// The checkpoints among a VM's snapshots, newest first: `label`, `created_at`, `size_bytes`, `memory`.
async fn checkpoints<A: VmApi>(api: &A, id: Uuid) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = api
        .list_snapshots(id)
        .await?
        .iter()
        .filter_map(|s| {
            let label = label_of(s["tag"].as_str()?)?;
            Some(json!({"label": label, "created_at": s["created_at"], "size_bytes": s["size_bytes"], "memory": s["has_memory"]}))
        })
        .collect();
    out.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    Ok(out)
}

/// `fluxctl kube checkpoints`
pub async fn list<A: VmApi>(api: &A, o: &kube_host::KubeOptions) -> Result<Value> {
    let (id, _) = server(api, o).await?;
    Ok(json!({"cluster": o.name, "checkpoints": checkpoints(api, id).await?}))
}

/// `fluxctl kube checkpoint-rm LABEL`
pub async fn remove<A: VmApi>(api: &A, o: &kube_host::KubeOptions, label: &str) -> Result<String> {
    check_label(label)?;
    let (id, _) = server(api, o).await?;
    if !api.has_snapshot(id, &tag_of(label)).await? {
        bail!("{} has no checkpoint {label:?}", o.name);
    }
    api.delete_snapshot(id, &tag_of(label)).await?;
    Ok(format!("checkpoint {label:?} of {} removed", o.name))
}

/// The checkpoint `sleep` makes and `wake` consumes.
const SLEEP_LABEL: &str = "sleep";

/// `fluxctl kube sleep`: save the running cluster, then stop its VM so it costs no CPU or memory.
pub async fn sleep<A: VmApi>(api: &A, o: &kube_host::KubeOptions) -> Result<String> {
    let (id, _) = server(api, o).await?;
    let started = Instant::now();
    checkpoint(api, o, SLEEP_LABEL).await?;
    kube_host::stop_forward(o);
    api.stop(id).await?;
    Ok(format!(
        "{} is asleep ({:.1}s). `fluxctl kube wake` or the first kubectl call under `kube supervise` brings it back",
        o.name,
        started.elapsed().as_secs_f32()
    ))
}

/// Whether the cluster is parked: its VM is not running and the sleep checkpoint exists.
pub async fn is_asleep<A: VmApi>(api: &A, o: &kube_host::KubeOptions) -> bool {
    match api.find(&o.name).await {
        Ok(Some((id, status))) if status != "running" => api
            .has_snapshot(id, &tag_of(SLEEP_LABEL))
            .await
            .unwrap_or(false),
        _ => false,
    }
}

/// `fluxctl kube wake`: restore the sleep checkpoint and delete it, so the next sleep can save a new one.
pub async fn wake<A: VmApi>(api: &A, o: &kube_host::KubeOptions) -> Result<String> {
    let (id, _) = server(api, o).await?;
    if !api.has_snapshot(id, &tag_of(SLEEP_LABEL)).await? {
        bail!("{} is not asleep", o.name);
    }
    let started = Instant::now();
    restore(api, o, SLEEP_LABEL).await?;
    api.delete_snapshot(id, &tag_of(SLEEP_LABEL)).await?;
    Ok(format!(
        "{} is awake ({:.1}s)",
        o.name,
        started.elapsed().as_secs_f32()
    ))
}

/// Checkpoints made by `kube try` are named `try-<UTC time>`, so they sort by age and `kube undo` knows which are its own.
const TRY_PREFIX: &str = "try-";

/// `try-20261025-103000123` (to the millisecond, so two runs in one second get two checkpoints)
pub(crate) fn try_label(now: chrono::DateTime<chrono::Utc>) -> String {
    format!("{TRY_PREFIX}{}", now.format("%Y%m%d-%H%M%S%3f"))
}

/// The `try-` labels among these, oldest first.
fn try_labels(labels: &[String]) -> Vec<String> {
    let mut v: Vec<String> = labels
        .iter()
        .filter(|l| l.starts_with(TRY_PREFIX))
        .cloned()
        .collect();
    v.sort();
    v
}

/// The labels starting with `prefix` beyond the newest `keep`, oldest first: the ones to delete.
fn stale_with(labels: &[String], prefix: &str, keep: usize) -> Vec<String> {
    let mut v: Vec<String> = labels
        .iter()
        .filter(|l| l.starts_with(prefix))
        .cloned()
        .collect();
    v.sort();
    v[..v.len().saturating_sub(keep)].to_vec()
}

/// The `try-` labels beyond the newest `keep`.
pub(crate) fn stale_tries(labels: &[String], keep: usize) -> Vec<String> {
    stale_with(labels, TRY_PREFIX, keep)
}

/// Checkpoints that `kube supervise --auto-checkpoint` makes are named `auto-<UTC time>`.
const AUTO_PREFIX: &str = "auto-";

pub(crate) fn auto_label(now: chrono::DateTime<chrono::Utc>) -> String {
    format!("{AUTO_PREFIX}{}", now.format("%Y%m%d-%H%M%S"))
}

pub(crate) fn stale_autos(labels: &[String], keep: usize) -> Vec<String> {
    stale_with(labels, AUTO_PREFIX, keep)
}

/// One round of the time machine: saves an `auto-` checkpoint when the cluster is running, then deletes the oldest ones
/// beyond `keep`. A stopped, sleeping or missing cluster is skipped, not an error.
pub async fn auto_checkpoint<A: VmApi>(
    api: &A,
    o: &kube_host::KubeOptions,
    keep: usize,
) -> Result<Option<String>> {
    let (id, status) = server(api, o).await?;
    if status != "running" {
        return Ok(None);
    }
    let label = auto_label(chrono::Utc::now());
    let done = checkpoint(api, o, &label).await?;
    for old in stale_autos(&labels_of(api, id).await?, keep) {
        api.delete_snapshot(id, &tag_of(&old)).await?;
    }
    Ok(Some(done))
}

async fn labels_of<A: VmApi>(api: &A, id: Uuid) -> Result<Vec<String>> {
    Ok(checkpoints(api, id)
        .await?
        .iter()
        .filter_map(|c| c["label"].as_str().map(str::to_owned))
        .collect())
}

/// `fluxctl kube try -- COMMAND`: saves a checkpoint, runs the command against this cluster (`KUBECONFIG` set), and keeps
/// the checkpoint so `kube undo` can put the cluster back. Returns the command's exit code.
pub async fn try_run<A: VmApi>(
    api: &A,
    o: &kube_host::KubeOptions,
    command: &[String],
    keep: usize,
) -> Result<i32> {
    let Some((program, args)) = command.split_first() else {
        bail!(
            "give a command after --, for example: fluxctl kube try -- kubectl apply -f app.yaml"
        );
    };
    if keep == 0 {
        bail!("--keep must be at least 1");
    }
    let label = try_label(chrono::Utc::now());
    eprintln!("{}", checkpoint(api, o, &label).await?);
    let kubeconfig = kube_host::kubeconfig_path(o)?;
    let (program, args) = (program.clone(), args.to_vec());
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(program)
            .args(args)
            .env("KUBECONFIG", kubeconfig)
            .status()
    })
    .await??;
    let (id, _) = server(api, o).await?;
    for old in stale_tries(&labels_of(api, id).await?, keep) {
        api.delete_snapshot(id, &tag_of(&old)).await?;
    }
    eprintln!(
        "{}. Put the cluster back with `fluxctl kube undo{}`",
        if status.success() {
            "done".to_owned()
        } else {
            format!("the command exited with {status}")
        },
        if o.name == kube_host::KubeOptions::default().name {
            String::new()
        } else {
            format!(" --name {}", o.name)
        }
    );
    Ok(status.code().unwrap_or(1))
}

/// `fluxctl kube undo`: restores the newest `try-` checkpoint and deletes it, so another `undo` goes one step further back.
pub async fn undo<A: VmApi>(api: &A, o: &kube_host::KubeOptions) -> Result<String> {
    let (id, _) = server(api, o).await?;
    let Some(label) = try_labels(&labels_of(api, id).await?).pop() else {
        bail!(
            "{} has nothing to undo: no checkpoint from `fluxctl kube try` (they are named try-<time>)",
            o.name
        );
    };
    let done = restore(api, o, &label).await?;
    api.delete_snapshot(id, &tag_of(&label)).await?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fluxvm_scheduler::oci_sandbox::SandboxLogs;
    use std::{collections::BTreeMap, sync::Mutex};

    /// Just enough of a daemon: named VMs with a status and snapshots.
    #[derive(Default)]
    struct Fake {
        vms: Vec<(String, Uuid, String)>,
        snaps: Mutex<Vec<(Uuid, String)>>,
        calls: Mutex<Vec<String>>,
    }

    impl VmApi for Fake {
        async fn create(&self, _: Value) -> Result<Uuid> {
            unimplemented!()
        }
        async fn guest_ip(&self, _: Uuid) -> Result<Option<String>> {
            Ok(None)
        }
        async fn delete(&self, _: Uuid) -> Result<()> {
            unimplemented!()
        }
        async fn find(&self, name: &str) -> Result<Option<(Uuid, String)>> {
            Ok(self
                .vms
                .iter()
                .find(|v| v.0 == name)
                .map(|v| (v.1, v.2.clone())))
        }
        async fn snapshot(&self, id: Uuid, tag: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("snapshot {tag}"));
            self.snaps.lock().unwrap().push((id, tag.into()));
            Ok(())
        }
        async fn has_snapshot(&self, id: Uuid, tag: &str) -> Result<bool> {
            Ok(self
                .snaps
                .lock()
                .unwrap()
                .iter()
                .any(|s| s.0 == id && s.1 == tag))
        }
        async fn list_snapshots(&self, id: Uuid) -> Result<Vec<Value>> {
            Ok(self.snaps.lock().unwrap().iter().filter(|s| s.0 == id).map(|s| json!({"tag": s.1, "created_at": "2026-10-25T10:00:00Z", "size_bytes": 5, "has_memory": true})).collect())
        }
        async fn delete_snapshot(&self, id: Uuid, tag: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("delete {tag}"));
            self.snaps
                .lock()
                .unwrap()
                .retain(|s| !(s.0 == id && s.1 == tag));
            Ok(())
        }
        async fn stop(&self, _: Uuid) -> Result<()> {
            self.calls.lock().unwrap().push("stop".into());
            Ok(())
        }
        async fn restore(&self, _: Uuid, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn start(&self, _: Uuid) -> Result<()> {
            unimplemented!()
        }
        async fn set_labels(&self, _: Uuid, _: BTreeMap<String, String>) -> Result<()> {
            unimplemented!()
        }
        async fn list_stack(&self, _: &str) -> Result<Vec<crate::stack::StackVm>> {
            unimplemented!()
        }
        async fn create_sandbox(&self, _: Value) -> Result<Uuid> {
            unimplemented!()
        }
        async fn sandbox_logs(&self, _: Uuid) -> Result<SandboxLogs> {
            unimplemented!()
        }
        async fn vznets(&self) -> Result<Value> {
            unimplemented!()
        }
    }

    fn opts() -> kube_host::KubeOptions {
        kube_host::KubeOptions::default()
    }

    fn fake(status: &str) -> Fake {
        Fake {
            vms: vec![(opts().name, Uuid::from_u128(1), status.into())],
            ..Default::default()
        }
    }

    #[test]
    fn labels_and_tags_round_trip_and_keep_other_snapshots_out() {
        for ok in ["a", "before-upgrade", "v1-2", &"x".repeat(40)] {
            assert!(label_ok(ok), "{ok}");
            assert_eq!(label_of(&tag_of(ok)), Some(ok));
        }
        for bad in [
            "",
            "-a",
            "a-",
            "A",
            "a b",
            "a/b",
            "a.b",
            "a_b",
            &"x".repeat(41),
        ] {
            assert!(!label_ok(bad), "{bad}");
        }
        assert_eq!(label_of("manual"), None);
        assert_eq!(label_of("kube-"), None);
        assert_eq!(label_of("kube-Bad"), None);
    }

    #[tokio::test]
    async fn a_running_cluster_is_checkpointed_once_per_label() {
        let api = fake("running");
        assert!(
            checkpoint(&api, &opts(), "before")
                .await
                .unwrap()
                .contains("saved")
        );
        assert_eq!(
            api.calls.lock().unwrap().as_slice(),
            ["snapshot kube-before"]
        );
        let again = checkpoint(&api, &opts(), "before")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            again.contains("already has a checkpoint") && again.contains("checkpoint-rm before"),
            "{again}"
        );
        let listed = list(&api, &opts()).await.unwrap();
        assert_eq!(listed["checkpoints"][0]["label"], "before");
        assert_eq!(listed["checkpoints"][0]["memory"], true);
    }

    #[tokio::test]
    async fn nothing_is_saved_from_a_stopped_cluster_or_a_bad_label_or_a_missing_cluster() {
        let stopped = fake("stopped");
        let e = checkpoint(&stopped, &opts(), "x")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("is stopped") && e.contains("kube up"), "{e}");
        assert!(
            checkpoint(&fake("running"), &opts(), "Bad Label")
                .await
                .is_err()
        );
        let e = checkpoint(&Fake::default(), &opts(), "x")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("no cluster named"), "{e}");
        assert!(stopped.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cluster_with_workers_is_refused_with_the_reason() {
        let mut api = fake("running");
        api.vms.push((
            format!("{}-w1", opts().name),
            Uuid::from_u128(2),
            "running".into(),
        ));
        for e in [
            checkpoint(&api, &opts(), "x")
                .await
                .unwrap_err()
                .to_string(),
            restore(&api, &opts(), "x").await.unwrap_err().to_string(),
            list(&api, &opts()).await.unwrap_err().to_string(),
        ] {
            assert!(
                e.contains("worker nodes") && e.contains("without --workers"),
                "{e}"
            );
        }
    }

    #[tokio::test]
    async fn restoring_a_missing_checkpoint_names_the_ones_there_are() {
        let api = fake("running");
        let none = restore(&api, &opts(), "gone")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            none.contains("no checkpoint \"gone\"") && none.contains("kube checkpoint LABEL"),
            "{none}"
        );
        checkpoint(&api, &opts(), "one").await.unwrap();
        let some = restore(&api, &opts(), "gone")
            .await
            .unwrap_err()
            .to_string();
        assert!(some.contains("(it has: one)"), "{some}");
    }

    #[tokio::test]
    async fn a_checkpoint_is_removed_and_only_a_checkpoint() {
        let api = fake("running");
        api.snaps
            .lock()
            .unwrap()
            .push((Uuid::from_u128(1), "manual".into()));
        checkpoint(&api, &opts(), "one").await.unwrap();
        assert!(
            remove(&api, &opts(), "manual").await.is_err(),
            "a plain snapshot is not a checkpoint"
        );
        assert!(
            remove(&api, &opts(), "one")
                .await
                .unwrap()
                .contains("removed")
        );
        assert_eq!(list(&api, &opts()).await.unwrap()["checkpoints"], json!([]));
        assert!(
            api.has_snapshot(Uuid::from_u128(1), "manual")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn sleep_saves_before_it_stops_and_only_once() {
        let api = fake("running");
        sleep(&api, &opts()).await.unwrap();
        assert_eq!(
            api.calls.lock().unwrap().as_slice(),
            ["snapshot kube-sleep", "stop"]
        );
        let e = sleep(&api, &opts()).await.unwrap_err().to_string();
        assert!(e.contains("already has a checkpoint"), "{e}");
        assert_eq!(api.calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn only_a_stopped_cluster_with_a_sleep_checkpoint_is_asleep() {
        let stopped = fake("stopped");
        assert!(!is_asleep(&stopped, &opts()).await);
        stopped
            .snaps
            .lock()
            .unwrap()
            .push((Uuid::from_u128(1), tag_of(SLEEP_LABEL)));
        assert!(is_asleep(&stopped, &opts()).await);
        let running = fake("running");
        running
            .snaps
            .lock()
            .unwrap()
            .push((Uuid::from_u128(1), tag_of(SLEEP_LABEL)));
        assert!(!is_asleep(&running, &opts()).await);
        let e = wake(&fake("stopped"), &opts())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("is not asleep"), "{e}");
    }

    #[test]
    fn try_labels_are_valid_sorted_and_pruned_oldest_first() {
        use chrono::TimeZone;
        let l = try_label(
            chrono::Utc
                .with_ymd_and_hms(2026, 10, 25, 10, 30, 5)
                .unwrap(),
        );
        assert_eq!(l, "try-20261025-103005000");
        assert!(label_ok(&l));
        let all: Vec<String> = ["try-2", "before", "try-1", "try-3", "sleep"]
            .map(String::from)
            .to_vec();
        assert_eq!(stale_tries(&all, 2), ["try-1"]);
        assert!(stale_tries(&all, 3).is_empty());
        assert!(stale_tries(&all, 9).is_empty());
        assert_eq!(try_labels(&all), ["try-1", "try-2", "try-3"]);
    }

    #[tokio::test]
    async fn try_keeps_the_checkpoint_whatever_the_command_does_and_reports_its_exit() {
        let api = fake("running");
        let ok = try_run(&api, &opts(), &["true".into()], 5).await.unwrap();
        assert_eq!(ok, 0);
        let bad = try_run(&api, &opts(), &["false".into()], 5).await.unwrap();
        assert_eq!(bad, 1);
        let listed = list(&api, &opts()).await.unwrap();
        assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 2);
        assert!(try_run(&api, &opts(), &[], 5).await.is_err());
        assert!(try_run(&api, &opts(), &["true".into()], 0).await.is_err());
    }

    #[tokio::test]
    async fn undo_with_nothing_to_undo_says_so() {
        let api = fake("running");
        let e = undo(&api, &opts()).await.unwrap_err().to_string();
        assert!(e.contains("nothing to undo"), "{e}");
    }

    #[tokio::test]
    async fn auto_checkpoints_are_taken_when_running_and_pruned_to_the_newest() {
        let stopped = fake("stopped");
        assert_eq!(auto_checkpoint(&stopped, &opts(), 2).await.unwrap(), None);
        assert!(stopped.calls.lock().unwrap().is_empty());
        let api = fake("running");
        for old in [
            "auto-20260101-000000",
            "auto-20260102-000000",
            "auto-20260103-000000",
            "keepme",
        ] {
            api.snaps
                .lock()
                .unwrap()
                .push((Uuid::from_u128(1), tag_of(old)));
        }
        assert!(auto_checkpoint(&api, &opts(), 2).await.unwrap().is_some());
        let left: Vec<String> = api
            .snaps
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.1.clone())
            .collect();
        assert!(left.contains(&"kube-keepme".to_owned()));
        assert!(!left.contains(&"kube-auto-20260101-000000".to_owned()));
        assert!(!left.contains(&"kube-auto-20260102-000000".to_owned()));
        assert_eq!(
            left.iter().filter(|t| t.starts_with("kube-auto-")).count(),
            2
        );
    }
}
