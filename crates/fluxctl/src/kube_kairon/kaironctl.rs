// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! Getting a `kaironctl` that is checked against the release's own `SHA256SUMS` before it is ever run. Fails closed: if the
//! checksums cannot be fetched, or the file does not match, nothing is executed.

use super::exec::{Cmd, Runner};
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn release_base(version: &str) -> String {
    format!("https://github.com/zyvorai/zyvor-kairon/releases/download/v{version}")
}

/// The release asset for this OS and CPU. Linux has bare `kaironctl-linux-*` binaries; for macOS the release ships the
/// `kubectl kairon` plugin archive, which its docs call the same command tree.
pub fn asset_name(os: &str, arch: &str) -> Result<String> {
    let a = match arch {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        o => bail!("no kaironctl release for CPU {o}"),
    };
    match os {
        "linux" => Ok(format!("kaironctl-linux-{a}")),
        "macos" | "darwin" => Ok(format!("kairon_darwin_{a}.tar.gz")),
        o => bail!("no kaironctl release for {o}"),
    }
}

/// `SHA256SUMS` lines are `HEX  NAME` (or `HEX *NAME`).
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let h = it.next()?;
            let n = it.next()?.trim_start_matches('*');
            (h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
                .then(|| (h.to_ascii_lowercase(), n.to_string()))
        })
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn verify(sums: &str, asset: &str, bytes: &[u8]) -> Result<()> {
    let Some((want, _)) = parse_sums(sums).into_iter().find(|(_, n)| n == asset) else {
        bail!("the release's SHA256SUMS has no line for {asset}; refusing to run it");
    };
    let got = sha256_hex(bytes);
    if got != want {
        bail!(
            "{asset} does not match the release's SHA256SUMS (expected {want}, got {got}); it was deleted and not run"
        );
    }
    Ok(())
}

/// Downloads the asset and `SHA256SUMS` into `dir`, verifies, and returns the executable. Re-uses a verified copy.
pub fn ensure(r: &dyn Runner, dir: &Path, version: &str, os: &str, arch: &str) -> Result<PathBuf> {
    let asset = asset_name(os, arch)?;
    std::fs::create_dir_all(dir)?;
    let base = release_base(version);
    let sums_path = dir.join("SHA256SUMS");
    let get = |url: String, to: &Path| -> Result<()> {
        let o = r.run(&Cmd::new(
            "curl",
            vec![
                "-fsSL".into(),
                "--proto".into(),
                "=https".into(),
                "-o".into(),
                to.display().to_string(),
                url.clone(),
            ],
        ))?;
        if !o.ok() {
            bail!("could not download {url}: {}", o.why());
        }
        Ok(())
    };
    get(format!("{base}/SHA256SUMS"), &sums_path).map_err(|e| {
        anyhow::anyhow!(
            "{e}\nWithout the release's SHA256SUMS fluxctl will not run a kaironctl it cannot verify. Check that release v{version} \
             has published assets at {base}/, or put a kaironctl you trust on PATH and run it yourself"
        )
    })?;
    let sums = std::fs::read_to_string(&sums_path)?;
    let file = dir.join(&asset);
    get(format!("{base}/{asset}"), &file)?;
    let bytes = std::fs::read(&file)?;
    if let Err(e) = verify(&sums, &asset, &bytes) {
        let _ = std::fs::remove_file(&file);
        return Err(e);
    }
    let exe = if asset.ends_with(".tar.gz") {
        let o = r.run(&Cmd::new(
            "tar",
            vec![
                "-xzf".into(),
                file.display().to_string(),
                "-C".into(),
                dir.display().to_string(),
            ],
        ))?;
        if !o.ok() {
            bail!("could not unpack {asset}: {}", o.why());
        }
        ["kaironctl", "kubectl-kairon"]
            .iter()
            .map(|n| dir.join(n))
            .find(|p| p.is_file())
            .ok_or_else(|| {
                anyhow::anyhow!("{asset} unpacked, but holds neither kaironctl nor kubectl-kairon")
            })?
    } else {
        file
    };
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
    Ok(exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kube_kairon::exec::fake::Fake;

    #[test]
    fn asset_names() {
        assert_eq!(
            asset_name("linux", "x86_64").unwrap(),
            "kaironctl-linux-amd64"
        );
        assert_eq!(
            asset_name("linux", "aarch64").unwrap(),
            "kaironctl-linux-arm64"
        );
        assert_eq!(
            asset_name("macos", "aarch64").unwrap(),
            "kairon_darwin_arm64.tar.gz"
        );
        assert!(asset_name("windows", "x86_64").is_err());
        assert!(asset_name("linux", "riscv64").is_err());
    }

    #[test]
    fn sums_parse_and_verify() {
        let body = b"hello";
        let h = sha256_hex(body);
        let sums = format!(
            "{h}  kaironctl-linux-amd64\n{}  other\nnot a line\n",
            "0".repeat(64)
        );
        assert_eq!(parse_sums(&sums).len(), 2);
        assert!(verify(&sums, "kaironctl-linux-amd64", body).is_ok());
        let e = verify(&sums, "kaironctl-linux-amd64", b"tampered")
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not match") && e.contains("not run"));
        assert!(verify(&sums, "missing", body).is_err());
        assert_eq!(
            h,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn unreachable_checksums_fail_closed() {
        let f = Fake::default();
        f.rule("SHA256SUMS", 22, "");
        let d = tempfile::tempdir().unwrap();
        let e = ensure(&f, d.path(), "0.10.6", "linux", "x86_64")
            .unwrap_err()
            .to_string();
        assert!(e.contains("will not run a kaironctl it cannot verify"));
        // only the checksums were attempted
        assert_eq!(f.calls.borrow().len(), 1);
    }
}
