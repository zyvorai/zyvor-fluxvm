//! Hibernation: write a paused guest's RAM/CPU/device state to a file through
//! QEMU's migration stream (`file:` URI), and load it back into a QEMU
//! started with `-incoming defer`.

use crate::qmp;
use anyhow::{Context, Result, bail};
use fluxvm_core::model::{HostHibernationFacts, MigrationPhase};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

const QMP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(200);

/// Pauses the guest (if running) and streams its state to `path`. Returns the
/// state size in bytes. On failure a guest that was running is resumed.
pub async fn save_state(qmp_socket: &Path, path: &Path, deadline: Duration) -> Result<u64> {
    let was_running = crate::receiver::run_state(qmp_socket).await? == "running";
    if was_running {
        qmp::execute(qmp_socket, "stop", None, QMP_TIMEOUT).await?;
    }
    let result = save_paused(qmp_socket, path, deadline).await;
    if result.is_err() && was_running {
        let _ = qmp::execute(qmp_socket, "cont", None, QMP_TIMEOUT).await;
    }
    result
}

async fn save_paused(qmp_socket: &Path, path: &Path, deadline: Duration) -> Result<u64> {
    let _ = std::fs::remove_file(path);
    qmp::execute(
        qmp_socket,
        "migrate",
        Some(json!({"uri": file_uri(path)?})),
        QMP_TIMEOUT,
    )
    .await
    .context("QMP migrate to file")?;
    let started = Instant::now();
    loop {
        let status = qmp::migration_status(qmp_socket, QMP_TIMEOUT).await?;
        match status.phase {
            MigrationPhase::Completed => break,
            MigrationPhase::Failed | MigrationPhase::Cancelled => bail!(
                "saving guest state failed: {}{}",
                status.status,
                status
                    .error
                    .as_deref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ),
            _ if started.elapsed() > deadline => {
                let _ = qmp::execute(qmp_socket, "migrate_cancel", None, QMP_TIMEOUT).await;
                bail!("saving guest state did not finish within {deadline:?}");
            }
            _ => tokio::time::sleep(POLL).await,
        }
    }
    let size = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if size == 0 {
        bail!(
            "QEMU reported the state saved but {} is empty",
            path.display()
        );
    }
    Ok(size)
}

/// Loads `path` into a QEMU launched with `-incoming defer`, then resumes the
/// guest.
pub async fn load_state(qmp_socket: &Path, path: &Path, deadline: Duration) -> Result<()> {
    qmp::execute(
        qmp_socket,
        "migrate-incoming",
        Some(json!({"uri": file_uri(path)?})),
        QMP_TIMEOUT,
    )
    .await
    .context("QMP migrate-incoming from file")?;
    let started = Instant::now();
    let state = loop {
        let state = crate::receiver::run_state(qmp_socket)
            .await
            .context("QEMU exited while loading the saved state (see the VM log)")?;
        if state != "inmigrate" {
            break state;
        }
        if started.elapsed() > deadline {
            bail!("loading guest state did not finish within {deadline:?}");
        }
        tokio::time::sleep(POLL).await;
    };
    match state.as_str() {
        "running" => Ok(()),
        "paused" | "postmigrate" | "prelaunch" => {
            qmp::execute(qmp_socket, "cont", None, QMP_TIMEOUT).await?;
            Ok(())
        }
        other => bail!("guest is {other} after loading its saved state"),
    }
}

/// Asks QEMU to exit without a guest shutdown (the guest is paused).
pub async fn quit(qmp_socket: &Path) -> Result<()> {
    qmp::execute(qmp_socket, "quit", None, QMP_TIMEOUT)
        .await
        .map(|_| ())
}

fn file_uri(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))?;
    if !path.is_absolute() || s.contains(',') {
        bail!("hibernation state path must be absolute without commas: {s}");
    }
    Ok(format!("file:{s}"))
}

/// Facts about this host that a saved state depends on.
pub async fn host_facts(qemu_binary: &str, state_dir: &Path) -> Result<HostHibernationFacts> {
    let version = command_stdout(qemu_binary, &["--version"]).await?;
    let machines = command_stdout(qemu_binary, &["-machine", "help"]).await?;
    let cpuinfo = tokio::fs::read_to_string("/proc/cpuinfo")
        .await
        .context("reading /proc/cpuinfo")?;
    let (cpu_vendor, cpu_flags) = parse_cpuinfo(&cpuinfo);
    Ok(HostHibernationFacts {
        qemu_version: parse_qemu_version(&version)
            .with_context(|| format!("unrecognized `{qemu_binary} --version` output"))?,
        machine_type: parse_q35_alias(&machines)
            .with_context(|| format!("`{qemu_binary} -machine help` lists no q35 alias"))?,
        cpu_vendor,
        cpu_flags,
        state_dir: state_dir.to_path_buf(),
    })
}

async fn command_stdout(bin: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {bin}"))?;
    if !out.status.success() {
        bail!("{bin} {} exited with {}", args.join(" "), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub(crate) fn parse_qemu_version(out: &str) -> Option<String> {
    let rest = out.split("version").nth(1)?;
    rest.split_whitespace().next().map(String::from)
}

pub(crate) fn parse_q35_alias(out: &str) -> Option<String> {
    out.lines()
        .find(|l| l.split_whitespace().next() == Some("q35"))
        .and_then(|l| l.split("(alias of ").nth(1))
        .and_then(|s| s.split(')').next())
        .map(|s| s.trim().to_string())
}

pub(crate) fn parse_cpuinfo(cpuinfo: &str) -> (String, Vec<String>) {
    let field = |name: &str| {
        cpuinfo
            .lines()
            .find(|l| l.split(':').next().map(str::trim) == Some(name))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    };
    let mut flags: Vec<String> = field("flags")
        .split_whitespace()
        .map(String::from)
        .collect();
    flags.sort();
    flags.dedup();
    (field("vendor_id"), flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qemu_version_is_parsed() {
        assert_eq!(
            parse_qemu_version(
                "QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1)\nCopyright (c) 2003-2025"
            )
            .as_deref(),
            Some("10.2.1")
        );
        assert_eq!(parse_qemu_version("nothing"), None);
    }

    #[test]
    fn q35_alias_is_resolved() {
        let help = "Supported machines are:\n\
            pc                   Standard PC (i440FX + PIIX, 1996) (alias of pc-i440fx-10.2)\n\
            q35                  Standard PC (Q35 + ICH9, 2009) (alias of pc-q35-10.2)\n\
            pc-q35-10.2          Standard PC (Q35 + ICH9, 2009) (default)\n";
        assert_eq!(parse_q35_alias(help).as_deref(), Some("pc-q35-10.2"));
        assert_eq!(parse_q35_alias("pc  x (alias of y)"), None);
    }

    #[test]
    fn cpuinfo_vendor_and_sorted_flags() {
        let info = "processor\t: 0\nvendor_id\t: GenuineIntel\nflags\t\t: sse4_2 avx2 fpu avx2\n\nprocessor\t: 1\n";
        let (v, f) = parse_cpuinfo(info);
        assert_eq!(v, "GenuineIntel");
        assert_eq!(f, vec!["avx2", "fpu", "sse4_2"]);
    }

    #[test]
    fn file_uri_rejects_relative_and_commas() {
        assert_eq!(
            file_uri(Path::new("/var/lib/h/state.bin")).unwrap(),
            "file:/var/lib/h/state.bin"
        );
        assert!(file_uri(Path::new("state.bin")).is_err());
        assert!(file_uri(Path::new("/a,b/state.bin")).is_err());
    }
}
