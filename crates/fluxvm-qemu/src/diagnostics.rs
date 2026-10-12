//! Incident diagnostics: read-only QMP state for a bundle, and a guest memory
//! dump through `dump-guest-memory`.

use crate::qmp;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

const QMP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(250);

/// Read-only QMP queries a bundle records, with their arguments. Each lands in
/// `qmp/<name>.json`.
pub const QUERIES: &[(&str, Option<&str>)] = &[
    ("query-status", None),
    ("query-version", None),
    ("query-kvm", None),
    ("query-cpus-fast", None),
    ("query-hotpluggable-cpus", None),
    ("query-memory-size-summary", None),
    ("query-memory-devices", None),
    ("query-balloon", None),
    ("query-block", None),
    ("query-blockstats", None),
    ("query-named-block-nodes", Some(r#"{"flat": true}"#)),
    ("query-block-jobs", None),
    ("query-jobs", None),
    ("query-migrate", None),
    ("query-migrate-capabilities", None),
    ("query-migrate-parameters", None),
    ("query-pci", None),
    ("query-chardev", None),
    ("query-iothreads", None),
    ("query-rx-filter", None),
    ("query-dump", None),
];

/// Runs every query in `QUERIES`; a failing query (for example
/// `query-balloon` without a balloon device) is returned as its error.
pub async fn query_state(qmp_socket: &Path) -> Vec<(&'static str, Result<Value>)> {
    let mut out = Vec::with_capacity(QUERIES.len());
    for (name, args) in QUERIES {
        let args = args.map(|a| serde_json::from_str(a).expect("static QMP arguments"));
        out.push((
            *name,
            qmp::execute(qmp_socket, name, args, QMP_TIMEOUT).await,
        ));
    }
    out
}

/// Writes guest memory to `path` as an ELF core and waits for it. QEMU pauses
/// the guest for the dump and resumes it afterwards if it was running.
/// Returns how long the dump took.
pub async fn dump_memory(qmp_socket: &Path, path: &Path, deadline: Duration) -> Result<Duration> {
    let protocol = dump_protocol(path)?;
    let _ = std::fs::remove_file(path);
    let started = Instant::now();
    qmp::execute(
        qmp_socket,
        "dump-guest-memory",
        Some(json!({"paging": false, "protocol": protocol, "detach": true, "format": "elf"})),
        QMP_TIMEOUT,
    )
    .await
    .context("QMP dump-guest-memory")?;
    loop {
        let v = qmp::execute(qmp_socket, "query-dump", None, QMP_TIMEOUT).await?;
        match parse_dump_status(&v) {
            DumpStatus::Completed => return Ok(started.elapsed()),
            DumpStatus::Failed => bail!("QEMU reported the memory dump failed"),
            DumpStatus::Active if started.elapsed() > deadline => {
                bail!(
                    "memory dump did not finish within {deadline:?}; QEMU may still be writing it"
                )
            }
            DumpStatus::Active => tokio::time::sleep(POLL).await,
        }
    }
}

fn dump_protocol(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .with_context(|| format!("dump path {} is not UTF-8", path.display()))?;
    if !path.is_absolute() {
        bail!("dump path {s} is not absolute");
    }
    Ok(format!("file:{s}"))
}

#[derive(Debug, PartialEq, Eq)]
enum DumpStatus {
    Active,
    Completed,
    Failed,
}

fn parse_dump_status(v: &Value) -> DumpStatus {
    match v.get("status").and_then(Value::as_str) {
        Some("completed") => DumpStatus::Completed,
        Some("failed") => DumpStatus::Failed,
        // "none" right after the command means the detached dump thread has
        // not reported yet.
        _ => DumpStatus::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_status_parses() {
        assert_eq!(
            parse_dump_status(&json!({"status": "completed", "completed": 10, "total": 10})),
            DumpStatus::Completed
        );
        assert_eq!(
            parse_dump_status(&json!({"status": "failed"})),
            DumpStatus::Failed
        );
        assert_eq!(
            parse_dump_status(&json!({"status": "active"})),
            DumpStatus::Active
        );
        assert_eq!(
            parse_dump_status(&json!({"status": "none"})),
            DumpStatus::Active
        );
    }

    #[test]
    fn dump_protocol_needs_an_absolute_path() {
        assert_eq!(
            dump_protocol(Path::new("/var/lib/fluxvm/d/mem.elf")).unwrap(),
            "file:/var/lib/fluxvm/d/mem.elf"
        );
        assert!(dump_protocol(Path::new("mem.elf")).is_err());
    }

    #[test]
    fn static_query_arguments_parse() {
        for (name, args) in QUERIES {
            if let Some(a) = args {
                serde_json::from_str::<Value>(a).unwrap_or_else(|e| panic!("{name}: {e}"));
            }
        }
    }
}
