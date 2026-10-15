// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! macOS guests installed through the API (`apple.install`), and the first-boot share that lets fresh clones accept SSH keys.

use anyhow::{bail, Context, Result};
use fluxvm_core::model::{AppleFirstBoot, AppleGuest, CreateVmRequest};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// Written into the workspace once the installer finished; until then a relaunch installs again.
pub const INSTALLED_MARKER: &str = "macos-installed";
/// The workspace directory shared into a macOS guest as `/Volumes/My Shared Files/firstboot`.
pub const FIRSTBOOT_DIR: &str = "firstboot";
/// Default and minimum size of a fresh macOS disk. The installed system takes about 24 GB.
pub const DEFAULT_INSTALL_DISK_GIB: u64 = 64;
pub const MIN_INSTALL_DISK_GIB: u64 = 40;
/// An IPSW install copies about 25 GB; a slow external disk takes well over an hour.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(3 * 3600);

/// The guest script that installs the shared keys and turns on Remote Login. Also shipped as `scripts/macos-firstboot-helper.sh`.
pub const FIRSTBOOT_SCRIPT: &str = include_str!("../../../scripts/macos-firstboot-helper.sh");

pub fn is_macos_install(req: &CreateVmRequest) -> bool {
    req.apple
        .as_ref()
        .is_some_and(|a| a.guest_os == AppleGuest::Macos && a.install)
}

/// The IPSW an install request restores from: `apple.media`, or `image` when `media` is unset.
pub fn install_media(req: &CreateVmRequest) -> PathBuf {
    req.apple
        .as_ref()
        .and_then(|a| a.media.clone())
        .unwrap_or_else(|| req.image.clone())
}

/// Admission checks for `apple.install` and `apple.firstboot`.
pub fn validate(req: &CreateVmRequest) -> Result<()> {
    let Some(apple) = req.apple.as_ref() else {
        return Ok(());
    };
    let macos = apple.guest_os == AppleGuest::Macos;
    if apple.install {
        if !macos {
            bail!("apple.install is for macOS guests; Linux guests boot a cloud image");
        }
        let media = install_media(req);
        let s = media.to_string_lossy();
        if s.starts_with("http://") || s.starts_with("https://") {
            bail!(
                "apple.install needs a local IPSW path; download it first (URLs are not fetched)"
            );
        }
        if !media.is_absolute() {
            bail!("apple.install needs an absolute IPSW path");
        }
        if req.loadvm_tag.is_some() {
            bail!("apple.install cannot restore a snapshot");
        }
        if req.disk_size_gib.is_some_and(|g| g < MIN_INSTALL_DISK_GIB) {
            bail!("a macOS install needs disk_size_gib of at least {MIN_INSTALL_DISK_GIB}");
        }
    }
    if let Some(fb) = &apple.firstboot {
        if !macos {
            bail!("apple.firstboot is for macOS guests; Linux guests use cloud_init");
        }
        for k in &fb.ssh_public_keys {
            if k.trim().is_empty() || k.contains('\n') || k.contains('\r') {
                bail!("apple.firstboot.ssh_public_keys entries must be single, non-empty lines");
            }
            if !k.starts_with("ssh-") && !k.starts_with("ecdsa-") && !k.starts_with("sk-") {
                bail!("apple.firstboot.ssh_public_keys entries must be OpenSSH public keys");
            }
        }
        if let Some(u) = &fb.username {
            let u = u.trim();
            if u.is_empty()
                || u.contains('/')
                || u.contains(' ')
                || u.contains('\n')
                || u.contains('\r')
                || u.contains(':')
                || u.starts_with('.')
            {
                bail!("apple.firstboot.username must be a simple POSIX username");
            }
        }
        if fb.auto_login
            && fb
                .username
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            bail!("apple.firstboot.auto_login requires a username");
        }
    }
    Ok(())
}

/// Creates (or replaces) the sparse raw disk the installer writes to.
pub fn create_install_disk(disk: &Path, size_gib: Option<u64>) -> Result<()> {
    let gib = size_gib.unwrap_or(DEFAULT_INSTALL_DISK_GIB);
    if gib < MIN_INSTALL_DISK_GIB {
        bail!("a macOS install needs disk_size_gib of at least {MIN_INSTALL_DISK_GIB}");
    }
    let f = fs::File::create(disk).with_context(|| format!("creating {}", disk.display()))?;
    f.set_len(gib * 1024 * 1024 * 1024)
        .with_context(|| format!("sizing {}", disk.display()))?;
    Ok(())
}

/// Writes `authorized_keys`, the `enable_remote_login` flag and `firstboot.sh` under `<workspace>/firstboot`.
pub fn write_firstboot(workspace: &Path, fb: &AppleFirstBoot) -> Result<PathBuf> {
    let dir = workspace.join(FIRSTBOOT_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut keys = fb.ssh_public_keys.join("\n");
    if !keys.is_empty() {
        keys.push('\n');
    }
    fs::write(dir.join("authorized_keys"), keys)?;
    let flag = dir.join("enable_remote_login");
    if fb.enable_remote_login {
        fs::write(&flag, "1\n")?;
    } else {
        let _ = fs::remove_file(&flag);
    }
    if let Some(user) = fb
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        fs::write(dir.join("username"), format!("{user}\n"))?;
        if let Some(name) = fb
            .full_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            fs::write(dir.join("full_name"), format!("{name}\n"))?;
        }
        let password = if let Some(pf) = &fb.password_file {
            fs::read_to_string(pf)
                .with_context(|| format!("reading password file {}", pf.display()))?
                .trim()
                .to_string()
        } else {
            fb.password.clone().unwrap_or_default()
        };
        if !password.is_empty() {
            fs::write(dir.join("password"), format!("{password}\n"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(dir.join("password"), fs::Permissions::from_mode(0o600))?;
            }
        }
        if fb.auto_login {
            fs::write(dir.join("auto_login"), "1\n")?;
        }
    }
    let script = dir.join("firstboot.sh");
    fs::write(&script, FIRSTBOOT_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
    }
    Ok(dir)
}

/// Runs `fluxvm-vz-runner install` to completion. Progress events go to the runner log.
pub async fn run_install(runner: &Path, conf_path: &Path, workspace: &Path) -> Result<()> {
    let log = crate::runner::open_runner_log(workspace)?;
    let mut cmd = tokio::process::Command::new(runner);
    cmd.args(["install", "--config"]).arg(conf_path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(log.try_clone().context("cloning the runner log")?)
        .stderr(log)
        .process_group(0)
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {} install", runner.display()))?;
    let status = match tokio::time::timeout(INSTALL_TIMEOUT, child.wait()).await {
        Ok(s) => s.context("waiting for the macOS installer")?,
        Err(_) => {
            let _ = child.start_kill();
            bail!(
                "the macOS install did not finish within {}h: {}",
                INSTALL_TIMEOUT.as_secs() / 3600,
                crate::runner::tail_runner_log(workspace)
            );
        }
    };
    if !status.success() {
        bail!(
            "the macOS install failed ({status}): {}",
            crate::runner::tail_runner_log(workspace)
        );
    }
    fs::write(workspace.join(INSTALLED_MARKER), "1\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(apple: &str) -> CreateVmRequest {
        serde_json::from_str(&format!(
            r#"{{"name":"m","backend":"vz","image":"/ipsw/Restore.ipsw","apple":{apple}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn install_takes_media_or_falls_back_to_image() {
        let r = req(r#"{"guest_os":"macos","install":true}"#);
        assert!(is_macos_install(&r));
        assert_eq!(install_media(&r), PathBuf::from("/ipsw/Restore.ipsw"));
        let r = req(r#"{"guest_os":"macos","install":true,"media":"/other.ipsw"}"#);
        assert_eq!(install_media(&r), PathBuf::from("/other.ipsw"));
        assert!(!is_macos_install(&req(r#"{"guest_os":"macos"}"#)));
    }

    #[test]
    fn install_requests_are_validated() {
        assert!(validate(&req(r#"{"guest_os":"macos","install":true}"#)).is_ok());
        for (apple, needle) in [
            (r#"{"install":true}"#, "macOS guests"),
            (
                r#"{"guest_os":"macos","install":true,"media":"https://example.com/r.ipsw"}"#,
                "URLs",
            ),
            (
                r#"{"guest_os":"macos","install":true,"media":"r.ipsw"}"#,
                "absolute",
            ),
            (
                r#"{"firstboot":{"ssh_public_keys":["ssh-ed25519 AAAA"]}}"#,
                "macOS guests",
            ),
            (
                r#"{"guest_os":"macos","firstboot":{"ssh_public_keys":["ssh-ed25519 A\nB"]}}"#,
                "single",
            ),
            (
                r#"{"guest_os":"macos","firstboot":{"ssh_public_keys":["not a key"]}}"#,
                "OpenSSH",
            ),
        ] {
            let err = validate(&req(apple)).expect_err(apple).to_string();
            assert!(err.contains(needle), "{apple}: {err}");
        }
        let mut small = req(r#"{"guest_os":"macos","install":true}"#);
        small.disk_size_gib = Some(20);
        assert!(validate(&small).is_err());
    }

    #[test]
    fn shipped_examples_parse_and_validate() {
        for f in ["macos-install.json", "macos-clone.json"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples")
                .join(f);
            let r: CreateVmRequest = serde_json::from_slice(&fs::read(&path).unwrap())
                .unwrap_or_else(|e| panic!("{f}: {e}"));
            validate(&r).unwrap_or_else(|e| panic!("{f}: {e}"));
        }
    }

    #[test]
    fn install_disk_is_sparse_and_sized() {
        let d = tempfile::tempdir().unwrap();
        let disk = d.path().join("root.raw");
        create_install_disk(&disk, Some(48)).unwrap();
        let md = fs::metadata(&disk).unwrap();
        assert_eq!(md.len(), 48 * 1024 * 1024 * 1024);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(md.blocks() * 512 < 1024 * 1024, "disk should be sparse");
        }
        assert!(create_install_disk(&disk, Some(10)).is_err());
    }

    #[test]
    fn firstboot_share_holds_keys_flag_and_script() {
        let d = tempfile::tempdir().unwrap();
        let fb = AppleFirstBoot {
            ssh_public_keys: vec!["ssh-ed25519 AAAA a".into(), "ssh-ed25519 BBBB b".into()],
            enable_remote_login: true,
            username: Some("fluxvm".into()),
            full_name: Some("FluxVM Admin".into()),
            password: Some("test-secret".into()),
            auto_login: true,
            ..Default::default()
        };
        let dir = write_firstboot(d.path(), &fb).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join("authorized_keys")).unwrap(),
            "ssh-ed25519 AAAA a\nssh-ed25519 BBBB b\n"
        );
        assert!(dir.join("enable_remote_login").is_file());
        assert_eq!(
            fs::read_to_string(dir.join("username")).unwrap().trim(),
            "fluxvm"
        );
        assert_eq!(
            fs::read_to_string(dir.join("full_name")).unwrap().trim(),
            "FluxVM Admin"
        );
        assert_eq!(
            fs::read_to_string(dir.join("password")).unwrap().trim(),
            "test-secret"
        );
        assert!(dir.join("auto_login").is_file());
        let script = fs::read_to_string(dir.join("firstboot.sh")).unwrap();
        assert!(script.contains("AuthorizedKeysFile"));
        assert!(script.contains("sysadminctl"));
        let off = AppleFirstBoot {
            enable_remote_login: false,
            ..fb.clone()
        };
        write_firstboot(d.path(), &off).unwrap();
        assert!(!dir.join("enable_remote_login").exists());
    }
}
