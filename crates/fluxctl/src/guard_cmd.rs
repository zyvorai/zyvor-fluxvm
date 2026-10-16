// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! `fluxctl guard ...`: the VMM Guard CLI from the control-plane binary.
//!
//! Applying a policy, exec allow-list or signed rule pack needs root and the
//! pinned BPF maps, so locally this hands the arguments to `fluxvm-guard`
//! unchanged (`apply`, `status`, `remove`, `events`, `learn-exec`,
//! `sign-exec-policy`, `sign-rule-pack`, `pack verify`). Against a remote
//! daemon only `guard status <vm>` works, via `GET /v1/vms/{id}/guard`.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

const BIN: &str = "fluxvm-guard";

/// Where `fluxvm-guard` may live, most specific first: an explicit override, next to this
/// executable, then the packaged locations.
fn candidates(override_path: Option<PathBuf>, exe_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    v.extend(override_path);
    if let Some(dir) = exe_dir {
        v.push(dir.join(BIN));
    }
    v.push(PathBuf::from("/usr/local/bin").join(BIN));
    v.push(PathBuf::from("/usr/libexec/fluxvm").join(BIN));
    v
}

fn resolve_with(cands: &[PathBuf]) -> Option<PathBuf> {
    cands.iter().find(|p| p.is_file()).cloned()
}

fn resolve() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    let cands = candidates(
        std::env::var_os("FLUXVM_GUARD_BIN").map(PathBuf::from),
        exe_dir.as_deref(),
    );
    resolve_with(&cands).or_else(|| {
        // Fall back to PATH.
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(BIN))
                .find(|p| p.is_file())
        })
    })
}

/// Replaces this process with `fluxvm-guard args...` so its exit code and output pass through.
#[cfg(unix)]
pub fn run_local(args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let bin = resolve().with_context(|| {
        format!("{BIN} not found; build it with `cargo build -p fluxvm-intelligence --bin {BIN}` or set FLUXVM_GUARD_BIN")
    })?;
    let err = std::process::Command::new(&bin).args(args).exec();
    bail!("running {}: {err}", bin.display())
}

#[cfg(not(unix))]
pub fn run_local(_args: &[String]) -> Result<()> {
    bail!("fluxctl guard needs a Linux host")
}

/// What a remote `guard` invocation asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Remote {
    Status(uuid::Uuid),
}

/// Only `status <vm>` can run against a remote daemon.
pub fn parse_remote(args: &[String]) -> Result<Remote> {
    match args {
        [cmd, vm] if cmd == "status" => Ok(Remote::Status(
            crate::output::parse_vm_ref(vm).map_err(anyhow::Error::msg)?,
        )),
        [cmd, ..] if cmd == "status" => bail!("usage: fluxctl guard status <vm>"),
        _ => bail!(
            "only `guard status <vm>` works with --server; run applies and pack commands on the host itself"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_prefer_override_then_sibling_then_packaged_paths() {
        let c = candidates(Some("/x/guard".into()), Some(Path::new("/opt/fx")));
        assert_eq!(c[0], PathBuf::from("/x/guard"));
        assert_eq!(c[1], PathBuf::from("/opt/fx/fluxvm-guard"));
        assert_eq!(c[2], PathBuf::from("/usr/local/bin/fluxvm-guard"));
        assert_eq!(c[3], PathBuf::from("/usr/libexec/fluxvm/fluxvm-guard"));
        assert_eq!(candidates(None, None).len(), 2);
    }

    #[test]
    fn resolve_picks_the_first_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("fluxvm-guard");
        std::fs::write(&real, "").unwrap();
        let cands = vec![dir.path().join("missing"), real.clone()];
        assert_eq!(resolve_with(&cands), Some(real));
        assert_eq!(resolve_with(&[dir.path().join("missing")]), None);
    }

    #[test]
    fn remote_accepts_only_status_with_a_vm() {
        let id = uuid::Uuid::nil();
        assert_eq!(
            parse_remote(&["status".into(), id.to_string()]).unwrap(),
            Remote::Status(id)
        );
        assert!(parse_remote(&["status".into()]).is_err());
        assert!(parse_remote(&["apply".into(), id.to_string(), "1".into()]).is_err());
        assert!(parse_remote(&[]).is_err());
    }
}
