//! QMP block primitives for changed-block-tracking backups: persistent
//! dirty bitmaps, backup targets, the backup transaction and job polling.
//!
//! A backup is one QMP `transaction` across all of a VM's disks, so every
//! disk is copied from the same instant and the bitmaps move together.
//! Jobs are created with `auto-dismiss: false` and polled with `query-jobs`
//! (QMP calls here are one-shot connections without an event stream), then
//! dismissed once their outcome has been read.

use crate::qmp;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

/// A dirty bitmap as `query-named-block-nodes` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    pub node: String,
    pub name: String,
    pub persistent: bool,
    /// Left marked in-use by an unclean shutdown; its contents can't be
    /// trusted and QEMU refuses to use it for a backup.
    pub inconsistent: bool,
    /// In use by a running job.
    pub busy: bool,
    /// Dirty bytes.
    pub count: u64,
    pub granularity: u64,
}

#[derive(Deserialize)]
struct NamedNode {
    #[serde(rename = "node-name", default)]
    node_name: String,
    #[serde(rename = "dirty-bitmaps", default)]
    dirty_bitmaps: Vec<RawBitmap>,
}

#[derive(Deserialize)]
struct RawBitmap {
    name: String,
    #[serde(default)]
    persistent: bool,
    #[serde(default)]
    inconsistent: bool,
    #[serde(default)]
    busy: bool,
    #[serde(default)]
    count: u64,
    #[serde(default)]
    granularity: u64,
}

pub(crate) fn parse_bitmaps(nodes: &Value) -> Result<Vec<Bitmap>> {
    let nodes: Vec<NamedNode> =
        serde_json::from_value(nodes.clone()).context("parsing query-named-block-nodes")?;
    Ok(nodes
        .into_iter()
        .flat_map(|n| {
            let node = n.node_name;
            n.dirty_bitmaps.into_iter().map(move |b| Bitmap {
                node: node.clone(),
                name: b.name,
                persistent: b.persistent,
                inconsistent: b.inconsistent,
                busy: b.busy,
                count: b.count,
                granularity: b.granularity,
            })
        })
        .collect())
}

/// Every dirty bitmap on every named node.
pub async fn query_bitmaps(socket: &Path, timeout: Duration) -> Result<Vec<Bitmap>> {
    let nodes = qmp::execute(
        socket,
        "query-named-block-nodes",
        Some(json!({"flat": true})),
        timeout,
    )
    .await?;
    parse_bitmaps(&nodes)
}

pub async fn remove_bitmap(socket: &Path, node: &str, name: &str, timeout: Duration) -> Result<()> {
    qmp::execute(
        socket,
        "block-dirty-bitmap-remove",
        Some(json!({"node": node, "name": name})),
        timeout,
    )
    .await
    .with_context(|| format!("removing bitmap {name} from {node}"))?;
    Ok(())
}

/// Opens an existing qcow2 file as a backup target node.
pub async fn add_target(socket: &Path, node: &str, path: &Path, timeout: Duration) -> Result<()> {
    qmp::execute(
        socket,
        "blockdev-add",
        Some(json!({
            "driver": "qcow2",
            "node-name": node,
            "file": {"driver": "file", "filename": path.to_string_lossy()},
        })),
        timeout,
    )
    .await
    .with_context(|| format!("adding backup target {node}"))?;
    Ok(())
}

pub async fn remove_target(socket: &Path, node: &str, timeout: Duration) -> Result<()> {
    qmp::execute(
        socket,
        "blockdev-del",
        Some(json!({"node-name": node})),
        timeout,
    )
    .await
    .with_context(|| format!("removing backup target {node}"))?;
    Ok(())
}

/// What a backup copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupSync {
    /// Everything.
    Full,
    /// Only clusters dirty in the parent checkpoint bitmap `from`. The
    /// bitmap is read, never cleared: it is dropped only once the new
    /// backup is safely stored, and the new checkpoint merged back into it
    /// if anything fails, so a failed upload loses no writes.
    Incremental { from: String },
}

/// One disk in a backup transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskBackup {
    /// Source format node (`root`, `data-<name>`).
    pub node: String,
    /// Target node added with [`add_target`].
    pub target: String,
    pub job_id: String,
}

/// The `transaction` arguments for a backup of `disks`. Every disk gets the
/// persistent `checkpoint` bitmap in the same transaction as its copy, so
/// the checkpoint records exactly the writes after the backup's instant.
/// Not grouped: QEMU refuses grouped completion next to bitmap actions, and
/// nothing here clears a bitmap, so a failure on one disk can't leave the
/// others' bitmaps out of step.
pub fn backup_transaction(disks: &[DiskBackup], checkpoint: &str, sync: &BackupSync) -> Value {
    let mut actions = Vec::new();
    for d in disks {
        actions.push(json!({
            "type": "block-dirty-bitmap-add",
            "data": {"node": d.node, "name": checkpoint, "persistent": true},
        }));
        let mut backup = json!({
            "device": d.node,
            "target": d.target,
            "job-id": d.job_id,
            "compress": true,
            "auto-finalize": true,
            "auto-dismiss": false,
        });
        match sync {
            BackupSync::Full => backup["sync"] = json!("full"),
            BackupSync::Incremental { from } => {
                backup["sync"] = json!("bitmap");
                backup["bitmap"] = json!(from);
                backup["bitmap-mode"] = json!("never");
            }
        }
        actions.push(json!({"type": "blockdev-backup", "data": backup}));
    }
    json!({"actions": actions})
}

/// ORs `sources` into `target` on `node`.
pub async fn merge_bitmaps(
    socket: &Path,
    node: &str,
    target: &str,
    sources: &[String],
    timeout: Duration,
) -> Result<()> {
    qmp::execute(
        socket,
        "block-dirty-bitmap-merge",
        Some(json!({"node": node, "target": target, "bitmaps": sources})),
        timeout,
    )
    .await
    .with_context(|| format!("merging {sources:?} into bitmap {target} on {node}"))?;
    Ok(())
}

/// Asks a QEMU process or `qemu-storage-daemon` to exit, flushing
/// persistent bitmaps to its images.
pub async fn quit(socket: &Path, timeout: Duration) -> Result<()> {
    qmp::execute(socket, "quit", None, timeout).await?;
    Ok(())
}

pub async fn transaction(socket: &Path, args: Value, timeout: Duration) -> Result<()> {
    qmp::execute(socket, "transaction", Some(args), timeout)
        .await
        .context("backup transaction")?;
    Ok(())
}

/// A block job as `query-jobs` reports it.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Job {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    #[serde(rename = "current-progress", default)]
    pub current: u64,
    #[serde(rename = "total-progress", default)]
    pub total: u64,
    #[serde(default)]
    pub error: Option<String>,
}

impl Job {
    /// Finished, successfully or not, and waiting to be dismissed.
    pub fn concluded(&self) -> bool {
        self.status == "concluded" || self.status == "null"
    }
}

pub async fn query_jobs(socket: &Path, timeout: Duration) -> Result<Vec<Job>> {
    let v = qmp::execute(socket, "query-jobs", None, timeout).await?;
    serde_json::from_value(v).context("parsing query-jobs")
}

pub async fn dismiss_job(socket: &Path, id: &str, timeout: Duration) -> Result<()> {
    qmp::execute(socket, "job-dismiss", Some(json!({"id": id})), timeout)
        .await
        .with_context(|| format!("dismissing job {id}"))?;
    Ok(())
}

/// Polls until every job in `ids` has concluded, dismisses them, and fails
/// with the first job error. `progress` sees each poll's jobs.
pub async fn wait_jobs(
    socket: &Path,
    ids: &[String],
    poll: Duration,
    qmp_timeout: Duration,
    mut progress: impl FnMut(&[Job]),
) -> Result<Vec<Job>> {
    loop {
        let jobs: Vec<Job> = query_jobs(socket, qmp_timeout)
            .await?
            .into_iter()
            .filter(|j| ids.contains(&j.id))
            .collect();
        progress(&jobs);
        for id in ids {
            if !jobs.iter().any(|j| &j.id == id) {
                bail!("block job {id} disappeared before it concluded");
            }
        }
        if jobs.iter().all(Job::concluded) {
            for j in &jobs {
                dismiss_job(socket, &j.id, qmp_timeout).await?;
            }
            if let Some(j) = jobs.iter().find(|j| j.error.is_some()) {
                bail!(
                    "block job {} failed: {}",
                    j.id,
                    j.error.as_deref().unwrap_or_default()
                );
            }
            return Ok(jobs);
        }
        tokio::time::sleep(poll).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// Serves one connection per step (`qmp::execute` reconnects for each
    /// command), recording each request and replying with the step's reply.
    async fn serve(listener: UnixListener, steps: Vec<Value>, seen: Arc<Mutex<Vec<Value>>>) {
        for reply in steps {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            write_half
                .write_all(b"{\"QMP\":{\"version\":{}}}\n")
                .await
                .unwrap();
            let mut caps = String::new();
            reader.read_line(&mut caps).await.unwrap();
            write_half.write_all(b"{\"return\":{}}\n").await.unwrap();
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            seen.lock()
                .unwrap()
                .push(serde_json::from_str(&req).unwrap());
            write_half
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    }

    fn disks() -> Vec<DiskBackup> {
        vec![
            DiskBackup {
                node: "root".into(),
                target: "cbt-root".into(),
                job_id: "cbt-1-root".into(),
            },
            DiskBackup {
                node: "data-logs".into(),
                target: "cbt-data-logs".into(),
                job_id: "cbt-1-data-logs".into(),
            },
        ]
    }

    #[test]
    fn full_transaction_adds_the_checkpoint_with_each_copy() {
        let t = backup_transaction(&disks(), "kairon-cbt-c-0", &BackupSync::Full);
        assert!(
            t.get("properties").is_none(),
            "QEMU rejects grouped with bitmap actions"
        );
        let actions = t["actions"].as_array().unwrap();
        let kinds: Vec<&str> = actions
            .iter()
            .map(|a| a["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "block-dirty-bitmap-add",
                "blockdev-backup",
                "block-dirty-bitmap-add",
                "blockdev-backup"
            ]
        );
        assert_eq!(
            actions[0]["data"],
            json!({"node": "root", "name": "kairon-cbt-c-0", "persistent": true})
        );
        let b = &actions[1]["data"];
        assert_eq!(b["sync"], "full");
        assert_eq!(b["device"], "root");
        assert_eq!(b["target"], "cbt-root");
        assert_eq!(b["compress"], true);
        assert_eq!(b["auto-dismiss"], false);
        assert!(b.get("bitmap").is_none());
        assert_eq!(actions[2]["data"]["node"], "data-logs");
    }

    #[test]
    fn incremental_transaction_reads_the_parent_checkpoint_without_clearing_it() {
        let t = backup_transaction(
            &disks(),
            "kairon-cbt-c-2",
            &BackupSync::Incremental {
                from: "kairon-cbt-c-1".into(),
            },
        );
        let actions = t["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[0]["data"]["name"], "kairon-cbt-c-2");
        let b = &actions[1]["data"];
        assert_eq!(b["sync"], "bitmap");
        assert_eq!(b["bitmap"], "kairon-cbt-c-1");
        assert_eq!(b["bitmap-mode"], "never");
    }

    #[test]
    fn parses_bitmaps_across_nodes() {
        let nodes = json!([
            {"node-name": "root", "dirty-bitmaps": [
                {"name": "kairon-cbt-a", "persistent": true, "count": 65536, "granularity": 65536,
                 "recording": true, "busy": false, "inconsistent": true}
            ]},
            {"node-name": "#block123"},
            {"node-name": "data-logs", "dirty-bitmaps": [{"name": "x", "busy": true}]}
        ]);
        let b = parse_bitmaps(&nodes).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].node, "root");
        assert!(b[0].persistent && b[0].inconsistent && !b[0].busy);
        assert_eq!(b[0].count, 65536);
        assert_eq!(b[1].node, "data-logs");
        assert!(b[1].busy);
    }

    #[tokio::test]
    async fn wait_jobs_polls_until_concluded_then_dismisses() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let running = json!({"return": [
            {"id": "j1", "type": "backup", "status": "running", "current-progress": 1, "total-progress": 4},
            {"id": "other", "type": "mirror", "status": "running"}
        ]});
        let done = json!({"return": [
            {"id": "j1", "type": "backup", "status": "concluded", "current-progress": 4, "total-progress": 4}
        ]});
        let server = tokio::spawn(serve(
            listener,
            vec![running, done, json!({"return": {}})],
            seen.clone(),
        ));
        let mut polls = 0;
        let jobs = wait_jobs(
            &sock,
            &["j1".to_string()],
            Duration::from_millis(1),
            Duration::from_secs(5),
            |_| polls += 1,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(polls, 2);
        assert_eq!(jobs[0].current, 4);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[2]["execute"], "job-dismiss");
        assert_eq!(seen[2]["arguments"]["id"], "j1");
    }

    #[tokio::test]
    async fn wait_jobs_reports_a_failed_job_after_dismissing_it() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let failed = json!({"return": [
            {"id": "j1", "type": "backup", "status": "concluded", "error": "No space left on device"}
        ]});
        let server = tokio::spawn(serve(
            listener,
            vec![failed, json!({"return": {}})],
            seen.clone(),
        ));
        let err = wait_jobs(
            &sock,
            &["j1".to_string()],
            Duration::from_millis(1),
            Duration::from_secs(5),
            |_| {},
        )
        .await
        .unwrap_err();
        server.await.unwrap();
        assert!(err.to_string().contains("No space left"), "{err}");
        assert_eq!(seen.lock().unwrap()[1]["execute"], "job-dismiss");
    }

    #[tokio::test]
    async fn add_target_opens_a_qcow2_file_node() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server = tokio::spawn(serve(listener, vec![json!({"return": {}})], seen.clone()));
        add_target(
            &sock,
            "cbt-root",
            Path::new("/w/b/root.qcow2"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        server.await.unwrap();
        let req = &seen.lock().unwrap()[0];
        assert_eq!(req["execute"], "blockdev-add");
        assert_eq!(
            req["arguments"],
            json!({"driver": "qcow2", "node-name": "cbt-root",
                   "file": {"driver": "file", "filename": "/w/b/root.qcow2"}})
        );
    }
}
