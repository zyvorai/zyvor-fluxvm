//! The node side of enrollment (see `central::enroll`). A node started with a join token (`fvjt_...`) generates a
//! `node_id` once, exchanges the token for its own credential, keeps both in its state directory (mode 0600; it
//! refuses files others can read) and uses the credential from then on. A node given a credential (`fvnc_...`) or an
//! old fleet token uses it as is.

use anyhow::{Context, Result, bail};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const JOIN_PREFIX: &str = "fvjt_";
pub const CRED_PREFIX: &str = "fvnc_";
/// Where state lives when `--state-dir` is not given (systemd sets `STATE_DIRECTORY`).
pub fn default_state_dir() -> PathBuf {
    std::env::var_os("STATE_DIRECTORY")
        .map(|d| {
            PathBuf::from(
                d.to_string_lossy()
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("/var/lib/fluxvm-agent"))
}

/// Reads a secret file; refuses one that group or others can read, so a leaked backup or a loose umask is noticed.
fn read_private(path: &Path) -> Result<Option<String>> {
    match std::fs::metadata(path) {
        Ok(m) => {
            if m.mode() & 0o077 != 0 {
                bail!(
                    "{} is readable by group or others (mode {:o}); chmod 600 it",
                    path.display(),
                    m.mode() & 0o777
                );
            }
            Ok(Some(
                std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?
                    .trim()
                    .to_owned(),
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes atomically with mode 0600.
fn write_private(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The node's stable id: generated once, then read back.
pub fn node_id(dir: &Path) -> Result<String> {
    let path = dir.join("node-id");
    if let Some(id) = read_private(&path)?.filter(|s| s.len() >= 16) {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    write_private(&path, &id)?;
    Ok(id)
}

pub fn credential_path(dir: &Path) -> PathBuf {
    dir.join("node-credential")
}

pub fn load_credential(dir: &Path) -> Result<Option<String>> {
    Ok(read_private(&credential_path(dir))?.filter(|c| c.starts_with(CRED_PREFIX)))
}

enum Outcome {
    Got(String),
    Retry(Duration),
}

async fn enroll_once(
    http: &reqwest::Client,
    central: &str,
    join: &str,
    name: &str,
    node_id: &str,
) -> Result<Outcome> {
    let resp = http
        .post(format!("{central}/fleet/enroll"))
        .bearer_auth(join)
        .json(&serde_json::json!({"name": name, "node_id": node_id}))
        .send()
        .await
        .with_context(|| format!("contacting {central}"))?;
    let status = resp.status();
    if status.is_success() {
        let v: serde_json::Value = resp.json().await?;
        let c = v["credential"]
            .as_str()
            .context("central sent no credential")?;
        anyhow::ensure!(
            c.starts_with(CRED_PREFIX),
            "central sent something that is not a node credential"
        );
        return Ok(Outcome::Got(c.to_owned()));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(10);
        return Ok(Outcome::Retry(Duration::from_secs(secs.clamp(1, 120))));
    }
    let body = resp.text().await.unwrap_or_default();
    // 4xx other than 429 will not get better by asking again.
    if status.is_client_error() {
        bail!("central refused enrollment ({status}): {body}");
    }
    Ok(Outcome::Retry(Duration::from_secs(15)))
}

/// A credential for `name`: the stored one, or one obtained now with `join`. Retries while central is down or busy.
pub async fn ensure_credential(
    http: &reqwest::Client,
    centrals: &[String],
    join: &str,
    name: &str,
    dir: &Path,
) -> Result<String> {
    if let Some(c) = load_credential(dir)? {
        return Ok(c);
    }
    let id = node_id(dir)?;
    let mut attempt = 0usize;
    loop {
        let central = &centrals[attempt % centrals.len()];
        attempt += 1;
        match enroll_once(http, central, join, name, &id).await {
            Ok(Outcome::Got(c)) => {
                write_private(&credential_path(dir), &c)?;
                tracing::info!(node = name, "enrolled with central");
                return Ok(c);
            }
            Ok(Outcome::Retry(wait)) => {
                tracing::warn!(?wait, "enrollment not possible yet; retrying");
                tokio::time::sleep(wait).await;
            }
            Err(e)
                if e.downcast_ref::<reqwest::Error>().is_some()
                    || format!("{e:#}").contains("contacting") =>
            {
                tracing::warn!(error = %e, "enrollment failed; retrying");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Asks central for the next generation of this node's credential and stores it.
pub async fn renew(
    http: &reqwest::Client,
    central: &str,
    current: &str,
    dir: &Path,
) -> Result<String> {
    let resp = http
        .post(format!("{central}/fleet/enroll/renew"))
        .bearer_auth(current)
        .send()
        .await?;
    anyhow::ensure!(
        resp.status().is_success(),
        "renewal refused: {}",
        resp.status()
    );
    let v: serde_json::Value = resp.json().await?;
    let c = v["credential"]
        .as_str()
        .context("no credential in the renewal")?
        .to_owned();
    anyhow::ensure!(c.starts_with(CRED_PREFIX), "not a node credential");
    write_private(&credential_path(dir), &c)?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn node_id_is_stable_and_files_are_private() {
        let d = tempfile::tempdir().unwrap();
        let a = node_id(d.path()).unwrap();
        assert_eq!(a, node_id(d.path()).unwrap());
        assert_eq!(
            std::fs::metadata(d.path().join("node-id")).unwrap().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_group_readable_secret_is_refused() {
        let d = tempfile::tempdir().unwrap();
        write_private(&credential_path(d.path()), "fvnc_x.1.y").unwrap();
        assert_eq!(
            load_credential(d.path()).unwrap().as_deref(),
            Some("fvnc_x.1.y")
        );
        std::fs::set_permissions(
            credential_path(d.path()),
            std::fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        assert!(load_credential(d.path()).is_err());
    }
}
