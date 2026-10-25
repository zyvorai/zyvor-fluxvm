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
            unimplemented!()
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
}
